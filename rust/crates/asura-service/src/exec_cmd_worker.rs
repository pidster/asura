//! One retained exec_cmd slot. Filesystem preparation/cleanup never run on the reactor.
//!
//! J1 phases: `Setup` ends on spawn, a setup fault or cancel (no deadline,
//! J1-Q4); `Running` ends when `ExecCmdJob` settles (no deadline, J1-Q3);
//! `Cleanup` runs scratch cleanup while the capture worker flushes;
//! `Finalizing` waits for the sealed capture report. After a cancel trigger
//! the existing L3 bound of 2.5 s also covers capture settlement (J1-Q1).
use crate::{
    capture_worker::{self, Message, PauseReason, Report},
    completion::Completion,
    job_controller::{CaptureOutcome, SlotEvent, SlotFactory, SlotPort, SlotStart, SlotStartError},
    tools::{self, Arguments, ExecutionError, Rejection},
};
use asura_platform::{
    RuntimeDirectory,
    events::WakeSender,
    exec_cmd::{
        ExecCmdCleanup, ExecCmdError, ExecCmdFailure, ExecCmdJob, ExecCmdRequest, ExecCmdResult,
        Exit, PreparedExecCmd, StopKind, StopReason, Tails, UnconfirmedReason, render_tail,
    },
    secrets::{SecretCommand, SecretVariable},
};
use asura_storage::authority::conversation::{
    self as journal, CaptureReason, CaptureSettled, CaptureState, CaptureStream, JobCleanup,
    JobReason, ToolResult,
};
use std::collections::VecDeque;
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

/// L3: cleanup until unconfirmed, from the cancel trigger.
const CLEANUP_BOUND: Duration = Duration::from_millis(2500);

struct ThreadWork<T> {
    thread: JoinHandle<T>,
    completion: Completion,
    cancel: Arc<AtomicBool>,
}
impl<T> ThreadWork<T> {
    fn next_deadline(&self, now: Instant) -> Option<Instant> {
        if self.thread.is_finished() {
            Some(now)
        } else {
            self.completion.settlement_deadline(now)
        }
    }
}
struct Failure {
    error: ExecCmdError,
    cleanup: Option<ExecCmdCleanup>,
}
impl From<ExecCmdFailure> for Failure {
    fn from(f: ExecCmdFailure) -> Self {
        Self {
            error: f.reason,
            cleanup: f.cleanup,
        }
    }
}
type SetupResult = Result<ExecCmdJob, Failure>;
struct Cleaned {
    token: Option<ExecCmdCleanup>,
    result: Result<(), ExecCmdError>,
}
enum Phase {
    Setup(ThreadWork<SetupResult>),
    Running(ExecCmdJob),
    Cleanup(ThreadWork<Cleaned>),
    Finalizing,
    Repair {
        _cleanup: Option<ExecCmdCleanup>,
        _job: Option<ExecCmdJob>,
    },
}
/// What the process did: a settled result, or a failure before or at setup.
enum Process {
    Settled(ExecCmdResult),
    Failed(ExecCmdError),
}

/// The live identity of one `run` exec_cmd call. Every value is derived from
/// the installation and the call, never journaled (TCR-O5, TCR-D23); replay
/// recomputes the same IDs with the same storage functions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ExecCmdRun {
    pub operation: journal::Id,
    pub generation: u64,
    pub ordinal: u32,
    /// The SL3 rewrite, or the SL7 fixed `eval` text: the command that runs.
    pub command: String,
    pub cwd: String,
    /// SL7: the line the model's result shows for a whole-command run.
    pub filtered: Option<String>,
    pub job: journal::Id,
    pub stdout_payload: journal::Id,
    pub stderr_payload: journal::Id,
}
impl ExecCmdRun {
    pub(crate) fn derive(installation: journal::Id, call: &tools::Call) -> Option<Self> {
        let Arguments::ExecCmd { command, cwd } = &call.arguments else {
            return None;
        };
        let job = journal::exec_cmd_job_id(call.operation, call.generation, call.ordinal);
        let [stdout_payload, stderr_payload] =
            asura_storage::payload::job_payloads(installation, job);
        Some(Self {
            operation: call.operation,
            generation: call.generation,
            ordinal: call.ordinal,
            command: command.clone(),
            cwd: cwd.clone(),
            filtered: None,
            job,
            stdout_payload,
            stderr_payload,
        })
    }
}

/// One exec_cmd call's dispatch: the committed `run` kind 14, the rewritten
/// call, the secret variables (moved into the request; spawn zeroizes them)
/// and the capture worker's start.
pub(crate) struct Dispatch {
    pub runtime: RuntimeDirectory,
    pub scope: tools::ExecutionScope,
    pub call: tools::Call,
    pub intent: ExecCmdRun,
    pub secrets: Vec<SecretVariable>,
    /// SL7: the whole-command variable's value; then `secrets` is empty.
    pub secret_command: Option<SecretCommand>,
    pub capture: capture_worker::Start,
}

/// A settled call: kind 23, then kind 15, and the capture worker that
/// outlives the call (installing, or abandoned after `unconfirmed`).
pub(crate) struct ExecCmdOutput {
    pub settled: CaptureSettled,
    pub result: ToolResult,
    pub capture: Option<capture_worker::Handle>,
}

