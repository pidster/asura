use asura_storage::authority::{self, ReplayError};
use authority::conversation::*;
use authority::index::{Entry, Key, Tag};
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
        thinking_level: 0,
        thinking_request: 0,
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
    let frame = &j.bytes[t.frame.range()];
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
    assert_eq!(s.last_frame.range().end, loaded.len());
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
/// SJ-35 unit: no registration total. The recent set keeps 64 projects by
/// activity plus pins; every project is found through the index (`J`, `D`).
#[test]
fn sj35_project_registrations_have_no_total_and_residency_is_bounded() {
    let mut j = Journal::base();
    let register = |j: &mut Journal, n: u64, inode: u64| {
        let location = format!("/project/{n}");
        j.append(Record::ProjectRegistered(ProjectRegistered {
            request: id(100_000 + 2 * n),
            digest: request_digest(Request::Register {
                location: &location,
            })
            .unwrap(),
            project: id(100_001 + 2 * n),
            location,
            device: 1,
            inode,
            registry_revision: 1,
            visibility: 1,
        }));
    };
    for n in 1..=1000 {
        register(&mut j, n, 2 + n);
        if n == 65 {
            let s = inspect(&j.bytes).unwrap();
            // The base project plus 65 new ones: the oldest by activity left memory.
            assert_eq!(s.projects.len(), RECENT_PROJECTS);
            assert!(!s.projects.contains_key(&id(6)));
        }
    }
    let s = inspect(&j.bytes).unwrap();
    assert_eq!(s.projects.len(), RECENT_PROJECTS);
    // The newest 64 by activity, then ID.
    assert!(s.projects.contains_key(&id(100_001 + 2000)));
    assert!(!s.projects.contains_key(&id(100_001 + 2 * 936)));
    let mut index = MemoryIndex {
        segments: std::collections::BTreeMap::from([(1, j.bytes.as_slice())]),
    };
    for n in [1u64, 500, 1000] {
        let entry = s
            .find_project(id(100_001 + 2 * n), &mut index)
            .unwrap()
            .unwrap();
        assert_eq!(entry.registration.location, format!("/project/{n}"));
    }
    // The base project is found through the index after eviction.
    assert!(s.find_project(id(6), &mut index).unwrap().is_some());
    // A second registration of an existing device and inode is rejected (`D`).
    register(&mut j, 1001, 2 + 7);
    assert!(inspect(&j.bytes).is_err());
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
    let range = state.inputs[&first.input].frame.range();
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
        ToolOperation::JobList => "{\"command\":\"list\"}",
        _ => "{\"command\":\"status\",\"job\":\"0101010101010101010101010101010101\"}",
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
        reason: None,
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
        whole_command: false,
        refusal: None,
    }
}
/// A kind 14 exec_cmd call with `decision` and its consistent codes.
fn exec14(ordinal: u32, decision: ToolDecision) -> ToolCall {
    let mut call = run14(ordinal, ToolOperation::ExecCmd);
    call.decision = decision;
    match decision {
        ToolDecision::Busy => call.reason = Some(ToolDecisionReason::QueueFull),
        ToolDecision::Refused => {
            call.refusal = Some(ExecCmdRefusal {
                code: RefusalCode::SecretInCwd,
                kind: Some(SecretKind::GithubToken),
                word: None,
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
    let frame = restarted.operations[&id(10)].tools[0].result_frame.unwrap();
    assert_eq!(
        decode_record(
            15,
            &valid.bytes[frame.range().start + 144..frame.range().end - 40]
        ),
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
        for (name, follow) in [("15", r15(1, 3)), ("29", r29(1, 1))] {
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
fn reopened_tool_display_projections_are_bounded_and_hold_no_output() {
    let mut call = run14(1, ToolOperation::ExecCmd);
    call.arguments = format!("{{\"command\":\"printf {}\"}}", "界".repeat(100));
    call.number_kinds = vec![];
    let journal = started_with([r14(call), r26(1), r29(1, 1)]);
    let state = inspect(&journal.bytes).unwrap();
    let tool = &state.operations[&id(10)].tools[0];
    assert!(tool.arguments.len() <= 192);
    assert!(tool.arguments.starts_with("{\"command\":\"printf 界"));
    // Kind 29 holds no text (PC12): no preview, only its status and facts.
    assert_eq!(tool.preview, None);
    assert_eq!(tool.result_bytes, 0);
    assert_eq!(tool.result_status, Some(1));
    assert!(tool.job_result.is_some());
    assert_eq!(
        inspect(&journal.bytes).unwrap().operations[&id(10)].tools[0],
        *tool
    );
}

// J3T-03 (unit): kinds 14, 26 and 29 of one exec_cmd job call, JR11 to
// JR17 and the kind 29 reservation (tool-job-controller.md, J3 records).
const FRAME_OVERHEAD: usize = 144 + 40;
const RESULT_CAPACITY: usize = FRAME_OVERHEAD + 43 + 16_384;
const JOB_RESULT_FRAME: usize = FRAME_OVERHEAD + MAX_JOB_RESULT_PAYLOAD;
const MODEL_NOTE_FRAME: usize = FRAME_OVERHEAD + MODEL_NOTE_PAYLOAD;
fn stream23(stored: u64) -> CaptureStream {
    CaptureStream {
        observed_bytes: stored,
        stored_bytes: stored,
        digest: Some([22; 32]),
        eof: true,
        ..CaptureStream::default()
    }
}
/// A consistent state block: elapsed and bytes are 0 before spawn and
/// when settled.
fn snapshot(state: JobRunState) -> JobSnapshot {
    let quiet = state.before_spawn() || state == JobRunState::Settled;
    JobSnapshot {
        state,
        numbers: [0; 3],
        elapsed_ms: if quiet { 0 } else { 7 },
        stdout_bytes: if quiet { 0 } else { 5 },
        stderr_bytes: 0,
    }
}
fn result29_at(
    operation: Id,
    ordinal: u32,
    outcome: JobResultOutcome,
    job: Id,
    body: JobResultBody,
) -> Record {
    Record::JobResult(Box::new(JobResult {
        operation,
        generation: 1,
        ordinal,
        status: outcome.status(),
        outcome,
        job,
        body,
    }))
}
fn result29(ordinal: u32, outcome: JobResultOutcome, job: Id, body: JobResultBody) -> Record {
    result29_at(id(10), ordinal, outcome, job, body)
}
/// Handle delivery of the exec_cmd call at `ordinal`: status 1 (`ok`) or 6
/// (`cancelled`), with a `running` snapshot.
fn r29(ordinal: u32, status: u8) -> Record {
    let outcome = if status == 6 {
        JobResultOutcome::Cancelled
    } else {
        JobResultOutcome::Ok
    };
    result29(
        ordinal,
        outcome,
        [0; 16],
        JobResultBody::State(snapshot(JobRunState::Running)),
    )
}
/// A kind 15 for an exec_cmd call; JR11 rejects it.
fn r15(ordinal: u32, status: u8) -> Record {
    Record::ToolResult(ToolResult {
        operation: id(10),
        generation: 1,
        ordinal,
        status,
        text: "exit_code=3\nstdout:\nhello\nstderr:\n".into(),
        next_offset: None,
        truncated: false,
    })
}
fn run_exec(ordinal: u32) -> Record {
    r14(run14(ordinal, ToolOperation::ExecCmd))
}
/// A `run` `job` call with operation code 11 to 16.
fn job14_at(operation: Id, ordinal: u32, op: ToolOperation) -> Record {
    let arguments = format!("{{\"command\":\"{}\"}}", op.as_str());
    r14(ToolCall {
        operation,
        name: "job".into(),
        name_bytes: 3,
        tool_operation: Some(op),
        form: ArgumentForm::Verbatim,
        arguments_bytes: arguments.len() as u32,
        arguments,
        number_kinds: vec![],
        ..run14(ordinal, ToolOperation::ReadFile)
    })
}
fn job14(ordinal: u32, op: ToolOperation) -> Record {
    job14_at(id(10), ordinal, op)
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
fn j3t_03_exec_cmd_call_is_a_job_resolved_by_kind_29() {
    let exec_cmd = |journal: &Journal| {
        let state = inspect(&journal.bytes).unwrap();
        state.operations[&id(10)].tools[0].clone()
    };
    // Run: 14, then 26, then 29 (handle delivery), then a terminal.
    let journal = started_with([run_exec(1)]);
    let tool = exec_cmd(&journal);
    let call = tool.exec_cmd.as_deref().unwrap();
    assert_eq!(tool.tool_operation, Some(ToolOperation::ExecCmd));
    assert_eq!(tool.tool, Some(RegisteredTool::ExecCmd));
    assert_eq!(tool.arguments, EXEC_ARGUMENTS);
    assert_eq!(call.job, exec_cmd_job_id(id(10), 1, 1));
    assert_eq!(call.intent_sequence, journal.sequence);
    assert!(call.job_intent.is_none() && tool.unresolved());
    let journal = started_with([run_exec(1), r26(1)]);
    let tool = exec_cmd(&journal);
    assert!(tool.exec_cmd.as_deref().unwrap().job_intent.is_some() && tool.unresolved());
    let journal = started_with([run_exec(1), r26(1), r29(1, 1)]);
    let tool = exec_cmd(&journal);
    assert_eq!(tool.result_status, Some(1));
    assert!(!tool.unresolved());
    assert_eq!(
        tool.job_result.as_deref().map(|r| (r.outcome, r.job)),
        Some((JobResultOutcome::Ok, [0; 16]))
    );
    for journal in [
        started_with([run_exec(1), r26(1), r29(1, 1), complete()]),
        started_with([run_exec(1), r26(1), r29(1, 6), failed()]),
        started_with([run_exec(1), r26(1), failed()]),
        started_with([run_exec(1), failed()]),
    ] {
        let state = inspect(&journal.bytes).unwrap();
        assert!(state.operations[&id(10)].terminal.is_some());
        assert_eq!(state.active_operation, None);
    }
    // Refused, busy and denied: kind 14 only, then a terminal; no derived
    // identities and no job.
    for decision in [
        ToolDecision::Refused,
        ToolDecision::Busy,
        ToolDecision::Denied,
    ] {
        let journal = started_with([r14(exec14(1, decision))]);
        let tool = exec_cmd(&journal);
        assert_eq!(tool.decision, decision);
        assert!(tool.exec_cmd.is_none() && !tool.unresolved());
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
            ..match tool_result() {
                Record::ToolResult(r) => r,
                _ => unreachable!(),
            }
        }),
        run_exec(3),
        r26(3),
        r29(3, 1),
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
fn j3t_03_jr11_jr12_jr13_every_invalid_order_is_corrupt() {
    let refused = || r14(exec14(1, ToolDecision::Refused));
    let busy = || r14(exec14(1, ToolDecision::Busy));
    let running = || JobResultBody::State(snapshot(JobRunState::Running));
    let cases: Vec<(&str, Journal, Vec<Record>)> = vec![
        ("29 before 14", Journal::started(), vec![r29(1, 1)]),
        (
            "JR13: 29 before 26",
            Journal::started(),
            vec![run_exec(1), r29(1, 1)],
        ),
        (
            "JR12: 29 twice",
            Journal::started(),
            vec![run_exec(1), r26(1), r29(1, 1), r29(1, 1)],
        ),
        (
            "JR11: kind 15 for exec_cmd",
            Journal::started(),
            vec![run_exec(1), r26(1), r15(1, 1)],
        ),
        (
            "JR11: kind 15 for a job command",
            Journal::started(),
            vec![job14(1, ToolOperation::JobList), r15(1, 1)],
        ),
        (
            "JR11: kind 29 for an ordinary tool",
            Journal::started(),
            vec![tool_call(), r29(1, 1)],
        ),
        (
            "29 after refused",
            Journal::started(),
            vec![refused(), r29(1, 1)],
        ),
        ("29 after busy", Journal::started(), vec![busy(), r29(1, 1)]),
        (
            "14 after terminal",
            Journal::started(),
            vec![failed(), run_exec(1)],
        ),
        (
            "29 after terminal",
            Journal::started(),
            vec![run_exec(1), r26(1), failed(), r29(1, 1)],
        ),
        (
            "complete while 29 pending",
            Journal::started(),
            vec![run_exec(1), r26(1), complete()],
        ),
        (
            "29 wrong ordinal",
            Journal::started(),
            vec![
                run_exec(1),
                r26(1),
                result29(2, JobResultOutcome::Ok, [0; 16], running()),
            ],
        ),
        (
            "29 wrong operation",
            Journal::started(),
            vec![
                run_exec(1),
                r26(1),
                result29_at(id(99), 1, JobResultOutcome::Ok, [0; 16], running()),
            ],
        ),
        ("14 skips an ordinal", Journal::started(), vec![run_exec(2)]),
        (
            "14 while 29 pending",
            Journal::started(),
            vec![run_exec(1), r26(1), run_exec(2)],
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
            "29 after restart",
            Journal::started(),
            vec![run_exec(1), r26(1), Record::OwnerGeneration, r29(1, 1)],
        ),
        (
            "JR12: code 10 names its job in field 6",
            Journal::started(),
            vec![
                run_exec(1),
                r26(1),
                result29(1, JobResultOutcome::Ok, job(1), running()),
            ],
        ),
        (
            "JR12: code 10 without its state block",
            Journal::started(),
            vec![
                run_exec(1),
                r26(1),
                result29(1, JobResultOutcome::Ok, [0; 16], JobResultBody::None),
            ],
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
/// JR10: a terminal while a job-mode call lacks kind 29 is accepted; the
/// job stays unsettled.
#[test]
fn j3t_03_jr10_terminal_with_run_call_lacking_kind_29_is_accepted() {
    let journal = started_with([
        run_exec(1),
        r26(1),
        Record::OwnerGeneration,
        Record::TurnTerminal(terminal(TerminalKind::Interrupted, Cause::Restart, false)),
    ]);
    let state = inspect(&journal.bytes).unwrap();
    let tool = &state.operations[&id(10)].tools[0];
    assert!(tool.result_frame.is_none());
    assert_eq!(state.active_operation, None);
    assert_eq!(state.unsettled_jobs().len(), 1);
    for cancelled in [false, true] {
        let mut journal = started_with([run_exec(1), r26(1)]);
        if cancelled {
            // A turn cancel during the yield window: kind 29 status 6
            // commits before the terminal.
            journal.append(cancel_requested());
            journal.append(r29(1, 6));
            journal.append(Record::TurnTerminal(terminal(
                TerminalKind::Cancelled,
                Cause::UserCancel,
                false,
            )));
        } else {
            journal.append(failed());
        }
        let state = inspect(&journal.bytes).unwrap();
        assert_eq!(
            state.operations[&id(10)].tools[0].result_status,
            cancelled.then_some(6)
        );
    }
}
/// TCR-D16 and J3: a `run` kind 14 holds its result: kind 15 for codes 1 to
/// 9, kind 29 for codes 10 to 16; a `job note` call also holds its model
/// note. A call that does not run holds nothing more.
#[test]
fn j3t_03_reserve_holds_one_kind_29_frame_until_it_commits() {
    let reserved = |journal: &Journal| {
        let state = inspect(&journal.bytes).unwrap();
        (state.reserved_bytes, state.reserved_frames)
    };
    let (base, frames) = reserved(&Journal::started());
    assert_eq!(JOB_RESULT_FRAME, 528);
    assert_eq!(MODEL_NOTE_FRAME, 242);
    assert_eq!(
        reserved(&started_with([run_exec(1)])),
        (base + JOB_RESULT_FRAME, frames + 1)
    );
    assert_eq!(
        reserved(&started_with([run_exec(1), r26(1), r29(1, 1)])),
        (base + JOB_LEDGER, frames + 2)
    );
    for decision in [ToolDecision::Refused, ToolDecision::Busy] {
        assert_eq!(
            reserved(&started_with([r14(exec14(1, decision))])),
            (base, frames)
        );
    }
    // An ordinary run tool keeps the kind 15 reservation.
    assert_eq!(
        reserved(&started_with([tool_call()])),
        (base + RESULT_CAPACITY, frames + 1)
    );
    // Codes 11 to 15 hold one kind 29 frame; code 16 also its model note.
    for op in [
        ToolOperation::JobList,
        ToolOperation::JobStatus,
        ToolOperation::JobRead,
        ToolOperation::JobWait,
        ToolOperation::JobCancel,
    ] {
        assert_eq!(
            reserved(&started_with([job14(1, op)])),
            (base + JOB_RESULT_FRAME, frames + 1)
        );
    }
    let note = [
        run_exec(1),
        r26(1),
        r29(1, 1),
        job14(2, ToolOperation::JobNote),
    ];
    assert_eq!(
        reserved(&started_with(note.clone())),
        (
            base + JOB_LEDGER + JOB_RESULT_FRAME + MODEL_NOTE_FRAME,
            frames + 4
        )
    );
    let mut with_note = note.to_vec();
    with_note.push(model_note(2, job(1)));
    assert_eq!(
        reserved(&started_with(with_note.clone())),
        (base + JOB_LEDGER + JOB_RESULT_FRAME, frames + 3)
    );
    with_note.push(result29(
        2,
        JobResultOutcome::Ok,
        job(1),
        JobResultBody::None,
    ));
    assert_eq!(
        reserved(&started_with(with_note)),
        (base + JOB_LEDGER, frames + 2)
    );
    // After restart the old operation keeps its kind 29 frame for the
    // terminal only while the result is missing.
    let journal = started_with([run_exec(1), Record::OwnerGeneration]);
    let state = inspect(&journal.bytes).unwrap();
    assert!(state.reserved_bytes >= JOB_RESULT_FRAME);
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
        "run exec_cmd needs kind 26 before kind 29",
        started_with([run_exec(1), r26(1), r29(1, 1)]),
        started_with([run_exec(1), r29(1, 1)]),
    );
    expect_rule(
        "another run takes kind 15 only",
        started_with([tool_call(), tool_result()]),
        started_with([tool_call(), r29(1, 1)]),
    );
    expect_rule(
        "a call that does not run takes no kind 15",
        started_with([rejected14(1).into_record(), failed()]),
        started_with([rejected14(1).into_record(), tool_result()]),
    );
    expect_rule(
        "a call that does not run takes no kind 29",
        started_with([r14(exec14(1, ToolDecision::Busy)), failed()]),
        started_with([r14(exec14(1, ToolDecision::Busy)), r29(1, 1)]),
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
        r26(1),
        r29(1, 1),
        r14(run14(2, ToolOperation::CreateNote)),
    ]);
    let state = inspect(&journal.bytes).unwrap();
    let tools = &state.operations[&id(10)].tools;
    let call = tools[0].exec_cmd.as_deref().unwrap();
    assert_eq!(call.job, exec_cmd_job_id(id(10), 1, 1));
    assert_eq!(
        call.stdout_payload,
        payload_id(id(1), call.job, ObjectKind::Stdout)
    );
    assert_eq!(
        call.stderr_payload,
        payload_id(id(1), call.job, ObjectKind::Stderr)
    );
    let Record::ToolCall(stored) = decode_record(
        14,
        &journal.bytes
            [tools[0].intent_frame.range().start + 144..tools[0].intent_frame.range().end - 40],
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
        payload_id(id(2), call.job, ObjectKind::Stdout)
    );
}
/// TCR-U17 and J3T-17: one source for each fact. Encoded kind 14, 15, 28
/// and 29 payloads hold no copy of the arguments, no recomputed value and
/// no body digest; kind 29 holds no output text (TJ-26).
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
    let results = [payload(&r29(1, 1)), payload(&create_result(""))];
    // Kind 28 is keyed by its job ID; it holds no other derived value.
    let settled = payload(&r28(1));
    let job = exec_cmd_job_id(id(10), 1, 1);
    let ids = create_call(2).identity;
    let derived: Vec<Vec<u8>> = vec![
        job.to_vec(),
        payload_id(id(1), job, ObjectKind::Stdout).to_vec(),
        payload_id(id(1), job, ObjectKind::Stderr).to_vec(),
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
        for result in results.iter().chain([&settled]) {
            assert!(!contains(result, call.arguments.as_bytes()));
            assert!(!contains(result, call.name.as_bytes()));
        }
    }
    for result in &results {
        for value in &derived[..8] {
            assert!(!contains(result, value), "kind 29/15 holds a derived value");
        }
        // Kind 14's rule-set digest is never repeated (TCR-D3).
        assert!(!contains(result, &RULE_SET_DIGEST));
    }
    for value in &derived[1..8] {
        assert!(!contains(&settled, value), "kind 28 holds a derived value");
    }
    assert!(!contains(&settled, &RULE_SET_DIGEST));
    // Kind 29 of a handle delivery: 46 fixed bytes and the 29-byte state
    // block; no text field exists.
    assert_eq!(results[0].len(), 46 + 29);
    // Kind 28 has no rule-set fields: 40 fixed bytes, two streams of 109
    // bytes (no kinds or spans), the number list header, the four counters
    // and the settled time.
    assert_eq!(settled.len(), 40 + 2 * 109 + 6 + 20 + 8 + 8);
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

// ---------------------------------------------------------- thinking text
// thinking-level.md: kind 25 segments and the kind 21 level and request.

fn thought(offset: u64, text: &str) -> Record {
    Record::ThinkingText(ResponseText {
        operation: id(10),
        generation: 1,
        offset,
        text: text.into(),
    })
}

/// TL-09 and TL-12 in replay: kind 25 segments build the operation's
/// thinking pieces beside the response; a cut keeps the shorter prefix; a
/// restart terminal keeps the journaled thinking.
#[test]
fn tl09_tl12_thinking_replays_beside_the_response() {
    let mut j = Journal::started();
    let full = MAX_TEXT as u64;
    for n in 0..3 {
        j.append(thought(n * full, &"t".repeat(MAX_TEXT)));
    }
    j.append(thought(3 * full - 5, "cut"));
    j.append(segment(0, "answer"));
    j.append(tail(TerminalKind::Complete, Cause::None, 6, ""));
    let op = &inspect(&j.bytes).unwrap().operations[&id(10)];
    assert_eq!(op.thinking.len, 3 * full - 2);
    assert_eq!(op.thinking.pieces.len(), 4);
    assert_eq!(op.response.len, 6);
    assert_eq!(op.response.pieces.len(), 1);
    let mut j = Journal::started();
    j.append(thought(0, "partial"));
    j.append(Record::OwnerGeneration);
    j.append(tail(TerminalKind::Interrupted, Cause::Restart, 0, ""));
    let op = &inspect(&j.bytes).unwrap().operations[&id(10)];
    assert_eq!(op.thinking.len, 7);
    assert_eq!(op.response.len, 0);
}

/// TL-15 in replay: kind 25 follows the kind 24 rules; kind 21 rejects an
/// unknown level or request code.
#[test]
fn tl15_invalid_thinking_records_reject() {
    for records in [
        // Offset above the journaled thinking length.
        vec![thought(1, "x")],
        // A no-op segment.
        vec![thought(0, "x"), thought(1, "")],
        // Response length does not count toward thinking.
        vec![segment(0, "abc"), thought(2, "x")],
    ] {
        let mut j = Journal::started();
        for record in records {
            j.append(record);
        }
        assert!(inspect(&j.bytes).is_err());
    }
    // Before StartAuthorized.
    let mut j = Journal::accepted();
    j.append(thought(0, "x"));
    assert!(inspect(&j.bytes).is_err());
    // After the terminal.
    let mut j = Journal::started();
    j.append(tail(TerminalKind::Complete, Cause::None, 0, "done"));
    j.append(thought(0, "x"));
    assert!(inspect(&j.bytes).is_err());
    // An oversized segment does not encode.
    let context = FrameContext {
        installation_id: id(1),
        transition_id: id(2),
        sequence: 1,
        prior_digest: [0; 32],
        expected_revision: 0,
        owner_generation: 1,
    };
    assert!(encode_frame(&context, &thought(0, &"x".repeat(MAX_TEXT + 1))).is_err());
    // Kind 21 level and request codes round trip and reject out of range.
    for (level, request, valid) in [(4, 5, true), (0, 0, true), (5, 0, false), (0, 6, false)] {
        let record = Record::TurnAccepted(Box::new(TurnAccepted {
            thinking_level: level,
            thinking_request: request,
            ..turn()
        }));
        match encode_frame(&context, &record) {
            Ok(frame) => {
                assert!(valid);
                // The format-1 frame: 144-byte header, payload, 40-byte trailer.
                let payload = &frame[144..frame.len() - 40];
                assert_eq!(decode_record(21, payload).unwrap(), record);
            }
            Err(_) => assert!(!valid),
        }
    }
}

// J2T-02 and J2T-17 (unit): replay order of job records, JR1 to JR10
// (tool-job-controller.md, replay order and the job reservation ledger).
fn model_origin(operation: Id, generation: u64, ordinal: u32) -> JobOrigin {
    JobOrigin::Model {
        operation,
        generation,
        ordinal,
    }
}
fn r26(ordinal: u32) -> Record {
    r26_for(model_origin(id(10), 1, ordinal))
}
fn r26_for(origin: JobOrigin) -> Record {
    Record::JobIntent(JobIntent {
        origin,
        agent: 0,
        time: 1_760_000_000_000,
    })
}
fn job(ordinal: u32) -> Id {
    job_id(&model_origin(id(10), 1, ordinal))
}
fn r27(ordinal: u32, reason: JobReason) -> Record {
    r27_for(job(ordinal), reason)
}
fn r27_for(job: Id, reason: JobReason) -> Record {
    Record::JobCancelRequested(JobCancelRequested {
        job,
        requester: reason.requester().unwrap(),
        reason,
        kill: reason == JobReason::PersonKill,
    })
}
fn settled28(job: Id) -> JobSettled {
    let stream = JobStream {
        capture: stream23(5),
        lines: 2,
    };
    JobSettled {
        job,
        outcome: JobOutcome::Exited,
        reason: JobReason::None,
        exit_kind: 1,
        exit_value: 3,
        cleanup: JobCleanup::Confirmed,
        unconfirmed_reason: 0,
        escapees: 0,
        elapsed_ms: 9,
        capture_state: JobCaptureState::Complete,
        capture_reason: CaptureReason::None,
        stdout: stream.clone(),
        stderr: stream,
        distinct_numbers: 0,
        numbers: vec![],
        pause_count: 0,
        first_pause_ms: 0,
        paused_ms: 0,
        final_reservation: INITIAL_RESERVATION,
        settled_time: 1_760_000_000_002,
    }
}
fn r28(ordinal: u32) -> Record {
    Record::JobSettled(Box::new(settled28(job(ordinal))))
}
fn r28_for(job: Id) -> Record {
    Record::JobSettled(Box::new(settled28(job)))
}
fn r28i(ordinal: u32) -> Record {
    Record::JobSettled(Box::new(JobSettled::interrupted(
        job(ordinal),
        1_760_000_000_001,
    )))
}
/// A job-mode call: kinds 14, 26 and 15.
fn job_call(ordinal: u32) -> [Record; 3] {
    [run_exec(ordinal), r26(ordinal), r29(ordinal, 1)]
}
const JOB_LEDGER: usize = 4_278 + 203;
#[test]
fn j2t_02_job_mode_orders_replay_and_derive_job_facts() {
    for records in [
        // 14, 26, 15 (handle delivery), 27, 28, terminal.
        vec![
            run_exec(1),
            r26(1),
            r29(1, 1),
            r27(1, JobReason::ModelCancel),
            r28(1),
            complete(),
        ],
        // 27 and 28 before kind 15.
        vec![
            run_exec(1),
            r26(1),
            r27(1, JobReason::PersonKill),
            r28(1),
            r29(1, 1),
            complete(),
        ],
        // JR8: 27 and 28 after the operation's terminal.
        vec![
            run_exec(1),
            r26(1),
            r29(1, 1),
            complete(),
            r27(1, JobReason::IdentityChanged),
            r28(1),
        ],
        // A job settles without kind 27.
        vec![run_exec(1), r26(1), r29(1, 1), r28(1), complete()],
    ] {
        let journal = started_with(records.clone());
        let state = inspect(&journal.bytes).unwrap();
        assert!(state.jobs.is_empty(), "{records:?}");
        let tool = &state.operations[&id(10)].tools[0];
        let exec = tool.exec_cmd.as_deref().unwrap();
        assert!(exec.job_intent.is_some());
        assert_eq!(exec.job, job(1));
        assert_eq!(
            [exec.stdout_payload, exec.stderr_payload],
            job_payload_ids(id(1), job(1))
        );
        assert_eq!(tool.result_status, Some(1));
    }
    // An unsettled job: derived identity, conversation, project, owner
    // generation and payloads; the call's kind 15 resolves the call only.
    let journal = started_with([run_exec(1), r26(1), r29(1, 1), run_exec(2), r26(2)]);
    let state = inspect(&journal.bytes).unwrap();
    let jobs = state.unsettled_jobs();
    assert_eq!(jobs.len(), 2);
    assert!(jobs[0].intent.sequence < jobs[1].intent.sequence);
    let first = &jobs[0];
    assert_eq!(first.job, job(1));
    assert_eq!(first.origin, model_origin(id(10), 1, 1));
    assert_eq!((first.agent, first.owner_generation), (0, 1));
    assert_eq!((first.conversation, first.project), (id(8), id(6)));
    assert_eq!(first.payloads, job_payload_ids(id(1), job(1)));
    assert_eq!(first.cancel, None);
    assert!(!state.operations[&id(10)].tools[0].unresolved());
    assert!(state.operations[&id(10)].tools[1].unresolved());
}
#[test]
fn j2t_02_jr1_kind_26_names_the_active_operations_last_run_exec_cmd() {
    let rejected = |records: Vec<Record>| {
        assert_eq!(
            inspect(&started_with(records.clone()).bytes).unwrap_err(),
            ReplayError::Corrupt,
            "{records:?}"
        );
    };
    assert!(inspect(&started_with([run_exec(1), r26(1)]).bytes).is_ok());
    // No kind 14, or not the last kind 14.
    rejected(vec![r26(1)]);
    rejected(vec![run_exec(1), r26(1), r29(1, 1), run_exec(2), r26(1)]);
    // Wrong operation, generation or ordinal.
    rejected(vec![run_exec(1), r26_for(model_origin(id(99), 1, 1))]);
    rejected(vec![run_exec(1), r26_for(model_origin(id(10), 2, 1))]);
    rejected(vec![run_exec(1), r26(2)]);
    // The kind 14 already has kind 23, kind 15 or kind 26.
    rejected(vec![run_exec(1), r26(1), r26(1)]);
    rejected(vec![run_exec(1), r26(1), r29(1, 1), r26(1)]);
    // Not a `run` exec_cmd: another operation, or another decision.
    rejected(vec![tool_call(), r26(1)]);
    for decision in [ToolDecision::Refused, ToolDecision::Busy] {
        rejected(vec![r14(exec14(1, decision)), r26(1)]);
    }
    // The operation has a terminal, or is from an older owner generation.
    rejected(vec![run_exec(1), failed(), r26(1)]);
    rejected(vec![run_exec(1), Record::OwnerGeneration, r26(1)]);
    // JR2: person origin waits for P2.
    rejected(vec![
        run_exec(1),
        r26_for(JobOrigin::Person { input: id(30) }),
    ]);
    // JR3: the agent field is 0 (a same-size tamper, resealed).
    let mut journal = started_with([run_exec(1), r26(1)]);
    tamper_last(&mut journal, |payload| payload[4] = 1);
    assert_eq!(inspect(&journal.bytes).unwrap_err(), ReplayError::Corrupt);
}
#[test]
fn j2t_02_jr4_jr5_jr10_kind_29_follows_kind_26_and_turn_rules_hold() {
    // JR4 and JR13: the call's kind 29 follows its kind 26.
    expect_rule(
        "JR13 kind 29 needs kind 26",
        started_with([run_exec(1), r26(1), r29(1, 1)]),
        started_with([run_exec(1), r29(1, 1)]),
    );
    // JR5: the job ID of a call is derived once (a repeated kind 26 is the
    // only way to derive it again).
    expect_rule(
        "JR5",
        started_with([run_exec(1), r26(1), r28(1), r29(1, 1)]),
        started_with([run_exec(1), r26(1), r28(1), r29(1, 1), r26(1)]),
    );
    // The kind 14 rules are unchanged: handle delivery resolves the call.
    expect_rule(
        "earlier run has kind 15",
        started_with([run_exec(1), r26(1), r29(1, 1), run_exec(2)]),
        started_with([run_exec(1), r26(1), run_exec(2)]),
    );
    expect_rule(
        "complete needs kind 15",
        started_with([run_exec(1), r26(1), r28(1), r29(1, 1), complete()]),
        started_with([run_exec(1), r26(1), r28(1), complete()]),
    );
    // JR10: a terminal while a job-mode call lacks kind 15 is accepted; the
    // job stays unsettled.
    for records in [
        vec![run_exec(1), r26(1), failed()],
        vec![
            run_exec(1),
            r26(1),
            cancel_requested(),
            Record::TurnTerminal(terminal(TerminalKind::Cancelled, Cause::UserCancel, false)),
        ],
        vec![
            run_exec(1),
            r26(1),
            Record::OwnerGeneration,
            Record::TurnTerminal(terminal(TerminalKind::Interrupted, Cause::Restart, false)),
        ],
    ] {
        let state = inspect(&started_with(records).bytes).unwrap();
        assert_eq!(state.active_operation, None);
        assert_eq!(state.unsettled_jobs().len(), 1);
        assert!(state.operations[&id(10)].tools[0].unresolved());
    }
}
#[test]
fn j2t_02_jr6_kind_27_once_for_an_unsettled_job_of_its_generation() {
    let base = || vec![run_exec(1), r26(1), r29(1, 1)];
    let with = |extra: Vec<Record>| {
        let mut records = base();
        records.extend(extra);
        started_with(records)
    };
    let valid = with(vec![r27(1, JobReason::ModelCancel)]);
    let state = inspect(&valid.bytes).unwrap();
    let cancel = state.jobs[&job(1)].cancel.unwrap();
    assert_eq!(
        (cancel.requester, cancel.reason, cancel.kill),
        (JobRequester::Model, JobReason::ModelCancel, false)
    );
    for (name, broken) in [
        ("unknown job", with(vec![r27(2, JobReason::ModelCancel)])),
        (
            "second kind 27",
            with(vec![
                r27(1, JobReason::PersonStop),
                r27(1, JobReason::PersonKill),
            ]),
        ),
        (
            "after kind 28",
            with(vec![r28(1), r27(1, JobReason::ClientCancel)]),
        ),
        (
            "later owner generation",
            with(vec![
                complete(),
                Record::OwnerGeneration,
                r27(1, JobReason::Shutdown),
            ]),
        ),
    ] {
        expect_rule(name, with(vec![r27(1, JobReason::ModelCancel)]), broken);
    }
    // The requester must agree with the reason table (codec).
    let mut journal = with(vec![r27(1, JobReason::ModelCancel)]);
    tamper_last(&mut journal, |payload| {
        payload[16] = JobRequester::Client as u8
    });
    assert_eq!(inspect(&journal.bytes).unwrap_err(), ReplayError::Corrupt);
}
#[test]
fn j2t_02_jr7_kind_28_once_and_interrupted_only_in_a_later_generation() {
    let base = || vec![run_exec(1), r26(1), r29(1, 1), complete()];
    let with = |extra: Vec<Record>| {
        let mut records = base();
        records.extend(extra);
        started_with(records)
    };
    expect_rule("unknown job", with(vec![r28(1)]), with(vec![r28(2)]));
    expect_rule(
        "second kind 28",
        with(vec![r28(1)]),
        with(vec![r28(1), r28(1)]),
    );
    expect_rule(
        "interrupted in the same generation",
        with(vec![r28(1)]),
        with(vec![r28i(1)]),
    );
    expect_rule(
        "settlement after restart is interrupted",
        with(vec![Record::OwnerGeneration, r28i(1)]),
        with(vec![Record::OwnerGeneration, r28(1)]),
    );
    // Outcome rules (codec): exited with a signal exit kind.
    let mut journal = with(vec![r28(1)]);
    tamper_last(&mut journal, |payload| payload[18] = 2);
    assert_eq!(inspect(&journal.bytes).unwrap_err(), ReplayError::Corrupt);
    // Interrupted after two restarts, and while the old operation is still
    // active before its kind 8 (JR8).
    let state = inspect(
        &with(vec![
            Record::OwnerGeneration,
            Record::OwnerGeneration,
            r28i(1),
        ])
        .bytes,
    )
    .unwrap();
    assert!(state.jobs.is_empty());
    let state = inspect(
        &started_with([
            run_exec(1),
            r26(1),
            Record::OwnerGeneration,
            r28i(1),
            Record::TurnTerminal(terminal(TerminalKind::Interrupted, Cause::Restart, false)),
        ])
        .bytes,
    )
    .unwrap();
    assert!(state.jobs.is_empty() && state.active_operation.is_none());
    // The old operation accepts no kind 27 and no new kind 26.
    for record in [r27(1, JobReason::Shutdown), r26(1)] {
        assert_eq!(
            inspect(&started_with([run_exec(1), r26(1), Record::OwnerGeneration, record]).bytes)
                .unwrap_err(),
            ReplayError::Corrupt
        );
    }
}
#[test]
fn j2t_02_jr9_at_most_64_unsettled_jobs() {
    let mut journal = Journal::started();
    for ordinal in 1..=64 {
        for record in job_call(ordinal) {
            journal.append(record);
        }
    }
    let state = inspect(&journal.bytes).unwrap();
    assert_eq!(state.jobs.len(), MAX_UNSETTLED_JOBS);
    assert_eq!(
        (state.reserved_bytes, state.reserved_frames),
        (
            inspect(&Journal::started().bytes).unwrap().reserved_bytes + 64 * JOB_LEDGER,
            inspect(&Journal::started().bytes).unwrap().reserved_frames + 128
        )
    );
    let mut over = Journal {
        bytes: journal.bytes.clone(),
        ..journal
    };
    over.append(run_exec(65));
    assert!(inspect(&over.bytes).is_ok());
    over.append(r26(65));
    assert_eq!(inspect(&over.bytes).unwrap_err(), ReplayError::Limit);
    // A settlement makes room for the next job.
    let mut journal = Journal {
        bytes: journal.bytes.clone(),
        ..journal
    };
    journal.append(r28(1));
    for record in job_call(65) {
        journal.append(record);
    }
    assert_eq!(inspect(&journal.bytes).unwrap().jobs.len(), 64);
}
/// The second conversation's turn: operation `id(43)`.
fn second_turn() -> [Record; 2] {
    [
        Record::TurnAccepted(Box::new(TurnAccepted {
            request: id(40),
            digest: request_digest(Request::Submit {
                project: id(6),
                conversation: None,
                expected_generation: 0,
                prompt: "second",
            })
            .unwrap(),
            conversation: id(41),
            task: id(42),
            operation: id(43),
            prompt: "second".into(),
            ..turn()
        })),
        Record::StartAuthorized(StartAuthorized {
            operation: id(43),
            generation: 1,
            helper: id(44),
            input_digest: [12; 32],
        }),
    ]
}
/// J2T-17 (unit): kind 28 after its turn's terminal, during another
/// conversation's turn and with no active operation; the ledger is
/// independent of the active operation (J2-E4).
#[test]
fn j2t_17_records_after_turn_end_and_during_another_turn() {
    let [accepted, started] = second_turn();
    let mut journal = started_with([
        run_exec(1),
        r26(1),
        r29(1, 1),
        run_exec(2),
        r26(2),
        r29(2, 1),
        complete(),
    ]);
    let idle = inspect(&journal.bytes).unwrap();
    assert_eq!(idle.active_operation, None);
    assert_eq!(
        (idle.reserved_bytes, idle.reserved_frames),
        (2 * JOB_LEDGER, 4)
    );
    journal.append(accepted);
    journal.append(started);
    // During the second conversation's turn.
    journal.append(r27(1, JobReason::ClientCancel));
    journal.append(r28(1));
    let during = inspect(&journal.bytes).unwrap();
    assert_eq!(during.active_operation, Some(id(43)));
    assert_eq!(during.unsettled_jobs().len(), 1);
    let second = &during.unsettled_jobs()[0];
    assert_eq!((second.conversation, second.project), (id(8), id(6)));
    // The first conversation stays resident while its job is unsettled.
    assert!(during.conversations.contains_key(&id(8)));
    journal.append(Record::TurnTerminal(TurnTerminal {
        operation: id(43),
        ..terminal(TerminalKind::Complete, Cause::None, true)
    }));
    // With no active operation.
    journal.append(r28(2));
    let done = inspect(&journal.bytes).unwrap();
    assert!(done.jobs.is_empty());
    assert_eq!((done.reserved_bytes, done.reserved_frames), (0, 0));
    // A job ID never settles twice, even after its turn has ended.
    journal.append(r28_for(job(2)));
    assert_eq!(inspect(&journal.bytes).unwrap_err(), ReplayError::Corrupt);
}
/// J2T-03 (unit): the job-mode call holds the kind 15 reservation until
/// kind 15 but no kind 23; the ledger holds kinds 27 and 28.
#[test]
fn j2t_03_job_mode_call_reserves_the_ledger_instead_of_kind_23() {
    let reserved = |journal: &Journal| {
        let state = inspect(&journal.bytes).unwrap();
        (state.reserved_bytes, state.reserved_frames)
    };
    let (base, frames) = reserved(&Journal::started());
    assert_eq!(
        reserved(&started_with([run_exec(1)])),
        (base + JOB_RESULT_FRAME, frames + 1)
    );
    assert_eq!(
        reserved(&started_with([run_exec(1), r26(1)])),
        (base + JOB_RESULT_FRAME + JOB_LEDGER, frames + 3)
    );
    assert_eq!(
        reserved(&started_with([
            run_exec(1),
            r26(1),
            r27(1, JobReason::TurnEnded)
        ])),
        (base + JOB_RESULT_FRAME + 4_278, frames + 2)
    );
    assert_eq!(
        reserved(&started_with([run_exec(1), r26(1), r29(1, 1)])),
        (base + JOB_LEDGER, frames + 2)
    );
    assert_eq!(
        reserved(&started_with([run_exec(1), r26(1), r29(1, 1), r28(1)])),
        (base, frames)
    );
}

// ------------------------------------------------- J3 and J4 job records
// J3T-03 (unit): JR11 to JR17 for the `job` tool codes 11 to 16; J4T-02
// (unit): kind 30 replay rules; J4T-03 (unit): `L`, `M`, `Y`, `U` and `E`
// entries and the listing reads over the live state and the delta.

/// A kind 30 model note that cites the `job note` call at `ordinal`.
fn model_note(ordinal: u32, job: Id) -> Record {
    model_note_at(id(10), ordinal, job)
}
fn model_note_at(operation: Id, ordinal: u32, job: Id) -> Record {
    Record::JobRetention(Box::new(JobRetention {
        job,
        action: RetentionAction::Note,
        author: RetentionAuthor::Model {
            operation,
            generation: 1,
            ordinal,
        },
        time: 1_760_000_000_005,
        note: String::new(),
    }))
}
/// A kind 30 written by the person.
fn person(job: Id, action: RetentionAction) -> Record {
    Record::JobRetention(Box::new(JobRetention {
        job,
        action,
        author: RetentionAuthor::Person { request: id(77) },
        time: 1_760_000_000_006,
        note: if action == RetentionAction::Note {
            "check the $ASURA_SECRET_0 rotation".into()
        } else {
            String::new()
        },
    }))
}
/// A job settled by kind 28, with its call resolved: ordinal 1.
fn settled_job() -> Vec<Record> {
    vec![run_exec(1), r26(1), r29(1, 1), r28(1)]
}
/// A live job at ordinal 1 with its call resolved.
fn live_job() -> Vec<Record> {
    vec![run_exec(1), r26(1), r29(1, 1)]
}
fn with(mut records: Vec<Record>, more: impl IntoIterator<Item = Record>) -> Vec<Record> {
    records.extend(more);
    records
}
fn page(state: JobPageState, source_len: u32) -> JobPage {
    JobPage {
        stream: OutputStream::Stdout,
        start: 0,
        source_len,
        total: 5,
        state,
        digest: if source_len == 0 { [0; 32] } else { [9; 32] },
    }
}
/// The body that code `op` with `outcome` takes for job `job` (JR12).
fn body_for(op: ToolOperation, outcome: JobResultOutcome, job: Id) -> JobResultBody {
    use JobResultOutcome as O;
    use ToolOperation as T;
    match (op, outcome) {
        (T::ExecCmd, O::Ok | O::Cancelled) | (T::JobStatus | T::JobCancel, O::Ok) => {
            JobResultBody::State(snapshot(JobRunState::Running))
        }
        (T::JobList, O::Ok) => JobResultBody::List {
            more: false,
            entries: vec![JobListEntry {
                job,
                state: JobRunState::Running,
                numbers: [0; 3],
                stdout_bytes: 5,
                stderr_bytes: 0,
            }],
        },
        (T::JobRead, O::Ok) => JobResultBody::Page(page(JobPageState::Live, 5)),
        (T::JobWait, O::Ok) => {
            JobResultBody::Wait(snapshot(JobRunState::Running), WaitEnd::NewOutput)
        }
        _ => JobResultBody::None,
    }
}
/// Field 6 for code `op` and `outcome` (zero for codes 10 and 11 and with
/// `not_found`).
fn field6(op: ToolOperation, outcome: JobResultOutcome) -> Id {
    if matches!(op, ToolOperation::ExecCmd | ToolOperation::JobList)
        || outcome == JobResultOutcome::NotFound
    {
        [0; 16]
    } else {
        job(1)
    }
}
/// JR11 and JR12: every outcome of every job code, with the body the code
/// allows, replays exactly when the outcome table allows it.
#[test]
fn j3t_03_jr12_outcome_table_and_body_per_operation_code() {
    use JobResultOutcome as O;
    let codes = [
        ToolOperation::ExecCmd,
        ToolOperation::JobList,
        ToolOperation::JobStatus,
        ToolOperation::JobRead,
        ToolOperation::JobWait,
        ToolOperation::JobCancel,
        ToolOperation::JobNote,
    ];
    let mut accepted = 0;
    for op in codes {
        for outcome in O::ALL {
            let mut records = if op == ToolOperation::ExecCmd {
                vec![run_exec(1), r26(1)]
            } else {
                with(live_job(), [job14(2, op)])
            };
            let ordinal = records
                .iter()
                .filter(|r| matches!(r, Record::ToolCall(_)))
                .count() as u32;
            if op == ToolOperation::JobNote && outcome == O::Ok {
                records.push(model_note(2, job(1)));
            }
            records.push(result29(
                ordinal,
                outcome,
                field6(op, outcome),
                body_for(op, outcome, job(1)),
            ));
            let result = inspect(&started_with(records).bytes);
            if outcome.allows(op) {
                assert!(result.is_ok(), "{op:?} {outcome:?}: {result:?}");
                accepted += 1;
            } else {
                assert_eq!(
                    result.unwrap_err(),
                    ReplayError::Corrupt,
                    "{op:?} {outcome:?}"
                );
            }
        }
    }
    // ok 7, not_found 5, not_permitted 1, busy 1, cancelled 6,
    // unavailable 6, limit 1.
    assert_eq!(accepted, 7 + 5 + 1 + 1 + 6 + 6 + 1);
    // An allowed outcome with another code's body is Corrupt.
    for (op, body) in [
        (
            ToolOperation::JobStatus,
            body_for(ToolOperation::JobRead, O::Ok, job(1)),
        ),
        (
            ToolOperation::JobRead,
            body_for(ToolOperation::JobStatus, O::Ok, job(1)),
        ),
        (
            ToolOperation::JobWait,
            body_for(ToolOperation::JobStatus, O::Ok, job(1)),
        ),
        (ToolOperation::JobCancel, JobResultBody::None),
        (
            ToolOperation::JobList,
            body_for(ToolOperation::JobStatus, O::Ok, job(1)),
        ),
        (
            ToolOperation::JobStatus,
            body_for(ToolOperation::JobStatus, O::Ok, job(1)),
        ),
    ] {
        let job = if op == ToolOperation::JobList {
            [0; 16]
        } else {
            job(1)
        };
        let ok = op == ToolOperation::JobStatus && matches!(body, JobResultBody::State(_));
        let records = with(live_job(), [job14(2, op), result29(2, O::Ok, job, body)]);
        assert_eq!(inspect(&started_with(records).bytes).is_ok(), ok, "{op:?}");
    }
    // Field 6: zero for code 11; a job for codes 12 to 16 unless not_found.
    for (op, outcome, job) in [
        (ToolOperation::JobList, O::Unavailable, job(1)),
        (ToolOperation::JobStatus, O::Ok, [0; 16]),
        (ToolOperation::JobCancel, O::Ok, [0; 16]),
        (ToolOperation::JobRead, O::Busy, [0; 16]),
    ] {
        let records = with(
            live_job(),
            [
                job14(2, op),
                result29(2, outcome, job, body_for(op, outcome, job)),
            ],
        );
        assert_eq!(
            inspect(&started_with(records).bytes).unwrap_err(),
            ReplayError::Corrupt,
            "{op:?}"
        );
    }
    // A cancelled turn during `wait`, `read` or `list` (J3 failure table).
    for op in [
        ToolOperation::JobWait,
        ToolOperation::JobRead,
        ToolOperation::JobList,
    ] {
        let job = if op == ToolOperation::JobList {
            [0; 16]
        } else {
            job(1)
        };
        let records = with(
            live_job(),
            [
                job14(2, op),
                cancel_requested(),
                result29(2, O::Cancelled, job, JobResultBody::None),
                Record::TurnTerminal(terminal(TerminalKind::Cancelled, Cause::UserCancel, false)),
            ],
        );
        assert!(inspect(&started_with(records).bytes).is_ok(), "{op:?}");
    }
}
/// JR14: state 9 needs the job's kind 28 earlier; states 1 to 8 need none.
#[test]
fn j3t_03_jr14_settled_state_needs_the_earlier_kind_28() {
    use JobResultOutcome as O;
    let state = |s: JobRunState| JobResultBody::State(snapshot(s));
    expect_rule(
        "code 10 settled",
        started_with([
            run_exec(1),
            r26(1),
            r28(1),
            result29(1, O::Ok, [0; 16], state(JobRunState::Settled)),
        ]),
        started_with([
            run_exec(1),
            r26(1),
            result29(1, O::Ok, [0; 16], state(JobRunState::Settled)),
        ]),
    );
    // A snapshot taken before kind 28 may commit after it.
    assert!(
        inspect(
            &started_with([
                run_exec(1),
                r26(1),
                r28(1),
                result29(1, O::Ok, [0; 16], state(JobRunState::Finalizing)),
            ])
            .bytes
        )
        .is_ok()
    );
    for running in [
        JobRunState::Queued(JobBlockReason::AgentSlots),
        JobRunState::Starting,
        JobRunState::CapturePaused(JobPauseReason::DiskSpace),
        JobRunState::CleanupUnconfirmed,
    ] {
        assert!(
            inspect(
                &started_with([
                    run_exec(1),
                    r26(1),
                    result29(1, O::Ok, [0; 16], state(running)),
                ])
                .bytes
            )
            .is_ok(),
            "{running:?}"
        );
    }
    let wait = |end| JobResultBody::Wait(snapshot(JobRunState::Settled), end);
    for end in [WaitEnd::Settled, WaitEnd::AlreadySettled] {
        expect_rule(
            "wait settled",
            started_with(with(
                settled_job(),
                [
                    job14(2, ToolOperation::JobWait),
                    result29(2, O::Ok, job(1), wait(end)),
                ],
            )),
            started_with(with(
                live_job(),
                [
                    job14(2, ToolOperation::JobWait),
                    result29(2, O::Ok, job(1), wait(end)),
                ],
            )),
        );
    }
    let list = |s: JobRunState| JobResultBody::List {
        more: false,
        entries: vec![JobListEntry {
            job: job(1),
            state: s,
            numbers: [0; 3],
            stdout_bytes: 0,
            stderr_bytes: 0,
        }],
    };
    expect_rule(
        "list entry settled",
        started_with(with(
            settled_job(),
            [
                job14(2, ToolOperation::JobList),
                result29(2, O::Ok, [0; 16], list(JobRunState::Settled)),
            ],
        )),
        started_with(with(
            live_job(),
            [
                job14(2, ToolOperation::JobList),
                result29(2, O::Ok, [0; 16], list(JobRunState::Settled)),
            ],
        )),
    );
}
/// JR15: a job in field 6 or a list entry has an earlier kind 26 in the
/// operation's conversation; list entries are newest first.
#[test]
fn j3t_03_jr15_jobs_are_earlier_and_of_the_conversation() {
    use JobResultOutcome as O;
    let status = |job: Id| {
        result29(
            2,
            O::Ok,
            job,
            body_for(ToolOperation::JobStatus, O::Ok, job),
        )
    };
    expect_rule(
        "unknown job",
        started_with(with(
            live_job(),
            [job14(2, ToolOperation::JobStatus), status(job(1))],
        )),
        started_with(with(
            live_job(),
            [job14(2, ToolOperation::JobStatus), status(id(99))],
        )),
    );
    // A job of another conversation: the first conversation's job, named
    // from the second conversation's turn.
    let [accepted, started] = second_turn();
    let mut other = started_with(with(live_job(), [complete(), accepted, started]));
    let second = |ordinal, op| job14_at(id(43), ordinal, op);
    other.append(second(1, ToolOperation::JobStatus));
    other.append(result29_at(
        id(43),
        1,
        O::NotFound,
        [0; 16],
        JobResultBody::None,
    ));
    assert!(inspect(&other.bytes).is_ok());
    other.append(second(2, ToolOperation::JobStatus));
    other.append(result29_at(
        id(43),
        2,
        O::Ok,
        job(1),
        body_for(ToolOperation::JobStatus, O::Ok, job(1)),
    ));
    assert_eq!(inspect(&other.bytes).unwrap_err(), ReplayError::Corrupt);
    // A list names jobs newest first by kind 26 sequence.
    let entry = |ordinal| JobListEntry {
        job: job(ordinal),
        state: JobRunState::Running,
        numbers: [0; 3],
        stdout_bytes: 1,
        stderr_bytes: 0,
    };
    let two = vec![
        run_exec(1),
        r26(1),
        r29(1, 1),
        run_exec(2),
        r26(2),
        r29(2, 1),
    ];
    let list = |entries| JobResultBody::List {
        more: false,
        entries,
    };
    expect_rule(
        "list order",
        started_with(with(
            two.clone(),
            [
                job14(3, ToolOperation::JobList),
                result29(3, O::Ok, [0; 16], list(vec![entry(2), entry(1)])),
            ],
        )),
        started_with(with(
            two,
            [
                job14(3, ToolOperation::JobList),
                result29(3, O::Ok, [0; 16], list(vec![entry(1), entry(2)])),
            ],
        )),
    );
}
/// JR16: a `note` `ok` needs the earlier kind 30 model note that cites the
/// same call, for the same job.
#[test]
fn j3t_03_jr16_note_ok_follows_its_model_note() {
    use JobResultOutcome as O;
    let note_ok = |job: Id| result29(2, O::Ok, job, JobResultBody::None);
    let base = || with(live_job(), [job14(2, ToolOperation::JobNote)]);
    expect_rule(
        "note ok without kind 30",
        started_with(with(base(), [model_note(2, job(1)), note_ok(job(1))])),
        started_with(with(base(), [note_ok(job(1))])),
    );
    // The `limit` outcome writes no note.
    assert!(
        inspect(
            &started_with(with(
                base(),
                [result29(2, O::Limit, job(1), JobResultBody::None)]
            ))
            .bytes
        )
        .is_ok()
    );
    // A note for another job than field 6.
    let two = with(
        vec![
            run_exec(1),
            r26(1),
            r29(1, 1),
            run_exec(2),
            r26(2),
            r29(2, 1),
        ],
        [job14(3, ToolOperation::JobNote), model_note(3, job(2))],
    );
    expect_rule(
        "note of another job",
        started_with(with(
            two.clone(),
            [result29(3, O::Ok, job(2), JobResultBody::None)],
        )),
        started_with(with(two, [result29(3, O::Ok, job(1), JobResultBody::None)])),
    );
}
/// JR17: a kind 23 frame is UnsupportedFormat, as any retired kind.
#[test]
fn j3t_03_jr17_kind_23_frame_is_unsupported_format() {
    let mut journal = started_with(settled_job());
    let range = last_frame(&journal);
    let frame = &mut journal.bytes[range];
    frame[10..12].copy_from_slice(&23u16.to_be_bytes());
    seal(frame);
    assert_eq!(
        inspect(&journal.bytes).unwrap_err(),
        ReplayError::UnsupportedFormat
    );
}
/// J4T-02 (unit): kind 30 replay rules. Pin only when unpinned; unpin
/// only when pinned; delete once after kind 28; notes always; the named
/// job exists; a model note cites its pending call.
#[test]
fn j4t_02_kind_30_rules_and_retention_summary() {
    use RetentionAction::*;
    let summary = |records: Vec<Record>, job: Id| {
        let journal = started_with(records);
        let state = inspect(&journal.bytes).unwrap();
        let mut index = MemoryIndex {
            segments: std::collections::BTreeMap::from([(1, &journal.bytes[..])]),
        };
        state.job_retention_summary(job, &mut index).unwrap()
    };
    let rejected = |records: Vec<Record>, name: &str| {
        assert_eq!(
            inspect(&started_with(records).bytes).unwrap_err(),
            ReplayError::Corrupt,
            "{name}"
        );
    };
    // A live job: pin, unpin, pin and notes; delete waits for kind 28.
    let live = with(
        live_job(),
        [
            person(job(1), Pin),
            person(job(1), Unpin),
            person(job(1), Pin),
            person(job(1), Note),
        ],
    );
    assert_eq!(
        summary(live.clone(), job(1)),
        JobRetentionSummary {
            pinned: true,
            deleted: false,
            notes: 1,
            records: 4,
        }
    );
    rejected(with(live.clone(), [person(job(1), Pin)]), "pin when pinned");
    rejected(
        with(live_job(), [person(job(1), Unpin)]),
        "unpin when unpinned",
    );
    rejected(
        with(live, [person(job(1), Delete)]),
        "delete before kind 28",
    );
    // A settled job: delete once; a note after delete is allowed.
    let deleted = with(
        settled_job(),
        [person(job(1), Delete), person(job(1), Note)],
    );
    assert_eq!(
        summary(deleted.clone(), job(1)),
        JobRetentionSummary {
            pinned: false,
            deleted: true,
            notes: 1,
            records: 2,
        }
    );
    rejected(with(deleted, [person(job(1), Delete)]), "delete twice");
    rejected(with(live_job(), [person(id(99), Note)]), "unknown job");
    // Person records are independent of any turn: after the terminal, and
    // after a restart while the old operation is still active.
    assert!(
        inspect(&started_with(with(settled_job(), [complete(), person(job(1), Pin)])).bytes)
            .is_ok()
    );
    assert!(
        inspect(
            &started_with(with(
                live_job(),
                [Record::OwnerGeneration, person(job(1), Pin)]
            ))
            .bytes
        )
        .is_ok()
    );
    // Model notes: the pending `job note` call, once, of this owner
    // generation, for a job of its conversation.
    let pending = with(live_job(), [job14(2, ToolOperation::JobNote)]);
    assert!(inspect(&started_with(with(pending.clone(), [model_note(2, job(1))])).bytes).is_ok());
    rejected(with(live_job(), [model_note(1, job(1))]), "no note call");
    rejected(
        with(
            live_job(),
            [job14(2, ToolOperation::JobStatus), model_note(2, job(1))],
        ),
        "a status call",
    );
    rejected(
        with(
            pending.clone(),
            [model_note(2, job(1)), model_note(2, job(1))],
        ),
        "two notes",
    );
    rejected(
        with(pending.clone(), [model_note(3, job(1))]),
        "wrong ordinal",
    );
    rejected(
        with(
            pending.clone(),
            [
                result29(2, JobResultOutcome::Limit, job(1), JobResultBody::None),
                model_note(2, job(1)),
            ],
        ),
        "note after the call's kind 29",
    );
    rejected(
        with(pending, [Record::OwnerGeneration, model_note(2, job(1))]),
        "note after restart",
    );
    // A note for a job of another conversation.
    let [accepted, started] = second_turn();
    rejected(
        with(
            live_job(),
            [
                complete(),
                accepted,
                started,
                job14_at(id(43), 1, ToolOperation::JobNote),
                model_note_at(id(43), 1, job(1)),
            ],
        ),
        "another conversation's job",
    );
}
/// J4T-03 (unit): the roll entries of a settled job and its retention, and
/// the listing reads merged from the live state and the delta.
#[test]
fn j4t_03_listing_entries_and_reads_over_live_state_and_delta() {
    use RetentionAction::*;
    let [accepted, started] = second_turn();
    // Conversation 8: job 1 settled, job 2 live; conversation 41: one job.
    let journal = started_with(vec![
        run_exec(1),
        r26(1),
        r29(1, 1),
        run_exec(2),
        r26(2),
        r29(2, 1),
        r28(1),
        person(job(1), Pin),
        person(job(1), Note),
        complete(),
        accepted,
        started,
        r14(ToolCall {
            operation: id(43),
            ..run14(1, ToolOperation::ExecCmd)
        }),
        r26_for(model_origin(id(43), 1, 1)),
        result29_at(
            id(43),
            1,
            JobResultOutcome::Ok,
            [0; 16],
            JobResultBody::State(snapshot(JobRunState::Running)),
        ),
    ]);
    let state = inspect(&journal.bytes).unwrap();
    let mut index = MemoryIndex {
        segments: std::collections::BTreeMap::from([(1, &journal.bytes[..])]),
    };
    let other = job_id(&model_origin(id(43), 1, 1));
    // The roll entries of job 1: `W`, `X`, `L`, `M`, `Y`, `U`; and two `E`.
    let entries = state.delta_entries();
    let tags = |tag| entries.iter().filter(|(k, _)| k.tag == tag).count();
    assert_eq!(tags(Tag::ConversationJob), 1);
    assert_eq!(tags(Tag::ProjectJob), 1);
    assert_eq!(tags(Tag::AllJobs), 1);
    assert_eq!(tags(Tag::Settlement), 1);
    assert_eq!(tags(Tag::Retention), 2);
    let settled = state.job_listing(job(1), &mut index).unwrap().unwrap();
    let intent_sequence = settled.intent_frame.sequence;
    let settled_sequence = settled.settled_frame.sequence;
    let key = |tag, id, disc| Key::new(tag, id, disc);
    assert_eq!(
        state
            .entry(
                &key(Tag::ConversationJob, id(8), intent_sequence),
                &mut index
            )
            .unwrap(),
        Some(Entry::ConversationJob { job: job(1) })
    );
    assert_eq!(
        state
            .entry(&key(Tag::ProjectJob, id(6), intent_sequence), &mut index)
            .unwrap(),
        Some(Entry::ProjectJob {
            job: job(1),
            conversation: id(8),
        })
    );
    assert_eq!(
        state
            .entry(&key(Tag::AllJobs, [0; 16], intent_sequence), &mut index)
            .unwrap(),
        Some(Entry::AllJobs {
            job: job(1),
            project: id(6),
        })
    );
    assert_eq!(
        state
            .entry(&key(Tag::Settlement, id(8), settled_sequence), &mut index)
            .unwrap(),
        Some(Entry::Settlement { job: job(1) })
    );
    // The listing facts of the settled job: its records, command source and
    // retention summary.
    assert_eq!((settled.conversation, settled.project), (id(8), id(6)));
    assert!(settled.settled.is_some() && settled.cancel.is_none());
    assert_eq!(
        settled.command,
        state.operations[&id(10)].tools[0].intent_frame
    );
    assert_eq!(
        settled.retention,
        JobRetentionSummary {
            pinned: true,
            deleted: false,
            notes: 1,
            records: 2,
        }
    );
    // Conversation scope: newest first, live merged with the delta.
    let jobs = |scope, before, limit, index: &mut MemoryIndex<'_>| {
        state
            .jobs(scope, before, limit, index)
            .unwrap()
            .into_iter()
            .map(|l| l.job)
            .collect::<Vec<_>>()
    };
    assert_eq!(
        jobs(JobScope::Conversation(id(8)), None, 9, &mut index),
        vec![job(2), job(1)]
    );
    assert_eq!(
        jobs(JobScope::Conversation(id(41)), None, 9, &mut index),
        vec![other]
    );
    assert_eq!(
        jobs(JobScope::Project(id(6)), None, 9, &mut index),
        vec![other, job(2), job(1)]
    );
    assert_eq!(
        jobs(JobScope::AllProjects, None, 9, &mut index),
        vec![other, job(2), job(1)]
    );
    // Paging by the kind 26 sequence of the last entry shown (J3-E5).
    let first = state
        .jobs(JobScope::AllProjects, None, 2, &mut index)
        .unwrap();
    assert_eq!(first.len(), 2);
    let before = first[1].intent_frame.sequence;
    assert_eq!(
        jobs(JobScope::AllProjects, Some(before), 2, &mut index),
        vec![job(1)]
    );
    assert!(jobs(JobScope::Project(id(99)), None, 9, &mut index).is_empty());
    // `JobFacts`: the kind 14 and the kind 30 records in order.
    let records = state.job_records(job(1), &mut index).unwrap().unwrap();
    assert_eq!(records.command.tool_operation, Some(ToolOperation::ExecCmd));
    assert_eq!(
        records
            .retention
            .iter()
            .map(|(_, r)| r.action)
            .collect::<Vec<_>>(),
        vec![Pin, Note]
    );
    assert!(state.job_records(id(99), &mut index).unwrap().is_none());
    // `Settlements` (J5): the range after < sequence <= through.
    let settlements = |after, through, index: &mut MemoryIndex<'_>| {
        state
            .job_settlements(id(8), after, through, 9, index)
            .unwrap()
            .len()
    };
    assert_eq!(settlements(0, u64::MAX - 1, &mut index), 1);
    assert_eq!(
        settlements(settled_sequence - 1, settled_sequence, &mut index),
        1
    );
    assert_eq!(settlements(settled_sequence, u64::MAX - 1, &mut index), 0);
    assert_eq!(settlements(0, settled_sequence - 1, &mut index), 0);
}
/// Reason 20 belongs to a person `!!` job; a model job never records it.
#[test]
fn j3t_03_reason_20_needs_a_person_job_and_reason_19_settles_model_jobs() {
    let not_started = |reason| {
        Record::JobSettled(Box::new(JobSettled {
            outcome: JobOutcome::NotStarted,
            reason,
            exit_kind: 0,
            exit_value: 0,
            cleanup: JobCleanup::None,
            elapsed_ms: 0,
            capture_state: JobCaptureState::NotStarted,
            stdout: JobStream::default(),
            stderr: JobStream::default(),
            final_reservation: 0,
            ..settled28(job(1))
        }))
    };
    expect_rule(
        "reason 20 on a model job",
        started_with(with(
            live_job(),
            [
                r27(1, JobReason::ExecutionOff),
                not_started(JobReason::ExecutionOff),
            ],
        )),
        started_with(with(
            live_job(),
            [not_started(JobReason::OutsideSandboxOff)],
        )),
    );
}
