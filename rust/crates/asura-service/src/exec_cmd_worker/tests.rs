use super::*;
use asura_storage::authority::conversation::CaptureStream;

fn intent() -> ExecCmdRun {
    ExecCmdRun {
        operation: [1; 16],
        generation: 1,
        ordinal: 1,
        command: "true".into(),
        cwd: ".".into(),
        filtered: None,
        job: [2; 16],
        stdout_payload: [3; 16],
        stderr_payload: [4; 16],
    }
}
fn settled_process() -> Process {
    Process::Settled(ExecCmdResult {
        failure: None,
        exit_code: Some(0),
        signal: None,
        reason: None,
        setup_reason: 0,
        cleanup_confirmed: true,
        escapees: 0,
    })
}
fn finalizing(cleanup_deadline: Option<Instant>, stop: Option<StopReason>) -> Worker {
    Worker {
        wake: None,
        slot: None,
        active: Some(Active {
            intent: intent(),
            phase: Some(Phase::Finalizing),
            cleanup_deadline,
            stop,
            process: Some(settled_process()),
            repair: false,
            capture: None,
            report: None,
            capture_unavailable: false,
        }),
    }
}

/// J1T-21 unit (J1-Q1): with no cancel trigger the call waits for the sealed
/// report without any bound; after a cancel trigger, passing the L3 bound
/// with no report settles `unconfirmed` with empty tails and no digest.
#[test]
fn j1t_21_bound_decision_after_a_cancel_trigger() {
    let now = Instant::now();
    let mut waiting = finalizing(None, None);
    assert!(waiting.poll(now + Duration::from_secs(3600)).is_none());
    assert!(
        waiting.next_deadline(now).is_none(),
        "normal completion has no bound"
    );
    assert!(!waiting.settlement_expired());

    let mut before = finalizing(Some(now + CLEANUP_BOUND), Some(StopReason::Cancelled));
    assert!(before.poll(now).is_none());
    assert_eq!(before.next_deadline(now), Some(now + CLEANUP_BOUND));

    let mut after = finalizing(Some(now), Some(StopReason::Cancelled));
    let output = after.poll(now).expect("settled at the bound");
    assert_eq!(output.settled.state, CaptureState::Unconfirmed);
    assert_eq!(output.settled.reason, CaptureReason::SettlementUnconfirmed);
    assert!(output.settled.stdout.digest.is_none() && output.settled.stderr.digest.is_none());
    assert_eq!(
        (output.settled.exit_kind, output.settled.exit_value),
        (1, 0)
    );
    assert!(output.result.text.contains("capture=unconfirmed"));
    assert!(output.result.text.ends_with("stdout:\n\nstderr:\n"));
    assert!(after.is_settled());
    assert!(
        !after.settlement_expired(),
        "unconfirmed capture is not repair"
    );
}

/// A sealed report settles at once, with the redacted tails and the
/// report's capture facts.
#[test]
fn sealed_report_builds_kind_23_and_kind_15_from_redacted_tails() {
    let now = Instant::now();
    let mut worker = finalizing(None, None);
    let stream = |tail: &[u8]| capture_worker::StreamReport {
        stored: tail.len() as u64,
        digest: Some([9; 32]),
        stats: Default::default(),
        discarded: 0,
        eof: true,
        lines: 1,
        tail: tail.to_vec(),
    };
    worker.active.as_mut().unwrap().report = Some(Box::new(Report {
        state: CaptureState::Complete,
        reason: CaptureReason::None,
        streams: [stream(b"token: $ASURA_SECRET_1\n"), stream(b"")],
        distinct_numbers: 1,
        numbers: Vec::new(),
        pause_count: 0,
        first_pause_ms: 0,
        paused_ms: 0,
        final_reservation: 1 << 20,
    }));
    let output = worker.poll(now).expect("report present");
    assert_eq!(output.settled.state, CaptureState::Complete);
    assert_eq!(output.settled.ordinal, 1);
    assert_eq!(output.result.status, 1);
    assert!(
        output
            .result
            .text
            .contains("capture=complete\nstdout:\ntoken: $ASURA_SECRET_1\n")
    );
    assert!(!output.result.truncated);
}

