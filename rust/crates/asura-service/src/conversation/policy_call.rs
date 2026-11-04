//! The policy decision of one model `exec_cmd` call (security-policy.md,
//! PL1 implementation contract, "The model call step"): after the EX11
//! setting and the secret rewrite, before job admission. The conversation
//! owner appends kind 35 (when the replay's active revision differs), then
//! kind 34, then kind 14, as consecutive appends.
use crate::policy::{JobDecision, JobRequest, PolicyOwner, Ticket};
use asura_policy as ap;
use asura_storage::authority::conversation as journal;

/// Where one call stands in its policy decision.
#[derive(Debug, Default)]
pub(super) enum Stage {
    /// No request yet.
    #[default]
    Idle,
    /// Waiting for the owner's reply.
    Waiting(Ticket),
    /// Decided; kind 35 and kind 34 are appended next, as needed.
    Decided(Box<JobDecision>),
    /// Kind 34 committed (or not needed); kind 14 is appended next.
    Recorded(Box<JobDecision>),
}

/// What the owner does next for the call.
pub(super) enum Next {
    /// Wait for a reply or a commit.
    Wait,
    /// The agent's decision queue is full: kind 14 `busy` `policy_busy`.
    Busy,
    /// Append this record (kind 35 or kind 34); the commit advances.
    Append(journal::Record, bool),
    /// Kind 14 next, with this decision.
    Ready(Box<JobDecision>),
}

/// One step of the call's decision. `revision` is the replay's active
/// kind 35 (sequence, digest).
pub(super) fn step(
    stage: &mut Stage,
    owner: &mut PolicyOwner,
    request: impl FnOnce() -> JobRequest,
    revision: Option<(u64, [u8; 32])>,
    last_seq: u64,
    subject: journal::PolicySubject,
) -> Next {
    loop {
        match std::mem::take(stage) {
            Stage::Idle => match owner.decide_job(request()) {
                Some(ticket) => {
                    *stage = Stage::Waiting(ticket);
                    return Next::Wait;
                }
                None => return Next::Busy,
            },
            Stage::Waiting(ticket) => match owner.take(ticket) {
                Some(decision) => *stage = Stage::Decided(Box::new(decision)),
                None => {
                    *stage = Stage::Waiting(ticket);
                    return Next::Wait;
                }
            },
            Stage::Decided(decision) => {
                if decision.unavailable {
                    *stage = Stage::Recorded(decision.clone());
                    return Next::Ready(decision);
                }
                match revision {
                    Some((seq, digest)) if digest == decision.revision_digest => {
                        let record = decision_record(&decision, seq, subject);
                        *stage = Stage::Decided(decision);
                        return Next::Append(record, true);
                    }
                    _ => {
                        let Some(manifest) = owner.manifest() else {
                            *stage = Stage::Decided(decision);
                            return Next::Wait;
                        };
                        let record = revision_record(
                            &manifest,
                            last_seq + 1,
                            revision.map_or(0, |(seq, _)| seq),
                        );
                        *stage = Stage::Decided(decision);
                        return Next::Append(record, false);
                    }
                }
            }
            Stage::Recorded(decision) => {
                *stage = Stage::Recorded(decision.clone());
                return Next::Ready(decision);
            }
        }
    }
}

/// The kind 34 commit: the decision is recorded.
pub(super) fn recorded(stage: &mut Stage) {
    if let Stage::Decided(decision) = std::mem::take(stage) {
        *stage = Stage::Recorded(decision);
    }
}

/// The requester's cancel.
pub(super) fn cancel(stage: &mut Stage, owner: Option<&mut PolicyOwner>) {
    if let (Stage::Waiting(ticket), Some(owner)) = (&*stage, owner) {
        owner.cancel(*ticket);
    }
    *stage = Stage::Idle;
}

fn scope_code(scope: Option<ap::Scope>) -> u8 {
    scope.map_or(0, ap::Scope::code)
}
fn error_code(error: Option<ap::ErrorClass>) -> u8 {
    error.map_or(0, |e| e as u8)
}

/// Kind 34 for one decision (security-policy.md, kinds 34 and 35 in PL1).
fn decision_record(
    decision: &JobDecision,
    revision_seq: u64,
    subject: journal::PolicySubject,
) -> journal::Record {
    let primary = decision.primary.as_ref();
    let origin = match decision.profile {
        ap::ProfileKind::Model1 => ap::Origin::Model,
        ap::ProfileKind::Person1 | ap::ProfileKind::PersonOutside1 => ap::Origin::PersonTyped,
    };
    journal::Record::PolicyDecision(Box::new(journal::PolicyDecision {
        subject,
        action: decision.action.code(),
        origin: origin.code(),
        profile: decision.profile.code(),
        project: decision.project,
        task_conversation: decision.task,
        task_agent: 0,
        resource_digest: decision.resource_digest,
        request_digest: decision.request_digest,
        intent_digest: decision.intent_digest,
        revision_seq,
        revision_digest: decision.revision_digest,
        scopes: decision
            .scopes
            .iter()
            .map(|s| journal::PolicyScopeResult {
                scope: s.scope.code(),
                allow: s.allow,
                error: error_code(s.error),
                rules: s.rules.iter().take(8).cloned().collect(),
            })
            .collect(),
        outcome: if decision.run {
            journal::PolicyOutcome::Run
        } else {
            journal::PolicyOutcome::Deny
        },
        primary_scope: scope_code(primary.and_then(|p| p.scope)),
        reason: primary.map_or_else(|| "allowed".into(), |p| p.reason.clone()),
        constraint_digest: decision.constraints.map(|c| c.digest()),
        now_ms: u64::try_from(decision.now_ms).unwrap_or(0),
    }))
}

