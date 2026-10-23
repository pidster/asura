use super::*;
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
            person: None,
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

/// J3T-22 slot part (TJ-Q10): every step clears the capture's stored
/// signal first, even when the slot's event queue is full; the signal adds
/// no slot event.
#[test]
fn j3t_22_step_clears_the_stored_signal_with_a_full_queue() {
    let home = Home::new(64 * MIB);
    let job = asura_platform::random_id();
    let handle = capture_worker::start(home.capture(job)).unwrap();
    let mut sink = handle.output_sink();
    let mut slot = Slot::new(SlotPhase::Done, Some(handle));
    for _ in 0..SLOT_EVENTS {
        slot.events.push_back(SlotEvent::CaptureResumed);
    }
    sink.accept(asura_platform::secrets::Stream::Stdout, b"stored\n");
    let end = Instant::now() + Duration::from_secs(30);
    while !slot.capture.as_ref().unwrap().stored_pending() {
        assert!(Instant::now() < end, "no stored signal");
        std::thread::sleep(Duration::from_millis(1));
    }
    slot.step(Instant::now(), None);
    let capture = slot.capture.as_ref().unwrap();
    assert!(!capture.stored_pending(), "cleared with a full queue");
    assert!(!capture.take_stored());
    assert_eq!(slot.events.len(), SLOT_EVENTS, "the signal adds no event");
    assert!(
        slot.events
            .iter()
            .all(|e| matches!(e, SlotEvent::CaptureResumed))
    );
    slot.capture.take().unwrap().abandon();
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

// ------------------------------------------------------------ person jobs
//
// P3-T worker layer (person-command-mode.md, outside survivors and lane
// release). The unit test binary is not a guardian, so these cases run a
// scripted guardian peer through the test-support replacement hook: it
// reads the FD 11 frame and checks FDs 12 onwards, then speaks the status
// notice protocol of a person profile (kinds 1, 5, 6 and 2) while a real
// survivor process holds the stdout pipe. The slot, capture worker, payload
// store, pipes and survivor are real. The real guardian's kind 6 and
// reason 14 and 15 decisions are qualified by P1 at the platform layer
// (`exec_cmd_process.rs`, EC-I9, OD2).

/// The scripted guardian peer. Argv is the guardian's: flag, job hex,
/// profile token, command; the command is `MODE DIR`. It never signals
/// any process. Survivors keep only FD 5, ignore SIGPIPE, record their pid
/// and how they ended, and live at most 30 s.
const PERSON_GUARDIAN: &str = r#"#!/usr/bin/perl
use strict; use warnings; use POSIX ();
my (undef, $hex, $profile, $command) = @ARGV;
my ($mode, $dir) = split(/ /, $command, 2);
$SIG{PIPE} = 'IGNORE';
sub record { my ($name, $text) = @_; open(my $f, '>', "$dir/$name.tmp") or POSIX::_exit(3); print $f $text; close $f; rename("$dir/$name.tmp", "$dir/$name") or POSIX::_exit(3); }
$SIG{__DIE__} = sub { record('guardian.err', $_[0]); POSIX::_exit(2); };
my $id = pack('H32', $hex);
open(my $frame, '<&=', 11) or die "fd 11: $!";
binmode $frame;
my $bytes = '';
while (1) { my $n = sysread($frame, my $buf, 65536); die "frame read: $!" unless defined $n; last if $n == 0; $bytes .= $buf; }
close $frame;
my $off = 0;
my $len = unpack('N', substr($bytes, $off, 4)); $off += 4;
my $roots = unpack('C', substr($bytes, $off, 1)); $off += 1;
my @roles = unpack('C*', substr($bytes, $off, $roots)); $off += $roots;
my $count = unpack('n', substr($bytes, $off, 2)); $off += 2;
my @names;
for (1 .. $count) {
  my $n = unpack('n', substr($bytes, $off, 2)); $off += 2;
  push @names, substr($bytes, $off, $n); $off += $n;
  my $v = unpack('N', substr($bytes, $off, 4)); $off += 4 + $v;
}
my @kinds = map { -d "/dev/fd/" . (12 + $_) ? 'dir' : 'other' } 0 .. $#roles;
record('frame', join("\n", 'length ' . ($len == length($bytes) - 4 && $off == length($bytes) ? 'ok' : 'bad'),
  "profile $profile", "roles @roles", "roots @kinds", @names) . "\n");
open(my $status, '>&=', 4) or die "fd 4: $!";
sub notice {
  my ($kind, $reason, $exit_kind, $cleanup, $value) = @_;
  my $b = 'ASSH' . pack('CCCC', $kind, $reason, $exit_kind, $cleanup) . $id . pack('N', $value) . "\0\0\0\0";
  syswrite($status, $b) == 32 or die "notice: $!";
}
notice(1, 0, 0, 0, 0);
if ($mode eq 'none') { notice(5, 0, 1, 0, 0); notice(2, 0, 1, 1, 0); POSIX::_exit(0); }
my $pid = fork(); die "fork" unless defined $pid;
if ($pid == 0) {
  POSIX::setsid();
  POSIX::close($_) for (3, 4, 6, 7, 8, 9, 10);
  open(my $out, '>&=', 5) or POSIX::_exit(4);
  $out->autoflush(1);
  record('survivor.pid', $$);
  my $end = time + 30;
  if ($mode eq 'left') { close $out; POSIX::close(5); select(undef, undef, undef, 0.3); record('done', 'left'); POSIX::_exit(0); }
  my $limit = $mode eq 'hold' ? 5 : 1_000_000;
  for my $i (0 .. $limit - 1) {
    my $n = syswrite($out, "tick $i\n");
    if (!defined $n) { record('epipe', $!{EPIPE} ? 'EPIPE' : "errno $!"); POSIX::_exit(0); }
    last if time > $end || -e "$dir/release";
    select(undef, undef, undef, $mode eq 'hold' ? 0.2 : 0.05);
  }
  record('done', 'ended');
  POSIX::_exit(0);
}
POSIX::close(5); POSIX::close(6);
my $wait = time + 10;
select(undef, undef, undef, 0.01) until -e "$dir/survivor.pid" || time > $wait;
die "no survivor" unless -e "$dir/survivor.pid";
notice(5, 0, 1, 0, 0);
select(undef, undef, undef, 0.5) if $mode eq 'late';
notice(6, 0, 0, 0, 1);
notice(2, $mode eq 'left' ? 14 : 15, 1, 1, 0);
POSIX::_exit(0);
"#;

/// A home whose guardian is the scripted peer, with a marks directory.
fn person_home() -> (Home, std::path::PathBuf) {
    use std::os::unix::fs::PermissionsExt;
    let home = Home::new(64 * MIB);
    let script = home.path.join("person-guardian");
    std::fs::write(&script, PERSON_GUARDIAN).unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::write(
        home.path.join("exec-cmd-guardian-executable"),
        script.to_str().unwrap(),
    )
    .unwrap();
    let marks = home.path.join("marks");
    std::fs::create_dir(&marks).unwrap();
    (home, marks)
}
fn launch_outside() -> PersonLaunch {
    PersonLaunch::Outside {
        launch: vec![
            (b"PATH".to_vec(), b"/usr/bin:/bin".to_vec()),
            (b"FIXTURE_TOKEN".to_vec(), b"launch-value".to_vec()),
        ],
    }
}
fn person_start(home: &Home, profile: Profile, mode: &str, marks: &std::path::Path) -> SlotStart {
    let mut start = home.start(asura_platform::random_id(), home.scope());
    start.profile = profile;
    start.command = format!("{mode} {}", marks.display());
    start
}
/// Polls until `done` holds for the events so far (30 s test guard).
fn poll_until(
    worker: &mut Worker,
    events: &mut Vec<SlotEvent>,
    done: impl Fn(&[SlotEvent]) -> bool,
) {
    let end = Instant::now() + Duration::from_secs(30);
    while !done(events) {
        worker.poll_job(Instant::now(), events);
        assert!(Instant::now() < end, "slot events: {:?}", names(events));
        std::thread::sleep(Duration::from_millis(2));
    }
}
fn released(events: &[SlotEvent]) -> bool {
    events.iter().any(|e| {
        matches!(
            e,
            SlotEvent::ProcessSettled {
                output_held: true,
                ..
            }
        )
    })
}
fn wait_file(path: &std::path::Path) -> String {
    let end = Instant::now() + Duration::from_secs(30);
    loop {
        if let Ok(text) = std::fs::read_to_string(path) {
            return text;
        }
        assert!(Instant::now() < end, "no {}", path.display());
        std::thread::sleep(Duration::from_millis(5));
    }
}
fn survivor(marks: &std::path::Path) -> i32 {
    wait_file(&marks.join("survivor.pid"))
        .trim()
        .parse()
        .unwrap()
}
fn alive(pid: i32) -> bool {
    // SAFETY: signal 0 only checks that the process exists.
    unsafe { libc::kill(pid, 0) == 0 }
}
fn stored_stdout(worker: &Worker) -> u64 {
    let slot = worker.slot.as_ref().unwrap();
    slot.capture.as_ref().unwrap().sink.counts().0[0]
}

/// The person start checks its inputs before any thread starts: a person
/// profile needs its `PersonLaunch`, of the same profile, and `model-1`
/// takes none. The launch values are zeroed when dropped.
#[test]
fn person_start_requires_matching_person_inputs() {
    let home = Home::new(64 * MIB);
    let person = |profile| {
        let mut start = home.start(asura_platform::random_id(), home.scope());
        start.profile = profile;
        start
    };
    let sandboxed = || PersonLaunch::Sandboxed {
        path: b"/usr/bin:/bin".to_vec(),
        client: Vec::new(),
    };
    let mut worker = Worker::default();
    for (start, launch) in [
        (person(Profile::Person1), None),
        (person(Profile::PersonOutside1), None),
        (person(Profile::Person1), Some(launch_outside())),
        (person(Profile::PersonOutside1), Some(sandboxed())),
        (person(Profile::Model1), Some(sandboxed())),
    ] {
        assert_eq!(
            worker.start_job_with(start, launch).err(),
            Some(SlotStartError::Invalid)
        );
        assert!(worker.idle());
    }
    assert_eq!(
        SlotPort::start(&mut worker, person(Profile::PersonOutside1)).err(),
        Some(SlotStartError::Invalid),
        "the port start has no person inputs yet"
    );
    assert_eq!(launch_outside().profile(), Profile::PersonOutside1);
    assert_eq!(sandboxed().profile(), Profile::Person1);
}

/// Person setup failures settle `not_started` before any spawn: a missing
/// home is `sandbox_unavailable` (role 1 is required), and an input that
/// fails the platform's form check is `setup_failed`.
#[cfg(feature = "test-support")]
#[test]
fn person_setup_failures_map_to_not_started_reasons() {
    let (home, marks) = person_home();
    std::fs::write(
        home.path.join("exec-cmd-person-account"),
        format!(
            "{}\n{}\n",
            home.path.join("missing-home").display(),
            home.path.display()
        ),
    )
    .unwrap();
    let mut worker = Worker::default();
    let launch = PersonLaunch::Sandboxed {
        path: b"/usr/bin:/bin".to_vec(),
        client: Vec::new(),
    };
    worker
        .start_job_with(
            person_start(&home, Profile::Person1, "none", &marks),
            Some(launch),
        )
        .unwrap();
    let events = poll_until_idle(&mut worker);
    assert_eq!(
        names(&events),
        ["setup_failed", "capture_settled", "scratch_cleaned"]
    );
    assert!(matches!(
        events[0],
        SlotEvent::SetupFailed(JobReason::SandboxUnavailable)
    ));
    let bad = PersonLaunch::Outside {
        launch: vec![(b"BAD=NAME".to_vec(), b"x".to_vec())],
    };
    worker
        .start_job_with(
            person_start(&home, Profile::PersonOutside1, "none", &marks),
            Some(bad),
        )
        .unwrap();
    let events = poll_until_idle(&mut worker);
    assert!(matches!(
        events[0],
        SlotEvent::SetupFailed(JobReason::SetupFailed)
    ));
    assert!(!marks.join("frame").exists(), "no guardian ran");
}

/// Person start with root resolution and the FD 11 writer: a `!` job's
/// setup resolves the fixture home and `$TMPDIR` into root descriptors 12
/// and 13 (roles 1 and 2) and the setup thread writes the frame, which the
/// guardian peer reads whole: the PC2 and PC3 names in order, and the
/// client locale. A `!!` job's frame carries the launch names and no
/// roots. Both then settle as ordinary jobs.
#[cfg(feature = "test-support")]
#[test]
fn person_start_sends_the_frame_and_root_descriptors() {
    let (home, marks) = person_home();
    let account = home.path.join("account");
    let tmpdir = home.path.join("account-tmp");
    std::fs::create_dir(&account).unwrap();
    std::fs::create_dir(&tmpdir).unwrap();
    std::fs::write(
        home.path.join("exec-cmd-person-account"),
        format!("{}\n{}\n", account.display(), tmpdir.display()),
    )
    .unwrap();
    let mut worker = Worker::default();
    let launch = PersonLaunch::Sandboxed {
        path: b"/usr/bin:/bin".to_vec(),
        client: vec![(b"LANG".to_vec(), b"en_GB.UTF-8".to_vec())],
    };
    worker
        .start_job_with(
            person_start(&home, Profile::Person1, "none", &marks),
            Some(launch),
        )
        .unwrap();
    let mut events = poll_until_idle(&mut worker);
    assert_port_order(&events);
    assert_eq!(
        names(&events),
        [
            "spawned",
            "leader_exited",
            "process_settled",
            "capture_settled",
            "scratch_cleaned"
        ]
    );
    let frame = wait_file(&marks.join("frame"));
    assert_eq!(
        frame,
        "length ok\nprofile person-1\nroles 1 2\nroots dir dir\nPATH\nHOME\nTMPDIR\nUSER\nLOGNAME\nTERM\nGIT_TERMINAL_PROMPT\nLANG\n"
    );
    let outcome = capture_outcome(events.get_mut(3));
    assert_eq!(outcome.state, CaptureState::Complete);
    outcome.capture.take().unwrap().abandon();

    std::fs::remove_file(marks.join("frame")).unwrap();
    worker
        .start_job_with(
            person_start(&home, Profile::PersonOutside1, "none", &marks),
            Some(launch_outside()),
        )
        .unwrap();
    let mut events = poll_until_idle(&mut worker);
    assert_port_order(&events);
    assert_eq!(
        wait_file(&marks.join("frame")),
        "length ok\nprofile person-outside-1\nroles \nroots \nPATH\nFIXTURE_TOKEN\n"
    );
    capture_outcome(events.get_mut(3))
        .capture
        .take()
        .unwrap()
        .abandon();
}

/// Kind 2 reason 14: the `!!` survivor holds no capture pipe, so the job
/// settles as usual with the survivor count; `output_held` is false and
/// the survivor is left running, never signalled (EC-D1).
#[cfg(feature = "test-support")]
#[test]
fn reason_14_settles_with_survivors_left_running() {
    let (home, marks) = person_home();
    let mut worker = Worker::default();
    worker
        .start_job_with(
            person_start(&home, Profile::PersonOutside1, "left", &marks),
            Some(launch_outside()),
        )
        .unwrap();
    let mut events = poll_until_idle(&mut worker);
    assert_port_order(&events);
    let settled: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            SlotEvent::ProcessSettled {
                exit,
                escapees,
                output_held,
                ..
            } => Some((*exit, *escapees, *output_held)),
            _ => None,
        })
        .collect();
    assert_eq!(settled, [(Exit::Exited(0), 1, false)]);
    let outcome = capture_outcome(events.iter_mut().find(|e| name(e) == "capture_settled"));
    assert_eq!(outcome.state, CaptureState::Complete);
    outcome.capture.take().unwrap().abandon();
    assert_eq!(wait_file(&marks.join("done")), "left", "ended by itself");
}

