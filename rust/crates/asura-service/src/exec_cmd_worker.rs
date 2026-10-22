//! One exec_cmd slot: the slot worker behind the job controller's
//! `SlotPort` (tool-job-controller.md, slot worker, capture worker and
//! controller interfaces). Filesystem preparation and cleanup never run on
//! the reactor. J3 retired the J1 call API and its kinds 23 and 15 (J3-E9).
use crate::{
    capture_worker::{self, Message, PauseReason, Report},
    completion::Completion,
    job_controller::{CaptureOutcome, SlotEvent, SlotFactory, SlotPort, SlotStart, SlotStartError},
    tools::{self, ExecutionError},
};
use asura_platform::{
    RuntimeDirectory,
    events::WakeSender,
    exec_cmd::{
        ExecCmdCleanup, ExecCmdError, ExecCmdFailure, ExecCmdJob, ExecCmdRequest, ExecCmdResult,
        Exit, PreparedExecCmd, StopKind, UnconfirmedReason,
    },
};
use asura_storage::authority::conversation::{
    self as journal, CaptureReason, CaptureState, JobCleanup, JobReason,
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
struct Cleaned {
    token: Option<ExecCmdCleanup>,
    result: Result<(), ExecCmdError>,
}
/// One exec_cmd slot: at most one job.
#[derive(Default)]
pub(crate) struct Worker {
    slot: Option<Box<Slot>>,
    wake: Option<WakeSender>,
}
impl Worker {
    pub(crate) fn register(&mut self, wake: WakeSender) {
        self.wake = Some(wake.clone());
        if let Some(slot) = &self.slot {
            match &slot.phase {
                SlotPhase::Setup(w) => w.completion.register(wake),
                SlotPhase::Cleanup(w) => w.completion.register(wake),
                _ => {}
            }
        }
    }
}

// The slot reports `SlotEvent`s and never makes the owner unavailable. Every
// method runs on the reactor and does bounded in-memory work plus at most
// one nonblocking lease byte; setup and scratch cleanup run on the slot's
// thread, staging on its capture worker.

/// Slot events returned by one poll (J2 numeric limits).
pub(crate) const SLOT_EVENTS: usize = 8;

/// A setup that ended without a process. `reason` is `None` when the
/// controller's own stop cancelled setup: the controller's first cancel
/// trigger is then the kind 28 reason.
struct SetupEnded {
    reason: Option<JobReason>,
    identity_changed: bool,
    cleanup: Option<ExecCmdCleanup>,
}
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
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SlotProcess {
    /// Setup or a live guardian: capture settlement waits.
    Live,
    /// No command ran: capture settles `not_started`.
    None,
    /// The settled notice arrived (`Finalizing`).
    Settled,
}
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
    /// until restart. Before it (for example an unknown profile token) the
    /// job settles `protocol_fault` with no process, and the slot keeps
    /// polling while the platform may release the scratch token (J2-A2):
    /// once the guardian is reaped with no notice and both capture pipes
    /// ended, the scratch is deleted and the slot released. A status byte,
    /// or a release that cannot happen, keeps token and slot until restart.
    fn lost(&mut self, mut job: ExecCmdJob, now: Instant, wake: Option<&WakeSender>) {
        job.close_lease();
        if self.spawned_at.is_some() {
            if !self.unconfirmed {
                self.unconfirmed = true;
                self.events.push_back(SlotEvent::CleanupUnconfirmed(
                    UnconfirmedReason::CleanupFailed,
                ));
            }
        } else {
            if self.process != SlotProcess::None {
                self.no_process(JobReason::ProtocolFault);
            }
            if let Some(token) = job.take_cleanup() {
                return self.cleanup(token, now, wake);
            }
            if job.release_pending() {
                self.phase = SlotPhase::Running(job);
                return;
            }
            self.scratch = Some(SlotEvent::ScratchRetained);
        }
        self.phase = SlotPhase::Held {
            _cleanup: None,
            _job: Some(job),
        };
    }
    fn running(&mut self, mut job: ExecCmdJob, now: Instant, wake: Option<&WakeSender>) {
        let faulted = job.poll(now).is_err() || job.faulted();
        if self.process == SlotProcess::None {
            // A fault before any notice already settled the job with no
            // process: only the token release remains. A notice read late
            // never adds events; it only keeps the token.
            return self.lost(job, now, wake);
        }
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
            return self.lost(job, now, wake);
        }
        if !job.settled() {
            self.phase = SlotPhase::Running(job);
            return;
        }
        let Some(result) = job.result() else {
            return self.lost(job, now, wake);
        };
        if result.failure.is_some() {
            // Kind 3: the guardian could not start the command.
            self.no_process(guardian_setup_reason(result.setup_reason));
        } else {
            // The platform admits kind 2 only with kind 5's exit facts.
            let Some(exit) = job.leader_exit().or_else(|| exit_of(&result)) else {
                return self.lost(job, now, wake);
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
        // TJ-Q10: clear the stored signal first, even when the event queue
        // is full; the controller reads the ledger after every slot polls.
        if let Some(capture) = &self.capture {
            capture.take_stored();
        }
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
/// No command ran: the `not_started` capture rules (no bytes, no digest,
/// reason 0, or 1 after a staging create fault) whatever the worker sealed.
/// A live worker removes its staging files and exits.
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

/// The kind 28 reason of a guardian setup failure, from the notice kind 3
/// reason in `ExecCmdResult::setup_reason` (J2-A2): 5 `spawn_failed`,
/// 6 `sandbox_unavailable`, 7 `denied` as `project_changed`, 8
/// `protocol_fault`; any other reason is `setup_failed`.
fn guardian_setup_reason(setup_reason: u8) -> JobReason {
    match setup_reason {
        5 => JobReason::SpawnFailed,
        6 => JobReason::SandboxUnavailable,
        7 => JobReason::ProjectChanged,
        8 => JobReason::ProtocolFault,
        _ => JobReason::SetupFailed,
    }
}
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
fn project_changed(scope: &tools::ExecutionScope) -> bool {
    asura_platform::ProjectIdentity::open(&scope.path, &scope.boundary)
        .map_or(true, |p| p.device != scope.device || p.inode != scope.inode)
}
/// Maps where setup stopped to the kind 28 `not_started` reason (codes
/// 12–18). Blocking (identity recheck): setup thread only.
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

impl Worker {
    /// `SlotPort::start`: the controller granted a slot and 1 MiB for this
    /// job. Starts the capture worker, then setup on the slot's setup
    /// thread. A capture thread that cannot start is `CaptureUnavailable`
    /// (the slot stays empty); a setup thread that cannot start settles the
    /// job through events (`setup_failed`).
    pub(crate) fn start_job(&mut self, start: SlotStart) -> Result<(), SlotStartError> {
        if self.slot.is_some() {
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
        if job == [0; 16] || scope.project == [0; 16] || capture.job != job {
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
        self.slot.is_none()
    }
}

/// The controller drives each job's slot through this port; every method
/// forwards to the methods above.
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
    #[cfg(feature = "test-support")]
    let executable = guardian_executable(&runtime).unwrap_or(executable);
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

/// Test-support guardian replacement (J2T-25): the first line of
/// `HOME/exec-cmd-guardian-executable` names the program the slot starts
/// in place of the guardian, to qualify a guardian that dies by a signal
/// before any notice. Read on the setup worker only.
#[cfg(feature = "test-support")]
fn guardian_executable(runtime: &RuntimeDirectory) -> Option<std::path::PathBuf> {
    let home = runtime.scratch_home_for_test()?;
    let text = std::fs::read_to_string(home.join("exec-cmd-guardian-executable")).ok()?;
    text.lines().next().map(std::path::PathBuf::from)
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
