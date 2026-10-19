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
        event_cursor: 1,
        composition: Composition::default(),
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
        text_offset: 0,
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
    // No turn reservation exists: unknown usage records zero output.
    assert!(!t.usage_known);
    assert_eq!(t.output_tokens, 0);
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
        }
        j.append(Record::TurnTerminal(p));
        let s = inspect(&j.bytes).unwrap();
        assert_eq!(s.active_operation, None);
        let t = s.operations[&id(10)].terminal.as_ref().unwrap();
        assert_eq!((t.usage_known, t.output_tokens), (!started, 0));
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
        p.composition = Composition {
            omitted_turns: omitted,
            manifest_digest: [7; 32],
            composer_revision: 1,
            window_tokens: 4096,
            output_reservation: 2048,
            overhead_tokens: 300,
            bytes_per_token_milli: 1000,
        };
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
    // Removed kind 5 (an accepted turn without composition) is an unknown kind.
    let payload = {
        let mut j = started();
        let start = j.bytes.len();
        j.append(Record::TurnAccepted(Box::new(composed(vec![id(10)], 0))));
        j.bytes[start + 144..j.bytes.len() - 40].to_vec()
    };
    assert_eq!(
        decode_record(5, &payload).unwrap_err(),
        ReplayError::UnsupportedFormat
    );
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
    // A removed diagnostic kind 1 after kind 10 is an unknown kind.
    let second = u32::from_be_bytes(j.bytes[first + 12..first + 16].try_into().unwrap()) as usize;
    let mut removed = j.bytes[first..first + second].to_vec();
    removed[10..12].copy_from_slice(&1u16.to_be_bytes());
    seal(&mut removed);
    let mut mixed = j.bytes[..first].to_vec();
    mixed.extend(removed);
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

    // Removed diagnostic kinds 1 and 2 never start a journal, even with correct checksums.
    for kind in [1u16, 2] {
        let mut wrong_kind = journal.bytes[..first].to_vec();
        wrong_kind[10..12].copy_from_slice(&kind.to_be_bytes());
        seal(&mut wrong_kind);
        assert_eq!(public(&wrong_kind), Err(ReplayError::UnsupportedFormat));
    }

    // All earlier records remain valid here: inspection must validate the changed tail.
    let mut mixed = journal.bytes[..first + second_length].to_vec();
    mixed[first + 10..first + 12].copy_from_slice(&2u16.to_be_bytes());
    seal(&mut mixed[first..]);
    assert_eq!(public(&mixed), Err(ReplayError::UnsupportedFormat));
    assert_eq!(
        public(&journal.bytes[..first + second_length])
            .unwrap()
            .revision,
        2
    );
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

fn queued(
    request: u64,
    input: u64,
    dispatch: u64,
    conversation: u64,
    new_conversation: bool,
    revision: u64,
) -> InputQueued {
    queued_at(
        request,
        input,
        dispatch,
        conversation,
        new_conversation,
        revision,
        0,
    )
}
fn queued_at(
    request: u64,
    input: u64,
    dispatch: u64,
    conversation: u64,
    new_conversation: bool,
    revision: u64,
    expected_generation: u64,
) -> InputQueued {
    let prompt = format!("queued {request}");
    InputQueued {
        request: id(request),
        digest: request_digest(Request::QueueInput {
            project: id(6),
            conversation: if new_conversation {
                None
            } else {
                Some(id(conversation))
            },
            expected_generation,
            new_conversation,
            prompt: &prompt,
        })
        .unwrap(),
        input: id(input),
        dispatch_request: id(dispatch),
        project: id(6),
        conversation: id(conversation),
        expected_generation,
        new_conversation,
        prompt,
        order_revision: revision,
    }
}

#[test]
fn idle_first_input_reorder_replay_and_provisional_dispatch() {
    let mut j = Journal::base();
    let first = queued(40, 41, 42, 43, true, 1);
    let second = queued(44, 45, 46, 43, false, 2);
    j.append(Record::InputQueued(first.clone()));
    j.append(Record::InputQueued(second.clone()));
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
    j.append(Record::InputQueued(queued(48, 49, 50, 43, false, 5)));
    let state = inspect(&j.bytes).unwrap();
    assert_eq!(state.input_status(id(49)), Some(InputStatus::Queued));
    assert_eq!(state.order_revision, 5);
}

#[test]
fn queue_rejects_revision_gap_and_holds_after_owner_restart() {
    let mut j = Journal::base();
    let first = queued(40, 41, 42, 43, true, 1);
    j.append(Record::InputQueued(first.clone()));
    let mut bad = queued(44, 45, 46, 43, false, 3);
    j.append(Record::InputQueued(bad.clone()));
    assert!(inspect(&j.bytes).is_err());
    // Build a fresh, valid chain rather than claiming a truncated tail is repairable.
    let mut j = Journal::base();
    bad.order_revision = 2;
    j.append(Record::InputQueued(queued(40, 41, 42, 43, true, 1)));
    j.append(Record::InputQueued(bad));
    j.append(Record::OwnerGeneration);
    let state = inspect(&j.bytes).unwrap();
    assert_eq!(state.input_status(id(41)), Some(InputStatus::Held));
    assert_eq!(state.input_status(id(45)), Some(InputStatus::Held));
    assert_eq!(state.ready_input(), None);
}

