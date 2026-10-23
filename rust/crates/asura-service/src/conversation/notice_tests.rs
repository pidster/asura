//! The owner's notice step (J5-E): `Prepared.notice` to the turn's notice,
//! the kind 21 notice fields and the `NoticeError` mapping (J5T-09 unit).
use super::super::{Owner, writer};
use super::*;
use crate::context::{Budget, compose_turn};
use asura_platform::RuntimeDirectory;
use asura_storage::authority::conversation::{
    ArgumentForm, CaptureReason, CaptureStream, JobCaptureState, JobCleanup, JobIntent, JobListing,
    JobOrigin, JobOutcome, JobReason, JobSettled, JobStream, Locator, NoticeSettlement,
    PriorNotice, PriorSettlement, ToolCall, ToolCallSource, ToolDecision, ToolOperation,
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
            settled_digest: Some(payload_digest(&settled(&intent, n as u32))),
            command: Locator::NONE,
            excerpt: String::new(),
            retention: Default::default(),
        },
        command: asura_storage::authority::conversation::JobCommand::Model(Box::new(call(
            n, arguments,
        ))),
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

/// A settlement read without its kind 28 header digest is `Job` (J5-E10):
/// the owner never derives the digest itself.
#[test]
fn a_settlement_without_its_header_digest_is_a_job_error() {
    let mut missing = settlement(1, 20, &arguments("cargo test", None));
    missing.listing.settled_digest = None;
    assert_eq!(
        build(CONVERSATION, &inputs(vec![missing], 0)).err(),
        Some(NoticeError::Job)
    );
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
    let asura_storage::authority::conversation::JobCommand::Model(no_call) =
        &mut no_command.command
    else {
        unreachable!("model command")
    };
    no_call.tool_operation = Some(ToolOperation::JobList);
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

/// The writer's reference facts for a binding-point read (the prior
/// notice read returns these, with no command source).
fn prior_inputs(notice: &JobNotice) -> PriorNotice {
    PriorNotice {
        after: notice.after,
        through: notice.through,
        live: notice.live,
        settlements: notice
            .settlements
            .iter()
            .map(|s| PriorSettlement {
                job: s.listing.job,
                intent: s.listing.intent,
                settled: *s.listing.settled.clone().unwrap(),
                digest: s.listing.settled_digest.unwrap(),
                sequence: s.listing.settled_frame.sequence,
                outside_sandbox: s.command.outside_sandbox(),
            })
            .collect(),
    }
}
/// One completed prior turn of generation `generation` whose kind 21
/// recorded `notice_through` = `through` and `notice_live` = `live`.
fn prior(
    generation: u64,
    through: u64,
    live: u32,
) -> (journal::TurnAccepted, journal::TurnTerminal) {
    let operation = [generation as u8; 16];
    (
        journal::TurnAccepted {
            request: [0x21; 16],
            digest: [0; 32],
            project: [0xa0; 16],
            original_conversation: Some(CONVERSATION),
            expected_generation: generation - 1,
            conversation: CONVERSATION,
            generation,
            task: [0x22; 16],
            operation,
            model: "system".into(),
            configuration_digest: [0; 32],
            instructions_digest: [0; 32],
            input_digest: [0; 32],
            prior_operations: Vec::new(),
            prompt: format!("question {generation}"),
            event_cursor: 1,
            composition: journal::Composition {
                notice_through: through,
                notice_live: live,
                ..journal::Composition::default()
            },
            thinking_level: 0,
            thinking_request: 0,
        },
        journal::TurnTerminal {
            operation,
            generation,
            kind: journal::TerminalKind::Complete,
            cause: journal::Cause::None,
            final_cursor: 2,
            usage_known: false,
            output_tokens: 0,
            text_offset: 0,
            text: format!("answer {generation}"),
        },
    )
}

/// J5T-11 (unit, owner part): each prior turn becomes a candidate with
/// the notice it had, built from its `Prepare` notice inputs by the same
/// rule as the turn's own notice; a turn past the inputs (beyond the
/// shown-turn bound) gets none; inputs that disagree with the turn's own
/// kind 21 facts are a `NoticeError`.
#[test]
fn j5t_11_prior_turns_carry_their_rebuilt_notice() {
    let entry = || settlement(1, 20, &arguments("cargo test", None));
    // Newest first, as `Prepare` returns them.
    let history = vec![prior(3, 40, 1), prior(2, 4, 0), prior(1, 0, 0)];
    let notices = vec![
        prior_inputs(&inputs(vec![entry()], 1)),
        PriorNotice {
            after: 0,
            through: 4,
            settlements: Vec::new(),
            live: 0,
        },
    ];
    let shown = candidates(CONVERSATION, history.clone(), &notices).unwrap();
    assert_eq!(
        shown.iter().map(|c| c.generation).collect::<Vec<_>>(),
        [3, 2, 1]
    );
    assert_eq!(shown[0].prompt, "question 3");
    assert_eq!(shown[0].answer, "answer 3");
    // The reference facts give the same entry identity, digest and
    // reference line as the binding-point notice; no full text is kept.
    let bound = build(CONVERSATION, &inputs(vec![entry()], 1)).unwrap();
    let rebuilt = &shown[0].notice;
    assert_eq!(
        (rebuilt.through, rebuilt.live, rebuilt.overflow),
        (40, 1, false)
    );
    assert_eq!(rebuilt.entries.len(), 1);
    let (got, want) = (&rebuilt.entries[0], &bound.entries[0]);
    assert_eq!(
        (got.job, got.sequence, got.digest, &got.reference),
        (want.job, want.sequence, want.digest, &want.reference)
    );
    assert!(got.full.is_empty());
    assert_eq!(rebuilt.text(1), bound.text(1));
    assert_eq!(rebuilt.summary_digest(), bound.summary_digest());
    assert!(shown[1].notice.is_empty());
    assert_eq!(shown[1].notice.through, 4);
    assert!(shown[2].notice.is_empty(), "past the inputs: no notice");
    // A `!!` person job's reference line names the sandbox.
    let mut outside = notices.clone();
    outside[0].settlements[0] =
        prior_inputs(&inputs(vec![person_settlement(7, 20, true, false)], 1))
            .settlements
            .remove(0);
    let shown_outside = candidates(CONVERSATION, history.clone(), &outside).unwrap();
    assert!(
        shown_outside[0].notice.entries[0]
            .reference
            .contains(": the person, outside the sandbox, exit "),
        "{:?}",
        shown_outside[0].notice.entries[0]
    );
    // The inputs must be the turn's own S and live count.
    for (through, live) in [(41, 1), (40, 2)] {
        let mut wrong = notices.clone();
        wrong[0].through = through;
        wrong[0].live = live;
        assert_eq!(
            candidates(CONVERSATION, history.clone(), &wrong).err(),
            Some(NoticeError::Range)
        );
    }
    // A damaged settlement of a prior turn is the same repair case.
    let mut damaged = notices.clone();
    damaged[0].settlements[0].settled.job = [0; 16];
    assert_eq!(
        candidates(CONVERSATION, history, &damaged).err(),
        Some(NoticeError::Job)
    );
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
        history_notices: Vec::new(),
    };
    // A prior turn's notice inputs that disagree with its records fail the
    // same way (notices in later turns, J5-E10).
    let mut damaged = prior_inputs(&inputs(
        vec![settlement(1, 20, &arguments("true", None))],
        0,
    ));
    damaged.settlements[0].settled.job = [0; 16];
    let mut with_prior = prepared(inputs(Vec::new(), 0));
    with_prior.history = vec![prior(1, 40, 0)];
    with_prior.history_notices = vec![damaged];
    assert_eq!(
        owner.prepared(queued(), with_prior).err(),
        Some("authority_repair_required")
    );
    assert!(owner.active.is_none(), "no turn starts; no kind 21");
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

/// A person job of input `n`, `!!` when `outside`, settled at `sequence`;
/// its command source is its kind 31.
fn person_settlement(n: u8, sequence: u64, outside: bool, whole: bool) -> NoticeSettlement {
    let mut model = settlement(1, sequence, &arguments("unused", None));
    let intent = JobIntent {
        origin: JobOrigin::Person { input: [n; 16] },
        agent: 0,
        time: 1_700_000_000_000,
    };
    let settled = settled(&intent, 0);
    model.listing.job = journal::job_id(&intent.origin);
    model.listing.intent = intent;
    model.listing.settled_digest = Some(payload_digest(&settled));
    model.listing.settled = Some(Box::new(settled));
    model.command = journal::JobCommand::Person(Box::new(journal::PersonCommandQueued {
        request: [0xa1; 16],
        digest: [0; 32],
        input: [n; 16],
        project: [0xa0; 16],
        conversation: CONVERSATION,
        expected_generation: 0,
        new_conversation: true,
        outside_sandbox: outside,
        command: if whole {
            journal::WHOLE_COMMAND.into()
        } else {
            "git status".into()
        },
        display_command: if whole {
            "curl -u 'admin:$ASURA_SECRET_1' x".into()
        } else {
            String::new()
        },
        cwd: "crates/cli".into(),
        search_path: if outside {
            String::new()
        } else {
            "/usr/bin:/bin".into()
        },
        environment: Vec::new(),
        order_revision: 1,
        number_kinds: Vec::new(),
        unnumbered: 0,
        command_cut: false,
        whole_command: whole,
        rule_set_revision: 1,
        rule_set_digest: [1; 32],
        fallback: None,
    }));
    model
}

/// P3-E: a person job's entry reads its kind 31: the command as recorded
/// (the display-only copy of a whole-command input), the kind 31 cwd, the
/// person label, and the `, outside the sandbox,` label for field 7 bit 1
/// (PCR13, PCR24). A person intent whose source is a kind 14, or a model
/// intent whose source is a kind 31, is inconsistent input.
#[test]
fn person_sources_give_person_entries() {
    let sandboxed = person_settlement(5, 20, false, false);
    let outside = person_settlement(6, 30, true, true);
    let notice = build(CONVERSATION, &inputs(vec![outside, sandboxed], 0)).unwrap();
    let first = &notice.entries[0].full;
    assert!(
        first.contains("[The person ran a command themselves. You did not run it."),
        "{first}"
    );
    assert!(
        first.contains("command: git status\ndirectory: crates/cli\n"),
        "{first}"
    );
    let second = &notice.entries[1].full;
    assert!(
        second.contains("[The person ran a command, outside the sandbox, themselves."),
        "{second}"
    );
    assert!(
        second.contains("command: curl -u 'admin:$ASURA_SECRET_1' x\n"),
        "{second}"
    );
    let mut crossed = person_settlement(7, 20, false, false);
    crossed.command = settlement(1, 20, &arguments("ls", None)).command;
    assert_eq!(
        build(CONVERSATION, &inputs(vec![crossed], 0)).err(),
        Some(NoticeError::Command)
    );
    let mut crossed = settlement(1, 20, &arguments("ls", None));
    crossed.command = person_settlement(7, 20, false, false).command;
    assert_eq!(
        build(CONVERSATION, &inputs(vec![crossed], 0)).err(),
        Some(NoticeError::Command)
    );
}
