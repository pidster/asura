//! The revision lifecycle of the policy owner (security-policy.md, PL3
//! implementation contract): the admission queue, restart and reproving,
//! rollback, activation with revision fencing, and the kind 35 and kind 36
//! records and audit events that the conversation owner appends and
//! emits. Every method here is a bounded in-memory step on the reactor;
//! the blocking work runs on the admission thread ([`super::admission`]).
use super::admission::{self, Done, SolverState, StoredText, Work};
use super::{PolicyOwner, Revision, State, UserScope};
use asura_policy as ap;
use asura_policy_admission as adm;
use asura_storage::audit::Event;
use asura_storage::authority::conversation as journal;
use std::collections::VecDeque;

/// Candidates that wait for admission (AE-03).
pub(crate) const ADMISSION_QUEUE: usize = 16;
/// Detail lines and counterexamples kept per candidate.
const LINES: usize = 8;
const LINE_BYTES: usize = 512;

/// The replay facts the owner needs at restart (PL3 restart item 1).
#[derive(Clone, Debug, Default)]
pub(crate) struct RestoreFacts {
    pub active: Option<journal::PolicyRevision>,
    pub open_grants: Vec<journal::PolicyGrant>,
}

/// One committed activation, for the conversation owner's rechecks.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Activation {
    /// The new dispatch generation.
    pub generation: u64,
    /// The change may deny what an earlier decision allowed.
    pub narrows: bool,
}

/// What a record that the owner offered does on its commit.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Commit {
    Activated,
    Rejected,
    GrantCancelled,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Phase {
    Queued,
    /// A rollback reads its stored text first.
    Loading,
    Proving,
    Activated(u64),
    Rejected,
    Cancelled,
}

/// One candidate and its result for `PolicyStatus`.
#[derive(Clone, Debug)]
struct Entry {
    id: [u8; 16],
    scope: ap::Scope,
    origin: admission::Origin,
    /// `None` until a rollback's stored text is read.
    text: Option<String>,
    stored: Option<StoredText>,
    phase: Phase,
    reason: String,
    details: Vec<String>,
    counterexamples: Vec<String>,
    widening: Vec<String>,
    queries: u32,
    proof_ms: u64,
}
impl Entry {
    fn new(scope: ap::Scope, origin: admission::Origin, text: Option<String>) -> Self {
        Self {
            id: asura_platform::random_id(),
            scope,
            origin,
            text,
            stored: None,
            phase: Phase::Queued,
            reason: String::new(),
            details: Vec::new(),
            counterexamples: Vec::new(),
            widening: Vec::new(),
            queries: 0,
            proof_ms: 0,
        }
    }
}

/// A ready candidate's revision while it installs and activates.
pub(super) struct Next {
    pub(super) revision: Revision,
    offered: bool,
    narrows: bool,
    widening: bool,
}

enum Out {
    Activated,
    Rejected(Box<Rejection>),
    GrantCancelled(Box<journal::PolicyGrant>),
}
struct Rejection {
    digest: [u8; 32],
    user_text: [u8; 32],
    policy_ids: Vec<String>,
    solver_digest: Option<[u8; 32]>,
    admission_digest: [u8; 32],
}

/// The restart facts that wait for the solver state or the text read.
struct Restoring {
    manifest: journal::PolicyRevision,
    loaded: Option<String>,
}

