//! One job-mode call of the active turn (tool-job-controller.md, J3): a
//! model `exec_cmd` call that runs as a job (operation code 10) or a `job`
//! tool command (codes 11 to 16), from its committed kind 14 `run` to its
//! kind 29 and the rendered result text.
//!
//! The call is a state machine on the reactor. It never blocks and never
//! submits anything itself: the conversation owner hands it its inputs (the
//! identity recheck outcome, a journal reply, the note's commit, an ended
//! controller wait, the turn's end) and takes its actions
//! ([`JobCall::action`]) when the serialized journal ticket is free. Every
//! transition runs in [`JobCall::advance`], which the owner calls after the
//! job controller's poll in the same reactor step, so a snapshot for kind
//! 29 always follows the poll (poll before a snapshot).
//!
//! No wait here has a time bound of its own. The yield window and the
//! `wait` `seconds` are caller-given bounds that the controller keeps;
//! every other wait ends on its reply or the turn's end (then `cancelled`).
use crate::context::taint::Taint;
use crate::context::view::{self, RangePlan, Shape, StageA};
use crate::job_controller::{
    AgentKey, CancelReply, JobController, JobReason, Requester, WaitEnding, WaitStart,
};
use crate::job_text::{self, Render, TurnCursors};
use crate::tools::JobArguments;
use asura_storage::authority::conversation::{
    self as journal, Id, JobCaptureState, JobListEntry, JobOrigin, JobPage, JobPageState,
    JobResult, JobResultBody, JobResultOutcome as O, JobRetention, JobRetentionSummary,
    JobRunState, JobScope, JobSettled, JobSnapshot, MAX_JOB_LIST_ENTRIES, OutputStream,
    RetentionAction, RetentionAuthor, ToolOperation, WaitEnd,
};
use asura_storage::authority::writer;
use asura_storage::payload::{self, PagePool, PageReply, PageRequest, PageState, Ticket};
use sha2::{Digest, Sha256};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

/// Notes per job, model and person together (J3 numeric limits).
pub(super) const MAX_NOTES: u32 = 16;
/// Kind 30 records per job (kind 30 rules).
pub(super) const MAX_RETENTION_RECORDS: u32 = 64;
/// The text of a result that could not be rendered from its records.
const UNRENDERED: &str = "unavailable: job records or output are unavailable now";

/// What the owner gives one step of a call.
pub(super) struct Env<'a> {
    /// The controller; `None` while the start-time limits read has not
    /// built it (TJ-F5).
    pub jobs: Option<&'a mut JobController>,
    /// The page pool, while the payload store is ready.
    pub pages: Option<&'a PagePool>,
    pub installation: Id,
    pub cursors: &'a TurnCursors,
    pub operation: Id,
    pub generation: u64,
    pub conversation: Id,
    pub now: Instant,
    /// The turn's stage A budget inputs for this call (context-composer.md,
    /// CC2 stage A sizing).
    pub stage: StageA,
}

/// The project identity recheck of a `job` command, from the tool executor.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Recheck {
    Same,
    /// The project was replaced: the owner revoked its jobs (J2-E10).
    Changed,
    Cancelled,
    Failed,
}

/// The owner's next action for this call.
#[derive(Debug)]
pub(super) enum Action {
    /// Start the identity recheck on the tool executor.
    Recheck,
    /// Submit a journal read on the serialized ticket.
    Read(writer::Command),
    /// Append kind 30, a model note.
    Note(journal::Record),
    /// Append kind 29, the result.
    Result(journal::Record),
}

/// One job's facts from its `JobFacts` read.
struct Known {
    origin: JobOrigin,
    sequence: u64,
    command: String,
    whole_command: bool,
    settled: Option<Box<JobSettled>>,
    retention: JobRetentionSummary,
    /// The taint decision from the job's command source (T1).
    taint: Taint,
}

/// One rendered job (owned; [`job_text::JobView`] borrows it).
#[derive(Clone)]
struct Shown {
    job: Id,
    origin: job_text::Origin,
    sequence: u64,
    command: String,
    whole_command: bool,
    settled: Option<Box<JobSettled>>,
    /// A tainted job's list row (T1).
    private: bool,
}

/// A stage A view being read before kind 29 (CC2): the planned ranges,
/// the read in flight, the ranges read so far, and the result and jobs to
/// render once the reads end.
struct ViewRead {
    ranges: Vec<RangePlan>,
    next: usize,
    ticket: Option<(Ticket, RangePlan)>,
    read: Vec<(JobPage, String)>,
    result: JobResult,
    shown: Vec<Shown>,
}

enum Step {
    /// Waiting for the kind 14 commit.
    Intent,
    /// Kind 14 committed: the call begins at the next `advance`.
    Start,
    /// The identity recheck to start / running.
    Recheck,
    Rechecking,
    /// A journal read to submit / in flight.
    Read(writer::Command),
    Reading,
    /// A controller agent wait is parked (yield window or `job wait`).
    Parked,
    /// A model page read on the pool.
    Page {
        ticket: Ticket,
        stream: OutputStream,
        offset: u64,
        known: u64,
        live: bool,
    },
    /// The ranges of a stage A view on the pool, one at a time (CC2).
    View(Box<ViewRead>),
    /// A kind 30 model note to append / in flight.
    Note(journal::Record),
    Noting(journal::Record),
    /// Kind 29 is built and rendered: append when allowed.
    Result,
    Appending,
    Done,
}

/// What a parked wait or a reread finishes with.
#[derive(Clone, Copy)]
enum Ending {
    /// Code 10: the state block; `cancelled` with status 6.
    Delivery { cancelled: bool },
    /// Code 14: the wait block.
    Wait(WaitEnd),
    /// Code 12 (`status`) and 15 (`cancel`): the state block.
    State,
}

