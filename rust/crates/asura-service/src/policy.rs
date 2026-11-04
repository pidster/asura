//! The policy owner (security-policy.md, PL1 implementation contract,
//! "Policy owner", and the PL2 contract): one reactor component that
//! decides model `exec_cmd` jobs and person (`!`, `!!`) job starts with
//! Cedar on a pool of worker threads. An attribute reader thread
//! canonicalises project roots and reads the entry program's code
//! identity (PL2). Each worker holds its own
//! engine built from the revision texts; nothing Cedar is shared between
//! threads. No call here blocks the reactor: engines are built, regions
//! compiled and requests evaluated on the workers, and the reactor only
//! moves typed values.
mod attributes;

use crate::job_controller::AgentKey;
use asura_platform::events::WakeSender;
use asura_platform::exec_cmd::Constraints;
use asura_platform::exec_cmd::split::PlanFacts;
use asura_policy as ap;
use asura_storage::authority::conversation as journal;
pub(crate) use attributes::RootKey;
use sha2::Digest;
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::Arc;
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::thread::JoinHandle;

/// Decision queue bound per agent (AE-03); a full queue is `policy_busy`.
pub(crate) const AGENT_QUEUE: usize = 8;
/// Decision queue bound for the service.
pub(crate) const SERVICE_QUEUE: usize = 512;
/// Requests in flight per worker; the owner holds the rest.
pub(crate) const IN_FLIGHT: usize = 2;
/// Constraint set cache entries.
pub(crate) const CACHE: usize = 256;

/// Workers: `clamp(available_parallelism() / 2, 2, 8)`.
pub(crate) fn worker_count() -> usize {
    std::thread::available_parallelism()
        .map_or(2, |n| n.get() / 2)
        .clamp(2, 8)
}

pub(crate) type Ticket = u64;

/// The person facts of a `!` or `!!` start (PL2 person command decision),
/// from kind 31 and the configuration owner.
#[derive(Clone, Debug, Default)]
pub(crate) struct PersonRequest {
    pub outside: bool,
    /// The terminal table names of kind 31's terminal variables.
    pub env_terminal: Vec<String>,
    /// The declared names in kind 31.
    pub env_declared: Vec<String>,
    pub outside_switch: bool,
    /// The named secrets, in reference order.
    pub secrets: Vec<String>,
}
/// One job decision request: a model `exec_cmd` call, or a person start.
/// Typed, never Cedar JSON from a caller; the worker builds entities from
/// these trusted values.
#[derive(Clone, Debug)]
pub(crate) struct JobRequest {
    pub agent: AgentKey,
    pub job: journal::Id,
    pub project: journal::Id,
    /// The registered project location and its retained identity.
    pub location: String,
    pub device: u64,
    pub inode: u64,
    /// The stored (rewritten) command: the intent's source text.
    pub command: String,
    pub plan: PlanFacts,
    /// Present for a person start.
    pub person: Option<PersonRequest>,
    /// Host wall clock at the request, milliseconds since the Unix epoch.
    pub now_ms: i64,
}
impl JobRequest {
    fn root_key(&self) -> RootKey {
        RootKey {
            project: self.project,
            location: self.location.clone(),
            device: self.device,
            inode: self.inode,
        }
    }
}

/// The reply of `decide_job`. A decision is a value; it authorizes
/// nothing by itself.
#[derive(Clone, Debug)]
pub(crate) struct JobDecision {
    pub run: bool,
    /// The deciding request's action (PL2: `secret_use` when a named
    /// secret denied a person start).
    pub action: ap::Action,
    /// `model-1`, or the person profile.
    pub profile: ap::ProfileKind,
    /// The owner could not decide (no engine): no kind 34 is written.
    pub unavailable: bool,
    pub revision_digest: [u8; 32],
    pub scopes: Vec<ap::ScopeResult>,
    pub primary: Option<ap::Primary>,
    pub constraints: Option<Constraints>,
    pub resource_digest: [u8; 32],
    pub request_digest: [u8; 32],
    pub intent_digest: Option<[u8; 32]>,
    pub now_ms: i64,
    /// The principal's project and task (conversation), for kind 34.
    pub project: journal::Id,
    pub task: journal::Id,
}
impl JobDecision {
    fn refused(revision: [u8; 32], flight: &InFlight, error: ap::ErrorClass, reason: &str) -> Self {
        let now_ms = flight.now_ms;
        Self {
            run: false,
            action: ap::Action::ProcessExec,
            profile: flight.profile,
            unavailable: error == ap::ErrorClass::ScopeUnavailable,
            revision_digest: revision,
            scopes: vec![ap::ScopeResult {
                scope: ap::Scope::Installation,
                allow: false,
                rules: Vec::new(),
                error: Some(error),
            }],
            primary: Some(ap::Primary {
                scope: Some(ap::Scope::Installation),
                reason: reason.into(),
                rule: None,
                explain: None,
            }),
            constraints: None,
            resource_digest: [0; 32],
            request_digest: [0; 32],
            intent_digest: None,
            now_ms,
            project: flight.project,
            task: flight.task,
        }
    }
}
/// One request on a worker.
struct InFlight {
    ticket: Ticket,
    key: CacheKey,
    now_ms: i64,
    project: journal::Id,
    task: journal::Id,
    profile: ap::ProfileKind,
}
impl InFlight {
    fn of(ticket: Ticket, request: &JobRequest, root: &str) -> Self {
        Self {
            ticket,
            key: CacheKey {
                project: request.project,
                root: root.to_owned(),
            },
            now_ms: request.now_ms,
            project: request.project,
            task: request.agent.conversation,
            profile: match &request.person {
                None => ap::ProfileKind::Model1,
                Some(p) if p.outside => ap::ProfileKind::PersonOutside1,
                Some(_) => ap::ProfileKind::Person1,
            },
        }
    }
}

/// Owner state (PL1-S1).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum State {
    Starting,
    Active,
    Restarting,
    Unavailable,
    Closing,
}

