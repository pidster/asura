//! The job controller (tool-job-controller.md, J2 implementation details):
//! per-agent job queues, slots and start conditions, the job lifecycle
//! (TJ-S1), cancel and revocation, the record outbox for kinds 26 to 28,
//! start-time recovery, and the job audit events. J3 and J4 add job
//! views (kind 29 state blocks and live listings), parked waits (the yield
//! window, the model's `job wait`, client observes and listings), the job
//! revision, live counts, the outlived-turn derivation (EX12), revocation
//! with an origin filter and the EX11 check at slot grant
//! (tool-job-controller.md, J3 and J4 implementation details).
//!
//! The conversation owner holds the controller on the reactor beside its
//! `Payloads`. Every method does bounded in-memory work: no file, journal or
//! process wait (AE-01). The controller never submits to the journal
//! writer; the owner takes records from [`JobController::next_record`] when
//! its serialized ticket is free and reports each reply with
//! [`JobController::record_done`]. No timer ends a job or a queued start
//! (J2a-1, AQ1); the only pacing is the `disk_space` recheck.

mod port;
mod reasons;
mod views;
mod waits;

#[cfg(test)]
mod fixture;
#[cfg(test)]
mod tests;

pub(crate) use asura_platform::exec_cmd::Profile;
pub(crate) use asura_storage::authority::conversation::{JobOutcome as Outcome, JobReason};
pub(crate) use port::{
    Admitted, AuditSink, CaptureOutcome, PayloadPort, Short, SlotEvent, SlotFactory, SlotPort,
    SlotStart, SlotStartError,
};
pub(crate) use reasons::{BlockReason, BusyReason, Counts, MAX_UNSETTLED, block_text};
pub(crate) use views::JobView;
#[cfg(test)]
pub(crate) use waits::{CLIENT_WAITS, TurnWaitEnd};
pub(crate) use waits::{WaitDone, WaitEnding, WaitKey, WaitStart};

use crate::{capture_worker, tools};
use asura_platform::{
    PollInterest, RuntimeDirectory,
    exec_cmd::{Exit, StopKind, UnconfirmedReason},
    pacing::Fibonacci,
    secrets::{JobRedaction, SecretCommand, SecretVariable},
};
use asura_storage::audit::{self as record, Event};
use asura_storage::authority::{
    conversation::{
        self as journal, CaptureReason, CaptureState, Id, JobCancelRequested, JobCaptureState,
        JobCleanup, JobIntent, JobOrigin, JobRequester, JobSettled, JobStream, UnsettledJob,
    },
    writer,
};
use asura_storage::config::JobLimits;
use asura_storage::payload::Victim;
use std::collections::{BTreeMap, VecDeque};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use waits::Parked;

/// Controller events per drain (J2 numeric limits).
pub(crate) const EVENTS_PER_DRAIN: usize = 64;
/// Slot events per slot per poll (J2 numeric limits).
pub(crate) const SLOT_EVENTS: usize = 8;
/// Pending-install list bound (condition 7).
pub(crate) const PENDING_INSTALLS: usize = crate::payload_owner::PENDING_INSTALLS;
/// Settled jobs whose identity the controller remembers after it releases
/// them, so a duplicate start still returns the existing job.
const RETIRED: usize = MAX_UNSETTLED;

/// An agent: (conversation ID, agent number). Agent 0 is the main agent.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub(crate) struct AgentKey {
    pub conversation: Id,
    pub agent: u32,
}

/// Who started a job and the reference that names it (kind 26 fields 1
/// and 3).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Origin {
    Model {
        operation: Id,
        generation: u64,
        ordinal: u32,
    },
    Person {
        input: Id,
    },
}
impl Origin {
    pub(crate) fn journal(&self) -> JobOrigin {
        match *self {
            Self::Model {
                operation,
                generation,
                ordinal,
            } => JobOrigin::Model {
                operation,
                generation,
                ordinal,
            },
            Self::Person { input } => JobOrigin::Person { input },
        }
    }
    fn from_journal(origin: JobOrigin) -> Self {
        match origin {
            JobOrigin::Model {
                operation,
                generation,
                ordinal,
            } => Self::Model {
                operation,
                generation,
                ordinal,
            },
            JobOrigin::Person { input } => Self::Person { input },
        }
    }
    /// The one job ID of this origin ([job identity](J2-E6)).
    pub(crate) fn job_id(&self) -> Id {
        journal::job_id(&self.journal())
    }
    fn is_person(&self) -> bool {
        matches!(self, Self::Person { .. })
    }
    fn audit(&self) -> record::JobOrigin {
        match self {
            Self::Model { .. } => record::JobOrigin::Model,
            Self::Person { .. } => record::JobOrigin::Person,
        }
    }
}