struct Active {
    intent: ExecCmdRun,
    phase: Option<Phase>,
    /// Set at the cancel trigger: L3 for cleanup and capture settlement.
    cleanup_deadline: Option<Instant>,
    stop: Option<StopReason>,
    process: Option<Process>,
    repair: bool,
    capture: Option<capture_worker::Handle>,
    report: Option<Box<Report>>,
    /// The capture worker could not start: settle `not_started`.
    capture_unavailable: bool,
}
/// One exec_cmd slot: a J1-mode call (`active`) or, in job mode (J2), one
/// job (`slot`); never both.
#[derive(Default)]
pub(crate) struct Worker {
    active: Option<Active>,
    slot: Option<Box<Slot>>,
    wake: Option<WakeSender>,
}
impl Worker {
    pub(crate) fn register(&mut self, wake: WakeSender) {
        self.wake = Some(wake.clone());
        if let Some(active) = &self.active {
            match active.phase.as_ref() {
                Some(Phase::Setup(w)) => w.completion.register(wake.clone()),
                Some(Phase::Cleanup(w)) => w.completion.register(wake.clone()),
                _ => {}
            }
        }
        if let Some(slot) = &self.slot {
            match &slot.phase {
                SlotPhase::Setup(w) => w.completion.register(wake),
                SlotPhase::Cleanup(w) => w.completion.register(wake),
                _ => {}
            }
        }
    }
    pub(crate) fn wake(&self) -> Option<WakeSender> {
        self.wake.clone()
    }
    /// Caller has committed this call's `run` kind 14 and rechecked the
    /// execution grant. The call carries the rewrite; it is compared with the
    /// derived run identity, never the raw arguments (G8). There is no
    /// deadline (J1-Q3, J1-Q4).
    pub(crate) fn start(&mut self, dispatch: Dispatch) -> Result<(), ExecutionError> {
        if self.active.is_some() || self.slot.is_some() {
            return Err(ExecutionError::Rejected(Rejection::Busy));
        }
        let Dispatch {
            runtime,
            scope,
            call,
            intent,
            secrets,
            secret_command,
            capture,
        } = dispatch;
        let Arguments::ExecCmd {
            command: proposed,
            cwd: proposed_cwd,
            ..
        } = &call.arguments
        else {
            return Err(ExecutionError::Rejected(Rejection::Denied));
        };
        if intent.command != *proposed
            || intent.cwd != *proposed_cwd
            || intent.operation != call.operation
            || intent.generation != call.generation
            || intent.ordinal != call.ordinal
            || intent.job == [0; 16]
            || scope.project == [0; 16]
        {
            return Err(ExecutionError::Rejected(Rejection::IdentityConflict));
        }
        let mut capture = capture;
        capture.wake = self.wake.clone();
        let handle = capture_worker::start(capture).ok();
        let Some(handle) = handle else {
            // The capture thread could not start: `not_started`, no process.
            self.active = Some(Active {
                intent,
                phase: Some(Phase::Finalizing),
                cleanup_deadline: None,
                stop: None,
                process: Some(Process::Failed(ExecCmdError::Unavailable)),
                repair: false,
                capture: None,
                report: None,
                capture_unavailable: true,
            });
            return Ok(());
        };
        let request = ExecCmdRequest {
            job_id: intent.job,
            profile: asura_platform::exec_cmd::Profile::Model1,
            command: intent.command.clone(),
            cwd: Some(intent.cwd.clone()),
            secrets,
            secret_command,
        };
        let sink = handle.output_sink();
        let work = spawn("asura-exec-cmd-setup", self.wake.as_ref(), move |cancel| {
            setup(runtime, &scope, request, sink, cancel).map_err(|(_, failure)| failure)
        });
        let work = match work {
            Ok(work) => work,
            Err(error) => {
                handle.sink.close();
                handle.cancel();
                self.active = Some(Active {
                    intent,
                    phase: Some(Phase::Finalizing),
                    cleanup_deadline: None,
                    stop: None,
                    process: Some(Process::Failed(ExecCmdError::Unavailable)),
                    repair: false,
                    capture: Some(handle),
                    report: None,
                    capture_unavailable: false,
                });
                return match error {
                    ExecutionError::WorkerFailed => Ok(()),
                    other => Err(other),
                };
            }
        };
        self.active = Some(Active {
            intent,
            phase: Some(Phase::Setup(work)),
            cleanup_deadline: None,
            stop: None,
            process: None,
            repair: false,
            capture: Some(handle),
            report: None,
            capture_unavailable: false,
        });
        Ok(())
    }
    pub(crate) fn interests(&self) -> Vec<asura_platform::PollInterest> {
        match self.active.as_ref().and_then(|a| a.phase.as_ref()) {
            Some(Phase::Running(job)) => job.interests(),
            _ => vec![],
        }
    }
    pub(crate) fn is_settled(&self) -> bool {
        self.active.is_none()
    }
    pub(crate) fn settlement_expired(&self) -> bool {
        self.active.as_ref().is_some_and(|a| a.repair)
    }
    /// Whether the active call's capture is paused (J1-Q2).
    pub(crate) fn capture_paused(&self) -> bool {
        self.active
            .as_ref()
            .and_then(|a| a.capture.as_ref())
            .is_some_and(|c| c.paused)
    }
    /// A cancel trigger: turn cancel, steering, turn failure, journal
    /// unavailability or shutdown.
    pub(crate) fn cancel(&mut self, reason: StopReason) {
        if let Some(active) = &mut self.active {
            stop(active, reason, Instant::now());
        }
    }
    pub(crate) fn next_deadline(&self, now: Instant) -> Option<Instant> {
        let active = self.active.as_ref()?;
        let stage = match active.phase.as_ref()? {
            Phase::Setup(w) => w.next_deadline(now),
            Phase::Running(job) => job.next_deadline(),
            Phase::Cleanup(w) => w.next_deadline(now),
            // The capture worker's messages wake the reactor.
            Phase::Finalizing => active.report.is_some().then_some(now),
            Phase::Repair { .. } => None,
        };
        stage
            .into_iter()
            .chain(
                (!active.repair)
                    .then_some(active.cleanup_deadline)
                    .flatten(),
            )
            .min()
    }
    /// Emits once, only after actual ownership and scratch cleanup settle,
    /// and after the sealed capture report or, after a cancel trigger, the
    /// L3 bound (J1-Q1).
    pub(crate) fn poll(&mut self, now: Instant) -> Option<ExecCmdOutput> {
        let mut active = self.active.take()?;
        drain_capture(&mut active, now);
        let past_bound = active.cleanup_deadline.is_some_and(|d| now >= d);
        let phase = active.phase.take().expect("retained stage");
        if past_bound && !matches!(phase, Phase::Finalizing) {
            active.repair = true;
        }
        active.phase = Some(match phase {
            Phase::Setup(work) if work.thread.is_finished() => match work.thread.join() {
                Ok(Ok(mut job)) => {
                    if active.stop.is_some() {
                        job.stop(StopKind::Term);
                    }
                    Phase::Running(job)
                }
                Ok(Err(failure)) => {
                    let error = match active.stop {
                        Some(_) => ExecCmdError::Cancelled,
                        None => failure.error,
                    };
                    active.process = Some(Process::Failed(error));
                    // No process ran: the producer side ends now.
                    if let Some(capture) = &active.capture {
                        capture.sink.close();
                    }
                    if let Some(token) = failure.cleanup {
                        self.cleanup(&mut active, token, now)
                    } else if matches!(failure.error, ExecCmdError::CleanupUnconfirmed) {
                        active.repair = true;
                        Phase::Repair {
                            _cleanup: None,
                            _job: None,
                        }
                    } else {
                        Phase::Finalizing
                    }
                }
                Err(_) => {
                    active.repair = true;
                    Phase::Repair {
                        _cleanup: None,
                        _job: None,
                    }
                }
            },
            Phase::Running(mut job) => {
                if job.poll(now).is_err() {
                    if active.process.is_none() {
                        active.process = Some(Process::Failed(ExecCmdError::Protocol));
                    }
                    // The platform closed the lease on the fault.
                    job.close_lease();
                    active.cleanup_deadline.get_or_insert(now + CLEANUP_BOUND);
                    if let Some(capture) = &active.capture {
                        capture.cancel();
                    }
                }
                if job.settled() {
                    // A late settlement after cleanup_unconfirmed still
                    // passed the guardian's gate: it releases the scratch
                    // and slot, and its result says cleanup_confirmed=false
                    // (escapee-control.md, current exec_cmd path).
                    match job.result() {
                        Some(result) => {
                            if active.process.is_none() {
                                active.process = Some(Process::Settled(result));
                            }
                            match job.take_cleanup() {
                                Some(token) => self.cleanup(&mut active, token, now),
                                None => {
                                    active.repair = true;
                                    Phase::Repair {
                                        _cleanup: None,
                                        _job: Some(job),
                                    }
                                }
                            }
                        }
                        _ => {
                            active.repair = true;
                            Phase::Repair {
                                _cleanup: None,
                                _job: Some(job),
                            }
                        }
                    }
                } else {
                    Phase::Running(job)
                }
            }
            Phase::Cleanup(work) if work.thread.is_finished() => match work.thread.join() {
                Ok(cleaned) if cleaned.result.is_ok() => Phase::Finalizing,
                Ok(cleaned) => {
                    active.repair = true;
                    Phase::Repair {
                        _cleanup: cleaned.token,
                        _job: None,
                    }
                }
                Err(_) => {
                    active.repair = true;
                    Phase::Repair {
                        _cleanup: None,
                        _job: None,
                    }
                }
            },
            phase => phase,
        });
        if matches!(active.phase, Some(Phase::Finalizing)) {
            // Keep moving bytes that wait on the reactor; then end the producer.
            if let Some(capture) = &active.capture {
                capture.sink.flush();
                if !capture.sink.pending() {
                    capture.sink.close();
                }
            }
            drain_capture(&mut active, now);
            let unconfirmed = active.report.is_none()
                && !active.capture_unavailable
                && active.cleanup_deadline.is_some_and(|d| now >= d);
            if active.report.is_some() || unconfirmed || active.capture_unavailable {
                return Some(output(active));
            }
        }
        self.active = Some(active);
        None
    }
    fn cleanup(&self, active: &mut Active, token: ExecCmdCleanup, now: Instant) -> Phase {
        let deadline = *active.cleanup_deadline.get_or_insert(now + CLEANUP_BOUND);
        // A failed thread spawn must not drop the unique cleanup token.
        // The reactor locks this handoff only if spawn failed: no thread exists
        // then, so no producer can hold the lock or block the reactor.
        let shared = Arc::new(std::sync::Mutex::new(Some(token)));
        let handoff = shared.clone();
        match spawn(
            "asura-exec-cmd-cleanup",
            self.wake.as_ref(),
            move |cancel| {
                let mut token = handoff
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .take()
                    .expect("exclusive cleanup transfer");
                let result = token.run(deadline, cancel);
                Cleaned {
                    token: Some(token),
                    result,
                }
            },
        ) {
            Ok(work) => Phase::Cleanup(work),
            Err(_) => {
                active.repair = true;
                Phase::Repair {
                    _cleanup: shared.lock().unwrap_or_else(|p| p.into_inner()).take(),
                    _job: None,
                }
            }
        }
    }
}