pub(super) struct Life {
    worker: admission::Worker,
    runtime: Option<asura_platform::RuntimeDirectory>,
    source: admission::SolverSource,
    limits: asura_platform::solver::SolverLimits,
    wake: Option<asura_platform::events::WakeSender>,
    solver: Option<(SolverState, Option<[u8; 32]>)>,
    queue: VecDeque<Entry>,
    /// The candidate on the admission thread, or the one activating.
    current: Option<Entry>,
    /// The last finished candidates, newest first.
    finished: VecDeque<Entry>,
    token: u64,
    running: Option<u64>,
    /// The restore text read in flight.
    loading: Option<u64>,
    restoring: Option<Restoring>,
    records: VecDeque<Out>,
    offered: Option<Commit>,
    audits: Vec<Event>,
    activations: VecDeque<Activation>,
}
impl Life {
    pub(super) fn start(
        options: &super::Options,
        wake: Option<asura_platform::events::WakeSender>,
    ) -> Self {
        Self {
            worker: admission::Worker::start(
                options.runtime.clone(),
                options.source.clone(),
                options.limits,
                wake.clone(),
            ),
            runtime: options.runtime.clone(),
            source: options.source.clone(),
            limits: options.limits,
            wake,
            solver: None,
            queue: VecDeque::new(),
            current: None,
            finished: VecDeque::new(),
            token: 0,
            running: None,
            loading: None,
            restoring: None,
            records: VecDeque::new(),
            offered: None,
            audits: Vec::new(),
            activations: VecDeque::new(),
        }
    }
    pub(super) fn solver_state(&self) -> SolverState {
        self.solver.map_or(SolverState::None, |(s, _)| s)
    }
    pub(super) fn queue_len(&self) -> u32 {
        self.queue.len() as u32
    }
    pub(super) fn audit_worker_failed(&mut self, index: usize) {
        self.audits.push(Event::PolicyWorkerFailed {
            worker: u32::try_from(index).unwrap_or(u32::MAX),
        });
    }
    pub(super) fn close(&mut self) {
        self.worker.close();
    }
    pub(super) fn settled(&mut self) -> bool {
        self.worker.settled()
    }
    /// The current or last candidate for `PolicyStatus`.
    pub(super) fn candidate_status(&self) -> Option<asura_control::pb::PolicyCandidate> {
        let (entry, position) = match (&self.current, self.queue.front()) {
            (Some(entry), _) => (entry, 0),
            (None, Some(entry)) => (entry, 1),
            (None, None) => (self.finished.front()?, 0),
        };
        Some(candidate_pb(entry, position))
    }
    /// The last finished candidates for `PolicyStatus.recent`, newest
    /// first, so that a client sees its result when another candidate
    /// starts in the same step.
    pub(super) fn recent_status(&self) -> Vec<asura_control::pb::PolicyCandidate> {
        self.finished.iter().map(|e| candidate_pb(e, 0)).collect()
    }
    fn position(&self, id: [u8; 16]) -> u32 {
        let ahead = usize::from(self.current.is_some());
        self.queue
            .iter()
            .position(|e| e.id == id)
            .map_or(0, |at| (at + 1 + ahead) as u32)
    }
    fn finish(&mut self, mut entry: Entry, phase: Phase) {
        entry.phase = phase;
        entry.text = None;
        self.finished.push_front(entry);
        self.finished
            .truncate(asura_control::policy::MAX_RECENT_CANDIDATES);
    }
}

fn candidate_pb(entry: &Entry, position: u32) -> asura_control::pb::PolicyCandidate {
    asura_control::pb::PolicyCandidate {
        candidate_id: Some(entry.id.to_vec()),
        state: Some(match entry.phase {
            Phase::Queued => 1,
            Phase::Loading | Phase::Proving => 2,
            Phase::Activated(_) => 3,
            Phase::Rejected => 4,
            Phase::Cancelled => 5,
        }),
        scope: Some(u32::from(entry.scope.code())),
        position: Some(position),
        reason: (!entry.reason.is_empty()).then(|| entry.reason.clone()),
        details: entry.details.clone(),
        counterexamples: entry.counterexamples.clone(),
        revision: match entry.phase {
            Phase::Activated(seq) => Some(seq),
            _ => None,
        },
        widening: matches!(entry.phase, Phase::Activated(_)).then(|| !entry.widening.is_empty()),
        widening_examples: entry.widening.clone(),
        queries: Some(entry.queries),
        proof_ms: Some(entry.proof_ms),
    }
}

/// Bounded printable lines for the person.
fn lines(items: impl IntoIterator<Item = String>) -> Vec<String> {
    items
        .into_iter()
        .take(LINES)
        .map(|line| {
            let mut out = String::new();
            for c in line.chars() {
                let c = if (' '..='~').contains(&c) { c } else { '?' };
                if out.len() + 1 > LINE_BYTES {
                    break;
                }
                out.push(c);
            }
            out
        })
        .collect()
}