/// The facts of the built-in revision that kind 35 records.
#[derive(Clone, Debug)]
pub(crate) struct Manifest {
    pub digest: [u8; 32],
    pub scopes: Vec<(ap::Scope, [u8; 32], &'static str, Vec<String>)>,
}

enum ToWorker {
    Decide {
        ticket: Ticket,
        principal: Box<ap::JobPrincipal>,
        entry: Box<ap::ProgramRef>,
        command: String,
        plan: PlanFacts,
        person: Option<Box<ap::PersonEntry>>,
        now_ms: i64,
        cached: Option<ap::Regions>,
    },
    /// Test-support fault: the worker panics (PL-I12).
    #[cfg(feature = "test-support")]
    Panic,
}
enum FromWorker {
    Installed {
        worker: usize,
        generation: u64,
        result: Result<Vec<(ap::Scope, Vec<String>)>, String>,
    },
    Decided {
        worker: usize,
        generation: u64,
        ticket: Ticket,
        outcome: Box<Outcome>,
    },
}
enum Outcome {
    Model(ap::JobOutcome),
    Person(ap::PersonOutcome),
}

struct Worker {
    sender: Option<SyncSender<ToWorker>>,
    handle: Option<JoinHandle<()>>,
    /// Distinguishes a replacement's replies from its predecessor's.
    generation: u64,
    installed: bool,
    in_flight: Vec<InFlight>,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct CacheKey {
    project: journal::Id,
    /// The canonical root spelling.
    root: String,
}
/// Canonical project root spellings in the owner (PL2 attribute reader).
pub(crate) const ROOTS: usize = 256;

struct Queued {
    ticket: Ticket,
    request: JobRequest,
}

pub(crate) struct PolicyOwner {
    texts: Arc<ap::RevisionTexts>,
    digest: [u8; 32],
    asura_home: String,
    state: State,
    workers: Vec<Worker>,
    replies: Receiver<FromWorker>,
    reply_sender: mpsc::Sender<FromWorker>,
    wake: Option<WakeSender>,
    queues: BTreeMap<AgentKey, VecDeque<Queued>>,
    queued: usize,
    /// The agent served last, for round-robin dispatch.
    last_agent: Option<AgentKey>,
    ready: HashMap<Ticket, JobDecision>,
    /// Tickets whose requester cancelled while the request was in flight.
    cancelled: Vec<Ticket>,
    next_ticket: Ticket,
    next_generation: u64,
    cache: HashMap<CacheKey, ap::Regions>,
    cache_order: VecDeque<CacheKey>,
    policy_ids: Option<Vec<(ap::Scope, Vec<String>)>>,
    reader: attributes::Reader,
    /// The entry program with its identity, once read.
    entry: Option<ap::ProgramRef>,
    /// An entry lookup or refresh is queued or in flight.
    entry_pending: bool,
    roots: HashMap<RootKey, String>,
    root_order: VecDeque<RootKey>,
    /// Root lookups queued or in flight.
    resolving: std::collections::HashSet<RootKey>,
    #[cfg(feature = "test-support")]
    panic_next: bool,
}

impl PolicyOwner {
    /// Starts the workers for the built-in revision, or for the test-only
    /// denying set when `test_deny` (test-support builds only).
    pub(crate) fn start(asura_home: String, wake: Option<WakeSender>, test_deny: bool) -> Self {
        let _ = test_deny;
        #[cfg(feature = "test-support")]
        let texts = if test_deny {
            ap::RevisionTexts::builtin_test_deny()
        } else {
            ap::RevisionTexts::builtin()
        };
        #[cfg(not(feature = "test-support"))]
        let texts = ap::RevisionTexts::builtin();
        Self::with_texts(texts, asura_home, wake, worker_count())
    }
    pub(crate) fn with_texts(
        texts: ap::RevisionTexts,
        asura_home: String,
        wake: Option<WakeSender>,
        workers: usize,
    ) -> Self {
        let digest = texts.digest();
        let (reply_sender, replies) = mpsc::channel();
        let wake_for_reader = wake.clone();
        let mut owner = Self {
            texts: Arc::new(texts),
            digest,
            asura_home,
            state: State::Starting,
            workers: Vec::new(),
            replies,
            reply_sender,
            wake,
            queues: BTreeMap::new(),
            queued: 0,
            last_agent: None,
            ready: HashMap::new(),
            cancelled: Vec::new(),
            next_ticket: 1,
            next_generation: 1,
            cache: HashMap::new(),
            cache_order: VecDeque::new(),
            policy_ids: None,
            reader: attributes::Reader::start(wake_for_reader),
            entry: None,
            entry_pending: false,
            roots: HashMap::new(),
            root_order: VecDeque::new(),
            resolving: std::collections::HashSet::new(),
            #[cfg(feature = "test-support")]
            panic_next: false,
        };
        owner.entry_pending = owner.reader.send(attributes::Lookup::Entry);
        for index in 0..workers {
            let worker = owner.spawn(index);
            owner.workers.push(worker);
        }
        if owner.workers.iter().all(|w| w.handle.is_none()) {
            owner.state = State::Unavailable;
        }
        owner
    }
    fn spawn(&mut self, index: usize) -> Worker {
        let generation = self.next_generation;
        self.next_generation += 1;
        let (sender, receiver) = mpsc::sync_channel::<ToWorker>(IN_FLIGHT);
        let replies = self.reply_sender.clone();
        let texts = self.texts.clone();
        let wake = self.wake.clone();
        let handle = std::thread::Builder::new()
            .name(format!("asura-policy-{index}"))
            .spawn(move || worker_main(index, generation, &texts, receiver, replies, wake));
        match handle {
            Ok(handle) => Worker {
                sender: Some(sender),
                handle: Some(handle),
                generation,
                installed: false,
                in_flight: Vec::new(),
            },
            Err(_) => Worker {
                sender: None,
                handle: None,
                generation,
                installed: false,
                in_flight: Vec::new(),
            },
        }
    }
    #[cfg(test)]
    pub(crate) fn state(&self) -> State {
        self.state
    }
    pub(crate) fn digest(&self) -> [u8; 32] {
        self.digest
    }
    /// The kind 35 manifest facts, once a worker has installed the revision.
    pub(crate) fn manifest(&self) -> Option<Manifest> {
        let ids = self.policy_ids.as_ref()?;
        Some(Manifest {
            digest: self.digest,
            scopes: self
                .texts
                .scopes
                .iter()
                .map(|scope| {
                    (
                        scope.scope,
                        self.texts.text_digest(scope.scope).unwrap_or([0; 32]),
                        scope.source,
                        ids.iter()
                            .find(|(s, _)| *s == scope.scope)
                            .map(|(_, ids)| ids.clone())
                            .unwrap_or_default(),
                    )
                })
                .collect(),
        })
    }
    /// Queue one decision. `None` is `policy_busy`: the agent's queue or
    /// the service queue is full, or the owner is closing.
    pub(crate) fn decide_job(&mut self, request: JobRequest) -> Option<Ticket> {
        if self.state == State::Closing || self.queued >= SERVICE_QUEUE {
            return None;
        }
        let queue = self.queues.entry(request.agent).or_default();
        if queue.len() >= AGENT_QUEUE {
            return None;
        }
        let ticket = self.next_ticket;
        self.next_ticket += 1;
        queue.push_back(Queued { ticket, request });
        self.queued += 1;
        if let Some(wake) = &self.wake {
            let _ = wake.notify();
        }
        Some(ticket)
    }
    /// The decision of `ticket`, once replied.
    pub(crate) fn take(&mut self, ticket: Ticket) -> Option<JobDecision> {
        self.ready.remove(&ticket)
    }
    /// The requester's cancel: a queued request is dropped; the reply of a
    /// request in flight is discarded.
    pub(crate) fn cancel(&mut self, ticket: Ticket) {
        self.ready.remove(&ticket);
        for queue in self.queues.values_mut() {
            let before = queue.len();
            queue.retain(|q| q.ticket != ticket);
            self.queued -= before - queue.len();
        }
        self.queues.retain(|_, q| !q.is_empty());
        if self
            .workers
            .iter()
            .any(|w| w.in_flight.iter().any(|f| f.ticket == ticket))
        {
            self.cancelled.push(ticket);
        }
    }
    /// Queued requests, for the waiting reason and tests.
    #[cfg(test)]
    pub(crate) fn queued(&self) -> usize {
        self.queued
    }
    /// One bounded reactor step: replies, worker loss, dispatch.
    pub(crate) fn poll(&mut self) {
        while let Ok(message) = self.replies.try_recv() {
            self.receive(message);
        }
        while let Some(found) = self.reader.try_recv() {
            self.found(found);
        }
        self.reap();
        self.reap_reader();
        self.dispatch();
    }
    /// One attribute reply (PL2 attribute reader).
    fn found(&mut self, found: attributes::Found) {
        match found {
            attributes::Found::Entry(identity) => {
                self.entry_pending = false;
                self.entry = Some(ap::ProgramRef {
                    path: ap::PathRef::ordinary(if identity.path.is_empty() {
                        ap::ENTRY
                    } else {
                        &identity.path
                    }),
                    identity_known: identity.identity_known,
                    platform: identity.platform,
                    team_id: identity.team_id,
                    signing_id: identity.signing_id,
                    cdhash: identity.cdhash,
                });
                self.maybe_active();
            }
            attributes::Found::Root(key, result) => {
                self.resolving.remove(&key);
                match result {
                    Ok(root) => {
                        if self.roots.insert(key.clone(), root).is_none() {
                            self.root_order.push_back(key);
                            while self.root_order.len() > ROOTS {
                                if let Some(old) = self.root_order.pop_front() {
                                    self.roots.remove(&old);
                                }
                            }
                        }
                    }
                    // A refusal is not cached: every queued request with
                    // this root is refused now; a later one looks again.
                    Err(refusal) => self.refuse_root(&key, refusal),
                }
            }
        }
    }
    /// Refuses every queued request whose root failed to canonicalise,
    /// with the path reason and no scope (PL2 canonical paths).
    fn refuse_root(&mut self, key: &RootKey, refusal: asura_platform::PathRefusal) {
        let scopes: Vec<ap::Scope> = self.texts.scopes.iter().map(|s| s.scope).collect();
        let mut refused = Vec::new();
        for queue in self.queues.values_mut() {
            let before = queue.len();
            queue.retain(|q| {
                if q.request.root_key() == *key {
                    refused.push((q.ticket, InFlight::of(q.ticket, &q.request, &key.location)));
                    false
                } else {
                    true
                }
            });
            self.queued -= before - queue.len();
        }
        self.queues.retain(|_, q| !q.is_empty());
        for (ticket, flight) in refused {
            let mut decision = JobDecision::refused(
                self.digest,
                &flight,
                ap::ErrorClass::RequestInvalid,
                refusal.reason(),
            );
            decision.scopes = scopes
                .iter()
                .map(|&scope| ap::ScopeResult {
                    scope,
                    allow: false,
                    rules: Vec::new(),
                    error: Some(ap::ErrorClass::RequestInvalid),
                })
                .collect();
            if let Some(primary) = decision.primary.as_mut() {
                primary.scope = None;
            }
            self.ready.insert(ticket, decision);
        }
    }
    /// A lost reader is joined and replaced; its lookups are sent again.
    fn reap_reader(&mut self) {
        if self.state == State::Closing || !self.reader.lost() {
            return;
        }
        tracing::warn!(stage = "policy", "policy_attribute_reader_failed");
        self.reader = attributes::Reader::start(self.wake.clone());
        if self.entry_pending || self.entry.is_none() {
            self.entry_pending = self.reader.send(attributes::Lookup::Entry);
        }
        let keys: Vec<RootKey> = self.resolving.drain().collect();
        for key in keys {
            if self.reader.send(attributes::Lookup::Root(key.clone())) {
                self.resolving.insert(key);
            }
        }
    }
    /// Starting or restarting becomes active once every live worker has
    /// installed the revision and the entry identity is known.
    fn maybe_active(&mut self) {
        if matches!(self.state, State::Starting | State::Restarting)
            && self.entry.is_some()
            && self
                .workers
                .iter()
                .all(|w| w.installed || w.handle.is_none())
        {
            self.state = State::Active;
        }
    }
    fn receive(&mut self, message: FromWorker) {
        match message {
            FromWorker::Installed {
                worker,
                generation,
                result,
            } => {
                let Some(w) = self
                    .workers
                    .get_mut(worker)
                    .filter(|w| w.generation == generation)
                else {
                    return;
                };
                match result {
                    Ok(ids) => {
                        w.installed = true;
                        self.policy_ids.get_or_insert(ids);
                        self.maybe_active();
                    }
                    Err(error) => {
                        tracing::warn!(stage = "policy", error, "policy_worker_load_failed");
                        if self.state != State::Closing {
                            self.state = State::Unavailable;
                        }
                    }
                }
            }
            FromWorker::Decided {
                worker,
                generation,
                ticket,
                outcome,
            } => {
                let Some(w) = self
                    .workers
                    .get_mut(worker)
                    .filter(|w| w.generation == generation)
                else {
                    return;
                };
                let Some(at) = w.in_flight.iter().position(|f| f.ticket == ticket) else {
                    return;
                };
                let flight = w.in_flight.remove(at);
                let outcome = match *outcome {
                    Outcome::Model(outcome) => outcome,
                    Outcome::Person(outcome) => {
                        if let Some(at) = self.cancelled.iter().position(|t| *t == ticket) {
                            self.cancelled.remove(at);
                            return;
                        }
                        let run = outcome.decision.allow;
                        let decision = JobDecision {
                            run,
                            action: outcome.action,
                            profile: flight.profile,
                            unavailable: false,
                            revision_digest: self.digest,
                            scopes: outcome.decision.scopes,
                            primary: if run { None } else { outcome.decision.primary },
                            constraints: None,
                            resource_digest: outcome.resource_digest,
                            request_digest: outcome.request_digest,
                            intent_digest: outcome.intent_digest,
                            now_ms: flight.now_ms,
                            project: flight.project,
                            task: flight.task,
                        };
                        self.ready.insert(ticket, decision);
                        return;
                    }
                };
                if let Some(regions) = outcome.regions {
                    self.remember(flight.key.clone(), regions);
                }
                if let Some(at) = self.cancelled.iter().position(|t| *t == ticket) {
                    self.cancelled.remove(at);
                    return;
                }
                let constraints = outcome
                    .regions
                    .and_then(|r| Constraints::model1(r.read.0, r.exec.0, r.write.0).ok());
                let run = outcome.decision.allow && constraints.is_some();
                let decision = JobDecision {
                    run,
                    action: ap::Action::ProcessExec,
                    profile: ap::ProfileKind::Model1,
                    unavailable: false,
                    revision_digest: self.digest,
                    scopes: outcome.decision.scopes.clone(),
                    primary: if run {
                        None
                    } else {
                        outcome.decision.primary.clone().or(Some(ap::Primary {
                            scope: None,
                            reason: "cannot_enforce".into(),
                            rule: None,
                            explain: None,
                        }))
                    },
                    constraints: constraints.filter(|_| run),
                    resource_digest: outcome.resource_digest,
                    request_digest: outcome.request_digest,
                    intent_digest: outcome.intent_digest,
                    now_ms: flight.now_ms,
                    project: flight.project,
                    task: flight.task,
                };
                self.ready.insert(ticket, decision);
            }
        }
    }
    fn remember(&mut self, key: CacheKey, regions: ap::Regions) {
        if self.cache.insert(key.clone(), regions).is_none() {
            self.cache_order.push_back(key);
            while self.cache_order.len() > CACHE {
                if let Some(old) = self.cache_order.pop_front() {
                    self.cache.remove(&old);
                }
            }
        }
    }
    /// Worker loss: a finished thread fails its in-flight requests with
    /// `policy_error`; it is joined, then replaced.
    fn reap(&mut self) {
        for index in 0..self.workers.len() {
            let finished = self.workers[index]
                .handle
                .as_ref()
                .is_some_and(JoinHandle::is_finished);
            if !finished {
                continue;
            }
            let handle = self.workers[index].handle.take().expect("finished handle");
            let _ = handle.join();
            let failed = std::mem::take(&mut self.workers[index].in_flight);
            for flight in failed {
                if let Some(at) = self.cancelled.iter().position(|t| *t == flight.ticket) {
                    self.cancelled.remove(at);
                    continue;
                }
                self.ready.insert(
                    flight.ticket,
                    JobDecision::refused(
                        self.digest,
                        &flight,
                        ap::ErrorClass::PolicyError,
                        "policy_error",
                    ),
                );
            }
            if self.state == State::Closing {
                continue;
            }
            tracing::warn!(stage = "policy", worker = index, "policy_worker_failed");
            if self.state == State::Active {
                self.state = State::Restarting;
            }
            let replacement = self.spawn(index);
            self.workers[index] = replacement;
        }
    }
    fn dispatch(&mut self) {
        if self.state == State::Unavailable {
            // No engine can decide: every queued request is refused with
            // `scope_unavailable`, and no kind 34 is written for it.
            for (_, queue) in std::mem::take(&mut self.queues) {
                for q in queue {
                    self.ready.insert(
                        q.ticket,
                        JobDecision::refused(
                            self.digest,
                            &InFlight::of(q.ticket, &q.request, &q.request.location),
                            ap::ErrorClass::ScopeUnavailable,
                            "scope_unavailable",
                        ),
                    );
                }
            }
            self.queued = 0;
            return;
        }
        if !matches!(self.state, State::Active | State::Restarting) {
            return;
        }
        loop {
            let Some(index) = self
                .workers
                .iter()
                .position(|w| w.installed && w.sender.is_some() && w.in_flight.len() < IN_FLIGHT)
            else {
                return;
            };
            let Some(entry) = self.entry.clone() else {
                return;
            };
            let Some((agent, root)) = self.next_ready_agent() else {
                return;
            };
            let queue = self.queues.get_mut(&agent).expect("agent queue");
            let q = queue.pop_front().expect("queued request");
            if queue.is_empty() {
                self.queues.remove(&agent);
            }
            self.queued -= 1;
            self.last_agent = Some(agent);
            #[cfg(feature = "test-support")]
            if std::mem::take(&mut self.panic_next) {
                let _ = self.workers[index]
                    .sender
                    .as_ref()
                    .expect("sender")
                    .try_send(ToWorker::Panic);
            }
            let flight = InFlight::of(q.ticket, &q.request, &root);
            let message = ToWorker::Decide {
                ticket: q.ticket,
                principal: Box::new(principal(&q.request, &root, &self.asura_home)),
                entry: Box::new(entry),
                command: q.request.command.clone(),
                plan: q.request.plan,
                person: q.request.person.as_ref().map(|p| {
                    Box::new(ap::PersonEntry {
                        env_terminal: p.env_terminal.clone(),
                        env_declared: p.env_declared.clone(),
                        outside_switch: p.outside_switch,
                        secrets: p.secrets.clone(),
                    })
                }),
                now_ms: q.request.now_ms,
                cached: self.cache.get(&flight.key).copied(),
            };
            let worker = &mut self.workers[index];
            match worker.sender.as_ref().expect("sender").try_send(message) {
                Ok(()) => worker.in_flight.push(flight),
                Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => {
                    // The worker cannot take it now (a test fault filled its
                    // channel, or it ended): requeue at the front; the next
                    // step reaps or retries it.
                    self.queues.entry(agent).or_default().push_front(q);
                    self.queued += 1;
                    return;
                }
            }
            // PL2: refresh the entry identity, at most one in flight; the
            // reader wakes the reactor only for a changed result.
            if !self.entry_pending {
                self.entry_pending = self.reader.send(attributes::Lookup::Entry);
            }
        }
    }
    /// The next agent after the last served, in key order (round robin),
    /// whose head request's project root is resolved. A head with an
    /// unresolved root starts its lookup and the agent is skipped.
    fn next_ready_agent(&mut self) -> Option<(AgentKey, String)> {
        let mut order: Vec<AgentKey> = match self.last_agent {
            Some(last) => self
                .queues
                .range((std::ops::Bound::Excluded(last), std::ops::Bound::Unbounded))
                .map(|(agent, _)| *agent)
                .collect(),
            None => Vec::new(),
        };
        order.extend(
            self.queues
                .keys()
                .copied()
                .filter(|a| self.last_agent.is_none_or(|last| *a <= last)),
        );
        for agent in order {
            let Some(key) = self
                .queues
                .get(&agent)
                .and_then(|q| q.front())
                .map(|q| q.request.root_key())
            else {
                continue;
            };
            if let Some(root) = self.roots.get(&key) {
                return Some((agent, root.clone()));
            }
            if !self.resolving.contains(&key)
                && self.reader.send(attributes::Lookup::Root(key.clone()))
            {
                self.resolving.insert(key);
            }
        }
        None
    }
    /// The `/status` facts; `revision` is the journaled kind 35 sequence
    /// of this revision, or 0.
    pub(crate) fn status(&self, revision: u64) -> asura_control::pb::PolicyStatus {
        let scope = if self.state == State::Unavailable {
            2
        } else {
            1
        };
        asura_control::pb::PolicyStatus {
            state: Some(match self.state {
                State::Starting => 1,
                State::Active | State::Restarting | State::Closing => 2,
                State::Unavailable => 3,
            }),
            revision: Some(revision),
            installation: Some(scope),
            user: Some(scope),
            workspace_sets: Some(0),
            repository_sets: Some(0),
            repository_changed: Some(0),
            solver: Some(1),
            approvals_waiting: Some(0),
        }
    }
    /// Shutdown: close ingress, drop the queues, close the worker channels.
    pub(crate) fn shutdown(&mut self) {
        self.state = State::Closing;
        self.queues.clear();
        self.queued = 0;
        for worker in &mut self.workers {
            worker.sender = None;
        }
        self.reader.close();
    }
    /// Every worker has finished and been joined.
    pub(crate) fn settled(&mut self) -> bool {
        while let Ok(message) = self.replies.try_recv() {
            self.receive(message);
        }
        for worker in &mut self.workers {
            if worker.handle.as_ref().is_some_and(JoinHandle::is_finished) {
                let _ = worker.handle.take().expect("handle").join();
            }
        }
        let reader = self.reader.settled();
        reader && self.workers.iter().all(|w| w.handle.is_none())
    }
    /// Test fault: the next dispatched request's worker panics.
    #[cfg(all(test, feature = "test-support"))]
    pub(crate) fn panic_next_for_test(&mut self) {
        self.panic_next = true;
    }
}

/// The principal of a job: the model `exec_cmd` shape, or a person job's
/// (PL2), with the canonical project root.
fn principal(request: &JobRequest, root: &str, asura_home: &str) -> ap::JobPrincipal {
    let task = format!(
        "{}:{}",
        hex(&request.agent.conversation),
        request.agent.agent
    );
    let (job, project) = (hex(&request.job), hex(&request.project));
    match &request.person {
        None => ap::model1_principal(&job, &project, root, &task, asura_home),
        Some(p) => ap::person_principal(
            &job,
            &project,
            root,
            &task,
            asura_home,
            p.outside,
            p.env_declared.clone(),
        ),
    }
}
/// The intent of a stored command with its features (PL2 intent
/// extractor). Pure and bounded; built on the worker.
fn intent(command: &str, plan: PlanFacts, person: Option<bool>) -> ap::Intent {
    use asura_platform::exec_cmd::split::{self, Parse};
    let f = split::intent(command, plan);
    let digest: [u8; 32] = sha2::Sha256::digest(command.as_bytes()).into();
    let (source, name) = match person {
        None => (ap::IntentSource::ModelTool, "exec_cmd"),
        Some(false) => (ap::IntentSource::PersonCommand, "!"),
        Some(true) => (ap::IntentSource::PersonCommand, "!!"),
    };
    ap::Intent {
        source,
        name: name.into(),
        digest: hex(&digest),
        command: Some(ap::Command {
            parse: match f.parse {
                Parse::Simple => ap::Parse::Simple,
                Parse::Compound => ap::Parse::Compound,
                Parse::Unparsed => ap::Parse::Unparsed,
            },
            tools: f.tools,
            first_tool: f.first_tool,
            subcommand: f.subcommand,
            flags: f.flags,
            shell_features: f.shell_features.iter().map(|s| (*s).to_owned()).collect(),
            bytes: f.bytes,
            secret_vars: f.secret_vars,
            secret_reference: f.secret_reference,
            whole_command: f.whole_command,
        }),
    }
}
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn worker_main(
    index: usize,
    generation: u64,
    texts: &ap::RevisionTexts,
    receiver: Receiver<ToWorker>,
    replies: mpsc::Sender<FromWorker>,
    wake: Option<WakeSender>,
) {
    let notify = || {
        if let Some(wake) = &wake {
            let _ = wake.notify();
        }
    };
    let engine = ap::Engine::build(texts);
    let result = match &engine {
        Ok(engine) => Ok(texts
            .scopes
            .iter()
            .map(|s| (s.scope, engine.policy_ids(s.scope)))
            .collect()),
        Err(error) => Err(format!("{error:?}")),
    };
    if replies
        .send(FromWorker::Installed {
            worker: index,
            generation,
            result,
        })
        .is_err()
    {
        return;
    }
    notify();
    let Ok(engine) = engine else {
        return;
    };
    // The loop ends when the owner drops the sender (shutdown or
    // replacement); every message is bounded work.
    while let Ok(message) = receiver.recv() {
        match message {
            ToWorker::Decide {
                ticket,
                principal,
                entry,
                command,
                plan,
                person,
                now_ms,
                cached,
            } => {
                let outside = principal.profile == ap::ProfileKind::PersonOutside1;
                let outcome = match person {
                    None => {
                        let intent = intent(&command, plan, None);
                        Outcome::Model(ap::decide_model1_job(
                            &engine,
                            &principal,
                            &entry,
                            Some(&intent),
                            now_ms,
                            cached,
                        ))
                    }
                    Some(facts) => {
                        let intent = intent(&command, plan, Some(outside));
                        Outcome::Person(ap::decide_person_job(
                            &engine,
                            &principal,
                            &entry,
                            &facts,
                            Some(&intent),
                            now_ms,
                        ))
                    }
                };
                if replies
                    .send(FromWorker::Decided {
                        worker: index,
                        generation,
                        ticket,
                        outcome: Box::new(outcome),
                    })
                    .is_err()
                {
                    return;
                }
                notify();
            }
            #[cfg(feature = "test-support")]
            ToWorker::Panic => {
                notify();
                panic!("policy worker fault for test");
            }
        }
    }
}

/// A real project directory for owner tests (the attribute reader walks
/// it): its canonical location, device and inode. Under the system temp
/// directory, never an Asura home.
#[cfg(test)]
pub(crate) fn test_project(name: &str) -> (String, u64, u64) {
    use std::os::unix::fs::MetadataExt;
    let root = std::env::temp_dir()
        .canonicalize()
        .expect("temp dir")
        .join(format!("asura-policy-owner-{}", std::process::id()))
        .join(name);
    std::fs::create_dir_all(&root).expect("project dir");
    let meta = std::fs::metadata(&root).expect("project metadata");
    (
        root.to_string_lossy().into_owned(),
        meta.dev() as u64,
        meta.ino(),
    )
}
/// A model request for `test_project(name)`.
#[cfg(test)]
pub(crate) fn test_request(name: &str, conversation: u8, n: u8, command: &str) -> JobRequest {
    let (location, device, inode) = test_project(name);
    JobRequest {
        agent: AgentKey {
            conversation: [conversation; 16],
            agent: 0,
        },
        job: [n; 16],
        project: [9; 16],
        location,
        device,
        inode,
        command: command.into(),
        plan: PlanFacts::default(),
        person: None,
        now_ms: 1_762_000_000_000,
    }
}

// The cases use the test-only set and the worker fault.
#[cfg(all(test, feature = "test-support"))]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn owner(texts: ap::RevisionTexts, workers: usize) -> PolicyOwner {
        let mut owner = PolicyOwner::with_texts(texts, "/Users/p/.asura".into(), None, workers);
        let end = Instant::now() + Duration::from_secs(30);
        while owner.state() != State::Active {
            assert!(Instant::now() < end, "owner did not become active");
            owner.poll();
            std::thread::sleep(Duration::from_millis(1));
        }
        owner
    }
    fn request(conversation: u8, n: u8) -> JobRequest {
        test_request("project", conversation, n, "printf ok")
    }
    fn person(outside: bool, facts: PersonRequest, command: &str) -> JobRequest {
        let mut r = test_request("person", 7, 1, command);
        r.person = Some(PersonRequest { outside, ..facts });
        r
    }
    fn reason(decision: &JobDecision) -> (&str, Option<&str>) {
        decision
            .primary
            .as_ref()
            .map_or(("", None), |p| (p.reason.as_str(), p.rule.as_deref()))
    }

