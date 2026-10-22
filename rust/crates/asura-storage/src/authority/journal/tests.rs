//! Segmented journal driver cases on in-memory files (unit layer of the
//! segmented-journal validation plan). The same roll, checkpoint, run,
//! merge, quota and replay code runs on real files in the writer tests.
use super::memory::MemoryFiles;
use super::*;
use crate::authority::checkpoint;
use crate::authority::index::{Entry, Key, Tag};
use std::collections::BTreeMap;

fn id(n: u64) -> Id {
    let mut out = [0x5a; 16];
    out[8..].copy_from_slice(&n.to_be_bytes());
    out
}

/// A journal driven through the real append path.
struct Harness {
    journal: Journal<MemoryFiles>,
    next: u64,
    completed: BTreeMap<Id, (u64, u64)>,
}
impl Harness {
    fn new() -> Self {
        let context = FrameContext {
            installation_id: id(1),
            transition_id: id(2),
            sequence: 1,
            prior_digest: [0; 32],
            expected_revision: 0,
            owner_generation: 1,
        };
        let init = PendingInit {
            request: id(3),
            digest: request_digest(Request::Initialize { mode: 1 }).unwrap(),
            mode: 1,
            configuration_revision: 1,
            configuration_digest: [4; 32],
            graph: id(5),
        };
        let journal = Journal::initialize(MemoryFiles::new(), &context, init).unwrap();
        let mut h = Self {
            journal,
            next: 100,
            completed: BTreeMap::new(),
        };
        h.append(Record::ActiveBinding(ActiveBinding {
            request: id(3),
            generation: 1,
            graph: id(5),
            configuration_digest: [4; 32],
        }))
        .unwrap();
        h
    }
    fn fresh(&mut self) -> Id {
        self.next += 1;
        id(self.next)
    }
    fn append(&mut self, record: Record) -> Result<(), Failure> {
        let transition = self.fresh();
        self.journal.append(record, transition)
    }
    fn state(&self) -> &Replay {
        self.journal.state()
    }
    fn register(&mut self, n: u64) -> Id {
        let location = format!("/project/{n}");
        let project = self.fresh();
        let request = self.fresh();
        self.append(Record::ProjectRegistered(ProjectRegistered {
            request,
            digest: request_digest(Request::Register {
                location: &location,
            })
            .unwrap(),
            project,
            location,
            device: 7,
            inode: 1000 + n,
            registry_revision: 1,
            visibility: 1,
        }))
        .unwrap();
        project
    }
    /// Accepted and started: returns (conversation, operation, generation).
    fn start(&mut self, project: Id, conversation: Option<Id>) -> (Id, Id, u64) {
        let target = conversation.unwrap_or_else(|| self.fresh());
        let expected = conversation.map_or(0, |c| self.completed.get(&c).map_or(0, |v| v.1));
        let (operation, task, request) = (self.fresh(), self.fresh(), self.fresh());
        let completed = self.completed.get(&target).map_or(0, |v| v.0);
        self.append(Record::TurnAccepted(Box::new(TurnAccepted {
            request,
            digest: request_digest(Request::Submit {
                project,
                conversation,
                expected_generation: expected,
                prompt: "p",
            })
            .unwrap(),
            project,
            original_conversation: conversation,
            expected_generation: expected,
            conversation: target,
            generation: expected + 1,
            task,
            operation,
            model: "m".into(),
            configuration_digest: [4; 32],
            instructions_digest: [1; 32],
            input_digest: [2; 32],
            prior_operations: vec![],
            prompt: "p".into(),
            event_cursor: 1,
            composition: Composition {
                omitted_turns: completed as u32,
                ..Composition::default()
            },
            thinking_level: 0,
            thinking_request: 0,
        })))
        .unwrap();
        let helper = self.fresh();
        self.append(Record::StartAuthorized(StartAuthorized {
            operation,
            generation: expected + 1,
            helper,
            input_digest: [2; 32],
        }))
        .unwrap();
        (target, operation, expected + 1)
    }
    fn segment(
        &mut self,
        operation: Id,
        generation: u64,
        offset: u64,
        text: &str,
    ) -> Result<(), Failure> {
        self.append(Record::ResponseText(ResponseText {
            operation,
            generation,
            offset,
            text: text.into(),
        }))
    }
    fn finish(&mut self, conversation: Id, operation: Id, generation: u64, length: u64) {
        self.append(Record::TurnTerminal(TurnTerminal {
            operation,
            generation,
            kind: TerminalKind::Complete,
            cause: Cause::None,
            final_cursor: u64::MAX,
            usage_known: true,
            output_tokens: 1,
            text_offset: length,
            text: "!".into(),
        }))
        .unwrap();
        let entry = self.completed.entry(conversation).or_default();
        entry.0 += 1;
        entry.1 = generation;
    }
    /// One complete turn with `pieces` kind 24 records of `size` bytes.
    fn turn(
        &mut self,
        project: Id,
        conversation: Option<Id>,
        pieces: u64,
        size: usize,
    ) -> (Id, Id) {
        let (conversation, operation, generation) = self.start(project, conversation);
        let text = "r".repeat(size);
        for n in 0..pieces {
            self.segment(operation, generation, n * size as u64, &text)
                .unwrap();
        }
        self.finish(conversation, operation, generation, pieces * size as u64);
        (conversation, operation)
    }
    fn files(&mut self) -> MemoryFiles {
        self.journal.files_mut().clone()
    }
}
fn open(files: MemoryFiles, writer: bool) -> Result<Journal<MemoryFiles>, Failure> {
    match Journal::open(files, writer, &AtomicBool::new(false))? {
        Opened::Journal(journal) => Ok(*journal),
        Opened::Empty(_) => panic!("journal expected"),
    }
}
fn segments(files: &MemoryFiles) -> Vec<(u64, Vec<u8>)> {
    files
        .files
        .iter()
        .filter_map(|(name, bytes)| match name {
            JournalName::Segment(first) => Some((*first, bytes.clone())),
            _ => None,
        })
        .collect()
}
fn names(files: &MemoryFiles, pick: fn(&JournalName) -> bool) -> Vec<JournalName> {
    files.names().into_iter().filter(pick).collect()
}
/// Frames of every segment file in one stream, replayed from genesis.
fn genesis(files: &MemoryFiles) -> Replay {
    let segments = segments(files);
    let map: BTreeMap<u64, &[u8]> = segments.iter().map(|(f, b)| (*f, b.as_slice())).collect();
    let mut index = MemoryIndex { segments: map };
    let mut state = None;
    for (first, bytes) in &segments {
        let mut offset = 0;
        while offset < bytes.len() {
            let envelope = crate::authority::envelope(&bytes[offset..]).unwrap();
            let locator = Locator {
                sequence: envelope.context.sequence,
                segment: *first,
                offset: offset as u32,
                length: envelope.length as u32,
            };
            let record = decode_record(envelope.kind, envelope.payload).unwrap();
            state = Some(
                apply_next(
                    state,
                    &envelope.context,
                    record,
                    envelope.digest,
                    locator,
                    &mut index,
                )
                .unwrap(),
            );
            offset += envelope.length;
        }
    }
    state.unwrap()
}
/// The canonical encoding of the live state (a checkpoint body).
fn canonical(state: &Replay) -> Vec<u8> {
    let mut state = state.clone();
    state.clear_delta();
    let census = checkpoint::Census {
        segments: 0,
        holding: 0,
        bytes: 0,
    };
    checkpoint::encode(&state, &census, &[]).unwrap()
}
/// A journal of about 20 MiB: three or more segment files and rolls.
fn long_journal() -> (Harness, Vec<(Id, Id)>) {
    let mut h = Harness::new();
    let project = h.register(1);
    let mut turns = Vec::new();
    for _ in 0..12 {
        turns.push(h.turn(project, None, 30, 60 * 1024));
    }
    (h, turns)
}