/// PC-I33 worker layer: a `!!` survivor in a new session keeps stdout and
/// prints `tick 0` to `tick 4` after the shell exited. Kind 6 count 1 and
/// kind 2 reason 15 give `ProcessSettled { output_held: true }` while
/// capture is open and the slot stays held; the survivor's later lines are
/// retained; its EOF then settles capture `complete` with all five lines,
/// and the scratch is cleaned. The survivor was never signalled.
#[cfg(feature = "test-support")]
#[test]
fn pc_i33_survivor_output_is_captured_after_release() {
    use sha2::{Digest, Sha256};
    let (home, marks) = person_home();
    let mut worker = Worker::default();
    worker
        .start_job_with(
            person_start(&home, Profile::PersonOutside1, "hold", &marks),
            Some(launch_outside()),
        )
        .unwrap();
    let mut events = Vec::new();
    poll_until(&mut worker, &mut events, released);
    assert_eq!(
        names(&events),
        ["spawned", "leader_exited", "process_settled"]
    );
    assert!(matches!(
        events[2],
        SlotEvent::ProcessSettled {
            exit: Exit::Exited(0),
            escapees: 1,
            output_held: true,
            cleanup: JobCleanup::Confirmed,
            ..
        }
    ));
    assert!(!worker.idle(), "the slot stays held while capture is open");
    let pid = survivor(&marks);
    assert!(alive(pid), "PQ3: not stopped at shell exit");
    let mut rest = poll_until_idle(&mut worker);
    events.append(&mut rest);
    assert_port_order(&events);
    assert_eq!(
        names(&events),
        [
            "spawned",
            "leader_exited",
            "process_settled",
            "capture_settled",
            "scratch_cleaned"
        ]
    );
    let expected = b"tick 0\ntick 1\ntick 2\ntick 3\ntick 4\n";
    let outcome = capture_outcome(events.get_mut(3));
    assert_eq!(outcome.state, CaptureState::Complete);
    assert_eq!(outcome.reason, CaptureReason::None);
    assert_eq!(outcome.stdout.lines, 5);
    assert_eq!(outcome.stdout.capture.stored_bytes, expected.len() as u64);
    let digest: [u8; 32] = Sha256::digest(expected).into();
    assert_eq!(outcome.stdout.capture.digest, Some(digest));
    outcome.capture.take().unwrap().abandon();
    assert_eq!(wait_file(&marks.join("done")), "ended", "never signalled");
    let end = Instant::now() + Duration::from_secs(10);
    while alive(pid) {
        assert!(Instant::now() < end, "survivor did not end by itself");
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(scratch_left(&home).is_empty(), "scratch deleted");
}
/// PC-I34 worker layer: a survivor that writes forever; the controller's
/// trigger on the released job calls `close_capture`. Capture seals
/// `incomplete` with reason 7 `closed_after_cancel` and a digest of the
/// stored bytes; the scratch is cleaned and the slot released. The survivor
/// is still alive after the close, never signalled, and its next write
/// fails with EPIPE, which it records.
#[cfg(feature = "test-support")]
#[test]
fn pc_i34_close_capture_seals_reason_7_and_leaves_the_survivor() {
    let (home, marks) = person_home();
    let mut worker = Worker::default();
    worker
        .start_job_with(
            person_start(&home, Profile::PersonOutside1, "forever", &marks),
            Some(launch_outside()),
        )
        .unwrap();
    let mut events = Vec::new();
    poll_until(&mut worker, &mut events, released);
    let pid = survivor(&marks);
    let end = Instant::now() + Duration::from_secs(30);
    while stored_stdout(&worker) < 28 {
        worker.poll_job(Instant::now(), &mut events);
        assert!(Instant::now() < end, "no survivor output");
        std::thread::sleep(Duration::from_millis(2));
    }
    assert_eq!(
        names(&events),
        ["spawned", "leader_exited", "process_settled"]
    );
    SlotPort::close_capture(&mut worker);
    SlotPort::close_capture(&mut worker);
    assert!(alive(pid), "EC-D1: close_capture signals nothing");
    let mut rest = poll_until_idle(&mut worker);
    events.append(&mut rest);
    assert_port_order(&events);
    assert_eq!(
        names(&events),
        [
            "spawned",
            "leader_exited",
            "process_settled",
            "capture_settled",
            "scratch_cleaned"
        ]
    );
    let outcome = capture_outcome(events.get_mut(3));
    assert_eq!(outcome.state, CaptureState::Incomplete);
    assert_eq!(outcome.reason, CaptureReason::ClosedAfterCancel);
    assert!(outcome.stdout.capture.stored_bytes >= 28);
    assert!(outcome.stdout.capture.digest.is_some());
    assert!(!outcome.stdout.capture.eof, "closed before EOF");
    outcome.capture.take().unwrap().abandon();
    assert_eq!(wait_file(&marks.join("epipe")), "EPIPE");
    assert!(scratch_left(&home).is_empty(), "scratch deleted");
    SlotPort::close_capture(&mut worker);
    assert!(worker.idle());
}

/// Step 5 of the survivor rules: a trigger that arrives after kind 5 but
/// before the reason 15 notice closes capture when that notice arrives.
#[cfg(feature = "test-support")]
#[test]
fn close_capture_before_reason_15_applies_at_release() {
    let (home, marks) = person_home();
    let mut worker = Worker::default();
    worker
        .start_job_with(
            person_start(&home, Profile::PersonOutside1, "late", &marks),
            Some(launch_outside()),
        )
        .unwrap();
    let mut events = Vec::new();
    poll_until(&mut worker, &mut events, |e| {
        e.iter().any(|e| matches!(e, SlotEvent::LeaderExited(_)))
    });
    assert!(!released(&events), "the peer waits 0.5 s before kind 6");
    SlotPort::close_capture(&mut worker);
    let pid = survivor(&marks);
    let mut rest = poll_until_idle(&mut worker);
    events.append(&mut rest);
    assert_port_order(&events);
    assert!(released(&events));
    let outcome = capture_outcome(events.iter_mut().find(|e| name(e) == "capture_settled"));
    assert_eq!(outcome.state, CaptureState::Incomplete);
    assert_eq!(outcome.reason, CaptureReason::ClosedAfterCancel);
    outcome.capture.take().unwrap().abandon();
    assert_eq!(wait_file(&marks.join("epipe")), "EPIPE");
    let end = Instant::now() + Duration::from_secs(10);
    while alive(pid) {
        assert!(Instant::now() < end, "survivor did not end by itself");
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// PC-I35 worker layer: the service ends while a released job drains. The
/// slot and its capture worker are dropped with no settlement (restart
/// recovery writes `interrupted`; P3-E). Dropping signals nothing: the
/// survivor is alive afterwards and its next write fails with EPIPE.
#[cfg(feature = "test-support")]
#[test]
fn pc_i35_service_end_while_released_does_not_signal_the_survivor() {
    let (home, marks) = person_home();
    let mut worker = Worker::default();
    worker
        .start_job_with(
            person_start(&home, Profile::PersonOutside1, "forever", &marks),
            Some(launch_outside()),
        )
        .unwrap();
    let mut events = Vec::new();
    poll_until(&mut worker, &mut events, released);
    let pid = survivor(&marks);
    drop(worker);
    assert!(alive(pid), "the survivor is not signalled");
    assert_eq!(wait_file(&marks.join("epipe")), "EPIPE");
    std::fs::write(marks.join("release"), b"").unwrap();
}

/// Capture reason 7 at the capture worker: closed read ends end the
/// producer without EOF; the seal is `incomplete`, reason 7, with digests,
/// also when no byte arrived (a process ran, so never `not_started`). A
/// fault names its own cause first.
#[test]
fn capture_seals_reason_7_after_close_output() {
    let home = Home::new(64 * MIB);
    for bytes in [&b""[..], b"tick 0\n"] {
        let job = asura_platform::random_id();
        let mut handle = capture_worker::start(home.capture(job)).unwrap();
        handle.leader_exited();
        let mut sink = handle.output_sink();
        if !bytes.is_empty() {
            sink.accept(asura_platform::secrets::Stream::Stdout, bytes);
        }
        handle.close_output();
        handle.sink.flush();
        handle.sink.close();
        let end = Instant::now() + Duration::from_secs(30);
        let report = loop {
            if let Some(report) = handle.drain().into_iter().find_map(|m| match m {
                Message::Sealed(report) => Some(report),
                _ => None,
            }) {
                break report;
            }
            assert!(Instant::now() < end, "no sealed report");
            std::thread::sleep(Duration::from_millis(2));
        };
        assert_eq!(report.state, CaptureState::Incomplete);
        assert_eq!(report.reason, CaptureReason::ClosedAfterCancel);
        assert_eq!(report.streams[0].stored, bytes.len() as u64);
        assert!(report.streams.iter().all(|s| s.digest.is_some() && !s.eof));
        handle.abandon();
    }
    // Both pipes at EOF before the close took effect: complete.
    let job = asura_platform::random_id();
    let mut handle = capture_worker::start(home.capture(job)).unwrap();
    let mut sink = handle.output_sink();
    sink.end(asura_platform::secrets::Stream::Stdout);
    sink.end(asura_platform::secrets::Stream::Stderr);
    handle.close_output();
    let end = Instant::now() + Duration::from_secs(30);
    let report = loop {
        if let Some(report) = handle.drain().into_iter().find_map(|m| match m {
            Message::Sealed(report) => Some(report),
            _ => None,
        }) {
            break report;
        }
        assert!(Instant::now() < end, "no sealed report");
        std::thread::sleep(Duration::from_millis(2));
    };
    assert_eq!(
        (report.state, report.reason),
        (CaptureState::Complete, CaptureReason::None)
    );
    handle.abandon();
}
