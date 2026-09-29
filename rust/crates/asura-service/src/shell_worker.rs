//! One retained shell slot. Filesystem preparation/cleanup never run on the reactor.
use crate::{
    completion::Completion,
    tools::{self, Arguments, ExecutionError, Rejection},
};
use asura_platform::{
    RuntimeDirectory,
    events::WakeSender,
    shell::{
        PreparedShell, ShellCleanup, ShellError, ShellFailure, ShellJob, ShellRequest, ShellResult,
        StopReason,
    },
};
use asura_storage::authority::conversation::{ToolIntent, ToolResult};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

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
    error: ShellError,
    cleanup: Option<ShellCleanup>,
}
impl From<ShellFailure> for Failure {
    fn from(f: ShellFailure) -> Self {
        Self {
            error: f.reason,
            cleanup: f.cleanup,
        }
    }
}
type SetupResult = Result<ShellJob, Failure>;
struct Cleaned {
    token: Option<ShellCleanup>,
    result: Result<(), ShellError>,
}
enum Phase {
    Setup(ThreadWork<SetupResult>),
    Running(ShellJob),
    Cleanup(ThreadWork<Cleaned>),
    Repair {
        _cleanup: Option<ShellCleanup>,
        _job: Option<ShellJob>,
    },
}
struct Active {
    intent: ToolIntent,
    phase: Option<Phase>,
    setup_deadline: Instant,
    command_deadline: Instant,
    cleanup_deadline: Option<Instant>,
    stop: Option<StopReason>,
    output: Option<ToolResult>,
    repair: bool,
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
    /// Caller has committed this exact intent and rechecked the execution grant.
    pub(crate) fn start(
        &mut self,
        runtime: RuntimeDirectory,
        scope: tools::ExecutionScope,
        call: tools::Call,
        intent: ToolIntent,
        deadline: Instant,
    ) -> Result<(), ExecutionError> {
        if self.active.is_some() {
            return Err(ExecutionError::Rejected(Rejection::Busy));
        }
        let (command, cwd) = intent
            .shell_arguments()
            .map_err(|_| ExecutionError::Rejected(Rejection::InvalidArguments))?;
        let Arguments::Shell {
            command: proposed,
            cwd: proposed_cwd,
            timeout_seconds,
        } = &call.arguments
        else {
            return Err(ExecutionError::Rejected(Rejection::Denied));
        };
        if command != proposed
            || cwd != proposed_cwd
            || intent.limit != *timeout_seconds
            || intent.operation != call.operation
            || intent.generation != call.generation
            || intent.ordinal != call.ordinal
            || scope.project == [0; 16]
        {
            return Err(ExecutionError::Rejected(Rejection::IdentityConflict));
        }
        let now = Instant::now();
        if deadline <= now || deadline.duration_since(now) > Duration::from_millis(intent.offset) {
            return Err(ExecutionError::Rejected(Rejection::Expired));
        }
        let request = ShellRequest {
            job_id: intent
                .shell_job_id()
                .map_err(|_| ExecutionError::Rejected(Rejection::InvalidArguments))?,
            command: command.into(),
            cwd: Some(cwd.into()),
            deadline,
        };
        let setup_deadline = deadline.min(now + Duration::from_secs(2));
        self.begin(intent, deadline, setup_deadline, move |cancel| {
            let failed = |error| Failure {
                error,
                cleanup: None,
            };
            if cancel.load(Ordering::Acquire) {
                return Err(failed(ShellError::Cancelled));
            }
            if Instant::now() >= setup_deadline {
                return Err(failed(ShellError::Deadline));
            }
            let executable =
                std::env::current_exe().map_err(|_| failed(ShellError::Unavailable))?;
            let project = asura_platform::ProjectIdentity::open(&scope.path)
                .map_err(|_| failed(ShellError::Denied))?;
            if project.device != scope.device || project.inode != scope.inode {
                return Err(failed(ShellError::Denied));
            }
            let prepared = PreparedShell::prepare(runtime, &project, request, cancel)
                .map_err(Failure::from)?;
            // Consuming spawn returns the cleanup token if preparation expired.
            if Instant::now() >= setup_deadline {
                cancel.store(true, Ordering::Release);
            }
            prepared.spawn(&executable, cancel).map_err(Failure::from)
        })
    }
    fn begin(
        &mut self,
        intent: ToolIntent,
        deadline: Instant,
        setup_deadline: Instant,
        run: impl FnOnce(&AtomicBool) -> SetupResult + Send + 'static,
    ) -> Result<(), ExecutionError> {
        if self.active.is_some() {
            return Err(ExecutionError::Rejected(Rejection::Busy));
        }
        let work = spawn("asura-shell-setup", self.wake.as_ref(), run)?;
        self.active = Some(Active {
            intent,
            phase: Some(Phase::Setup(work)),
            setup_deadline,
            command_deadline: deadline,
            cleanup_deadline: None,
            stop: None,
            output: None,
            repair: false,
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
    pub(crate) fn cancel(&mut self, reason: StopReason) {
        if let Some(active) = &mut self.active {
            stop(active, reason, Instant::now());
        }
    }
    pub(crate) fn next_deadline(&self, now: Instant) -> Option<Instant> {
        let active = self.active.as_ref()?;
        let stage = match active.phase.as_ref()? {
            Phase::Setup(w) => w
                .next_deadline(now)
                .into_iter()
                .chain(active.stop.is_none().then_some(active.setup_deadline))
                .min(),
            Phase::Running(job) => job
                .next_deadline()
                .into_iter()
                .chain(active.stop.is_none().then_some(active.command_deadline))
                .min(),
            Phase::Cleanup(w) => w.next_deadline(now),
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
    /// Emits once, only after actual ownership and scratch cleanup settle.
    pub(crate) fn poll(&mut self, now: Instant) -> Option<ToolResult> {
        let mut active = self.active.take()?;
        if active.stop.is_none()
            && (now >= active.command_deadline
                || matches!(active.phase, Some(Phase::Setup(_))) && now >= active.setup_deadline)
        {
            stop(&mut active, StopReason::Deadline, now);
        }
        if active.cleanup_deadline.is_some_and(|d| now >= d) {
            active.repair = true;
        }
        let phase = active.phase.take().expect("retained stage");
        let mut complete = false;
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
                        Some(StopReason::Deadline) => ShellError::Deadline,
                        Some(_) => ShellError::Cancelled,
                        None => failure.error,
                    };
                    active.output = Some(failed(&active.intent, error));
                    if let Some(token) = failure.cleanup {
                        self.cleanup(&mut active, token, now)
                    } else if matches!(failure.error, ShellError::CleanupUnconfirmed) {
                        active.repair = true;
                        Phase::Repair {
                            _cleanup: None,
                            _job: None,
                        }
                    } else {
                        complete = true;
                        Phase::Repair {
                            _cleanup: None,
                            _job: None,
                        }
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
                    if active.output.is_none() {
                        active.output = Some(failed(&active.intent, ShellError::Protocol));
                    }
                    job.stop(StopReason::Cancelled);
                    active
                        .cleanup_deadline
                        .get_or_insert(now + Duration::from_millis(2500));
                }
                if job.settled() {
                    if let Some(result) = job.result() {
                        if !result.cleanup_confirmed {
                            active.repair = true;
                            Phase::Repair {
                                _cleanup: None,
                                _job: Some(job),
                            }
                        } else {
                            if active.output.is_none() {
                                active.output = Some(mapped_result(&active.intent, &result));
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
                    } else {
                        active.repair = true;
                        Phase::Repair {
                            _cleanup: None,
                            _job: Some(job),
                        }
                    }
                } else {
                    Phase::Running(job)
                }
            }
            Phase::Cleanup(work) if work.thread.is_finished() => match work.thread.join() {
                Ok(cleaned) if cleaned.result.is_ok() => {
                    complete = true;
                    Phase::Repair {
                        _cleanup: None,
                        _job: None,
                    }
                }
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
        if complete && active.output.is_some() {
            active.output.take()
        } else {
            self.active = Some(active);
            None
        }
    }
    fn cleanup(&self, active: &mut Active, token: ShellCleanup, now: Instant) -> Phase {
        let deadline = *active
            .cleanup_deadline
            .get_or_insert(now + Duration::from_millis(2500));
        // A failed thread spawn must not drop the unique cleanup token.
        // The reactor locks this handoff only if spawn failed: no thread exists
        // then, so no producer can hold the lock or block the reactor.
        let shared = Arc::new(std::sync::Mutex::new(Some(token)));
        let handoff = shared.clone();
        match spawn("asura-shell-cleanup", self.wake.as_ref(), move |cancel| {
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
fn stop(active: &mut Active, reason: StopReason, now: Instant) {
    if active.stop.is_none() {
        active.stop = Some(reason);
        active.cleanup_deadline = Some(now + Duration::from_millis(2500));
    }
    match active.phase.as_mut() {
        Some(Phase::Setup(work)) => work.cancel.store(true, Ordering::Release),
        Some(Phase::Running(job)) => job.stop(active.stop.unwrap_or(reason)),
        _ => {}
    }
}
fn mapped_result(intent: &ToolIntent, result: &ShellResult) -> ToolResult {
    // A settled setup failure proves cleanup, not successful command execution.
    if let Some(error) = result.failure {
        return failed(intent, error);
    }
    if (result.exit_code.is_some() && result.signal.is_some())
        || (result.exit_code.is_none() && result.signal.is_none() && result.reason.is_none())
    {
        return failed(intent, ShellError::Protocol);
    }
    let status = match result.reason {
        Some(StopReason::Deadline) => 5,
        Some(StopReason::OutputLimit) => 7,
        Some(StopReason::Cancelled | StopReason::LeaseEnded) => 6,
        None => 1,
    };
    let text = result.render_text();
    if text.len() > 16_384 {
        return failed(intent, ShellError::Protocol);
    }
    ToolResult {
        operation: intent.operation,
        generation: intent.generation,
        ordinal: intent.ordinal,
        status,
        text,
        next_offset: None,
        truncated: result.truncated,
    }
}

fn failed(intent: &ToolIntent, error: ShellError) -> ToolResult {
    let (status, text) = match error {
        ShellError::Invalid => (3, "shell_invalid_arguments"),
        ShellError::Denied => (2, "shell_denied"),
        ShellError::Deadline => (5, "shell_timeout; effects may have occurred"),
        ShellError::Cancelled => (6, "shell_cancelled; effects may have occurred"),
        ShellError::Protocol | ShellError::CleanupUnconfirmed => {
            (4, "shell_outcome_unconfirmed; effects may have occurred")
        }
        ShellError::Unavailable | ShellError::Io => (4, "shell_unavailable"),
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
mod tests {
    use super::*;
    fn intent() -> ToolIntent {
        ToolIntent {
            operation: [1; 16],
            generation: 1,
            ordinal: 1,
            kind: 16,
            path: "true\0.".into(),
            offset: 1000,
            limit: 30,
        }
    }
    fn finish(worker: &mut Worker) -> Option<ToolResult> {
        let end = Instant::now() + Duration::from_secs(2);
        loop {
            if let Some(result) = worker.poll(Instant::now()) {
                return Some(result);
            }
            if worker.settlement_expired()
                && matches!(
                    worker.active.as_ref().and_then(|a| a.phase.as_ref()),
                    Some(Phase::Repair { .. })
                )
            {
                return None;
            }
            assert!(Instant::now() < end, "worker did not settle");
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    #[test]
    fn stalled_setup_retains_slot_after_deadline_and_accepts_late_settlement() {
        let mut worker = Worker::default();
        let (release, blocked) = std::sync::mpsc::sync_channel(1);
        let (reader, wake) = asura_platform::events::wake_pair().unwrap();
        worker.register(wake);
        let now = Instant::now();
        worker
            .begin(
                intent(),
                now + Duration::from_secs(10),
                now,
                move |cancel| {
                    blocked.recv_timeout(Duration::from_secs(2)).unwrap();
                    assert!(cancel.load(Ordering::Acquire));
                    Err(Failure {
                        error: ShellError::Cancelled,
                        cleanup: None,
                    })
                },
            )
            .unwrap();
        let before = Instant::now();
        assert!(worker.poll(now).is_none());
        assert!(before.elapsed() < Duration::from_millis(100));
        assert!(!worker.is_settled());
        assert_eq!(
            worker.begin(intent(), now, now, |_| unreachable!()),
            Err(ExecutionError::Rejected(Rejection::Busy))
        );
        assert!(worker.poll(now + Duration::from_secs(3)).is_none());
        assert!(worker.settlement_expired());
        assert!(worker.next_deadline(now + Duration::from_secs(3)).is_none());
        release.send(()).unwrap();
        let result = finish(&mut worker).unwrap();
        assert_eq!(result.status, 5);
        assert!(worker.is_settled());
        reader.drain().unwrap();
        assert!(worker.poll(Instant::now()).is_none());
    }
    #[test]
    fn completion_signal_is_not_thread_settlement_or_permission_to_replace() {
        let mut worker = Worker::default();
        let (release, blocked) = std::sync::mpsc::sync_channel(1);
        let now = Instant::now();
        worker
            .begin(
                intent(),
                now + Duration::from_secs(10),
                now + Duration::from_secs(2),
                move |_| {
                    blocked.recv_timeout(Duration::from_secs(2)).unwrap();
                    Err(Failure {
                        error: ShellError::Unavailable,
                        cleanup: None,
                    })
                },
            )
            .unwrap();
        if let Some(Phase::Setup(work)) = worker.active.as_ref().unwrap().phase.as_ref() {
            work.completion.finish();
        }
        assert!(worker.next_deadline(now).is_some());
        assert!(worker.poll(now).is_none());
        assert!(!worker.is_settled());
        worker.cancel(StopReason::Cancelled);
        release.send(()).unwrap();
        assert_eq!(finish(&mut worker).unwrap().status, 6);
    }
    #[test]
    fn cleanup_deadline_retains_thread_then_success_or_failure_is_handled_once() {
        for succeeds in [true, false] {
            let now = Instant::now();
            let (release, blocked) = std::sync::mpsc::sync_channel(1);
            let work = spawn("shell-cleanup-fixture", None, move |_| {
                blocked.recv_timeout(Duration::from_secs(2)).unwrap();
                Cleaned {
                    token: None,
                    result: if succeeds {
                        Ok(())
                    } else {
                        Err(ShellError::Io)
                    },
                }
            })
            .unwrap();
            let mut worker = Worker {
                wake: None,
                active: Some(Active {
                    intent: intent(),
                    phase: Some(Phase::Cleanup(work)),
                    setup_deadline: now,
                    command_deadline: now + Duration::from_secs(10),
                    cleanup_deadline: Some(now),
                    stop: None,
                    output: Some(failed(&intent(), ShellError::Deadline)),
                    repair: false,
                }),
            };
            assert!(worker.poll(now).is_none());
            assert!(worker.settlement_expired());
            assert!(!worker.is_settled());
            assert!(worker.next_deadline(now).is_none());
            release.send(()).unwrap();
            let result = finish(&mut worker);
            if succeeds {
                assert_eq!(result.unwrap().status, 5);
                assert!(worker.is_settled());
            } else {
                assert!(result.is_none());
                assert!(!worker.is_settled());
                assert!(worker.next_deadline(Instant::now()).is_none());
                assert_eq!(
                    worker.begin(intent(), now, now, |_| unreachable!()),
                    Err(ExecutionError::Rejected(Rejection::Busy))
                );
            }
        }
    }
    #[test]
    fn settled_setup_failures_never_become_successful_command_results() {
        let mut result = ShellResult {
            exit_code: None,
            signal: None,
            reason: None,
            failure: None,
            stdout: "untrusted capture".into(),
            stderr: String::new(),
            truncated: true,
            cleanup_confirmed: true,
        };
        for (error, status) in [
            (ShellError::Invalid, 3),
            (ShellError::Denied, 2),
            (ShellError::Deadline, 5),
            (ShellError::Cancelled, 6),
            (ShellError::Unavailable, 4),
            (ShellError::Protocol, 4),
            (ShellError::CleanupUnconfirmed, 4),
            (ShellError::Io, 4),
        ] {
            result.failure = Some(error);
            let mapped = mapped_result(&intent(), &result);
            assert_eq!(mapped.status, status);
            assert!(!mapped.truncated);
            assert!(mapped.next_offset.is_none());
            assert!(!mapped.text.contains("untrusted capture"));
        }
        result.failure = None;
        assert_eq!(mapped_result(&intent(), &result).status, 4);
        result.exit_code = Some(17);
        assert_eq!(mapped_result(&intent(), &result).status, 1);
        result.reason = Some(StopReason::Deadline);
        assert_eq!(mapped_result(&intent(), &result).status, 5);
        result.failure = Some(ShellError::Denied);
        assert_eq!(mapped_result(&intent(), &result).status, 2);
    }
}