    /// PL-I15 (owner, real workers and attribute reader): person starts are
    /// decided with the environment, switch and named secret requests.
    #[test]
    fn pl_i15_person_decisions_on_the_owner() {
        let mut owner = owner(ap::RevisionTexts::builtin(), 2);
        let facts =
            |terminal: &[&str], declared: &[&str], switch, secrets: &[&str]| PersonRequest {
                outside: false,
                env_terminal: terminal.iter().map(|s| (*s).to_owned()).collect(),
                env_declared: declared.iter().map(|s| (*s).to_owned()).collect(),
                outside_switch: switch,
                secrets: secrets.iter().map(|s| (*s).to_owned()).collect(),
            };
        let mut decide = |request| {
            let ticket = owner.decide_job(request).unwrap();
            wait(&mut owner, ticket)
        };
        // A `!` with listed names and two named secrets runs; no constraint
        // set until PL6; the identified entry is in the resource digest.
        let ok = decide(person(
            false,
            facts(&["LANG"], &[], false, &["GITHUB_TOKEN", "NPM_TOKEN"]),
            "cargo build",
        ));
        assert!(ok.run && ok.constraints.is_none(), "{ok:?}");
        assert_eq!(ok.profile, ap::ProfileKind::Person1);
        assert_eq!(ok.action, ap::Action::ProcessExec);
        // An unlisted terminal name, an undeclared name.
        let d = decide(person(
            false,
            facts(&["LANG", "AWS_SECRET"], &[], false, &[]),
            "env",
        ));
        assert_eq!(
            reason(&d),
            ("env_not_allowed", Some("installation:person.env_terminal"))
        );
        // The owner gives the principal kind 31's declared names, so a
        // declared name passes; the engine case covers an undeclared one.
        let d = decide(person(false, facts(&[], &["RUST_LOG"], false, &[]), "env"));
        assert!(d.run, "{d:?}");
        // `!!` with the switch off and on.
        let d = decide(person(true, facts(&[], &[], false, &[]), "ls"));
        assert_eq!(d.profile, ap::ProfileKind::PersonOutside1);
        assert_eq!(
            reason(&d),
            (
                "outside_sandbox_off",
                Some("installation:person.outside_switch")
            )
        );
        assert!(decide(person(true, facts(&[], &[], true, &[]), "ls")).run);
        close(owner);
    }