/// SJ-02: segment files end on frame boundaries, within 8 MiB and 32,768
/// frames; a frame is never split; the delta charge rolls a segment.
#[test]
fn sj02_rolls_on_bytes_frames_and_delta_charge_without_splitting_frames() {
    let (mut h, _) = long_journal();
    let files = h.files();
    let all = segments(&files);
    assert!(all.len() >= 3, "{} segments", all.len());
    let mut sequence = 0;
    for (first, bytes) in &all {
        assert!(bytes.len() <= SEGMENT_BYTES);
        assert_eq!(*first, sequence + 1, "file name equals its first sequence");
        let mut offset = 0;
        let mut frames = 0;
        while offset < bytes.len() {
            let envelope = crate::authority::envelope(&bytes[offset..]).expect("whole frame");
            sequence = envelope.context.sequence;
            offset += envelope.length;
            frames += 1;
        }
        assert_eq!(offset, bytes.len(), "ends on a frame boundary");
        assert!(frames <= SEGMENT_FRAMES);
    }
    // Frame count: small records roll at 32,768 frames per file.
    let mut h = Harness::new();
    for _ in 0..33_000 {
        h.append(Record::OwnerGeneration).unwrap();
    }
    let files = h.files();
    for (_, bytes) in segments(&files) {
        let mut offset = 0;
        let mut frames = 0;
        while offset < bytes.len() {
            offset += crate::authority::envelope(&bytes[offset..]).unwrap().length;
            frames += 1;
        }
        assert!(frames <= SEGMENT_FRAMES);
    }
    assert!(segments(&files).len() >= 2);
    assert!(h.state().delta_charge() <= DELTA_CHARGE + 64 * 1024);
    // Delta charge: registrations add index entries and roll before 8 MiB.
    let mut h = Harness::new();
    for n in 0..9_000 {
        h.register(n);
        assert!(h.state().delta_charge() <= DELTA_CHARGE + 4096);
    }
    let files = h.files();
    assert!(segments(&files).len() >= 2);
    assert!(segments(&files)[0].1.len() < SEGMENT_BYTES);
}

/// SJ-03: the hash chain continues across files; a swapped file is rejected.
#[test]
fn sj03_chain_across_files_and_swapped_file_is_corrupt() {
    let (mut h, _) = long_journal();
    let files = h.files();
    let reopened = open(files.clone(), true).unwrap();
    assert_eq!(canonical(reopened.state()), canonical(h.state()));
    let segments = segments(&files);
    let mut swapped = files.clone();
    let (a, b) = (segments[1].0, segments[2].0);
    let bytes_a = swapped.files[&JournalName::Segment(a)].clone();
    let bytes_b = swapped.files[&JournalName::Segment(b)].clone();
    swapped.files.insert(JournalName::Segment(a), bytes_b);
    swapped.files.insert(JournalName::Segment(b), bytes_a);
    // Remove derived files so replay must read the swapped frames.
    swapped.files.retain(|name, _| !name.is_derived());
    assert!(open(swapped, true).is_err());
}

/// SJ-04 (unit): a crash at each roll step leaves files from which the
/// writer recovers; the next appends repeat the roll and the state equals
/// the uninterrupted journal.
#[test]
fn sj04_crash_at_each_roll_step_recovers_and_repeats_the_roll() {
    use asura_platform::JournalPoint as P;
    for point in [
        P::RunWrite,
        P::RunRenamed,
        P::CheckpointSync,
        P::CheckpointRenamed,
        P::SegmentCreate,
        P::SegmentCreated,
    ] {
        let mut h = Harness::new();
        let project = h.register(1);
        let (conversation, operation, generation) = h.start(project, None);
        let text = "x".repeat(60 * 1024);
        let mut offset = 0;
        // Fill the first segment until the next frame needs a roll.
        h.journal.files_mut().crash_at = Some(point);
        let failure = loop {
            match h.segment(operation, generation, offset, &text) {
                Ok(()) => offset += text.len() as u64,
                Err(failure) => break failure,
            }
        };
        assert_eq!(failure, Failure::Cancelled, "{point:?}");
        let mut files = h.files();
        files.crash_at = None;
        // Restart: tmp and unlisted derived files are removed; replay works.
        let mut journal = open(files, true).unwrap();
        assert!(
            journal.files_mut().names().iter().all(|n| !n.is_tmp()),
            "{point:?}"
        );
        let mut h2 = Harness {
            journal,
            next: 1_000_000,
            completed: BTreeMap::new(),
        };
        // The crashed frame was not written; the turn continues.
        for _ in 0..3 {
            h2.segment(operation, generation, offset, &text).unwrap();
            offset += text.len() as u64;
        }
        h2.finish(conversation, operation, generation, offset);
        let files = h2.files();
        assert!(segments(&files).len() >= 2, "{point:?}");
        assert_eq!(
            canonical(&genesis(&files)),
            canonical(h2.state()),
            "{point:?}"
        );
        let reopened = open(files, false).unwrap();
        assert_eq!(
            canonical(reopened.state()),
            canonical(h2.state()),
            "{point:?}"
        );
    }
}

