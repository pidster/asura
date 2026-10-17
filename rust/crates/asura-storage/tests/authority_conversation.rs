use asura_storage::authority::{self, ReplayError};
use authority::conversation::*;
use std::sync::atomic::AtomicBool;

fn id(n: u64) -> Id {
    let mut id = [0; 16];
    id[8..].copy_from_slice(&n.to_be_bytes());
    id
}
fn inspect(bytes: &[u8]) -> Result<Replay, ReplayError> {
    replay(bytes, &AtomicBool::new(false))
}
struct Journal {
    bytes: Vec<u8>,
    sequence: u64,
    owner: u64,
    digest: Hash,
}
impl Journal {
    fn empty() -> Self {
        Self {
            bytes: Vec::new(),
            sequence: 0,
            owner: 1,
            digest: [0; 32],
        }
    }
    fn append(&mut self, record: Record) {
        if record == Record::OwnerGeneration {
            self.owner += 1;
        }
        let frame = encode_frame(
            &FrameContext {
                installation_id: id(1),
                transition_id: id(10000 + self.sequence),
                sequence: self.sequence + 1,
                prior_digest: self.digest,
                expected_revision: self.sequence,
                owner_generation: self.owner,
            },
            &record,
        )
        .unwrap();
        self.digest
            .copy_from_slice(&frame[frame.len() - 40..frame.len() - 8]);
        self.bytes.extend(frame);
        self.sequence += 1;
    }
    fn base() -> Self {
        let mut j = Self::empty();
        j.append(Record::PendingInit(PendingInit {
            request: id(2),
            digest: request_digest(Request::Initialize { mode: 1 }).unwrap(),
            mode: 1,
            configuration_revision: 1,
            configuration_digest: [3; 32],
            graph: id(4),
        }));
        j.append(Record::ActiveBinding(ActiveBinding {
            request: id(2),
            generation: 1,
            graph: id(4),
            configuration_digest: [3; 32],
        }));
        j.append(Record::ProjectRegistered(ProjectRegistered {
            request: id(5),
            digest: request_digest(Request::Register {
                location: "/workspace",
            })
            .unwrap(),
            project: id(6),
            location: "/workspace".into(),
            device: 1,
            inode: 2,
            registry_revision: 1,
            visibility: 1,
        }));
        j
    }
    fn accepted() -> Self {
        let mut j = Self::base();
        j.append(Record::TurnAccepted(Box::new(turn())));
        j
    }
    fn started() -> Self {
        let mut j = Self::accepted();
        j.append(start());
        j
    }
}
fn turn() -> TurnAccepted {
    TurnAccepted {
        request: id(7),
        digest: request_digest(Request::Submit {
            project: id(6),
            conversation: None,
            expected_generation: 0,
            prompt: "hello",
        })
        .unwrap(),
        project: id(6),
        original_conversation: None,
        expected_generation: 0,
        conversation: id(8),
        generation: 1,
        task: id(9),
        operation: id(10),
        model: "system".into(),
        configuration_digest: [3; 32],
        instructions_digest: [11; 32],
        input_digest: [12; 32],
        prior_operations: vec![],
        prompt: "hello".into(),
        reserved_output_tokens: 512,
        event_cursor: 1,
        composition: None,
    }
}
fn start() -> Record {
    Record::StartAuthorized(StartAuthorized {
        operation: id(10),
        generation: 1,
        helper: id(13),
        input_digest: [12; 32],
    })
}
fn terminal(kind: TerminalKind, cause: Cause, known: bool) -> TurnTerminal {
    TurnTerminal {
        operation: id(10),
        generation: 1,
        kind,
        cause,
        final_cursor: u64::MAX,
        usage_known: known,
        output_tokens: if known { 12 } else { 0 },
        charged_tokens: if known { 12 } else { 512 },
        text: "reply".into(),
    }
}
#[test]
fn roundtrip_requests_terminal_offsets_and_unknown_complete_usage() {
    let mut j = Journal::started();
    j.append(Record::TurnTerminal(terminal(
        TerminalKind::Complete,
        Cause::None,
        false,
    )));
    let s = inspect(&j.bytes).unwrap();
    assert_eq!(s.sequence, 6);
    assert_eq!(s.reserved_bytes, 0);
    assert_eq!(
        s.requests[&id(7)].result,
        Outcome::Turn {
            conversation: id(8),
            generation: 1,
            task: id(9),
            operation: id(10)
        }
    );
    let t = s.operations[&id(10)].terminal.as_ref().unwrap();
    assert!(!t.usage_known);
    assert_eq!(t.charged_tokens, 512);
    let frame = &j.bytes[t.frame.clone()];
    let Record::TurnTerminal(decoded) = decode_record(8, &frame[144..frame.len() - 40]).unwrap()
    else {
        panic!("terminal")
    };
    assert_eq!(decoded.text, "reply");
    let summary = authority::replay(&j.bytes, &AtomicBool::new(false)).unwrap();
    assert_eq!(summary.state, authority::ReplayState::ActiveBinding);
    assert_eq!(summary.revision, s.revision);
    assert_eq!(summary.owner_generation, s.owner_generation);
}
#[test]
fn restart_never_reuses_permit_and_charges_conservatively() {
    for started in [false, true] {
        let mut j = if started {
            Journal::started()
        } else {
            Journal::accepted()
        };
        j.append(Record::OwnerGeneration);
        let mut p = terminal(TerminalKind::Interrupted, Cause::Restart, !started);
        if !started {
            p.output_tokens = 0;
            p.charged_tokens = 0;
        }
        j.append(Record::TurnTerminal(p));
        let s = inspect(&j.bytes).unwrap();
        assert_eq!(s.active_operation, None);
        assert_eq!(
            s.operations[&id(10)]
                .terminal
                .as_ref()
                .unwrap()
                .charged_tokens,
            if started { 512 } else { 0 }
        );
    }
    let mut j = Journal::accepted();
    j.append(Record::OwnerGeneration);
    j.append(start());
    assert_eq!(inspect(&j.bytes).unwrap_err(), ReplayError::Corrupt);
}
#[test]
fn cancellation_and_terminal_races_are_strict() {
    let cancel = Record::CancelRequested(CancelRequested {
        operation: id(10),
        generation: 1,
        cause: Cause::UserCancel,
    });
    let mut j = Journal::accepted();
    j.append(cancel.clone());
    j.append(start());
    assert!(inspect(&j.bytes).is_err());
    let mut j = Journal::started();
    j.append(cancel.clone());
    j.append(Record::TurnTerminal(terminal(
        TerminalKind::Complete,
        Cause::None,
        true,
    )));
    assert!(inspect(&j.bytes).is_err());
    let mut j = Journal::started();
    j.append(cancel.clone());
    j.append(Record::TurnTerminal(terminal(
        TerminalKind::Cancelled,
        Cause::UserCancel,
        true,
    )));
    assert!(inspect(&j.bytes).is_ok());
    j.append(cancel);
    assert!(inspect(&j.bytes).is_err());
}
#[test]
fn reconstructed_digests_ids_and_generations_reject_forgery() {
    let mut cases = Vec::new();
    let p = turn();
    let mut q = p.clone();
    q.prompt = "other".into();
    cases.push(q);
    let mut q = p.clone();
    q.task = q.operation;
    cases.push(q);
    let mut q = p.clone();
    q.expected_generation = 1;
    cases.push(q);
    let mut q = p.clone();
    q.conversation = id(6);
    cases.push(q);
    let mut q = p.clone();
    q.generation = 2;
    cases.push(q);
    let mut q = p.clone();
    q.reserved_output_tokens = 511;
    cases.push(q);
    let mut q = p.clone();
    q.request = id(5);
    cases.push(q);
    for q in cases {
        let mut j = Journal::base();
        j.append(Record::TurnAccepted(Box::new(q)));
        assert!(inspect(&j.bytes).is_err());
    }
    let mut j = Journal::accepted();
    j.append(Record::TurnAccepted(Box::new(p)));
    assert!(inspect(&j.bytes).is_err());
}
#[test]
fn history_must_be_complete_same_conversation_and_ordered() {
    let mut j = Journal::started();
    j.append(Record::TurnTerminal(terminal(
        TerminalKind::Complete,
        Cause::None,
        true,
    )));
    let mut p = turn();
    p.request = id(20);
    p.original_conversation = Some(id(8));
    p.expected_generation = 1;
    p.generation = 2;
    p.task = id(21);
    p.operation = id(22);
    p.prior_operations = vec![id(10)];
    p.digest = request_digest(Request::Submit {
        project: p.project,
        conversation: p.original_conversation,
        expected_generation: 1,
        prompt: &p.prompt,
    })
    .unwrap();
    j.append(Record::TurnAccepted(Box::new(p)));
    assert_eq!(
        inspect(&j.bytes).unwrap().conversations[&id(8)].generation,
        2
    );
    let mut j = Journal::started();
    j.append(Record::TurnTerminal(terminal(
        TerminalKind::Complete,
        Cause::None,
        true,
    )));
    let mut p = turn();
    p.request = id(20);
    p.conversation = id(23);
    p.task = id(21);
    p.operation = id(22);
    p.prior_operations = vec![id(10)];
    j.append(Record::TurnAccepted(Box::new(p)));
    assert!(inspect(&j.bytes).is_err());
}
/// Composed turns (kind 21) round-trip and account for every completed prior turn.
#[test]
fn composed_turns_record_budget_and_account_for_omitted_history() {
    let composed = |shown: Vec<Id>, omitted: u32| {
        let mut p = turn();
        p.request = id(20);
        p.original_conversation = Some(id(8));
        p.expected_generation = 1;
        p.generation = 2;
        p.task = id(21);
        p.operation = id(22);
        p.prior_operations = shown;
        p.digest = request_digest(Request::Submit {
            project: p.project,
            conversation: p.original_conversation,
            expected_generation: 1,
            prompt: &p.prompt,
        })
        .unwrap();
        p.composition = Some(Composition {
            omitted_turns: omitted,
            manifest_digest: [7; 32],
            composer_revision: 1,
            window_tokens: 4096,
            output_reservation: 2048,
            overhead_tokens: 300,
            bytes_per_token_milli: 1000,
        });
        p
    };
    let started = || {
        let mut j = Journal::started();
        j.append(Record::TurnTerminal(terminal(
            TerminalKind::Complete,
            Cause::None,
            true,
        )));
        j
    };
    for (shown, omitted) in [(vec![id(10)], 0), (vec![], 1)] {
        let p = composed(shown, omitted);
        let record = Record::TurnAccepted(Box::new(p.clone()));
        assert_eq!(record.kind(), 21);
        let mut j = started();
        j.append(record);
        let state = inspect(&j.bytes).unwrap();
        assert_eq!(state.conversations[&id(8)].generation, 2);
    }
    for (shown, omitted) in [(vec![id(10)], 1), (vec![], 0), (vec![], 2)] {
        let mut j = started();
        j.append(Record::TurnAccepted(Box::new(composed(shown, omitted))));
        assert!(inspect(&j.bytes).is_err());
    }
    let mut legacy = composed(vec![id(10)], 0);
    legacy.composition = None;
    assert_eq!(Record::TurnAccepted(Box::new(legacy)).kind(), 5);
}
#[test]
fn malformed_bytes_mixed_versions_and_truncated_frames_reject() {
    let j = Journal::base();
    for n in 0..j.bytes.len() {
        let mut damaged = j.bytes.clone();
        damaged[n] ^= 0x80;
        assert!(inspect(&damaged).is_err(), "byte {n}");
    }
    let first = u32::from_be_bytes(j.bytes[12..16].try_into().unwrap()) as usize;
    for n in 1..first {
        assert_eq!(
            inspect(&j.bytes[..n]).unwrap_err(),
            ReplayError::IncompleteTail
        );
    }
    let mut mixed = j.bytes[..first].to_vec();
    mixed.extend_from_slice(include_bytes!("fixtures/pending.bin"));
    assert_eq!(inspect(&mixed).unwrap_err(), ReplayError::UnsupportedFormat);
    assert!(decode_record(3, &[1]).is_err());
    assert_eq!(
        decode_record(255, &[]).unwrap_err(),
        ReplayError::UnsupportedFormat
    );
    assert!(
        request_digest(Request::Register {
            location: "/a/../b"
        })
        .is_err()
    );
    let mut p = turn();
    p.prompt = "x".repeat(MAX_PROMPT + 1);
    assert_eq!(
        encode_frame(
            &FrameContext {
                installation_id: id(1),
                transition_id: id(2),
                sequence: 1,
                prior_digest: [0; 32],
                expected_revision: 0,
                owner_generation: 1
            },
            &Record::TurnAccepted(Box::new(p))
        )
        .unwrap_err(),
        ReplayError::Limit
    );
}
#[test]
fn file_fixture_reopen_preserves_the_same_authority_result() {
    use std::fs::OpenOptions;
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let j = Journal::started();
    let path = std::env::temp_dir().join(format!(
        "asura-authority-conversation-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let mut f = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .unwrap();
    f.write_all(&j.bytes).unwrap();
    drop(f);
    let loaded = std::fs::read(&path).unwrap();
    std::fs::remove_file(path).unwrap();
    let s = inspect(&loaded).unwrap();
    assert_eq!(s.active_operation, Some(id(10)));
    assert_eq!(s.end_offset, loaded.len());
    assert_eq!(s.last_digest, j.digest);
}
#[test]
fn cancellation_has_no_partial_public_result() {
    let j = Journal::base();
    assert_eq!(
        replay(&j.bytes, &AtomicBool::new(true)).unwrap_err(),
        ReplayError::Cancelled
    );
}

#[test]
fn alias_preserves_original_digest_and_does_not_create_another_project() {
    let mut j = Journal::base();
    let alias = ProjectRequestAlias {
        request: id(30),
        digest: request_digest(Request::Register { location: "/alias" }).unwrap(),
        location: "/alias".into(),
        project: id(6),
        registry_revision: 1,
    };
    j.append(Record::ProjectRequestAlias(alias.clone()));
    let s = inspect(&j.bytes).unwrap();
    assert_eq!(s.projects.len(), 1);
    assert_eq!(s.requests[&id(30)].result, Outcome::Project(id(6)));
    let mut j = Journal::base();
    let mut wrong = alias;
    wrong.location = "/other".into();
    j.append(Record::ProjectRequestAlias(wrong));
    assert!(inspect(&j.bytes).is_err());
}
#[test]
fn project_capacity_does_not_discard_existing_entries() {
    let mut j = Journal::base();
    for n in 1..=MAX_PROJECTS {
        let location = format!("/project/{n}");
        j.append(Record::ProjectRegistered(ProjectRegistered {
            request: id(100 + 2 * n as u64),
            digest: request_digest(Request::Register {
                location: &location,
            })
            .unwrap(),
            project: id(101 + 2 * n as u64),
            location,
            device: 1,
            inode: 2 + n as u64,
            registry_revision: 1,
            visibility: 1,
        }));
        if n == MAX_PROJECTS - 1 {
            assert_eq!(inspect(&j.bytes).unwrap().projects.len(), MAX_PROJECTS);
        }
    }
    assert_eq!(inspect(&j.bytes).unwrap_err(), ReplayError::Limit);
}
#[test]
fn accepted_turn_reserves_closure_frames_before_the_frame_limit() {
    let mut j = Journal::base();
    while j.sequence < authority::MAX_FRAMES as u64 - 4 {
        j.append(Record::OwnerGeneration);
    }
    j.append(Record::TurnAccepted(Box::new(turn())));
    assert_eq!(inspect(&j.bytes).unwrap_err(), ReplayError::Limit);
}

fn seal(frame: &mut [u8]) {
    use sha2::{Digest, Sha256};
    let end = frame.len() - 40;
    let mut command = Sha256::new();
    command.update(&frame[10..12]);
    command.update(&frame[144..end]);
    frame[88..120].copy_from_slice(&command.finalize());
    let digest = Sha256::digest(&frame[..end]);
    frame[end..end + 32].copy_from_slice(&digest);
}

#[test]
fn format_one_and_distinct_initialization_kinds_are_enforced() {
    let journal = Journal::base();
    let first = u32::from_be_bytes(journal.bytes[12..16].try_into().unwrap()) as usize;
    let second_length =
        u32::from_be_bytes(journal.bytes[first + 12..first + 16].try_into().unwrap()) as usize;
    for (offset, kind) in [(0, 10u16), (first, 11), (first + second_length, 4)] {
        assert_eq!(&journal.bytes[offset + 8..offset + 10], &1u16.to_be_bytes());
        assert_eq!(
            &journal.bytes[offset + 10..offset + 12],
            &kind.to_be_bytes()
        );
    }
    let public = |bytes: &[u8]| authority::replay(bytes, &AtomicBool::new(false));
    let mut wrong_version = journal.bytes[..first].to_vec();
    wrong_version[8..10].copy_from_slice(&2u16.to_be_bytes());
    seal(&mut wrong_version);
    assert_eq!(
        inspect(&wrong_version).unwrap_err(),
        ReplayError::UnsupportedFormat
    );
    assert_eq!(public(&wrong_version), Err(ReplayError::UnsupportedFormat));

    // Correct checksums cannot make the new payload valid as diagnostic kind 1.
    let mut wrong_kind = journal.bytes[..first].to_vec();
    wrong_kind[10..12].copy_from_slice(&1u16.to_be_bytes());
    seal(&mut wrong_kind);
    assert_eq!(public(&wrong_kind), Err(ReplayError::Corrupt));

    // Nor may a diagnostic payload become request-bearing merely by changing its kind.
    let mut diagnostic = include_bytes!("fixtures/pending.bin").to_vec();
    diagnostic[10..12].copy_from_slice(&10u16.to_be_bytes());
    seal(&mut diagnostic);
    assert!(public(&diagnostic).is_err());

    // All earlier records remain valid here: inspection must validate the changed tail.
    let mut mixed = journal.bytes[..first + second_length].to_vec();
    mixed[first + 10..first + 12].copy_from_slice(&2u16.to_be_bytes());
    seal(&mut mixed[first..]);
    assert_eq!(public(&mixed), Err(ReplayError::UnsupportedFormat));
    assert_eq!(
        public(include_bytes!("fixtures/active.bin"))
            .unwrap()
            .revision,
        2
    );
}

fn queued(n: u64, previous: Option<Id>, kind: InputKind) -> InputQueued {
    let prompt = format!("follow-up {n}");
    InputQueued {
        request: id(n),
        digest: request_digest(Request::Queue {
            project: id(6),
            conversation: id(8),
            target_operation: id(10),
            target_generation: 1,
            kind,
            prompt: &prompt,
        })
        .unwrap(),
        dispatch_request: id(n + 100),
        project: id(6),
        conversation: id(8),
        target_operation: id(10),
        target_generation: 1,
        previous,
        kind,
        prompt,
    }
}
fn decision(request: u64, input: u64, action: InputAction) -> Record {
    Record::InputDecision(InputDecision {
        request: id(request),
        digest: request_digest(Request::InputDecision {
            input: id(input),
            action,
        })
        .unwrap(),
        input: id(input),
        action,
    })
}

fn queued_v2(
    request: u64,
    input: u64,
    dispatch: u64,
    conversation: u64,
    new_conversation: bool,
    revision: u64,
) -> InputQueuedV2 {
    let prompt = format!("queued {request}");
    InputQueuedV2 {
        request: id(request),
        digest: request_digest(Request::QueueInput {
            project: id(6),
            conversation: if new_conversation {
                None
            } else {
                Some(id(conversation))
            },
            expected_generation: 0,
            new_conversation,
            prompt: &prompt,
        })
        .unwrap(),
        input: id(input),
        dispatch_request: id(dispatch),
        project: id(6),
        conversation: id(conversation),
        expected_generation: 0,
        new_conversation,
        prompt,
        order_revision: revision,
    }
}

#[test]
fn v2_idle_first_input_reorder_replay_and_provisional_dispatch() {
    let mut j = Journal::base();
    let first = queued_v2(40, 41, 42, 43, true, 1);
    let second = queued_v2(44, 45, 46, 43, false, 2);
    j.append(Record::InputQueuedV2(first.clone()));
    j.append(Record::InputQueuedV2(second.clone()));
    let state = inspect(&j.bytes).unwrap();
    assert_eq!(state.conversations[&id(43)].generation, 0);
    assert_eq!(state.order_revision, 2);
    assert_eq!(state.ready_input(), Some(first.input));
    let reorder = InputReordered {
        request: id(47),
        digest: request_digest(Request::ReorderInput {
            input: second.input,
            after: None,
            expected_order_revision: 2,
        })
        .unwrap(),
        input: second.input,
        after: None,
        expected_order_revision: 2,
        order_revision: 3,
    };
    let reorder_start = j.bytes.len();
    j.append(Record::InputReordered(reorder.clone()));
    let state = inspect(&j.bytes).unwrap();
    assert_eq!(state.ready_input(), Some(second.input));
    assert_eq!(state.inputs[&second.input].order_position, 1);
    assert_eq!(state.order_revision, 3);
    let frame = &j.bytes[reorder_start..];
    assert_eq!(
        decode_record(19, &frame[144..frame.len() - 40]).unwrap(),
        Record::InputReordered(reorder)
    );

    let mut turn = turn();
    turn.request = second.dispatch_request;
    turn.original_conversation = Some(second.conversation);
    turn.expected_generation = 0;
    turn.conversation = second.conversation;
    turn.prompt = second.prompt;
    turn.digest = request_digest(Request::Submit {
        project: id(6),
        conversation: Some(id(43)),
        expected_generation: 0,
        prompt: &turn.prompt,
    })
    .unwrap();
    j.append(Record::TurnAccepted(Box::new(turn)));
    let state = inspect(&j.bytes).unwrap();
    assert_eq!(state.conversations[&id(43)].generation, 1);
    assert_eq!(state.input_status(second.input), Some(InputStatus::Running));
    assert_eq!(state.order_revision, 4);
    // A serial client may still hold generation zero while the first queued
    // message has already dispatched. The known lane accepts the later input.
    j.append(Record::InputQueuedV2(queued_v2(48, 49, 50, 43, false, 5)));
    let state = inspect(&j.bytes).unwrap();
    assert_eq!(state.input_status(id(49)), Some(InputStatus::Queued));
    assert_eq!(state.order_revision, 5);
}

#[test]
fn v2_rejects_revision_gap_and_holds_after_owner_restart() {
    let mut j = Journal::base();
    let first = queued_v2(40, 41, 42, 43, true, 1);
    j.append(Record::InputQueuedV2(first.clone()));
    let mut bad = queued_v2(44, 45, 46, 43, false, 3);
    j.append(Record::InputQueuedV2(bad.clone()));
    assert!(inspect(&j.bytes).is_err());
    // Build a fresh, valid chain rather than claiming a truncated tail is repairable.
    let mut j = Journal::base();
    bad.order_revision = 2;
    j.append(Record::InputQueuedV2(queued_v2(40, 41, 42, 43, true, 1)));
    j.append(Record::InputQueuedV2(bad));
    j.append(Record::OwnerGeneration);
    let state = inspect(&j.bytes).unwrap();
    assert_eq!(state.input_status(id(41)), Some(InputStatus::Held));
    assert_eq!(state.input_status(id(45)), Some(InputStatus::Held));
    assert_eq!(state.ready_input(), None);
}

#[test]
fn v2_committed_failure_or_cancellation_keeps_successors_eligible() {
    for (kind, cause) in [
        (TerminalKind::Failed, Cause::ProviderFailure),
        (TerminalKind::Failed, Cause::Deadline),
        (TerminalKind::Cancelled, Cause::UserCancel),
    ] {
        let mut j = Journal::started();
        let first = queued_v2(40, 41, 42, 8, false, 1);
        let second = queued_v2(44, 45, 46, 8, false, 2);
        j.append(Record::InputQueuedV2(first.clone()));
        if kind == TerminalKind::Cancelled {
            j.append(Record::CancelRequested(CancelRequested {
                operation: id(10),
                generation: 1,
                cause,
            }));
        }
        j.append(Record::TurnTerminal(terminal(kind, cause, false)));
        j.append(Record::InputQueuedV2(second.clone()));
        let state = inspect(&j.bytes).unwrap();
        assert_eq!(state.input_status(first.input), Some(InputStatus::Queued));
        assert_eq!(state.input_status(second.input), Some(InputStatus::Queued));
        assert_eq!(state.ready_input(), Some(first.input));

        let mut next = turn();
        next.request = first.dispatch_request;
        next.original_conversation = Some(id(8));
        next.expected_generation = 1;
        next.generation = 2;
        next.operation = id(30);
        next.task = id(31);
        next.prompt = first.prompt;
        next.digest = request_digest(Request::Submit {
            project: id(6),
            conversation: Some(id(8)),
            expected_generation: 1,
            prompt: &next.prompt,
        })
        .unwrap();
        j.append(Record::TurnAccepted(Box::new(next)));
        assert_eq!(
            inspect(&j.bytes).unwrap().input_status(first.input),
            Some(InputStatus::Running)
        );
        j.append(Record::TurnTerminal(TurnTerminal {
            operation: id(30),
            generation: 2,
            kind: TerminalKind::Failed,
            cause: Cause::ProviderFailure,
            final_cursor: u64::MAX,
            usage_known: true,
            output_tokens: 0,
            charged_tokens: 0,
            text: String::new(),
        }));
        let state = inspect(&j.bytes).unwrap();
        assert_eq!(state.input_status(first.input), Some(InputStatus::Failed));
        assert_eq!(state.ready_input(), Some(second.input));
        j.append(Record::OwnerGeneration);
        let state = inspect(&j.bytes).unwrap();
        assert_eq!(state.input_status(second.input), Some(InputStatus::Held));
        assert_eq!(state.ready_input(), None);
    }
}
#[test]
fn input_queue_is_durable_offset_backed_and_fifo_dispatch_is_atomic() {
    let mut j = Journal::started();
    let first = queued(20, None, InputKind::Queue);
    j.append(Record::InputQueued(first.clone()));
    j.append(Record::InputQueued(queued(
        21,
        Some(id(20)),
        InputKind::Queue,
    )));
    let state = inspect(&j.bytes).unwrap();
    assert_eq!(state.input_status(id(20)), Some(InputStatus::Queued));
    assert_eq!(state.ready_input(), None);
    let range = state.inputs[&id(20)].frame.clone();
    let frame = &j.bytes[range];
    assert_eq!(
        decode_record(12, &frame[144..frame.len() - 40]).unwrap(),
        Record::InputQueued(first.clone())
    );
    j.append(Record::TurnTerminal(terminal(
        TerminalKind::Complete,
        Cause::None,
        false,
    )));
    assert_eq!(inspect(&j.bytes).unwrap().ready_input(), Some(id(20)));
    let mut next = turn();
    next.request = first.dispatch_request;
    next.original_conversation = Some(id(8));
    next.expected_generation = 1;
    next.generation = 2;
    next.operation = id(30);
    next.task = id(31);
    next.prompt = first.prompt;
    next.digest = request_digest(Request::Submit {
        project: id(6),
        conversation: Some(id(8)),
        expected_generation: 1,
        prompt: &next.prompt,
    })
    .unwrap();
    j.append(Record::TurnAccepted(Box::new(next.clone())));
    let state = inspect(&j.bytes).unwrap();
    assert_eq!(state.input_status(id(20)), Some(InputStatus::Running));
    assert_eq!(state.input_operation(id(20)), Some((id(30), 2)));
    assert!(!state.can_promote_input(id(20), id(30), 2));
    assert_eq!(state.ready_input(), None);
    // Exact dispatch cannot be replayed as another operation.
    next.operation = id(32);
    next.task = id(33);
    j.append(Record::TurnAccepted(Box::new(next)));
    assert!(inspect(&j.bytes).is_err());
}
#[test]
fn failed_dependency_holds_successors_and_resume_drop_are_explicit() {
    let mut j = Journal::started();
    j.append(Record::InputQueued(queued(20, None, InputKind::Queue)));
    j.append(Record::InputQueued(queued(
        21,
        Some(id(20)),
        InputKind::Queue,
    )));
    j.append(Record::TurnTerminal(terminal(
        TerminalKind::Failed,
        Cause::ProviderFailure,
        false,
    )));
    let state = inspect(&j.bytes).unwrap();
    assert_eq!(state.input_status(id(20)), Some(InputStatus::Held));
    assert_eq!(state.input_status(id(21)), Some(InputStatus::Held));
    assert_eq!(state.ready_input(), None);
    j.append(decision(50, 20, InputAction::Resume));
    assert_eq!(inspect(&j.bytes).unwrap().ready_input(), Some(id(20)));
    j.append(decision(51, 20, InputAction::Hold));
    j.append(decision(52, 20, InputAction::Drop));
    let state = inspect(&j.bytes).unwrap();
    assert_eq!(state.input_status(id(20)), Some(InputStatus::Dropped));
    assert_eq!(state.input_status(id(21)), Some(InputStatus::Held));
    assert_eq!(state.ready_input(), None);
}
#[test]
fn restart_holds_queued_input_without_replaying_any_dispatch() {
    let mut j = Journal::started();
    j.append(Record::InputQueued(queued(20, None, InputKind::Queue)));
    j.append(Record::OwnerGeneration);
    j.append(Record::TurnTerminal(terminal(
        TerminalKind::Interrupted,
        Cause::Restart,
        false,
    )));
    let state = inspect(&j.bytes).unwrap();
    assert_eq!(state.input_status(id(20)), Some(InputStatus::Held));
    assert_eq!(state.ready_input(), None);
    assert_eq!(state.input_operation(id(20)), None);
    j.append(decision(50, 20, InputAction::Resume));
    assert_eq!(inspect(&j.bytes).unwrap().ready_input(), Some(id(20)));
}
#[test]
fn steering_requires_exact_cancel_cause_and_never_runs_after_restart() {
    for cause in [Cause::UserCancel, Cause::Steering, Cause::ServiceShutdown] {
        let mut j = Journal::started();
        j.append(Record::InputQueued(queued(20, None, InputKind::Steer)));
        assert_eq!(
            inspect(&j.bytes).unwrap().steering_input(id(10)),
            Some(id(20))
        );
        j.append(Record::CancelRequested(CancelRequested {
            operation: id(10),
            generation: 1,
            cause,
        }));
        j.append(Record::TurnTerminal(terminal(
            TerminalKind::Cancelled,
            cause,
            false,
        )));
        assert_eq!(
            inspect(&j.bytes).unwrap().ready_input(),
            if cause == Cause::Steering {
                Some(id(20))
            } else {
                None
            }
        );
        j.append(Record::OwnerGeneration);
        assert_eq!(
            inspect(&j.bytes).unwrap().input_status(id(20)),
            Some(InputStatus::Held)
        );
    }
}
#[test]
fn queue_rejects_stale_targets_duplicate_identity_and_overload() {
    let mut j = Journal::started();
    let mut wrong = queued(20, None, InputKind::Queue);
    wrong.target_generation = 2;
    j.append(Record::InputQueued(wrong));
    assert!(inspect(&j.bytes).is_err());
    let mut j = Journal::started();
    for n in 0..16 {
        j.append(Record::InputQueued(queued(
            20 + n,
            if n == 0 { None } else { Some(id(19 + n)) },
            InputKind::Queue,
        )));
    }
    assert_eq!(inspect(&j.bytes).unwrap().inputs.len(), 16);
    j.append(Record::InputQueued(queued(
        36,
        Some(id(35)),
        InputKind::Queue,
    )));
    assert_eq!(inspect(&j.bytes).unwrap_err(), ReplayError::Limit);
    let mut j = Journal::started();
    let input = queued(20, None, InputKind::Steer);
    j.append(Record::InputQueued(input.clone()));
    j.append(Record::InputQueued(input));
    assert!(inspect(&j.bytes).is_err());
}

fn tool_intent() -> Record {
    Record::ToolIntent(ToolIntent {
        operation: id(10),
        generation: 1,
        ordinal: 1,
        kind: 1,
        path: "README.md".into(),
        offset: 0,
        limit: 1024,
    })
}
fn tool_result() -> Record {
    Record::ToolResult(ToolResult {
        operation: id(10),
        generation: 1,
        ordinal: 1,
        status: 1,
        text: "observed".into(),
        next_offset: Some(8),
        truncated: false,
    })
}
#[test]
fn tool_records_require_started_operation_and_commit_before_complete() {
    let mut premature = Journal::accepted();
    premature.append(tool_intent());
    assert_eq!(inspect(&premature.bytes).unwrap_err(), ReplayError::Corrupt);
    let mut j = Journal::started();
    let baseline = inspect(&j.bytes).unwrap().reserved_bytes;
    j.append(tool_intent());
    let state = inspect(&j.bytes).unwrap();
    assert!(state.reserved_bytes >= baseline + 16_384);
    assert_eq!(state.operations[&id(10)].tools.len(), 1);
    j.append(Record::TurnTerminal(terminal(
        TerminalKind::Complete,
        Cause::None,
        false,
    )));
    assert_eq!(inspect(&j.bytes).unwrap_err(), ReplayError::Corrupt);
    // Construct a fresh chain instead of mutating its sequence after rejection.
    let mut j = Journal::started();
    j.append(tool_intent());
    j.append(tool_result());
    let state = inspect(&j.bytes).unwrap();
    assert_eq!(state.operations[&id(10)].tools[0].result_status, Some(1));
    assert_eq!(state.operations[&id(10)].tools[0].result_bytes, 8);
    j.append(Record::TurnTerminal(terminal(
        TerminalKind::Complete,
        Cause::None,
        false,
    )));
    assert!(inspect(&j.bytes).unwrap().active_operation.is_none());
}
#[test]
fn restart_interrupts_pending_tool_without_fabricating_result() {
    let mut j = Journal::started();
    j.append(tool_intent());
    j.append(Record::OwnerGeneration);
    j.append(Record::TurnTerminal(terminal(
        TerminalKind::Interrupted,
        Cause::Restart,
        false,
    )));
    let state = inspect(&j.bytes).unwrap();
    let operation = &state.operations[&id(10)];
    assert!(operation.tools[0].result_frame.is_none());
    assert_eq!(operation.terminal.as_ref().unwrap().charged_tokens, 512);
    assert!(state.active_operation.is_none());
    let mut duplicate = Journal::started();
    duplicate.append(tool_intent());
    duplicate.append(tool_result());
    duplicate.append(tool_result());
    assert_eq!(inspect(&duplicate.bytes).unwrap_err(), ReplayError::Corrupt);
}

#[test]
fn status_tool_result_replays_and_rejects_file_paging_or_oversized_payload() {
    let intent = ToolIntent {
        operation: id(10),
        generation: 1,
        ordinal: 1,
        kind: 3,
        path: String::new(),
        offset: 0,
        limit: 0,
    };
    let result = ToolResult {
        operation: id(10),
        generation: 1,
        ordinal: 1,
        status: 1,
        text: "{\"lifecycle\":1}".into(),
        next_offset: None,
        truncated: false,
    };
    let mut valid = Journal::started();
    valid.append(Record::ToolIntent(intent.clone()));
    valid.append(Record::ToolResult(result.clone()));
    let replay = inspect(&valid.bytes).unwrap();
    assert_eq!(replay.operations[&id(10)].tools[0].kind, 3);
    assert_eq!(replay.operations[&id(10)].tools[0].result_status, Some(1));
    valid.append(Record::TurnTerminal(terminal(
        TerminalKind::Complete,
        Cause::None,
        false,
    )));
    assert!(inspect(&valid.bytes).unwrap().active_operation.is_none());
    valid.append(Record::OwnerGeneration);
    let restarted = inspect(&valid.bytes).unwrap();
    let frame = restarted.operations[&id(10)].tools[0]
        .result_frame
        .clone()
        .unwrap();
    assert_eq!(
        decode_record(15, &valid.bytes[frame.start + 144..frame.end - 40]),
        Ok(Record::ToolResult(result.clone()))
    );
    let mut interrupted = Journal::started();
    interrupted.append(Record::ToolIntent(intent.clone()));
    interrupted.append(Record::OwnerGeneration);
    interrupted.append(Record::TurnTerminal(terminal(
        TerminalKind::Interrupted,
        Cause::Restart,
        false,
    )));
    let replay = inspect(&interrupted.bytes).unwrap();
    assert!(replay.operations[&id(10)].tools[0].result_frame.is_none());
    assert_eq!(
        replay.operations[&id(10)]
            .terminal
            .as_ref()
            .unwrap()
            .charged_tokens,
        512
    );
    for invalid in [
        ToolResult {
            next_offset: Some(1),
            ..result.clone()
        },
        ToolResult {
            truncated: true,
            ..result.clone()
        },
    ] {
        let mut journal = Journal::started();
        journal.append(Record::ToolIntent(intent.clone()));
        journal.append(Record::ToolResult(invalid));
        assert_eq!(inspect(&journal.bytes).unwrap_err(), ReplayError::Corrupt);
    }
    // The shared codec rejects oversized payloads before a frame can enter replay.
    let oversized = Record::ToolResult(ToolResult {
        text: "x".repeat(16_385),
        ..result
    });
    assert_eq!(
        encode_frame(
            &FrameContext {
                installation_id: id(1),
                transition_id: id(900),
                sequence: 1,
                prior_digest: [0; 32],
                expected_revision: 0,
                owner_generation: 1,
            },
            &oversized
        )
        .unwrap_err(),
        ReplayError::Corrupt
    );
}

#[test]
fn rejected_semantic_tool_intents_never_replay_success_or_host_data() {
    for kind in [4, 5, 9, 10, 11] {
        let intent = ToolIntent {
            operation: id(10),
            generation: 1,
            ordinal: 1,
            kind,
            path: "../outside".into(),
            offset: if matches!(kind, 4 | 10) { u64::MAX } else { 0 },
            limit: if matches!(kind, 4 | 10) { 16 } else { 0 },
        };
        let result = ToolResult {
            operation: id(10),
            generation: 1,
            ordinal: 1,
            status: 3,
            text: String::new(),
            next_offset: None,
            truncated: false,
        };
        for status in [3, 5, 6] {
            let mut journal = Journal::started();
            journal.append(Record::ToolIntent(intent.clone()));
            journal.append(Record::ToolResult(ToolResult {
                status,
                ..result.clone()
            }));
            assert_eq!(
                inspect(&journal.bytes).unwrap().operations[&id(10)].tools[0].result_status,
                Some(status)
            );
        }
        for invalid in [
            ToolResult {
                status: 1,
                ..result.clone()
            },
            ToolResult {
                status: 4,
                ..result.clone()
            },
            ToolResult {
                text: "host data".into(),
                ..result.clone()
            },
            ToolResult {
                next_offset: Some(0),
                ..result.clone()
            },
            ToolResult {
                truncated: true,
                ..result.clone()
            },
        ] {
            let mut journal = Journal::started();
            journal.append(Record::ToolIntent(intent.clone()));
            let frame = encode_frame(
                &FrameContext {
                    installation_id: id(1),
                    transition_id: id(900),
                    sequence: journal.sequence + 1,
                    prior_digest: journal.digest,
                    expected_revision: journal.sequence,
                    owner_generation: journal.owner,
                },
                &Record::ToolResult(invalid),
            );
            // Shared codec may reject malformed failure results before replay.
            if let Ok(frame) = frame {
                journal.bytes.extend(frame);
                assert_eq!(inspect(&journal.bytes).unwrap_err(), ReplayError::Corrupt);
            } else {
                assert_eq!(frame.unwrap_err(), ReplayError::Corrupt);
            }
        }
        let mut interrupted = Journal::started();
        interrupted.append(Record::ToolIntent(intent));
        interrupted.append(Record::OwnerGeneration);
        interrupted.append(Record::TurnTerminal(terminal(
            TerminalKind::Interrupted,
            Cause::Restart,
            false,
        )));
        let replay = inspect(&interrupted.bytes).unwrap();
        assert!(replay.operations[&id(10)].tools[0].result_frame.is_none());
        assert_eq!(
            replay.operations[&id(10)]
                .terminal
                .as_ref()
                .unwrap()
                .charged_tokens,
            512
        );
    }
}

fn promotion(request: u64, input: u64, target: u64, generation: u64) -> Record {
    Record::InputPromoted(InputPromoted {
        request: id(request),
        input: id(input),
        target_operation: id(target),
        target_generation: generation,
        digest: request_digest(Request::PromoteInput {
            input: id(input),
            target_operation: id(target),
            target_generation: generation,
        })
        .unwrap(),
    })
}
#[test]
fn promotion_preserves_identity_and_prompt_and_waits_for_steering_settlement() {
    let mut j = Journal::started();
    j.append(Record::InputQueued(queued(20, None, InputKind::Queue)));
    j.append(Record::InputQueued(queued(
        21,
        Some(id(20)),
        InputKind::Queue,
    )));
    let before = inspect(&j.bytes).unwrap().inputs[&id(21)].clone();
    j.append(promotion(50, 21, 10, 1));
    let state = inspect(&j.bytes).unwrap();
    let after = &state.inputs[&id(21)];
    assert_eq!(state.inputs.len(), 2);
    assert_eq!(after.request, before.request);
    assert_eq!(after.dispatch_request, before.dispatch_request);
    assert_eq!(after.frame, before.frame);
    assert_eq!(after.sequence, before.sequence);
    assert_eq!(after.previous, None);
    assert_eq!(after.kind, InputKind::Steer);
    assert_eq!(state.ready_input(), None);
    assert_eq!(state.steering_input(id(10)), Some(id(21)));
    assert!(!state.can_promote_input(id(20), id(10), 1));
    j.append(Record::CancelRequested(CancelRequested {
        operation: id(10),
        generation: 1,
        cause: Cause::Steering,
    }));
    j.append(Record::TurnTerminal(terminal(
        TerminalKind::Cancelled,
        Cause::Steering,
        false,
    )));
    let state = inspect(&j.bytes).unwrap();
    assert_eq!(state.ready_input(), Some(id(21)));
    assert_eq!(state.input_status(id(20)), Some(InputStatus::Held));
    j.append(Record::OwnerGeneration);
    assert_eq!(
        inspect(&j.bytes).unwrap().input_status(id(21)),
        Some(InputStatus::Held)
    );
    assert_eq!(inspect(&j.bytes).unwrap().ready_input(), None);
}
#[test]
fn promotion_replay_rejects_stale_unknown_cancelled_and_competing_targets() {
    for (target, generation) in [(10, 2), (99, 1)] {
        let mut j = Journal::started();
        j.append(Record::InputQueued(queued(20, None, InputKind::Queue)));
        j.append(promotion(50, 20, target, generation));
        assert!(inspect(&j.bytes).is_err());
    }
    for cancel in [false, true] {
        let mut j = Journal::started();
        j.append(Record::InputQueued(queued(20, None, InputKind::Queue)));
        if cancel {
            j.append(Record::CancelRequested(CancelRequested {
                operation: id(10),
                generation: 1,
                cause: Cause::UserCancel,
            }));
        } else {
            j.append(Record::InputQueued(queued(21, None, InputKind::Steer)));
        }
        j.append(promotion(50, 20, 10, 1));
        assert!(inspect(&j.bytes).is_err());
    }
}

#[test]
fn explicit_promotion_clears_hold_with_exact_active_target() {
    let mut j = Journal::started();
    j.append(Record::InputQueued(queued(20, None, InputKind::Queue)));
    j.append(decision(49, 20, InputAction::Hold));
    assert_eq!(
        inspect(&j.bytes).unwrap().input_status(id(20)),
        Some(InputStatus::Held)
    );
    j.append(promotion(50, 20, 10, 1));
    let state = inspect(&j.bytes).unwrap();
    assert_eq!(state.input_status(id(20)), Some(InputStatus::Queued));
    assert_eq!(state.steering_input(id(10)), Some(id(20)));
    assert_eq!(state.inputs.len(), 1);
}

#[test]
fn memory_tool_replay_keeps_exact_results_and_rejects_inconsistent_pagination() {
    for kind in [6, 7, 8] {
        let intent = ToolIntent {
            operation: id(10),
            generation: 1,
            ordinal: 1,
            kind,
            path: if kind == 6 {
                String::new()
            } else {
                "01".repeat(16)
            },
            offset: if kind == 7 { 3 } else { 0 },
            limit: match kind {
                6 => 8,
                7 => 10,
                _ => 0,
            },
        };
        let result = ToolResult {
            operation: id(10),
            generation: 1,
            ordinal: 1,
            status: 1,
            text: "€".into(),
            next_offset: (kind == 7).then_some(6),
            truncated: kind == 7,
        };
        let mut journal = Journal::started();
        journal.append(Record::ToolIntent(intent.clone()));
        journal.append(Record::ToolResult(result.clone()));
        let state = inspect(&journal.bytes).unwrap();
        let tool = &state.operations[&id(10)].tools[0];
        assert_eq!(tool.kind, kind);
        assert_eq!(tool.result_status, Some(1));
        assert_eq!(tool.result_bytes, 3);
        let invalid_results = if kind == 7 {
            vec![
                ToolResult {
                    next_offset: Some(4),
                    ..result.clone()
                },
                ToolResult {
                    next_offset: None,
                    ..result.clone()
                },
                ToolResult {
                    text: "x".repeat(11),
                    next_offset: Some(14),
                    ..result.clone()
                },
            ]
        } else {
            vec![
                ToolResult {
                    next_offset: Some(0),
                    ..result.clone()
                },
                ToolResult {
                    truncated: true,
                    ..result.clone()
                },
            ]
        };
        for invalid in invalid_results {
            let mut journal = Journal::started();
            journal.append(Record::ToolIntent(intent.clone()));
            journal.append(Record::ToolResult(invalid));
            assert_eq!(inspect(&journal.bytes).unwrap_err(), ReplayError::Corrupt);
        }
        let mut interrupted = Journal::started();
        interrupted.append(Record::ToolIntent(intent));
        interrupted.append(Record::OwnerGeneration);
        interrupted.append(Record::TurnTerminal(terminal(
            TerminalKind::Interrupted,
            Cause::Restart,
            false,
        )));
        assert!(
            inspect(&interrupted.bytes).unwrap().operations[&id(10)].tools[0]
                .result_frame
                .is_none()
        );
    }
}

#[test]
fn inventory_results_replay_without_file_pagination_or_redispatch() {
    let intent = ToolIntent {
        operation: id(10),
        generation: 1,
        ordinal: 1,
        kind: 14,
        path: String::new(),
        offset: 0,
        limit: 0,
    };
    let result = ToolResult {
        operation: id(10),
        generation: 1,
        ordinal: 1,
        status: 1,
        text: "service_list_tools\tList registered tools.\n".into(),
        next_offset: None,
        truncated: false,
    };
    let mut journal = Journal::started();
    journal.append(Record::ToolIntent(intent.clone()));
    journal.append(Record::ToolResult(result.clone()));
    let replay = inspect(&journal.bytes).unwrap();
    assert_eq!(replay.operations[&id(10)].tools[0].kind, 14);
    assert_eq!(replay.operations[&id(10)].tools[0].result_status, Some(1));
    for invalid in [
        ToolResult {
            next_offset: Some(0),
            ..result.clone()
        },
        ToolResult {
            truncated: true,
            ..result.clone()
        },
    ] {
        let mut journal = Journal::started();
        journal.append(Record::ToolIntent(intent.clone()));
        journal.append(Record::ToolResult(invalid));
        assert_eq!(inspect(&journal.bytes).unwrap_err(), ReplayError::Corrupt);
    }
    let mut interrupted = Journal::started();
    interrupted.append(Record::ToolIntent(intent));
    interrupted.append(Record::OwnerGeneration);
    interrupted.append(Record::TurnTerminal(terminal(
        TerminalKind::Interrupted,
        Cause::Restart,
        false,
    )));
    assert!(
        inspect(&interrupted.bytes).unwrap().operations[&id(10)].tools[0]
            .result_frame
            .is_none()
    );
}

fn exec_cmd_intent(ordinal: u32) -> ToolIntent {
    ToolIntent {
        operation: id(10),
        generation: 1,
        ordinal,
        kind: EXEC_CMD_TOOL_KIND,
        path: "printf proof\0.".into(),
        offset: 1000,
        limit: 30,
    }
}
fn exec_cmd_result(ordinal: u32, status: u8) -> ToolResult {
    ToolResult {
        operation: id(10),
        generation: 1,
        ordinal,
        status,
        text: "stdout: partial proof\nstderr: warning".into(),
        next_offset: None,
        truncated: true,
    }
}
#[test]
fn exec_cmd_captured_outcomes_bind_to_intent_and_preserve_other_tool_rules() {
    for status in [1, 5, 6, 7] {
        let mut journal = Journal::started();
        journal.append(Record::ToolIntent(exec_cmd_intent(1)));
        journal.append(Record::ToolResult(exec_cmd_result(1, status)));
        assert_eq!(
            inspect(&journal.bytes).unwrap().operations[&id(10)].tools[0].result_status,
            Some(status)
        );
    }
    let mut journal = Journal::started();
    journal.append(Record::ToolIntent(exec_cmd_intent(1)));
    journal.append(Record::ToolResult(ToolResult {
        next_offset: Some(1),
        ..exec_cmd_result(1, 1)
    }));
    assert_eq!(inspect(&journal.bytes).unwrap_err(), ReplayError::Corrupt);
    for status in [5, 6, 7] {
        let mut journal = Journal::started();
        journal.append(Record::ToolIntent(ToolIntent {
            kind: 14,
            path: String::new(),
            offset: 0,
            limit: 0,
            ..exec_cmd_intent(1)
        }));
        journal.append(Record::ToolResult(exec_cmd_result(1, status)));
        assert_eq!(inspect(&journal.bytes).unwrap_err(), ReplayError::Corrupt);
    }
}
#[test]
fn exec_cmd_unresolved_restart_has_no_result_or_new_execution_authority() {
    let intent = exec_cmd_intent(1);
    let mut journal = Journal::started();
    journal.append(Record::ToolIntent(intent.clone()));
    journal.append(Record::OwnerGeneration);
    let state = inspect(&journal.bytes).unwrap();
    assert!(state.operations[&id(10)].tools[0].result_frame.is_none());
    journal.append(Record::TurnTerminal(terminal(
        TerminalKind::Interrupted,
        Cause::Restart,
        false,
    )));
    let state = inspect(&journal.bytes).unwrap();
    assert!(state.operations[&id(10)].tools[0].result_frame.is_none());
    for changed in [false, true] {
        let mut journal = Journal::started();
        journal.append(Record::ToolIntent(intent.clone()));
        let mut duplicate = intent.clone();
        if changed {
            duplicate.path = "echo different\0.".into();
        }
        journal.append(Record::ToolIntent(duplicate));
        assert_eq!(inspect(&journal.bytes).unwrap_err(), ReplayError::Corrupt);
    }
    let mut journal = Journal::started();
    journal.append(Record::OwnerGeneration);
    journal.append(Record::ToolIntent(intent));
    assert_eq!(inspect(&journal.bytes).unwrap_err(), ReplayError::Corrupt);
}
#[test]
fn exec_cmd_results_enforce_existing_aggregate_result_budget() {
    let mut journal = Journal::started();
    for ordinal in 1..=4 {
        journal.append(Record::ToolIntent(exec_cmd_intent(ordinal)));
        journal.append(Record::ToolResult(ToolResult {
            text: "x".repeat(16_384),
            ..exec_cmd_result(ordinal, 5)
        }));
    }
    assert!(inspect(&journal.bytes).is_ok());
    journal.append(Record::ToolIntent(exec_cmd_intent(5)));
    journal.append(Record::ToolResult(ToolResult {
        text: "x".into(),
        ..exec_cmd_result(5, 5)
    }));
    assert_eq!(inspect(&journal.bytes).unwrap_err(), ReplayError::Corrupt);
}

#[test]
fn response_accounting_uses_each_recorded_reservation_for_new_and_legacy_turns() {
    assert_eq!(OUTPUT_RESERVATION, 2048);
    for reservation in [LEGACY_OUTPUT_RESERVATION, OUTPUT_RESERVATION] {
        for known in [false, true] {
            let mut journal = Journal::base();
            let mut admitted = turn();
            admitted.reserved_output_tokens = reservation;
            journal.append(Record::TurnAccepted(Box::new(admitted)));
            journal.append(start());
            let mut result = terminal(TerminalKind::Complete, Cause::None, known);
            result.output_tokens = if known { reservation } else { 0 };
            result.charged_tokens = reservation;
            journal.append(Record::TurnTerminal(result));
            let state = inspect(&journal.bytes).unwrap();
            let operation = &state.operations[&id(10)];
            assert_eq!(operation.reserved_output_tokens, reservation);
            assert_eq!(
                operation.terminal.as_ref().unwrap().charged_tokens,
                reservation
            );
        }
        for (known, output, charged) in [
            (true, reservation + 1, reservation + 1),
            (false, 0, if reservation == 512 { 2048 } else { 512 }),
        ] {
            let mut journal = Journal::base();
            let mut admitted = turn();
            admitted.reserved_output_tokens = reservation;
            journal.append(Record::TurnAccepted(Box::new(admitted)));
            journal.append(start());
            let mut result = terminal(TerminalKind::Complete, Cause::None, known);
            result.output_tokens = output;
            result.charged_tokens = charged;
            journal.append(Record::TurnTerminal(result));
            assert_eq!(inspect(&journal.bytes).unwrap_err(), ReplayError::Corrupt);
        }
        let mut journal = Journal::base();
        let mut admitted = turn();
        admitted.reserved_output_tokens = reservation;
        journal.append(Record::TurnAccepted(Box::new(admitted)));
        journal.append(start());
        journal.append(Record::OwnerGeneration);
        let mut interrupted = terminal(TerminalKind::Interrupted, Cause::Restart, false);
        interrupted.charged_tokens = reservation;
        journal.append(Record::TurnTerminal(interrupted));
        assert_eq!(
            inspect(&journal.bytes).unwrap().operations[&id(10)]
                .terminal
                .as_ref()
                .unwrap()
                .charged_tokens,
            reservation
        );
    }
    for invalid in [0, 511, 513, 1024, 2047, 2049, u32::MAX] {
        let mut journal = Journal::base();
        let mut admitted = turn();
        admitted.reserved_output_tokens = invalid;
        journal.append(Record::TurnAccepted(Box::new(admitted)));
        assert_eq!(inspect(&journal.bytes).unwrap_err(), ReplayError::Corrupt);
    }
}

fn create_intent() -> MemoryCreateIntent {
    let ids = CreateNoteIdentity::derive([id(1), id(4), id(2)], id(10), 1, 1).unwrap();
    MemoryCreateIntent {
        operation: id(10),
        generation: 1,
        ordinal: 1,
        project: id(6),
        memory_operation: ids.operation,
        object: ids.object,
        version: ids.version,
        body_bytes: 4,
        body_sha256: [44; 32],
        source: Some((id(100), ids.edge)),
        command_sha256: [45; 32],
    }
}
#[test]
fn memory_create_replay_gates_publication_and_requires_exact_success() {
    let intent = create_intent();
    let mut journal = Journal::started();
    journal.append(Record::MemoryCreateIntent(intent.clone()));
    let state = inspect(&journal.bytes).unwrap();
    assert!(state.project_memory_blocked(id(6)));
    assert!(!state.project_memory_blocked(id(99)));
    assert_eq!(state.unresolved_creates().next(), Some(&intent));
    let before = journal.bytes.clone();
    journal.append(Record::TurnTerminal(terminal(
        TerminalKind::Interrupted,
        Cause::Restart,
        false,
    )));
    assert!(inspect(&journal.bytes).is_err());
    journal.bytes = before;
    journal.sequence -= 1;
    journal.digest = state.last_digest;
    journal.append(Record::ToolResult(ToolResult {
        operation: id(10),
        generation: 1,
        ordinal: 1,
        status: 1,
        text: "invented receipt".into(),
        truncated: false,
        next_offset: None,
    }));
    assert!(inspect(&journal.bytes).is_err());
}
#[test]
fn memory_create_old_owner_can_resolve_but_cannot_write_new_intent() {
    let intent = create_intent();
    let mut journal = Journal::started();
    journal.append(Record::MemoryCreateIntent(intent.clone()));
    journal.append(Record::OwnerGeneration);
    journal.append(Record::ToolResult(ToolResult {
        operation: id(10),
        generation: 1,
        ordinal: 1,
        status: 1,
        text: intent.success_text(),
        truncated: false,
        next_offset: None,
    }));
    let state = inspect(&journal.bytes).unwrap();
    assert!(!state.project_memory_blocked(id(6)));
    journal.append(Record::TurnTerminal(terminal(
        TerminalKind::Interrupted,
        Cause::Restart,
        false,
    )));
    assert!(inspect(&journal.bytes).is_ok());
    for mutate in [0, 1, 2] {
        let mut journal = Journal::started();
        let mut intent = create_intent();
        match mutate {
            0 => intent.project = id(99),
            1 => intent.memory_operation = id(98),
            _ => {
                journal.append(Record::OwnerGeneration);
            }
        }
        journal.append(Record::MemoryCreateIntent(intent));
        assert!(inspect(&journal.bytes).is_err());
    }
}

#[test]
fn old_owner_result_exception_cannot_resolve_an_ordinary_tool_or_wrong_create() {
    let mut journal = Journal::started();
    journal.append(Record::ToolIntent(ToolIntent {
        operation: id(10),
        generation: 1,
        ordinal: 1,
        kind: 14,
        path: String::new(),
        offset: 0,
        limit: 0,
    }));
    journal.append(Record::OwnerGeneration);
    journal.append(Record::ToolResult(ToolResult {
        operation: id(10),
        generation: 1,
        ordinal: 1,
        status: 1,
        text: "inventory".into(),
        truncated: false,
        next_offset: None,
    }));
    assert!(inspect(&journal.bytes).is_err());
    for wrong_operation in [true, false] {
        let intent = create_intent();
        let mut journal = Journal::started();
        journal.append(Record::MemoryCreateIntent(intent.clone()));
        journal.append(Record::OwnerGeneration);
        journal.append(Record::ToolResult(ToolResult {
            operation: if wrong_operation { id(99) } else { id(10) },
            generation: 1,
            ordinal: if wrong_operation { 1 } else { 2 },
            status: 1,
            text: intent.success_text(),
            truncated: false,
            next_offset: None,
        }));
        assert!(inspect(&journal.bytes).is_err());
    }
}

#[test]
fn reopened_tool_display_projections_are_bounded_and_capture_stdout() {
    let mut journal = Journal::started();
    let mut intent = exec_cmd_intent(1);
    intent.path = format!("printf {}\0.", "界".repeat(100));
    journal.append(Record::ToolIntent(intent));
    journal.append(Record::ToolResult(ToolResult {
        text: format!("failure=None exit_code=17 signal=none reason=none truncated=true cleanup_confirmed=true\nstdout:\n{}\nstderr:\nwarning", "界".repeat(100)),
        ..exec_cmd_result(1, 1)
    }));
    let state = inspect(&journal.bytes).unwrap();
    let tool = &state.operations[&id(10)].tools[0];
    assert!(tool.arguments.len() <= 192);
    assert!(tool.preview.as_ref().unwrap().len() <= 128);
    assert!(tool.preview.as_ref().unwrap().starts_with("界"));
    assert_eq!(tool.exit_code, Some(17));
    assert_eq!(tool.truncated, Some(true));
    assert_eq!(
        inspect(&journal.bytes).unwrap().operations[&id(10)].tools[0],
        *tool
    );
}

// J1T-02: kinds 22, 23 and 15 replay order and reservation.
const FRAME_OVERHEAD: usize = 144 + 40;
const RESULT_CAPACITY: usize = FRAME_OVERHEAD + 43 + 16_384;
const CAPTURE_CAPACITY: usize = FRAME_OVERHEAD + 4_102;
fn exec_cmd22(ordinal: u32, disposition: ExecCmdDisposition) -> ExecCmdIntent {
    use asura_storage::payload::{ObjectKind, payload_id};
    let admitted = disposition == ExecCmdDisposition::Admitted;
    let derived = |kind| payload_id(id(1), id(10), 1, ordinal, kind);
    ExecCmdIntent {
        operation: id(10),
        generation: 1,
        ordinal,
        disposition,
        command_truncated: false,
        command: "printf \"$ASURA_SECRET_1\"".into(),
        cwd: ".".into(),
        refusal: (disposition == ExecCmdDisposition::Refused).then_some(ExecCmdRefusal {
            code: RefusalCode::SecretSingleQuoted,
            kind: Some(SecretKind::GithubToken),
            word: Some(2),
        }),
        busy: (disposition == ExecCmdDisposition::Busy).then_some(ExecCmdBusyReason::Quota),
        variables: if admitted {
            vec![SecretKind::GithubToken]
        } else {
            vec![]
        },
        job: if admitted {
            exec_cmd_job_id(id(10), 1, ordinal)
        } else {
            [0; 16]
        },
        stdout_payload: if admitted {
            derived(ObjectKind::Stdout)
        } else {
            [0; 16]
        },
        stderr_payload: if admitted {
            derived(ObjectKind::Stderr)
        } else {
            [0; 16]
        },
        initial_reservation: if admitted { INITIAL_RESERVATION } else { 0 },
        rule_set_revision: 1,
        rule_set_digest: [21; 32],
    }
}
fn stream23(stored: u64) -> CaptureStream {
    CaptureStream {
        observed_bytes: stored,
        stored_bytes: stored,
        digest: Some([22; 32]),
        eof: true,
        ..CaptureStream::default()
    }
}
fn settled23(ordinal: u32) -> CaptureSettled {
    CaptureSettled {
        operation: id(10),
        generation: 1,
        ordinal,
        state: CaptureState::Complete,
        reason: CaptureReason::None,
        exit_kind: 1,
        exit_value: 3,
        rule_set_revision: 1,
        rule_set_digest: [21; 32],
        stdout: stream23(5),
        stderr: stream23(0),
        distinct_numbers: 0,
        numbers: vec![],
        pause_count: 0,
        first_pause_ms: 0,
        paused_ms: 0,
        final_reservation: INITIAL_RESERVATION,
    }
}
fn result15(ordinal: u32, status: u8) -> ToolResult {
    ToolResult {
        operation: id(10),
        generation: 1,
        ordinal,
        status,
        text: "failure=None exit_code=3 signal=none reason=none truncated=false cleanup_confirmed=true capture=complete\nstdout:\nhello\nstderr:\n".into(),
        next_offset: None,
        truncated: false,
    }
}
fn r22(ordinal: u32, disposition: ExecCmdDisposition) -> Record {
    Record::ExecCmdIntent(Box::new(exec_cmd22(ordinal, disposition)))
}
fn r23(ordinal: u32) -> Record {
    Record::CaptureSettled(Box::new(settled23(ordinal)))
}
fn r15(ordinal: u32, status: u8) -> Record {
    Record::ToolResult(result15(ordinal, status))
}
fn complete() -> Record {
    Record::TurnTerminal(terminal(TerminalKind::Complete, Cause::None, true))
}
fn failed() -> Record {
    Record::TurnTerminal(terminal(
        TerminalKind::Failed,
        Cause::ProviderFailure,
        false,
    ))
}
fn started_with(records: impl IntoIterator<Item = Record>) -> Journal {
    let mut journal = Journal::started();
    for record in records {
        journal.append(record);
    }
    journal
}
#[test]
fn j1t_02_each_disposition_with_and_without_capture_result_and_terminal() {
    use ExecCmdDisposition::*;
    let exec_cmd = |journal: &Journal| {
        let state = inspect(&journal.bytes).unwrap();
        state.operations[&id(10)].tools[0].clone()
    };
    // Admitted: 22, then 23, then 15, then a terminal.
    let journal = started_with([r22(1, Admitted)]);
    let tool = exec_cmd(&journal);
    let call = tool.exec_cmd.as_deref().unwrap();
    assert_eq!(tool.kind, EXEC_CMD_TOOL_KIND);
    assert_eq!(tool.arguments, "printf \"$ASURA_SECRET_1\" cwd=.");
    assert_eq!((tool.offset, tool.limit), (0, 0));
    assert_eq!(call.disposition, Admitted);
    assert_eq!(call.job, exec_cmd_job_id(id(10), 1, 1));
    assert_eq!(call.intent_sequence, journal.sequence);
    assert!(call.settled.is_none() && tool.result_frame.is_none());
    let journal = started_with([r22(1, Admitted), r23(1)]);
    let tool = exec_cmd(&journal);
    let settled = tool.exec_cmd.as_deref().unwrap().settled.clone().unwrap();
    assert_eq!(settled.state, CaptureState::Complete);
    assert_eq!(settled.reason, CaptureReason::None);
    assert_eq!(settled.sequence, journal.sequence);
    assert_eq!(settled.streams, [(5, Some([22; 32])), (0, Some([22; 32]))]);
    assert!(tool.result_frame.is_none());
    let journal = started_with([r22(1, Admitted), r23(1), r15(1, 1)]);
    let tool = exec_cmd(&journal);
    assert_eq!(tool.result_status, Some(1));
    assert_eq!(tool.exit_code, Some(3));
    assert_eq!(tool.preview.as_deref(), Some("hello"));
    assert!(tool.exec_cmd.as_deref().unwrap().settled.is_some());
    for journal in [
        started_with([r22(1, Admitted), r23(1), r15(1, 1), complete()]),
        started_with([r22(1, Admitted), r23(1), r15(1, 5), failed()]),
        started_with([r22(1, Admitted), r23(1), failed()]),
    ] {
        let state = inspect(&journal.bytes).unwrap();
        assert!(state.operations[&id(10)].terminal.is_some());
        assert_eq!(state.active_operation, None);
    }
    // Refused and busy: 22 then 15 with status 3 or 4, then a terminal.
    for (disposition, status) in [(Refused, 3), (Busy, 4)] {
        let journal = started_with([r22(1, disposition)]);
        let tool = exec_cmd(&journal);
        let call = tool.exec_cmd.as_deref().unwrap();
        assert_eq!(call.disposition, disposition);
        assert_eq!(
            [call.job, call.stdout_payload, call.stderr_payload],
            [[0; 16]; 3]
        );
        let journal = started_with([r22(1, disposition), r15(1, status)]);
        assert_eq!(exec_cmd(&journal).result_status, Some(status));
        for end in [complete(), failed()] {
            let journal = started_with([r22(1, disposition), r15(1, status), end]);
            assert!(
                inspect(&journal.bytes).unwrap().operations[&id(10)]
                    .terminal
                    .is_some()
            );
        }
        let journal = started_with([r22(1, disposition), failed()]);
        assert!(inspect(&journal.bytes).is_ok());
    }
    // Calls follow one another; a legacy kind 14 exec_cmd keeps its rules.
    let journal = started_with([
        r22(1, Refused),
        r15(1, 3),
        Record::ToolIntent(exec_cmd_intent(2)),
        Record::ToolResult(exec_cmd_result(2, 1)),
        r22(3, Admitted),
        r23(3),
        r15(3, 1),
        r22(4, Busy),
        r15(4, 4),
        complete(),
    ]);
    let state = inspect(&journal.bytes).unwrap();
    let tools = &state.operations[&id(10)].tools;
    assert_eq!(tools.len(), 4);
    assert!(tools[1].exec_cmd.is_none());
    assert_eq!(tools[2].exec_cmd.as_deref().unwrap().disposition, Admitted);
}
#[test]
fn j1t_02_every_invalid_order_is_corrupt() {
    use ExecCmdDisposition::*;
    let mismatched = |change: fn(&mut ExecCmdIntent)| {
        let mut p = exec_cmd22(1, Admitted);
        change(&mut p);
        Record::ExecCmdIntent(Box::new(p))
    };
    let other_rule_set = |change: fn(&mut CaptureSettled)| {
        let mut p = settled23(1);
        change(&mut p);
        Record::CaptureSettled(Box::new(p))
    };
    let cases: Vec<(&str, Journal, Vec<Record>)> = vec![
        ("23 before 22", Journal::started(), vec![r23(1)]),
        (
            "23 twice",
            Journal::started(),
            vec![r22(1, Admitted), r23(1), r23(1)],
        ),
        (
            "15 before 23",
            Journal::started(),
            vec![r22(1, Admitted), r15(1, 1)],
        ),
        (
            "15 status 3 before 23",
            Journal::started(),
            vec![r22(1, Admitted), r15(1, 3)],
        ),
        (
            "23 after 15",
            Journal::started(),
            vec![r22(1, Admitted), r23(1), r15(1, 1), r23(1)],
        ),
        (
            "23 after refused",
            Journal::started(),
            vec![r22(1, Refused), r23(1)],
        ),
        (
            "23 after busy",
            Journal::started(),
            vec![r22(1, Busy), r23(1)],
        ),
        (
            "refused then 15 status 1",
            Journal::started(),
            vec![r22(1, Refused), r15(1, 1)],
        ),
        (
            "refused then 15 status 4",
            Journal::started(),
            vec![r22(1, Refused), r15(1, 4)],
        ),
        (
            "busy then 15 status 3",
            Journal::started(),
            vec![r22(1, Busy), r15(1, 3)],
        ),
        (
            "busy then 15 status 5",
            Journal::started(),
            vec![r22(1, Busy), r15(1, 5)],
        ),
        (
            "22 after terminal",
            Journal::started(),
            vec![failed(), r22(1, Admitted)],
        ),
        (
            "23 after terminal",
            Journal::started(),
            vec![r22(1, Admitted), failed(), r23(1)],
        ),
        (
            "15 after terminal",
            Journal::started(),
            vec![r22(1, Admitted), r23(1), failed(), r15(1, 1)],
        ),
        (
            "15 after terminal, refused",
            Journal::started(),
            vec![r22(1, Refused), failed(), r15(1, 3)],
        ),
        (
            "complete while 23 pending",
            Journal::started(),
            vec![r22(1, Admitted), complete()],
        ),
        (
            "complete while 15 pending",
            Journal::started(),
            vec![r22(1, Admitted), r23(1), complete()],
        ),
        (
            "job ID mismatch",
            Journal::started(),
            vec![mismatched(|p| p.job = exec_cmd_job_id(id(10), 1, 2))],
        ),
        (
            "stdout ID mismatch",
            Journal::started(),
            vec![mismatched(|p| p.stdout_payload = p.stderr_payload)],
        ),
        (
            "stderr ID mismatch",
            Journal::started(),
            vec![mismatched(|p| p.stderr_payload = [7; 16])],
        ),
        (
            "IDs of another installation",
            Journal::started(),
            vec![mismatched(|p| {
                p.stdout_payload = asura_storage::payload::payload_id(
                    id(2),
                    id(10),
                    1,
                    1,
                    asura_storage::payload::ObjectKind::Stdout,
                )
            })],
        ),
        (
            "23 rule-set revision differs",
            Journal::started(),
            vec![
                r22(1, Admitted),
                other_rule_set(|p| p.rule_set_revision = 2),
            ],
        ),
        (
            "23 rule-set digest differs",
            Journal::started(),
            vec![
                r22(1, Admitted),
                other_rule_set(|p| p.rule_set_digest = [9; 32]),
            ],
        ),
        (
            "23 wrong ordinal",
            Journal::started(),
            vec![r22(1, Admitted), other_rule_set(|p| p.ordinal = 2)],
        ),
        (
            "23 wrong generation",
            Journal::started(),
            vec![r22(1, Admitted), other_rule_set(|p| p.generation = 2)],
        ),
        (
            "23 for a legacy kind 14 exec_cmd",
            Journal::started(),
            vec![Record::ToolIntent(exec_cmd_intent(1)), r23(1)],
        ),
        (
            "22 skips an ordinal",
            Journal::started(),
            vec![r22(2, Admitted)],
        ),
        (
            "22 while 22 pending",
            Journal::started(),
            vec![r22(1, Refused), r22(2, Refused)],
        ),
        (
            "22 while 15 pending",
            Journal::started(),
            vec![r22(1, Admitted), r23(1), r22(2, Admitted)],
        ),
        (
            "22 before start",
            Journal::accepted(),
            vec![r22(1, Admitted)],
        ),
        (
            "22 after cancel",
            Journal::started(),
            vec![
                Record::CancelRequested(CancelRequested {
                    operation: id(10),
                    generation: 1,
                    cause: Cause::UserCancel,
                }),
                r22(1, Admitted),
            ],
        ),
        (
            "22 after restart",
            Journal::started(),
            vec![Record::OwnerGeneration, r22(1, Admitted)],
        ),
        (
            "23 after restart",
            Journal::started(),
            vec![r22(1, Admitted), Record::OwnerGeneration, r23(1)],
        ),
        (
            "15 after restart",
            Journal::started(),
            vec![r22(1, Admitted), r23(1), Record::OwnerGeneration, r15(1, 1)],
        ),
    ];
    for (name, mut journal, records) in cases {
        for record in records {
            journal.append(record);
        }
        assert_eq!(
            inspect(&journal.bytes).unwrap_err(),
            ReplayError::Corrupt,
            "{name}"
        );
    }
    // A ninth tool call.
    let mut journal = Journal::started();
    for ordinal in 1..=8 {
        journal.append(r22(ordinal, Refused));
        journal.append(r15(ordinal, 3));
    }
    assert!(inspect(&journal.bytes).is_ok());
    // A ninth ordinal cannot even be encoded.
    let ninth = FrameContext {
        installation_id: id(1),
        transition_id: id(99_999),
        sequence: journal.sequence + 1,
        prior_digest: journal.digest,
        expected_revision: journal.sequence,
        owner_generation: journal.owner,
    };
    assert_eq!(
        encode_frame(&ninth, &r22(9, Refused)),
        Err(ReplayError::Corrupt)
    );
}
#[test]
fn j1t_02_terminal_with_admitted_intent_lacking_capture_is_accepted() {
    let journal = started_with([
        r22(1, ExecCmdDisposition::Admitted),
        Record::OwnerGeneration,
        Record::TurnTerminal(terminal(TerminalKind::Interrupted, Cause::Restart, false)),
    ]);
    let state = inspect(&journal.bytes).unwrap();
    let tool = &state.operations[&id(10)].tools[0];
    assert!(tool.exec_cmd.as_deref().unwrap().settled.is_none());
    assert!(tool.result_frame.is_none());
    assert_eq!(state.active_operation, None);
    for end in [
        failed(),
        Record::TurnTerminal(terminal(TerminalKind::Cancelled, Cause::UserCancel, false)),
    ] {
        let mut journal = started_with([r22(1, ExecCmdDisposition::Admitted)]);
        if matches!(&end, Record::TurnTerminal(t) if t.kind == TerminalKind::Cancelled) {
            journal.append(Record::CancelRequested(CancelRequested {
                operation: id(10),
                generation: 1,
                cause: Cause::UserCancel,
            }));
        }
        journal.append(end);
        let state = inspect(&journal.bytes).unwrap();
        assert!(
            state.operations[&id(10)].tools[0]
                .exec_cmd
                .as_deref()
                .unwrap()
                .settled
                .is_none()
        );
    }
}
#[test]
fn j1t_02_reserve_holds_one_capture_frame_until_kind_23() {
    use ExecCmdDisposition::*;
    let reserved = |journal: &Journal| {
        let state = inspect(&journal.bytes).unwrap();
        (state.reserved_bytes, state.reserved_frames)
    };
    let (base, frames) = reserved(&Journal::started());
    assert_eq!(CAPTURE_CAPACITY, 4_286);
    assert_eq!(
        reserved(&started_with([r22(1, Admitted)])),
        (base + RESULT_CAPACITY + CAPTURE_CAPACITY, frames + 2)
    );
    assert_eq!(
        reserved(&started_with([r22(1, Admitted), r23(1)])),
        (base + RESULT_CAPACITY, frames + 1)
    );
    assert_eq!(
        reserved(&started_with([r22(1, Admitted), r23(1), r15(1, 1)])),
        (base, frames)
    );
    for disposition in [Refused, Busy] {
        assert_eq!(
            reserved(&started_with([r22(1, disposition)])),
            (base + RESULT_CAPACITY, frames + 1)
        );
    }
    // A legacy kind 14 exec_cmd keeps the result reservation only.
    assert_eq!(
        reserved(&started_with([Record::ToolIntent(exec_cmd_intent(1))])),
        (base + RESULT_CAPACITY, frames + 1)
    );
    // After restart the old operation keeps the capture frame for its terminal
    // only while kind 23 is missing; no owner or cancel frames remain.
    let journal = started_with([r22(1, Admitted), Record::OwnerGeneration]);
    let state = inspect(&journal.bytes).unwrap();
    assert!(state.reserved_bytes >= RESULT_CAPACITY + CAPTURE_CAPACITY);
}