    /// PL-I15 (owner): a named secret is denied for a model-shaped job by
    /// the hard forbid S4, through a person start whose user scope forbids
    /// one secret: the deciding request is that `secret_use`.
    #[test]
    fn pl_i15_secret_use_decides_the_start() {
        let mut texts = ap::RevisionTexts::builtin();
        texts.scopes[1].text.push_str("\n@id(\"no-npm\") @reason(\"secret_forbidden\")\nforbid (principal, action == Asura::Action::\"secret_use\", resource == Asura::Secret::\"NPM_TOKEN\");\n");
        let mut owner = owner(texts, 2);
        let facts = PersonRequest {
            secrets: vec!["GITHUB_TOKEN".into(), "NPM_TOKEN".into()],
            ..PersonRequest::default()
        };
        let ticket = owner
            .decide_job(person(false, facts, "npm publish"))
            .unwrap();
        let d = wait(&mut owner, ticket);
        assert!(!d.run);
        assert_eq!(d.action, ap::Action::SecretUse);
        assert_eq!(reason(&d), ("secret_forbidden", Some("user:no-npm")));
        close(owner);
    }

    /// PL-I15 (owner): a revision with a person network forbid fails the
    /// AD-7 load check; the owner is unavailable and refuses starts.
    #[test]
    fn pl_i15_person_network_forbid_is_not_accepted() {
        let mut texts = ap::RevisionTexts::builtin();
        texts.scopes[1].text.push_str("\n@id(\"no-net\") @reason(\"no_net\")\nforbid (principal, action == Asura::Action::\"network.connect\", resource) when { principal.origin == Asura::Origin::\"person_typed\" };\n");
        let mut owner = PolicyOwner::with_texts(texts, "/h/.asura".into(), None, 2);
        let end = Instant::now() + Duration::from_secs(30);
        while owner.state() != State::Unavailable {
            assert!(Instant::now() < end);
            owner.poll();
            std::thread::sleep(Duration::from_millis(1));
        }
        let ticket = owner
            .decide_job(person(false, PersonRequest::default(), "ls"))
            .unwrap();
        assert!(wait(&mut owner, ticket).unavailable);
        close(owner);
    }

