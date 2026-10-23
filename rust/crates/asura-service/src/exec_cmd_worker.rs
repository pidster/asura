//! One exec_cmd slot: the slot worker behind the job controller's
//! `SlotPort` (tool-job-controller.md, slot worker, capture worker and
//! controller interfaces). Filesystem preparation and cleanup never run on
//! the reactor. J3 retired the J1 call API and its kinds 23 and 15 (J3-E9).
//!
//! Person jobs (person-command-mode.md, P3-T): the slot resolves the
//! person setup ([`PersonLaunch`]) on its setup thread, and the platform
//! spawn writes the FD 11 frame there. A `!!` job's kind 2 reason 15
//! reports `ProcessSettled { output_held: true }` while capture continues;
//! `close_capture` then closes the read ends and capture seals with reason
//! 7 (OD2).
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
        Exit, PersonSetup, PreparedExecCmd, Profile, StopKind, UnconfirmedReason,
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

/// The person setup inputs of one person job (person-command-mode.md,
/// environment sections), which the conversation owner moves into the job
/// admission at dispatch: from kind 31 (the sanitized PATH) and the input's
/// secret store item (client variables, launch environment). The slot
/// resolves them on its setup thread with [`PersonSetup`]; the values reach
/// the guardian only in the FD 11 frame. No `Debug`, and every byte is
/// zeroed on drop: a launch environment commonly holds tokens.
#[cfg_attr(not(test), allow(dead_code))] // P3-E builds it at dispatch
pub(crate) enum PersonLaunch {
    /// `person-1` (`!`): the service-sanitized PATH (PC2) and the PC3
    /// client variables (locale names and `SSH_AUTH_SOCK`). HOME, USER,
    /// LOGNAME, `$TMPDIR` and the root descriptors are resolved by the
    /// setup thread, never taken from the client.
    Sandboxed {
        path: Vec<u8>,
        client: Vec<(Vec<u8>, Vec<u8>)>,
    },
    /// `person-outside-1` (`!!`): the TUI launch environment, form-checked
    /// only (PCR21, OD4).
    Outside { launch: Vec<(Vec<u8>, Vec<u8>)> },
}
impl PersonLaunch {
    /// The guardian profile these inputs belong to.
    pub(crate) fn profile(&self) -> Profile {
        match self {
            Self::Sandboxed { .. } => Profile::Person1,
            Self::Outside { .. } => Profile::PersonOutside1,
        }
    }
}
impl Drop for PersonLaunch {
    fn drop(&mut self) {
        use zeroize::Zeroize;
        let (path, entries) = match self {
            Self::Sandboxed { path, client } => (Some(path), client),
            Self::Outside { launch } => (None, launch),
        };
        if let Some(path) = path {
            path.zeroize();
        }
        for (name, value) in entries.iter_mut() {
            name.zeroize();
            value.zeroize();
        }
    }
}

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
    /// A `!!` guardian settled with kind 2 reason 15 (OD2) and
    /// `ProcessSettled { output_held: true }` was reported: survivors
    /// hold the capture pipes, so capture continues until both reach EOF
    /// or `close_capture` closes the read ends.
    Released,
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
    /// `SlotPort::close_capture` was called; applied once the guardian's
    /// reason 15 has released the job.
    close_requested: bool,
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
            close_requested: false,
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
        if self.process == SlotProcess::Live
            && let Some(settled) = job.guardian_settled()
            && settled.output_held
        {
            // OD2: kind 6 and kind 2 reason 15 of a `!!` guardian. It has
            // exited; survivors hold the capture pipes, so capture
            // continues and the slot stays held. The controller releases
            // the lane on this event.
            self.process = SlotProcess::Released;
            self.events.push_back(SlotEvent::ProcessSettled {
                exit: settled.exit,
                cleanup: self.cleanup_fact(),
                escapees: settled.escapees,
                elapsed_ms: self.elapsed_ms(now),
                output_held: true,
            });
        }
        if self.process == SlotProcess::Released && self.close_requested && !job.capture_closed() {
            self.close(&mut job);
        }
        if !job.settled() {
            self.phase = SlotPhase::Running(job);
            return;
        }
        let Some(result) = job.result() else {
            return self.lost(job, now, wake);
        };
        if self.process == SlotProcess::Released {
            // Both pipes reached EOF, or the read ends were closed: the
            // process facts were reported at reason 15.
            self.process = SlotProcess::Settled;
        } else if result.failure.is_some() {
            // Kind 3: the guardian could not start the command.
            self.no_process(guardian_setup_reason(result.setup_reason));
        } else {
            // The platform admits kind 2 only with kind 5's exit facts.
            let Some(exit) = job.leader_exit().or_else(|| exit_of(&result)) else {
                return self.lost(job, now, wake);
            };
            // Kind 2 reasons 0, 2, 4, 11, 12 and 14: every pipe is at
            // EOF. Reason 14 (`!!` survivors that hold no pipe) carries
            // the survivor count in `escapees`.
            self.process = SlotProcess::Settled;
            self.events.push_back(SlotEvent::ProcessSettled {
                exit,
                cleanup: self.cleanup_fact(),
                escapees: result.escapees,
                elapsed_ms: self.elapsed_ms(now),
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
    fn cleanup_fact(&self) -> JobCleanup {
        if self.unconfirmed {
            JobCleanup::Late
        } else {
            JobCleanup::Confirmed
        }
    }
    fn elapsed_ms(&self, now: Instant) -> u64 {
        self.spawned_at
            .map_or(0, |at| now.saturating_duration_since(at).as_millis() as u64)
    }
    /// OD2: closes a released job's capture read ends. The platform
    /// allows it only after the guardian's settled notice; the capture
    /// worker then seals `incomplete` with reason 7 once the reactor has
    /// moved the bytes already read. Survivors are never signalled; the
    /// next write of one gets EPIPE.
    fn close(&mut self, job: &mut ExecCmdJob) {
        if job.close_capture()
            && let Some(capture) = &self.capture
        {
            capture.close_output();
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
        if self.capture_settled || matches!(self.process, SlotProcess::Live | SlotProcess::Released)
        {
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
        let capture = (!self.capture_settled
            && !matches!(self.process, SlotProcess::Live | SlotProcess::Released))
        .then(|| {
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
        // A missing home or `$TMPDIR` (person Seatbelt profile, roles 1
        // and 2). An input the submit check should have refused is
        // `setup_failed` below.
        (Stage::Person, ExecCmdError::Unavailable) => (Some(JobReason::SandboxUnavailable), false),
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
    /// `SlotPort::start` for a `model-1` job: [`Self::start_job_with`]
    /// with no person setup. A person profile is `Invalid` here, because
    /// it needs its [`PersonLaunch`].
    pub(crate) fn start_job(&mut self, start: SlotStart) -> Result<(), SlotStartError> {
        self.start_job_with(start, None)
    }
    /// `SlotPort::start`: the controller granted a slot and 1 MiB for this
    /// job. Starts the capture worker, then setup on the slot's setup
    /// thread. A capture thread that cannot start is `CaptureUnavailable`
    /// (the slot stays empty); a setup thread that cannot start settles the
    /// job through events (`setup_failed`). `person` is required for, and
    /// must match, a person profile, and absent for `model-1`; otherwise
    /// the start is `Invalid` before any thread starts.
    pub(crate) fn start_job_with(
        &mut self,
        start: SlotStart,
        person: Option<PersonLaunch>,
    ) -> Result<(), SlotStartError> {
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
        let person_matches = match &person {
            None => !profile.person(),
            Some(launch) => launch.profile() == profile,
        };
        if job == [0; 16] || scope.project == [0; 16] || capture.job != job || !person_matches {
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
            setup(runtime, &scope, request, person, sink, cancel)
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
    /// `SlotPort::close_capture`: a trigger on a released `!!` job (after
    /// kind 2 reason 15, OD2). It closes both capture read ends without
    /// waiting for EOF, writes no lease byte and signals nothing; capture
    /// seals the stored bytes `incomplete` with reason 7, and a later
    /// `CaptureSettled` reports it. Like any cancel trigger it starts the
    /// L3 bound for capture settlement. A call before the reason 15 event
    /// is applied when that event arrives; after the pipes reached EOF it
    /// changes nothing. Idempotent; never blocks.
    pub(crate) fn close_capture(&mut self) {
        let Some(slot) = self.slot.as_mut() else {
            return;
        };
        slot.trigger(Instant::now());
        slot.close_requested = true;
        if slot.process != SlotProcess::Released {
            return;
        }
        let mut phase = std::mem::replace(&mut slot.phase, SlotPhase::Done);
        if let SlotPhase::Running(job) = &mut phase {
            slot.close(job);
        }
        slot.phase = phase;
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
    fn close_capture(&mut self) {
        Worker::close_capture(self);
    }
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
    /// The person setup: account, `$TMPDIR` and root descriptors, or the
    /// launch environment's form check.
    Person,
    Prepare,
    Spawn,
}
/// Blocking slot setup on the setup thread: the project identity recheck,
/// the person setup (account database, `confstr`, `readlink` and root
/// descriptors for `person-1`; the form check for `person-outside-1`),
/// descriptor validation and scratch creation, then the guardian spawn,
/// after which this thread writes the FD 11 frame of a person job.
/// Setup checks only cancellation (J1-Q4); it has no deadline.
fn setup(
    runtime: RuntimeDirectory,
    scope: &tools::ExecutionScope,
    request: ExecCmdRequest,
    person: Option<PersonLaunch>,
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
    let person = match person {
        Some(launch) => Some(
            person_setup(&runtime, &launch, cancel)
                .map_err(|error| failed(Stage::Person, error))?,
        ),
        None => None,
    };
    #[allow(unused_mut)]
    let mut prepared = match person {
        Some(person) => PreparedExecCmd::prepare_person(runtime, &project, request, person, cancel),
        None => PreparedExecCmd::prepare(runtime, &project, request, cancel),
    }
    .map_err(|f| (Stage::Prepare, Failure::from(f)))?;
    #[cfg(feature = "test-support")]
    if let Some(fault) = fault {
        prepared.guardian_fault_for_test(&fault);
    }
    prepared
        .spawn(&executable, cancel, sink)
        .map_err(|f| (Stage::Spawn, Failure::from(f)))
}

/// The person setup of one job. Blocking (account database, `confstr`,
/// `readlink`, directory opens): setup thread only; it checks `cancel`
/// between steps.
fn person_setup(
    runtime: &RuntimeDirectory,
    launch: &PersonLaunch,
    cancel: &AtomicBool,
) -> Result<PersonSetup, ExecCmdError> {
    #[cfg(not(feature = "test-support"))]
    let _ = runtime;
    match launch {
        PersonLaunch::Sandboxed { path, client } => {
            #[cfg(feature = "test-support")]
            if let Some((home, tmpdir)) = person_account(runtime) {
                return PersonSetup::sandboxed_for_test(
                    "fixture", &home, &tmpdir, None, path, client,
                );
            }
            PersonSetup::sandboxed(path, client, cancel)
        }
        PersonLaunch::Outside { launch } => {
            if cancel.load(Ordering::Acquire) {
                return Err(ExecCmdError::Cancelled);
            }
            PersonSetup::outside(launch)
        }
    }
}

/// Test-support person account: when `HOME/exec-cmd-person-account`
/// exists, its first line names the fixture home and its second line the
/// fixture `$TMPDIR`, so a `person-1` test never resolves the account's
/// real home or temporary directory. Read on the setup worker only.
#[cfg(feature = "test-support")]
fn person_account(runtime: &RuntimeDirectory) -> Option<(std::path::PathBuf, std::path::PathBuf)> {
    let home = runtime.scratch_home_for_test()?;
    let text = std::fs::read_to_string(home.join("exec-cmd-person-account")).ok()?;
    let mut lines = text.lines();
    let account = std::path::PathBuf::from(lines.next()?);
    let tmpdir = std::path::PathBuf::from(lines.next()?);
    Some((account, tmpdir))
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