/// SJ-05, SJ-40: the checkpoint plus tail equals genesis replay, byte for
/// byte in canonical encoding, including the recent sets, for generated
/// journals.
#[test]
fn sj05_sj40_checkpoint_plus_tail_equals_genesis_replay() {
    for seed in 1..=4u64 {
        let mut h = Harness::new();
        let mut projects = Vec::new();
        let mut conversations: Vec<(Id, Id)> = Vec::new();
        let mut rng = seed;
        let mut step = || {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            rng
        };
        for n in 0..90 {
            projects.push(h.register(seed * 1000 + n));
        }
        for _ in 0..600 {
            let choice = step() % 10;
            let project = projects[(step() % projects.len() as u64) as usize];
            if choice < 3 && !conversations.is_empty() {
                let (project, conversation) =
                    conversations[(step() % conversations.len() as u64) as usize];
                h.turn(project, Some(conversation), step() % 3, 1000);
            } else if choice < 8 {
                let (conversation, _) = h.turn(project, None, step() % 3, 61_440);
                conversations.push((project, conversation));
            } else {
                h.append(Record::OwnerGeneration).unwrap();
            }
        }
        let files = h.files();
        assert!(segments(&files).len() >= 2, "seed {seed}");
        let replayed = genesis(&files);
        assert_eq!(canonical(&replayed), canonical(h.state()), "seed {seed}");
        let reopened = open(files, false).unwrap();
        assert_eq!(
            canonical(reopened.state()),
            canonical(&replayed),
            "seed {seed}"
        );
        assert!(replayed.projects.len() <= RECENT_PROJECTS + MAX_INPUTS + 1);
        assert!(replayed.conversations.len() <= RECENT_CONVERSATIONS + MAX_INPUTS + 1);
    }
}

/// SJ-06: a checkpoint with an extra field is Corrupt; derived values are
/// recomputed after load.
#[test]
fn sj06_extra_checkpoint_content_is_corrupt_and_derived_values_recompute() {
    let (mut h, _) = long_journal();
    let files = h.files();
    let newest = names(&files, |n| matches!(n, JournalName::Checkpoint(_)))
        .last()
        .copied()
        .unwrap();
    let bytes = files.files[&newest].clone();
    let mut extra = bytes[..bytes.len() - 40].to_vec();
    extra.push(0);
    let digest: Hash = sha2::Sha256::digest(&extra).into();
    extra.extend_from_slice(&digest);
    extra.extend_from_slice(b"ASURAC01");
    let mut store = MemoryIndex {
        segments: BTreeMap::new(),
    };
    assert_eq!(
        checkpoint::decode(&extra, &mut store).unwrap_err(),
        ReplayError::Corrupt
    );
    let reopened = open(files, false).unwrap();
    assert_eq!(reopened.state().reserved_bytes, h.state().reserved_bytes);
    assert_eq!(reopened.state().reserved_frames, h.state().reserved_frames);
}
use sha2::Digest;

/// SJ-07: a damaged newest checkpoint falls back to the previous one; the
/// next roll writes a new checkpoint and removes the damaged one.
#[test]
fn sj07_damaged_newest_checkpoint_falls_back_to_the_previous() {
    let (mut h, _) = long_journal();
    let mut files = h.files();
    let checkpoints = names(&files, |n| matches!(n, JournalName::Checkpoint(_)));
    assert_eq!(checkpoints.len(), 2, "the newest two are kept");
    let newest = checkpoints[1];
    let bytes = files.files.get_mut(&newest).unwrap();
    let middle = bytes.len() / 2;
    bytes[middle] ^= 1;
    let journal = open(files.clone(), true).unwrap();
    assert_eq!(canonical(journal.state()), canonical(h.state()));
    let mut h2 = Harness {
        journal,
        next: 2_000_000,
        completed: h.completed.clone(),
    };
    let project = *h2.state().projects.keys().next().unwrap();
    h2.turn(project, None, 140, 60 * 1024);
    let files = h2.files();
    assert!(
        !files.files.contains_key(&newest),
        "damaged checkpoint removed"
    );
    assert_eq!(canonical(&genesis(&files)), canonical(h2.state()));
}

/// SJ-08, SJ-F09: with no checkpoint the writer rebuilds runs and
/// checkpoints from sequence 1; the read-only scan reports pending.
#[test]
fn sj08_rebuild_without_checkpoints_and_pending_for_read_only_scan() {
    let (mut h, _) = long_journal();
    let original = h.files();
    let mut stripped = original.clone();
    stripped.files.retain(|name, _| !name.is_derived());
    assert!(segments(&stripped).len() > 2);
    assert!(matches!(
        Journal::open(stripped.clone(), false, &AtomicBool::new(false)),
        Err(Failure::RebuildPending)
    ));
    let mut rebuilt = open(stripped, true).unwrap();
    assert_eq!(canonical(rebuilt.state()), canonical(h.state()));
    // The rebuilt derived files equal the ones the rolls wrote.
    let derived = |files: &MemoryFiles| -> BTreeMap<JournalName, Vec<u8>> {
        files
            .files
            .iter()
            .filter(|(n, _)| matches!(n, JournalName::Run(..) | JournalName::Checkpoint(_)))
            .map(|(n, b)| (*n, b.clone()))
            .collect()
    };
    assert_eq!(derived(rebuilt.files_mut()), derived(&original));
}

/// SJ-09 (unit): replay memory stays within 16 MiB and does not grow with
/// the number of segment files.
#[test]
fn sj09_replay_charge_is_independent_of_journal_size() {
    let mut charges = Vec::new();
    for turns in [4u64, 32] {
        let mut h = Harness::new();
        let project = h.register(1);
        for _ in 0..turns {
            h.turn(project, None, 40, 60 * 1024);
        }
        let files = h.files();
        let segments = segments(&files).len();
        let reopened = open(files, false).unwrap();
        let charge = reopened.state().memory_charge();
        assert!(charge <= crate::authority::conversation::MAX_MEMORY);
        charges.push((segments, charge));
    }
    assert!(charges[1].0 >= 3 * charges[0].0, "{charges:?}");
    assert!(
        charges[1].1.abs_diff(charges[0].1) < 2 * 1024 * 1024,
        "{charges:?}"
    );
}