/// One job-mode call. Code 10 holds the job it submitted.
pub(super) struct JobCall {
    code: ToolOperation,
    args: Option<JobArguments>,
    ordinal: u32,
    job: Id,
    agent: AgentKey,
    yield_seconds: u32,
    /// The stored command and the SL7 flag of the call's kind 14 (code 10).
    command: String,
    whole_command: bool,
    step: Step,
    known: Option<Known>,
    /// The turn ended (cancel, steer, terminal or helper exit).
    ended: bool,
    // Inputs, taken by `advance`.
    recheck: Option<Recheck>,
    reply: Option<writer::Result<writer::Reply>>,
    wait: Option<WaitEnding>,
    noted: Option<writer::Result<()>>,
    /// A finished state whose kind 28 facts are being read again.
    refresh: Option<(Ending, JobSnapshot)>,
    result: Option<JobResult>,
    text: Option<String>,
    /// Code 10: the job's kind 26 sequence, latched once seen committed.
    /// The controller forgets a settled job after 64 newer ones retire, so
    /// the kind 29 hold (JR13) must not depend on it still holding the job.
    intent: Option<u64>,
}

fn wall_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

fn settled_snapshot() -> JobSnapshot {
    JobSnapshot {
        state: JobRunState::Settled,
        numbers: [0; 3],
        elapsed_ms: 0,
        stdout_bytes: 0,
        stderr_bytes: 0,
    }
}

/// A state that is not `settled` (JR14 needs kind 28 earlier for state 9).
fn finalizing() -> JobSnapshot {
    JobSnapshot {
        state: JobRunState::Finalizing,
        numbers: [0; 3],
        elapsed_ms: 0,
        stdout_bytes: 0,
        stderr_bytes: 0,
    }
}

fn stream_index(stream: OutputStream) -> usize {
    match stream {
        OutputStream::Stdout => 0,
        OutputStream::Stderr => 1,
    }
}

impl JobCall {
    /// A model `exec_cmd` call whose start the controller accepted as `job`.
    pub(super) fn exec(
        ordinal: u32,
        job: Id,
        agent: AgentKey,
        yield_seconds: u32,
        command: String,
        whole_command: bool,
    ) -> Self {
        let mut call = Self::new(ToolOperation::ExecCmd, None, ordinal, job, agent);
        call.yield_seconds = yield_seconds;
        call.command = command;
        call.whole_command = whole_command;
        call
    }
    /// A `job` tool command (codes 11-16).
    pub(super) fn command(ordinal: u32, args: JobArguments, agent: AgentKey) -> Self {
        let job = match &args {
            JobArguments::List { .. } => [0; 16],
            JobArguments::Status { job }
            | JobArguments::Read { job, .. }
            | JobArguments::Wait { job, .. }
            | JobArguments::Cancel { job }
            | JobArguments::Note { job, .. } => *job,
        };
        Self::new(args.operation(), Some(args), ordinal, job, agent)
    }
    fn new(
        code: ToolOperation,
        args: Option<JobArguments>,
        ordinal: u32,
        job: Id,
        agent: AgentKey,
    ) -> Self {
        Self {
            code,
            args,
            ordinal,
            job,
            agent,
            yield_seconds: 0,
            command: String::new(),
            whole_command: false,
            step: Step::Intent,
            known: None,
            ended: false,
            recheck: None,
            reply: None,
            wait: None,
            noted: None,
            refresh: None,
            result: None,
            text: None,
            intent: None,
        }
    }

    /// The job this call names, for `ToolProgress.job_id`; `None` for
    /// `list`.
    pub(super) fn job(&self) -> Option<Id> {
        (self.job != [0; 16]).then_some(self.job)
    }
    /// The kind 14 committed.
    pub(super) fn committed(&mut self) {
        if matches!(self.step, Step::Intent) {
            self.step = Step::Start;
        }
    }
    /// An agent wait is parked in the controller (the owner ends it with
    /// `end_turn_wait` when the turn ends).
    pub(super) fn parked(&self) -> bool {
        matches!(self.step, Step::Parked)
    }
    // ----- Inputs -----

    pub(super) fn recheck_done(&mut self, outcome: Recheck) {
        self.recheck = Some(outcome);
    }
    pub(super) fn journal_done(&mut self, reply: writer::Result<writer::Reply>) {
        self.reply = Some(reply);
    }
    pub(super) fn note_done(&mut self, result: writer::Result<()>) {
        self.noted = Some(result);
    }
    pub(super) fn wait_done(&mut self, end: WaitEnding) {
        if self.parked() {
            self.wait = Some(end);
        }
    }
    /// The turn ended. A parked wait is the owner's to end
    /// (`end_turn_wait`, then [`Self::turn_wait_ended`]); a page read is
    /// withdrawn; work in flight finishes and the call then records
    /// `cancelled`.
    pub(super) fn turn_ended(&mut self) {
        self.ended = true;
    }
    /// The owner ended the parked wait at the turn's end: a yield window
    /// cancelled its job (`turn_ended`, TJ-04); a `job wait` ended.
    pub(super) fn turn_wait_ended(&mut self, env: &mut Env) {
        self.ended = true;
        if !self.parked() {
            return;
        }
        if self.code == ToolOperation::ExecCmd {
            let snapshot = self.live_snapshot(env).unwrap_or_else(finalizing);
            self.finish(env, Ending::Delivery { cancelled: true }, snapshot);
        } else {
            self.outcome(env, O::Cancelled);
        }
    }

    /// The kind 29 committed (`Ok`), or failed (`Err`): the owner's
    /// unavailable or held paths apply.
    pub(super) fn result_done(&mut self, committed: bool) {
        self.step = if committed { Step::Done } else { Step::Result };
    }

    // ----- Actions -----