// Job mode (J2): the slot worker behind the controller's `SlotPort`
// (tool-job-controller.md, slot worker, capture worker and controller
// interfaces). It reports `SlotEvent`s instead of building kinds 23 and 15,
// never sets the repair flag and never makes the owner unavailable. Every
// method runs on the reactor and does bounded in-memory work plus at most
// one nonblocking lease byte; setup and scratch cleanup run on the slot's
// thread, staging on its capture worker.

/// Slot events returned by one poll (J2 numeric limits).
#[cfg_attr(not(test), allow(dead_code))] // J2 job mode: no production caller until J3
pub(crate) const SLOT_EVENTS: usize = 8;

/// A setup that ended without a process. `reason` is `None` when the
/// controller's own stop cancelled setup: the controller's first cancel
/// trigger is then the kind 28 reason.
#[cfg_attr(not(test), allow(dead_code))] // J2 job mode: no production caller until J3
struct SetupEnded {
    reason: Option<JobReason>,
    identity_changed: bool,
    cleanup: Option<ExecCmdCleanup>,
}
#[cfg_attr(not(test), allow(dead_code))] // J2 job mode: no production caller until J3
enum SlotPhase {
    Setup(ThreadWork<Result<ExecCmdJob, SetupEnded>>),
    Running(ExecCmdJob),
    Cleanup(ThreadWork<Cleaned>),
    /// Scratch cleaned, or none was created: only capture may remain.
    Done,
    /// Held until restart (J2-E9): a retained scratch token, or a guardian
    /// that faulted or was lost, whose job no later notice can settle. No
    /// timer releases it.
    Held {
        _cleanup: Option<ExecCmdCleanup>,
        _job: Option<ExecCmdJob>,
    },
}
#[cfg_attr(not(test), allow(dead_code))] // J2 job mode: no production caller until J3
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SlotProcess {
    /// Setup or a live guardian: capture settlement waits.
    Live,
    /// No command ran: capture settles `not_started`.
    None,
    /// The settled notice arrived (`Finalizing`).
    Settled,
}
#[cfg_attr(not(test), allow(dead_code))] // J2 job mode: no production caller until J3
/// One job in a slot.
pub(crate) struct Slot {
    phase: SlotPhase,
    /// The strongest stop requested.
    stop: Option<StopKind>,
    /// L3 for capture settlement, from the first cancel trigger (J1-Q1).
    capture_bound: Option<Instant>,
    capture: Option<capture_worker::Handle>,
    report: Option<Box<Report>>,
    process: SlotProcess,
    spawned_at: Option<Instant>,
    leader_exited: bool,
    unconfirmed: bool,
    capture_settled: bool,
    scratch_cleaned: bool,
    /// `ScratchCleaned` or `ScratchRetained`, reported after
    /// `CaptureSettled` so the controller sees the port's event order.
    scratch: Option<SlotEvent>,
    events: VecDeque<SlotEvent>,
}
#[cfg_attr(not(test), allow(dead_code))] // J2 job mode: no production caller until J3
impl Slot {
    fn new(phase: SlotPhase, capture: Option<capture_worker::Handle>) -> Self {
        Self {
            phase,
            stop: None,
            capture_bound: None,
            capture,
            report: None,
            process: SlotProcess::Live,
            spawned_at: None,
            leader_exited: false,
            unconfirmed: false,
            capture_settled: false,
            scratch_cleaned: false,
            scratch: None,
            events: VecDeque::new(),
        }
    }
    fn trigger(&mut self, now: Instant) {
        self.capture_bound.get_or_insert(now + CLEANUP_BOUND);
    }
    /// Setup ended with no command: the producer side of capture ends now.
    fn no_process(&mut self, reason: JobReason) {
        self.ended(SlotEvent::SetupFailed(reason));
    }
    /// No command ran, reported by `event`: `SetupFailed`, or
    /// `IdentityChanged` for the setup recheck (`project_changed`).
    fn ended(&mut self, event: SlotEvent) {
        self.process = SlotProcess::None;
        self.events.push_back(event);
        if let Some(capture) = &self.capture {
            capture.sink.close();
        }
    }
    fn finished(&self) -> bool {
        self.events.is_empty()
            && self.scratch.is_none()
            && self.capture_settled
            && self.scratch_cleaned
            && matches!(self.phase, SlotPhase::Done)
    }
    fn drain_capture(&mut self) {
        if self.events.len() >= SLOT_EVENTS {
            return;
        }
        let Some(capture) = self.capture.as_mut() else {
            return;
        };
        for message in capture.drain() {
            match message {
                Message::Paused(reason) => self.events.push_back(SlotEvent::CapturePaused(reason)),
                Message::Resumed => self.events.push_back(SlotEvent::CaptureResumed),
                // A short in `Draining` never pauses the worker; it reaches
                // the controller as the pause event that, in `Draining`, is
                // the `quota` or `enospc` cancel trigger (TJ-S1).
                Message::Fault(CaptureReason::Quota) => self
                    .events
                    .push_back(SlotEvent::CapturePaused(PauseReason::Quota)),
                Message::Fault(CaptureReason::Enospc) => self
                    .events
                    .push_back(SlotEvent::CapturePaused(PauseReason::Enospc)),
                Message::Fault(reason) => {
                    tracing::warn!(reason = reason.as_str(), "job_capture_fault");
                    self.events.push_back(SlotEvent::CaptureFault(reason));
                }
                Message::Sealed(report) => self.report = Some(report),
                Message::Room
                | Message::Installed
                | Message::InstallPending
                | Message::Abandoned => {}
            }
        }
    }
    /// Scratch cleanup on the slot's cleanup thread with its own L3 bound
    /// from now; a failed thread spawn keeps the token and the slot.
    fn cleanup(&mut self, token: ExecCmdCleanup, now: Instant, wake: Option<&WakeSender>) {
        let deadline = now + CLEANUP_BOUND;
        let shared = Arc::new(std::sync::Mutex::new(Some(token)));
        let handoff = shared.clone();
        self.phase = match spawn("asura-job-cleanup", wake, move |cancel| {
            let mut token = handoff
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .take()
                .expect("exclusive cleanup transfer");
            let result = token.run(deadline, cancel);
            Cleaned {
                token: Some(token),
                result,
            }
        }) {
            Ok(work) => SlotPhase::Cleanup(work),
            Err(_) => {
                self.scratch = Some(SlotEvent::ScratchRetained);
                SlotPhase::Held {
                    // Spawn failed: no thread holds the lock.
                    _cleanup: shared.lock().unwrap_or_else(|p| p.into_inner()).take(),
                    _job: None,
                }
            }
        };
    }
    /// A protocol fault or a lost guardian: the lease is closed and no
    /// later notice is trusted. After kind 1 the job is `CleanupUnconfirmed`
    /// until restart; before it (for example an unknown profile token) no
    /// command ran, and the scratch token stays with the faulted job.
    fn lost(&mut self, mut job: ExecCmdJob) {
        job.close_lease();
        if self.spawned_at.is_some() {
            if !self.unconfirmed {
                self.unconfirmed = true;
                self.events.push_back(SlotEvent::CleanupUnconfirmed(
                    UnconfirmedReason::CleanupFailed,
                ));
            }
        } else {
            self.no_process(JobReason::ProtocolFault);
            self.scratch = Some(SlotEvent::ScratchRetained);
        }
        self.phase = SlotPhase::Held {
            _cleanup: None,
            _job: Some(job),
        };
    }
    fn running(&mut self, mut job: ExecCmdJob, now: Instant, wake: Option<&WakeSender>) {
        let faulted = job.poll(now).is_err() || job.faulted();
        if self.spawned_at.is_none() && job.spawned() {
            self.spawned_at = Some(now);
            self.events.push_back(SlotEvent::Spawned);
        }
        if !self.leader_exited
            && let Some(exit) = job.leader_exit()
        {
            self.leader_exited = true;
            self.events.push_back(SlotEvent::LeaderExited(exit));
            // Draining: capture never pauses again; a paused one retries now.
            if let Some(capture) = &self.capture {
                capture.leader_exited();
            }
        }
        if !self.unconfirmed
            && let Some(reason) = job.cleanup_unconfirmed()
        {
            self.unconfirmed = true;
            self.events.push_back(SlotEvent::CleanupUnconfirmed(reason));
        }
        if faulted {
            return self.lost(job);
        }
        if !job.settled() {
            self.phase = SlotPhase::Running(job);
            return;
        }
        let Some(result) = job.result() else {
            return self.lost(job);
        };
        if result.failure.is_some() {
            // Kind 3: the guardian could not start the command.
            self.no_process(guardian_setup_reason(result.failure));
        } else {
            // The platform admits kind 2 only with kind 5's exit facts.
            let Some(exit) = job.leader_exit().or_else(|| exit_of(&result)) else {
                return self.lost(job);
            };
            self.process = SlotProcess::Settled;
            self.events.push_back(SlotEvent::ProcessSettled {
                exit,
                cleanup: if self.unconfirmed {
                    JobCleanup::Late
                } else {
                    JobCleanup::Confirmed
                },
                escapees: result.escapees,
                elapsed_ms: self
                    .spawned_at
                    .map_or(0, |at| now.saturating_duration_since(at).as_millis() as u64),
                // Only `person-outside-1` (P3-T) settles with output held.
                output_held: false,
            });
        }
        match job.take_cleanup() {
            Some(token) => self.cleanup(token, now, wake),
            None => {
                self.scratch = Some(SlotEvent::ScratchRetained);
                self.phase = SlotPhase::Held {
                    _cleanup: None,
                    _job: Some(job),
                };
            }
        }
    }
    fn step(&mut self, now: Instant, wake: Option<&WakeSender>) {
        self.drain_capture();
        let phase = std::mem::replace(&mut self.phase, SlotPhase::Done);
        match phase {
            SlotPhase::Setup(work) if work.thread.is_finished() => match work.thread.join() {
                Ok(Ok(mut job)) => {
                    // A cancel before spawn: the late returned job is stopped now.
                    if let Some(kind) = self.stop {
                        job.stop(kind);
                    }
                    self.running(job, now, wake);
                }
                Ok(Err(ended)) => {
                    if ended.identity_changed {
                        // The setup recheck before spawn: `project_changed`.
                        self.ended(SlotEvent::IdentityChanged);
                    } else {
                        self.no_process(ended.reason.unwrap_or(JobReason::None));
                    }
                    match ended.cleanup {
                        Some(token) => self.cleanup(token, now, wake),
                        None => {
                            self.scratch_cleaned = true;
                            self.scratch = Some(SlotEvent::ScratchCleaned);
                        }
                    }
                }
                Err(_) => {
                    // A setup panic: whether scratch exists is unknown.
                    self.no_process(JobReason::SetupFailed);
                    self.scratch = Some(SlotEvent::ScratchRetained);
                    self.phase = SlotPhase::Held {
                        _cleanup: None,
                        _job: None,
                    };
                }
            },
            SlotPhase::Running(job) => self.running(job, now, wake),
            SlotPhase::Cleanup(work) if work.thread.is_finished() => match work.thread.join() {
                Ok(cleaned) if cleaned.result.is_ok() => {
                    self.scratch_cleaned = true;
                    self.scratch = Some(SlotEvent::ScratchCleaned);
                }
                Ok(cleaned) => {
                    // Failure or L3 passed: token and slot held until restart.
                    self.scratch = Some(SlotEvent::ScratchRetained);
                    self.phase = SlotPhase::Held {
                        _cleanup: cleaned.token,
                        _job: None,
                    };
                }
                Err(_) => {
                    self.scratch = Some(SlotEvent::ScratchRetained);
                    self.phase = SlotPhase::Held {
                        _cleanup: None,
                        _job: None,
                    };
                }
            },
            phase => self.phase = phase,
        }
        self.settle_capture(now);
        if self.capture_settled
            && let Some(event) = self.scratch.take()
        {
            self.events.push_back(event);
        }
    }
    /// `Finalizing` (TJ-S1): the sealed report, or, after a cancel trigger,
    /// the L3 bound with no report. With no process, capture is always
    /// `not_started`.
    fn settle_capture(&mut self, now: Instant) {
        if self.capture_settled || self.process == SlotProcess::Live {
            return;
        }
        if let Some(capture) = &self.capture {
            // Keep moving bytes that wait on the reactor; then end the producer.
            capture.sink.flush();
            if !capture.sink.pending() {
                capture.sink.close();
            }
        }
        self.drain_capture();
        let past_bound = self.capture_bound.is_some_and(|bound| now >= bound);
        let outcome = match (self.process, self.report.take()) {
            (SlotProcess::None, report) if report.is_some() || past_bound => {
                not_started_outcome(report.as_deref(), self.capture.take())
            }
            (SlotProcess::None, _) if self.capture.is_none() => not_started_outcome(None, None),
            (SlotProcess::Settled, Some(report)) => sealed_outcome(&report, self.capture.take()),
            (SlotProcess::Settled, None) if past_bound => match self.capture.take() {
                Some(handle) => unconfirmed_outcome(handle),
                None => not_started_outcome(None, None),
            },
            (_, report) => {
                self.report = report;
                return;
            }
        };
        self.capture_settled = true;
        self.events
            .push_back(SlotEvent::CaptureSettled(Box::new(outcome)));
    }
    fn next_deadline(&self, now: Instant) -> Option<Instant> {
        if !self.events.is_empty() {
            return Some(now);
        }
        let stage = match &self.phase {
            SlotPhase::Setup(w) => w.next_deadline(now),
            SlotPhase::Running(job) => job.next_deadline(),
            SlotPhase::Cleanup(w) => w.next_deadline(now),
            SlotPhase::Done | SlotPhase::Held { .. } => None,
        };
        let capture = (!self.capture_settled && self.process != SlotProcess::Live).then(|| {
            if self.report.is_some() || self.capture.is_none() {
                Some(now)
            } else {
                self.capture_bound
            }
        });
        stage.into_iter().chain(capture.flatten()).min()
    }
}

