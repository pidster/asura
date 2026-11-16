//! The conversation owner's part of the policy lifecycle
//! (security-policy.md, PL3 implementation contract): the policy control
//! messages, the restore hand-off, the lifecycle records (kinds 35 and 36)
//! that this owner appends as the one journal appender, the policy audit
//! events, and the rechecks of queued and running jobs after an
//! activation (PL3-Q1, PL3-F3). Every step is bounded in-memory work on
//! the reactor.
use super::{Body, Job, Owner, policy_call, reply, storage_error};
use crate::job_controller::JobReason;
use crate::policy::{Commit, JobDecision, JobRequest, RestoreFacts, Ticket};
use asura_control::pb;
use asura_policy as ap;
use asura_storage::authority::conversation as journal;
use asura_storage::authority::writer;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

/// Rechecks in flight or waiting: the job controller's unsettled bound.
pub(super) const RECHECKS: usize = crate::job_controller::MAX_UNSETTLED;

/// One job decided again under the dispatch revision.
pub(super) struct Recheck {
    job: journal::Id,
    request: JobRequest,
    /// A running job's constraint set; `None` for a queued start.
    running: Option<Option<asura_platform::exec_cmd::Constraints>>,
    ticket: Option<Ticket>,
    decision: Option<Box<JobDecision>>,
}

/// The policy state the conversation owner keeps.
#[derive(Default)]
pub(super) struct PolicyAdmin {
    restored: bool,
    /// The dispatch generation the job controller reads at slot grant.
    generation: Arc<AtomicU64>,
    fenced: bool,
    rechecks: VecDeque<Recheck>,
    /// A recheck's kind 34 is in the writer.
    writing: bool,
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

/// The person's control reply.
fn policy_reply(
    request: pb::Envelope,
    candidate: Option<[u8; 16]>,
    position: u32,
    error: Option<&str>,
) -> pb::Envelope {
    reply(
        request,
        Body::PolicyReply(pb::PolicyReply {
            candidate_id: candidate.map(|id| id.to_vec()),
            position: candidate.map(|_| position),
            error: error.map(str::to_owned),
        }),
    )
}

/// The `policy.decision_denied` audit event of a committed `deny`.
pub(super) fn denied_event(decision: &JobDecision) -> asura_storage::audit::Event {
    let primary = decision.primary.as_ref();
    use asura_storage::audit::PolicyActionGroup as G;
    asura_storage::audit::Event::PolicyDecisionDenied {
        action_group: match decision.action.group() {
            ap::ActionGroup::File => G::File,
            ap::ActionGroup::Process => G::Process,
            ap::ActionGroup::Network => G::Network,
            ap::ActionGroup::Secret => G::Secret,
            ap::ActionGroup::Mach => G::Mach,
            ap::ActionGroup::Egress => G::Egress,
        },
        scope: crate::policy::audit_scope(primary.and_then(|p| p.scope)),
        reason: primary.map_or_else(|| "no_permit".into(), |p| p.reason.clone()),
        rule: primary.and_then(|p| p.rule.clone()).unwrap_or_default(),
        project: decision.project,
    }
}

impl Owner {
    /// `PolicyCandidateSubmit`, `PolicyRollback` and `PolicyCancel`.
    pub(super) fn policy_request(&mut self, request: pb::Envelope) -> Option<pb::Envelope> {
        if self.closing {
            return Some(policy_reply(request, None, 0, Some("unavailable")));
        }
        let recent_replay = self.replay.clone();
        let Some(owner) = self.policy.as_mut() else {
            return Some(policy_reply(request, None, 0, Some("unavailable")));
        };
        let scope = |code: Option<u32>| {
            code.and_then(|c| u8::try_from(c).ok())
                .and_then(ap::Scope::from_code)
        };
        let result = match request.body.as_ref() {
            Some(Body::PolicyCandidateSubmit(v)) => match scope(v.scope) {
                Some(s) => owner.submit_candidate(s, v.text.clone().unwrap_or_default()),
                None => Err("scope_invalid"),
            },
            Some(Body::PolicyRollback(v)) => match scope(v.scope) {
                Some(s) => {
                    let recent = recent_replay
                        .as_ref()
                        .map(|r| r.policy.recent().to_vec())
                        .unwrap_or_default();
                    owner.rollback(s, v.revision.unwrap_or(0), &recent)
                }
                None => Err("scope_invalid"),
            },
            Some(Body::PolicyCancel(v)) => {
                let id: Option<[u8; 16]> = v
                    .candidate_id
                    .as_deref()
                    .and_then(|b| <[u8; 16]>::try_from(b).ok());
                match id {
                    Some(id) if owner.cancel_candidate(id) => Ok((id, 0)),
                    _ => Err("not_found"),
                }
            }
            _ => Err("unavailable"),
        };
        Some(match result {
            Ok((id, position)) => policy_reply(request, Some(id), position, None),
            Err(error) => policy_reply(request, None, 0, Some(error)),
        })
    }