    /// The next action when the ticket is free, and its in-flight step.
    /// Code 10's kind 29 waits until the job's kind 26 committed (JR13).
    pub(super) fn action(&mut self, jobs: Option<&JobController>) -> Option<Action> {
        self.step_action(Self::intent_of(jobs, self.job))
    }
    /// The job's kind 26 sequence as the controller now shows it.
    fn intent_of(jobs: Option<&JobController>, job: Id) -> Option<u64> {
        jobs.and_then(|j| j.view(job, Instant::now()))
            .and_then(|v| v.sequence)
    }
    /// Latches the kind 26 sequence the first time it is seen committed.
    fn latch_intent(&mut self, seen: Option<u64>) {
        if self.intent.is_none() {
            self.intent = seen;
        }
    }
    /// [`Self::action`] with the controller's current view of the job's
    /// kind 26 sequence.
    fn step_action(&mut self, seen: Option<u64>) -> Option<Action> {
        self.latch_intent(seen);
        match std::mem::replace(&mut self.step, Step::Done) {
            Step::Recheck => {
                self.step = Step::Rechecking;
                Some(Action::Recheck)
            }
            Step::Read(command) => {
                self.step = Step::Reading;
                Some(Action::Read(command))
            }
            Step::Note(record) => {
                self.step = Step::Noting(record.clone());
                Some(Action::Note(record))
            }
            Step::Result => {
                let intent_committed = self.code != ToolOperation::ExecCmd || self.intent.is_some();
                let Some(result) = self.result.clone().filter(|_| intent_committed) else {
                    self.step = Step::Result;
                    return None;
                };
                self.step = Step::Appending;
                Some(Action::Result(journal::Record::JobResult(Box::new(result))))
            }
            other => {
                self.step = other;
                None
            }
        }
    }
    /// The next action appends a record (kind 30 or kind 29): it waits
    /// while the journal holds normal records (SJ-D19).
    pub(super) fn next_is_append(&self) -> bool {
        matches!(self.step, Step::Note(_) | Step::Result)
    }
    /// The journal refused the append at its quota: it is tried again when
    /// the hold ends.
    pub(super) fn append_refused(&mut self) {
        self.step = match std::mem::replace(&mut self.step, Step::Done) {
            Step::Noting(record) => Step::Note(record),
            Step::Appending => Step::Result,
            other => other,
        };
    }
    /// The tool executor could not start the recheck.
    pub(super) fn recheck_not_started(&mut self) {
        self.step = Step::Rechecking;
        self.recheck = Some(Recheck::Failed);
    }
    /// The result and its text, once kind 29 is built.
    pub(super) fn result(&self) -> Option<(&JobResult, &str)> {
        Some((self.result.as_ref()?, self.text.as_deref()?))
    }

    // ----- Transitions -----

    /// Runs every transition whose input arrived. In-memory work only.
    pub(super) fn advance(&mut self, env: &mut Env) {
        if self.code == ToolOperation::ExecCmd {
            let seen = Self::intent_of(env.jobs.as_deref(), self.job);
            self.latch_intent(seen);
        }
        match self.step {
            Step::Start => self.start(env),
            Step::Rechecking => {
                if let Some(outcome) = self.recheck.take() {
                    self.rechecked(env, outcome);
                }
            }
            Step::Reading => {
                if let Some(reply) = self.reply.take() {
                    self.read_done(env, reply);
                }
            }
            Step::Noting(_) => {
                if let Some(result) = self.noted.take() {
                    match result {
                        Ok(()) => self.outcome(env, O::Ok),
                        Err(_) => self.outcome(env, O::Unavailable),
                    }
                }
            }
            Step::Parked => {
                if let Some(end) = self.wait.take() {
                    self.wait_ended(env, end);
                }
            }
            Step::Page { .. } => self.page(env),
            Step::View(_) => self.view_step(env),
            _ => {}
        }
    }

    fn start(&mut self, env: &mut Env) {
        if self.code == ToolOperation::ExecCmd {
            let Some(jobs) = env.jobs.as_deref_mut() else {
                // The controller accepted this job: it exists.
                self.finish(env, Ending::Delivery { cancelled: true }, finalizing());
                return;
            };
            if self.ended {
                // The turn ended before handle delivery (TJ-04).
                let _ = jobs.cancel(self.job, Requester::Model, JobReason::TurnEnded, false);
                let snapshot = self.live_snapshot(env).unwrap_or_else(finalizing);
                self.finish(env, Ending::Delivery { cancelled: true }, snapshot);
                return;
            }
            match jobs.await_yield(self.agent, self.job, self.yield_seconds) {
                WaitStart::Parked => self.step = Step::Parked,
                WaitStart::Done(end) => {
                    self.step = Step::Parked;
                    self.wait_ended(env, end);
                }
                WaitStart::NotLive | WaitStart::Busy => {
                    let snapshot = self.live_snapshot(env).unwrap_or_else(finalizing);
                    self.finish(env, Ending::Delivery { cancelled: false }, snapshot);
                }
            }
            return;
        }
        // `cancel` acts at once, also after the turn ended; the others
        // record `cancelled` (TCR-D8: no result is sent then).
        if self.ended && self.code != ToolOperation::JobCancel {
            self.outcome(env, O::Cancelled);
            return;
        }
        self.step = Step::Recheck;
    }