    /// PL2 canonical paths in the owner: a root that does not exist, or
    /// whose identity differs, is refused with its path reason and no
    /// scope, in every scope result; the entry is identified.
    #[test]
    fn root_refusal_and_entry_identity() {
        let mut owner = owner(ap::RevisionTexts::builtin(), 2);
        assert!(
            owner
                .entry
                .as_ref()
                .is_some_and(|e| e.identity_known && e.platform)
        );
        let mut missing = request(1, 1);
        missing.location = "/private/tmp/asura-policy-no-such-root/x".into();
        let ticket = owner.decide_job(missing).unwrap();
        let d = wait(&mut owner, ticket);
        assert!(!d.run && !d.unavailable);
        assert_eq!(reason(&d).0, "path_unavailable");
        assert_eq!(d.primary.as_ref().unwrap().scope, None);
        assert!(
            d.scopes
                .iter()
                .all(|s| s.error == Some(ap::ErrorClass::RequestInvalid))
        );
        let mut moved = request(1, 2);
        moved.inode += 1;
        let ticket = owner.decide_job(moved).unwrap();
        assert_eq!(reason(&wait(&mut owner, ticket)).0, "path_outside_root");
        // The same root decides once resolved.
        let ticket = owner.decide_job(request(1, 3)).unwrap();
        assert!(wait(&mut owner, ticket).run);
        close(owner);
    }