#[test]
fn committed_failure_or_cancellation_keeps_successors_eligible() {
    for (kind, cause) in [
        (TerminalKind::Failed, Cause::ProviderFailure),
        (TerminalKind::Failed, Cause::Deadline),
        (TerminalKind::Cancelled, Cause::UserCancel),
    ] {
        let mut j = Journal::started();
        let first = queued(40, 41, 42, 8, false, 1);
        let second = queued(44, 45, 46, 8, false, 2);
        j.append(Record::InputQueued(first.clone()));
        if kind == TerminalKind::Cancelled {
            j.append(Record::CancelRequested(CancelRequested {
                operation: id(10),
                generation: 1,
                cause,
            }));
        }
        j.append(Record::TurnTerminal(terminal(kind, cause, false)));
        j.append(Record::InputQueued(second.clone()));
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
            text_offset: 0,
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
    let first = queued(20, 21, 22, 8, false, 1);
    j.append(Record::InputQueued(first.clone()));
    j.append(Record::InputQueued(queued(23, 24, 25, 8, false, 2)));
    let state = inspect(&j.bytes).unwrap();
    assert_eq!(state.input_status(first.input), Some(InputStatus::Queued));
    assert_eq!(state.ready_input(), None);
    let range = state.inputs[&first.input].frame.clone();
    let frame = &j.bytes[range];
    assert_eq!(
        decode_record(18, &frame[144..frame.len() - 40]).unwrap(),
        Record::InputQueued(first.clone())
    );
    // Removed kind 12 is an unknown kind, never an input record.
    assert_eq!(
        decode_record(12, &frame[144..frame.len() - 40]).unwrap_err(),
        ReplayError::UnsupportedFormat
    );
    j.append(Record::TurnTerminal(terminal(
        TerminalKind::Complete,
        Cause::None,
        false,
    )));
    assert_eq!(inspect(&j.bytes).unwrap().ready_input(), Some(first.input));
    let mut next = turn();
    next.request = first.dispatch_request;
    next.original_conversation = Some(id(8));
    next.expected_generation = 1;
    next.generation = 2;
    next.operation = id(30);
    next.task = id(31);
    next.prompt = first.prompt.clone();
    next.composition.omitted_turns = 1;
    next.digest = request_digest(Request::Submit {
        project: id(6),
        conversation: Some(id(8)),
        expected_generation: 1,
        prompt: &next.prompt,
    })
    .unwrap();
    j.append(Record::TurnAccepted(Box::new(next.clone())));
    let state = inspect(&j.bytes).unwrap();
    assert_eq!(state.input_status(first.input), Some(InputStatus::Running));
    assert_eq!(state.input_operation(first.input), Some((id(30), 2)));
    assert!(!state.can_promote_input(first.input, id(30), 2));
    assert_eq!(state.ready_input(), None);
    // Exact dispatch cannot be replayed as another operation.
    next.operation = id(32);
    next.task = id(33);
    j.append(Record::TurnAccepted(Box::new(next)));
    assert!(inspect(&j.bytes).is_err());
}
#[test]
fn held_head_blocks_its_lane_and_resume_drop_are_explicit() {
    let mut j = Journal::base();
    j.append(Record::InputQueued(queued(20, 21, 22, 43, true, 1)));
    j.append(Record::InputQueued(queued(23, 24, 25, 43, false, 2)));
    j.append(decision(50, 21, InputAction::Hold));
    let state = inspect(&j.bytes).unwrap();
    assert_eq!(state.input_status(id(21)), Some(InputStatus::Held));
    assert_eq!(state.input_status(id(24)), Some(InputStatus::Queued));
    assert_eq!(state.ready_input(), None);
    j.append(decision(51, 21, InputAction::Resume));
    assert_eq!(inspect(&j.bytes).unwrap().ready_input(), Some(id(21)));
    j.append(decision(52, 21, InputAction::Hold));
    j.append(decision(53, 21, InputAction::Drop));
    let state = inspect(&j.bytes).unwrap();
    assert_eq!(state.input_status(id(21)), Some(InputStatus::Dropped));
    assert_eq!(state.ready_input(), Some(id(24)));
    // Resume needs Held; Hold needs Queued.
    j.append(decision(54, 24, InputAction::Resume));
    assert!(inspect(&j.bytes).is_err());
}
#[test]
fn restart_holds_queued_input_without_replaying_any_dispatch() {
    let mut j = Journal::started();
    j.append(Record::InputQueued(queued(20, 21, 22, 8, false, 1)));
    j.append(Record::OwnerGeneration);
    j.append(Record::TurnTerminal(terminal(
        TerminalKind::Interrupted,
        Cause::Restart,
        false,
    )));
    let state = inspect(&j.bytes).unwrap();
    assert_eq!(state.input_status(id(21)), Some(InputStatus::Held));
    assert_eq!(state.ready_input(), None);
    assert_eq!(state.input_operation(id(21)), None);
    j.append(decision(50, 21, InputAction::Resume));
    assert_eq!(inspect(&j.bytes).unwrap().ready_input(), Some(id(21)));
}
/// MQ06/IQ2 replay: a queued kind 18 input promoted during the active turn
/// becomes its single Steer input; only a steering cancellation lets it run.
#[test]
fn promoted_kind_18_input_steers_only_after_steering_cancel_and_never_after_restart() {
    for cause in [Cause::UserCancel, Cause::Steering, Cause::ServiceShutdown] {
        let mut j = Journal::started();
        j.append(Record::InputQueued(queued(20, 21, 22, 8, false, 1)));
        j.append(Record::InputQueued(queued(23, 24, 25, 8, false, 2)));
        let state = inspect(&j.bytes).unwrap();
        assert!(state.can_promote_input(id(24), id(10), 1));
        assert!(!state.can_promote_input(id(24), id(10), 2));
        assert!(!state.can_promote_input(id(24), id(99), 1));
        j.append(promotion(26, 24, 10, 1));
        let state = inspect(&j.bytes).unwrap();
        assert_eq!(state.steering_input(id(10)), Some(id(24)));
        assert_eq!(state.inputs[&id(24)].kind, InputKind::Steer);
        assert_eq!(state.order_revision, 3);
        // One steering input per target.
        assert!(!state.can_promote_input(id(21), id(10), 1));
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
        let state = inspect(&j.bytes).unwrap();
        if cause == Cause::Steering {
            // The replacement runs before its lane's ordinary successors.
            assert_eq!(state.ready_input(), Some(id(24)));
            assert_eq!(state.input_status(id(21)), Some(InputStatus::Queued));
        } else {
            assert_eq!(state.input_status(id(24)), Some(InputStatus::Held));
            assert_eq!(state.ready_input(), Some(id(21)));
        }
        j.append(Record::OwnerGeneration);
        assert_eq!(
            inspect(&j.bytes).unwrap().input_status(id(24)),
            Some(InputStatus::Held)
        );
    }
}
#[test]
fn queue_rejects_stale_targets_duplicate_identity_and_overload() {
    let mut j = Journal::started();
    j.append(Record::InputQueued(queued_at(20, 21, 22, 8, false, 1, 2)));
    assert!(inspect(&j.bytes).is_err());
    let mut j = Journal::started();
    j.append(Record::InputQueued(queued(20, 21, 22, 8, false, 1)));
    // Promotion requires the exact active target.
    let mut stale = Journal::started();
    stale.append(Record::InputQueued(queued(20, 21, 22, 8, false, 1)));
    stale.append(Record::TurnTerminal(terminal(
        TerminalKind::Complete,
        Cause::None,
        false,
    )));
    stale.append(promotion(23, 21, 10, 1));
    assert!(inspect(&stale.bytes).is_err());
    let mut j = Journal::started();
    for n in 0..16 {
        j.append(Record::InputQueued(queued(
            100 + 3 * n,
            101 + 3 * n,
            102 + 3 * n,
            8,
            false,
            n + 1,
        )));
    }
    assert_eq!(inspect(&j.bytes).unwrap().inputs.len(), 16);
    j.append(Record::InputQueued(queued(200, 201, 202, 8, false, 17)));
    assert_eq!(inspect(&j.bytes).unwrap_err(), ReplayError::Limit);
    let mut j = Journal::started();
    let input = queued(20, 21, 22, 8, false, 1);
    j.append(Record::InputQueued(input.clone()));
    let mut again = input;
    again.order_revision = 2;
    j.append(Record::InputQueued(again));
    assert!(inspect(&j.bytes).is_err());
}

// Kind 14 ToolCall fixtures (tool-call-records.md).
const RULE_SET_DIGEST: Hash = [21; 32];
const EXEC_ARGUMENTS: &str = "{\"command\":\"printf \\\"$ASURA_SECRET_1\\\"\",\"cwd\":\".\"}";
fn default_arguments(op: ToolOperation) -> &'static str {
    match op {
        ToolOperation::ReadFile => "{\"command\":\"read_file\",\"path\":\"README.md\"}",
        ToolOperation::ListDirectory => "{\"command\":\"list_directory\"}",
        ToolOperation::Status => "{\"command\":\"status\"}",
        ToolOperation::Tools => "{\"command\":\"tools\"}",
        ToolOperation::Audit => "{\"command\":\"audit\"}",
        ToolOperation::ListNotes => "{\"command\":\"list_notes\"}",
        ToolOperation::GetNote => {
            "{\"command\":\"get_note\",\"version\":\"01010101010101010101010101010101\"}"
        }
        ToolOperation::NoteSources => {
            "{\"command\":\"note_sources\",\"version\":\"01010101010101010101010101010101\"}"
        }
        ToolOperation::CreateNote => "{\"command\":\"create_note\",\"body\":\"note\"}",
        ToolOperation::ExecCmd => EXEC_ARGUMENTS,
    }
}
/// A `run` kind 14 of `op` at `ordinal`.
fn run14(ordinal: u32, op: ToolOperation) -> ToolCall {
    let name = op.tool().name();
    let arguments = default_arguments(op);
    let exec = op == ToolOperation::ExecCmd;
    ToolCall {
        operation: id(10),
        generation: 1,
        ordinal,
        source: ToolCallSource::GeneratedContent,
        name: name.into(),
        name_bytes: name.len() as u32,
        tool_operation: Some(op),
        decision: ToolDecision::Run,
        fault: None,
        busy: None,
        form: if exec {
            ArgumentForm::Canonical
        } else {
            ArgumentForm::Verbatim
        },
        arguments_bytes: arguments.len() as u32,
        arguments: arguments.into(),
        number_kinds: if exec {
            vec![SecretKind::GithubToken]
        } else {
            vec![]
        },
        unnumbered: 0,
        rule_set_revision: 1,
        rule_set_digest: RULE_SET_DIGEST,
        command_cut: false,
        refusal: None,
    }
}
/// A kind 14 exec_cmd call with `decision` and its consistent codes.
fn exec14(ordinal: u32, decision: ToolDecision) -> ToolCall {
    let mut call = run14(ordinal, ToolOperation::ExecCmd);
    call.decision = decision;
    match decision {
        ToolDecision::Busy => call.busy = Some(ExecCmdBusyReason::Quota),
        ToolDecision::Refused => {
            call.refusal = Some(ExecCmdRefusal {
                code: RefusalCode::SecretSingleQuoted,
                kind: Some(SecretKind::GithubToken),
                word: Some(2),
            })
        }
        ToolDecision::InvalidArguments => call.fault = Some(ToolFault::WrongType),
        _ => {}
    }
    call
}
/// A `project` call with a missing required field: `invalid_arguments`.
fn rejected14(ordinal: u32) -> ToolCall {
    ToolCall {
        decision: ToolDecision::InvalidArguments,
        fault: Some(ToolFault::MissingField),
        arguments: "{\"command\":\"read_file\"}".into(),
        arguments_bytes: 23,
        ..run14(ordinal, ToolOperation::ReadFile)
    }
}
fn r14(call: ToolCall) -> Record {
    Record::ToolCall(Box::new(call))
}
fn tool_call() -> Record {
    r14(run14(1, ToolOperation::ReadFile))
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
fn cancel_requested() -> Record {
    Record::CancelRequested(CancelRequested {
        operation: id(10),
        generation: 1,
        cause: Cause::UserCancel,
    })
}
/// The frame range of the last appended record.
fn last_frame(journal: &Journal) -> std::ops::Range<usize> {
    let mut start = 0;
    let mut end = 0;
    while end < journal.bytes.len() {
        start = end;
        end += u32::from_be_bytes(journal.bytes[end + 12..end + 16].try_into().unwrap()) as usize;
    }
    start..end
}
#[test]
fn tool_records_require_started_operation_and_commit_before_complete() {
    let mut premature = Journal::accepted();
    premature.append(tool_call());
    assert_eq!(inspect(&premature.bytes).unwrap_err(), ReplayError::Corrupt);
    let mut j = Journal::started();
    let baseline = inspect(&j.bytes).unwrap().reserved_bytes;
    j.append(tool_call());
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
    j.append(tool_call());
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
    j.append(tool_call());
    j.append(Record::OwnerGeneration);
    j.append(Record::TurnTerminal(terminal(
        TerminalKind::Interrupted,
        Cause::Restart,
        false,
    )));
    let state = inspect(&j.bytes).unwrap();
    let operation = &state.operations[&id(10)];
    assert!(operation.tools[0].result_frame.is_none());
    assert!(!operation.terminal.as_ref().unwrap().usage_known);
    assert!(state.active_operation.is_none());
    let mut duplicate = Journal::started();
    duplicate.append(tool_call());
    duplicate.append(tool_result());
    duplicate.append(tool_result());
    assert_eq!(inspect(&duplicate.bytes).unwrap_err(), ReplayError::Corrupt);
}

#[test]
fn status_tool_result_replays_and_rejects_file_paging_or_oversized_payload() {
    let call = run14(1, ToolOperation::Status);
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
    valid.append(r14(call.clone()));
    valid.append(Record::ToolResult(result.clone()));
    let replay = inspect(&valid.bytes).unwrap();
    let tool = &replay.operations[&id(10)].tools[0];
    assert_eq!(tool.tool_operation, Some(ToolOperation::Status));
    assert_eq!(tool.tool, Some(RegisteredTool::Service));
    assert_eq!(tool.result_status, Some(1));
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
    interrupted.append(r14(call.clone()));
    interrupted.append(Record::OwnerGeneration);
    interrupted.append(Record::TurnTerminal(terminal(
        TerminalKind::Interrupted,
        Cause::Restart,
        false,
    )));
    let replay = inspect(&interrupted.bytes).unwrap();
    assert!(replay.operations[&id(10)].tools[0].result_frame.is_none());
    assert!(
        !replay.operations[&id(10)]
            .terminal
            .as_ref()
            .unwrap()
            .usage_known
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
        journal.append(r14(call.clone()));
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

/// TCR-R5 and TCR-D16: a call that does not run is complete when its kind 14
/// commits. It holds no reservation, never takes a kind 15 or kind 23, and
/// restart has nothing to interrupt.
#[test]
fn calls_that_do_not_run_are_complete_at_kind_14() {
    let not_run = [
        rejected14(1),
        exec14(1, ToolDecision::InvalidArguments),
        exec14(1, ToolDecision::Refused),
        exec14(1, ToolDecision::Busy),
        exec14(1, ToolDecision::Denied),
        exec14(1, ToolDecision::Limit),
        exec14(1, ToolDecision::Cancelled),
        ToolCall {
            name: "unknown".into(),
            name_bytes: 7,
            tool_operation: None,
            decision: ToolDecision::UnknownTool,
            fault: Some(ToolFault::UnregisteredName),
            form: ArgumentForm::Verbatim,
            arguments: "{}".into(),
            number_kinds: vec![],
            ..run14(1, ToolOperation::ReadFile)
        },
    ];
    let (base, frames) = {
        let state = inspect(&Journal::started().bytes).unwrap();
        (state.reserved_bytes, state.reserved_frames)
    };
    for call in not_run {
        let decision = call.decision;
        let mut journal = Journal::started();
        if decision == ToolDecision::Cancelled {
            journal.append(cancel_requested());
        }
        let (cancel_base, cancel_frames) = {
            let state = inspect(&journal.bytes).unwrap();
            (state.reserved_bytes, state.reserved_frames)
        };
        journal.append(r14(call.clone()));
        let state = inspect(&journal.bytes).unwrap();
        let tool = &state.operations[&id(10)].tools[0];
        assert_eq!(tool.decision, decision);
        assert!(!tool.unresolved() && tool.result_frame.is_none() && tool.exec_cmd.is_none());
        assert_eq!(
            (state.reserved_bytes, state.reserved_frames),
            (cancel_base, cancel_frames),
            "{decision:?} holds no result frame"
        );
        if decision != ToolDecision::Cancelled {
            assert_eq!((cancel_base, cancel_frames), (base, frames));
            let mut complete = Journal::started();
            complete.append(r14(call.clone()));
            complete.append(Record::TurnTerminal(terminal(
                TerminalKind::Complete,
                Cause::None,
                false,
            )));
            assert!(inspect(&complete.bytes).is_ok(), "{decision:?}");
        }
        for (name, follow) in [("15", Record::ToolResult(result15(1, 3))), ("23", r23(1))] {
            let mut journal = Journal::started();
            if decision == ToolDecision::Cancelled {
                journal.append(cancel_requested());
            }
            journal.append(r14(call.clone()));
            journal.append(follow);
            assert_eq!(
                inspect(&journal.bytes).unwrap_err(),
                ReplayError::Corrupt,
                "kind {name} after {decision:?}"
            );
        }
        let mut restarted = Journal::started();
        if decision == ToolDecision::Cancelled {
            restarted.append(cancel_requested());
        }
        restarted.append(r14(call));
        restarted.append(Record::OwnerGeneration);
        restarted.append(Record::TurnTerminal(terminal(
            TerminalKind::Interrupted,
            Cause::Restart,
            false,
        )));
        assert!(inspect(&restarted.bytes).is_ok());
    }
}
trait IntoRecord {
    fn into_record(self) -> Record;
}
impl IntoRecord for ToolCall {
    fn into_record(self) -> Record {
        r14(self)
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
    j.append(Record::InputQueued(queued(20, 21, 22, 8, false, 1)));
    j.append(Record::InputQueued(queued(23, 24, 25, 8, false, 2)));
    let before = inspect(&j.bytes).unwrap().inputs[&id(24)].clone();
    j.append(promotion(50, 24, 10, 1));
    let state = inspect(&j.bytes).unwrap();
    let after = &state.inputs[&id(24)];
    assert_eq!(state.inputs.len(), 2);
    assert_eq!(after.request, before.request);
    assert_eq!(after.dispatch_request, before.dispatch_request);
    assert_eq!(after.frame, before.frame);
    assert_eq!(after.sequence, before.sequence);
    assert_eq!(after.order_position, before.order_position);
    assert_eq!(after.kind, InputKind::Steer);
    assert_eq!(state.ready_input(), None);
    assert_eq!(state.steering_input(id(10)), Some(id(24)));
    assert!(!state.can_promote_input(id(21), id(10), 1));
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
    assert_eq!(state.ready_input(), Some(id(24)));
    // Ordinary successors keep their durable order behind the replacement.
    assert_eq!(state.input_status(id(21)), Some(InputStatus::Queued));
    j.append(Record::OwnerGeneration);
    assert_eq!(
        inspect(&j.bytes).unwrap().input_status(id(24)),
        Some(InputStatus::Held)
    );
    assert_eq!(inspect(&j.bytes).unwrap().ready_input(), None);
}
#[test]
fn promotion_replay_rejects_stale_unknown_cancelled_and_competing_targets() {
    for (target, generation) in [(10, 2), (99, 1)] {
        let mut j = Journal::started();
        j.append(Record::InputQueued(queued(20, 21, 22, 8, false, 1)));
        j.append(promotion(50, 21, target, generation));
        assert!(inspect(&j.bytes).is_err());
    }
    for cancel in [false, true] {
        let mut j = Journal::started();
        j.append(Record::InputQueued(queued(20, 21, 22, 8, false, 1)));
        if cancel {
            j.append(Record::CancelRequested(CancelRequested {
                operation: id(10),
                generation: 1,
                cause: Cause::UserCancel,
            }));
        } else {
            j.append(Record::InputQueued(queued(23, 24, 25, 8, false, 2)));
            j.append(promotion(49, 24, 10, 1));
        }
        j.append(promotion(50, 21, 10, 1));
        assert!(inspect(&j.bytes).is_err());
    }
}

#[test]
fn explicit_promotion_clears_hold_with_exact_active_target() {
    let mut j = Journal::started();
    j.append(Record::InputQueued(queued(20, 21, 22, 8, false, 1)));
    j.append(decision(49, 21, InputAction::Hold));
    assert_eq!(
        inspect(&j.bytes).unwrap().input_status(id(21)),
        Some(InputStatus::Held)
    );
    j.append(promotion(50, 21, 10, 1));
    let state = inspect(&j.bytes).unwrap();
    assert_eq!(state.input_status(id(21)), Some(InputStatus::Queued));
    assert_eq!(state.steering_input(id(10)), Some(id(21)));
    assert_eq!(state.inputs.len(), 1);
}

#[test]
fn memory_tool_replay_keeps_exact_results_and_rejects_inconsistent_pagination() {
    for op in [
        ToolOperation::ListNotes,
        ToolOperation::GetNote,
        ToolOperation::NoteSources,
    ] {
        let call = run14(1, op);
        let get = op == ToolOperation::GetNote;
        let result = ToolResult {
            operation: id(10),
            generation: 1,
            ordinal: 1,
            status: 1,
            text: "€".into(),
            next_offset: get.then_some(6),
            truncated: get,
        };
        let mut journal = Journal::started();
        journal.append(r14(call.clone()));
        journal.append(Record::ToolResult(result.clone()));
        let state = inspect(&journal.bytes).unwrap();
        let tool = &state.operations[&id(10)].tools[0];
        assert_eq!(tool.tool_operation, Some(op));
        assert_eq!(tool.tool, Some(RegisteredTool::Memory));
        assert_eq!(tool.result_status, Some(1));
        assert_eq!(tool.result_bytes, 3);
        // TCR-D25: a get_note page needs its continuation offset; replay no
        // longer compares it with the call's limit and offset.
        let invalid_results = if get {
            vec![ToolResult {
                next_offset: None,
                ..result.clone()
            }]
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
            journal.append(r14(call.clone()));
            journal.append(Record::ToolResult(invalid));
            assert_eq!(inspect(&journal.bytes).unwrap_err(), ReplayError::Corrupt);
        }
        let mut interrupted = Journal::started();
        interrupted.append(r14(call));
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
    let call = run14(1, ToolOperation::Tools);
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
    journal.append(r14(call.clone()));
    journal.append(Record::ToolResult(result.clone()));
    let replay = inspect(&journal.bytes).unwrap();
    assert_eq!(
        replay.operations[&id(10)].tools[0].tool_operation,
        Some(ToolOperation::Tools)
    );
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
        journal.append(r14(call.clone()));
        journal.append(Record::ToolResult(invalid));
        assert_eq!(inspect(&journal.bytes).unwrap_err(), ReplayError::Corrupt);
    }
    let mut interrupted = Journal::started();
    interrupted.append(r14(call));
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

/// No turn token reservation exists (owner, 2026-10-18): kind 21 has no
/// reservation field and kind 8 no charge. Known usage is any count, beyond
/// the former 2,048 total; unknown usage must record zero output.
#[test]
fn response_accounting_has_no_turn_reservation() {
    for (known, output) in [
        (true, 0),
        (true, 2048),
        (true, 2049),
        (true, u32::MAX),
        (false, 0),
    ] {
        let mut journal = Journal::base();
        journal.append(Record::TurnAccepted(Box::new(turn())));
        journal.append(start());
        let mut result = terminal(TerminalKind::Complete, Cause::None, known);
        result.output_tokens = output;
        journal.append(Record::TurnTerminal(result));
        let state = inspect(&journal.bytes).unwrap();
        let t = state.operations[&id(10)].terminal.as_ref().unwrap();
        assert_eq!((t.usage_known, t.output_tokens), (known, output));
    }
    let mut journal = Journal::base();
    journal.append(Record::TurnAccepted(Box::new(turn())));
    journal.append(start());
    let mut result = terminal(TerminalKind::Complete, Cause::None, false);
    result.output_tokens = 1;
    journal.append(Record::TurnTerminal(result));
    assert_eq!(inspect(&journal.bytes).unwrap_err(), ReplayError::Corrupt);
    let mut journal = Journal::base();
    journal.append(Record::TurnAccepted(Box::new(turn())));
    journal.append(start());
    journal.append(Record::OwnerGeneration);
    journal.append(Record::TurnTerminal(terminal(
        TerminalKind::Interrupted,
        Cause::Restart,
        false,
    )));
    let t = inspect(&journal.bytes).unwrap().operations[&id(10)]
        .terminal
        .clone()
        .unwrap();
    assert_eq!((t.usage_known, t.output_tokens), (false, 0));
}

/// The derived call of a `run` create_note kind 14 at `ordinal` (HM3).
fn create_call(ordinal: u32) -> CreateNoteCall {
    CreateNoteCall::derive([id(1), id(4), id(2)], id(6), id(10), 1, ordinal).unwrap()
}
fn create_result(text: &str) -> Record {
    Record::ToolResult(ToolResult {
        operation: id(10),
        generation: 1,
        ordinal: 1,
        status: 1,
        text: text.into(),
        truncated: false,
        next_offset: None,
    })
}
#[test]
fn memory_create_replay_gates_publication_and_requires_empty_success_text() {
    let mut journal = Journal::started();
    journal.append(r14(run14(1, ToolOperation::CreateNote)));
    let state = inspect(&journal.bytes).unwrap();
    assert!(state.project_memory_blocked(id(6)));
    assert!(!state.project_memory_blocked(id(99)));
    assert_eq!(state.unresolved_creates().next(), Some(&create_call(1)));
    let before = journal.bytes.clone();
    journal.append(Record::TurnTerminal(terminal(
        TerminalKind::Interrupted,
        Cause::Restart,
        false,
    )));
    assert!(inspect(&journal.bytes).is_err());
    // TCR-D1: kind 15 of create_note stores no text; any text is Corrupt,
    // including the success text rendered from the derived identity.
    for text in ["invented receipt".to_owned(), create_call(1).success_text()] {
        journal.bytes = before.clone();
        journal.sequence = state.sequence;
        journal.digest = state.last_digest;
        journal.append(create_result(&text));
        assert!(inspect(&journal.bytes).is_err());
    }
    journal.bytes = before;
    journal.sequence = state.sequence;
    journal.digest = state.last_digest;
    journal.append(create_result(""));
    let state = inspect(&journal.bytes).unwrap();
    assert!(!state.project_memory_blocked(id(6)));
    assert_eq!(state.unresolved_creates().count(), 0);
}
#[test]
fn memory_create_old_owner_can_resolve_but_cannot_write_new_call() {
    let mut journal = Journal::started();
    journal.append(r14(run14(1, ToolOperation::CreateNote)));
    journal.append(Record::OwnerGeneration);
    journal.append(create_result(""));
    let state = inspect(&journal.bytes).unwrap();
    assert!(!state.project_memory_blocked(id(6)));
    journal.append(Record::TurnTerminal(terminal(
        TerminalKind::Interrupted,
        Cause::Restart,
        false,
    )));
    assert!(inspect(&journal.bytes).is_ok());
    // An old owner cannot record a new create.
    let mut journal = Journal::started();
    journal.append(Record::OwnerGeneration);
    journal.append(r14(run14(1, ToolOperation::CreateNote)));
    assert!(inspect(&journal.bytes).is_err());
}

#[test]
fn old_owner_result_exception_cannot_resolve_an_ordinary_tool_or_wrong_create() {
    let mut journal = Journal::started();
    journal.append(r14(run14(1, ToolOperation::Tools)));
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
        let mut journal = Journal::started();
        journal.append(r14(run14(1, ToolOperation::CreateNote)));
        journal.append(Record::OwnerGeneration);
        journal.append(Record::ToolResult(ToolResult {
            operation: if wrong_operation { id(99) } else { id(10) },
            generation: 1,
            ordinal: if wrong_operation { 1 } else { 2 },
            status: 1,
            text: String::new(),
            truncated: false,
            next_offset: None,
        }));
        assert!(inspect(&journal.bytes).is_err());
    }
}

#[test]
fn reopened_tool_display_projections_are_bounded_and_capture_stdout() {
    let mut call = run14(1, ToolOperation::ExecCmd);
    call.arguments = format!("{{\"command\":\"printf {}\"}}", "界".repeat(100));
    call.number_kinds = vec![];
    let journal = started_with([
        r14(call),
        r23(1),
        Record::ToolResult(ToolResult {
            text: format!(
                "failure=None exit_code=17 signal=none reason=none truncated=true cleanup_confirmed=true\nstdout:\n{}\nstderr:\nwarning",
                "界".repeat(100)
            ),
            ..result15(1, 1)
        }),
    ]);
    let state = inspect(&journal.bytes).unwrap();
    let tool = &state.operations[&id(10)].tools[0];
    assert!(tool.arguments.len() <= 192);
    assert!(tool.arguments.starts_with("{\"command\":\"printf 界"));
    assert!(tool.preview.as_ref().unwrap().len() <= 128);
    assert!(tool.preview.as_ref().unwrap().starts_with("界"));
    assert_eq!(tool.exit_code, Some(17));
    assert_eq!(tool.truncated, Some(false));
    assert_eq!(
        inspect(&journal.bytes).unwrap().operations[&id(10)].tools[0],
        *tool
    );
}

// J1T-02: kinds 14, 23 and 15 replay order and reservation.
const FRAME_OVERHEAD: usize = 144 + 40;
const RESULT_CAPACITY: usize = FRAME_OVERHEAD + 43 + 16_384;
const CAPTURE_CAPACITY: usize = FRAME_OVERHEAD + 4_066;
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
fn run_exec(ordinal: u32) -> Record {
    r14(run14(ordinal, ToolOperation::ExecCmd))
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
fn j1t_02_each_decision_with_and_without_capture_result_and_terminal() {
    let exec_cmd = |journal: &Journal| {
        let state = inspect(&journal.bytes).unwrap();
        state.operations[&id(10)].tools[0].clone()
    };
    // Run: 14, then 23, then 15, then a terminal.
    let journal = started_with([run_exec(1)]);
    let tool = exec_cmd(&journal);
    let call = tool.exec_cmd.as_deref().unwrap();
    assert_eq!(tool.tool_operation, Some(ToolOperation::ExecCmd));
    assert_eq!(tool.tool, Some(RegisteredTool::ExecCmd));
    assert_eq!(tool.arguments, EXEC_ARGUMENTS);
    assert_eq!(call.job, exec_cmd_job_id(id(10), 1, 1));
    assert_eq!(call.intent_sequence, journal.sequence);
    assert!(call.settled.is_none() && tool.result_frame.is_none());
    let journal = started_with([run_exec(1), r23(1)]);
    let tool = exec_cmd(&journal);
    let settled = tool.exec_cmd.as_deref().unwrap().settled.clone().unwrap();
    assert_eq!(settled.state, CaptureState::Complete);
    assert_eq!(settled.reason, CaptureReason::None);
    assert_eq!(settled.sequence, journal.sequence);
    assert_eq!(settled.streams, [(5, Some([22; 32])), (0, Some([22; 32]))]);
    assert!(tool.result_frame.is_none());
    let journal = started_with([run_exec(1), r23(1), r15(1, 1)]);
    let tool = exec_cmd(&journal);
    assert_eq!(tool.result_status, Some(1));
    assert_eq!(tool.exit_code, Some(3));
    assert_eq!(tool.preview.as_deref(), Some("hello"));
    assert!(tool.exec_cmd.as_deref().unwrap().settled.is_some());
    for journal in [
        started_with([run_exec(1), r23(1), r15(1, 1), complete()]),
        started_with([run_exec(1), r23(1), r15(1, 5), failed()]),
        started_with([run_exec(1), r23(1), failed()]),
    ] {
        let state = inspect(&journal.bytes).unwrap();
        assert!(state.operations[&id(10)].terminal.is_some());
        assert_eq!(state.active_operation, None);
    }
    // Refused and busy: kind 14 only, then a terminal; no derived identities.
    for decision in [ToolDecision::Refused, ToolDecision::Busy] {
        let journal = started_with([r14(exec14(1, decision))]);
        let tool = exec_cmd(&journal);
        assert_eq!(tool.decision, decision);
        assert!(tool.exec_cmd.is_none());
        for end in [complete(), failed()] {
            let journal = started_with([r14(exec14(1, decision)), end]);
            assert!(
                inspect(&journal.bytes).unwrap().operations[&id(10)]
                    .terminal
                    .is_some()
            );
        }
    }
    // Calls follow one another, including ordinary tools.
    let journal = started_with([
        r14(exec14(1, ToolDecision::Refused)),
        r14(run14(2, ToolOperation::ReadFile)),
        Record::ToolResult(ToolResult {
            ordinal: 2,
            text: "observed".into(),
            next_offset: Some(8),
            truncated: false,
            ..result15(2, 1)
        }),
        run_exec(3),
        r23(3),
        r15(3, 1),
        r14(exec14(4, ToolDecision::Busy)),
        complete(),
    ]);
    let state = inspect(&journal.bytes).unwrap();
    let tools = &state.operations[&id(10)].tools;
    assert_eq!(tools.len(), 4);
    assert!(tools[1].exec_cmd.is_none());
    assert!(tools[2].exec_cmd.is_some());
}
#[test]
fn j1t_02_every_invalid_order_is_corrupt() {
    let other = |change: fn(&mut CaptureSettled)| {
        let mut p = settled23(1);
        change(&mut p);
        Record::CaptureSettled(Box::new(p))
    };
    let refused = || r14(exec14(1, ToolDecision::Refused));
    let busy = || r14(exec14(1, ToolDecision::Busy));
    let cases: Vec<(&str, Journal, Vec<Record>)> = vec![
        ("23 before 14", Journal::started(), vec![r23(1)]),
        (
            "23 twice",
            Journal::started(),
            vec![run_exec(1), r23(1), r23(1)],
        ),
        (
            "15 before 23",
            Journal::started(),
            vec![run_exec(1), r15(1, 1)],
        ),
        (
            "15 status 3 before 23",
            Journal::started(),
            vec![run_exec(1), r15(1, 3)],
        ),
        (
            "23 after 15",
            Journal::started(),
            vec![run_exec(1), r23(1), r15(1, 1), r23(1)],
        ),
        (
            "23 after refused",
            Journal::started(),
            vec![refused(), r23(1)],
        ),
        ("23 after busy", Journal::started(), vec![busy(), r23(1)]),
        (
            "15 after refused",
            Journal::started(),
            vec![refused(), r15(1, 3)],
        ),
        ("15 after busy", Journal::started(), vec![busy(), r15(1, 4)]),
        (
            "14 after terminal",
            Journal::started(),
            vec![failed(), run_exec(1)],
        ),
        (
            "23 after terminal",
            Journal::started(),
            vec![run_exec(1), failed(), r23(1)],
        ),
        (
            "15 after terminal",
            Journal::started(),
            vec![run_exec(1), r23(1), failed(), r15(1, 1)],
        ),
        (
            "complete while 23 pending",
            Journal::started(),
            vec![run_exec(1), complete()],
        ),
        (
            "complete while 15 pending",
            Journal::started(),
            vec![run_exec(1), r23(1), complete()],
        ),
        (
            "23 wrong ordinal",
            Journal::started(),
            vec![run_exec(1), other(|p| p.ordinal = 2)],
        ),
        (
            "23 wrong generation",
            Journal::started(),
            vec![run_exec(1), other(|p| p.generation = 2)],
        ),
        (
            "23 for an ordinary tool",
            Journal::started(),
            vec![tool_call(), r23(1)],
        ),
        ("14 skips an ordinal", Journal::started(), vec![run_exec(2)]),
        (
            "14 while 15 pending",
            Journal::started(),
            vec![run_exec(1), r23(1), run_exec(2)],
        ),
        ("14 before start", Journal::accepted(), vec![run_exec(1)]),
        (
            "run 14 after cancel",
            Journal::started(),
            vec![cancel_requested(), run_exec(1)],
        ),
        (
            "14 after restart",
            Journal::started(),
            vec![Record::OwnerGeneration, run_exec(1)],
        ),
        (
            "23 after restart",
            Journal::started(),
            vec![run_exec(1), Record::OwnerGeneration, r23(1)],
        ),
        (
            "15 after restart",
            Journal::started(),
            vec![run_exec(1), r23(1), Record::OwnerGeneration, r15(1, 1)],
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
    // All recorded calls, then only a `limit` record at the over-limit ordinal.
    let mut journal = Journal::started();
    for ordinal in 1..=MAX_RECORDED_CALLS {
        journal.append(r14(exec14(ordinal, ToolDecision::Refused)));
    }
    assert!(inspect(&journal.bytes).is_ok());
    let over_limit = FrameContext {
        installation_id: id(1),
        transition_id: id(99_999),
        sequence: journal.sequence + 1,
        prior_digest: journal.digest,
        expected_revision: journal.sequence,
        owner_generation: journal.owner,
    };
    assert_eq!(
        encode_frame(
            &over_limit,
            &r14(exec14(MAX_TOOL_CALL_ORDINAL, ToolDecision::Refused))
        ),
        Err(ReplayError::Corrupt)
    );
    journal.append(r14(exec14(MAX_TOOL_CALL_ORDINAL, ToolDecision::Limit)));
    assert!(inspect(&journal.bytes).is_ok());
    // An ordinal past the over-limit ordinal cannot even be encoded.
    let beyond = FrameContext {
        transition_id: id(99_998),
        sequence: journal.sequence + 1,
        prior_digest: journal.digest,
        expected_revision: journal.sequence,
        ..over_limit
    };
    assert_eq!(
        encode_frame(
            &beyond,
            &r14(exec14(MAX_TOOL_CALL_ORDINAL + 1, ToolDecision::Limit))
        ),
        Err(ReplayError::Corrupt)
    );
}
#[test]
fn j1t_02_terminal_with_run_call_lacking_capture_is_accepted() {
    let journal = started_with([
        run_exec(1),
        Record::OwnerGeneration,
        Record::TurnTerminal(terminal(TerminalKind::Interrupted, Cause::Restart, false)),
    ]);
    let state = inspect(&journal.bytes).unwrap();
    let tool = &state.operations[&id(10)].tools[0];
    assert!(tool.exec_cmd.as_deref().unwrap().settled.is_none());
    assert!(tool.result_frame.is_none());
    assert_eq!(state.active_operation, None);
    for cancelled in [false, true] {
        let mut journal = started_with([run_exec(1)]);
        if cancelled {
            journal.append(cancel_requested());
            journal.append(Record::TurnTerminal(terminal(
                TerminalKind::Cancelled,
                Cause::UserCancel,
                false,
            )));
        } else {
            journal.append(failed());
        }
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
/// TCR-D16: a `run` kind 14 holds kind 15 (and kind 23 for exec_cmd); a call
/// that does not run holds nothing more.
#[test]
fn j1t_02_reserve_holds_one_capture_frame_until_kind_23() {
    let reserved = |journal: &Journal| {
        let state = inspect(&journal.bytes).unwrap();
        (state.reserved_bytes, state.reserved_frames)
    };
    let (base, frames) = reserved(&Journal::started());
    assert_eq!(CAPTURE_CAPACITY, 4_250);
    assert_eq!(
        reserved(&started_with([run_exec(1)])),
        (base + RESULT_CAPACITY + CAPTURE_CAPACITY, frames + 2)
    );
    assert_eq!(
        reserved(&started_with([run_exec(1), r23(1)])),
        (base + RESULT_CAPACITY, frames + 1)
    );
    assert_eq!(
        reserved(&started_with([run_exec(1), r23(1), r15(1, 1)])),
        (base, frames)
    );
    for decision in [ToolDecision::Refused, ToolDecision::Busy] {
        assert_eq!(
            reserved(&started_with([r14(exec14(1, decision))])),
            (base, frames)
        );
    }
    // An ordinary run tool keeps the result reservation only.
    assert_eq!(
        reserved(&started_with([tool_call()])),
        (base + RESULT_CAPACITY, frames + 1)
    );
    // After restart the old operation keeps the capture frame for its terminal
    // only while kind 23 is missing; no owner or cancel frames remain.
    let journal = started_with([run_exec(1), Record::OwnerGeneration]);
    let state = inspect(&journal.bytes).unwrap();
    assert!(state.reserved_bytes >= RESULT_CAPACITY + CAPTURE_CAPACITY);
}

// TCR-U11: one broken journal per replay rule (tool-call-records.md, replay
// table), each beside a valid twin. Record-alone rules are codec TCR-U01.
fn expect_rule(name: &str, valid: Journal, broken: Journal) {
    assert!(inspect(&valid.bytes).is_ok(), "{name}: valid twin");
    assert_eq!(
        inspect(&broken.bytes).unwrap_err(),
        ReplayError::Corrupt,
        "{name}: broken"
    );
}
/// Replaces the payload of the last frame and reseals it, so only the replay
/// rule (not the checksum) can reject it.
fn tamper_last(journal: &mut Journal, change: impl FnOnce(&mut Vec<u8>)) {
    let range = last_frame(journal);
    let mut frame = journal.bytes[range.clone()].to_vec();
    let mut payload = frame[144..frame.len() - 40].to_vec();
    change(&mut payload);
    assert_eq!(payload.len(), frame.len() - 184, "same-size tamper");
    frame[144..144 + payload.len()].copy_from_slice(&payload);
    seal(&mut frame);
    journal.bytes.truncate(range.start);
    journal.bytes.extend(frame);
}
#[test]
fn tcr_u11_operation_is_active_with_the_record_and_owner_generation() {
    expect_rule(
        "record generation",
        started_with([tool_call()]),
        started_with([r14(ToolCall {
            generation: 2,
            ..run14(1, ToolOperation::ReadFile)
        })]),
    );
    expect_rule(
        "active operation",
        started_with([tool_call()]),
        started_with([r14(ToolCall {
            operation: id(99),
            ..run14(1, ToolOperation::ReadFile)
        })]),
    );
    expect_rule(
        "current owner generation",
        started_with([rejected14(1).into_record()]),
        started_with([Record::OwnerGeneration, rejected14(1).into_record()]),
    );
    let mut before_start = Journal::accepted();
    before_start.append(rejected14(1).into_record());
    expect_rule(
        "started operation",
        started_with([rejected14(1).into_record()]),
        before_start,
    );
}
#[test]
fn tcr_u11_ordinal_is_next_and_every_earlier_run_has_kind_15() {
    expect_rule(
        "next ordinal",
        started_with([rejected14(1).into_record()]),
        started_with([rejected14(2).into_record()]),
    );
    expect_rule(
        "no repeated ordinal",
        started_with([rejected14(1).into_record(), rejected14(2).into_record()]),
        started_with([rejected14(1).into_record(), rejected14(1).into_record()]),
    );
    expect_rule(
        "earlier run has kind 15",
        started_with([tool_call(), tool_result(), rejected14(2).into_record()]),
        started_with([tool_call(), rejected14(2).into_record()]),
    );
}
#[test]
fn tcr_u11_over_limit_ordinal_has_limit() {
    let all = || {
        let mut journal = Journal::started();
        for ordinal in 1..=MAX_RECORDED_CALLS {
            journal.append(rejected14(ordinal).into_record());
        }
        journal
    };
    let mut valid = all();
    valid.append(r14(exec14(MAX_TOOL_CALL_ORDINAL, ToolDecision::Limit)));
    // A `limit` record at the over-limit ordinal without the earlier records skips ordinals.
    expect_rule(
        "over-limit ordinal only after every recorded call",
        valid,
        started_with([r14(exec14(MAX_TOOL_CALL_ORDINAL, ToolDecision::Limit))]),
    );
    // The over-limit ordinal with any other decision: forge the decision byte of a valid
    // over-limit record (exec_cmd name is 8 bytes; decision at offset 46).
    let mut broken = all();
    broken.append(r14(exec14(MAX_TOOL_CALL_ORDINAL, ToolDecision::Limit)));
    tamper_last(&mut broken, |payload| {
        assert_eq!(payload[46], ToolDecision::Limit as u8);
        payload[46] = ToolDecision::Denied as u8;
    });
    let mut valid = all();
    valid.append(r14(exec14(MAX_TOOL_CALL_ORDINAL, ToolDecision::Limit)));
    expect_rule("over-limit ordinal decision", valid, broken);
}
#[test]
fn tcr_u11_after_cancel_requested_only_cancelled() {
    for decision in [
        ToolDecision::Run,
        ToolDecision::InvalidArguments,
        ToolDecision::Refused,
        ToolDecision::Limit,
    ] {
        expect_rule(
            &format!("{decision:?} after cancel"),
            started_with([cancel_requested(), r14(exec14(1, ToolDecision::Cancelled))]),
            started_with([cancel_requested(), r14(exec14(1, decision))]),
        );
    }
    // Cancelled, then the terminal, in that order (TCR-I07 order).
    let journal = started_with([
        cancel_requested(),
        r14(exec14(1, ToolDecision::Cancelled)),
        Record::TurnTerminal(terminal(TerminalKind::Cancelled, Cause::UserCancel, false)),
    ]);
    assert!(inspect(&journal.bytes).is_ok());
}
#[test]
fn tcr_u11_no_record_follows_the_terminal() {
    expect_rule(
        "record after terminal",
        started_with([rejected14(1).into_record(), failed()]),
        started_with([failed(), rejected14(1).into_record()]),
    );
}
#[test]
fn tcr_u11_derived_tool_and_codes_agree() {
    // Replay recomputes the tool from the stored name.
    let state = inspect(&started_with([tool_call()]).bytes).unwrap();
    assert_eq!(
        state.operations[&id(10)].tools[0].tool,
        RegisteredTool::lookup("project")
    );
    // A resealed frame whose operation code names another tool's operation.
    let mut broken = started_with([run_exec(1)]);
    tamper_last(&mut broken, |payload| {
        assert_eq!(payload[45], ToolOperation::ExecCmd as u8);
        payload[45] = ToolOperation::ReadFile as u8;
    });
    expect_rule(
        "operation of another tool",
        started_with([run_exec(1)]),
        broken,
    );
    // A registered name with fault unregistered_name.
    let mut broken = started_with([rejected14(1).into_record()]);
    tamper_last(&mut broken, |payload| {
        let fault = 33 + "project".len() + 4 + 2;
        assert_eq!(payload[fault], ToolFault::MissingField as u8);
        payload[fault] = ToolFault::UnregisteredName as u8;
    });
    expect_rule(
        "registered name with unknown_tool fault",
        started_with([rejected14(1).into_record()]),
        broken,
    );
}
#[test]
fn tcr_u11_field_15_matches_the_stored_placeholders() {
    // Rename the stored placeholder 1 to 2 without changing field 15.
    let mut broken = started_with([run_exec(1)]);
    tamper_last(&mut broken, |payload| {
        let at = payload
            .windows(15)
            .position(|w| w == b"$ASURA_SECRET_1")
            .unwrap();
        payload[at + 14] = b'2';
    });
    expect_rule("field 15", started_with([run_exec(1)]), broken);
}
#[test]
fn tcr_u11_run_kinds_follow_their_record() {
    expect_rule(
        "run exec_cmd needs kind 23 before kind 15",
        started_with([run_exec(1), r23(1), r15(1, 1)]),
        started_with([run_exec(1), r15(1, 1)]),
    );
    expect_rule(
        "another run takes kind 15 only",
        started_with([tool_call(), tool_result()]),
        started_with([tool_call(), r23(1)]),
    );
    expect_rule(
        "a call that does not run takes no kind 15",
        started_with([rejected14(1).into_record(), failed()]),
        started_with([rejected14(1).into_record(), tool_result()]),
    );
    expect_rule(
        "a call that does not run takes no kind 23",
        started_with([r14(exec14(1, ToolDecision::Busy)), failed()]),
        started_with([r14(exec14(1, ToolDecision::Busy)), r23(1)]),
    );
}
/// TCR-U12: the cancel, over-limit ordinal and kind-15 rules in one place.
#[test]
fn tcr_u12_cancel_limit_and_result_rules() {
    // A record after CancelRequested is accepted only with `cancelled`.
    assert!(
        inspect(&started_with([cancel_requested(), r14(exec14(1, ToolDecision::Cancelled))]).bytes)
            .is_ok()
    );
    assert_eq!(
        inspect(&started_with([cancel_requested(), rejected14(1).into_record()]).bytes)
            .unwrap_err(),
        ReplayError::Corrupt
    );
    // The over-limit ordinal is accepted only with `limit`, after every recorded call.
    let mut journal = Journal::started();
    for ordinal in 1..=MAX_RECORDED_CALLS {
        journal.append(r14(exec14(ordinal, ToolDecision::Denied)));
    }
    journal.append(r14(exec14(MAX_TOOL_CALL_ORDINAL, ToolDecision::Limit)));
    let state = inspect(&journal.bytes).unwrap();
    assert_eq!(
        state.operations[&id(10)].tools.len(),
        MAX_TOOL_CALL_ORDINAL as usize
    );
    assert_eq!(
        state.operations[&id(10)].tools[MAX_RECORDED_CALLS as usize].decision,
        ToolDecision::Limit
    );
    // A kind 15 after a call that does not run is Corrupt.
    assert_eq!(
        inspect(&started_with([rejected14(1).into_record(), tool_result()]).bytes).unwrap_err(),
        ReplayError::Corrupt
    );
}
/// TCR-U13 (storage part): replay recomputes the job, output payload and
/// create_note identities with the same derivations the live path calls, and
/// the variable count from the stored command.
#[test]
fn tcr_u13_replay_recomputes_derived_identities() {
    use asura_storage::payload::{ObjectKind, payload_id};
    let journal = started_with([
        run_exec(1),
        r23(1),
        r15(1, 1),
        r14(run14(2, ToolOperation::CreateNote)),
    ]);
    let state = inspect(&journal.bytes).unwrap();
    let tools = &state.operations[&id(10)].tools;
    let call = tools[0].exec_cmd.as_deref().unwrap();
    assert_eq!(call.job, exec_cmd_job_id(id(10), 1, 1));
    assert_eq!(
        call.stdout_payload,
        payload_id(id(1), id(10), 1, 1, ObjectKind::Stdout)
    );
    assert_eq!(
        call.stderr_payload,
        payload_id(id(1), id(10), 1, 1, ObjectKind::Stderr)
    );
    let Record::ToolCall(stored) = decode_record(
        14,
        &journal.bytes[tools[0].intent_frame.start + 144..tools[0].intent_frame.end - 40],
    )
    .unwrap() else {
        panic!("kind 14");
    };
    let variables = asura_platform::secrets::placeholder_tokens(&stored.arguments)
        .distinct
        .len();
    assert_eq!(variables, stored.number_kinds.len());
    assert_eq!(variables, 1);
    assert_eq!(tools[1].create, Some(create_call(2)));
    assert!(tools[0].create.is_none() && tools[1].exec_cmd.is_none());
    // Another installation derives other payload IDs.
    assert_ne!(
        call.stdout_payload,
        payload_id(id(2), id(10), 1, 1, ObjectKind::Stdout)
    );
}
/// TCR-U17: one source for each fact. Encoded kind 14, 15 and 23 payloads
/// hold no copy of the arguments, no recomputed value and no body digest.
#[test]
fn tcr_u17_no_fact_is_stored_twice() {
    use asura_storage::payload::{ObjectKind, payload_id};
    use sha2::{Digest, Sha256};
    let payload = |record: &Record| {
        let frame = encode_frame(
            &FrameContext {
                installation_id: id(1),
                transition_id: id(500),
                sequence: 2,
                prior_digest: [7; 32],
                expected_revision: 1,
                owner_generation: 1,
            },
            record,
        )
        .unwrap();
        frame[144..frame.len() - 40].to_vec()
    };
    let contains = |haystack: &[u8], needle: &[u8]| {
        !needle.is_empty() && haystack.windows(needle.len()).any(|w| w == needle)
    };
    let body = "a private note body that only the memory store keeps";
    let mut create = run14(2, ToolOperation::CreateNote);
    create.arguments = format!("{{\"command\":\"create_note\",\"body\":\"{body}\"}}");
    create.arguments_bytes = create.arguments.len() as u32;
    let calls = [run14(1, ToolOperation::ExecCmd), create, rejected14(3)];
    let results = [
        payload(&r15(1, 1)),
        payload(&create_result("")),
        payload(&r23(1)),
    ];
    let job = exec_cmd_job_id(id(10), 1, 1);
    let ids = create_call(2).identity;
    let derived: Vec<Vec<u8>> = vec![
        job.to_vec(),
        payload_id(id(1), id(10), 1, 1, ObjectKind::Stdout).to_vec(),
        payload_id(id(1), id(10), 1, 1, ObjectKind::Stderr).to_vec(),
        ids.operation.to_vec(),
        ids.object.to_vec(),
        ids.version.to_vec(),
        ids.edge.to_vec(),
        Sha256::digest(body.as_bytes()).to_vec(),
        INITIAL_RESERVATION.to_be_bytes().to_vec(),
    ];
    for call in &calls {
        let bytes = payload(&r14(call.clone()));
        for value in &derived {
            assert!(!contains(&bytes, value), "kind 14 holds a derived value");
        }
        // The argument text appears exactly once, in field 14.
        assert_eq!(
            bytes
                .windows(call.arguments.len())
                .filter(|w| *w == call.arguments.as_bytes())
                .count(),
            1
        );
        for result in &results {
            assert!(!contains(result, call.arguments.as_bytes()));
            assert!(!contains(result, call.name.as_bytes()));
        }
    }
    for result in &results {
        for value in &derived[..8] {
            assert!(!contains(result, value), "kind 15/23 holds a derived value");
        }
        // Kind 14's rule-set digest is never repeated (TCR-D3).
        assert!(!contains(result, &RULE_SET_DIGEST));
    }
    // Kind 23 has no rule-set fields: 36 fixed bytes, two streams of 101 bytes
    // (no kinds or spans), the number list header and the four counters.
    assert_eq!(results[2].len(), 36 + 2 * 101 + 6 + 20 + 8);
    // Kind 15 of create_note stores no text (TCR-D1).
    let Ok(Record::ToolResult(stored)) = decode_record(15, &results[1]) else {
        panic!("kind 15");
    };
    assert!(stored.text.is_empty());
}

/// TCR-I11 (storage part; owner, 2026-10-18): every recorded call runs with a
/// full 16 KiB result, far beyond the removed 65,536-byte turn total; replay
/// accepts all of them and the over-limit `limit` record.
#[test]
fn tcr_i11_replay_accepts_every_call_with_full_results_and_no_turn_total() {
    let mut journal = Journal::started();
    let text = "r".repeat(16_384);
    for ordinal in 1..=MAX_RECORDED_CALLS {
        journal.append(r14(run14(ordinal, ToolOperation::ReadFile)));
        journal.append(Record::ToolResult(ToolResult {
            ordinal,
            text: text.clone(),
            next_offset: Some(u64::from(ordinal) * 16_384),
            ..match tool_result() {
                Record::ToolResult(result) => result,
                _ => unreachable!(),
            }
        }));
    }
    journal.append(r14(exec14(MAX_TOOL_CALL_ORDINAL, ToolDecision::Limit)));
    journal.append(complete());
    let state = inspect(&journal.bytes).unwrap();
    let op = &state.operations[&id(10)];
    assert_eq!(op.tools.len(), MAX_TOOL_CALL_ORDINAL as usize);
    let total: usize = op.tools.iter().map(|tool| tool.result_bytes).sum();
    assert_eq!(total, MAX_RECORDED_CALLS as usize * 16_384);
    assert!(total > 65_536);
}

// ---------------------------------------------------------- response text
// response-text-transfer.md: kind 24 segments and the kind 8 tail.

fn segment(offset: u64, text: &str) -> Record {
    Record::ResponseText(ResponseText {
        operation: id(10),
        generation: 1,
        offset,
        text: text.into(),
    })
}
fn tail(kind: TerminalKind, cause: Cause, offset: u64, text: &str) -> Record {
    Record::TurnTerminal(TurnTerminal {
        text_offset: offset,
        text: text.into(),
        ..terminal(kind, cause, false)
    })
}

/// RT-03: a response far above 61,440 bytes replays from kind 24 segments and
/// the kind 8 tail; replay keeps frame pieces, not text.
#[test]
fn rt03_long_response_replays_from_segments_and_tail() {
    let mut j = Journal::started();
    let full = MAX_TEXT as u64;
    for n in 0..5 {
        j.append(segment(n * full, &"a".repeat(MAX_TEXT)));
    }
    j.append(tail(TerminalKind::Complete, Cause::None, 5 * full, "end"));
    let state = inspect(&j.bytes).unwrap();
    let op = &state.operations[&id(10)];
    assert_eq!(op.response.len, 5 * full + 3);
    assert_eq!(op.response.pieces.len(), 6);
    assert_eq!(op.response.pieces[5].start, 5 * full);
    assert!(op.terminal.is_some());
}

/// RT-04 in the journal: a later segment keeps a shorter prefix; the cut
/// pieces shrink and later pieces are dropped.
#[test]
fn rt04_segment_offset_cuts_the_journaled_response() {
    let mut j = Journal::started();
    j.append(segment(0, "first text"));
    j.append(segment(10, " more"));
    j.append(segment(5, "rewritten"));
    j.append(tail(TerminalKind::Complete, Cause::None, 3, ""));
    let op = &inspect(&j.bytes).unwrap().operations[&id(10)];
    assert_eq!(op.response.len, 3);
    assert_eq!(op.response.pieces.len(), 1);
    assert_eq!(op.response.pieces[0].kept, 3);
}

/// RT-09: invalid kind 24 and kind 8 offsets reject the whole journal.
#[test]
fn rt09_invalid_response_offsets_and_states_reject() {
    // Offset above the journaled length.
    let mut j = Journal::started();
    j.append(segment(1, "x"));
    assert!(inspect(&j.bytes).is_err());
    // A no-op segment.
    let mut j = Journal::started();
    j.append(segment(0, "x"));
    j.append(segment(1, ""));
    assert!(inspect(&j.bytes).is_err());
    // Before StartAuthorized.
    let mut j = Journal::accepted();
    j.append(segment(0, "x"));
    assert!(inspect(&j.bytes).is_err());
    // Wrong generation.
    let mut j = Journal::started();
    j.append(Record::ResponseText(ResponseText {
        operation: id(10),
        generation: 2,
        offset: 0,
        text: "x".into(),
    }));
    assert!(inspect(&j.bytes).is_err());
    // After the terminal.
    let mut j = Journal::started();
    j.append(tail(TerminalKind::Complete, Cause::None, 0, "done"));
    j.append(segment(4, "x"));
    assert!(inspect(&j.bytes).is_err());
    // Terminal tail offset above the journaled length.
    let mut j = Journal::started();
    j.append(segment(0, "abc"));
    j.append(tail(TerminalKind::Complete, Cause::None, 4, "d"));
    assert!(inspect(&j.bytes).is_err());
    // Complete with an empty whole response.
    let mut j = Journal::started();
    j.append(segment(0, "abc"));
    j.append(tail(TerminalKind::Complete, Cause::None, 0, ""));
    assert!(inspect(&j.bytes).is_err());
    // Complete with only kind 24 text and an empty tail is valid.
    let mut j = Journal::started();
    j.append(segment(0, "abc"));
    j.append(tail(TerminalKind::Complete, Cause::None, 3, ""));
    assert_eq!(
        inspect(&j.bytes).unwrap().operations[&id(10)].response.len,
        3
    );
    // An oversized segment does not encode.
    assert!(
        encode_frame(
            &FrameContext {
                installation_id: id(1),
                transition_id: id(2),
                sequence: 1,
                prior_digest: [0; 32],
                expected_revision: 0,
                owner_generation: 1,
            },
            &segment(0, &"x".repeat(MAX_TEXT + 1)),
        )
        .is_err()
    );
}

/// RT-08 in replay: a restart terminal keeps the journaled prefix; a segment
/// from the earlier owner generation after restart rejects.
#[test]
fn rt08_restart_keeps_the_journaled_prefix() {
    let mut j = Journal::started();
    j.append(segment(0, "partial"));
    j.append(Record::OwnerGeneration);
    let mut stale = Journal::started();
    stale.append(segment(0, "partial"));
    stale.append(Record::OwnerGeneration);
    stale.append(segment(7, " more"));
    assert!(inspect(&stale.bytes).is_err());
    j.append(tail(TerminalKind::Interrupted, Cause::Restart, 7, ""));
    let op = &inspect(&j.bytes).unwrap().operations[&id(10)];
    assert_eq!(op.response.len, 7);
}