    fn rechecked(&mut self, env: &mut Env, outcome: Recheck) {
        match outcome {
            Recheck::Same => {}
            // A mismatch revoked the project's jobs (J2-E10); `list` has
            // no `not_found` outcome.
            Recheck::Changed if self.code == ToolOperation::JobList => {
                return self.outcome(env, O::Unavailable);
            }
            Recheck::Changed => return self.outcome(env, O::NotFound),
            Recheck::Cancelled if self.code != ToolOperation::JobCancel => {
                return self.outcome(env, O::Cancelled);
            }
            Recheck::Cancelled | Recheck::Failed => return self.outcome(env, O::Unavailable),
        }
        if self.ended && self.code != ToolOperation::JobCancel {
            return self.outcome(env, O::Cancelled);
        }
        self.step = Step::Read(match &self.args {
            Some(JobArguments::List { before }) => writer::Command::Jobs {
                scope: JobScope::Conversation(env.conversation),
                before: *before,
                limit: journal::MAX_JOB_LISTING_READ,
            },
            _ => writer::Command::JobFacts { job: self.job },
        });
    }

    fn read_done(&mut self, env: &mut Env, reply: writer::Result<writer::Reply>) {
        let Ok(reply) = reply else {
            return self.outcome(env, O::Unavailable);
        };
        if self.ended && self.code != ToolOperation::JobCancel && self.refresh.is_none() {
            return self.outcome(env, O::Cancelled);
        }
        if self.code == ToolOperation::JobList {
            return self.list(env, reply.jobs);
        }
        let Some(records) = reply.job_records else {
            return self.outcome(env, O::NotFound);
        };
        // Scope (TJ-12): only the jobs of the turn's conversation.
        if records.listing.conversation != env.conversation || records.listing.job != self.job {
            return self.outcome(env, O::NotFound);
        }
        // A job read from the journal has its kind 26 committed.
        self.latch_intent(Some(records.listing.intent_frame.sequence));
        let known = Known {
            origin: records.listing.intent.origin,
            sequence: records.listing.intent_frame.sequence,
            command: records.command.command().unwrap_or_default(),
            whole_command: records.command.whole_command(),
            settled: records.listing.settled.clone(),
            retention: records.listing.retention,
            taint: Taint::of(&records.command),
        };
        self.known = Some(known);
        if let Some((ending, snapshot)) = self.refresh.take() {
            return self.finish(env, ending, snapshot);
        }
        self.dispatch(env);
    }

    /// Codes 12-16 once the job's facts are known.
    fn dispatch(&mut self, env: &mut Env) {
        // T1: a tainted job's status, page and wait are private; nothing
        // is read and no wait starts (context-composer.md, taint hook).
        let tainted = self.known.as_ref().is_some_and(|k| k.taint.is_tainted());
        if tainted
            && matches!(
                self.code,
                ToolOperation::JobStatus | ToolOperation::JobRead | ToolOperation::JobWait
            )
        {
            return self.outcome(env, O::Private);
        }
        let settled = self.known.as_ref().is_some_and(|k| k.settled.is_some());
        let person = self
            .known
            .as_ref()
            .is_some_and(|k| matches!(k.origin, JobOrigin::Person { .. }));
        match self.args.clone() {
            Some(JobArguments::Status { .. }) => {
                if settled {
                    return self.finish(env, Ending::State, settled_snapshot());
                }
                match self.live_snapshot(env) {
                    Some(snapshot) => self.finish(env, Ending::State, snapshot),
                    // An unsettled job without the controller (TJ-F5).
                    None => self.outcome(env, O::Unavailable),
                }
            }
            Some(JobArguments::Read {
                stream,
                offset,
                limit,
                ..
            }) => self.read_page(env, stream, offset, limit),
            Some(JobArguments::Wait { seconds, .. }) => {
                if settled {
                    return self.finish(
                        env,
                        Ending::Wait(WaitEnd::AlreadySettled),
                        settled_snapshot(),
                    );
                }
                let Some(jobs) = env.jobs.as_deref_mut() else {
                    return self.outcome(env, O::Unavailable);
                };
                match jobs.await_job(self.agent, self.job, seconds, env.now) {
                    WaitStart::Parked => self.step = Step::Parked,
                    WaitStart::Done(end) => {
                        self.step = Step::Parked;
                        self.wait_ended(env, end);
                    }
                    WaitStart::NotLive | WaitStart::Busy => self.outcome(env, O::Unavailable),
                }
            }
            Some(JobArguments::Cancel { .. }) => {
                if person {
                    return self.outcome(env, O::NotPermitted);
                }
                if settled {
                    return self.finish(env, Ending::State, settled_snapshot());
                }
                let Some(jobs) = env.jobs.as_deref_mut() else {
                    return self.outcome(env, O::Unavailable);
                };
                match jobs.cancel(self.job, Requester::Model, JobReason::ModelCancel, false) {
                    CancelReply::Accepted(_) | CancelReply::AlreadySettled(_) => {
                        match self.settled_from_controller(env) {
                            true => self.finish(env, Ending::State, settled_snapshot()),
                            false => {
                                let snapshot = self.live_snapshot(env).unwrap_or_else(finalizing);
                                self.finish(env, Ending::State, snapshot)
                            }
                        }
                    }
                    CancelReply::NotPermitted => self.outcome(env, O::NotPermitted),
                    CancelReply::NotFound => self.outcome(env, O::Unavailable),
                }
            }
            Some(JobArguments::Note { .. }) => {
                let full = self.known.as_ref().is_none_or(|k| {
                    k.retention.notes >= MAX_NOTES || k.retention.records >= MAX_RETENTION_RECORDS
                });
                if full {
                    return self.outcome(env, O::Limit);
                }
                self.step = Step::Note(journal::Record::JobRetention(Box::new(JobRetention {
                    job: self.job,
                    action: RetentionAction::Note,
                    author: RetentionAuthor::Model {
                        operation: env.operation,
                        generation: env.generation,
                        ordinal: self.ordinal,
                    },
                    time: wall_ms(),
                    note: String::new(),
                })));
            }
            Some(JobArguments::List { .. }) | None => self.outcome(env, O::Unavailable),
        }
    }