/// J1T-06 unit: a setup thread that has not finished sets no deadline; only
/// its completion or a cancel trigger ends setup (J1-Q4).
#[test]
fn j1t_06_setup_has_no_deadline() {
    let (release, blocked) = std::sync::mpsc::sync_channel::<()>(1);
    let work = spawn("exec-cmd-setup-fixture", None, move |cancel| {
        let _ = blocked.recv();
        if cancel.load(Ordering::Acquire) {
            Err(Failure {
                error: ExecCmdError::Cancelled,
                cleanup: None,
            })
        } else {
            Err(Failure {
                error: ExecCmdError::Unavailable,
                cleanup: None,
            })
        }
    })
    .unwrap();
    let mut worker = Worker {
        wake: None,
        slot: None,
        active: Some(Active {
            intent: intent(),
            phase: Some(Phase::Setup(work)),
            cleanup_deadline: None,
            stop: None,
            process: None,
            repair: false,
            capture: None,
            report: None,
            capture_unavailable: true,
        }),
    };
    let now = Instant::now();
    assert!(worker.next_deadline(now).is_none());
    assert!(worker.poll(now + Duration::from_secs(3600)).is_none());
    worker.cancel(StopReason::Cancelled);
    assert_eq!(worker.next_deadline(now).map(|d| d > now), Some(true));
    release.send(()).unwrap();
    let end = Instant::now() + Duration::from_secs(5);
    let output = loop {
        if let Some(output) = worker.poll(Instant::now()) {
            break output;
        }
        assert!(Instant::now() < end);
        std::thread::sleep(Duration::from_millis(1));
    };
    assert_eq!(output.result.status, 6, "cancelled during setup");
    assert_eq!(output.settled.state, CaptureState::NotStarted);
    assert_eq!(output.settled.stdout, CaptureStream::default());
}

/// Settled setup failures never become a successful command result.
#[test]
fn settled_setup_failures_never_become_successful_command_results() {
    let tails = Tails {
        stdout: "untrusted capture".into(),
        stderr: String::new(),
        truncated: true,
    };
    let mut result = ExecCmdResult {
        exit_code: None,
        signal: None,
        reason: None,
        failure: None,
        setup_reason: 0,
        cleanup_confirmed: true,
        escapees: 0,
    };
    for (error, status) in [
        (ExecCmdError::Invalid, 3),
        (ExecCmdError::Denied, 2),
        (ExecCmdError::Cancelled, 6),
        (ExecCmdError::Unavailable, 4),
        (ExecCmdError::Protocol, 4),
        (ExecCmdError::CleanupUnconfirmed, 4),
        (ExecCmdError::Io, 4),
    ] {
        result.failure = Some(error);
        let mapped = mapped_result(&intent(), &result, &tails, CaptureState::Complete);
        assert_eq!(mapped.status, status);
        assert!(!mapped.truncated);
        assert!(!mapped.text.contains("untrusted capture"));
    }
    result.failure = None;
    assert_eq!(
        mapped_result(&intent(), &result, &tails, CaptureState::Complete).status,
        4
    );
    result.exit_code = Some(17);
    let mapped = mapped_result(&intent(), &result, &tails, CaptureState::Incomplete);
    assert_eq!(mapped.status, 1);
    assert!(mapped.text.contains("capture=incomplete"));
    result.reason = Some(StopReason::Cancelled);
    assert_eq!(
        mapped_result(&intent(), &result, &tails, CaptureState::Complete).status,
        6
    );
}

/// SL7: a whole-command run's result says it was filtered beside the display
/// text, after the status line that replay reads the exit code from; the
/// display is cut at 2,048 bytes before a placeholder and stays one line.
#[test]
fn sl7_whole_command_result_says_filtered_beside_display() {
    assert_eq!(
        filtered_line("echo '$ASURA_SECRET_1'\nls"),
        "command filtered: run from a secret variable: echo '$ASURA_SECRET_1' ls"
    );
    let long = format!("{}$ASURA_SECRET_12 tail", "x".repeat(2040));
    let line = filtered_line(&long);
    assert_eq!(
        line,
        format!(
            "command filtered: run from a secret variable: {}…",
            "x".repeat(2040)
        )
    );
    let fits = format!("{}$ASURA_SECRET_1 tail", "x".repeat(2048 - 15));
    assert!(filtered_line(&fits).ends_with("$ASURA_SECRET_1…"));
    let wide = "é".repeat(1100);
    assert!(filtered_line(&wide).ends_with('…'));
    let mut run = intent();
    run.filtered = Some(filtered_line("echo '$ASURA_SECRET_1'"));
    let process = ExecCmdResult {
        failure: None,
        exit_code: Some(3),
        signal: None,
        reason: None,
        setup_reason: 0,
        cleanup_confirmed: true,
        escapees: 0,
    };
    let tails = Tails {
        stdout: "$ASURA_SECRET_1\n".into(),
        stderr: String::new(),
        truncated: false,
    };
    let result = mapped_result(&run, &process, &tails, CaptureState::Complete);
    let mut lines = result.text.lines();
    assert!(lines.next().unwrap().contains("exit_code=3"));
    assert_eq!(
        lines.next(),
        Some("command filtered: run from a secret variable: echo '$ASURA_SECRET_1'")
    );
    assert!(result.text.contains("\nstdout:\n$ASURA_SECRET_1\n"));
    let failed = failed(&run, ExecCmdError::Unavailable);
    assert_eq!(
        failed.text,
        "exec_cmd_unavailable\ncommand filtered: run from a secret variable: echo '$ASURA_SECRET_1'"
    );
    // The largest tails and display still fit the 16,384-byte result.
    let big = Tails {
        stdout: "o".repeat(6144),
        stderr: "e".repeat(6144),
        truncated: true,
    };
    run.filtered = Some(filtered_line(&"d".repeat(8192)));
    let result = mapped_result(&run, &process, &big, CaptureState::Complete);
    assert_eq!(result.status, 1);
    assert!(result.text.len() <= 16_384);
    // An ordinary run is unchanged.
    let plain = mapped_result(&intent(), &process, &tails, CaptureState::Complete);
    assert!(!plain.text.contains("command filtered"));
}

