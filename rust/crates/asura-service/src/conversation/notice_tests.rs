//! The owner's notice step (J5-E): `Prepared.notice` to the turn's notice,
//! the kind 21 notice fields and the `NoticeError` mapping (J5T-09 unit).
use super::super::{Owner, writer};
use super::*;
use crate::context::{Budget, compose_turn};
use asura_platform::RuntimeDirectory;
use asura_storage::authority::conversation::{
    ArgumentForm, CaptureReason, CaptureStream, JobCaptureState, JobCleanup, JobIntent, JobListing,
    JobOrigin, JobOutcome, JobReason, JobStream, Locator, NoticeSettlement, ToolCall,
    ToolCallSource, ToolDecision, ToolOperation,
};
use sha2::{Digest, Sha256};

const CONVERSATION: Id = [0xc0; 16];

fn call(generation: u64, arguments: &str) -> ToolCall {
    ToolCall {
        operation: [0x11; 16],
        generation,
        ordinal: 1,
        source: ToolCallSource::ProviderJson,
        name: "exec_cmd".into(),
        name_bytes: 8,
        tool_operation: Some(ToolOperation::ExecCmd),
        decision: ToolDecision::Run,
        fault: None,
        reason: None,
        form: ArgumentForm::Canonical,
        arguments_bytes: arguments.len() as u32,
        arguments: arguments.into(),
        number_kinds: vec![],
        unnumbered: 0,
        rule_set_revision: 1,
        rule_set_digest: [0; 32],
        command_cut: false,
        whole_command: false,
        refusal: None,
    }
}
fn arguments(command: &str, cwd: Option<&str>) -> String {
    let mut text = String::new();
    asura_platform::secrets::json::write_canonical_string(&mut text, command);
    match cwd {
        Some(cwd) => {
            let mut quoted = String::new();
            asura_platform::secrets::json::write_canonical_string(&mut quoted, cwd);
            format!("{{\"command\":{text},\"cwd\":{quoted}}}")
        }
        None => format!("{{\"command\":{text}}}"),
    }
}
fn settled(intent: &JobIntent, value: u32) -> JobSettled {
    JobSettled {
        job: journal::job_id(&intent.origin),
        outcome: JobOutcome::Exited,
        reason: JobReason::None,
        exit_kind: 1,
        exit_value: value,
        cleanup: JobCleanup::Confirmed,
        unconfirmed_reason: 0,
        escapees: 0,
        elapsed_ms: 10,
        capture_state: JobCaptureState::Complete,
        capture_reason: CaptureReason::None,
        stdout: JobStream {
            capture: CaptureStream {
                observed_bytes: 3,
                stored_bytes: 3,
                digest: Some([0xab; 32]),
                eof: true,
                ..CaptureStream::default()
            },
            lines: 1,
        },
        stderr: JobStream {
            capture: CaptureStream {
                digest: Some([0xcd; 32]),
                eof: true,
                ..CaptureStream::default()
            },
            lines: 0,
        },
        distinct_numbers: 0,
        numbers: Vec::new(),
        pause_count: 0,
        first_pause_ms: 0,
        paused_ms: 0,
        final_reservation: 1 << 20,
        settled_time: 1_700_000_000_000,
    }
}
/// A model job of generation `n` settled at frame `sequence`.
fn settlement(n: u64, sequence: u64, arguments: &str) -> NoticeSettlement {
    let intent = JobIntent {
        origin: JobOrigin::Model {
            operation: [0x11; 16],
            generation: n,
            ordinal: 1,
        },
        agent: 0,
        time: 1_700_000_000_000,
    };
    let job = journal::job_id(&intent.origin);
    NoticeSettlement {
        listing: JobListing {
            job,
            conversation: CONVERSATION,
            project: [0xa0; 16],
            intent,
            intent_frame: Locator {
                sequence: sequence - 2,
                ..Locator::NONE
            },
            cancel: None,
            cancel_frame: Locator::NONE,
            settled: Some(Box::new(settled(&intent, n as u32))),
            settled_frame: Locator {
                sequence,
                ..Locator::NONE
            },
            command: Locator::NONE,
            excerpt: String::new(),
            retention: Default::default(),
        },
        command: Box::new(call(n, arguments)),
    }
}
fn inputs(settlements: Vec<NoticeSettlement>, live: u32) -> JobNotice {
    JobNotice {
        after: 4,
        through: 40,
        settlements,
        live,
    }
}
/// The command digest of a kind 28 frame computed from the frame's own
/// payload bytes: SHA-256 of the two-byte kind 28 and the payload, which
/// sits between the 144-byte header and the 40-byte trailer.
fn payload_digest(settled: &JobSettled) -> Hash {
    let frame = journal::encode_frame(
        &journal::FrameContext {
            installation_id: [7; 16],
            transition_id: [8; 16],
            sequence: 99,
            prior_digest: [9; 32],
            expected_revision: 5,
            owner_generation: 2,
        },
        &journal::Record::JobSettled(Box::new(settled.clone())),
    )
    .unwrap();
    let mut h = Sha256::new();
    h.update(28u16.to_be_bytes());
    h.update(&frame[144..frame.len() - 40]);
    h.finalize().into()
}