    /// The controller's live snapshot of the job; `None` when it does not
    /// hold it, or holds it settled (the journal is then the source).
    fn live_snapshot(&self, env: &mut Env) -> Option<JobSnapshot> {
        let view = env.jobs.as_deref()?.view(self.job, env.now)?;
        if view.settled() {
            return self.settled_known(env).then(settled_snapshot);
        }
        Some(view.snapshot)
    }
    /// The job's kind 28 is known: from the read, or from the controller.
    fn settled_known(&self, env: &Env) -> bool {
        self.known.as_ref().is_some_and(|k| k.settled.is_some())
            || env
                .jobs
                .as_deref()
                .is_some_and(|j| j.settled_facts(self.job).is_some())
    }
    /// Adopts the controller's committed kind 28 when it has one.
    fn settled_from_controller(&mut self, env: &Env) -> bool {
        let Some(facts) = env
            .jobs
            .as_deref()
            .and_then(|j| j.settled_facts(self.job))
            .cloned()
        else {
            return false;
        };
        if let Some(known) = &mut self.known {
            known.settled = Some(Box::new(facts));
        } else {
            self.known = Some(Known {
                origin: JobOrigin::Model {
                    operation: env.operation,
                    generation: env.generation,
                    ordinal: self.ordinal,
                },
                sequence: env
                    .jobs
                    .as_deref()
                    .and_then(|j| j.view(self.job, env.now))
                    .and_then(|v| v.sequence)
                    .unwrap_or(0),
                command: self.command.clone(),
                whole_command: self.whole_command,
                settled: Some(Box::new(facts)),
                retention: JobRetentionSummary::default(),
                taint: Taint::Clean,
            });
        }
        true
    }

    fn wait_ended(&mut self, env: &mut Env, end: WaitEnding) {
        let ending = |wait: WaitEnd| {
            if self.code == ToolOperation::ExecCmd {
                Ending::Delivery { cancelled: false }
            } else {
                Ending::Wait(wait)
            }
        };
        match end {
            WaitEnding::Settled | WaitEnding::AlreadySettled => {
                let wait = if end == WaitEnding::Settled {
                    WaitEnd::Settled
                } else {
                    WaitEnd::AlreadySettled
                };
                let ending = ending(wait);
                if self.settled_from_controller(env) {
                    self.finish(env, ending, settled_snapshot());
                } else {
                    // The facts are read from the journal (J3-E7).
                    self.refresh = Some((ending, settled_snapshot()));
                    self.step = Step::Read(writer::Command::JobFacts { job: self.job });
                }
            }
            WaitEnding::NewOutput | WaitEnding::Time | WaitEnding::Ready => {
                let wait = match end {
                    WaitEnding::NewOutput => WaitEnd::NewOutput,
                    _ => WaitEnd::Time,
                };
                let snapshot = self.live_snapshot(env).unwrap_or_else(finalizing);
                self.finish(env, ending(wait), snapshot);
            }
            // The journal became unavailable or the service stops: the
            // owner's own paths end the turn.
            WaitEnding::Withdrawn
            | WaitEnding::Closed
            | WaitEnding::StateChanged
            | WaitEnding::Revision(_) => {
                if self.code == ToolOperation::ExecCmd {
                    self.finish(env, Ending::Delivery { cancelled: true }, finalizing());
                } else {
                    self.outcome(env, O::Unavailable);
                }
            }
        }
    }

    fn read_page(&mut self, env: &mut Env, stream: OutputStream, offset: Option<u64>, limit: u32) {
        let offset = offset.unwrap_or_else(|| env.cursors.get(&self.job, stream));
        let known = self.known.as_ref();
        let settled = known.and_then(|k| k.settled.as_deref());
        let view = env
            .jobs
            .as_deref()
            .and_then(|j| j.view(self.job, env.now))
            .filter(|v| !v.settled());
        if settled.is_none() && view.is_none() {
            // An unsettled job the controller does not hold: nothing to
            // page from (TJ-F5 reads the journal only for settled jobs).
            return self.outcome(env, O::Unavailable);
        }
        // No payload exists before the spawn, nor after a settlement with
        // no process: an empty page, without a read.
        let (state, total) = match (settled, &view) {
            (Some(s), _) => {
                let total = match stream {
                    OutputStream::Stdout => s.stdout.capture.stored_bytes,
                    OutputStream::Stderr => s.stderr.capture.stored_bytes,
                };
                (
                    matches!(
                        s.capture_state,
                        JobCaptureState::NotStarted | JobCaptureState::Interrupted
                    )
                    .then_some(JobPageState::Complete),
                    total,
                )
            }
            (None, Some(v)) => (
                v.snapshot
                    .state
                    .before_spawn()
                    .then_some(JobPageState::Live),
                [v.snapshot.stdout_bytes, v.snapshot.stderr_bytes][stream_index(stream)],
            ),
            (None, None) => (None, 0),
        };
        if let Some(state) = state {
            let page = JobPage {
                stream,
                start: 0,
                source_len: 0,
                total: 0,
                state,
                digest: [0; 32],
            };
            return self.page_result(env, page, String::new());
        }
        let Some(pages) = env.pages else {
            return self.outcome(env, O::Unavailable);
        };
        let payload = payload::job_payloads(env.installation, self.job)[stream_index(stream)];
        match pages.submit(
            PageRequest {
                payload,
                offset,
                // The composer's page cap (CC2 stage A sizing, rule 3).
                limit: env.stage.page_cap(limit) as usize,
                scope: None,
                lines: asura_storage::payload::Lines::Any,
            },
            payload::Origin::Model,
        ) {
            Ok(ticket) => {
                self.step = Step::Page {
                    ticket,
                    stream,
                    offset,
                    known: total,
                    live: settled.is_none(),
                }
            }
            // The page pool's queue of 8 is full.
            Err(_) => self.outcome(env, O::Busy),
        }
    }