// Job mode (J2). The guardian cannot run inside the unit test binary, so
// these cases cover the slot's own decisions with real capture workers and
// a real payload store; the real guardian paths are worker-layer
// integration cases (J2-E, `tests/job_controller_flow.rs`).

/// A private scratch home with a project and a real payload store.
struct Home {
    path: std::path::PathBuf,
    runtime: RuntimeDirectory,
    store: Arc<asura_storage::payload::PayloadStore>,
}
impl Home {
    fn new(quota: u64) -> Self {
        use std::os::unix::fs::DirBuilderExt;
        let path = std::path::PathBuf::from(format!(
            "/private/tmp/as-slot-{:x}",
            u128::from_ne_bytes(asura_platform::random_id())
        ));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .unwrap();
        std::fs::create_dir(path.join("project")).unwrap();
        let runtime = RuntimeDirectory::scratch(&path, true).unwrap();
        let store = asura_storage::payload::PayloadStore::open(&runtime).unwrap();
        store.set_quota(quota).unwrap();
        Self {
            path,
            runtime,
            store: Arc::new(store),
        }
    }
    fn scope(&self) -> tools::ExecutionScope {
        let path = self.path.join("project").to_string_lossy().into_owned();
        let identity =
            asura_platform::ProjectIdentity::open(&path, &self.runtime.state_boundary()).unwrap();
        tools::ExecutionScope {
            project: [7; 16],
            path,
            device: identity.device,
            inode: identity.inode,
            boundary: self.runtime.state_boundary(),
        }
    }
    fn capture(&self, job: journal::Id) -> capture_worker::Start {
        let victims = self
            .store
            .admit([9; 16], job, asura_platform::random_id())
            .unwrap();
        capture_worker::Start {
            store: self.store.clone(),
            job,
            victims,
            redaction: asura_platform::secrets::JobRedaction::new(
                asura_platform::secrets::detection().unwrap(),
                &[],
            ),
            wake: None,
            gate_home: Some(self.path.clone()),
        }
    }
    fn start(&self, job: journal::Id, scope: tools::ExecutionScope) -> SlotStart {
        SlotStart {
            job,
            profile: asura_platform::exec_cmd::Profile::Model1,
            runtime: self.runtime.clone(),
            scope,
            command: "true".into(),
            cwd: ".".into(),
            secrets: Vec::new(),
            secret_command: None,
            capture: self.capture(job),
        }
    }
}
impl Drop for Home {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}
const MIB: u64 = asura_storage::payload::MIB;
fn name(event: &SlotEvent) -> &'static str {
    match event {
        SlotEvent::Spawned => "spawned",
        SlotEvent::SetupFailed(_) => "setup_failed",
        SlotEvent::LeaderExited(_) => "leader_exited",
        SlotEvent::CleanupUnconfirmed(_) => "cleanup_unconfirmed",
        SlotEvent::CapturePaused(_) => "capture_paused",
        SlotEvent::CaptureResumed => "capture_resumed",
        SlotEvent::CaptureFault(_) => "capture_fault",
        SlotEvent::ProcessSettled { .. } => "process_settled",
        SlotEvent::CaptureSettled(_) => "capture_settled",
        SlotEvent::ScratchCleaned => "scratch_cleaned",
        SlotEvent::ScratchRetained => "scratch_retained",
        SlotEvent::IdentityChanged => "identity_changed",
    }
}
fn names(events: &[SlotEvent]) -> Vec<&'static str> {
    events.iter().map(name).collect()
}
/// Polls until the slot is idle; returns every event in order. A 30 s test
/// guard turns a regression into a failure instead of a hung suite.
fn poll_until_idle(worker: &mut Worker) -> Vec<SlotEvent> {
    let end = Instant::now() + Duration::from_secs(30);
    let mut events = Vec::new();
    while !worker.idle() {
        let before = events.len();
        worker.poll_job(Instant::now(), &mut events);
        assert!(events.len() - before <= SLOT_EVENTS);
        assert!(Instant::now() < end, "slot not idle: {:?}", names(&events));
        std::thread::sleep(Duration::from_millis(1));
    }
    events
}
fn capture_outcome(event: Option<&mut SlotEvent>) -> &mut CaptureOutcome {
    match event {
        Some(SlotEvent::CaptureSettled(outcome)) => outcome,
        _ => panic!("capture settled event"),
    }
}
fn settled_slot(events: usize) -> Slot {
    let mut slot = Slot::new(SlotPhase::Done, None);
    slot.process = SlotProcess::None;
    slot.capture_settled = true;
    slot.scratch_cleaned = true;
    for _ in 0..events {
        slot.events.push_back(SlotEvent::CaptureResumed);
    }
    slot
}