    /// One policy lifecycle step, after the owner's poll: restore, audit,
    /// activations and rechecks, and the next lifecycle record.
    pub(super) fn policy_step(&mut self) {
        let Some(owner) = self.policy.as_mut() else {
            return;
        };
        if !self.policy_admin.restored
            && let Some(replay) = self.replay.as_ref()
        {
            owner.restore(RestoreFacts {
                active: replay.policy.active_manifest().cloned(),
                open_grants: replay.policy.open_grants().cloned().collect(),
            });
            self.policy_admin.restored = true;
        }
        let events = owner.take_audit();
        let generation = owner.dispatch_generation();
        self.policy_admin
            .generation
            .store(generation, Ordering::Release);
        let mut activations = Vec::new();
        while let Some(activation) = owner.take_activation() {
            activations.push(activation);
        }
        if let Some(emitter) = &self.audit_emitter {
            for event in events {
                let _ = emitter.try_emit(event);
            }
        }
        if let Some(controller) = self.jobs.controller() {
            if !self.policy_admin.fenced {
                let shared = self.policy_admin.generation.clone();
                controller.set_policy(Box::new(move || shared.load(Ordering::Acquire)));
                self.policy_admin.fenced = true;
            }
            for activation in activations {
                if activation.narrows {
                    controller
                        .revoke_starting_before(activation.generation, JobReason::PolicyChanged);
                    for (job, request, constraints) in
                        controller.live_rechecks(activation.generation)
                    {
                        if self.policy_admin.rechecks.len() < RECHECKS {
                            self.policy_admin.rechecks.push_back(Recheck {
                                job,
                                request,
                                running: Some(constraints),
                                ticket: None,
                                decision: None,
                            });
                        }
                    }
                }
            }
            for (job, request) in controller.take_rechecks(generation) {
                if self.policy_admin.rechecks.len() < RECHECKS
                    && !self.policy_admin.rechecks.iter().any(|r| r.job == job)
                {
                    self.policy_admin.rechecks.push_back(Recheck {
                        job,
                        request,
                        running: None,
                        ticket: None,
                        decision: None,
                    });
                }
            }
        }
        self.recheck_step();
        self.policy_record_step();
    }