/// SJ-10 (unit): history entries merge the delta and runs newest first.
#[test]
fn sj10_turn_entries_merge_delta_and_runs() {
    let mut h = Harness::new();
    let project = h.register(1);
    let (conversation, _) = h.turn(project, None, 1, 100);
    for _ in 0..30 {
        h.turn(project, Some(conversation), 6, 60 * 1024);
    }
    assert!(!h.journal.runs().is_empty());
    let (state, mut store) = h.journal.store();
    let (lo, hi) = (
        Key::new(Tag::Turn, conversation, 0),
        Key::new(Tag::Turn, conversation, u64::MAX),
    );
    let all = state.range(&lo, &hi, true, 100, &mut store).unwrap();
    assert_eq!(all.len(), 31);
    let generations: Vec<u64> = all.iter().map(|(k, _)| k.disc).collect();
    assert_eq!(generations, (1..=31).rev().collect::<Vec<_>>());
    let Entry::Turn { completed, .. } = Entry::decode(Tag::Turn, &all[0].1).unwrap() else {
        panic!("turn");
    };
    assert_eq!(completed, 31);
    let page = state
        .range(
            &lo,
            &Key::new(Tag::Turn, conversation, 10),
            true,
            3,
            &mut store,
        )
        .unwrap();
    assert_eq!(
        page.iter().map(|(k, _)| k.disc).collect::<Vec<_>>(),
        [9, 8, 7]
    );
}

/// SJ-11: merge steps compact eight runs; lookups are unchanged; inputs
/// are removed once neither kept checkpoint lists them.
#[test]
fn sj11_run_compaction_preserves_lookups_and_retires_inputs() {
    let mut h = Harness::new();
    let project = h.register(1);
    let mut operations = Vec::new();
    while h.journal.runs().len() < 9 {
        operations.push(h.turn(project, None, 50, 60 * 1024).1);
    }
    let before: Vec<_> = operations
        .iter()
        .map(|op| {
            let (state, mut store) = h.journal.store();
            state.operation_facts(*op, &mut store).unwrap()
        })
        .collect();
    let mut steps = 0;
    while h.journal.merge_pending() {
        steps += 1;
        if !h.journal.merge_step().unwrap() {
            break;
        }
    }
    assert!(steps >= 1);
    // Two rolls make the merged run listed, then the previous checkpoint too.
    for _ in 0..2 {
        operations.push(h.turn(project, None, 140, 60 * 1024).1);
    }
    assert!(
        h.journal.runs().iter().any(|r| r.level == 1),
        "{:?}",
        h.journal.runs()
    );
    for (op, facts) in operations.iter().zip(&before) {
        let (state, mut store) = h.journal.store();
        assert_eq!(&state.operation_facts(*op, &mut store).unwrap(), facts);
    }
    let files = h.files();
    let runs = names(&files, |n| matches!(n, JournalName::Run(..)));
    assert!(runs.len() < 9 + 2, "{runs:?}");
    assert_eq!(canonical(&genesis(&files)), canonical(h.state()));
    let reopened = open(files, false).unwrap();
    assert_eq!(canonical(reopened.state()), canonical(h.state()));
}

/// SJ-13: an ID already generated in a sealed run is not drawn again.
#[test]
fn sj13_generated_id_in_a_run_is_rejected() {
    let mut h = Harness::new();
    let project = h.register(1);
    let (_, old) = h.turn(project, None, 1, 100);
    while h.journal.runs().is_empty() {
        h.turn(project, None, 30, 60 * 1024);
    }
    assert!(
        !h.state().operations.contains_key(&old),
        "old operation rolled"
    );
    let request = h.fresh();
    let task = h.fresh();
    let conversation = h.fresh();
    let reused = Record::TurnAccepted(Box::new(TurnAccepted {
        request,
        digest: request_digest(Request::Submit {
            project,
            conversation: None,
            expected_generation: 0,
            prompt: "p",
        })
        .unwrap(),
        project,
        original_conversation: None,
        expected_generation: 0,
        conversation,
        generation: 1,
        task,
        operation: old,
        model: "m".into(),
        configuration_digest: [4; 32],
        instructions_digest: [1; 32],
        input_digest: [2; 32],
        prior_operations: vec![],
        prompt: "p".into(),
        event_cursor: 1,
        composition: Composition::default(),
        thinking_level: 0,
        thinking_request: 0,
    }));
    assert_eq!(h.append(reused), Err(Failure::Invalid));
}

/// SJ-15, SJ-16, SJ-17 (unit): at the quota normal records are held and
/// nothing is written; closure records commit; raising the quota or free
/// space ends the hold; ENOSPC with no byte written holds without fencing.
#[test]
fn sj15_sj16_sj17_quota_holds_normal_records_and_admits_closures() {
    let mut h = Harness::new();
    let project = h.register(1);
    let (conversation, operation, generation) = h.start(project, None);
    let usage = h.journal.usage();
    h.journal
        .set_quota(usage + ROLL_RESERVE + h.state().reserved_bytes as u64);
    let files = h.files();
    assert_eq!(
        h.segment(operation, generation, 0, "text"),
        Ok(()),
        "a frame in the active segment needs no new bytes"
    );
    h.journal.set_quota(usage + ROLL_RESERVE);
    assert_eq!(
        h.segment(operation, generation, 4, "more"),
        Err(Failure::Held(HoldReason::Quota))
    );
    assert_eq!(h.journal.held(), Some(HoldReason::Quota));
    let before = segments(&h.files());
    let request = h.fresh();
    let held = h.append(Record::ProjectRegistered(ProjectRegistered {
        request,
        digest: request_digest(Request::Register { location: "/held" }).unwrap(),
        project: id(9_998),
        location: "/held".into(),
        device: 8,
        inode: 8,
        registry_revision: 1,
        visibility: 1,
    }));
    assert_eq!(held, Err(Failure::Held(HoldReason::Quota)));
    assert_eq!(segments(&h.files()), before, "nothing written while held");
    drop(files);
    // Closure records commit while held.
    h.append(Record::CancelRequested(CancelRequested {
        operation,
        generation,
        cause: Cause::UserCancel,
    }))
    .unwrap();
    // Raising the quota ends the hold at the next recheck.
    h.journal.set_quota(1 << 40);
    assert!(h.journal.recheck());
    h.append(Record::TurnTerminal(TurnTerminal {
        operation,
        generation,
        kind: TerminalKind::Cancelled,
        cause: Cause::UserCancel,
        final_cursor: u64::MAX,
        usage_known: false,
        output_tokens: 0,
        text_offset: 4,
        text: String::new(),
    }))
    .unwrap();
    let _ = conversation;
    // Disk floor: free space below the floor holds with journal_disk_space.
    h.journal.files_mut().free = DISK_FLOOR;
    assert!(!h.journal.recheck() || h.journal.held().is_none());
    let request = h.fresh();
    let result = h.append(Record::ProjectRegistered(ProjectRegistered {
        request,
        digest: request_digest(Request::Register { location: "/disk" }).unwrap(),
        project: id(9_999),
        location: "/disk".into(),
        device: 9,
        inode: 9,
        registry_revision: 1,
        visibility: 1,
    }));
    assert_eq!(result, Err(Failure::Held(HoldReason::Disk)));
    h.journal.files_mut().free = 1 << 40;
    assert!(h.journal.recheck());
    // SJ-F16: ENOSPC with the size unchanged holds and does not fence.
    h.journal.files_mut().no_space = true;
    assert_eq!(
        h.append(Record::OwnerGeneration),
        Err(Failure::Held(HoldReason::Disk))
    );
    assert!(!h.journal.fenced());
    assert!(h.journal.recheck());
    h.append(Record::OwnerGeneration).unwrap();
}