/// One poll returns at most 8 slot events; the rest wait for the next
/// poll, and the slot empties only after its last event is delivered.
#[test]
fn slot_poll_returns_at_most_eight_events() {
    let mut worker = Worker {
        slot: Some(Box::new(settled_slot(11))),
        ..Worker::default()
    };
    let now = Instant::now();
    assert_eq!(worker.job_next_deadline(now), Some(now));
    let mut out = Vec::new();
    worker.poll_job(now, &mut out);
    assert_eq!(out.len(), SLOT_EVENTS);
    assert!(!worker.idle());
    worker.poll_job(now, &mut out);
    assert_eq!(out.len(), 11);
    assert!(worker.idle());
    assert!(worker.job_next_deadline(now).is_none());
    let mut interests = Vec::new();
    worker.job_interests(&mut interests);
    assert!(interests.is_empty(), "no running job, no pipe reads");
}

/// `stop` is idempotent; `Kill` is the strongest request and a later
/// `Term` never weakens it. The first trigger alone starts the L3 bound.
#[test]
fn stop_requests_are_idempotent_and_kill_is_strongest() {
    let mut worker = Worker {
        slot: Some(Box::new(Slot::new(SlotPhase::Done, None))),
        ..Worker::default()
    };
    worker.stop_job(StopKind::Term);
    let slot = worker.slot.as_ref().unwrap();
    let bound = slot.capture_bound.expect("L3 starts at the first trigger");
    assert_eq!(slot.stop, Some(StopKind::Term));
    std::thread::sleep(Duration::from_millis(5));
    worker.stop_job(StopKind::Kill);
    worker.stop_job(StopKind::Term);
    worker.discard_capture();
    let slot = worker.slot.as_ref().unwrap();
    assert_eq!(slot.stop, Some(StopKind::Kill));
    assert_eq!(slot.capture_bound, Some(bound));
    // An idle slot ignores requests.
    let mut idle = Worker::default();
    idle.stop_job(StopKind::Kill);
    idle.discard_capture();
    assert!(idle.idle());
}

/// A slot holds one job; a zero job, a zero project or a capture keyed by
/// another job is refused before any thread starts.
#[test]
fn start_job_refuses_busy_and_invalid_starts() {
    let home = Home::new(64 * MIB);
    let mut worker = Worker {
        slot: Some(Box::new(Slot::new(SlotPhase::Done, None))),
        ..Worker::default()
    };
    let job = asura_platform::random_id();
    assert_eq!(
        worker.start_job(home.start(job, home.scope())).err(),
        Some(SlotStartError::Busy)
    );
    let mut worker = Worker::default();
    let mut start = home.start(asura_platform::random_id(), home.scope());
    start.capture.job = asura_platform::random_id();
    assert_eq!(worker.start_job(start).err(), Some(SlotStartError::Invalid));
    let mut scope = home.scope();
    scope.project = [0; 16];
    let start = home.start(asura_platform::random_id(), scope);
    assert_eq!(worker.start_job(start).err(), Some(SlotStartError::Invalid));
    assert!(worker.idle());
}