/// The owner reads command and cwd from the kind 14 and orders entries by
/// kind 28 sequence; each entry binds its kind 28 command digest.
#[test]
fn prepare_inputs_become_the_notice() {
    let newest = settlement(2, 30, &arguments("make\nmake check", Some("src/ui")));
    let oldest = settlement(1, 20, &arguments("cargo test", None));
    let digests = [
        payload_digest(oldest.listing.settled.as_deref().unwrap()),
        payload_digest(newest.listing.settled.as_deref().unwrap()),
    ];
    // `Prepare` returns the newest first.
    let notice = build(CONVERSATION, &inputs(vec![newest, oldest], 1)).unwrap();
    assert_eq!(notice.through, 40);
    assert_eq!(notice.live, 1);
    assert!(!notice.overflow);
    let sequences: Vec<u64> = notice.entries.iter().map(|e| e.sequence).collect();
    assert_eq!(sequences, [20, 30]);
    assert_eq!(
        notice.entries.iter().map(|e| e.digest).collect::<Vec<_>>(),
        digests,
        "the source digest is the kind 28 command digest"
    );
    assert!(
        notice.entries[0]
            .full
            .contains("command: cargo test\ndirectory: .\nresult: exit 1\n"),
        "{}",
        notice.entries[0].full
    );
    // A multi-line command uses the quoted value form (J5-E6).
    assert!(
        notice.entries[1]
            .full
            .contains("command: make\n  | make check\ndirectory: src/ui\n"),
        "{}",
        notice.entries[1].full
    );
}

/// Change 3: the composer's record carries `notice_through` = S and
/// `notice_live` from the notice; a turn without inputs records S and 0.
#[test]
fn composition_records_the_cursor_and_the_live_count() {
    let budget = Budget {
        window_tokens: 16384,
        output_reservation: 4096,
        overhead_tokens: 0,
        bytes_per_token_milli: 1000,
    };
    let notice = build(
        CONVERSATION,
        &inputs(vec![settlement(1, 20, &arguments("true", None))], 3),
    )
    .unwrap();
    let composed = compose_turn("rules", "now?", &[], 0, &notice, budget).unwrap();
    let composition = composed.composition(budget);
    assert_eq!(composition.notice_through, 40);
    assert_eq!(composition.notice_live, 3);

    let empty = build(CONVERSATION, &inputs(Vec::new(), 0)).unwrap();
    assert!(empty.is_empty());
    let composed = compose_turn("rules", "now?", &[], 0, &empty, budget).unwrap();
    let composition = composed.composition(budget);
    assert_eq!(composition.notice_through, 40);
    assert_eq!(composition.notice_live, 0);
}