    /// PL-U10 (owner): the worker extracts the command features, so a
    /// tighten-only forbid on them denies that command only.
    #[test]
    fn worker_extracts_intent_features() {
        let mut texts = ap::RevisionTexts::builtin();
        texts.scopes[1].text.push_str("\n@id(\"no-push\") @reason(\"push_denied\") @intent(\"true\")\nforbid (principal, action == Asura::Action::\"process.exec\", resource) when { context has intent && context.intent has command && context.intent.command.first_tool == \"git\" && context.intent.command.subcommand == \"push\" };\n");
        let mut owner = owner(texts, 2);
        let ticket = owner
            .decide_job(test_request(
                "project",
                1,
                1,
                "sudo true; g\"\"it push --force origin main",
            ))
            .unwrap();
        assert!(wait(&mut owner, ticket).run, "first_tool is sudo");
        let ticket = owner
            .decide_job(test_request(
                "project",
                1,
                2,
                "g\"\"it push --force origin main",
            ))
            .unwrap();
        assert_eq!(
            reason(&wait(&mut owner, ticket)),
            ("push_denied", Some("user:no-push"))
        );
        close(owner);
    }
    fn wait(owner: &mut PolicyOwner, ticket: Ticket) -> JobDecision {
        let end = Instant::now() + Duration::from_secs(30);
        loop {
            owner.poll();
            if let Some(decision) = owner.take(ticket) {
                return decision;
            }
            assert!(Instant::now() < end, "no decision");
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    fn close(mut owner: PolicyOwner) {
        owner.shutdown();
        let end = Instant::now() + Duration::from_secs(30);
        while !owner.settled() {
            assert!(Instant::now() < end, "workers did not join");
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// PL-I2 (service part): the built-in sets compile to the `model-1`
    /// template, and the guardian renders today's profile byte for byte
    /// from that decision, for paths that need quoting and escaping.
    #[test]
    fn pl_i2_builtin_decision_renders_the_35cc82b_profile() {
        let mut owner = owner(ap::RevisionTexts::builtin(), 2);
        let ticket = owner.decide_job(request(1, 1)).unwrap();
        let decision = wait(&mut owner, ticket);
        assert!(decision.run, "{decision:?}");
        let constraints = decision.constraints.unwrap();
        assert_eq!(constraints, asura_platform::exec_cmd::template_for_test());
        for (project, scratch, asura) in [
            (
                "/Users/person/src/project",
                "/Users/person/.asura/tmp/exec-cmd-00112233445566778899aabbccddeeff",
                "/Users/person/.asura",
            ),
            (
                "/private/tmp/a b/pro\"j.c+(x)\\y",
                "/private/tmp/h o/.asura/tmp/exec-cmd-0f",
                "/private/tmp/h o/.asura",
            ),
            (
                "/w/p.a+b(c)[d]{e}$f^g|h*i?j\\k\"l",
                "/h/.asura/tmp/exec-cmd-1",
                "/h/.asura",
            ),
        ] {
            assert_eq!(
                asura_platform::exec_cmd::render_model1(&constraints, project, scratch, asura)
                    .unwrap(),
                asura_platform::exec_cmd::legacy_model1_for_test(project, scratch, asura).unwrap()
            );
        }
        assert_eq!(decision.scopes.len(), 2);
        close(owner);
    }

    /// PL-I3 (owner part): the test-only denying set refuses the entry in
    /// the installation scope with its `@explain` text.
    #[test]
    fn pl_i3_test_set_denies_with_installation_reason() {
        let mut owner = owner(ap::RevisionTexts::builtin_test_deny(), 2);
        let ticket = owner.decide_job(request(1, 1)).unwrap();
        let decision = wait(&mut owner, ticket);
        assert!(!decision.run && !decision.unavailable && decision.constraints.is_none());
        let primary = decision.primary.unwrap();
        assert_eq!(primary.scope, Some(ap::Scope::Installation));
        assert_eq!(primary.reason, "test_denied");
        assert_eq!(
            primary.rule.as_deref(),
            Some("installation:test_deny_model_exec")
        );
        assert_eq!(
            primary.explain.as_deref(),
            Some("the test policy denies model commands")
        );
        close(owner);
    }

    /// PL-I10: 64 concurrent requesters on 64 agents are all decided; an
    /// agent's ninth queued request and the service's 513th are
    /// `policy_busy`; every reactor step stays far inside 100 ms while the
    /// workers are busy.
    #[test]
    fn pl_i10_overload_is_bounded_and_responsive() {
        let mut owner = owner(ap::RevisionTexts::builtin(), worker_count());
        let mut slowest = Duration::ZERO;
        let mut step = |owner: &mut PolicyOwner| {
            let started = Instant::now();
            owner.poll();
            slowest = slowest.max(started.elapsed());
        };
        let tickets: Vec<Ticket> = (0..64)
            .map(|n| owner.decide_job(request(n, n)).unwrap())
            .collect();
        let end = Instant::now() + Duration::from_secs(60);
        let mut decided = 0;
        while decided < tickets.len() {
            assert!(Instant::now() < end, "decisions did not finish");
            step(&mut owner);
            decided = tickets
                .iter()
                .filter(|t| owner.ready.contains_key(t))
                .count();
        }
        assert!(tickets.iter().all(|t| owner.take(*t).unwrap().run));
        // Per-agent bound: 8 queued, the ninth is busy.
        owner.state = State::Starting; // hold dispatch to observe the queue bound
        for n in 0..AGENT_QUEUE as u8 {
            assert!(owner.decide_job(request(200, n)).is_some());
        }
        assert!(owner.decide_job(request(200, 99)).is_none());
        // Service bound: 512 queued in total.
        let mut agent = 0u8;
        while owner.queued() < SERVICE_QUEUE {
            agent = agent.wrapping_add(1);
            for n in 0..AGENT_QUEUE as u8 {
                if owner.queued() < SERVICE_QUEUE {
                    let mut r = request(agent, n);
                    r.agent.agent = 1;
                    assert!(owner.decide_job(r).is_some());
                }
            }
        }
        assert!(owner.decide_job(request(201, 0)).is_none());
        owner.state = State::Active;
        let end = Instant::now() + Duration::from_secs(120);
        while owner.queued() > 0 || owner.workers.iter().any(|w| !w.in_flight.is_empty()) {
            assert!(Instant::now() < end, "queue did not drain");
            step(&mut owner);
        }
        assert!(
            slowest < Duration::from_millis(100),
            "slowest step {slowest:?}"
        );
        println!("PL-I10 slowest owner step {slowest:?}");
        close(owner);
    }

    /// PL-I12: a worker that panics is joined and replaced; its in-flight
    /// request is refused with `policy_error`; later requests run.
    #[test]
    fn pl_i12_worker_loss_refuses_in_flight_and_replaces_the_worker() {
        let mut owner = owner(ap::RevisionTexts::builtin(), 2);
        owner.panic_next_for_test();
        let ticket = owner.decide_job(request(1, 1)).unwrap();
        let decision = wait(&mut owner, ticket);
        assert!(!decision.run);
        assert_eq!(decision.primary.unwrap().reason, "policy_error");
        assert_eq!(decision.scopes[0].error, Some(ap::ErrorClass::PolicyError));
        let end = Instant::now() + Duration::from_secs(30);
        while owner.state() != State::Active {
            assert!(Instant::now() < end, "replacement did not install");
            owner.poll();
            std::thread::sleep(Duration::from_millis(1));
        }
        let ticket = owner.decide_job(request(1, 2)).unwrap();
        assert!(wait(&mut owner, ticket).run);
        close(owner);
    }

    /// PL-B2 bench (run in release with `--ignored`): from `decide_job` on
    /// a reactor-like loop to the reply taken in the loop after the
    /// worker's wake, 64 closed-loop requesters on the product worker
    /// pool, 20,000 decisions after 2,000 warm-up decisions. The p99 must
    /// be at most 2.5 ms (security-policy.md, PL-B2, revised by AD-34; was
    /// 1.5 ms under AD-33).
    #[test]
    #[ignore = "bench: cargo test --release ... pl_b2 -- --ignored --nocapture"]
    fn pl_b2_end_to_end_p99_at_64_concurrent_requesters() {
        use std::os::fd::AsRawFd;
        let (reader, sender) = asura_platform::events::wake_pair().unwrap();
        let workers = worker_count();
        let mut owner = PolicyOwner::with_texts(
            ap::RevisionTexts::builtin(),
            "/Users/p/.asura".into(),
            Some(sender),
            workers,
        );
        let started = Instant::now();
        let wait = |reader: &asura_platform::events::WakeReader| {
            let mut fd = libc::pollfd {
                fd: reader.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: one valid pollfd; a 100 ms pacing wait that ends nothing.
            unsafe { libc::poll(&mut fd, 1, 100) };
            let _ = reader.drain();
        };
        while owner.state() != State::Active {
            wait(&reader);
            owner.poll();
        }
        let startup = started.elapsed();
        const WARM: usize = 2_000;
        const MEASURED: usize = 20_000;
        let mut outstanding: Vec<(Ticket, Instant)> = Vec::new();
        let mut next = 0u32;
        let projects: Vec<(String, u64, u64)> = (0..40)
            .map(|i| test_project(&format!("bench-{i}")))
            .collect();
        let submit = |owner: &mut PolicyOwner, out: &mut Vec<(Ticket, Instant)>, n: u32| {
            let mut r = request((n % 64) as u8, (n % 251) as u8);
            (r.location, r.device, r.inode) = projects[(n % 40) as usize].clone();
            r.command = "git status --short | head -n 20".into();
            let at = Instant::now();
            let ticket = owner.decide_job(r).expect("not busy");
            out.push((ticket, at));
        };
        for _ in 0..64 {
            submit(&mut owner, &mut outstanding, next);
            next += 1;
        }
        let mut samples = Vec::with_capacity(MEASURED);
        let mut done = 0usize;
        while done < WARM + MEASURED {
            owner.poll();
            let mut i = 0;
            let mut finished = 0;
            while i < outstanding.len() {
                let (ticket, at) = outstanding[i];
                if let Some(decision) = owner.take(ticket) {
                    assert!(decision.run);
                    if done >= WARM {
                        samples.push(at.elapsed());
                    }
                    done += 1;
                    outstanding.swap_remove(i);
                    finished += 1;
                } else {
                    i += 1;
                }
            }
            for _ in 0..finished {
                submit(&mut owner, &mut outstanding, next);
                next += 1;
            }
            if finished == 0 {
                wait(&reader);
            }
        }
        samples.sort();
        let pct = |p: f64| samples[((samples.len() as f64 * p) as usize).min(samples.len() - 1)];
        let budget = Duration::from_micros(2_500);
        println!(
            "PL-B2 workers={workers} requesters=64 decisions={} p50={:?} p99={:?} max={:?} owner_startup={:?}; budget p99 <= 2.5 ms (AD-34): {}",
            samples.len(),
            pct(0.50),
            pct(0.99),
            samples[samples.len() - 1],
            startup,
            if pct(0.99) <= budget { "PASS" } else { "FAIL" }
        );
        close(owner);
    }
    /// Cancel drops a queued request and discards an in-flight reply.
    #[test]
    fn cancel_drops_queued_and_discards_in_flight() {
        let mut owner = owner(ap::RevisionTexts::builtin(), 2);
        let ticket = owner.decide_job(request(1, 1)).unwrap();
        owner.cancel(ticket);
        assert_eq!(owner.queued(), 0);
        let ticket = owner.decide_job(request(1, 2)).unwrap();
        owner.poll(); // dispatched
        owner.cancel(ticket);
        let end = Instant::now() + Duration::from_secs(10);
        while owner.workers.iter().any(|w| !w.in_flight.is_empty()) {
            assert!(Instant::now() < end);
            owner.poll();
        }
        assert!(owner.take(ticket).is_none());
        close(owner);
    }

    /// Unavailable: a set that fails to load refuses every request with
    /// `scope_unavailable`, and the status says so.
    #[test]
    fn unloadable_set_is_unavailable() {
        let mut texts = ap::RevisionTexts::builtin();
        texts.scopes[0].text = "permit (principal, action, resource) when { 1 };".into();
        let mut owner = PolicyOwner::with_texts(texts, "/h/.asura".into(), None, 2);
        let end = Instant::now() + Duration::from_secs(30);
        while owner.state() != State::Unavailable {
            assert!(Instant::now() < end);
            owner.poll();
        }
        let ticket = owner.decide_job(request(1, 1)).unwrap();
        let decision = wait(&mut owner, ticket);
        assert!(decision.unavailable && !decision.run);
        assert_eq!(owner.status(0).state, Some(3));
        assert_eq!(owner.status(0).installation, Some(2));
        close(owner);
    }
}