/// J2T-15 slot part (J2-E10): the setup recheck before spawn finds another
/// project directory: `IdentityChanged` (`project_changed` for this job),
/// no scratch was created, capture is `not_started` and the slot is
/// released.
#[test]
fn j2t_15_setup_recheck_mismatch_reports_identity_changed() {
    let home = Home::new(64 * MIB);
    let mut scope = home.scope();
    scope.inode += 1;
    let mut worker = Worker::default();
    worker
        .start_job(home.start(asura_platform::random_id(), scope))
        .unwrap();
    assert!(!worker.idle());
    let mut events = poll_until_idle(&mut worker);
    assert_eq!(
        names(&events),
        ["identity_changed", "capture_settled", "scratch_cleaned"]
    );
    let outcome = capture_outcome(events.get_mut(1));
    assert_eq!(outcome.state, CaptureState::NotStarted);
    assert_eq!(outcome.reason, CaptureReason::None);
    assert!(outcome.stdout.capture.digest.is_none() && outcome.stderr.capture.digest.is_none());
    // The worker removes its staging and exits; the payload owner waits.
    let mut capture = outcome.capture.take().expect("worker handed over");
    let end = Instant::now() + Duration::from_secs(30);
    while !capture.exited {
        capture.drain();
        assert!(Instant::now() < end, "discarding worker did not exit");
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// Scratch directories of exec_cmd jobs left in `home`.
fn scratch_left(home: &Home) -> Vec<String> {
    match std::fs::read_dir(home.path.join(".asura/tmp")) {
        Ok(entries) => entries
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.starts_with("exec-cmd-"))
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// J2T-25 slot part (J2-A2): a guardian that exits before any notice. The
/// unit test binary is not a guardian: run with the guardian argv it
/// refuses the arguments and exits with no status byte, as a guardian with
/// an unknown profile token does. The job settles `protocol_fault` with
/// capture `not_started`; the platform releases the scratch token, so the
/// scratch is deleted (`ScratchCleaned`) and the slot is released, and a
/// second job then uses the same slot. The controller's kind 28 with the
/// real guardian is J2-E's flow case.
#[test]
fn j2t_25_guardian_exit_before_any_notice_releases_scratch_and_slot() {
    let home = Home::new(64 * MIB);
    let mut worker = Worker::default();
    for round in 0..2 {
        worker
            .start_job(home.start(asura_platform::random_id(), home.scope()))
            .unwrap();
        let mut events = poll_until_idle(&mut worker);
        assert_eq!(
            names(&events),
            ["setup_failed", "capture_settled", "scratch_cleaned"],
            "round {round}"
        );
        assert!(
            matches!(events[0], SlotEvent::SetupFailed(JobReason::ProtocolFault)),
            "round {round}"
        );
        assert_port_order(&events);
        let outcome = capture_outcome(events.get_mut(1));
        assert_eq!(outcome.state, CaptureState::NotStarted, "round {round}");
        if let Some(capture) = outcome.capture.take() {
            capture.abandon();
        }
        assert!(worker.idle(), "round {round}: slot released");
        assert!(
            scratch_left(&home).is_empty(),
            "round {round}: scratch deleted"
        );
    }
}

/// J2T-25 slot part (J2-A2): a guardian that dies by a signal before any
/// notice may have launched the command, so the job settles
/// `protocol_fault` with capture `not_started`, but the slot reports
/// `ScratchRetained`, keeps the scratch and stays held until restart. The
/// test-support replacement runs a script that kills itself with SIGKILL.
#[cfg(feature = "test-support")]
#[test]
fn j2t_25_guardian_signal_death_before_any_notice_retains_scratch() {
    use std::os::unix::fs::PermissionsExt;
    let home = Home::new(64 * MIB);
    let script = home.path.join("killed-guardian");
    std::fs::write(&script, "#!/bin/sh\nkill -KILL $$\n").unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::write(
        home.path.join("exec-cmd-guardian-executable"),
        script.to_str().unwrap(),
    )
    .unwrap();
    let mut worker = Worker::default();
    worker
        .start_job(home.start(asura_platform::random_id(), home.scope()))
        .unwrap();
    let end = Instant::now() + Duration::from_secs(30);
    let mut events = Vec::new();
    while !matches!(events.last(), Some(SlotEvent::ScratchRetained)) {
        worker.poll_job(Instant::now(), &mut events);
        assert!(
            Instant::now() < end,
            "no scratch event: {:?}",
            names(&events)
        );
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(
        names(&events),
        ["setup_failed", "capture_settled", "scratch_retained"]
    );
    assert!(matches!(
        events[0],
        SlotEvent::SetupFailed(JobReason::ProtocolFault)
    ));
    assert_port_order(&events);
    let outcome = capture_outcome(events.get_mut(1));
    assert_eq!(outcome.state, CaptureState::NotStarted);
    if let Some(capture) = outcome.capture.take() {
        capture.abandon();
    }
    let mut more = Vec::new();
    worker.poll_job(Instant::now(), &mut more);
    assert!(more.is_empty());
    assert!(!worker.idle(), "the slot stays held");
    assert_eq!(scratch_left(&home).len(), 1, "the scratch is retained");
}

/// TJ-S1 `Starting` → `Recording` (J2T-07 slot part): a stop before spawn
/// sets the setup cancel flag; setup ends with no process and no reason of
/// its own (the controller's first trigger is the reason), and capture
/// settles `not_started`. Setup itself has no deadline (J1-Q4).
#[cfg(feature = "test-support")]
#[test]
fn stop_before_spawn_settles_without_a_process() {
    let home = Home::new(64 * MIB);
    std::fs::write(home.path.join("exec-cmd-hold-setup"), b"").unwrap();
    let mut worker = Worker::default();
    worker
        .start_job(home.start(asura_platform::random_id(), home.scope()))
        .unwrap();
    let mut events = Vec::new();
    std::thread::sleep(Duration::from_millis(200));
    worker.poll_job(Instant::now(), &mut events);
    assert!(events.is_empty(), "setup waits with no bound");
    assert!(
        worker.job_next_deadline(Instant::now()).is_none(),
        "no setup deadline"
    );
    worker.stop_job(StopKind::Term);
    let mut events = poll_until_idle(&mut worker);
    assert_eq!(
        names(&events),
        ["setup_failed", "capture_settled", "scratch_cleaned"]
    );
    assert!(matches!(events[0], SlotEvent::SetupFailed(JobReason::None)));
    let outcome = capture_outcome(events.get_mut(1));
    assert_eq!(outcome.state, CaptureState::NotStarted);
}

/// J2T-19 unit (J1-Q1 for jobs): after the settled notice, capture waits
/// for its sealed report with no bound; after a cancel trigger, passing L3
/// with no report settles `unconfirmed` and abandons the worker.
#[cfg(feature = "test-support")]
#[test]
fn j2t_19_capture_bound_only_after_a_cancel_trigger() {
    let home = Home::new(64 * MIB);
    let gate = home.path.join("capture-hold-scan");
    std::fs::write(&gate, b"").unwrap();
    let job = asura_platform::random_id();
    let handle = capture_worker::start(home.capture(job)).unwrap();
    let mut sink = handle.output_sink();
    sink.accept(asura_platform::secrets::Stream::Stdout, b"held\n");
    let mut slot = Slot::new(SlotPhase::Done, Some(handle));
    slot.process = SlotProcess::Settled;
    slot.scratch_cleaned = true;
    let mut worker = Worker {
        slot: Some(Box::new(slot)),
        ..Worker::default()
    };
    let now = Instant::now();
    let mut events = Vec::new();
    worker.poll_job(now + Duration::from_secs(3600), &mut events);
    assert!(events.is_empty(), "normal completion has no bound");
    assert!(worker.job_next_deadline(now).is_none());
    worker.stop_job(StopKind::Term);
    let bound = worker.job_next_deadline(now).expect("L3 deadline");
    assert!(bound > now);
    worker.poll_job(bound + Duration::from_millis(1), &mut events);
    assert_eq!(names(&events), ["capture_settled"]);
    let outcome = capture_outcome(events.last_mut());
    assert_eq!(outcome.state, CaptureState::Unconfirmed);
    assert_eq!(outcome.reason, CaptureReason::SettlementUnconfirmed);
    assert_eq!(outcome.stdout.capture.observed_bytes, 5);
    assert!(outcome.stdout.capture.digest.is_none());
    assert_eq!(outcome.final_reservation, MIB);
    assert!(worker.idle());
    // The kept worker exits once its blocked scan returns.
    let mut handle = outcome.capture.take().expect("handed to pending installs");
    std::fs::remove_file(&gate).unwrap();
    let end = Instant::now() + Duration::from_secs(30);
    while !handle.exited {
        handle.drain();
        assert!(Instant::now() < end, "abandoned worker did not exit");
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// J2T-22 slot part: a settled job's capture outcome carries the kind 28
/// stream blocks with line counts from the sealed report, and the worker
/// that installs after kind 28 commits.
#[test]
fn j2t_22_sealed_outcome_carries_line_counts() {
    let home = Home::new(64 * MIB);
    let job = asura_platform::random_id();
    let handle = capture_worker::start(home.capture(job)).unwrap();
    let mut sink = handle.output_sink();
    sink.accept(asura_platform::secrets::Stream::Stdout, b"a\nb\nc");
    sink.accept(asura_platform::secrets::Stream::Stderr, b"e\n");
    sink.end(asura_platform::secrets::Stream::Stdout);
    sink.end(asura_platform::secrets::Stream::Stderr);
    let mut slot = Slot::new(SlotPhase::Done, Some(handle));
    slot.process = SlotProcess::Settled;
    slot.scratch_cleaned = true;
    let mut worker = Worker {
        slot: Some(Box::new(slot)),
        ..Worker::default()
    };
    let mut events = poll_until_idle(&mut worker);
    assert_eq!(names(&events), ["capture_settled"]);
    let outcome = capture_outcome(events.last_mut());
    assert_eq!(outcome.state, CaptureState::Complete);
    assert_eq!((outcome.stdout.lines, outcome.stderr.lines), (2, 1));
    assert_eq!(
        (
            outcome.stdout.capture.stored_bytes,
            outcome.stderr.capture.stored_bytes
        ),
        (5, 2)
    );
    assert!(outcome.stdout.capture.digest.is_some());
    assert_eq!(outcome.final_reservation, MIB);
    let mut capture = outcome.capture.take().expect("installs after kind 28");
    capture.install();
    let end = Instant::now() + Duration::from_secs(30);
    while !capture.exited {
        capture.drain();
        assert!(Instant::now() < end, "worker did not install");
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// TJ-S1: the leader's exit moves capture to `Draining`; a quota short
/// there reaches the controller as `CapturePaused(Quota)`, the `quota`
/// cancel trigger, and never as a capture fault.
#[test]
fn draining_short_reports_the_quota_trigger() {
    let home = Home::new(MIB);
    let job = asura_platform::random_id();
    let handle = capture_worker::start(home.capture(job)).unwrap();
    handle.leader_exited();
    let mut sink = handle.output_sink();
    let mut slot = Slot::new(SlotPhase::Done, Some(handle));
    slot.leader_exited = true;
    let mut worker = Worker {
        slot: Some(Box::new(slot)),
        ..Worker::default()
    };
    let mut line = vec![b'a'; capture_worker::CHUNK_BYTES - 1];
    line.push(b'\n');
    let mut events = Vec::new();
    let end = Instant::now() + Duration::from_secs(30);
    let mut sent = 0;
    while !events
        .iter()
        .any(|e| matches!(e, SlotEvent::CapturePaused(_)))
    {
        if sent < 17 && sink.room() >= capture_worker::CHUNK_BYTES {
            sink.accept(asura_platform::secrets::Stream::Stdout, &line);
            sent += 1;
        }
        worker.poll_job(Instant::now(), &mut events);
        assert!(Instant::now() < end, "no trigger: {:?}", names(&events));
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(matches!(
        events.as_slice(),
        [SlotEvent::CapturePaused(PauseReason::Quota)]
    ));
    // The controller's trigger: discard; the job then settles.
    worker.discard_capture();
    sink.end(asura_platform::secrets::Stream::Stdout);
    sink.end(asura_platform::secrets::Stream::Stderr);
    let slot = worker.slot.as_mut().unwrap();
    slot.process = SlotProcess::Settled;
    slot.scratch_cleaned = true;
    let mut events = poll_until_idle(&mut worker);
    let outcome = capture_outcome(events.last_mut());
    assert_eq!(outcome.state, CaptureState::Incomplete);
    assert_eq!(outcome.reason, CaptureReason::Quota);
    assert_eq!(outcome.pause_count, 0, "capture never paused in Draining");
    assert_eq!(outcome.stdout.capture.stored_bytes, MIB);
    assert_eq!(outcome.stdout.lines, 16);
    if let Some(capture) = outcome.capture.take() {
        capture.abandon();
    }
}

/// Guardian setup failures (kind 3) and setup stages map to the kind 28
/// `not_started` reasons; a cancelled setup has no reason of its own.
/// J2T-25 unit (J2-A2): kind 3 reasons 5, 6, 7 and 8 give job reasons 15,
/// 14, 13 and 17; reason 9 and no reason give `setup_failed`.
#[test]
fn setup_failures_map_to_not_started_reasons() {
    for (reason, job_reason, code) in [
        (5, JobReason::SpawnFailed, 15),
        (6, JobReason::SandboxUnavailable, 14),
        (7, JobReason::ProjectChanged, 13),
        (8, JobReason::ProtocolFault, 17),
        (9, JobReason::SetupFailed, 18),
        (0, JobReason::SetupFailed, 18),
    ] {
        assert_eq!(guardian_setup_reason(reason), job_reason, "reason {reason}");
        assert_eq!(job_reason as u8, code, "reason {reason}");
    }
    let home = Home::new(64 * MIB);
    let scope = home.scope();
    let failure = |error| Failure {
        error,
        cleanup: None,
    };
    let ended = |stage, error| {
        let ended = setup_ended(stage, failure(error), &scope);
        (ended.reason, ended.identity_changed)
    };
    assert_eq!(
        ended(Stage::Prepare, ExecCmdError::Cancelled),
        (None, false)
    );
    assert_eq!(
        ended(Stage::Identity, ExecCmdError::Denied),
        (Some(JobReason::ProjectChanged), true)
    );
    // The project still matches: a denied cwd is `cwd_unavailable`.
    assert_eq!(
        ended(Stage::Prepare, ExecCmdError::Denied),
        (Some(JobReason::CwdUnavailable), false)
    );
    assert_eq!(
        ended(Stage::Spawn, ExecCmdError::Io),
        (Some(JobReason::SpawnFailed), false)
    );
    assert_eq!(
        ended(Stage::Executable, ExecCmdError::Unavailable),
        (Some(JobReason::SetupFailed), false)
    );
    let mut moved = home.scope();
    moved.inode += 1;
    let ended = setup_ended(Stage::Spawn, failure(ExecCmdError::Denied), &moved);
    assert_eq!(
        (ended.reason, ended.identity_changed),
        (Some(JobReason::ProjectChanged), true)
    );
    assert_eq!(
        exit_of(&ExecCmdResult {
            failure: None,
            exit_code: Some(3),
            signal: None,
            reason: None,
            setup_reason: 0,
            cleanup_confirmed: true,
            escapees: 0,
        }),
        Some(Exit::Exited(3))
    );
}

/// The port's event order (`job_controller/port.rs`): `Spawned`,
/// `SetupFailed` or `IdentityChanged` first; after a spawn `LeaderExited`,
/// at most one `CleanupUnconfirmed`, then `ProcessSettled`; exactly one
/// `CaptureSettled`; then `ScratchCleaned` or `ScratchRetained` last.
/// Pause events may come at any point before `CaptureSettled`.
fn assert_port_order(events: &[SlotEvent]) {
    let names: Vec<&str> = names(events)
        .into_iter()
        .filter(|n| !matches!(*n, "capture_paused" | "capture_resumed" | "capture_fault"))
        .collect();
    let first = names.first().copied().expect("an event");
    assert!(
        matches!(first, "spawned" | "setup_failed" | "identity_changed"),
        "{names:?}"
    );
    let position = |name: &str| names.iter().position(|n| *n == name);
    let capture = position("capture_settled").expect("one capture settlement");
    assert_eq!(names.iter().filter(|n| **n == "capture_settled").count(), 1);
    if first == "spawned" {
        let settled = position("process_settled").expect("process settled");
        assert!(position("leader_exited").is_some_and(|l| l < settled));
        assert!(settled < capture, "{names:?}");
        assert!(
            names
                .iter()
                .filter(|n| **n == "cleanup_unconfirmed")
                .count()
                <= 1
        );
    } else {
        assert!(position("spawned").is_none() && position("process_settled").is_none());
    }
    let last = *names.last().unwrap();
    assert!(
        matches!(last, "scratch_cleaned" | "scratch_retained"),
        "{names:?}"
    );
    assert_eq!(capture, names.len() - 2, "{names:?}");
}

/// Drives `slot` as the controller does, through `dyn SlotPort`, waiting on
/// the reactor wake between polls; returns the events once it is idle.
fn drive(slot: &mut dyn SlotPort, reader: &asura_platform::events::WakeReader) -> Vec<SlotEvent> {
    use std::os::fd::AsRawFd;
    let end = Instant::now() + Duration::from_secs(30);
    let mut events = Vec::new();
    loop {
        let now = Instant::now();
        let before = events.len();
        slot.poll(now, &mut events);
        assert!(events.len() - before <= SLOT_EVENTS);
        if slot.idle() {
            return events;
        }
        assert!(Instant::now() < end, "slot not idle: {:?}", names(&events));
        let mut interests = Vec::new();
        slot.interests(&mut interests);
        interests.push(asura_platform::PollInterest {
            fd: reader.as_raw_fd(),
            read: true,
            write: false,
        });
        let wait = slot
            .next_deadline(now)
            .map_or(Duration::from_millis(100), |d| {
                d.saturating_duration_since(now)
            })
            .min(Duration::from_millis(100));
        let _ = asura_platform::poll(&interests, wait);
        let _ = reader.drain();
    }
}

/// The real slot behind the controller's `SlotPort` and `SlotFactory`: a
/// setup recheck mismatch and a stop before spawn each follow the port's
/// event order, carry a `not_started` capture, and leave the slot idle with
/// its scratch not retained.
#[test]
fn factory_slots_follow_the_port_event_order() {
    let home = Home::new(64 * MIB);
    let (reader, wake) = asura_platform::events::wake_pair().unwrap();
    let mut factory = slot_factory(Some(wake));
    let mut moved = home.scope();
    moved.inode += 1;
    let mut slot = factory();
    assert!(slot.idle());
    slot.start(home.start(asura_platform::random_id(), moved))
        .unwrap();
    assert!(!slot.idle());
    assert_eq!(
        slot.start(home.start(asura_platform::random_id(), home.scope()))
            .err(),
        Some(SlotStartError::Busy)
    );
    let mut events = drive(slot.as_mut(), &reader);
    assert_port_order(&events);
    assert!(matches!(events[0], SlotEvent::IdentityChanged));
    assert!(matches!(events.last(), Some(SlotEvent::ScratchCleaned)));
    let outcome = capture_outcome(events.get_mut(1));
    assert_eq!(outcome.state, CaptureState::NotStarted);
    assert!(outcome.capture.is_some(), "the worker is handed over");
    slot.close_capture();

    #[cfg(feature = "test-support")]
    {
        std::fs::write(home.path.join("exec-cmd-hold-setup"), b"").unwrap();
        let mut slot = factory();
        slot.start(home.start(asura_platform::random_id(), home.scope()))
            .unwrap();
        slot.stop(StopKind::Term);
        slot.stop(StopKind::Kill);
        slot.discard_capture();
        let events = drive(slot.as_mut(), &reader);
        assert_port_order(&events);
        assert!(matches!(events[0], SlotEvent::SetupFailed(JobReason::None)));
    }
}