    fn page(&mut self, env: &mut Env) {
        let Step::Page { ticket, .. } = &self.step else {
            return;
        };
        if self.ended {
            // Dropping the ticket withdraws the read (WB1).
            return self.outcome(env, O::Cancelled);
        }
        let Some(reply) = ticket.poll() else {
            return;
        };
        let Step::Page {
            stream,
            offset,
            known,
            live,
            ..
        } = std::mem::replace(&mut self.step, Step::Done)
        else {
            return;
        };
        let PageReply::Page(page) = reply else {
            return self.outcome(env, O::Unavailable);
        };
        let state = match page.state {
            PageState::Live => JobPageState::Live,
            PageState::Complete => JobPageState::Complete,
            PageState::Incomplete => JobPageState::Incomplete,
            PageState::UnverifiedPrefix => JobPageState::UnverifiedPrefix,
            PageState::Expired => JobPageState::Expired,
            PageState::Missing => JobPageState::Missing,
            PageState::Corrupt => JobPageState::Corrupt,
            PageState::Deleted => JobPageState::Deleted,
            PageState::StaleCursor => {
                // The offset is past the stored bytes: an empty page at the
                // end.
                let state = if live {
                    JobPageState::Live
                } else {
                    JobPageState::Complete
                };
                let at = page.total.max(known);
                let page = JobPage {
                    stream,
                    start: at,
                    source_len: 0,
                    total: at,
                    state,
                    digest: [0; 32],
                };
                return self.page_result(env, page, String::new());
            }
            PageState::Busy | PageState::Denied => return self.outcome(env, O::Unavailable),
        };
        if !state.readable() {
            let page = JobPage {
                stream,
                start: offset.min(known),
                source_len: 0,
                total: known,
                state,
                digest: [0; 32],
            };
            return self.page_result(env, page, String::new());
        }
        let consumed = page.next_offset.saturating_sub(page.start);
        let Ok(source_len) = u32::try_from(consumed) else {
            return self.outcome(env, O::Unavailable);
        };
        let digest = if source_len == 0 {
            [0; 32]
        } else {
            Sha256::digest(page.text.as_bytes()).into()
        };
        let text = if source_len == 0 {
            String::new()
        } else {
            page.text
        };
        let result = JobPage {
            stream,
            start: page.start,
            source_len,
            total: page.total,
            state,
            digest,
        };
        self.page_result(env, result, text);
    }

    fn page_result(&mut self, env: &mut Env, page: JobPage, text: String) {
        let result = self.kind_29(env, O::Ok, self.job, JobResultBody::Page(page));
        let shown = self.shown(None);
        self.render(env, result, shown.into_iter().collect(), Some(text));
    }

    fn list(&mut self, env: &mut Env, mut listings: Vec<journal::JobListing>) {
        let more = listings.len() > MAX_JOB_LIST_ENTRIES;
        listings.truncate(MAX_JOB_LIST_ENTRIES);
        let mut entries = Vec::with_capacity(listings.len());
        let mut shown = Vec::with_capacity(listings.len());
        for listing in listings {
            let settled_entry = |state: JobSnapshot| JobListEntry {
                job: listing.job,
                state: state.state,
                numbers: state.numbers,
                stdout_bytes: state.stdout_bytes,
                stderr_bytes: state.stderr_bytes,
            };
            // A settled entry stores zero bytes (its facts are in kind 28);
            // a live one is the controller's view.
            entries.push(match &listing.settled {
                Some(_) => settled_entry(settled_snapshot()),
                None => env
                    .jobs
                    .as_deref()
                    .and_then(|j| j.view(listing.job, env.now))
                    .filter(|v| !v.settled())
                    .map_or_else(|| settled_entry(finalizing()), |v| v.list_entry()),
            });
            shown.push(Shown {
                job: listing.job,
                origin: job_text::Origin::of(&listing.intent.origin, false),
                sequence: listing.intent_frame.sequence,
                command: listing.excerpt,
                whole_command: false,
                settled: listing.settled,
                private: listing.received_secret,
            });
        }
        let result = self.kind_29(env, O::Ok, [0; 16], JobResultBody::List { more, entries });
        self.render(env, result, shown, None);
    }

    /// A state block result (codes 10, 12, 14 and 15).
    fn finish(&mut self, env: &mut Env, ending: Ending, snapshot: JobSnapshot) {
        // State 9 needs the job's kind 28 facts to render (J3-E7).
        if snapshot.state == JobRunState::Settled
            && !self.known.as_ref().is_some_and(|k| k.settled.is_some())
            && !self.settled_from_controller(env)
        {
            self.refresh = Some((ending, snapshot));
            self.step = Step::Read(writer::Command::JobFacts { job: self.job });
            return;
        }
        let (outcome, job, body) = match ending {
            Ending::Delivery { cancelled } => (
                if cancelled { O::Cancelled } else { O::Ok },
                [0; 16],
                JobResultBody::State(snapshot),
            ),
            Ending::Wait(end) => (O::Ok, self.job, JobResultBody::Wait(snapshot, end)),
            Ending::State => (O::Ok, self.job, JobResultBody::State(snapshot)),
        };
        let result = self.kind_29(env, outcome, job, body);
        let shown: Vec<Shown> = self.shown(Some(env)).into_iter().collect();
        if !self.start_view(env, &result, &shown, snapshot) {
            self.render(env, result, shown, None);
        }
    }