/// Kind 35 `activated` for the running build's built-in revision.
fn revision_record(
    manifest: &crate::policy::Manifest,
    seq: u64,
    previous_seq: u64,
) -> journal::Record {
    journal::Record::PolicyRevision(Box::new(journal::PolicyRevision {
        seq,
        digest: manifest.digest,
        previous_seq,
        event: journal::PolicyRevisionEvent::Activated,
        schema_digest: ap::schema_digest(),
        cedar_lang: ap::cedar_lang(),
        cedar_sdk: ap::CEDAR_SDK.into(),
        solver_digest: None,
        admission_digest: None,
        scopes: manifest
            .scopes
            .iter()
            .map(|(scope, digest, source, ids)| journal::PolicyScopeSet {
                scope: scope.code(),
                project: None,
                text_digest: *digest,
                source: (*source).into(),
                // Kind 35 holds the scope-local `@id`; the scope is its own field.
                policy_ids: ids
                    .iter()
                    .map(|id| id.rsplit(':').next().unwrap_or(id).to_owned())
                    .collect(),
            })
            .collect(),
    }))
}

/// Kind 14's policy refusal fields for a refusal (security-policy.md,
/// kind 14 additions). `None` for a `run` decision.
pub(super) fn refusal(decision: &JobDecision) -> Option<journal::PolicyRefusal> {
    if decision.run {
        return None;
    }
    let primary = decision.primary.as_ref()?;
    let explain = primary
        .explain
        .as_deref()
        .filter(|_| {
            matches!(
                primary.scope,
                Some(ap::Scope::Installation | ap::Scope::User)
            )
        })
        .filter(|e| e.len() <= 200 && e.bytes().all(|b| (0x20..=0x7e).contains(&b)))
        .unwrap_or_default()
        .to_owned();
    Some(journal::PolicyRefusal {
        scope: scope_code(primary.scope),
        reason: primary.reason.clone(),
        rule: primary.rule.clone().unwrap_or_default(),
        explain,
    })
}

#[cfg(all(test, feature = "test-support"))]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// The kind 35 and kind 34 records built from a real decision of the
    /// built-in sets encode under the journal's validation, and a refusal
    /// gives valid kind 14 policy fields.
    #[test]
    fn records_from_a_real_decision_encode() {
        for (texts, run) in [
            (ap::RevisionTexts::builtin(), true),
            (ap::RevisionTexts::builtin_test_deny(), false),
        ] {
            let mut owner = PolicyOwner::with_texts(texts, "/Users/p/.asura".into(), None, 2);
            let request = || {
                let mut r = crate::policy::test_request("records", 3, 4, "printf ok");
                r.project = [5; 16];
                r
            };
            let mut stage = Stage::Idle;
            let end = Instant::now() + Duration::from_secs(30);
            let record = loop {
                assert!(Instant::now() < end, "no decision");
                owner.poll();
                match step(
                    &mut stage,
                    &mut owner,
                    request,
                    None,
                    0,
                    journal::PolicySubject::ModelCall {
                        operation: [6; 16],
                        generation: 1,
                        ordinal: 1,
                    },
                ) {
                    Next::Append(record, false) => break record,
                    Next::Wait => std::thread::sleep(Duration::from_millis(1)),
                    _ => panic!("unexpected step"),
                }
            };
            let context = journal::FrameContext {
                installation_id: [1; 16],
                transition_id: [2; 16],
                sequence: 1,
                prior_digest: [0; 32],
                expected_revision: 0,
                owner_generation: 1,
            };
            journal::encode_frame(&context, &record)
                .unwrap_or_else(|e| panic!("kind 35 {e:?}: {record:?}"));
            let journal::Record::PolicyRevision(revision) = &record else {
                panic!("kind 35 first")
            };
            let digest = revision.digest;
            let Next::Append(decision, true) = step(
                &mut stage,
                &mut owner,
                request,
                Some((1, digest)),
                1,
                journal::PolicySubject::ModelCall {
                    operation: [6; 16],
                    generation: 1,
                    ordinal: 1,
                },
            ) else {
                panic!("kind 34 next")
            };
            journal::encode_frame(&context, &decision).expect("kind 34 encodes");
            recorded(&mut stage);
            let Next::Ready(ready) = step(
                &mut stage,
                &mut owner,
                request,
                None,
                1,
                journal::PolicySubject::ModelCall {
                    operation: [6; 16],
                    generation: 1,
                    ordinal: 1,
                },
            ) else {
                panic!("ready")
            };
            assert_eq!(ready.run, run);
            if !run {
                let fields = refusal(&ready).expect("refusal fields");
                assert_eq!(fields.scope, 1);
                assert_eq!(fields.reason, "test_denied");
            }
            owner.shutdown();
            while !owner.settled() {
                std::thread::sleep(Duration::from_millis(1));
            }
        }
    }
}