/// SJ-18: a short frame in the newest file is an incomplete tail; in a
/// sealed file it is damage.
#[test]
fn sj18_torn_tail_in_newest_file_and_short_sealed_file() {
    let (mut h, _) = long_journal();
    let files = h.files();
    let newest = *files
        .files
        .keys()
        .rfind(|n| matches!(n, JournalName::Segment(_)))
        .unwrap();
    for cut in [1usize, 100, 150, 1000] {
        let mut torn = files.clone();
        let bytes = torn.files.get_mut(&newest).unwrap();
        if bytes.len() <= cut {
            continue;
        }
        let length = bytes.len();
        bytes.truncate(length - cut);
        assert_eq!(
            open(torn, true).err(),
            Some(Failure::Replay(ReplayError::IncompleteTail)),
            "cut {cut}"
        );
    }
    let sealed = segments(&files)[0].0;
    let mut short = files.clone();
    short.files.retain(|name, _| !name.is_derived());
    let bytes = short.files.get_mut(&JournalName::Segment(sealed)).unwrap();
    let length = bytes.len();
    bytes.truncate(length - 10);
    assert!(open(short, true).is_err());
}

/// SJ-19, SJ-21: a damaged sealed frame or run page fails the lookup that
/// reads it.
#[test]
fn sj19_sj21_sealed_frame_and_run_page_damage_fail_reads() {
    let mut h = Harness::new();
    let first = h.register(1);
    for n in 2..=70 {
        h.register(n);
    }
    let project = h.register(71);
    while h.journal.runs().is_empty() {
        h.turn(project, None, 30, 60 * 1024);
    }
    assert!(!h.state().projects.contains_key(&first), "evicted");
    let files = h.files();
    // Damage the kind 4 frame of the evicted project in its sealed file.
    let (_, entry) = {
        let (state, mut store) = h.journal.store();
        let found = state.find_project(first, &mut store).unwrap().unwrap();
        (state.clone(), found)
    };
    let mut damaged = files.clone();
    let locator = entry.registered;
    damaged
        .files
        .get_mut(&JournalName::Segment(locator.segment))
        .unwrap()[locator.offset as usize + 200] ^= 1;
    let mut journal = open(damaged, false).unwrap();
    let (state, mut store) = journal.store();
    assert_eq!(
        state.find_project(first, &mut store).unwrap_err(),
        ReplayError::Corrupt
    );
    // Damage a run page: the lookup fails.
    let mut damaged = files.clone();
    let run = names(&damaged, |n| matches!(n, JournalName::Run(..)))[0];
    let bytes = damaged.files.get_mut(&run).unwrap();
    for at in (100..bytes.len() - 100).step_by(4096) {
        bytes[at] ^= 1;
    }
    let mut journal = open(damaged, false).unwrap();
    let (state, mut store) = journal.store();
    assert_eq!(
        state.find_project(first, &mut store).unwrap_err(),
        ReplayError::Corrupt
    );
}

/// SJ-20: a missing interior segment file is damage.
#[test]
fn sj20_missing_interior_segment_is_corrupt() {
    let (mut h, _) = long_journal();
    let mut files = h.files();
    let interior = segments(&files)[1].0;
    files.files.remove(&JournalName::Segment(interior));
    assert!(matches!(open(files, true), Err(Failure::Replay(_))));
}

/// SJ-26: the read-only open never writes, for valid and damaged layouts.
#[test]
fn sj26_read_only_open_never_writes() {
    let (mut h, _) = long_journal();
    let files = h.files();
    let mut fixtures = vec![files.clone()];
    let mut with_tmp = files.clone();
    with_tmp
        .files
        .insert(JournalName::CheckpointTmp(3), vec![1, 2, 3]);
    fixtures.push(with_tmp);
    let mut stripped = files.clone();
    stripped.files.retain(|name, _| !name.is_derived());
    fixtures.push(stripped);
    for fixture in fixtures {
        let before = fixture.files.clone();
        let mut opened = match Journal::open(fixture, false, &AtomicBool::new(false)) {
            Ok(Opened::Journal(mut journal)) => journal.files_mut().clone(),
            Ok(Opened::Empty(files)) => files,
            Err(_) => continue,
        };
        assert_eq!(opened.files, before);
        let _ = &mut opened;
    }
}

/// SJ-30 (unit): the 65,537th piece (here a lowered bound) is refused.
#[test]
fn sj30_single_response_piece_bound() {
    let mut h = Harness::new();
    let project = h.register(1);
    let (_, operation, generation) = h.start(project, None);
    h.journal.set_piece_limit_for_tests(3);
    for n in 0..3 {
        h.segment(operation, generation, n * 10, &"a".repeat(10))
            .unwrap();
    }
    assert_eq!(
        h.segment(operation, generation, 30, &"a".repeat(10)),
        Err(Failure::Exhausted)
    );
}

