//! One retained exec_cmd slot. Filesystem preparation/cleanup never run on the reactor.
//!
//! J1 phases: `Setup` ends on spawn, a setup fault or cancel (no deadline,
//! J1-Q4); `Running` ends when `ExecCmdJob` settles (no deadline, J1-Q3);
//! `Cleanup` runs scratch cleanup while the capture worker flushes;
//! `Finalizing` waits for the sealed capture report. After a cancel trigger
//! the existing L3 bound of 2.5 s also covers capture settlement (J1-Q1).
use crate::{
    capture_worker::{self, Message, Report},
    completion::Completion,
    tools::{self, Arguments, ExecutionError, Rejection},
};
use asura_platform::{
    RuntimeDirectory,
    events::WakeSender,
    exec_cmd::{
        ExecCmdCleanup, ExecCmdError, ExecCmdFailure, ExecCmdJob, ExecCmdRequest, ExecCmdResult,
        PreparedExecCmd, StopReason, Tails, render_tail,
    },
    secrets::SecretVariable,
};
use asura_storage::authority::conversation::{
    CaptureReason, CaptureSettled, CaptureState, CaptureStream, ExecCmdDisposition, ExecCmdIntent,
    ToolResult,
};
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

/// One exec_cmd call's dispatch: the committed kind 22, the rewritten call, the
/// secret variables (moved into the request; spawn zeroizes them) and the
/// capture worker's start.
pub(crate) struct Dispatch {
    pub runtime: RuntimeDirectory,
    pub scope: tools::ExecutionScope,
    pub call: tools::Call,
    pub intent: ExecCmdIntent,
    pub secrets: Vec<SecretVariable>,
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
    intent: ExecCmdIntent,
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
#[derive(Default)]
pub(crate) struct Worker {
    active: Option<Active>,
    wake: Option<WakeSender>,
}
impl Worker {
    pub(crate) fn register(&mut self, wake: WakeSender) {
        self.wake = Some(wake.clone());
        if let Some(active) = &self.active {
            match active.phase.as_ref() {
                Some(Phase::Setup(w)) => w.completion.register(wake),
                Some(Phase::Cleanup(w)) => w.completion.register(wake),
                _ => {}
            }
        }
    }
    pub(crate) fn wake(&self) -> Option<WakeSender> {
        self.wake.clone()
    }
    /// Caller has committed this exact kind 22 and rechecked the execution
    /// grant. The call carries the rewrite; it is compared with kind 22, not
    /// with the raw arguments (G8). There is no deadline (J1-Q3, J1-Q4).
    pub(crate) fn start(&mut self, dispatch: Dispatch) -> Result<(), ExecutionError> {
        if self.active.is_some() {
            return Err(ExecutionError::Rejected(Rejection::Busy));
        }
        let Dispatch {
            runtime,
            scope,
            call,
            intent,
            secrets,
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
        if intent.disposition != ExecCmdDisposition::Admitted
            || intent.command != *proposed
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
            command: intent.command.clone(),
            cwd: Some(intent.cwd.clone()),
            secrets,
        };
        let sink = handle.output_sink();
        let work = spawn("asura-exec-cmd-setup", self.wake.as_ref(), move |cancel| {
            let failed = |error| Failure {
                error,
                cleanup: None,
            };
            // Setup checks only cancellation (J1-Q4).
            if cancel.load(Ordering::Acquire) {
                return Err(failed(ExecCmdError::Cancelled));
            }
            let executable =
                std::env::current_exe().map_err(|_| failed(ExecCmdError::Unavailable))?;
            let project = asura_platform::ProjectIdentity::open(&scope.path, &scope.boundary)
                .map_err(|_| failed(ExecCmdError::Denied))?;
            if project.device != scope.device || project.inode != scope.inode {
                return Err(failed(ExecCmdError::Denied));
            }
            #[cfg(feature = "test-support")]
            setup_gate(&runtime, cancel);
            let prepared = PreparedExecCmd::prepare(runtime, &project, request, cancel)
                .map_err(Failure::from)?;
            prepared
                .spawn(&executable, cancel, sink)
                .map_err(Failure::from)
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
                    if let Some(reason) = active.stop {
                        job.stop(reason);
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
                    job.stop(StopReason::Cancelled);
                    active.cleanup_deadline.get_or_insert(now + CLEANUP_BOUND);
                    if let Some(capture) = &active.capture {
                        capture.cancel();
                    }
                }
                if job.settled() {
                    match job.result() {
                        Some(result) if result.cleanup_confirmed => {
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
        Some(Phase::Running(job)) => job.stop(active.stop.unwrap_or(reason)),
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
                let (accepted, discarded, ended) = active
                    .capture
                    .as_ref()
                    .map(|c| c.sink.counts())
                    .unwrap_or_default();
                let stream = |i: usize| CaptureStream {
                    observed_bytes: accepted[i],
                    discarded_bytes: discarded[i],
                    eof: ended[i],
                    ..CaptureStream::default()
                };
                (
                    CaptureState::Unconfirmed,
                    CaptureReason::SettlementUnconfirmed,
                    [stream(0), stream(1)],
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
        rule_set_revision: intent.rule_set_revision,
        rule_set_digest: intent.rule_set_digest,
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
    intent: &ExecCmdIntent,
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
        Some(StopReason::OutputLimit) => 7,
        Some(StopReason::Cancelled | StopReason::LeaseEnded) => 6,
        None => 1,
    };
    let text = result.render_text(tails, capture.as_str());
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

pub(crate) fn failed(intent: &ExecCmdIntent, error: ExecCmdError) -> ToolResult {
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
        text: text.into(),
        next_offset: None,
        truncated: false,
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
