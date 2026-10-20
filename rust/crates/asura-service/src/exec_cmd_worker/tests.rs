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
        cleanup_confirmed: true,
    })
}
fn finalizing(cleanup_deadline: Option<Instant>, stop: Option<StopReason>) -> Worker {
    Worker {
        wake: None,
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
        cleanup_confirmed: true,
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
        cleanup_confirmed: true,
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