/// The kind 28 capture facts from the sealed report: `complete` or
/// `incomplete`, with digests and line counts of stored bytes.
#[cfg_attr(not(test), allow(dead_code))] // J2 job mode: no production caller until J3
fn sealed_outcome(report: &Report, capture: Option<capture_worker::Handle>) -> CaptureOutcome {
    CaptureOutcome {
        state: report.state,
        reason: report.reason,
        stdout: report.job_stream(0),
        stderr: report.job_stream(1),
        distinct_numbers: report.distinct_numbers,
        numbers: report.numbers.clone(),
        pause_count: report.pause_count,
        first_pause_ms: report.first_pause_ms,
        paused_ms: report.paused_ms,
        final_reservation: report.final_reservation,
        capture,
    }
}
/// J1-Q1 for jobs: no sealed report within L3 after a cancel trigger. The
/// worker is abandoned (its staging stays an unverified prefix); the
/// stream facts are the sink's raw counts, with no digest and no lines.
#[cfg_attr(not(test), allow(dead_code))] // J2 job mode: no production caller until J3
fn unconfirmed_outcome(capture: capture_worker::Handle) -> CaptureOutcome {
    capture.abandon();
    let [stdout, stderr] = capture_worker::unconfirmed_streams(&capture.sink)
        .map(|capture| journal::JobStream { capture, lines: 0 });
    CaptureOutcome {
        state: CaptureState::Unconfirmed,
        reason: CaptureReason::SettlementUnconfirmed,
        stdout,
        stderr,
        distinct_numbers: 0,
        numbers: Vec::new(),
        pause_count: 0,
        first_pause_ms: 0,
        paused_ms: 0,
        final_reservation: asura_storage::payload::RESERVATION_STEP,
        capture: Some(capture),
    }
}
/// No command ran: the kind 23 `not_started` rules (no bytes, no digest,
/// reason 0, or 1 after a staging create fault) whatever the worker sealed.
/// A live worker removes its staging files and exits.
#[cfg_attr(not(test), allow(dead_code))] // J2 job mode: no production caller until J3
fn not_started_outcome(
    report: Option<&Report>,
    capture: Option<capture_worker::Handle>,
) -> CaptureOutcome {
    if let Some(capture) = &capture {
        capture.discard();
    }
    let reason = match report {
        Some(report)
            if report.state == CaptureState::NotStarted
                && report.reason == CaptureReason::StorageFault =>
        {
            CaptureReason::StorageFault
        }
        _ => CaptureReason::None,
    };
    CaptureOutcome {
        state: CaptureState::NotStarted,
        reason,
        stdout: journal::JobStream::default(),
        stderr: journal::JobStream::default(),
        distinct_numbers: 0,
        numbers: Vec::new(),
        pause_count: 0,
        first_pause_ms: 0,
        paused_ms: 0,
        final_reservation: 0,
        capture,
    }
}