/// SJ-32 (unit): derived files hold no prompt, response or argument text.
#[test]
fn sj32_derived_files_hold_no_text() {
    let mut h = Harness::new();
    let project = h.register(1);
    let (_, operation, generation) = h.start(project, None);
    let marker = "MARKER-RESPONSE-7f3a";
    let text = format!("{marker}{}", "z".repeat(60_000));
    for n in 0..140 {
        h.segment(operation, generation, n * text.len() as u64, &text)
            .unwrap();
    }
    let files = h.files();
    let derived: Vec<_> = files.files.iter().filter(|(n, _)| n.is_derived()).collect();
    assert!(!derived.is_empty());
    for (name, bytes) in derived {
        assert!(
            !bytes.windows(marker.len()).any(|w| w == marker.as_bytes()),
            "{name:?}"
        );
    }
}

/// SJ-33: format numbers are unchanged: frame 1, checkpoint 1, run 1.
#[test]
fn sj33_format_numbers_are_unchanged() {
    let (mut h, _) = long_journal();
    let files = h.files();
    let (_, segment) = &segments(&files)[0];
    assert_eq!(&segment[..8], b"ASURAJ01");
    assert_eq!(segment[8..10], 1u16.to_be_bytes());
    let checkpoint = names(&files, |n| matches!(n, JournalName::Checkpoint(_)))[0];
    assert_eq!(&files.files[&checkpoint][..8], b"ASURAK01");
    assert_eq!(files.files[&checkpoint][8..10], 1u16.to_be_bytes());
    let run = names(&files, |n| matches!(n, JournalName::Run(..)))[0];
    assert_eq!(&files.files[&run][..8], b"ASURAX01");
    assert_eq!(files.files[&run][8..10], 1u16.to_be_bytes());
}

/// SJ-35, SJ-36, SJ-38 (unit): no project or conversation total; recent
/// sets stay bounded; the latest conversation of an evicted project comes
/// from its highest `A` entry; an evicted conversation continues.
#[test]
fn sj35_sj36_sj38_no_totals_bounded_recent_sets_and_index_lookups() {
    let mut h = Harness::new();
    let first = h.register(1);
    let (old_conversation, _) = h.turn(first, None, 0, 0);
    let mut projects = vec![first];
    for n in 2..=300 {
        projects.push(h.register(n));
    }
    let busy = projects[299];
    for _ in 0..300 {
        h.turn(busy, None, 0, 0);
    }
    assert!(h.state().projects.len() <= RECENT_PROJECTS + MAX_INPUTS + 1);
    assert!(h.state().conversations.len() <= RECENT_CONVERSATIONS + MAX_INPUTS + 1);
    assert!(!h.state().projects.contains_key(&first));
    assert!(!h.state().conversations.contains_key(&old_conversation));
    {
        let (state, mut store) = h.journal.store();
        let entry = state.find_project(first, &mut store).unwrap().unwrap();
        assert_eq!(entry.latest.unwrap().conversation, old_conversation);
        let conversation = state
            .find_conversation(old_conversation, &mut store)
            .unwrap()
            .unwrap();
        assert_eq!((conversation.generation, conversation.completed), (1, 1));
    }
    // The evicted conversation continues with generation 2.
    h.completed.insert(old_conversation, (1, 1));
    h.turn(first, Some(old_conversation), 0, 0);
    assert_eq!(h.state().conversations[&old_conversation].generation, 2);
}

/// SJ-36 (unit, 10,000 conversations): no conversation total; the recent
/// set stays at 256 plus pins; eviction keeps the newest by activity, then
/// ID; a turn in an evicted conversation is admitted through `V` and `T`;
/// a restart keeps every generation.
#[test]
fn sj36_ten_thousand_conversations_without_a_total() {
    let mut h = Harness::new();
    let project = h.register(1);
    let mut conversations = Vec::new();
    for _ in 0..10_000 {
        conversations.push(h.turn(project, None, 0, 0).0);
    }
    assert!(h.state().conversations.len() <= RECENT_CONVERSATIONS + MAX_INPUTS + 1);
    // The newest 256 by activity stay; the oldest left memory.
    assert!(h.state().conversations.contains_key(&conversations[9_999]));
    assert!(!h.state().conversations.contains_key(&conversations[0]));
    let old = conversations[17];
    h.completed.insert(old, (1, 1));
    h.turn(project, Some(old), 0, 0);
    assert_eq!(h.state().conversations[&old].generation, 2);
    let files = h.files();
    let mut reopened = open(files, false).unwrap();
    let (state, mut store) = reopened.store();
    for (index, conversation) in conversations.iter().enumerate().step_by(997) {
        let found = state
            .find_conversation(*conversation, &mut store)
            .unwrap()
            .unwrap();
        let expected = if *conversation == old { 2 } else { 1 };
        assert_eq!(found.generation, expected, "conversation {index}");
    }
}

// ----- J2 job records (tool-job-controller.md, J2T-02, J2T-03). -----

/// A `run` exec_cmd kind 14 that fits a job.
fn exec_call(operation: Id, generation: u64, ordinal: u32) -> Record {
    Record::ToolCall(Box::new(ToolCall {
        operation,
        generation,
        ordinal,
        source: ToolCallSource::GeneratedContent,
        name: "exec_cmd".into(),
        name_bytes: 8,
        tool_operation: Some(ToolOperation::ExecCmd),
        decision: ToolDecision::Run,
        fault: None,
        busy: None,
        form: ArgumentForm::Canonical,
        arguments_bytes: 18,
        arguments: "{\"command\":\"true\"}".into(),
        number_kinds: Vec::new(),
        unnumbered: 0,
        rule_set_revision: 1,
        rule_set_digest: [3; 32],
        command_cut: false,
        whole_command: false,
        refusal: None,
    }))
}
fn intent(operation: Id, generation: u64, ordinal: u32) -> Record {
    Record::JobIntent(JobIntent {
        origin: JobOrigin::Model {
            operation,
            generation,
            ordinal,
        },
        agent: 0,
        time: 1_760_000_000_000,
    })
}
fn handle(operation: Id, generation: u64, ordinal: u32) -> Record {
    Record::ToolResult(ToolResult {
        operation,
        generation,
        ordinal,
        status: 1,
        text: "job".into(),
        next_offset: None,
        truncated: false,
    })
}
fn exited(job: Id) -> Record {
    let stream = || JobStream {
        capture: CaptureStream {
            observed_bytes: 4,
            stored_bytes: 4,
            digest: Some([7; 32]),
            eof: true,
            ..CaptureStream::default()
        },
        lines: 1,
    };
    Record::JobSettled(Box::new(JobSettled {
        job,
        outcome: JobOutcome::Exited,
        reason: JobReason::None,
        exit_kind: 1,
        exit_value: 0,
        cleanup: JobCleanup::Confirmed,
        unconfirmed_reason: 0,
        escapees: 0,
        elapsed_ms: 3,
        capture_state: JobCaptureState::Complete,
        capture_reason: CaptureReason::None,
        stdout: stream(),
        stderr: stream(),
        distinct_numbers: 0,
        numbers: Vec::new(),
        pause_count: 0,
        first_pause_ms: 0,
        paused_ms: 0,
        final_reservation: INITIAL_RESERVATION,
        settled_time: 1_760_000_000_002,
    }))
}
fn cancel(job: Id, reason: JobReason) -> Record {
    Record::JobCancelRequested(JobCancelRequested {
        job,
        requester: reason.requester().unwrap(),
        reason,
        kill: reason == JobReason::PersonKill,
    })
}
fn interrupted(job: Id) -> Record {
    Record::JobSettled(Box::new(JobSettled::interrupted(job, 1_760_000_000_001)))
}
impl Harness {
    /// Kind 14 `run` exec_cmd, kind 26 and kind 15 at `ordinal`; returns
    /// the job ID.
    fn job(&mut self, operation: Id, generation: u64, ordinal: u32) -> Id {
        self.append(exec_call(operation, generation, ordinal))
            .unwrap();
        self.append(intent(operation, generation, ordinal)).unwrap();
        self.append(handle(operation, generation, ordinal)).unwrap();
        exec_cmd_job_id(operation, generation, ordinal)
    }
}
fn unsettled(state: &Replay) -> Vec<UnsettledJob> {
    state.unsettled_jobs()
}