    /// Decide each recheck; append its kind 34 subject 3 one at a time.
    fn recheck_step(&mut self) {
        let Some(owner) = self.policy.as_mut() else {
            return;
        };
        let generation = owner.dispatch_generation();
        for recheck in &mut self.policy_admin.rechecks {
            if recheck.decision.is_some() {
                continue;
            }
            match recheck.ticket {
                None => {
                    let mut request = recheck.request.clone();
                    request.now_ms = now_ms();
                    recheck.ticket = owner.decide_job(request);
                }
                Some(ticket) => {
                    if let Some(decision) = owner.take(ticket) {
                        recheck.ticket = None;
                        if decision.unavailable || decision.generation == generation {
                            recheck.decision = Some(Box::new(decision));
                        }
                    }
                }
            }
        }
        if self.policy_admin.writing || self.pending.is_some() {
            return;
        }
        let Some(at) = self
            .policy_admin
            .rechecks
            .iter()
            .position(|r| r.decision.is_some())
        else {
            return;
        };
        let Some(mut recheck) = self.policy_admin.rechecks.remove(at) else {
            return;
        };
        let Some(mut decision) = recheck.decision.take() else {
            return;
        };
        // A running `model-1` job continues only inside the new regions.
        if decision.run
            && let Some(Some(running)) = recheck.running
            && let Some(new) = decision.constraints
            && (running.read() & !new.read() != 0
                || running.exec() & !new.exec() != 0
                || running.write() & !new.write() != 0)
        {
            decision.run = false;
            decision.constraints = None;
            decision.primary = Some(ap::Primary {
                scope: None,
                reason: "regions_narrowed".into(),
                rule: None,
                explain: None,
            });
        }
        if decision.unavailable {
            // No engine or no user scope: no record; the job cannot start
            // or continue under a revision that cannot decide.
            if let Some(controller) = self.jobs.controller() {
                controller.revoke_job(recheck.job, JobReason::PolicyRevoked);
            }
            return;
        }
        let Some(revision) = self.replay.as_ref().and_then(|r| r.policy.active()) else {
            self.policy_admin.rechecks.push_front(Recheck {
                decision: Some(decision),
                ..recheck
            });
            return;
        };
        if revision.1 != decision.revision_digest {
            // Activated again since: decide again.
            recheck.ticket = None;
            self.policy_admin.rechecks.push_front(recheck);
            return;
        }
        let record = policy_call::decision_record(
            &decision,
            revision.0,
            journal::PolicySubject::JobRecheck { job: recheck.job },
        );
        let job = recheck.job;
        let running = recheck.running.is_some();
        match self.append(record, Job::PolicyRecheck(job, running, decision), None) {
            Ok(()) => self.policy_admin.writing = true,
            Err(e) => self.unavailable = Some(storage_error(e)),
        }
    }

    /// A recheck's kind 34 committed (or failed): the controller acts.
    pub(super) fn policy_recheck_done(
        &mut self,
        job: journal::Id,
        _running: bool,
        decision: Box<JobDecision>,
        committed: bool,
    ) {
        self.policy_admin.writing = false;
        let Some(controller) = self.jobs.controller() else {
            return;
        };
        if committed && decision.run {
            controller.rechecked(job, decision.generation, decision.constraints);
        } else {
            controller.revoke_job(job, JobReason::PolicyRevoked);
        }
        if committed
            && !decision.run
            && let Some(emitter) = &self.audit_emitter
        {
            let _ = emitter.try_emit(denied_event(&decision));
        }
    }

    /// Offer the owner's next lifecycle record (kind 35 or kind 36).
    fn policy_record_step(&mut self) {
        if self.pending.is_some() || self.closing {
            return;
        }
        let (Some(owner), Some(replay)) = (self.policy.as_mut(), self.replay.as_ref()) else {
            return;
        };
        let Some((record, commit)) = owner.take_record(&replay.policy) else {
            return;
        };
        if let Err(e) = self.append(record, Job::PolicyLifecycle(commit), None) {
            if let Some(owner) = self.policy.as_mut() {
                owner.record_done(commit, Err(()));
            }
            if e != writer::Error::Busy {
                self.unavailable = Some(storage_error(e));
            }
        }
    }

    /// A lifecycle record committed (or failed).
    pub(super) fn policy_lifecycle_done(&mut self, commit: Commit, committed: bool) {
        let seq = self.replay.as_ref().map_or(0, |r| r.policy.last_seq);
        if let Some(owner) = self.policy.as_mut() {
            owner.record_done(commit, if committed { Ok(seq) } else { Err(()) });
        }
    }
}