/// The kind 28 reason of a guardian setup failure (notice kind 3). The
/// platform result folds reasons 5 `spawn_failed` and 6
/// `sandbox_unavailable` into `Unavailable`; both report `spawn_failed`.
#[cfg_attr(not(test), allow(dead_code))] // J2 job mode: no production caller until J3
fn guardian_setup_reason(failure: Option<ExecCmdError>) -> JobReason {
    match failure {
        Some(ExecCmdError::Denied) => JobReason::ProjectChanged,
        Some(ExecCmdError::Protocol) => JobReason::ProtocolFault,
        Some(ExecCmdError::Unavailable | ExecCmdError::Io) => JobReason::SpawnFailed,
        _ => JobReason::SetupFailed,
    }
}
#[cfg_attr(not(test), allow(dead_code))] // J2 job mode: no production caller until J3
fn exit_of(result: &ExecCmdResult) -> Option<Exit> {
    match (result.exit_code, result.signal) {
        (Some(code), None) => u8::try_from(code).ok().map(Exit::Exited),
        (None, Some(signal)) => u8::try_from(signal)
            .ok()
            .filter(|s| (1..=127).contains(s))
            .map(Exit::Signalled),
        _ => None,
    }
}
/// Whether the project directory no longer matches the scope's identity.
/// Blocking: setup thread only.
#[cfg_attr(not(test), allow(dead_code))] // J2 job mode: no production caller until J3
fn project_changed(scope: &tools::ExecutionScope) -> bool {
    asura_platform::ProjectIdentity::open(&scope.path, &scope.boundary)
        .map_or(true, |p| p.device != scope.device || p.inode != scope.inode)
}
/// Maps where setup stopped to the kind 28 `not_started` reason (codes
/// 12–18). Blocking (identity recheck): setup thread only.
#[cfg_attr(not(test), allow(dead_code))] // J2 job mode: no production caller until J3
fn setup_ended(stage: Stage, failure: Failure, scope: &tools::ExecutionScope) -> SetupEnded {
    let (reason, identity_changed) = match (stage, failure.error) {
        (_, ExecCmdError::Cancelled) => (None, false),
        // The setup recheck before spawn (J2-E10).
        (Stage::Identity, _) => (Some(JobReason::ProjectChanged), true),
        (Stage::Prepare | Stage::Spawn, ExecCmdError::Denied) => {
            if project_changed(scope) {
                (Some(JobReason::ProjectChanged), true)
            } else {
                (Some(JobReason::CwdUnavailable), false)
            }
        }
        (Stage::Spawn, ExecCmdError::Io | ExecCmdError::Unavailable | ExecCmdError::Protocol) => {
            (Some(JobReason::SpawnFailed), false)
        }
        _ => (Some(JobReason::SetupFailed), false),
    };
    SetupEnded {
        reason,
        identity_changed,
        cleanup: failure.cleanup,
    }
}