    /// Plans a stage A view for a code 10, 12 or 14 `ok` result (CC2) and
    /// submits its first range; false when the result gets no view.
    fn start_view(
        &mut self,
        env: &mut Env,
        result: &JobResult,
        shown: &[Shown],
        snapshot: JobSnapshot,
    ) -> bool {
        if result.outcome != O::Ok
            || !matches!(
                self.code,
                ToolOperation::ExecCmd | ToolOperation::JobStatus | ToolOperation::JobWait
            )
            || self.ended
            || env.pages.is_none()
        {
            return false;
        }
        let Some(job) = shown.first() else {
            return false;
        };
        if job.private {
            return false;
        }
        let (shape, totals) = match job.settled.as_deref() {
            Some(facts) if snapshot.state == JobRunState::Settled => (
                Shape::settled(facts),
                [
                    facts.stdout.capture.stored_bytes,
                    facts.stderr.capture.stored_bytes,
                ],
            ),
            _ if snapshot.state == JobRunState::Settled || snapshot.state.before_spawn() => {
                return false;
            }
            _ => (
                Shape::Running,
                [snapshot.stdout_bytes, snapshot.stderr_bytes],
            ),
        };
        if totals == [0, 0] {
            return false;
        }
        let Ok(fixed) = self.render_text(env, result, shown, None, &[]) else {
            return false;
        };
        let Some(budget) = env.stage.view_budget(fixed.len() as u64) else {
            return false;
        };
        let ranges = view::plan(shape, totals, budget);
        if ranges.is_empty() {
            return false;
        }
        self.step = Step::View(Box::new(ViewRead {
            ranges,
            next: 0,
            ticket: None,
            read: Vec::new(),
            result: result.clone(),
            shown: shown.to_vec(),
        }));
        self.view_step(env);
        true
    }

    /// Advances the view reads: polls the read in flight, then submits the
    /// next range; renders once every range is read or a read stops.
    fn view_step(&mut self, env: &mut Env) {
        let ended = self.ended;
        let job = self.job;
        let Step::View(read) = &mut self.step else {
            return;
        };
        loop {
            if let Some((ticket, plan)) = &read.ticket {
                if ended {
                    // Dropping the ticket withdraws the read (WB1).
                    read.ticket = None;
                    break;
                }
                let Some(reply) = ticket.poll() else {
                    return;
                };
                let plan = *plan;
                read.ticket = None;
                match view_page(plan, reply) {
                    Some((page, text, go_on)) => {
                        read.read.push((page, text));
                        if !go_on {
                            break;
                        }
                    }
                    None => break,
                }
            }
            if ended || read.next >= read.ranges.len() {
                break;
            }
            let plan = read.ranges[read.next];
            read.next += 1;
            let Some(pages) = env.pages else {
                break;
            };
            let payload = payload::job_payloads(env.installation, job)[stream_index(plan.stream)];
            match pages.submit(
                PageRequest {
                    payload,
                    offset: plan.offset,
                    limit: plan.limit as usize,
                    scope: None,
                    lines: plan.lines,
                },
                payload::Origin::Model,
            ) {
                Ok(ticket) => read.ticket = Some((ticket, plan)),
                // The pool is busy or closed: later ranges are dropped.
                Err(_) => break,
            }
        }
        let Step::View(read) = std::mem::replace(&mut self.step, Step::Done) else {
            return;
        };
        let ViewRead {
            read,
            result,
            shown,
            ..
        } = *read;
        self.render_view(env, result, shown, read);
    }

    /// Renders a result with its view; drops the last range while the text
    /// exceeds the stage A cap, and records only the kept ranges.
    fn render_view(
        &mut self,
        env: &Env,
        mut result: JobResult,
        shown: Vec<Shown>,
        mut read: Vec<(JobPage, String)>,
    ) {
        let cap = env.stage.cap();
        while !read.is_empty() {
            let texts: Vec<&str> = read.iter().map(|(_, t)| t.as_str()).collect();
            result.view = read.iter().map(|(p, _)| *p).collect();
            match self.render_text(env, &result, &shown, None, &texts) {
                Ok(text) if text.len() as u64 <= cap => {
                    self.result = Some(result);
                    self.text = Some(text);
                    self.step = Step::Result;
                    return;
                }
                _ => {
                    read.pop();
                }
            }
        }
        result.view = Vec::new();
        self.render(env, result, shown, None);
    }

    /// A result without a body (codes 11-16).
    fn outcome(&mut self, env: &mut Env, outcome: O) {
        if self.code == ToolOperation::ExecCmd {
            // Code 10 records `ok` or `cancelled` with a state block.
            let snapshot = self.live_snapshot(env).unwrap_or_else(finalizing);
            let snapshot = if snapshot.state == JobRunState::Settled {
                finalizing()
            } else {
                snapshot
            };
            return self.finish(env, Ending::Delivery { cancelled: true }, snapshot);
        }
        let outcome = if outcome.allows(self.code) {
            outcome
        } else {
            O::Unavailable
        };
        let job = match outcome {
            O::NotFound => [0; 16],
            _ if self.code == ToolOperation::JobList => [0; 16],
            _ => self.job,
        };
        let result = self.kind_29(env, outcome, job, JobResultBody::None);
        let shown = self.shown(Some(env));
        self.render(env, result, shown.into_iter().collect(), None);
    }

    fn kind_29(&self, env: &Env, outcome: O, job: Id, body: JobResultBody) -> JobResult {
        JobResult {
            operation: env.operation,
            generation: env.generation,
            ordinal: self.ordinal,
            status: outcome.status(),
            outcome,
            job,
            view: Vec::new(),
            body,
        }
    }