/// J2T-03 (unit, SJ-05 property with live jobs): random turns start jobs,
/// cancel and settle them in later turns (JR8), restart and settle the rest
/// `interrupted`, across rolls. The checkpoint plus tail equals the genesis
/// replay, including the unsettled jobs and their derived facts.
#[test]
fn j2t_03_checkpoint_with_live_jobs_equals_genesis_replay() {
    for seed in 1..=3u64 {
        let mut h = Harness::new();
        let mut rng = seed;
        let mut step = || {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            rng
        };
        let projects: Vec<Id> = (0..4).map(|n| h.register(seed * 100 + n)).collect();
        // Unsettled jobs: (job, owner generation of kind 26, kind 27 sent).
        let mut live: Vec<(Id, u64, bool)> = Vec::new();
        let mut settled = 0;
        for _ in 0..160 {
            let project = projects[(step() % 4) as usize];
            let (conversation, operation, generation) = h.start(project, None);
            let owner = h.state().owner_generation;
            for ordinal in 1..=(step() % 3) as u32 {
                if live.len() >= MAX_UNSETTLED_JOBS - 2 {
                    break;
                }
                let job = h.job(operation, generation, ordinal);
                live.push((job, owner, false));
            }
            // Cancel and settle older jobs during this turn (JR8).
            for _ in 0..step() % 3 {
                if live.is_empty() {
                    break;
                }
                let at = (step() % live.len() as u64) as usize;
                let (job, job_owner, cancelled) = live[at];
                if job_owner != owner {
                    h.append(interrupted(job)).unwrap();
                    live.remove(at);
                    settled += 1;
                } else if !cancelled && step() % 2 == 0 {
                    h.append(cancel(job, JobReason::ClientCancel)).unwrap();
                    live[at].2 = true;
                } else {
                    h.append(exited(job)).unwrap();
                    live.remove(at);
                    settled += 1;
                }
            }
            let text = "r".repeat(61_440);
            let pieces = step() % 4;
            for n in 0..pieces {
                h.segment(operation, generation, n * 61_440, &text).unwrap();
            }
            h.finish(conversation, operation, generation, pieces * 61_440);
            if step() % 17 == 0 {
                h.append(Record::OwnerGeneration).unwrap();
            }
        }
        assert!(settled > 20 && !live.is_empty(), "seed {seed}");
        let files = h.files();
        assert!(segments(&files).len() >= 2, "seed {seed}");
        assert!(!h.journal.runs().is_empty(), "seed {seed}");
        let replayed = genesis(&files);
        assert_eq!(canonical(&replayed), canonical(h.state()), "seed {seed}");
        assert_eq!(unsettled(&replayed), unsettled(h.state()), "seed {seed}");
        assert_eq!(unsettled(&replayed).len(), live.len());
        let reopened = open(files, false).unwrap();
        assert_eq!(canonical(reopened.state()), canonical(&replayed));
        assert_eq!(unsettled(reopened.state()), unsettled(&replayed));
        assert_eq!(
            (
                reopened.state().reserved_bytes,
                reopened.state().reserved_frames
            ),
            (replayed.reserved_bytes, replayed.reserved_frames)
        );
        // Each unsettled job pins its conversation and project.
        for job in unsettled(&replayed) {
            assert!(
                reopened
                    .state()
                    .conversations
                    .contains_key(&job.conversation)
            );
            assert!(reopened.state().projects.contains_key(&job.project));
        }
    }
}