#[cfg_attr(not(test), allow(dead_code))] // J2 job mode: no production caller until J3
impl Worker {
    /// `SlotPort::start`: the controller granted a slot and 1 MiB for this
    /// job. Starts the capture worker, then setup on the slot's setup
    /// thread. A capture thread that cannot start is `CaptureUnavailable`
    /// (the slot stays empty); a setup thread that cannot start settles the
    /// job through events (`setup_failed`).
    pub(crate) fn start_job(&mut self, start: SlotStart) -> Result<(), SlotStartError> {
        if self.active.is_some() || self.slot.is_some() {
            return Err(SlotStartError::Busy);
        }
        let SlotStart {
            job,
            profile,
            runtime,
            scope,
            command,
            cwd,
            secrets,
            secret_command,
            mut capture,
        } = start;
        if job == [0; 16] || scope.project == [0; 16] || capture.call != job {
            return Err(SlotStartError::Invalid);
        }
        capture.wake = self.wake.clone();
        let handle =
            capture_worker::start(capture).map_err(|_| SlotStartError::CaptureUnavailable)?;
        let request = ExecCmdRequest {
            job_id: job,
            profile,
            command,
            cwd: Some(cwd),
            secrets,
            secret_command,
        };
        let sink = handle.output_sink();
        let work = spawn("asura-job-setup", self.wake.as_ref(), move |cancel| {
            setup(runtime, &scope, request, sink, cancel)
                .map_err(|(stage, failure)| setup_ended(stage, failure, &scope))
        });
        let slot = match work {
            Ok(work) => Slot::new(SlotPhase::Setup(work), Some(handle)),
            Err(_) => {
                let mut slot = Slot::new(SlotPhase::Done, Some(handle));
                slot.no_process(JobReason::SetupFailed);
                slot.scratch_cleaned = true;
                slot.scratch = Some(SlotEvent::ScratchCleaned);
                slot
            }
        };
        self.slot = Some(Box::new(slot));
        Ok(())
    }
    /// `SlotPort::stop`: a cancel trigger. `Term` writes `T`, `Kill` writes
    /// `K` (at most one of each, nonblocking); before spawn it sets the
    /// setup cancel flag, and a late returned job is stopped at once. It
    /// starts the L3 bound for capture settlement. Idempotent.
    pub(crate) fn stop_job(&mut self, kind: StopKind) {
        let Some(slot) = self.slot.as_mut() else {
            return;
        };
        slot.trigger(Instant::now());
        slot.stop = Some(match (slot.stop, kind) {
            (Some(StopKind::Kill), _) | (_, StopKind::Kill) => StopKind::Kill,
            _ => StopKind::Term,
        });
        if let Some(capture) = &slot.capture {
            capture.cancel();
        }
        match &mut slot.phase {
            SlotPhase::Setup(work) => work.cancel.store(true, Ordering::Release),
            SlotPhase::Running(job)
            | SlotPhase::Held {
                _job: Some(job), ..
            } => job.stop(kind),
            _ => {}
        }
    }
    /// `SlotPort::discard_capture`: a cancel trigger in `Draining` (quota,
    /// ENOSPC, or a cancel after the leader exited). Capture discards and
    /// counts what it has not staged; the L3 bound starts.
    pub(crate) fn discard_capture(&mut self) {
        let Some(slot) = self.slot.as_mut() else {
            return;
        };
        slot.trigger(Instant::now());
        if let Some(capture) = &slot.capture {
            capture.discard_output();
        }
    }
    /// `SlotPort::interests`: the running job's pipe and status reads.
    pub(crate) fn job_interests(&self, out: &mut Vec<asura_platform::PollInterest>) {
        if let Some(Slot {
            phase: SlotPhase::Running(job),
            ..
        }) = self.slot.as_deref()
        {
            out.extend(job.interests());
        }
    }
    /// `SlotPort::next_deadline`: thread completion, the 100 ms job tick,
    /// a deliverable capture report or the L3 bound after a cancel trigger.
    pub(crate) fn job_next_deadline(&self, now: Instant) -> Option<Instant> {
        self.slot.as_ref()?.next_deadline(now)
    }
    /// `SlotPort::poll`: advances the job and appends at most
    /// [`SLOT_EVENTS`] events; the rest wait for the next poll. The slot
    /// empties after its capture settled and its scratch was cleaned.
    pub(crate) fn poll_job(&mut self, now: Instant, out: &mut Vec<SlotEvent>) {
        let wake = self.wake.clone();
        let Some(slot) = self.slot.as_mut() else {
            return;
        };
        slot.step(now, wake.as_ref());
        for _ in 0..SLOT_EVENTS {
            let Some(event) = slot.events.pop_front() else {
                break;
            };
            out.push(event);
        }
        if slot.finished() {
            self.slot = None;
        }
    }
    /// `SlotPort::idle`: no call and no job holds this slot.
    pub(crate) fn idle(&self) -> bool {
        self.active.is_none() && self.slot.is_none()
    }
}

