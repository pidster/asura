//! J4T-01: job control codecs (tool-job-controller.md, J4 control messages).
//! Every message and enum at its bounds, each invalid combination, direction
//! and error tokens; plus `ToolProgress.job_id` (J4T-09), the outlived-job
//! fields (EX12) and the audit job events (J4T-10).
use asura_control::{
    jobs::{self, error},
    pb::{self, envelope::Body},
    *,
};

/// A named change to a fixture and whether the result is valid.
type Case<T> = (&'static str, fn(&mut T), bool);
/// A named change that makes a valid fixture invalid.
type Change<T> = (&'static str, fn(&mut T));

const JOB: [u8; 16] = [0x11; 16];
const PROJECT: [u8; 16] = [0x22; 16];
const CONVERSATION: [u8; 16] = [0x33; 16];
const REQUEST: [u8; 16] = [0x44; 16];

fn envelope(body: Body) -> pb::Envelope {
    pb::Envelope {
        service_epoch: Some(vec![1; 16]),
        attachment_id: Some(vec![2; 16]),
        request_counter: Some(1),
        body: Some(body),
    }
}
fn request_ok(body: Body) -> bool {
    let message = envelope(body);
    let ok = validate_semantics(&message, Direction::ClientToServer).is_ok();
    if ok {
        assert_eq!(
            validate_semantics(&message, Direction::ServerToClient),
            Err(ProtocolError::WrongDirection)
        );
        roundtrip(&message);
    }
    ok
}
fn reply_ok(body: Body) -> bool {
    let message = envelope(body);
    let ok = validate_semantics(&message, Direction::ServerToClient).is_ok();
    if ok {
        assert_eq!(
            validate_semantics(&message, Direction::ClientToServer),
            Err(ProtocolError::WrongDirection)
        );
        roundtrip(&message);
    }
    ok
}
fn roundtrip(message: &pb::Envelope) {
    let frame = encode_frame(message).unwrap();
    assert!(frame.len() <= MAX_FRAME_BYTES + HEADER_BYTES);
    assert_eq!(&decode_body(&frame[HEADER_BYTES..]).unwrap(), message);
}

fn live_entry() -> pb::JobEntry {
    pb::JobEntry {
        job_id: Some(JOB.to_vec()),
        project_id: Some(PROJECT.to_vec()),
        conversation_id: Some(CONVERSATION.to_vec()),
        origin: Some(pb::JobOrigin::Model as i32),
        command: Some("cargo test".into()),
        state: Some(pb::JobState::Running as i32),
        stdout_bytes: Some(10),
        stderr_bytes: Some(0),
        intent_unix_ms: Some(1_760_000_000_000),
        elapsed_ms: Some(5),
        pinned: Some(false),
        deleted: Some(false),
        notes: Some(0),
        sequence: Some(40),
        cancel_requested: Some(false),
        ..Default::default()
    }
}
fn settled_entry() -> pb::JobEntry {
    pb::JobEntry {
        state: Some(pb::JobState::Settled as i32),
        outcome: Some(1),
        reason: Some(0),
        exit_kind: Some(1),
        exit_value: Some(0),
        stdout_lines: Some(1),
        stderr_lines: Some(0),
        settled_unix_ms: Some(1_760_000_001_000),
        capture_state: Some(1),
        ..live_entry()
    }
}
fn list_reply(entries: Vec<pb::JobEntry>) -> pb::JobListReply {
    pb::JobListReply {
        entries,
        next_before: None,
        revision: Some(1),
        live_in_scope: Some(1),
        live_in_service: Some(2),
        execution_enabled: Some(true),
        error: None,
    }
}
fn entry_ok(entry: pb::JobEntry) -> bool {
    reply_ok(Body::JobListReply(list_reply(vec![entry])))
}
fn page(stream: pb::JobStream, start: u64, text: &str) -> pb::JobStreamPage {
    pb::JobStreamPage {
        stream: Some(stream as i32),
        start: Some(start),
        text: Some(text.into()),
        next_offset: Some(start + text.len() as u64),
        total_bytes: Some(start + text.len() as u64),
        state: Some(pb::JobPageState::Live as i32),
    }
}

#[test]
fn envelope_fields_57_to_67_and_capability_7_are_exact() {
    let bodies: [(u32, Body); 11] = [
        (57, Body::JobList(pb::JobList::default())),
        (58, Body::JobListReply(pb::JobListReply::default())),
        (59, Body::JobInspect(pb::JobInspect::default())),
        (60, Body::JobDetail(pb::JobDetail::default())),
        (61, Body::JobRead(pb::JobRead::default())),
        (62, Body::JobPage(pb::JobPage::default())),
        (63, Body::JobObserve(pb::JobObserve::default())),
        (64, Body::JobCancel(pb::JobCancel::default())),
        (65, Body::JobCancelReply(pb::JobCancelReply::default())),
        (66, Body::JobRetain(pb::JobRetain::default())),
        (67, Body::JobRetainReply(pb::JobRetainReply::default())),
    ];
    for (tag, body) in bodies {
        let message = pb::Envelope {
            service_epoch: None,
            attachment_id: None,
            request_counter: None,
            body: Some(body),
        };
        let key = (tag << 3) | 2;
        let bytes = prost::Message::encode_to_vec(&message);
        assert_eq!(bytes[..2], [(key & 0x7f) as u8 | 0x80, (key >> 7) as u8]);
    }
    assert_eq!(pb::Capability::Jobs as i32, 7);
    // The wire accepts capabilities 7 and 8 (person commands) and rejects 9.
    assert!(decode_body(&[0x5a, 2, 0x10, 7]).is_ok());
    assert!(decode_body(&[0x5a, 2, 0x10, 8]).is_ok());
    assert!(decode_body(&[0x5a, 2, 0x10, 9]).is_err());
    let mut hello = envelope(Body::HelloReply(pb::HelloReply {
        service_build: Some("asura/0.1.0".into()),
        capabilities: (1..=7).collect(),
        max_frame_bytes: Some(MAX_FRAME_BYTES as u32),
    }));
    hello.request_counter = None;
    assert!(validate_semantics(&hello, Direction::ServerToClient).is_ok());
    if let Some(Body::HelloReply(v)) = hello.body.as_mut() {
        v.capabilities = vec![1, 2, 3, 4, 7];
    }
    assert!(validate_semantics(&hello, Direction::ServerToClient).is_ok());
    if let Some(Body::HelloReply(v)) = hello.body.as_mut() {
        v.capabilities = vec![1, 2, 3, 4, 7, 7];
    }
    assert!(validate_semantics(&hello, Direction::ServerToClient).is_err());
}

#[test]
fn enum_values_equal_the_journal_codes() {
    use pb::{JobOrigin as O, JobPageState as P, JobRetainAction as A, JobState as S};
    assert_eq!(
        [
            pb::JobScope::Project,
            pb::JobScope::AllProjects,
            pb::JobScope::Conversation
        ]
        .map(|v| v as i32),
        [1, 2, 3]
    );
    assert_eq!([O::Model as i32, O::Person as i32], [1, 2]);
    assert_eq!(
        [
            S::Queued,
            S::Starting,
            S::Running,
            S::CapturePaused,
            S::Stopping,
            S::Draining,
            S::CleanupUnconfirmed,
            S::Finalizing,
            S::Settled
        ]
        .map(|v| v as i32),
        [1, 2, 3, 4, 5, 6, 7, 8, 9]
    );
    assert_eq!(
        [pb::JobStream::Stdout as i32, pb::JobStream::Stderr as i32],
        [1, 2]
    );
    assert_eq!(
        [
            P::Live,
            P::Complete,
            P::Incomplete,
            P::UnverifiedPrefix,
            P::Expired,
            P::Missing,
            P::Corrupt,
            P::Deleted
        ]
        .map(|v| v as i32),
        [1, 2, 3, 4, 5, 6, 7, 8]
    );
    assert_eq!(
        [A::Pin, A::Unpin, A::Delete, A::Note].map(|v| v as i32),
        [1, 2, 3, 4]
    );
    // Out-of-range enum values are malformed wire: JobList scope 4 and
    // JobRead stream 3 (field 57 = 0xca 0x03, field 61 = 0xea 0x03).
    assert!(decode_body(&[0xca, 0x03, 2, 0x08, 3]).is_ok());
    assert!(decode_body(&[0xca, 0x03, 2, 0x08, 4]).is_err());
    assert!(decode_body(&[0xea, 0x03, 2, 0x18, 2]).is_ok());
    assert!(decode_body(&[0xea, 0x03, 2, 0x18, 3]).is_err());
}

#[test]
fn job_list_scope_cursor_and_limit_rules() {
    let list = |scope: Option<i32>, project: bool, conversation: bool| pb::JobList {
        scope,
        project_id: project.then(|| PROJECT.to_vec()),
        conversation_id: conversation.then(|| CONVERSATION.to_vec()),
        before: None,
        limit: None,
        live_only: None,
        after_revision: None,
    };
    for (scope, project, conversation, ok) in [
        (Some(1), true, false, true),
        (Some(1), false, false, false),
        (Some(1), true, true, false),
        (Some(2), false, false, true),
        (Some(2), true, false, false),
        (Some(2), false, true, false),
        (Some(3), true, true, true),
        (Some(3), true, false, false),
        (Some(3), false, true, false),
        (Some(0), true, false, false),
        (None, true, false, false),
    ] {
        assert_eq!(
            request_ok(Body::JobList(list(scope, project, conversation))),
            ok,
            "{scope:?} {project} {conversation}"
        );
    }
    for (limit, ok) in [
        (Some(0), false),
        (Some(1), true),
        (Some(8), true),
        (Some(9), false),
    ] {
        let mut v = list(Some(1), true, false);
        v.limit = limit;
        assert_eq!(request_ok(Body::JobList(v)), ok);
    }
    for (before, ok) in [(Some(0), false), (Some(1), true), (Some(u64::MAX), true)] {
        let mut v = list(Some(2), false, false);
        v.before = before;
        assert_eq!(request_ok(Body::JobList(v)), ok);
    }
    let mut v = list(Some(1), true, false);
    v.live_only = Some(true);
    v.after_revision = Some(0);
    assert!(request_ok(Body::JobList(v.clone())));
    v.project_id = Some(vec![0; 16]);
    assert!(!request_ok(Body::JobList(v.clone())));
    v.project_id = Some(vec![1; 15]);
    assert!(!request_ok(Body::JobList(v)));
}

#[test]
fn job_list_reply_order_cursor_counts_and_bounds() {
    let entries = |n: u64| {
        (0..n)
            .map(|i| pb::JobEntry {
                job_id: Some(vec![i as u8 + 1; 16]),
                sequence: Some(100 - i),
                ..live_entry()
            })
            .collect::<Vec<_>>()
    };
    let mut full = list_reply(entries(8));
    full.next_before = Some(93);
    assert!(reply_ok(Body::JobListReply(full.clone())));
    assert!(reply_ok(Body::JobListReply(list_reply(Vec::new()))));
    // The cursor names the last entry shown.
    let mut v = full.clone();
    v.next_before = Some(94);
    assert!(!reply_ok(Body::JobListReply(v)));
    let mut v = list_reply(Vec::new());
    v.next_before = Some(1);
    assert!(!reply_ok(Body::JobListReply(v)));
    // Newest first, one entry per job.
    let mut v = full.clone();
    v.entries.swap(0, 1);
    assert!(!reply_ok(Body::JobListReply(v)));
    let mut v = full.clone();
    v.entries[1].job_id = v.entries[0].job_id.clone();
    assert!(!reply_ok(Body::JobListReply(v)));
    // At most 8 entries: the semantic check and the wire bound.
    let mut v = list_reply(entries(9));
    v.next_before = None;
    assert!(!reply_ok(Body::JobListReply(v.clone())));
    let bytes = prost::Message::encode_to_vec(&envelope(Body::JobListReply(v)));
    assert_eq!(decode_body(&bytes), Err(ProtocolError::MalformedWire));
    // Live counts: scope within service, service within the 64-job bound.
    for (scope, service, ok) in [
        (Some(0), Some(0), true),
        (Some(64), Some(64), true),
        (Some(3), Some(2), false),
        (Some(0), Some(65), false),
        (None, Some(1), false),
        (Some(0), None, false),
    ] {
        let mut v = list_reply(Vec::new());
        v.live_in_scope = scope;
        v.live_in_service = service;
        assert_eq!(reply_ok(Body::JobListReply(v)), ok, "{scope:?} {service:?}");
    }
    let mut v = list_reply(Vec::new());
    v.revision = Some(0);
    assert!(!reply_ok(Body::JobListReply(v)));
    // EX11: execution_enabled is optional on the wire (absent for all projects).
    let mut v = list_reply(Vec::new());
    v.execution_enabled = None;
    assert!(reply_ok(Body::JobListReply(v)));
    // Error replies carry only the token.
    for token in [
        error::NOT_FOUND,
        error::BUSY,
        error::UNAVAILABLE,
        error::INVALID_CURSOR,
    ] {
        let v = pb::JobListReply {
            error: Some(token.into()),
            ..Default::default()
        };
        assert!(reply_ok(Body::JobListReply(v)), "{token}");
    }
    for token in [
        error::OUTCOME_UNCONFIRMED,
        error::KILL_NOT_PERMITTED,
        error::NOTE_INVALID,
        "",
        "other",
    ] {
        let v = pb::JobListReply {
            error: Some(token.into()),
            ..Default::default()
        };
        assert!(!reply_ok(Body::JobListReply(v)), "{token}");
    }
    let mut v = list_reply(Vec::new());
    v.error = Some(error::BUSY.into());
    assert!(!reply_ok(Body::JobListReply(v)));
    // The largest page fits the 40 KiB reply bound and the frame.
    let largest = pb::JobEntry {
        command: Some("c".repeat(jobs::MAX_JOB_COMMAND_EXCERPT_BYTES)),
        state: Some(pb::JobState::Queued as i32),
        block_reason: Some(jobs::MAX_BLOCK_REASON),
        reason_text: Some("r".repeat(jobs::MAX_JOB_REASON_TEXT_BYTES)),
        reason: Some(jobs::MAX_JOB_REASON),
        stdout_bytes: Some(u64::MAX),
        stderr_bytes: Some(u64::MAX),
        intent_unix_ms: Some(u64::MAX),
        elapsed_ms: Some(u64::MAX),
        input_id: Some(vec![0xff; 16]),
        origin: Some(pb::JobOrigin::Person as i32),
        outside_sandbox: Some(true),
        notes: Some(jobs::MAX_JOB_NOTES),
        ..live_entry()
    };
    let mut v = list_reply(
        (0..8u8)
            .map(|i| pb::JobEntry {
                job_id: Some(vec![i + 1; 16]),
                sequence: Some(u64::MAX - u64::from(i)),
                ..largest.clone()
            })
            .collect(),
    );
    v.next_before = Some(u64::MAX - 7);
    v.revision = Some(u64::MAX);
    assert!(prost::Message::encoded_len(&v) <= jobs::MAX_JOB_LIST_REPLY_BYTES);
    assert!(reply_ok(Body::JobListReply(v)));
}

#[test]
fn job_entry_cross_field_rules() {
    assert!(entry_ok(live_entry()));
    assert!(entry_ok(settled_entry()));
    let changes: Vec<Case<pb::JobEntry>> = vec![
        ("zero job", |e| e.job_id = Some(vec![0; 16]), false),
        ("no project", |e| e.project_id = None, false),
        ("no conversation", |e| e.conversation_id = None, false),
        ("no origin", |e| e.origin = None, false),
        ("no state", |e| e.state = None, false),
        ("no sequence", |e| e.sequence = None, false),
        ("zero sequence", |e| e.sequence = Some(0), false),
        ("no command", |e| e.command = None, false),
        ("empty command", |e| e.command = Some(String::new()), true),
        ("excerpt 192", |e| e.command = Some("x".repeat(192)), true),
        ("excerpt 193", |e| e.command = Some("x".repeat(193)), false),
        (
            "excerpt newline",
            |e| e.command = Some("a\nb".into()),
            false,
        ),
        ("excerpt NUL", |e| e.command = Some("a\0b".into()), false),
        ("model outside", |e| e.outside_sandbox = Some(true), false),
        ("model inside", |e| e.outside_sandbox = Some(false), true),
        ("model input", |e| e.input_id = Some(vec![5; 16]), false),
        ("reason 19", |e| e.reason = Some(19), true),
        ("reason 20", |e| e.reason = Some(20), true),
        ("reason 21", |e| e.reason = Some(21), false),
        ("notes 16", |e| e.notes = Some(16), true),
        ("notes 17", |e| e.notes = Some(17), false),
        ("live outcome", |e| e.outcome = Some(1), false),
        ("live capture", |e| e.capture_state = Some(1), false),
        ("live lines", |e| e.stdout_lines = Some(1), false),
        ("live settled time", |e| e.settled_unix_ms = Some(1), false),
        ("live deleted", |e| e.deleted = Some(true), false),
        (
            "live exit",
            |e| {
                e.exit_kind = Some(1);
                e.exit_value = Some(0)
            },
            false,
        ),
        ("running block", |e| e.block_reason = Some(1), false),
        (
            "reason text alone",
            |e| e.reason_text = Some("waiting".into()),
            false,
        ),
        ("queued no reason", |e| e.state = Some(1), false),
        (
            "queued reason 9",
            |e| {
                e.state = Some(1);
                e.block_reason = Some(9);
                e.reason_text = Some("waiting: disk space low".into());
            },
            true,
        ),
        (
            "queued reason 10",
            |e| {
                e.state = Some(1);
                e.block_reason = Some(10)
            },
            false,
        ),
        (
            "queued reason 0",
            |e| {
                e.state = Some(1);
                e.block_reason = Some(0)
            },
            false,
        ),
        (
            "queued control text",
            |e| {
                e.state = Some(1);
                e.block_reason = Some(5);
                e.reason_text = Some("a\nb".into());
            },
            false,
        ),
        (
            "queued long text",
            |e| {
                e.state = Some(1);
                e.block_reason = Some(5);
                e.reason_text = Some("a".repeat(257));
            },
            false,
        ),
        (
            "paused reason",
            |e| {
                e.state = Some(4);
                e.block_reason = Some(2)
            },
            true,
        ),
        ("paused no reason", |e| e.state = Some(4), true),
        (
            "paused enospc",
            |e| {
                e.state = Some(4);
                e.block_reason = Some(3)
            },
            true,
        ),
        (
            "paused reason 1",
            |e| {
                e.state = Some(4);
                e.block_reason = Some(1)
            },
            false,
        ),
        (
            "paused reason 4",
            |e| {
                e.state = Some(4);
                e.block_reason = Some(4)
            },
            false,
        ),
        ("state 9 needs outcome", |e| e.state = Some(9), false),
    ];
    for (name, change, ok) in changes {
        let mut e = live_entry();
        change(&mut e);
        assert_eq!(entry_ok(e), ok, "{name}");
    }
    let settled: Vec<Case<pb::JobEntry>> = vec![
        ("deleted", |e| e.deleted = Some(true), true),
        ("outcome 5", |e| e.outcome = Some(5), true),
        ("outcome 0", |e| e.outcome = Some(0), false),
        ("outcome 6", |e| e.outcome = Some(6), false),
        ("no outcome", |e| e.outcome = None, false),
        ("no reason", |e| e.reason = None, false),
        ("no capture", |e| e.capture_state = None, false),
        ("capture 6", |e| e.capture_state = Some(6), false),
        (
            "no exit",
            |e| {
                e.exit_kind = None;
                e.exit_value = None
            },
            false,
        ),
        ("exit kind alone", |e| e.exit_value = None, false),
        (
            "unknown exit 0",
            |e| {
                e.exit_kind = Some(0);
                e.exit_value = Some(0)
            },
            true,
        ),
        (
            "unknown exit 1",
            |e| {
                e.exit_kind = Some(0);
                e.exit_value = Some(1)
            },
            false,
        ),
        ("exited 255", |e| e.exit_value = Some(255), true),
        ("exited 256", |e| e.exit_value = Some(256), false),
        (
            "signal 0",
            |e| {
                e.exit_kind = Some(2);
                e.exit_value = Some(0)
            },
            false,
        ),
        (
            "signal 127",
            |e| {
                e.exit_kind = Some(2);
                e.exit_value = Some(127)
            },
            true,
        ),
        (
            "signal 128",
            |e| {
                e.exit_kind = Some(2);
                e.exit_value = Some(128)
            },
            false,
        ),
        ("exit kind 3", |e| e.exit_kind = Some(3), false),
        (
            "person outside with input",
            |e| {
                e.origin = Some(2);
                e.outside_sandbox = Some(true);
                e.input_id = Some(vec![5; 16]);
            },
            true,
        ),
        (
            "person zero input",
            |e| {
                e.origin = Some(2);
                e.input_id = Some(vec![0; 16])
            },
            false,
        ),
    ];
    for (name, change, ok) in settled {
        let mut e = settled_entry();
        change(&mut e);
        assert_eq!(entry_ok(e), ok, "settled {name}");
    }
}

#[test]
fn job_inspect_and_detail_bounds() {
    for (job, project, ok) in [
        (Some(JOB.to_vec()), None, true),
        (Some(JOB.to_vec()), Some(PROJECT.to_vec()), true),
        (None, None, false),
        (Some(vec![0; 16]), None, false),
        (Some(JOB.to_vec()), Some(vec![0; 16]), false),
    ] {
        assert_eq!(
            request_ok(Body::JobInspect(pb::JobInspect {
                job_id: job,
                project_id: project
            })),
            ok
        );
    }
    let note = |author, text: &str| pb::JobNote {
        author: Some(author),
        unix_ms: Some(0),
        text: Some(text.into()),
    };
    let detail = pb::JobDetail {
        entry: Some(settled_entry()),
        command: Some("x".repeat(jobs::MAX_JOB_COMMAND_BYTES)),
        cwd: Some(format!("/{}", "d".repeat(jobs::MAX_JOB_CWD_BYTES - 1))),
        notes: (0..16)
            .map(|i| note(1 + i % 2, &"n".repeat(jobs::MAX_JOB_NOTE_BYTES)))
            .collect(),
        error: None,
        path_warnings: Vec::new(),
        path_overflow: None,
    };
    // The largest detail fits one frame.
    assert!(reply_ok(Body::JobDetail(detail.clone())));
    let changes: Vec<Change<pb::JobDetail>> = vec![
        ("no entry", |d| d.entry = None),
        ("no command", |d| d.command = None),
        ("long command", |d| d.command = Some("x".repeat(8193))),
        ("NUL command", |d| d.command = Some("a\0".into())),
        ("relative cwd", |d| d.cwd = Some("tmp".into())),
        ("long cwd", |d| {
            d.cwd = Some(format!("/{}", "d".repeat(4096)))
        }),
        ("author 0", |d| d.notes[0].author = Some(0)),
        ("author 3", |d| d.notes[0].author = Some(3)),
        ("no time", |d| d.notes[0].unix_ms = None),
        ("empty note", |d| d.notes[0].text = Some(String::new())),
        ("long note", |d| d.notes[0].text = Some("n".repeat(2049))),
        ("control note", |d| d.notes[0].text = Some("a\u{7}".into())),
        ("17 notes", |d| d.notes.push(d.notes[0].clone())),
    ];
    for (name, change) in changes {
        let mut d = detail.clone();
        d.command = Some("ls".into());
        d.notes.truncate(16);
        for n in &mut d.notes {
            n.text = Some("a note".into());
        }
        assert!(reply_ok(Body::JobDetail(d.clone())), "base for {name}");
        change(&mut d);
        assert!(!reply_ok(Body::JobDetail(d)), "{name}");
    }
    // The wire rejects a 17th note before semantics.
    let mut d = detail.clone();
    d.command = Some("ls".into());
    d.notes = vec![note(1, "a"); 17];
    let bytes = prost::Message::encode_to_vec(&envelope(Body::JobDetail(d)));
    assert_eq!(decode_body(&bytes), Err(ProtocolError::MalformedWire));
    let mut d = detail.clone();
    d.cwd = None;
    d.notes.clear();
    assert!(reply_ok(Body::JobDetail(d)));
    for (token, ok) in [
        (error::NOT_FOUND, true),
        (error::PROJECT_MISMATCH, true),
        (error::BUSY, true),
        (error::UNAVAILABLE, true),
        (error::NOT_SETTLED, false),
        (error::OUTCOME_UNCONFIRMED, false),
    ] {
        assert_eq!(
            reply_ok(Body::JobDetail(pb::JobDetail {
                error: Some(token.into()),
                ..Default::default()
            })),
            ok,
            "{token}"
        );
    }
    assert!(!reply_ok(Body::JobDetail(pb::JobDetail {
        error: Some(error::NOT_FOUND.into()),
        entry: Some(live_entry()),
        ..Default::default()
    })));
    // EPG-T2: field 7 carries the kind 32 overflow count beside 16 warnings;
    // it is never 0 and never on an error reply.
    let warning = |kind, path: &str| pb::PathWarning {
        kind: Some(kind),
        path: Some(path.into()),
    };
    let mut d = detail.clone();
    d.notes.clear();
    d.path_warnings = vec![warning(1, &"p".repeat(1024)); 16];
    d.path_overflow = Some(u32::MAX);
    // `reply_ok` also encodes, decodes and compares the frame.
    assert!(reply_ok(Body::JobDetail(d.clone())), "largest with field 7");
    d.path_warnings.clear();
    d.path_overflow = Some(2);
    assert!(reply_ok(Body::JobDetail(d.clone())), "overflow alone");
    d.path_overflow = Some(0);
    assert!(!reply_ok(Body::JobDetail(d)), "zero overflow");
    assert!(!reply_ok(Body::JobDetail(pb::JobDetail {
        error: Some(error::NOT_FOUND.into()),
        path_overflow: Some(1),
        ..Default::default()
    })));
}

#[test]
fn job_read_and_observe_request_rules() {
    let read = |stream: Option<i32>, offset: Option<u64>, limit: Option<u32>, at_end| pb::JobRead {
        job_id: Some(JOB.to_vec()),
        project_id: Some(PROJECT.to_vec()),
        stream,
        offset,
        limit,
        at_end,
    };
    for (v, ok) in [
        (read(Some(1), Some(0), None, None), true),
        (read(Some(2), Some(u64::MAX), Some(1), None), true),
        (read(Some(1), Some(0), Some(8192), Some(false)), true),
        (read(Some(1), Some(0), Some(0), None), false),
        (read(Some(1), Some(0), Some(8193), None), false),
        (read(None, Some(0), None, None), false),
        (read(Some(1), None, None, None), false),
        (read(Some(1), None, None, Some(true)), true),
        (read(Some(1), Some(0), None, Some(true)), false),
        (read(None, None, None, Some(true)), false),
    ] {
        assert_eq!(request_ok(Body::JobRead(v.clone())), ok, "{v:?}");
    }
    let mut v = read(Some(1), Some(0), None, None);
    v.project_id = None;
    assert!(request_ok(Body::JobRead(v.clone())));
    v.job_id = None;
    assert!(!request_ok(Body::JobRead(v)));
    let observe = |stream: Option<i32>, offset: Option<u64>, at_end, after_state| pb::JobObserve {
        job_id: Some(JOB.to_vec()),
        project_id: None,
        stream,
        offset,
        at_end,
        after_state,
    };
    for (v, ok) in [
        (observe(Some(1), Some(10), None, Some(3)), true),
        (observe(Some(2), Some(0), Some(true), None), true),
        (observe(None, None, Some(true), Some(9)), true),
        (observe(None, None, None, None), false),
        (observe(None, None, Some(false), None), false),
        (observe(None, Some(0), Some(true), None), false),
        (observe(Some(1), None, None, None), false),
        (observe(Some(1), Some(0), None, Some(0)), false),
    ] {
        assert_eq!(request_ok(Body::JobObserve(v.clone())), ok, "{v:?}");
    }
}

#[test]
fn job_page_reply_rules() {
    let one = pb::JobPage {
        entry: Some(live_entry()),
        pages: vec![page(pb::JobStream::Stdout, 0, "hello\n")],
        error: None,
    };
    assert!(reply_ok(Body::JobPage(one.clone())));
    // Two streams at their largest delivered text fit one frame.
    let two = pb::JobPage {
        entry: Some(live_entry()),
        pages: vec![
            page(pb::JobStream::Stdout, u64::MAX - 8192, &"o".repeat(8192)),
            page(pb::JobStream::Stderr, 0, &"e".repeat(8192)),
        ],
        error: None,
    };
    assert!(reply_ok(Body::JobPage(two.clone())));
    let changes: Vec<Change<pb::JobPage>> = vec![
        ("no entry", |p| p.entry = None),
        ("no pages", |p| p.pages.clear()),
        ("same stream twice", |p| p.pages.push(p.pages[0].clone())),
        ("no stream", |p| p.pages[0].stream = None),
        ("no start", |p| p.pages[0].start = None),
        ("no text", |p| p.pages[0].text = None),
        ("no next", |p| p.pages[0].next_offset = None),
        ("no total", |p| p.pages[0].total_bytes = None),
        ("no state", |p| p.pages[0].state = None),
        ("next before start", |p| {
            p.pages[0].start = Some(10);
            p.pages[0].next_offset = Some(9);
            p.pages[0].total_bytes = Some(20);
        }),
        ("next past total", |p| p.pages[0].total_bytes = Some(5)),
        ("source over 8192", |p| {
            p.pages[0].next_offset = Some(8193);
            p.pages[0].total_bytes = Some(8193);
        }),
        ("text over 8192", |p| {
            p.pages[0].text = Some("x".repeat(8193));
            p.pages[0].next_offset = Some(8192);
            p.pages[0].total_bytes = Some(8192);
        }),
        ("deleted with text", |p| p.pages[0].state = Some(8)),
    ];
    for (name, change) in changes {
        let mut p = one.clone();
        change(&mut p);
        assert!(!reply_ok(Body::JobPage(p)), "{name}");
    }
    // States 5-8 carry no source bytes and no text; 1-4 may.
    for state in 1..=8 {
        let mut p = one.clone();
        p.pages[0] = pb::JobStreamPage {
            state: Some(state),
            ..page(pb::JobStream::Stdout, 7, "")
        };
        p.pages[0].total_bytes = Some(100);
        assert!(reply_ok(Body::JobPage(p)), "state {state}");
    }
    // Three pages are malformed wire.
    let mut p = two.clone();
    p.pages.push(page(pb::JobStream::Stdout, 0, ""));
    let bytes = prost::Message::encode_to_vec(&envelope(Body::JobPage(p)));
    assert_eq!(decode_body(&bytes), Err(ProtocolError::MalformedWire));
    for (token, ok) in [
        (error::NOT_FOUND, true),
        (error::PROJECT_MISMATCH, true),
        (error::BUSY, true),
        (error::UNAVAILABLE, true),
        (error::INVALID_CURSOR, false),
        (error::OUTCOME_UNCONFIRMED, false),
    ] {
        assert_eq!(
            reply_ok(Body::JobPage(pb::JobPage {
                error: Some(token.into()),
                ..Default::default()
            })),
            ok,
            "{token}"
        );
    }
    assert!(!reply_ok(Body::JobPage(pb::JobPage {
        error: Some(error::BUSY.into()),
        pages: one.pages.clone(),
        ..Default::default()
    })));
}

#[test]
fn job_cancel_request_and_reply_rules() {
    let cancel = pb::JobCancel {
        request_id: Some(REQUEST.to_vec()),
        job_id: Some(JOB.to_vec()),
        expected_project_id: Some(PROJECT.to_vec()),
        kill: None,
    };
    assert!(request_ok(Body::JobCancel(cancel.clone())));
    for kill in [Some(true), Some(false)] {
        assert!(request_ok(Body::JobCancel(pb::JobCancel {
            kill,
            ..cancel.clone()
        })));
    }
    for change in [
        |c: &mut pb::JobCancel| c.request_id = None,
        |c: &mut pb::JobCancel| c.job_id = None,
        |c: &mut pb::JobCancel| c.expected_project_id = None,
        |c: &mut pb::JobCancel| c.job_id = Some(vec![0; 16]),
        |c: &mut pb::JobCancel| c.request_id = Some(vec![1; 17]),
    ] {
        let mut c = cancel.clone();
        change(&mut c);
        assert!(!request_ok(Body::JobCancel(c)));
    }
    let reply = |entry: Option<pb::JobEntry>, requested, error: Option<&str>| {
        Body::JobCancelReply(pb::JobCancelReply {
            request_id: Some(REQUEST.to_vec()),
            entry,
            requested,
            error: error.map(Into::into),
        })
    };
    assert!(reply_ok(reply(Some(live_entry()), Some(true), None)));
    assert!(reply_ok(reply(Some(settled_entry()), Some(false), None)));
    assert!(reply_ok(reply(Some(settled_entry()), Some(true), None)));
    // Not requested only because the job had settled.
    assert!(!reply_ok(reply(Some(live_entry()), Some(false), None)));
    assert!(!reply_ok(reply(Some(live_entry()), None, None)));
    assert!(!reply_ok(reply(None, Some(true), None)));
    for (token, ok) in [
        (error::NOT_FOUND, true),
        (error::PROJECT_MISMATCH, true),
        (error::KILL_NOT_PERMITTED, true),
        (error::UNAVAILABLE, true),
        (error::NOT_SETTLED, false),
        (error::BUSY, false),
        (error::OUTCOME_UNCONFIRMED, false),
    ] {
        assert_eq!(reply_ok(reply(None, None, Some(token))), ok, "{token}");
    }
    assert!(!reply_ok(reply(
        Some(live_entry()),
        None,
        Some(error::NOT_FOUND)
    )));
    let mut no_request = pb::JobCancelReply {
        request_id: None,
        entry: Some(live_entry()),
        requested: Some(true),
        error: None,
    };
    assert!(!reply_ok(Body::JobCancelReply(no_request.clone())));
    no_request.request_id = Some(vec![0; 16]);
    assert!(!reply_ok(Body::JobCancelReply(no_request)));
}

#[test]
fn job_retain_request_and_reply_rules() {
    let retain = |action: Option<i32>, note: Option<&str>, confirmed| pb::JobRetain {
        request_id: Some(REQUEST.to_vec()),
        job_id: Some(JOB.to_vec()),
        expected_project_id: Some(PROJECT.to_vec()),
        action,
        note: note.map(Into::into),
        confirmed,
    };
    for (v, ok) in [
        (retain(Some(1), None, None), true),
        (retain(Some(2), None, None), true),
        (retain(Some(3), None, Some(true)), true),
        (retain(Some(4), Some("checked"), None), true),
        (retain(Some(0), None, None), false),
        (retain(None, None, None), false),
        (retain(Some(1), Some("x"), None), false),
        (retain(Some(1), None, Some(true)), false),
        (retain(Some(2), Some("x"), None), false),
        (retain(Some(3), None, None), false),
        (retain(Some(3), None, Some(false)), false),
        (retain(Some(3), Some("x"), Some(true)), false),
        (retain(Some(4), None, None), false),
        (retain(Some(4), Some("x"), Some(true)), false),
    ] {
        assert_eq!(request_ok(Body::JobRetain(v.clone())), ok, "{v:?}");
    }
    // The note text rule is the service's `note_invalid` token, so an
    // invalid text still passes the codec and reaches the service.
    for text in ["", "a\u{1b}"] {
        assert!(request_ok(Body::JobRetain(retain(
            Some(4),
            Some(text),
            None
        ))));
        assert!(!jobs::note_text_valid(text));
    }
    let long = "n".repeat(jobs::MAX_JOB_NOTE_BYTES + 1);
    assert!(request_ok(Body::JobRetain(retain(
        Some(4),
        Some(&long),
        None
    ))));
    assert!(!jobs::note_text_valid(&long));
    let mut v = retain(Some(1), None, None);
    v.expected_project_id = None;
    assert!(!request_ok(Body::JobRetain(v)));
    let reply = |entry: Option<pb::JobEntry>, changed, error: Option<&str>| {
        Body::JobRetainReply(pb::JobRetainReply {
            request_id: Some(REQUEST.to_vec()),
            entry,
            changed,
            error: error.map(Into::into),
        })
    };
    assert!(reply_ok(reply(Some(settled_entry()), Some(true), None)));
    assert!(reply_ok(reply(Some(live_entry()), Some(false), None)));
    assert!(!reply_ok(reply(Some(live_entry()), None, None)));
    assert!(!reply_ok(reply(None, Some(true), None)));
    for (token, ok) in [
        (error::NOT_FOUND, true),
        (error::PROJECT_MISMATCH, true),
        (error::NOT_SETTLED, true),
        (error::NOTES_FULL, true),
        (error::RETENTION_FULL, true),
        (error::NOTE_INVALID, true),
        (error::UNAVAILABLE, true),
        (error::KILL_NOT_PERMITTED, false),
        (error::BUSY, false),
        (error::OUTCOME_UNCONFIRMED, false),
    ] {
        assert_eq!(reply_ok(reply(None, None, Some(token))), ok, "{token}");
    }
    assert!(!reply_ok(reply(None, Some(false), Some(error::NOT_FOUND))));
}

fn tool(decision: Option<u32>, job: Option<Vec<u8>>) -> pb::ToolProgress {
    pb::ToolProgress {
        ordinal: Some(1),
        name: Some("exec_cmd".into()),
        state: Some(1),
        policy_decision: decision,
        job_id: job,
        ..Default::default()
    }
}
fn terminal_event() -> pb::ConversationEvent {
    pb::ConversationEvent {
        operation_id: Some(vec![1; 16]),
        cursor: Some(u64::MAX),
        generation: Some(1),
        kind: Some(3),
        text: Some("Complete".into()),
        reason: Some(0),
        usage_known: Some(false),
        text_offset: Some(0),
        text_bytes: Some(8),
        ..Default::default()
    }
}

#[test]
fn tool_progress_job_id_and_fault_16() {
    let event = |tools| {
        Body::ConversationEvent(pb::ConversationEvent {
            tools,
            ..terminal_event()
        })
    };
    assert!(reply_ok(event(vec![tool(Some(1), Some(JOB.to_vec()))])));
    assert!(reply_ok(event(vec![tool(Some(1), None)])));
    // Only an admitted call names a job, and only with a valid ID.
    assert!(!reply_ok(event(vec![tool(Some(4), Some(JOB.to_vec()))])));
    assert!(!reply_ok(event(vec![tool(None, Some(JOB.to_vec()))])));
    assert!(!reply_ok(event(vec![tool(Some(1), Some(vec![0; 16]))])));
    assert!(!reply_ok(event(vec![tool(Some(1), Some(vec![1; 8]))])));
    // Fault 16 `invalid_job_id` is accepted; 17 is not.
    let faulted = |fault| pb::ToolProgress {
        state: Some(3),
        status: Some(2),
        fault: Some(fault),
        ..tool(Some(4), None)
    };
    assert!(reply_ok(event(vec![faulted(16)])));
    assert!(!reply_ok(event(vec![faulted(17)])));
}

#[test]
fn outlived_jobs_appear_on_terminal_events_only() {
    let ids = |n: u8| (1..=n).map(|i| vec![i; 16]).collect::<Vec<_>>();
    for kind in 3..=6 {
        let mut v = terminal_event();
        v.kind = Some(kind);
        v.reason = Some(if kind == 3 { 0 } else { 7 });
        v.outlived_jobs = Some(2);
        v.outlived_job_ids = ids(2);
        assert!(reply_ok(Body::ConversationEvent(v)), "kind {kind}");
    }
    // The count may exceed the 8 IDs carried.
    let mut v = terminal_event();
    v.outlived_jobs = Some(20);
    v.outlived_job_ids = ids(8);
    assert!(reply_ok(Body::ConversationEvent(v.clone())));
    v.outlived_job_ids = Vec::new();
    assert!(reply_ok(Body::ConversationEvent(v.clone())));
    // More IDs than the count, duplicates, a zero ID, or IDs without a count.
    for (count, list) in [
        (Some(1), ids(2)),
        (None, ids(1)),
        (Some(2), vec![vec![1; 16], vec![1; 16]]),
        (Some(1), vec![vec![0; 16]]),
        (Some(1), vec![vec![1; 15]]),
    ] {
        let mut v = terminal_event();
        v.outlived_jobs = count;
        v.outlived_job_ids = list;
        assert!(!reply_ok(Body::ConversationEvent(v)));
    }
    // Nine IDs are malformed wire.
    let mut v = terminal_event();
    v.outlived_jobs = Some(9);
    v.outlived_job_ids = ids(9);
    let bytes = prost::Message::encode_to_vec(&envelope(Body::ConversationEvent(v)));
    assert_eq!(decode_body(&bytes), Err(ProtocolError::MalformedWire));
    // A pending event carries neither field.
    let pending = pb::ConversationEvent {
        operation_id: Some(vec![1; 16]),
        cursor: Some(0),
        generation: Some(1),
        kind: Some(1),
        reason: Some(0),
        ..Default::default()
    };
    assert!(reply_ok(Body::ConversationEvent(pending.clone())));
    for (count, list) in [(Some(1), Vec::new()), (None, ids(1)), (Some(0), Vec::new())] {
        let mut v = pending.clone();
        v.outlived_jobs = count;
        v.outlived_job_ids = list;
        assert!(!reply_ok(Body::ConversationEvent(v)));
    }
}

fn audit(entry: pb::AuditEntry) -> Body {
    Body::AuditReply(pb::AuditReply {
        project_id: Some(PROJECT.to_vec()),
        health: Some(pb::AuditHealth {
            state: Some(2),
            enabled: Some(true),
            keep_files: Some(5),
            max_file_bytes: Some(10_485_760),
            dropped: Some(0),
            window_capacity: Some(256),
            hydrated: Some(true),
            older_omitted: Some(false),
            reason: None,
        }),
        entries: vec![entry],
        error: None,
    })
}
fn job_event(kind: u32) -> pb::AuditEntry {
    pb::AuditEntry {
        service_epoch: Some(vec![9; 16]),
        sequence: Some(1),
        unix_time_ms: Some(1),
        kind: Some(kind),
        project: Some(PROJECT.to_vec()),
        conversation: Some(CONVERSATION.to_vec()),
        job: Some(JOB.to_vec()),
        ..Default::default()
    }
}

#[test]
fn audit_job_events_map_kinds_4_to_7() {
    let cases: Vec<(&str, pb::AuditEntry, bool)> = vec![
        (
            "queued",
            pb::AuditEntry {
                reason: Some(5),
                ..job_event(4)
            },
            true,
        ),
        (
            "queued 9",
            pb::AuditEntry {
                reason: Some(9),
                ..job_event(4)
            },
            true,
        ),
        (
            "queued 0",
            pb::AuditEntry {
                reason: Some(0),
                ..job_event(4)
            },
            false,
        ),
        (
            "queued 10",
            pb::AuditEntry {
                reason: Some(10),
                ..job_event(4)
            },
            false,
        ),
        (
            "queued outcome",
            pb::AuditEntry {
                reason: Some(5),
                outcome: Some(1),
                ..job_event(4)
            },
            false,
        ),
        (
            "started person",
            pb::AuditEntry {
                reason: Some(0),
                ..job_event(5)
            },
            true,
        ),
        (
            "started model",
            pb::AuditEntry {
                reason: Some(0),
                operation: Some(vec![5; 16]),
                generation: Some(2),
                ordinal: Some(3),
                ..job_event(5)
            },
            true,
        ),
        (
            "started partial origin",
            pb::AuditEntry {
                reason: Some(0),
                operation: Some(vec![5; 16]),
                ..job_event(5)
            },
            false,
        ),
        (
            "started reason",
            pb::AuditEntry {
                reason: Some(1),
                ..job_event(5)
            },
            false,
        ),
        (
            "cancel client",
            pb::AuditEntry {
                outcome: Some(2),
                reason: Some(3),
                ..job_event(6)
            },
            true,
        ),
        (
            "cancel execution_off",
            pb::AuditEntry {
                outcome: Some(4),
                reason: Some(19),
                ..job_event(6)
            },
            true,
        ),
        (
            "cancel requester 5",
            pb::AuditEntry {
                outcome: Some(5),
                reason: Some(3),
                ..job_event(6)
            },
            false,
        ),
        (
            "cancel reason 0",
            pb::AuditEntry {
                outcome: Some(2),
                reason: Some(0),
                ..job_event(6)
            },
            false,
        ),
        (
            "cancel reason 11",
            pb::AuditEntry {
                outcome: Some(2),
                reason: Some(11),
                ..job_event(6)
            },
            false,
        ),
        (
            "settled",
            pb::AuditEntry {
                outcome: Some(5),
                reason: Some(11),
                ..job_event(7)
            },
            true,
        ),
        (
            "settled 19",
            pb::AuditEntry {
                outcome: Some(3),
                reason: Some(19),
                ..job_event(7)
            },
            true,
        ),
        (
            "settled outcome 6",
            pb::AuditEntry {
                outcome: Some(6),
                reason: Some(0),
                ..job_event(7)
            },
            false,
        ),
        (
            "settled reason 20",
            pb::AuditEntry {
                outcome: Some(4),
                reason: Some(20),
                ..job_event(7)
            },
            true,
        ),
        (
            "settled reason 21",
            pb::AuditEntry {
                outcome: Some(4),
                reason: Some(21),
                ..job_event(7)
            },
            false,
        ),
        (
            "cancel reason 20",
            pb::AuditEntry {
                outcome: Some(4),
                reason: Some(20),
                ..job_event(6)
            },
            false,
        ),
        (
            "no job",
            pb::AuditEntry {
                job: None,
                outcome: Some(1),
                reason: Some(0),
                ..job_event(7)
            },
            false,
        ),
        (
            "no conversation",
            pb::AuditEntry {
                conversation: None,
                outcome: Some(1),
                reason: Some(0),
                ..job_event(7)
            },
            false,
        ),
        (
            "tool code",
            pb::AuditEntry {
                tool: Some(10),
                outcome: Some(1),
                reason: Some(0),
                ..job_event(7)
            },
            false,
        ),
        (
            "request",
            pb::AuditEntry {
                request: Some(REQUEST.to_vec()),
                outcome: Some(1),
                reason: Some(0),
                ..job_event(7)
            },
            false,
        ),
        (
            "kind 8",
            pb::AuditEntry {
                outcome: Some(1),
                reason: Some(0),
                ..job_event(8)
            },
            false,
        ),
    ];
    for (name, entry, ok) in cases {
        assert_eq!(reply_ok(audit(entry)), ok, "{name}");
    }
    // Kinds 1-3 never carry a job.
    let request_event = pb::AuditEntry {
        service_epoch: Some(vec![9; 16]),
        sequence: Some(1),
        kind: Some(1),
        project: Some(PROJECT.to_vec()),
        request: Some(REQUEST.to_vec()),
        requested_generation: Some(1),
        outcome: Some(1),
        reason: Some(0),
        ..Default::default()
    };
    assert!(reply_ok(audit(request_event.clone())));
    assert!(!reply_ok(audit(pb::AuditEntry {
        job: Some(JOB.to_vec()),
        ..request_event
    })));
}