/// J2T-03 (unit): the roll after a job's kind 28 emits its `W` entry and
/// the `X` form 2 entries of both payloads; before the roll the delta
/// answers. A checkpoint holds the unsettled job's kind 26 and kind 27
/// locators and rebuilds it.
#[test]
fn j2t_03_w_and_x_entries_and_checkpoint_jobs() {
    let mut h = Harness::new();
    let project = h.register(1);
    let (conversation, operation, generation) = h.start(project, None);
    let first = h.job(operation, generation, 1);
    let second = h.job(operation, generation, 2);
    h.append(cancel(second, JobReason::PersonStop)).unwrap();
    h.append(exited(first)).unwrap();
    h.finish(conversation, operation, generation, 0);
    let installation = h.state().installation_id;
    let payloads = job_payload_ids(installation, first);
    let check = |state: &Replay, store: &mut dyn Index| {
        let Some(JobState::Settled(settled)) = state.job_state(first, store).unwrap() else {
            panic!("settled");
        };
        assert_eq!(settled.conversation, conversation);
        assert_eq!(settled.origin, 1);
        assert!(settled.cancel.is_none());
        for payload in payloads {
            let Some(Entry::JobPayload {
                job,
                intent,
                settled: at,
                last_use,
            }) = state
                .entry(&Key::new(Tag::Payload, payload, 0), store)
                .unwrap()
            else {
                panic!("X form 2");
            };
            assert_eq!((job, intent, at), (first, settled.intent, settled.settled));
            assert_eq!(last_use, settled.settled.sequence);
        }
        let Some(JobState::Unsettled(job)) = state.job_state(second, store).unwrap() else {
            panic!("unsettled");
        };
        assert_eq!(job.cancel.unwrap().reason, JobReason::PersonStop);
        assert_eq!((job.conversation, job.project), (conversation, project));
        assert_eq!(job.payloads, job_payload_ids(installation, second));
        // The job-mode calls have no `X` form 1 entries: a call's payload
        // IDs are its job's (one derivation), whose entries are form 2.
        assert_eq!(
            crate::payload::job_payloads(installation, exec_cmd_job_id(operation, generation, 1)),
            payloads
        );
        assert!(state.job_state([0x77; 16], store).unwrap().is_none());
    };
    // Before a roll: the delta holds the entries.
    {
        let (state, mut store) = h.journal.store();
        assert!(!state.delta_is_empty());
        check(&state, &mut store);
    }
    // Force rolls: the entries move to a run; the checkpoint keeps `second`.
    let (_, op2, gen2) = h.start(project, None);
    for n in 0..150u64 {
        h.segment(op2, gen2, n * 61_440, &"z".repeat(61_440))
            .unwrap();
    }
    assert!(!h.journal.runs().is_empty());
    let files = h.files();
    let mut reopened = open(files.clone(), false).unwrap();
    {
        let (state, mut store) = reopened.store();
        assert!(state.base > 0 && state.delta_tag(Tag::Job).is_empty());
        check(&state, &mut store);
    }
    assert_eq!(canonical(reopened.state()), canonical(&genesis(&files)));
    // The checkpoint lists the job: kind 26 then kind 27 locators.
    let job = &unsettled(reopened.state())[0];
    let bytes = canonical(reopened.state());
    let tail = &bytes[bytes.len() - 40 - 52..bytes.len() - 40];
    assert_eq!(&tail[..4], &1u32.to_be_bytes());
    assert_eq!(&tail[4..28], &job.intent.encode());
    assert_eq!(&tail[28..52], &job.cancel.unwrap().frame.encode());
}

/// J2T-03 (unit): `reserve()` sizes for 0, 1 and 64 jobs, with and without
/// kind 27, independent of the active operation; JR9 at the 65th job.
#[test]
fn j2t_03_ledger_sizes_for_0_1_and_64_jobs() {
    assert_eq!(JOB_SETTLED_CAPACITY, 4_278);
    assert_eq!(JOB_CANCEL_CAPACITY, 203);
    assert_eq!(job_ledger(&[]), (0, 0));
    let mut h = Harness::new();
    let project = h.register(1);
    let (conversation, operation, generation) = h.start(project, None);
    let reserved = |h: &Harness| (h.state().reserved_bytes, h.state().reserved_frames);
    let (base, frames) = reserved(&h);
    let job = h.job(operation, generation, 1);
    assert_eq!(reserved(&h), (base + 4_481, frames + 2));
    h.append(cancel(job, JobReason::ModelCancel)).unwrap();
    assert_eq!(reserved(&h), (base + 4_278, frames + 1));
    h.finish(conversation, operation, generation, 0);
    // No active operation: the ledger alone.
    assert_eq!(reserved(&h), (4_278, 1));
    h.append(exited(job)).unwrap();
    assert_eq!(reserved(&h), (0, 0));
    // 64 unsettled jobs across two turns: 286,784 bytes and 128 frames.
    let mut jobs = Vec::new();
    for _ in 0..2 {
        let (conversation, operation, generation) = h.start(project, None);
        for ordinal in 1..=32 {
            jobs.push(h.job(operation, generation, ordinal));
        }
        h.finish(conversation, operation, generation, 0);
    }
    assert_eq!(reserved(&h), (286_784, 128));
    assert_eq!(job_ledger(h.state().jobs.values()), (286_784, 128));
    for job in &jobs {
        h.append(cancel(*job, JobReason::Shutdown)).unwrap();
    }
    assert_eq!(reserved(&h), (64 * 4_278, 64));
    // JR9: a 65th unsettled job is `Limit`; the journal reports `Exhausted`.
    let (_, operation, generation) = h.start(project, None);
    h.append(exec_call(operation, generation, 1)).unwrap();
    assert_eq!(
        h.append(intent(operation, generation, 1)),
        Err(Failure::Exhausted)
    );
    h.append(exited(jobs[0])).unwrap();
    h.append(intent(operation, generation, 1)).unwrap();
}

/// J2T-02 (unit, journal quota): kind 26 is a normal record held when its
/// ledger does not fit; kinds 27 and 28 are closure records that commit
/// while the journal is held (SJ-D18, SJ-D19).
#[test]
fn j2t_02_kind_26_is_held_at_the_quota_while_27_and_28_commit() {
    assert!(!is_closure(&intent([1; 16], 1, 1)));
    assert!(is_closure(&cancel([1; 16], JobReason::Shutdown)));
    assert!(is_closure(&interrupted([1; 16])));
    let mut h = Harness::new();
    let project = h.register(1);
    let (_, operation, generation) = h.start(project, None);
    let live = h.job(operation, generation, 1);
    h.append(exec_call(operation, generation, 2)).unwrap();
    let usage = h.journal.usage();
    // Kind 26 replaces the 4,250-byte kind 23 reservation with the 4,481-byte
    // ledger: 231 more bytes than the 100 left.
    h.journal
        .set_quota(usage + ROLL_RESERVE + h.state().reserved_bytes as u64 + 100);
    let before = segments(&h.files());
    assert_eq!(
        h.append(intent(operation, generation, 2)),
        Err(Failure::Held(HoldReason::Quota))
    );
    assert_eq!(segments(&h.files()), before, "nothing written");
    assert_eq!(h.journal.held(), Some(HoldReason::Quota));
    // The live job's kinds 27 and 28 commit while held.
    h.append(cancel(live, JobReason::PersonKill)).unwrap();
    h.append(exited(live)).unwrap();
    assert!(h.state().jobs.is_empty());
    // Still held: the hold ends only when the quota or disk has room again.
    assert_eq!(
        h.append(intent(operation, generation, 2)),
        Err(Failure::Held(HoldReason::Quota))
    );
    h.journal.set_quota(1 << 40);
    assert!(h.journal.recheck());
    h.append(intent(operation, generation, 2)).unwrap();
    assert_eq!(h.state().jobs.len(), 1);
}