/// The controller drives each job's slot through this port; every method
/// forwards to the job mode above (the J1 call API keeps its own names).
impl SlotPort for Worker {
    fn start(&mut self, start: SlotStart) -> Result<(), SlotStartError> {
        self.start_job(start)
    }
    fn stop(&mut self, kind: StopKind) {
        self.stop_job(kind);
    }
    fn discard_capture(&mut self) {
        Worker::discard_capture(self);
    }
    /// P3-T: closes the read ends after a `person-outside-1` kind 2 reason
    /// 15. No J2 profile settles with output held, so nothing calls it yet.
    fn close_capture(&mut self) {}
    fn interests(&self, out: &mut Vec<asura_platform::PollInterest>) {
        self.job_interests(out);
    }
    fn next_deadline(&self, now: Instant) -> Option<Instant> {
        self.job_next_deadline(now)
    }
    fn poll(&mut self, now: Instant, out: &mut Vec<SlotEvent>) {
        self.poll_job(now, out);
    }
    fn idle(&self) -> bool {
        Worker::idle(self)
    }
}

/// The controller's slot factory: each granted job gets a new slot worker
/// registered with the reactor's wake, so setup, cleanup and capture
/// completions wake the reactor.
#[cfg_attr(not(test), allow(dead_code))] // J2-E's owner wiring calls it
pub(crate) fn slot_factory(wake: Option<WakeSender>) -> SlotFactory {
    Box::new(move || {
        let mut worker = Worker::default();
        if let Some(wake) = &wake {
            worker.register(wake.clone());
        }
        Box::new(worker)
    })
}

/// Where setup stopped: the job mode maps it to a kind 28 reason.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Stage {
    Executable,
    Identity,
    Prepare,
    Spawn,
}
/// Blocking slot setup on the setup thread: the project identity recheck,
/// descriptor validation and scratch creation, then the guardian spawn.
/// Setup checks only cancellation (J1-Q4); it has no deadline.
fn setup(
    runtime: RuntimeDirectory,
    scope: &tools::ExecutionScope,
    request: ExecCmdRequest,
    sink: Box<dyn asura_platform::exec_cmd::OutputSink>,
    cancel: &AtomicBool,
) -> Result<ExecCmdJob, (Stage, Failure)> {
    let failed = |stage, error| {
        (
            stage,
            Failure {
                error,
                cleanup: None,
            },
        )
    };
    if cancel.load(Ordering::Acquire) {
        return Err(failed(Stage::Executable, ExecCmdError::Cancelled));
    }
    let executable = std::env::current_exe()
        .map_err(|_| failed(Stage::Executable, ExecCmdError::Unavailable))?;
    let project = asura_platform::ProjectIdentity::open(&scope.path, &scope.boundary)
        .map_err(|_| failed(Stage::Identity, ExecCmdError::Denied))?;
    if project.device != scope.device || project.inode != scope.inode {
        return Err(failed(Stage::Identity, ExecCmdError::Denied));
    }
    #[cfg(feature = "test-support")]
    setup_gate(&runtime, cancel);
    #[cfg(feature = "test-support")]
    let fault = guardian_fault(&runtime);
    #[allow(unused_mut)]
    let mut prepared = PreparedExecCmd::prepare(runtime, &project, request, cancel)
        .map_err(|f| (Stage::Prepare, Failure::from(f)))?;
    #[cfg(feature = "test-support")]
    if let Some(fault) = fault {
        prepared.guardian_fault_for_test(&fault);
    }
    prepared
        .spawn(&executable, cancel, sink)
        .map_err(|f| (Stage::Spawn, Failure::from(f)))
}