/// J5T-09 (unit): each `NoticeError` variant from crafted `Prepare` inputs.
#[test]
fn j5t_09_each_notice_error_from_crafted_inputs() {
    let fine = || settlement(1, 20, &arguments("true", None));
    // Range: S below P.
    let mut range = inputs(vec![fine()], 0);
    range.after = 41;
    assert_eq!(build(CONVERSATION, &range).err(), Some(NoticeError::Range));
    // Job: a kind 28 that names another job, a listing without its kind
    // 28, and a listing of another conversation.
    let mut other_job = fine();
    other_job.listing.settled.as_mut().unwrap().job = [0; 16];
    let mut unsettled = fine();
    unsettled.listing.settled = None;
    let mut elsewhere = fine();
    elsewhere.listing.conversation = [0xc1; 16];
    for case in [other_job, unsettled, elsewhere] {
        assert_eq!(
            build(CONVERSATION, &inputs(vec![case], 0)).err(),
            Some(NoticeError::Job)
        );
    }
    // Duplicate: two settlements at one sequence, and one job twice.
    let twice = vec![fine(), settlement(2, 20, &arguments("true", None))];
    assert_eq!(
        build(CONVERSATION, &inputs(twice, 0)).err(),
        Some(NoticeError::Duplicate)
    );
    let same_job = vec![fine(), settlement(1, 25, &arguments("true", None))];
    assert_eq!(
        build(CONVERSATION, &inputs(same_job, 0)).err(),
        Some(NoticeError::Duplicate)
    );
    // Command: over 8,192 stored bytes, and a source with no command.
    let long = settlement(1, 20, &arguments(&"x".repeat(8193), None));
    let mut no_command = fine();
    no_command.command.tool_operation = Some(ToolOperation::JobList);
    for case in [long, no_command] {
        assert_eq!(
            build(CONVERSATION, &inputs(vec![case], 0)).err(),
            Some(NoticeError::Command)
        );
    }
    // A command of exactly 8,192 bytes is accepted.
    let limit = settlement(1, 20, &arguments(&"x".repeat(8192), None));
    assert!(build(CONVERSATION, &inputs(vec![limit], 0)).is_ok());
}

/// J5T-09 (unit, J5-E10): a `NoticeError` fails the preparation with
/// `authority_repair_required` before the helper starts; consistent inputs
/// pass the notice step (and then stop at the missing model package).
#[test]
fn j5t_09_notice_error_fails_the_turn_as_a_repair_case() {
    let _serial = crate::TEST_WRITER.lock().unwrap_or_else(|p| p.into_inner());
    let path = std::path::PathBuf::from(format!(
        "/private/tmp/asura-notice-owner-{:x}",
        u128::from_ne_bytes(asura_platform::random_id())
    ));
    std::os::unix::fs::DirBuilderExt::mode(&mut std::fs::DirBuilder::new(), 0o700)
        .create(&path)
        .unwrap();
    struct Scratch(std::path::PathBuf);
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    let scratch = Scratch(path);
    let runtime = RuntimeDirectory::scratch(&scratch.0, true).unwrap();
    let mut owner = Owner::new(runtime, None);
    let queued = || writer::QueuedPreparation {
        request: [1; 16],
        project: [0xa0; 16],
        conversation: CONVERSATION,
        generation: 1,
        prompt: "next".into(),
    };
    let prepared = |notice: JobNotice| writer::Prepared {
        model: "system".into(),
        asset_root: String::new(),
        ollama_endpoint: None,
        model_capabilities: None,
        thinking: 0,
        configuration_digest: [0; 32],
        history: Vec::new(),
        omitted: 0,
        notice,
    };
    let mut bad = inputs(vec![settlement(1, 20, &arguments("true", None))], 0);
    bad.after = 41;
    assert_eq!(
        owner.prepared(queued(), prepared(bad)).err(),
        Some("authority_repair_required")
    );
    assert!(owner.active.is_none(), "no turn starts; no kind 21");
    let good = inputs(vec![settlement(1, 20, &arguments("true", None))], 0);
    assert_eq!(
        owner.prepared(queued(), prepared(good)).err(),
        Some("model_package_unavailable")
    );
}