fn builtin_text(scope: ap::Scope) -> String {
    ap::RevisionTexts::builtin()
        .scopes
        .into_iter()
        .find(|s| s.scope == scope)
        .map(|s| s.text)
        .unwrap_or_default()
}
fn digest(text: &str) -> [u8; 32] {
    use sha2::Digest;
    sha2::Sha256::digest(text.as_bytes()).into()
}

impl PolicyOwner {
    /// The replay facts at restart (PL3 restart and reproving). Until they
    /// arrive, requests wait in their agent queues.
    pub(crate) fn restore(&mut self, facts: RestoreFacts) {
        if self.user != UserScope::Restoring {
            return;
        }
        for grant in facts.open_grants {
            if grant.event == journal::PolicyGrantEvent::Requested {
                self.life
                    .records
                    .push_back(Out::GrantCancelled(Box::new(grant)));
            }
        }
        let Some(manifest) = facts.active else {
            self.user = UserScope::Ok;
            return;
        };
        let user = manifest.scopes.iter().find(|s| s.scope == 2);
        match user {
            Some(set) if set.source == "person" => {
                self.user = UserScope::Loading;
                self.life.token += 1;
                let token = self.life.token;
                let text = StoredText {
                    digest: set.text_digest,
                    builtin: false,
                };
                self.life.restoring = Some(Restoring {
                    manifest,
                    loaded: None,
                });
                if self.life.worker.send(Work::Load {
                    token,
                    text,
                    scope: ap::Scope::User,
                }) {
                    self.life.loading = Some(token);
                } else {
                    self.user = UserScope::Unavailable;
                }
            }
            _ => self.user = UserScope::Ok,
        }
    }
    /// `PolicyCandidateSubmit`: queue a user-scope text. The reply is the
    /// candidate ID and its position, or an error code.
    pub(crate) fn submit_candidate(
        &mut self,
        scope: ap::Scope,
        text: String,
    ) -> Result<([u8; 16], u32), &'static str> {
        if self.state == State::Closing {
            return Err("unavailable");
        }
        if scope != ap::Scope::User {
            return Err("scope_invalid");
        }
        if self.life.queue.len() >= ADMISSION_QUEUE {
            return Err("busy");
        }
        let entry = Entry::new(scope, admission::Origin::Person, Some(text));
        let id = entry.id;
        self.life.queue.push_back(entry);
        self.notify();
        Ok((id, self.life.position(id)))
    }
    /// `PolicyRollback`: queue an earlier activated user text, one of the
    /// replay's last 64 activated revisions.
    pub(crate) fn rollback(
        &mut self,
        scope: ap::Scope,
        seq: u64,
        recent: &[journal::RecentRevision],
    ) -> Result<([u8; 16], u32), &'static str> {
        if self.state == State::Closing {
            return Err("unavailable");
        }
        if scope != ap::Scope::User {
            return Err("scope_invalid");
        }
        let Some(recent) = recent.iter().find(|r| r.seq == seq) else {
            return Err("unknown_revision");
        };
        if self.life.queue.len() >= ADMISSION_QUEUE {
            return Err("busy");
        }
        let builtin = recent.user_source == "builtin";
        let mut entry = Entry::new(
            scope,
            admission::Origin::Rollback(seq),
            builtin.then(|| builtin_text(ap::Scope::User)),
        );
        entry.stored = Some(StoredText {
            digest: recent.user_text,
            builtin,
        });
        let id = entry.id;
        self.life.queue.push_back(entry);
        self.notify();
        Ok((id, self.life.position(id)))
    }
    /// `PolicyCancel`: drop a queued candidate, or cancel the running
    /// admission (its solver is killed and reaped).
    pub(crate) fn cancel_candidate(&mut self, id: [u8; 16]) -> bool {
        if let Some(at) = self.life.queue.iter().position(|e| e.id == id) {
            if let Some(entry) = self.life.queue.remove(at) {
                self.life.finish(entry, Phase::Cancelled);
            }
            return true;
        }
        match &self.life.current {
            Some(entry) if entry.id == id && self.next.is_none() => {
                self.life.worker.cancel.cancel();
                true
            }
            _ => false,
        }
    }
    /// The next lifecycle record to append, built with the replay's facts
    /// at this moment (PL3 activation and fencing).
    pub(crate) fn take_record(
        &mut self,
        replay: &journal::PolicyReplay,
    ) -> Option<(journal::Record, Commit)> {
        if self.life.offered.is_some() {
            return None;
        }
        let last = replay.last_seq;
        let active = replay.active().map_or(0, |(seq, _)| seq);
        let (record, commit) = match self.life.records.front()? {
            Out::Activated => {
                let next = self.next.as_ref()?;
                let manifest = next.revision.manifest()?;
                (
                    crate::conversation::revision_record(&manifest, last + 1, active),
                    Commit::Activated,
                )
            }
            Out::Rejected(r) => (
                journal::Record::PolicyRevision(Box::new(journal::PolicyRevision {
                    seq: last + 1,
                    digest: r.digest,
                    previous_seq: active,
                    event: journal::PolicyRevisionEvent::Rejected,
                    schema_digest: ap::schema_digest(),
                    cedar_lang: ap::cedar_lang(),
                    cedar_sdk: ap::CEDAR_SDK.into(),
                    solver_digest: r.solver_digest,
                    admission_digest: Some(r.admission_digest),
                    scopes: vec![
                        journal::PolicyScopeSet {
                            scope: 1,
                            project: None,
                            text_digest: self
                                .dispatch
                                .texts
                                .text_digest(ap::Scope::Installation)
                                .unwrap_or([0; 32]),
                            source: "builtin".into(),
                            policy_ids: self
                                .dispatch
                                .policy_ids
                                .as_ref()
                                .and_then(|ids| {
                                    ids.iter().find(|(s, _)| *s == ap::Scope::Installation)
                                })
                                .map(|(_, ids)| local_ids(ids))
                                .unwrap_or_default(),
                        },
                        journal::PolicyScopeSet {
                            scope: 2,
                            project: None,
                            text_digest: r.user_text,
                            source: "person".into(),
                            policy_ids: local_ids(&r.policy_ids),
                        },
                    ],
                })),
                Commit::Rejected,
            ),
            Out::GrantCancelled(grant) => {
                let mut record = (**grant).clone();
                record.event = journal::PolicyGrantEvent::Cancelled;
                record.cause = 1;
                record.request_digest = None;
                record.constraint_digest = None;
                record.intent_digest = None;
                record.expiry_ms = None;
                record.revision_seq = active;
                (
                    journal::Record::PolicyGrant(Box::new(record)),
                    Commit::GrantCancelled,
                )
            }
        };
        self.life.offered = Some(commit);
        Some((record, commit))
    }
    /// The commit or failure of the offered record.
    pub(crate) fn record_done(&mut self, commit: Commit, result: Result<u64, ()>) {
        if self.life.offered != Some(commit) {
            return;
        }
        self.life.offered = None;
        self.life.records.pop_front();
        if commit != Commit::Activated {
            return;
        }
        let Some(next) = self.next.take() else {
            return;
        };
        let entry = self.life.current.take();
        match result {
            Ok(seq) => {
                let scopes_changed = vec![asura_storage::audit::PolicyScope::User];
                self.life.audits.push(Event::PolicyRevisionActivated {
                    seq,
                    scopes: scopes_changed,
                    widening: next.widening,
                    solver_digest: next.revision.solver_digest.map(|d| ap::hex(&d)),
                });
                self.dispatch = next.revision;
                self.cache.clear();
                self.cache_order.clear();
                self.user = UserScope::Ok;
                self.life.activations.push_back(super::Activation {
                    generation: self.dispatch.generation,
                    narrows: next.narrows,
                });
                if let Some(entry) = entry {
                    self.life.finish(entry, Phase::Activated(seq));
                }
            }
            Err(()) => {
                if let Some(mut entry) = entry {
                    if entry.origin == admission::Origin::Reprove {
                        self.user = UserScope::Unavailable;
                    }
                    entry.reason = "storage_unavailable".into();
                    self.life.finish(entry, Phase::Rejected);
                }
            }
        }
    }
    /// The audit events to emit.
    pub(crate) fn take_audit(&mut self) -> Vec<Event> {
        std::mem::take(&mut self.life.audits)
    }
    /// The next committed activation, for the rechecks.
    pub(crate) fn take_activation(&mut self) -> Option<Activation> {
        self.life.activations.pop_front()
    }
    /// The next revision failed to install: its candidate is refused.
    pub(super) fn next_failed(&mut self) {
        if self.next.take().is_some()
            && let Some(mut entry) = self.life.current.take()
        {
            entry.reason = "candidate_invalid".into();
            if entry.origin == admission::Origin::Reprove {
                self.user = UserScope::Unavailable;
            }
            self.life.finish(entry, Phase::Rejected);
        }
    }
    /// One lifecycle step: admission replies, thread loss, the next
    /// admission, and the activation offer once every worker installed.
    pub(super) fn step_lifecycle(&mut self) {
        while let Some(done) = self.life.worker.try_recv() {
            self.admission_done(done);
        }
        if self.state != State::Closing && self.life.worker.lost() {
            tracing::warn!(stage = "policy", "policy_admission_thread_failed");
            self.life.worker = admission::Worker::start(
                self.life.runtime.clone(),
                self.life.source.clone(),
                self.life.limits,
                self.life.wake.clone(),
            );
            self.life.solver = None;
            if self.life.running.take().is_some()
                && let Some(mut entry) = self.life.current.take()
            {
                entry.reason = "solver_failed".into();
                self.life.audits.push(Event::PolicySolverFailed {
                    cause: asura_storage::audit::PolicySolverCause::Io,
                });
                if entry.origin == admission::Origin::Reprove {
                    self.user = UserScope::Unavailable;
                }
                self.life.finish(entry, Phase::Rejected);
            }
            if self.life.loading.take().is_some() {
                self.user = UserScope::Unavailable;
            }
        }
        self.restore_step();
        // Offer the activation once every live worker installed it.
        if let Some(next) = &self.next
            && !next.offered
            && next.revision.policy_ids.is_some()
            && self
                .workers
                .iter()
                .all(|w| w.installed >= next.revision.generation || w.handle.is_none())
        {
            if let Some(next) = self.next.as_mut() {
                next.offered = true;
            }
            self.life.records.push_back(Out::Activated);
        }
        self.start_admission();
    }
    /// Restart: compare the loaded text's recorded digests with the running
    /// build once the solver state is known.
    fn restore_step(&mut self) {
        let Some(solver) = self.life.solver else {
            return;
        };
        let Some(restoring) = self.life.restoring.as_mut() else {
            return;
        };
        let Some(text) = restoring.loaded.take() else {
            return;
        };
        let manifest = self.life.restoring.take().expect("restoring").manifest;
        let builtin = ap::RevisionTexts::builtin();
        let installation = builtin
            .text_digest(ap::Scope::Installation)
            .unwrap_or([0; 32]);
        let same = manifest.schema_digest == ap::schema_digest()
            && manifest.cedar_lang == ap::cedar_lang()
            && manifest.cedar_sdk == ap::CEDAR_SDK
            && manifest.solver_digest == solver.1
            && manifest
                .scopes
                .iter()
                .any(|s| s.scope == 1 && s.text_digest == installation);
        let mut texts = self.dispatch.texts.as_ref().clone();
        if let Some(user) = texts.scopes.iter_mut().find(|s| s.scope == ap::Scope::User) {
            user.text = text.clone();
            user.source = "person";
        }
        if same && texts.digest() == manifest.digest {
            let generation = self.next_generation_id;
            self.next_generation_id += 1;
            let mut revision = Revision::new(generation, texts);
            revision.solver_digest = manifest.solver_digest;
            revision.admission_digest = manifest.admission_digest;
            for worker in &mut self.workers {
                super::install(worker, generation, revision.texts.clone());
            }
            self.dispatch = revision;
            if self.state == State::Active {
                self.state = State::Starting;
            }
            self.user = UserScope::Ok;
        } else {
            self.user = UserScope::Reproving;
            let entry = Entry::new(ap::Scope::User, admission::Origin::Reprove, Some(text));
            self.life.queue.push_front(entry);
        }
    }
    fn admission_done(&mut self, done: Done) {
        match done {
            Done::Solver(state, digest) => self.life.solver = Some((state, digest)),
            Done::Loaded { token, result } => {
                if self.life.loading == Some(token) {
                    self.life.loading = None;
                    match result {
                        Ok(text) => {
                            if let Some(restoring) = self.life.restoring.as_mut() {
                                restoring.loaded = Some(text);
                            }
                        }
                        Err(reason) => {
                            tracing::warn!(
                                stage = "policy",
                                reason,
                                "policy_user_scope_unavailable"
                            );
                            self.life.restoring = None;
                            self.user = UserScope::Unavailable;
                        }
                    }
                    return;
                }
                // A rollback's stored text.
                if self.life.running != Some(token) {
                    return;
                }
                self.life.running = None;
                let Some(mut entry) = self.life.current.take() else {
                    return;
                };
                match result {
                    Ok(text) => {
                        entry.text = Some(text);
                        entry.phase = Phase::Queued;
                        self.life.queue.push_front(entry);
                    }
                    Err(reason) => {
                        entry.reason = "storage_unavailable".into();
                        entry.details = lines([reason]);
                        self.life.finish(entry, Phase::Rejected);
                    }
                }
            }
            Done::Admitted { token, result } => {
                if self.life.running != Some(token) {
                    return;
                }
                self.life.running = None;
                self.admitted(*result);
            }
        }
    }
    fn admitted(&mut self, result: admission::Admitted) {
        let Some(mut entry) = self.life.current.take() else {
            return;
        };
        let admission = result.admission;
        tracing::info!(
            stage = "policy",
            queries = admission.queries.len(),
            peak_footprint = result.peak_footprint,
            "policy_admission_done"
        );
        entry.queries = u32::try_from(admission.queries.len()).unwrap_or(u32::MAX);
        entry.proof_ms = admission.queries.iter().map(|q| q.micros).sum::<u64>() / 1000;
        let text = entry.text.clone().unwrap_or_default();
        let user_text = digest(&text);
        let mut texts = self.dispatch.texts.as_ref().clone();
        if let Some(user) = texts.scopes.iter_mut().find(|s| s.scope == ap::Scope::User) {
            user.text = text;
            user.source = "person";
        }
        let reason = match (&admission.outcome, &result.pinned) {
            (adm::Outcome::Ready, Some(Ok(_))) => None,
            (adm::Outcome::Ready, _) => {
                Some(("storage_unavailable".to_owned(), Vec::new(), Vec::new()))
            }
            (
                adm::Outcome::Rejected {
                    reason,
                    details,
                    counterexamples,
                    ..
                },
                _,
            ) => Some((
                reason.as_str().to_owned(),
                details.clone(),
                counterexamples.clone(),
            )),
        };
        match reason {
            None => {
                entry.widening = lines(admission.widening.clone());
                let generation = self.next_generation_id;
                self.next_generation_id += 1;
                let mut revision = Revision::new(generation, texts);
                revision.solver_digest = result.solver_digest;
                revision.admission_digest = Some(admission.digest);
                for worker in &mut self.workers {
                    super::install(worker, generation, revision.texts.clone());
                }
                self.next = Some(Next {
                    revision,
                    offered: false,
                    narrows: admission.narrows || entry.origin == admission::Origin::Reprove,
                    widening: !admission.widening.is_empty(),
                });
                self.life.current = Some(entry);
            }
            Some((reason, details, counterexamples)) if reason == "cancelled" => {
                entry.details = lines(details);
                entry.counterexamples = lines(counterexamples);
                if entry.origin == admission::Origin::Reprove {
                    self.user = UserScope::Unavailable;
                }
                self.life.finish(entry, Phase::Cancelled);
            }
            Some((reason, details, counterexamples)) => {
                if let Some(cause) = result.solver_cause.and_then(solver_cause) {
                    self.life.audits.push(Event::PolicySolverFailed { cause });
                }
                self.life.audits.push(Event::PolicyRevisionRejected {
                    scope: audit_scope(Some(entry.scope)),
                    reason: reason.clone(),
                });
                if !admission.stderr_tail.is_empty() {
                    tracing::warn!(
                        stage = "policy",
                        bytes = admission.stderr_tail.len(),
                        "policy_solver_stderr"
                    );
                }
                self.life
                    .records
                    .push_back(Out::Rejected(Box::new(Rejection {
                        digest: texts.digest(),
                        user_text,
                        policy_ids: admission.policy_ids.clone(),
                        solver_digest: result.solver_digest,
                        admission_digest: admission.digest,
                    })));
                entry.reason = reason;
                entry.details = lines(details);
                entry.counterexamples = lines(counterexamples);
                if entry.origin == admission::Origin::Reprove {
                    self.user = UserScope::Unavailable;
                }
                self.life.finish(entry, Phase::Rejected);
            }
        }
    }
    /// Send the next candidate to the admission thread: one at a time, and
    /// only while no revision activates.
    fn start_admission(&mut self) {
        if self.state == State::Closing
            || self.life.running.is_some()
            || self.life.current.is_some()
            || self.next.is_some()
            || self.life.solver.is_none()
            || matches!(self.user, UserScope::Restoring | UserScope::Loading)
            || self.dispatch.policy_ids.is_none()
        {
            return;
        }
        let Some(mut entry) = self.life.queue.pop_front() else {
            return;
        };
        self.life.token += 1;
        let token = self.life.token;
        let work = match (&entry.text, &entry.stored) {
            (None, Some(stored)) => {
                entry.phase = Phase::Loading;
                Work::Load {
                    token,
                    text: stored.clone(),
                    scope: entry.scope,
                }
            }
            (Some(text), _) => {
                entry.phase = Phase::Proving;
                let installation = self
                    .dispatch
                    .texts
                    .scopes
                    .iter()
                    .find(|s| s.scope == ap::Scope::Installation)
                    .map(|s| s.text.clone())
                    .unwrap_or_default();
                let previous = (entry.origin != admission::Origin::Reprove
                    && self.user == UserScope::Ok)
                    .then(|| {
                        self.dispatch
                            .texts
                            .scopes
                            .iter()
                            .find(|s| s.scope == ap::Scope::User)
                            .map(|s| s.text.clone())
                    })
                    .flatten();
                Work::Admit {
                    token,
                    scope: entry.scope,
                    text: text.clone(),
                    installation,
                    user_default: builtin_text(ap::Scope::User),
                    previous,
                }
            }
            (None, None) => {
                entry.reason = "text_invalid".into();
                self.life.finish(entry, Phase::Rejected);
                return;
            }
        };
        if self.life.worker.send(work) {
            self.life.running = Some(token);
            self.life.current = Some(entry);
        } else {
            entry.phase = Phase::Queued;
            self.life.queue.push_front(entry);
        }
    }
}