/// Test-support setup gate (J1T-06): holds setup while `HOME/exec-cmd-hold-setup`
/// exists; only cancellation releases it early.
#[cfg(feature = "test-support")]
fn setup_gate(runtime: &RuntimeDirectory, cancel: &AtomicBool) {
    if let Some(home) = runtime.scratch_home_for_test() {
        while home.join("exec-cmd-hold-setup").exists() && !cancel.load(Ordering::Acquire) {
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

/// Test-support guardian fault (escapee-control.md): the first line of
/// `HOME/exec-cmd-guardian-fault`, read on the setup worker only.
#[cfg(feature = "test-support")]
fn guardian_fault(runtime: &RuntimeDirectory) -> Option<String> {
    let home = runtime.scratch_home_for_test()?;
    let text = std::fs::read_to_string(home.join("exec-cmd-guardian-fault")).ok()?;
    text.lines().next().map(str::to_owned)
}

fn drain_capture(active: &mut Active, now: Instant) {
    let Some(capture) = active.capture.as_mut() else {
        return;
    };
    let mut fault = false;
    for message in capture.drain() {
        match message {
            Message::Sealed(report) => active.report = Some(report),
            Message::Fault(reason) => {
                tracing::warn!(reason = reason.as_str(), "exec_cmd_capture_fault");
                fault = true;
            }
            Message::Paused(reason) => {
                tracing::info!(reason = ?reason, "exec_cmd_capture_paused");
            }
            _ => {}
        }
    }
    if fault {
        // A capture fault is a cancel trigger: stop the job, keep the prefix.
        stop(active, StopReason::Cancelled, now);
    }
}

fn stop(active: &mut Active, reason: StopReason, now: Instant) {
    if active.stop.is_none() {
        active.stop = Some(reason);
        active.cleanup_deadline.get_or_insert(now + CLEANUP_BOUND);
    }
    if let Some(capture) = &active.capture {
        capture.cancel();
    }
    match active.phase.as_mut() {
        Some(Phase::Setup(work)) => work.cancel.store(true, Ordering::Release),
        Some(Phase::Running(job)) => job.stop(StopKind::Term),
        _ => {}
    }
}

fn exit_facts(process: Option<&Process>) -> (u8, u32) {
    match process {
        Some(Process::Settled(result)) if result.failure.is_none() => {
            match (result.exit_code, result.signal) {
                (Some(code), None) if (0..=255).contains(&code) => (1, code as u32),
                (None, Some(signal)) if (1..=127).contains(&signal) => (2, signal as u32),
                _ => (0, 0),
            }
        }
        _ => (0, 0),
    }
}

/// Builds kind 23 and kind 15 for a settled call.
fn output(mut active: Active) -> ExecCmdOutput {
    let (exit_kind, exit_value) = exit_facts(active.process.as_ref());
    let intent = &active.intent;
    let report = active.report.take();
    let (state, reason, streams, distinct_numbers, numbers, pauses, final_reservation) =
        match &report {
            Some(report) => (
                report.state,
                report.reason,
                [report.stream(0), report.stream(1)],
                report.distinct_numbers,
                report.numbers.clone(),
                (report.pause_count, report.first_pause_ms, report.paused_ms),
                report.final_reservation,
            ),
            None if active.capture_unavailable => (
                CaptureState::NotStarted,
                CaptureReason::None,
                [CaptureStream::default(), CaptureStream::default()],
                0,
                Vec::new(),
                (0, 0, 0),
                0,
            ),
            None => {
                // J1-Q1: no sealed report within L3 after the cancel trigger.
                let streams = active.capture.as_ref().map_or_else(Default::default, |c| {
                    capture_worker::unconfirmed_streams(&c.sink)
                });
                (
                    CaptureState::Unconfirmed,
                    CaptureReason::SettlementUnconfirmed,
                    streams,
                    0,
                    Vec::new(),
                    (0, 0, 0),
                    asura_storage::payload::RESERVATION_STEP,
                )
            }
        };
    let not_started = state == CaptureState::NotStarted;
    let [stdout, stderr] = streams;
    let settled = CaptureSettled {
        operation: intent.operation,
        generation: intent.generation,
        ordinal: intent.ordinal,
        state,
        reason,
        exit_kind: if not_started { 0 } else { exit_kind },
        exit_value: if not_started { 0 } else { exit_value },
        stdout,
        stderr,
        distinct_numbers,
        numbers,
        pause_count: pauses.0,
        first_pause_ms: pauses.1,
        paused_ms: pauses.2,
        final_reservation,
    };
    // Kind 15 tails come from the redacted stream only; `unconfirmed` has none.
    let tails = report.as_ref().map_or_else(Tails::default, |report| {
        let render = |i: usize| {
            let s = &report.streams[i];
            let (text, cut) = render_tail(&s.tail);
            (text, cut || s.stored > s.tail.len() as u64)
        };
        let (stdout, a) = render(0);
        let (stderr, b) = render(1);
        Tails {
            stdout,
            stderr,
            truncated: a || b,
        }
    });
    let result = match active.process.as_ref() {
        Some(Process::Settled(result)) => mapped_result(intent, result, &tails, state),
        Some(Process::Failed(error)) => failed(intent, *error),
        None => failed(intent, ExecCmdError::Protocol),
    };
    let capture = active.capture.take();
    if state == CaptureState::Unconfirmed
        && let Some(capture) = &capture
    {
        capture.abandon();
    }
    ExecCmdOutput {
        settled,
        result,
        capture,
    }
}

fn mapped_result(
    intent: &ExecCmdRun,
    result: &ExecCmdResult,
    tails: &Tails,
    capture: CaptureState,
) -> ToolResult {
    // A settled setup failure proves cleanup, not successful command execution.
    if let Some(error) = result.failure {
        return failed(intent, error);
    }
    if (result.exit_code.is_some() && result.signal.is_some())
        || (result.exit_code.is_none() && result.signal.is_none() && result.reason.is_none())
    {
        return failed(intent, ExecCmdError::Protocol);
    }
    let status = match result.reason {
        Some(StopReason::Cancelled | StopReason::Killed | StopReason::LeaseEnded) => 6,
        None => 1,
    };
    let text = with_filtered(intent, result.render_text(tails, capture.as_str()));
    if text.len() > 16_384 {
        return failed(intent, ExecCmdError::Protocol);
    }
    ToolResult {
        operation: intent.operation,
        generation: intent.generation,
        ordinal: intent.ordinal,
        status,
        text,
        next_offset: None,
        truncated: tails.truncated,
    }
}

pub(crate) fn failed(intent: &ExecCmdRun, error: ExecCmdError) -> ToolResult {
    let (status, text) = match error {
        ExecCmdError::Invalid => (3, "exec_cmd_invalid_arguments"),
        ExecCmdError::Denied => (2, "exec_cmd_denied"),
        ExecCmdError::Cancelled => (6, "exec_cmd_cancelled; effects may have occurred"),
        ExecCmdError::Protocol | ExecCmdError::CleanupUnconfirmed => {
            (4, "exec_cmd_outcome_unconfirmed; effects may have occurred")
        }
        ExecCmdError::Unavailable | ExecCmdError::Io => (4, "exec_cmd_unavailable"),
    };
    ToolResult {
        operation: intent.operation,
        generation: intent.generation,
        ordinal: intent.ordinal,
        status,
        text: with_filtered(intent, text.into()),
        next_offset: None,
        truncated: false,
    }
}
/// Display bytes the model's result shows for a whole-command run (SL7).
/// With two 6,144-byte tails and the status line, a result stays below its
/// 16,384-byte limit.
const FILTERED_DISPLAY_BYTES: usize = 2048;
/// The SL7 line beside the display text: `command filtered: run from a
/// secret variable: <display>`. The display is cut at 2,048 bytes, never
/// inside a placeholder, with `…` when cut; control characters become
/// spaces so the line stays one line. It holds placeholders, never a value.
pub(crate) fn filtered_line(display: &str) -> String {
    let mut cut = display.len().min(FILTERED_DISPLAY_BYTES);
    while !display.is_char_boundary(cut) {
        cut -= 1;
    }
    // Never inside `$ASURA_SECRET_<n>`: a cut inside one backs up to its
    // `$`, the last `$` before the cut.
    if cut < display.len()
        && let Some(at) = display[..cut].rfind('$')
        && let Some(digits) = display[at..].strip_prefix("$ASURA_SECRET_")
    {
        let end =
            at + "$ASURA_SECRET_".len() + digits.bytes().take_while(u8::is_ascii_digit).count();
        if end > cut {
            cut = at;
        }
    }
    let mut line = format!("{}: ", asura_platform::secrets::WHOLE_COMMAND_NOTICE);
    line.extend(
        display[..cut]
            .chars()
            .map(|c| if c.is_control() { ' ' } else { c }),
    );
    if cut < display.len() {
        line.push('…');
    }
    line
}
/// Inserts the SL7 line after the result's first (status) line, which
/// replay reads the exit code from.
fn with_filtered(intent: &ExecCmdRun, text: String) -> String {
    let Some(line) = &intent.filtered else {
        return text;
    };
    match text.split_once('\n') {
        Some((first, rest)) => format!("{first}\n{line}\n{rest}"),
        None => format!("{text}\n{line}"),
    }
}
fn spawn<T: Send + 'static>(
    name: &str,
    wake: Option<&WakeSender>,
    run: impl FnOnce(&AtomicBool) -> T + Send + 'static,
) -> Result<ThreadWork<T>, ExecutionError> {
    let cancel = Arc::new(AtomicBool::new(false));
    let token = cancel.clone();
    let completion = Completion::default();
    if let Some(wake) = wake {
        completion.register(wake.clone());
    }
    let signal = completion.clone();
    let thread = std::thread::Builder::new()
        .name(name.into())
        .spawn(move || {
            let _completion = signal.guard();
            run(&token)
        })
        .map_err(|_| ExecutionError::WorkerFailed)?;
    Ok(ThreadWork {
        thread,
        completion,
        cancel,
    })
}

#[cfg(test)]
mod tests;