/// One start. The secrets and redaction move into the slot at its start;
/// `duplicate_digest` is a keyed digest of the arguments, held in memory
/// only, that tells a duplicate start from a conflicting one.
/// `stored_command` is the stored, redacted command of the job's source
/// record (a model call's kind 14 `command`, the display text for a
/// whole-command call): the live-only listing excerpt is built from it at
/// admission, never from `command`.
pub(crate) struct StartRequest {
    pub origin: Origin,
    pub agent: AgentKey,
    pub scope: tools::ExecutionScope,
    pub profile: Profile,
    pub command: String,
    pub cwd: String,
    pub secrets: Vec<SecretVariable>,
    pub secret_command: Option<SecretCommand>,
    pub redaction: JobRedaction,
    pub duplicate_digest: [u8; 32],
    pub stored_command: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Admission {
    Accepted { job: Id },
    Existing { job: Id },
    Conflict,
    Busy(BusyReason),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Requester {
    Model,
    Client,
    Person,
    /// Service triggers go through `revoke`, `close` and the journal
    /// outage; tests name it.
    #[cfg_attr(not(test), allow(dead_code))]
    Service,
}
impl Requester {
    fn journal(self) -> JobRequester {
        match self {
            Self::Model => JobRequester::Model,
            Self::Client => JobRequester::Client,
            Self::Person => JobRequester::Person,
            Self::Service => JobRequester::Service,
        }
    }
}

/// Which origins a revocation reaches (EX11 revokes model jobs only,
/// ES-R5).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum OriginFilter {
    All,
    Model,
    #[allow(dead_code)] // the person-command packets revoke person jobs
    Person,
}
impl OriginFilter {
    fn admits(self, origin: &Origin) -> bool {
        match self {
            Self::All => true,
            Self::Model => !origin.is_person(),
            Self::Person => origin.is_person(),
        }
    }
}

/// The job states of TJ-S1. `Withdrawn` names a start that left before any
/// record: a person waiter, a person job before its kind 26 is submitted or
/// while it is held, or, while the journal is unavailable, a start whose
/// kind 26 never committed. A model start is never withdrawn while the
/// journal is available (J34-K2).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum JobState {
    Queued,
    Starting,
    Running,
    CapturePaused,
    Stopping,
    Draining,
    CleanupUnconfirmed,
    Finalizing,
    Recording,
    Recorded,
    Interrupted,
    Withdrawn,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CancelReply {
    /// The requested state; the settled state follows from slot events.
    Accepted(JobState),
    AlreadySettled(Outcome),
    NotPermitted,
    NotFound,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum JobEvent {
    Queued(Id, BlockReason),
    Started(Id),
    State(Id, JobState),
    Settled(Id, Outcome),
    /// P3-K: a `!!` job's kind 2 reason 15; survivors still hold output
    /// (OD2). Never emitted in J2.
    #[allow(dead_code)] // P3-K emits it
    ShellExited(Id, Exit, u32),
}

/// What the controller knows of one job.
#[derive(Clone, Debug, Eq, PartialEq)]
#[cfg(any(test, feature = "test-support"))]
pub(crate) struct JobStatus {
    pub state: JobState,
    pub origin: Origin,
    pub agent: AgentKey,
    /// The blocking reason of a queued job, and its brief text.
    pub reason: Option<BlockReason>,
    pub text: Option<String>,
    /// The first accepted cancel trigger.
    pub cancel: Option<JobReason>,
    pub outcome: Option<Outcome>,
}

/// Where a job's kind 26 is.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Intent {
    /// A person waiter: no record until its slot is granted.
    None,
    /// In the outbox.
    Pending,
    /// Submitted; the reply has not arrived.
    InFlight,
    Committed,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Kind {
    Intent,
    Cancel,
    Settled,
}

/// The request facts that wait in the queue until the slot starts.
struct Body {
    scope: tools::ExecutionScope,
    profile: Profile,
    command: String,
    cwd: String,
    secrets: Vec<SecretVariable>,
    secret_command: Option<SecretCommand>,
    redaction: JobRedaction,
}
impl Body {
    fn from_start(start: SlotStart) -> Self {
        let SlotStart {
            profile,
            scope,
            command,
            cwd,
            secrets,
            secret_command,
            capture,
            ..
        } = start;
        Self {
            scope,
            profile,
            command,
            cwd,
            secrets,
            secret_command,
            redaction: capture.redaction,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Trigger {
    requester: JobRequester,
    reason: JobReason,
    kill: bool,
}

struct Process {
    exit: Exit,
    cleanup: JobCleanup,
    escapees: u32,
    elapsed_ms: u64,
}

struct Job {
    id: Id,
    origin: Origin,
    agent: AgentKey,
    project: Id,
    digest: [u8; 32],
    profile: Profile,
    state: JobState,
    intent: Intent,
    body: Option<Body>,
    /// A person job's start, held from its grant until kind 26 commits.
    held_start: Option<SlotStart>,
    reserved: bool,
    /// The expiry victims of the reservation, returned if no slot starts.
    victims: Vec<Victim>,
    /// A slot accepted the start: its capture worker owns the reservation.
    slot_started: bool,
    /// Counts toward the agent and service live jobs.
    holds_slot: bool,
    slot: Option<Box<dyn SlotPort>>,
    block: Option<BlockReason>,
    queued_audited: bool,
    /// The first accepted cancel trigger: kind 27's facts.
    cancel: Option<Trigger>,
    /// The settled reason: the first trigger, or `person_kill` after an
    /// escalation (J2d).
    reason: JobReason,
    term_sent: bool,
    kill_sent: bool,
    /// The first trigger came before the leader exited: `cancelled`.
    before_exit: bool,
    spawned: bool,
    exit: Option<Exit>,
    unconfirmed: Option<UnconfirmedReason>,
    process: Option<Process>,
    no_process: Option<JobReason>,
    capture: Option<CaptureOutcome>,
    /// The worker after capture settlement, until kind 28 commits.
    worker: Option<capture_worker::Handle>,
    settled: Option<Box<JobSettled>>,
    /// Kind 28's sequence once committed; `None` again after delivery.
    committed: Option<u64>,
    delivered: bool,
    scratch_retained: bool,
    /// The journal became unavailable: no settlement will be recorded.
    abandoned: bool,
    recovered: bool,
    /// The spawned notice: the elapsed time of a state block runs from it.
    spawned_at: Option<Instant>,
    /// Why a `CapturePaused` job paused (state block reason).
    pause: Option<journal::JobPauseReason>,
    /// The kind 26 frame sequence once committed: it orders listings
    /// (J3-E5) and the outlived count (EX12).
    sequence: Option<u64>,
    /// Kind 26 field 4, the intent wall time.
    intent_ms: u64,
    /// Stored bytes per stream (the readable watermark) at the last poll.
    stored: [u64; 2],
    /// The `JobEntry.command` excerpt, built once at admission with
    /// storage's `command_excerpt` (the one owner).
    excerpt: String,
}
impl Job {
    fn new(
        id: Id,
        origin: Origin,
        agent: AgentKey,
        project: Id,
        digest: [u8; 32],
        state: JobState,
    ) -> Self {
        Self {
            id,
            origin,
            agent,
            project,
            digest,
            profile: Profile::Model1,
            state,
            intent: Intent::None,
            body: None,
            held_start: None,
            reserved: false,
            victims: Vec::new(),
            slot_started: false,
            holds_slot: false,
            slot: None,
            block: None,
            queued_audited: false,
            cancel: None,
            reason: JobReason::None,
            term_sent: false,
            kill_sent: false,
            before_exit: false,
            spawned: false,
            exit: None,
            unconfirmed: None,
            process: None,
            no_process: None,
            capture: None,
            worker: None,
            settled: None,
            committed: None,
            delivered: false,
            scratch_retained: false,
            abandoned: false,
            recovered: false,
            spawned_at: None,
            pause: None,
            sequence: None,
            intent_ms: 0,
            stored: [0; 2],
            excerpt: String::new(),
        }
    }
}

struct Retired {
    job: Id,
    digest: [u8; 32],
    origin: Origin,
    agent: AgentKey,
    project: Id,
    sequence: Option<u64>,
    intent_ms: u64,
    outcome: Outcome,
    /// The committed kind 28, so a result renders the settled facts without
    /// a journal read; `None` for a restart settlement.
    settled: Option<Box<JobSettled>>,
    excerpt: String,
}

/// The shared [`Fibonacci`] pacing (PACE-1: 1, 1, 2, 3, 5, 8 s and so on,
/// at most 60 s apart; it ends nothing) and the time of the next step. The
/// controller's `disk_space` recheck uses it, and the owner's start-time
/// limits read retry (TJ-F5) reuses it.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Pacing {
    at: Instant,
    delays: Fibonacci,
}
impl Pacing {
    /// The first step: `now` plus the first delay.
    pub(crate) fn start(now: Instant) -> Self {
        let mut delays = Fibonacci::new();
        Self {
            at: now + delays.next_delay(),
            delays,
        }
    }
    /// The next step: `now` plus the next delay.
    pub(crate) fn advance(&mut self, now: Instant) {
        self.at = now + self.delays.next_delay();
    }
    /// When the paced step is due.
    pub(crate) fn at(&self) -> Instant {
        self.at
    }
}

pub(crate) struct JobController {
    limits: JobLimits,
    runtime: RuntimeDirectory,
    audit: Option<Box<dyn AuditSink>>,
    slots: SlotFactory,
    jobs: BTreeMap<Id, Job>,
    queues: BTreeMap<AgentKey, VecDeque<Id>>,
    last_served: Option<AgentKey>,
    /// Kinds 27 and 28, oldest first.
    closures: VecDeque<(Id, Kind)>,
    /// Kind 26, oldest first.
    intents: VecDeque<Id>,
    in_flight: Option<(Id, Kind)>,
    /// Reservations to return to the payload owner at the next poll.
    releases: Vec<(Id, Vec<Victim>)>,
    events: VecDeque<JobEvent>,
    retired: VecDeque<Retired>,
    /// Start-time recovery has not committed every interrupted settlement.
    recovering: bool,
    recover_called: bool,
    journal_down: bool,
    journal_held: bool,
    closed: bool,
    /// A wake event since the last head evaluation.
    dirty: bool,
    seen: Option<(bool, usize, u64)>,
    disk: Option<Pacing>,
    /// The job revision (J4 control handler): increments on every
    /// admission, start, state change and settlement, and on the owner's
    /// retention and execution setting changes. In memory; 1 at start.
    revision: u64,
    /// Parked waits: at most one per agent (yield window or `job wait`,
    /// AE-03) and at most `CLIENT_WAITS` client observes and listings.
    waits: BTreeMap<WaitKey, Parked>,
    /// Ended waits for the owner, oldest first.
    done: VecDeque<WaitDone>,
    /// The EX11 effective value of a project, checked at slot grant for a
    /// model-origin head. Absent: execution is on.
    execution: Option<Box<dyn Fn(Id) -> bool>>,
}

impl JobController {
    /// `limits` come from the service-start configuration; `slots` creates
    /// one slot per granted job; `runtime` is the service runtime that each
    /// slot's setup uses.
    pub(crate) fn new(
        limits: JobLimits,
        emitter: Option<crate::audit::Emitter>,
        runtime: RuntimeDirectory,
        slots: SlotFactory,
    ) -> Self {
        Self::with_audit(
            limits,
            emitter.map(|e| Box::new(e) as Box<dyn AuditSink>),
            runtime,
            slots,
        )
    }
    pub(crate) fn with_audit(
        limits: JobLimits,
        audit: Option<Box<dyn AuditSink>>,
        runtime: RuntimeDirectory,
        slots: SlotFactory,
    ) -> Self {
        Self {
            limits,
            runtime,
            audit,
            slots,
            jobs: BTreeMap::new(),
            queues: BTreeMap::new(),
            last_served: None,
            closures: VecDeque::new(),
            intents: VecDeque::new(),
            in_flight: None,
            releases: Vec::new(),
            events: VecDeque::new(),
            retired: VecDeque::new(),
            recovering: true,
            recover_called: false,
            journal_down: false,
            journal_held: false,
            closed: false,
            dirty: true,
            seen: None,
            disk: None,
            revision: 1,
            waits: BTreeMap::new(),
            done: VecDeque::new(),
            execution: None,
        }
    }

    // ----- Recovery -----

    /// Start-time recovery (J2-E5): one kind 28 `interrupted`, reason
    /// `restart`, for each unsettled job from replay, oldest first.
    /// Admission waits with `recovering` until every one commits.
    pub(crate) fn recover(&mut self, mut unsettled: Vec<UnsettledJob>) {
        if self.recover_called {
            return;
        }
        self.recover_called = true;
        unsettled.sort_by_key(|u| u.intent.sequence);
        let now = wall_ms();
        for u in unsettled {
            let agent = AgentKey {
                conversation: u.conversation,
                agent: u.agent,
            };
            let mut job = Job::new(
                u.job,
                Origin::from_journal(u.origin),
                agent,
                u.project,
                [0; 32],
                JobState::Interrupted,
            );
            job.intent = Intent::Committed;
            job.sequence = Some(u.intent.sequence);
            job.intent_ms = u.time;
            job.recovered = true;
            job.reason = JobReason::Restart;
            job.settled = Some(Box::new(JobSettled::interrupted(u.job, now)));
            self.closures.push_back((u.job, Kind::Settled));
            self.jobs.insert(u.job, job);
        }
        self.check_recovered();
    }
    fn check_recovered(&mut self) {
        if self.recovering && self.recover_called && !self.jobs.values().any(|j| j.recovered) {
            self.recovering = false;
            self.dirty = true;
        }
    }

    // ----- Admission -----

    pub(crate) fn submit(&mut self, request: StartRequest, _now: Instant) -> Admission {
        let id = request.origin.job_id();
        if let Some(job) = self.jobs.get(&id) {
            return duplicate(job.digest, request.duplicate_digest, id);
        }
        if let Some(r) = self.retired.iter().find(|r| r.job == id) {
            return duplicate(r.digest, request.duplicate_digest, id);
        }
        if self.closed {
            return Admission::Busy(BusyReason::Closed);
        }
        if self.unsettled() >= MAX_UNSETTLED {
            return Admission::Busy(BusyReason::JobsFull);
        }
        let queued = self.queues.get(&request.agent).map_or(0, VecDeque::len);
        if queued >= self.limits.queued_agent as usize {
            return Admission::Busy(BusyReason::QueueFull);
        }
        // Replay accepts agent 0 only until a subagent design assigns
        // numbers (JR3); a model start of another agent could never commit.
        if request.agent.agent != 0 || request.scope.project == [0; 16] {
            return Admission::Conflict;
        }
        let StartRequest {
            origin,
            agent,
            scope,
            profile,
            command,
            cwd,
            secrets,
            secret_command,
            redaction,
            duplicate_digest,
            stored_command,
        } = request;
        let mut job = Job::new(
            id,
            origin,
            agent,
            scope.project,
            duplicate_digest,
            JobState::Queued,
        );
        job.profile = profile;
        job.excerpt = journal::command_excerpt(&stored_command);
        job.body = Some(Body {
            scope,
            profile,
            command,
            cwd,
            secrets,
            secret_command,
            redaction,
        });
        if !origin.is_person() {
            // J2-E7: a model start is durable when it is queued; its kind 26
            // commits before it can start.
            job.intent = Intent::Pending;
            if !self.journal_down {
                self.intents.push_back(id);
            }
        }
        self.jobs.insert(id, job);
        self.queues.entry(agent).or_default().push_back(id);
        self.event(JobEvent::State(id, JobState::Queued));
        self.dirty = true;
        Admission::Accepted { job: id }
    }

    /// Jobs with a kind 26 (or about to get one) and no committed kind 28,
    /// plus person waiters: the 64-job bound.
    fn unsettled(&self) -> usize {
        self.jobs
            .values()
            .filter(|j| j.state != JobState::Recorded && !j.abandoned)
            .count()
    }

    // ----- Cancel and revocation -----

    pub(crate) fn cancel(
        &mut self,
        job: Id,
        by: Requester,
        reason: JobReason,
        kill: bool,
    ) -> CancelReply {
        let requester = by.journal();
        if reason.requester() != Some(requester) || kill != (reason == JobReason::PersonKill) {
            return CancelReply::NotPermitted;
        }
        let Some(entry) = self.jobs.get(&job) else {
            return match self.retired.iter().find(|r| r.job == job) {
                Some(r) => CancelReply::AlreadySettled(r.outcome),
                None => CancelReply::NotFound,
            };
        };
        if by == Requester::Model && entry.origin.is_person() {
            return CancelReply::NotPermitted;
        }
        match entry.state {
            JobState::Recording | JobState::Recorded | JobState::Interrupted => {
                let outcome = entry
                    .settled
                    .as_ref()
                    .map_or(Outcome::Interrupted, |s| s.outcome);
                CancelReply::AlreadySettled(outcome)
            }
            _ => CancelReply::Accepted(self.trigger(
                job,
                Trigger {
                    requester,
                    reason,
                    kill,
                },
            )),
        }
    }

    /// Revokes every job of `project` with a service reason
    /// (`identity_changed`, or `shutdown`): queued starts settle
    /// `not_started`, live jobs get `T`.
    pub(crate) fn revoke_project(&mut self, project: Id, reason: JobReason) {
        self.revoke(Some(project), OriginFilter::All, reason);
    }
    /// Revokes the jobs of `project` (every project when `None`) whose
    /// origin passes `origin`, with a service reason. EX11 calls it with
    /// `OriginFilter::Model` and `execution_off` for each project turned
    /// off (ES-05): queued model starts settle `not_started` with kinds 27
    /// and 28, starting and live jobs get `T` and one kind 27, person jobs
    /// and other projects are untouched (ES-R5).
    pub(crate) fn revoke(&mut self, project: Option<Id>, origin: OriginFilter, reason: JobReason) {
        self.revoke_where(
            |j| project.is_none_or(|p| j.project == p) && origin.admits(&j.origin),
            None,
            reason,
        );
    }
    fn revoke_where(
        &mut self,
        filter: impl Fn(&Job) -> bool,
        except: Option<Id>,
        reason: JobReason,
    ) {
        if reason.requester() != Some(JobRequester::Service) {
            return;
        }
        let ids: Vec<Id> = self
            .jobs
            .values()
            .filter(|j| Some(j.id) != except && filter(j))
            .map(|j| j.id)
            .collect();
        for id in ids {
            self.trigger(
                id,
                Trigger {
                    requester: JobRequester::Service,
                    reason,
                    kill: false,
                },
            );
        }
    }

    /// Applies one cancel trigger. Returns the requested state.
    fn trigger(&mut self, id: Id, trigger: Trigger) -> JobState {
        let Some(job) = self.jobs.get_mut(&id) else {
            return JobState::Withdrawn;
        };
        if matches!(
            job.state,
            JobState::Recording | JobState::Recorded | JobState::Interrupted | JobState::Withdrawn
        ) {
            return job.state;
        }
        let first = job.cancel.is_none();
        if first {
            job.cancel = Some(trigger);
            job.reason = trigger.reason;
            job.before_exit = job.exit.is_none();
        } else if trigger.reason == JobReason::PersonKill && job.reason == JobReason::PersonStop {
            job.reason = JobReason::PersonKill;
        }
        let stop = if trigger.kill {
            StopKind::Kill
        } else {
            StopKind::Term
        };
        match job.state {
            JobState::Queued => {
                // TJ-S1 withdrawal rule: only a person waiter, or a start
                // whose kind 26 never committed while the journal is
                // unavailable, leaves with no record. A model start keeps
                // its kind 26 queued (also when held at the quota); kinds
                // 27 and 28 follow it.
                let waiter = job.intent == Intent::None;
                let unrecordable = self.journal_down && job.intent != Intent::Committed;
                if waiter || unrecordable {
                    self.withdraw(id);
                    return JobState::Withdrawn;
                }
                self.dequeue(id);
                self.cancel_record(id, first);
                self.settle_without_process(id, None);
                return JobState::Recording;
            }
            JobState::Starting if job.slot.is_none() => {
                // A person job granted its slot, waiting for kind 26.
                if job.intent == Intent::Pending {
                    self.withdraw(id);
                    return JobState::Withdrawn;
                }
                self.cancel_record(id, first);
                self.settle_without_process(id, None);
                return JobState::Recording;
            }
            JobState::Starting | JobState::Running | JobState::CapturePaused => {
                send_stop(job, stop);
                if job.state != JobState::Starting {
                    job.state = JobState::Stopping;
                }
            }
            JobState::Stopping | JobState::CleanupUnconfirmed => {
                if trigger.kill {
                    send_stop(job, StopKind::Kill);
                }
            }
            JobState::Draining | JobState::Finalizing => {
                if let Some(slot) = job.slot.as_mut() {
                    slot.discard_capture();
                }
                if trigger.kill {
                    send_stop(job, StopKind::Kill);
                }
            }
            JobState::Recording
            | JobState::Recorded
            | JobState::Interrupted
            | JobState::Withdrawn => {}
        }
        let state = job.state;
        self.cancel_record(id, first);
        self.event(JobEvent::State(id, state));
        state
    }

    /// Queues kind 27 for the first accepted trigger while the journal is
    /// available.
    fn cancel_record(&mut self, id: Id, first: bool) {
        if first && !self.journal_down {
            self.closures.push_back((id, Kind::Cancel));
        }
    }

    /// Removes a start that has no committed record and will get none: a
    /// person waiter, a person job granted but before its kind 26 is
    /// submitted or while it is held, or, while the journal is unavailable,
    /// a start whose kind 26 never committed. A model start is never
    /// withdrawn while the journal is available (TJ-S1, J34-K2).
    fn withdraw(&mut self, id: Id) {
        self.dequeue(id);
        self.intents.retain(|j| *j != id);
        self.closures.retain(|(j, _)| *j != id);
        if let Some(job) = self.jobs.remove(&id)
            && job.reserved
        {
            self.releases.push((id, job.victims));
        }
        self.event(JobEvent::State(id, JobState::Withdrawn));
        self.dirty = true;
    }
    fn dequeue(&mut self, id: Id) {
        let Some(agent) = self.jobs.get(&id).map(|j| j.agent) else {
            return;
        };
        if let Some(queue) = self.queues.get_mut(&agent) {
            queue.retain(|j| *j != id);
            if queue.is_empty() {
                self.queues.remove(&agent);
            }
        }
        self.dirty = true;
    }

    // ----- Journal state and shutdown -----

    /// The journal writer is fenced, repair-required or exhausted: every
    /// live job gets `T`, nothing starts and the outbox is dropped. The next
    /// start writes `interrupted` for every unsettled job.
    pub(crate) fn journal_unavailable(&mut self) {
        if self.journal_down {
            return;
        }
        self.journal_down = true;
        self.closures.clear();
        self.intents.clear();
        self.in_flight = None;
        self.dirty = true;
        let ids: Vec<Id> = self.jobs.keys().copied().collect();
        for id in ids {
            let Some(job) = self.jobs.get_mut(&id) else {
                continue;
            };
            if job.state == JobState::Recorded {
                continue;
            }
            match job.state {
                JobState::Queued => {
                    // Model entries stay in memory; person waiters stay at
                    // their lane heads.
                    if job.intent != Intent::Committed {
                        job.intent = if job.origin.is_person() {
                            Intent::None
                        } else {
                            Intent::Pending
                        };
                    }
                }
                JobState::Starting if job.slot.is_none() && job.held_start.is_some() => {
                    // A granted person job before its kind 26: back to its
                    // lane head as a waiter.
                    self.requeue_person(id);
                }
                _ => {
                    job.abandoned = true;
                    send_stop(job, StopKind::Term);
                    if job.slot.is_none() {
                        // Interrupted or not_started records that will not
                        // be written: the next start settles the job.
                        if let Some(job) = self.jobs.remove(&id)
                            && job.reserved
                        {
                            self.releases.push((id, job.victims));
                        }
                    }
                }
            }
        }
    }

    /// The journal left its held state: a held kind 26 may be resubmitted.
    pub(crate) fn journal_hold_ended(&mut self) {
        if self.journal_held {
            self.journal_held = false;
            self.dirty = true;
        }
    }

    /// Shutdown (AE-06): new submissions get `closed`; queued model starts
    /// settle `not_started` with `shutdown`; person waiters are removed;
    /// live jobs get `T` with `shutdown`.
    pub(crate) fn close(&mut self) {
        if self.closed {
            return;
        }
        self.closed = true;
        self.revoke_where(|_| true, None, JobReason::Shutdown);
        self.close_client_waits();
    }

    /// The owner's EX11 effective value of a project, read when a slot is
    /// granted to a model-origin queue head. The owner keeps the one copy
    /// of the setting; the closure reads it.
    pub(crate) fn set_execution(&mut self, check: Box<dyn Fn(Id) -> bool>) {
        self.execution = Some(check);
        self.dirty = true;
    }
    fn execution_on(&self, project: Id) -> bool {
        self.execution.as_ref().is_none_or(|check| check(project))
    }

    // ----- Reactor -----

    pub(crate) fn interests(&self, out: &mut Vec<PollInterest>) {
        for slot in self.jobs.values().filter_map(|j| j.slot.as_ref()) {
            slot.interests(out);
        }
    }

    pub(crate) fn next_deadline(&self, now: Instant) -> Option<Instant> {
        self.jobs
            .values()
            .filter_map(|j| j.slot.as_ref())
            .filter_map(|s| s.next_deadline(now))
            .chain(self.disk.map(|d| d.at()))
            .chain(self.wait_deadline())
            .min()
    }

    pub(crate) fn poll(&mut self, now: Instant, payloads: &mut dyn PayloadPort) {
        for (id, victims) in std::mem::take(&mut self.releases) {
            payloads.release(id, &victims);
        }
        let mut events = Vec::with_capacity(SLOT_EVENTS);
        let ids: Vec<Id> = self
            .jobs
            .values()
            .filter(|j| j.slot.is_some())
            .map(|j| j.id)
            .collect();
        for id in ids {
            events.clear();
            if let Some(slot) = self.jobs.get_mut(&id).and_then(|j| j.slot.as_mut()) {
                slot.poll(now, &mut events);
            }
            for event in events.drain(..) {
                self.slot_event(id, event, now, payloads);
            }
        }
        self.refresh_stored(payloads);
        self.deliver(payloads);
        self.release_slots();
        let seen = (
            payloads.ready(),
            payloads.pending_installs(),
            payloads.space_epoch(),
        );
        let due = self.disk.is_some_and(|d| now >= d.at());
        if self.dirty || due || self.seen != Some(seen) {
            self.seen = Some(seen);
            self.dirty = false;
            self.evaluate(now, payloads, due);
        }
        self.check_waits(now);
    }

    /// Reads each live capture's stored bytes from the payload owner (the
    /// readable watermark), for state blocks and the `new output` ends.
    fn refresh_stored(&mut self, payloads: &dyn PayloadPort) {
        for job in self.jobs.values_mut() {
            if job.slot_started && job.committed.is_none() && !job.abandoned {
                let [out, err] = payloads.stored(job.id);
                job.stored = [job.stored[0].max(out), job.stored[1].max(err)];
            }
        }
    }

    // ----- Records -----

    /// The next record to append, in the owner's order after the active
    /// turn's records: kinds 28 and 27 oldest first, then kind 26 oldest
    /// first. A job's kinds 27 and 28 wait until its kind 26 commits. One
    /// record is in flight at a time, as the owner's ticket is serialized.
    pub(crate) fn next_record(&mut self) -> Option<(Id, journal::Record)> {
        if self.in_flight.is_some() || self.journal_down {
            return None;
        }
        let ready = self.closures.iter().position(|(id, _)| {
            self.jobs
                .get(id)
                .is_some_and(|j| j.intent == Intent::Committed)
        });
        if let Some(index) = ready {
            let (id, kind) = self.closures.remove(index)?;
            let job = self.jobs.get(&id)?;
            let record = match kind {
                Kind::Cancel => {
                    let t = job.cancel?;
                    journal::Record::JobCancelRequested(JobCancelRequested {
                        job: id,
                        requester: t.requester,
                        reason: t.reason,
                        kill: t.kill,
                    })
                }
                Kind::Settled => journal::Record::JobSettled(job.settled.clone()?),
                Kind::Intent => return None,
            };
            self.in_flight = Some((id, kind));
            return Some((id, record));
        }
        if self.journal_held {
            return None;
        }
        let id = self.intents.pop_front()?;
        let job = self.jobs.get_mut(&id)?;
        job.intent = Intent::InFlight;
        job.intent_ms = wall_ms();
        self.in_flight = Some((id, Kind::Intent));
        Some((
            id,
            journal::Record::JobIntent(JobIntent {
                origin: job.origin.journal(),
                agent: job.agent.agent,
                time: job.intent_ms,
            }),
        ))
    }

    /// The owner's reply for the record of `job` that `next_record` gave.
    /// `Ok` carries the committed sequence.
    pub(crate) fn record_done(&mut self, job: Id, result: writer::Result<u64>) {
        let Some((id, kind)) = self.in_flight else {
            return;
        };
        if id != job {
            return;
        }
        self.in_flight = None;
        if self.journal_down {
            return;
        }
        match (kind, result) {
            (Kind::Intent, Ok(sequence)) => self.intent_committed(id, sequence),
            (Kind::Intent, Err(writer::Error::JournalQuota | writer::Error::JournalDiskSpace)) => {
                self.journal_held = true;
                self.intent_held(id);
            }
            (Kind::Cancel, Ok(_)) => {
                if let Some(job) = self.jobs.get(&id)
                    && let Some(t) = job.cancel
                {
                    let event = Event::JobCancelRequested {
                        project: job.project,
                        conversation: job.agent.conversation,
                        job: id,
                        requester: reasons::audit_requester(t.requester),
                        reason: reasons::audit_reason(t.reason),
                        kill: t.kill,
                    };
                    self.audit(event);
                }
            }
            (Kind::Settled, Ok(sequence)) => self.settlement_committed(id, sequence),
            (_, Err(_)) => self.journal_unavailable(),
        }
    }

    fn intent_committed(&mut self, id: Id, sequence: u64) {
        self.dirty = true;
        self.bump_revision();
        let Some(job) = self.jobs.get_mut(&id) else {
            return;
        };
        job.intent = Intent::Committed;
        job.sequence = Some(sequence);
        if job.state == JobState::Starting
            && job.cancel.is_none()
            && let Some(start) = job.held_start.take()
        {
            // A person job: kind 26 committed after the grant.
            self.start_slot(id, start);
        }
    }

    fn intent_held(&mut self, id: Id) {
        let Some(job) = self.jobs.get_mut(&id) else {
            return;
        };
        if job.origin.is_person() && job.cancel.is_some() {
            // A person job cancelled while its kind 26 was in flight: no
            // record at all (TJ-S1).
            self.withdraw(id);
            return;
        }
        if job.origin.is_person() {
            // The slot and reservation return; the waiter waits again at the
            // head of its agent queue with `journal_held`.
            self.requeue_person(id);
        } else {
            // A model start waits with `journal_held`; when it was cancelled
            // or revoked, its kinds 27 and 28 follow the kind 26 after the
            // hold ends (TJ-S1).
            job.intent = Intent::Pending;
            self.intents.push_front(id);
        }
        self.dirty = true;
    }

    /// A granted person job returns to the head of its agent queue as a
    /// waiter: its slot and reservation are released.
    fn requeue_person(&mut self, id: Id) {
        let Some(job) = self.jobs.get_mut(&id) else {
            return;
        };
        if let Some(start) = job.held_start.take() {
            job.body = Some(Body::from_start(start));
        }
        job.intent = Intent::None;
        job.state = JobState::Queued;
        job.holds_slot = false;
        let agent = job.agent;
        if std::mem::take(&mut job.reserved) {
            self.releases.push((id, std::mem::take(&mut job.victims)));
        }
        self.queues.entry(agent).or_default().push_front(id);
        self.event(JobEvent::State(id, JobState::Queued));
        self.dirty = true;
    }

    fn settlement_committed(&mut self, id: Id, sequence: u64) {
        let Some(job) = self.jobs.get_mut(&id) else {
            return;
        };
        job.state = JobState::Recorded;
        job.committed = Some(sequence);
        let Some(settled) = job.settled.as_deref() else {
            return;
        };
        let outcome = settled.outcome;
        let event = Event::JobSettled {
            project: job.project,
            conversation: job.agent.conversation,
            job: id,
            outcome: reasons::audit_outcome(settled.outcome),
            reason: reasons::audit_reason(settled.reason),
            exit_kind: reasons::audit_exit_kind(settled.exit_kind),
            exit_value: settled.exit_value,
            cleanup: reasons::audit_cleanup(settled.cleanup),
            unconfirmed_reason: reasons::audit_unconfirmed(settled.unconfirmed_reason),
            escapees: settled.escapees,
            capture_state: reasons::audit_capture(settled.capture_state),
            stdout_bytes: settled.stdout.capture.stored_bytes,
            stderr_bytes: settled.stderr.capture.stored_bytes,
            elapsed_ms: settled.elapsed_ms,
            pause_count: settled.pause_count,
        };
        let recovered = job.recovered;
        self.audit(event);
        self.event(JobEvent::Settled(id, outcome));
        if recovered {
            self.retire(id);
            self.check_recovered();
        }
        self.dirty = true;
    }

    /// After kind 28 commits: the payload owner settles the ledger and the
    /// capture worker installs (R2).
    fn deliver(&mut self, payloads: &mut dyn PayloadPort) {
        for job in self.jobs.values_mut() {
            if job.delivered {
                continue;
            }
            if let (Some(sequence), Some(settled)) = (job.committed, job.settled.as_deref()) {
                job.delivered = true;
                if job.slot_started {
                    payloads.committed(job.id, settled, sequence, job.worker.take());
                } else if job.reserved {
                    payloads.release(job.id, &job.victims);
                }
            } else if job.abandoned && (job.capture.is_some() || job.settled.is_some()) {
                job.delivered = true;
                let worker = job.capture.take().and_then(|c| c.capture);
                payloads.abandon(job.id, worker.or(job.worker.take()));
            }
        }
    }

    /// Releases slots whose job is recorded (or abandoned) and whose slot is
    /// idle. A retained scratch keeps its slot until restart.
    fn release_slots(&mut self) {
        let mut done = Vec::new();
        for job in self.jobs.values_mut() {
            let finished = (job.state == JobState::Recorded || job.abandoned) && job.delivered;
            if !finished || job.scratch_retained {
                continue;
            }
            if job.slot.as_ref().is_none_or(|s| s.idle()) {
                if job.slot.take().is_some() || job.holds_slot {
                    self.dirty = true;
                }
                job.holds_slot = false;
                done.push(job.id);
            }
        }
        for id in done {
            self.retire(id);
        }
    }

    fn retire(&mut self, id: Id) {
        let Some(job) = self.jobs.remove(&id) else {
            return;
        };
        if job.abandoned {
            return;
        }
        if self.retired.len() >= RETIRED {
            self.retired.pop_front();
        }
        self.retired.push_back(Retired {
            job: id,
            digest: job.digest,
            origin: job.origin,
            agent: job.agent,
            project: job.project,
            sequence: job.sequence,
            intent_ms: job.intent_ms,
            outcome: job
                .settled
                .as_ref()
                .map_or(Outcome::Interrupted, |s| s.outcome),
            settled: job.settled.filter(|_| !job.recovered),
            excerpt: job.excerpt,
        });
    }

    // ----- Slot events -----

    fn slot_event(
        &mut self,
        id: Id,
        event: SlotEvent,
        now: Instant,
        payloads: &mut dyn PayloadPort,
    ) {
        let Some(job) = self.jobs.get_mut(&id) else {
            return;
        };
        match event {
            SlotEvent::Spawned => {
                job.spawned = true;
                job.spawned_at = Some(now);
                if job.state == JobState::Starting {
                    // A cancel before spawn: the slot wrote `T` at once.
                    job.state = if job.cancel.is_some() {
                        JobState::Stopping
                    } else {
                        JobState::Running
                    };
                }
                let event = started_event(job);
                let state = job.state;
                self.audit(event);
                self.event(JobEvent::Started(id));
                self.event(JobEvent::State(id, state));
            }
            SlotEvent::SetupFailed(reason) => {
                job.no_process = Some(reason);
                self.finish(id, payloads);
            }
            SlotEvent::IdentityChanged => {
                job.no_process = Some(JobReason::ProjectChanged);
                let project = job.project;
                self.revoke_where(
                    |j| j.project == project,
                    Some(id),
                    JobReason::IdentityChanged,
                );
                self.finish(id, payloads);
            }
            SlotEvent::LeaderExited(exit) => {
                job.exit = Some(exit);
                if matches!(
                    job.state,
                    JobState::Running | JobState::CapturePaused | JobState::Stopping
                ) {
                    self.set_state(id, JobState::Draining);
                }
            }
            SlotEvent::CleanupUnconfirmed(reason) => {
                job.unconfirmed = Some(reason);
                if matches!(
                    job.state,
                    JobState::Running
                        | JobState::CapturePaused
                        | JobState::Stopping
                        | JobState::Draining
                ) {
                    self.set_state(id, JobState::CleanupUnconfirmed);
                }
            }
            SlotEvent::CapturePaused(pause) => match job.state {
                JobState::Running => {
                    job.pause = Some(match pause {
                        capture_worker::PauseReason::Quota => journal::JobPauseReason::Quota,
                        capture_worker::PauseReason::Enospc => journal::JobPauseReason::DiskSpace,
                    });
                    self.set_state(id, JobState::CapturePaused);
                }
                JobState::Draining | JobState::Finalizing => {
                    // Quota or ENOSPC in `Draining` is a cancel trigger:
                    // capture discards and the reason is recorded.
                    let reason = match pause {
                        capture_worker::PauseReason::Quota => JobReason::Quota,
                        capture_worker::PauseReason::Enospc => JobReason::Enospc,
                    };
                    self.trigger(
                        id,
                        Trigger {
                            requester: JobRequester::Service,
                            reason,
                            kill: false,
                        },
                    );
                }
                _ => {}
            },
            SlotEvent::CaptureResumed => {
                if job.state == JobState::CapturePaused {
                    job.pause = None;
                    self.set_state(id, JobState::Running);
                }
            }
            SlotEvent::CaptureFault(reason) => {
                if reason != CaptureReason::Enospc {
                    self.trigger(
                        id,
                        Trigger {
                            requester: JobRequester::Service,
                            reason: JobReason::CaptureFault,
                            kill: false,
                        },
                    );
                }
            }
            SlotEvent::ProcessSettled {
                exit,
                cleanup,
                escapees,
                elapsed_ms,
                output_held: _,
            } => {
                job.exit.get_or_insert(exit);
                job.process = Some(Process {
                    exit,
                    cleanup,
                    escapees,
                    elapsed_ms,
                });
                if !matches!(
                    job.state,
                    JobState::Recording | JobState::Recorded | JobState::Withdrawn
                ) {
                    self.set_state(id, JobState::Finalizing);
                }
                self.finish(id, payloads);
            }
            SlotEvent::CaptureSettled(outcome) => {
                job.capture = Some(*outcome);
                self.finish(id, payloads);
            }
            SlotEvent::ScratchCleaned => {}
            SlotEvent::ScratchRetained => job.scratch_retained = true,
        }
    }

    /// Builds kind 28 once the process (or its absence) and the capture are
    /// both settled.
    fn finish(&mut self, id: Id, payloads: &mut dyn PayloadPort) {
        let journal_down = self.journal_down;
        let Some(job) = self.jobs.get_mut(&id) else {
            return;
        };
        if job.settled.is_some() || job.capture.is_none() {
            return;
        }
        if job.process.is_none() && job.no_process.is_none() {
            return;
        }
        if journal_down || job.abandoned {
            job.abandoned = true;
            job.delivered = true;
            let worker = job.capture.take().and_then(|c| c.capture);
            payloads.abandon(id, worker);
            return;
        }
        let mut capture = job.capture.take();
        job.worker = capture.as_mut().and_then(|c| c.capture.take());
        let settled = match (&job.process, capture) {
            (Some(process), Some(capture)) => process_settlement(job, process, capture),
            (_, capture) => not_started(
                job,
                job.no_process.unwrap_or(JobReason::SetupFailed),
                capture,
            ),
        };
        job.settled = Some(Box::new(settled));
        job.state = JobState::Recording;
        self.closures.push_back((id, Kind::Settled));
        self.event(JobEvent::State(id, JobState::Recording));
    }

    /// Kind 28 `not_started` for a job with no process: a queued or granted
    /// start that a trigger removed, or a slot start error.
    fn settle_without_process(&mut self, id: Id, reason: Option<JobReason>) {
        let journal_down = self.journal_down;
        let Some(job) = self.jobs.get_mut(&id) else {
            return;
        };
        if job.settled.is_some() {
            return;
        }
        drop(job.held_start.take());
        job.no_process = reason;
        job.holds_slot = job.slot.is_some();
        job.state = JobState::Recording;
        if journal_down {
            job.abandoned = true;
            job.delivered = true;
            return;
        }
        let settled = not_started(job, reason.unwrap_or(JobReason::SetupFailed), None);
        job.settled = Some(Box::new(settled));
        self.closures.push_back((id, Kind::Settled));
        self.event(JobEvent::State(id, JobState::Recording));
    }

    fn set_state(&mut self, id: Id, state: JobState) {
        if let Some(job) = self.jobs.get_mut(&id)
            && job.state != state
        {
            job.state = state;
            self.event(JobEvent::State(id, state));
        }
    }

    // ----- Start conditions -----

    fn counts(&self, agent: AgentKey, pending_installs: usize) -> Counts {
        let mut c = Counts {
            agent_limit: self.limits.live_agent as usize,
            service_limit: self.limits.live_service as usize,
            pending_installs,
            ..Counts::default()
        };
        for job in self.jobs.values().filter(|j| j.holds_slot) {
            let cleanup = job.state == JobState::CleanupUnconfirmed;
            c.service_live += 1;
            c.service_awaiting_cleanup += usize::from(cleanup);
            if job.agent == agent {
                c.agent_live += 1;
                c.agent_awaiting_cleanup += usize::from(cleanup);
            }
        }
        c
    }

    /// Conditions 1 to 7 for the head of `agent`'s queue; `Err(None)` when
    /// the head's kind 26 has not committed yet.
    fn head_block(
        &self,
        agent: AgentKey,
        head: &Job,
        payloads: &dyn PayloadPort,
    ) -> Result<(), Option<BlockReason>> {
        if self.recovering {
            return Err(Some(BlockReason::Recovering));
        }
        if self.journal_down {
            return Err(Some(BlockReason::JournalUnavailable));
        }
        if head.intent != Intent::Committed {
            // A model kind 26 still to commit, or a person kind 26 still to
            // write: condition 3 fails only while the journal is held.
            if self.journal_held {
                return Err(Some(BlockReason::JournalHeld));
            }
            if !head.origin.is_person() {
                return Err(None);
            }
        }
        if !payloads.ready() {
            return Err(Some(BlockReason::PayloadUnavailable));
        }
        let counts = self.counts(agent, payloads.pending_installs());
        if counts.agent_live >= counts.agent_limit {
            return Err(Some(BlockReason::AgentSlots));
        }
        if counts.service_live >= counts.service_limit {
            return Err(Some(BlockReason::ServiceSlots));
        }
        if counts.pending_installs >= PENDING_INSTALLS {
            return Err(Some(BlockReason::PendingInstalls));
        }
        Ok(())
    }

    /// Evaluates the queue heads. Agents rotate, beginning after the agent
    /// served last; within an agent a blocked head blocks the entries
    /// behind it.
    fn evaluate(&mut self, now: Instant, payloads: &mut dyn PayloadPort, disk_due: bool) {
        let mut disk_blocked = false;
        if !self.closed {
            loop {
                let mut agents: Vec<AgentKey> = self.queues.keys().copied().collect();
                if let Some(last) = self.last_served {
                    let split = agents.partition_point(|a| *a <= last);
                    agents.rotate_left(split);
                }
                let mut started = false;
                let mut execution_off = None;
                for agent in agents {
                    let Some(head) = self.queues.get(&agent).and_then(|q| q.front()).copied()
                    else {
                        continue;
                    };
                    let Some(job) = self.jobs.get(&head) else {
                        continue;
                    };
                    let mut block = self.head_block(agent, job, payloads);
                    if block.is_ok() && !job.origin.is_person() && !self.execution_on(job.project) {
                        // EX11: a change revokes queued model starts in
                        // the same reactor step, so this cannot hold in a
                        // correct owner; settle the head `not_started`.
                        execution_off = Some(head);
                        break;
                    }
                    let mut admitted = None;
                    if block.is_ok() {
                        match payloads.admit(head, &job.origin) {
                            Ok(a) => admitted = Some(a),
                            Err(Short::Quota) => block = Err(Some(BlockReason::Quota)),
                            Err(Short::DiskSpace) => block = Err(Some(BlockReason::DiskSpace)),
                        }
                    }
                    match (block, admitted) {
                        (Ok(()), Some(admitted)) => {
                            self.grant(agent, head, admitted);
                            self.last_served = Some(agent);
                            started = true;
                            break;
                        }
                        (Err(reason), _) => {
                            disk_blocked |= reason == Some(BlockReason::DiskSpace);
                            self.block_queue(agent, reason);
                        }
                        (Ok(()), None) => {}
                    }
                }
                if let Some(head) = execution_off {
                    tracing::warn!("job_execution_off_at_grant");
                    self.trigger(
                        head,
                        Trigger {
                            requester: JobRequester::Service,
                            reason: JobReason::ExecutionOff,
                            kill: false,
                        },
                    );
                    continue;
                }
                if !started {
                    break;
                }
            }
        }
        self.disk = match (disk_blocked, self.disk) {
            (false, _) => None,
            (true, None) => Some(Pacing::start(now)),
            (true, Some(mut d)) => {
                if disk_due {
                    // TJ-Q9: the head is still blocked by `disk_space` at
                    // this paced recheck: ask the payload owner to measure
                    // free space again, once per recheck.
                    payloads.refresh_space();
                    d.advance(now);
                }
                Some(d)
            }
        };
    }

    /// Every entry of a blocked agent queue shows the head's reason.
    fn block_queue(&mut self, agent: AgentKey, reason: Option<BlockReason>) {
        let ids: Vec<Id> = self
            .queues
            .get(&agent)
            .map(|q| q.iter().copied().collect())
            .unwrap_or_default();
        for id in ids {
            let Some(job) = self.jobs.get_mut(&id) else {
                continue;
            };
            if job.block == reason {
                continue;
            }
            job.block = reason;
            let Some(reason) = reason else {
                continue;
            };
            let audit = (!job.queued_audited).then(|| {
                job.queued_audited = true;
                Event::JobQueued {
                    project: job.project,
                    conversation: job.agent.conversation,
                    job: id,
                    origin: job.origin.audit(),
                    agent: job.agent.agent,
                    reason: reason.audit(),
                }
            });
            if let Some(event) = audit {
                self.audit(event);
            }
            self.event(JobEvent::Queued(id, reason));
        }
    }

    /// Takes the slot and the 1 MiB reservation for the head of `agent`.
    fn grant(&mut self, agent: AgentKey, id: Id, admitted: Admitted) {
        if let Some(queue) = self.queues.get_mut(&agent) {
            queue.pop_front();
            if queue.is_empty() {
                self.queues.remove(&agent);
            }
        }
        let gate_home = gate_home(&self.runtime);
        let runtime = self.runtime.clone();
        let Some(job) = self.jobs.get_mut(&id) else {
            return;
        };
        let Some(body) = job.body.take() else {
            return;
        };
        job.reserved = true;
        job.victims = admitted.victims.clone();
        job.holds_slot = true;
        job.block = None;
        job.state = JobState::Starting;
        let start = SlotStart {
            job: id,
            profile: body.profile,
            runtime,
            scope: body.scope,
            command: body.command,
            cwd: body.cwd,
            secrets: body.secrets,
            secret_command: body.secret_command,
            capture: capture_worker::Start {
                store: admitted.store,
                job: id,
                victims: admitted.victims,
                redaction: body.redaction,
                wake: None,
                gate_home,
            },
        };
        let person_intent = job.origin.is_person() && job.intent != Intent::Committed;
        if person_intent {
            // A person start is durable only when it gets its slot (J2-E7):
            // the slot and reservation are taken first, then kind 26.
            job.held_start = Some(start);
            job.intent = Intent::Pending;
            self.intents.push_back(id);
            self.event(JobEvent::State(id, JobState::Starting));
            return;
        }
        self.event(JobEvent::State(id, JobState::Starting));
        self.start_slot(id, start);
    }

    fn start_slot(&mut self, id: Id, start: SlotStart) {
        let mut slot = (self.slots)();
        let result = slot.start(start);
        let Some(job) = self.jobs.get_mut(&id) else {
            return;
        };
        match result {
            Ok(()) => {
                job.slot = Some(slot);
                job.slot_started = true;
            }
            Err(error) => {
                let reason = match error {
                    SlotStartError::CaptureUnavailable => JobReason::CaptureUnavailable,
                    SlotStartError::Busy | SlotStartError::Invalid => JobReason::SetupFailed,
                };
                self.settle_without_process(id, Some(reason));
            }
        }
    }

    // ----- Events, status and settlement -----

    /// At most 64 events per call. Undrained state events of one job
    /// coalesce, so the buffer stays bounded by the unsettled jobs.
    pub(crate) fn drain_events(&mut self, out: &mut Vec<JobEvent>) {
        let n = self.events.len().min(EVENTS_PER_DRAIN);
        out.extend(self.events.drain(..n));
    }
    fn event(&mut self, event: JobEvent) {
        self.bump_revision();
        let same = |e: &JobEvent| match (e, &event) {
            (JobEvent::State(a, _), JobEvent::State(b, _)) => a == b,
            (JobEvent::Queued(a, _), JobEvent::Queued(b, _)) => a == b,
            _ => false,
        };
        if let Some(slot) = self.events.iter_mut().find(|e| same(e)) {
            *slot = event;
        } else {
            self.events.push_back(event);
        }
    }
    fn audit(&self, event: Event) {
        if let Some(audit) = &self.audit {
            audit.emit(event);
        }
    }

    /// What the controller knows of one job (tests and the harness).
    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn status(&self, job: Id) -> Option<JobStatus> {
        if let Some(j) = self.jobs.get(&job) {
            let text = j.block.map(|reason| {
                let pending = self.seen.map_or(0, |s| s.1);
                block_text(reason, &self.counts(j.agent, pending))
            });
            return Some(JobStatus {
                state: j.state,
                origin: j.origin,
                agent: j.agent,
                reason: if j.state == JobState::Queued {
                    j.block
                } else {
                    None
                },
                text: if j.state == JobState::Queued {
                    text
                } else {
                    None
                },
                cancel: j.cancel.map(|t| t.reason),
                outcome: j.settled.as_ref().map(|s| s.outcome),
            });
        }
        self.retired
            .iter()
            .find(|r| r.job == job)
            .map(|r| JobStatus {
                state: JobState::Recorded,
                origin: r.origin,
                agent: r.agent,
                reason: None,
                text: None,
                cancel: None,
                outcome: Some(r.outcome),
            })
    }

    /// No slot is busy and no closure record waits or is in flight.
    pub(crate) fn settled(&self) -> bool {
        self.closures.is_empty()
            && self.in_flight.is_none_or(|(_, kind)| kind == Kind::Intent)
            && self
                .jobs
                .values()
                .filter_map(|j| j.slot.as_ref())
                .all(|s| s.idle())
    }
}

fn duplicate(existing: [u8; 32], proposed: [u8; 32], job: Id) -> Admission {
    if existing == proposed {
        Admission::Existing { job }
    } else {
        Admission::Conflict
    }
}

/// At most one `T` and one `K` per job (guardian lease bytes).
fn send_stop(job: &mut Job, kind: StopKind) {
    let sent = match kind {
        StopKind::Kill => &mut job.kill_sent,
        StopKind::Term => &mut job.term_sent,
    };
    if std::mem::replace(sent, true) || (kind == StopKind::Term && job.kill_sent) {
        return;
    }
    if let Some(slot) = job.slot.as_mut() {
        slot.stop(kind);
    }
}

fn started_event(job: &Job) -> Event {
    let (operation, generation, ordinal, input) = match job.origin {
        Origin::Model {
            operation,
            generation,
            ordinal,
        } => (Some(operation), Some(generation), Some(ordinal), None),
        Origin::Person { input } => (None, None, None, Some(input)),
    };
    Event::JobStarted {
        project: job.project,
        conversation: job.agent.conversation,
        job: job.id,
        origin: job.origin.audit(),
        agent: job.agent.agent,
        profile: match job.profile {
            Profile::Model1 => record::JobProfile::Model1,
        },
        operation,
        generation,
        ordinal,
        input,
    }
}

fn exit_facts(exit: Exit) -> (u8, u32) {
    match exit {
        Exit::Exited(code) => (1, u32::from(code)),
        Exit::Signalled(signal) => (2, u32::from(signal)),
    }
}

fn unconfirmed_code(reason: UnconfirmedReason) -> u8 {
    match reason {
        UnconfirmedReason::CleanupFailed => 9,
        UnconfirmedReason::EscapeeUnterminated => 13,
    }
}

/// Kind 28 for a job whose process settled.
fn process_settlement(job: &Job, process: &Process, capture: CaptureOutcome) -> JobSettled {
    let (exit_kind, exit_value) = exit_facts(process.exit);
    let outcome = if job.before_exit && job.cancel.is_some() {
        Outcome::Cancelled
    } else if exit_kind == 1 {
        Outcome::Exited
    } else {
        Outcome::Signalled
    };
    let (cleanup, unconfirmed_reason) = match job.unconfirmed {
        Some(reason) => (JobCleanup::Late, unconfirmed_code(reason)),
        None if process.cleanup == JobCleanup::Late => (
            JobCleanup::Late,
            unconfirmed_code(UnconfirmedReason::CleanupFailed),
        ),
        None => (JobCleanup::Confirmed, 0),
    };
    // A process outcome never carries capture `not_started`: a worker that
    // could not report is `unconfirmed`, with no digest (AE-05).
    let unconfirmed = capture.state == CaptureState::NotStarted;
    let capture_state = match capture.state {
        CaptureState::Complete => JobCaptureState::Complete,
        CaptureState::Incomplete => JobCaptureState::Incomplete,
        CaptureState::Unconfirmed | CaptureState::NotStarted => JobCaptureState::Unconfirmed,
    };
    let strip = |mut stream: JobStream| {
        if unconfirmed {
            stream.capture.digest = None;
        }
        stream
    };
    JobSettled {
        job: job.id,
        outcome,
        reason: job.reason,
        exit_kind,
        exit_value,
        cleanup,
        unconfirmed_reason,
        escapees: process.escapees,
        elapsed_ms: process.elapsed_ms,
        capture_state,
        capture_reason: if unconfirmed {
            CaptureReason::SettlementUnconfirmed
        } else {
            capture.reason
        },
        stdout: strip(capture.stdout),
        stderr: strip(capture.stderr),
        distinct_numbers: capture.distinct_numbers,
        numbers: capture.numbers,
        pause_count: capture.pause_count,
        first_pause_ms: capture.first_pause_ms,
        paused_ms: capture.paused_ms,
        final_reservation: capture.final_reservation,
        settled_time: wall_ms(),
    }
}

/// Kind 28 `not_started`. The reason is the first cancel trigger, else the
/// slot's setup reason (12 to 18). Capture facts keep the kind 23
/// `not_started` rules: no digest and no stored bytes.
fn not_started(job: &Job, setup: JobReason, capture: Option<CaptureOutcome>) -> JobSettled {
    let reason = job.cancel.map_or(setup, |t| t.reason);
    let (capture_reason, final_reservation) = match capture {
        Some(c) if c.state == CaptureState::NotStarted => (c.reason, c.final_reservation),
        _ => (CaptureReason::None, 0),
    };
    JobSettled {
        job: job.id,
        outcome: Outcome::NotStarted,
        reason,
        exit_kind: 0,
        exit_value: 0,
        cleanup: JobCleanup::None,
        unconfirmed_reason: 0,
        escapees: 0,
        elapsed_ms: 0,
        capture_state: JobCaptureState::NotStarted,
        capture_reason,
        stdout: JobStream::default(),
        stderr: JobStream::default(),
        distinct_numbers: 0,
        numbers: Vec::new(),
        pause_count: 0,
        first_pause_ms: 0,
        paused_ms: 0,
        final_reservation,
        settled_time: wall_ms(),
    }
}

/// Unix milliseconds of the service clock; 0 only when the clock read
/// fails (kind 26 field 4, kind 28 field 22).
fn wall_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// The scratch home whose files drive the test-support capture gates.
fn gate_home(runtime: &RuntimeDirectory) -> Option<std::path::PathBuf> {
    #[cfg(feature = "test-support")]
    {
        runtime
            .scratch_home_for_test()
            .map(std::path::Path::to_path_buf)
    }
    #[cfg(not(feature = "test-support"))]
    {
        let _ = runtime;
        None
    }
}