/// Kind 35 holds the scope-local `@id`; the scope is its own field.
fn local_ids(ids: &[String]) -> Vec<String> {
    ids.iter()
        .map(|id| id.rsplit(':').next().unwrap_or(id).to_owned())
        .collect()
}

/// The audit scope of a policy scope; `none` for no scope.
pub(crate) fn audit_scope(scope: Option<ap::Scope>) -> asura_storage::audit::PolicyScope {
    use asura_storage::audit::PolicyScope as A;
    match scope {
        None => A::None,
        Some(ap::Scope::Installation) => A::Installation,
        Some(ap::Scope::User) => A::User,
        Some(ap::Scope::Workspace) => A::Workspace,
        Some(ap::Scope::Repository) => A::Repository,
    }
}
/// The audit cause of a solver failure cause code; `None` for a cancel.
fn solver_cause(cause: &str) -> Option<asura_storage::audit::PolicySolverCause> {
    use asura_storage::audit::PolicySolverCause as C;
    Some(match cause {
        "unavailable" => C::Unavailable,
        "digest_mismatch" => C::DigestMismatch,
        "copy_failed" => C::CopyFailed,
        "spawn_failed" => C::SpawnFailed,
        "closed" => C::Closed,
        "protocol" => C::Protocol,
        "input_bound" => C::InputBound,
        "output_bound" => C::OutputBound,
        "memory_bound" => C::MemoryBound,
        "io" => C::Io,
        _ => return None,
    })
}