    /// The rendered view of this call's job.
    fn shown(&self, env: Option<&Env>) -> Option<Shown> {
        if self.code == ToolOperation::ExecCmd {
            let sequence = self
                .intent
                .or_else(|| Self::intent_of(env.and_then(|e| e.jobs.as_deref()), self.job))
                .unwrap_or(0);
            return Some(Shown {
                job: self.job,
                origin: job_text::Origin::Model,
                sequence,
                command: self.command.clone(),
                whole_command: self.whole_command,
                settled: self.known.as_ref().and_then(|k| k.settled.clone()),
                private: false,
            });
        }
        let known = self.known.as_ref()?;
        Some(Shown {
            job: self.job,
            origin: job_text::Origin::of(&known.origin, false),
            sequence: known.sequence,
            command: known.command.clone(),
            whole_command: known.whole_command,
            settled: known.settled.clone(),
            private: known.taint.is_tainted(),
        })
    }

    /// Renders the text of `result`; a result that its records cannot
    /// render becomes `unavailable` (codes 11-16).
    /// The text of `result` for `shown`, or the renderer's error.
    fn render_text(
        &self,
        env: &Env,
        result: &JobResult,
        shown: &[Shown],
        page_text: Option<&str>,
        view_texts: &[&str],
    ) -> Result<String, job_text::RenderError> {
        let views: Vec<job_text::JobView> = shown
            .iter()
            .map(|s| job_text::JobView {
                job: s.job,
                origin: s.origin,
                intent_sequence: s.sequence,
                command: &s.command,
                whole_command: s.whole_command,
                settled: s.settled.as_deref(),
                cursors: env.cursors.of(&s.job),
                private: s.private,
            })
            .collect();
        let list = self.code == ToolOperation::JobList;
        job_text::render(&Render {
            code: self.code,
            result,
            job: if list { None } else { views.first().copied() },
            entries: if list { &views } else { &[] },
            page_text,
            view_texts,
        })
    }

    fn render(
        &mut self,
        env: &Env,
        result: JobResult,
        shown: Vec<Shown>,
        page_text: Option<String>,
    ) {
        let text = match self.render_text(env, &result, &shown, page_text.as_deref(), &[]) {
            Ok(text) => text,
            Err(error) => {
                tracing::warn!(?error, "job_result_unrendered");
                if self.code != ToolOperation::ExecCmd && result.outcome != O::Unavailable {
                    let fallback = self.kind_29(env, O::Unavailable, self.job, JobResultBody::None);
                    return self.render(env, fallback, Vec::new(), None);
                }
                UNRENDERED.to_owned()
            }
        };
        self.result = Some(result);
        self.text = Some(text);
        self.step = Step::Result;
    }
}

/// One view range's reply: the range in the page block form, its delivered
/// text, and whether later ranges may still be read. `None` stops the view
/// with no range for this reply (pool shutdown, `busy`, `denied`, a stale
/// offset, an empty page).
pub(super) fn view_page(plan: RangePlan, reply: PageReply) -> Option<(JobPage, String, bool)> {
    let PageReply::Page(page) = reply else {
        return None;
    };
    let state = match page.state {
        PageState::Live => JobPageState::Live,
        PageState::Complete => JobPageState::Complete,
        PageState::Incomplete => JobPageState::Incomplete,
        PageState::UnverifiedPrefix => JobPageState::UnverifiedPrefix,
        PageState::Expired => JobPageState::Expired,
        PageState::Missing => JobPageState::Missing,
        PageState::Corrupt => JobPageState::Corrupt,
        PageState::Deleted => JobPageState::Deleted,
        PageState::StaleCursor | PageState::Busy | PageState::Denied => return None,
    };
    if !state.readable() {
        // An unreadable state is recorded as a 0-byte range; later ranges
        // are dropped.
        let unreadable = JobPage {
            stream: plan.stream,
            start: plan.offset.min(page.total),
            source_len: 0,
            total: page.total,
            state,
            digest: [0; 32],
        };
        return Some((unreadable, String::new(), false));
    }
    let source_len = u32::try_from(page.next_offset.saturating_sub(page.start)).ok()?;
    if source_len == 0 {
        return None;
    }
    let digest: [u8; 32] = Sha256::digest(page.text.as_bytes()).into();
    let range = JobPage {
        stream: plan.stream,
        start: page.start,
        source_len,
        total: page.total.max(page.start + u64::from(source_len)),
        state,
        digest,
    };
    Some((range, page.text, true))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn delivery(job: Id) -> JobCall {
        let agent = AgentKey {
            conversation: [3; 16],
            agent: 0,
        };
        let mut call = JobCall::exec(1, job, agent, 0, "true".into(), false);
        call.result = Some(JobResult {
            operation: [1; 16],
            generation: 1,
            ordinal: 1,
            status: O::Ok.status(),
            outcome: O::Ok,
            job: [0; 16],
            view: Vec::new(),
            body: JobResultBody::State(settled_snapshot()),
        });
        call.text = Some("settled".into());
        call.step = Step::Result;
        call
    }

    /// JR13 (code 10): kind 29 waits for the job's kind 26; once seen
    /// committed, the call keeps that fact, so a controller that later
    /// forgets the settled job (64-entry retired list) cannot hold it.
    #[test]
    fn code_10_kind_29_waits_for_kind_26_and_keeps_it_after_eviction() {
        let mut call = delivery([7; 16]);
        assert!(call.step_action(None).is_none(), "no kind 26 yet: held");
        assert!(call.next_is_append());
        assert!(matches!(
            call.step_action(Some(40)),
            Some(Action::Result(journal::Record::JobResult(_)))
        ));
        // The journal refused the append at its quota; meanwhile the
        // controller forgot the job. The retry still appends.
        call.append_refused();
        assert!(matches!(
            call.step_action(None),
            Some(Action::Result(journal::Record::JobResult(_)))
        ));
        assert_eq!(call.shown(None).map(|s| s.sequence), Some(40));

        // Seen in an earlier step only, then evicted before the result.
        let mut call = delivery([8; 16]);
        call.step = Step::Parked;
        assert!(call.step_action(Some(41)).is_none());
        call.step = Step::Result;
        assert!(call.step_action(None).is_some());
    }
}
