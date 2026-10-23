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
        self.start_with(project, conversation, 0, 0)
    }
    /// [`Self::start`] with kind 21 `notice_through` = `notice` and
    /// `notice_live` = `live` (J5).
    fn start_with(
        &mut self,
        project: Id,
        conversation: Option<Id>,
        notice: u64,
        live: u32,
    ) -> (Id, Id, u64) {
        let (record, target, operation, generation) =
            self.accepted(project, conversation, notice, live);
        self.append(record).unwrap();
        let helper = self.fresh();
        self.append(Record::StartAuthorized(StartAuthorized {
            operation,
            generation,
            helper,
            input_digest: [2; 32],
        }))
        .unwrap();
        (target, operation, generation)
    }
    /// The kind 21 of the next turn: (record, conversation, operation,
    /// generation).
    fn accepted(
        &mut self,
        project: Id,
        conversation: Option<Id>,
        notice: u64,
        live: u32,
    ) -> (Record, Id, Id, u64) {
        let target = conversation.unwrap_or_else(|| self.fresh());
        let expected = conversation.map_or(0, |c| self.completed.get(&c).map_or(0, |v| v.1));
        let (operation, task, request) = (self.fresh(), self.fresh(), self.fresh());
        let completed = self.completed.get(&target).map_or(0, |v| v.0);
        let record = Record::TurnAccepted(Box::new(TurnAccepted {
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
                notice_through: notice,
                notice_live: live,
                ..Composition::default()
            },
            thinking_level: 0,
            thinking_request: 0,
        }));
        (record, target, operation, expected + 1)
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
        reason: None,
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
/// Handle delivery: the call's kind 29 with a `running` snapshot (J3).
fn handle(operation: Id, generation: u64, ordinal: u32) -> Record {
    Record::JobResult(Box::new(JobResult {
        operation,
        generation,
        ordinal,
        status: 1,
        outcome: JobResultOutcome::Ok,
        job: [0; 16],
        body: JobResultBody::State(JobSnapshot {
            state: JobRunState::Running,
            numbers: [0; 3],
            elapsed_ms: 1,
            stdout_bytes: 0,
            stderr_bytes: 0,
        }),
    }))
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

// ----- J3 and J4 (tool-job-controller.md, J3T-03 and J4T-03). -----

/// A `run` `job` call with operation code `op` (11 to 16).
fn job_tool(operation: Id, generation: u64, ordinal: u32, op: ToolOperation) -> Record {
    let Record::ToolCall(call) = exec_call(operation, generation, ordinal) else {
        unreachable!()
    };
    let arguments = format!("{{\"command\":\"{}\"}}", op.as_str());
    Record::ToolCall(Box::new(ToolCall {
        name: "job".into(),
        name_bytes: 3,
        tool_operation: Some(op),
        form: ArgumentForm::Verbatim,
        arguments_bytes: arguments.len() as u32,
        arguments,
        ..*call
    }))
}
fn retention(job: Id, action: RetentionAction, author: RetentionAuthor) -> Record {
    Record::JobRetention(Box::new(JobRetention {
        job,
        action,
        author,
        time: 1_760_000_000_007,
        note: if action == RetentionAction::Note && matches!(author, RetentionAuthor::Person { .. })
        {
            "person note".into()
        } else {
            String::new()
        },
    }))
}
/// J4T-03 (unit): the listing read merges a settled job whose `L`, `M`,
/// `Y`, `U` and `E` entries are in an index run, a settled job in the
/// delta and a live job; J3T-03: a checkpoint taken while a `job note`
/// call waits for its kind 29 keeps the call's model note (the follow
/// locator), so the reopened state and its reservation equal the genesis
/// replay.
#[test]
fn j4t_03_listing_merges_runs_delta_and_live_and_checkpoint_keeps_the_model_note() {
    let mut h = Harness::new();
    let project = h.register(1);
    let person = RetentionAuthor::Person { request: id(900) };
    let (conversation, operation, generation) = h.start(project, None);
    let first = h.job(operation, generation, 1);
    h.append(exited(first)).unwrap();
    h.append(retention(first, RetentionAction::Pin, person))
        .unwrap();
    h.finish(conversation, operation, generation, 0);
    // Force rolls: the first job's entries move to an index run.
    let (_, op2, gen2) = h.start(project, Some(conversation));
    let text = "z".repeat(61_440);
    for n in 0..150u64 {
        h.segment(op2, gen2, n * 61_440, &text).unwrap();
    }
    assert!(!h.journal.runs().is_empty());
    let second = h.job(op2, gen2, 1);
    let third = h.job(op2, gen2, 2);
    h.append(exited(third)).unwrap();
    // A pending `job note` call with its model note committed.
    h.append(job_tool(op2, gen2, 3, ToolOperation::JobNote))
        .unwrap();
    h.append(retention(
        second,
        RetentionAction::Note,
        RetentionAuthor::Model {
            operation: op2,
            generation: gen2,
            ordinal: 3,
        },
    ))
    .unwrap();
    let check = |state: &Replay, store: &mut dyn Index| {
        let listed: Vec<Id> = state
            .jobs(JobScope::Conversation(conversation), None, 9, store)
            .unwrap()
            .iter()
            .map(|l| l.job)
            .collect();
        assert_eq!(listed, vec![third, second, first]);
        let all = state.jobs(JobScope::AllProjects, None, 9, store).unwrap();
        assert_eq!(all.len(), 3);
        assert!(all[0].settled.is_some() && all[1].settled.is_none());
        assert_eq!(
            all[2].retention,
            JobRetentionSummary {
                pinned: true,
                deleted: false,
                notes: 0,
                records: 1,
            }
        );
        assert_eq!(all[1].retention.notes, 1);
        let settlements = state
            .job_settlements(conversation, 0, u64::MAX - 1, 9, store)
            .unwrap();
        assert_eq!(
            settlements.iter().map(|l| l.job).collect::<Vec<_>>(),
            vec![third, first]
        );
    };
    {
        let (state, mut store) = h.journal.store();
        // The first job's entries are in a run; the third's in the delta.
        assert!(state.delta_tag(Tag::ConversationJob).len() == 1);
        check(&state, &mut store);
    }
    // More rolls with the note call pending: the checkpoint holds it.
    for n in 150..300u64 {
        h.segment(op2, gen2, n * 61_440, &text).unwrap();
    }
    let note_frame = h.state().operations[&op2].tools[2].note;
    assert!(note_frame.is_some());
    let files = h.files();
    let mut reopened = open(files.clone(), false).unwrap();
    assert!(reopened.state().base > 0);
    let replayed = genesis(&files);
    assert_eq!(canonical(reopened.state()), canonical(&replayed));
    let tool = &reopened.state().operations[&op2].tools[2];
    assert_eq!(tool.note, note_frame);
    assert!(tool.unresolved());
    assert_eq!(
        (
            reopened.state().reserved_bytes,
            reopened.state().reserved_frames
        ),
        (replayed.reserved_bytes, replayed.reserved_frames)
    );
    {
        let (state, mut store) = reopened.store();
        assert!(state.delta_tag(Tag::ConversationJob).is_empty());
        check(&state, &mut store);
        // JR16 after the checkpoint: the note call's kind 29 replays.
    }
    h.append(Record::JobResult(Box::new(JobResult {
        operation: op2,
        generation: gen2,
        ordinal: 3,
        status: 1,
        outcome: JobResultOutcome::Ok,
        job: second,
        body: JobResultBody::None,
    })))
    .unwrap();
    let files = h.files();
    assert_eq!(
        canonical(open(files.clone(), false).unwrap().state()),
        canonical(&genesis(&files))
    );
}

// ----- J5 job notices (tool-job-controller.md, J5T-01 to J5T-03). -----

/// The `notice_live` of an operation's kind 21, read from its frame: the
/// count stays in the frame only (J5-E8).
fn accepted_live(journal: &mut Journal<MemoryFiles>, operation: Id) -> u32 {
    let (state, mut store) = journal.store();
    let locator = match state.operations.get(&operation) {
        Some(op) => op.accepted_frame,
        None => {
            state
                .operation_facts(operation, &mut store)
                .unwrap()
                .unwrap()
                .accepted
        }
    };
    match store.frame(&locator).unwrap() {
        Record::TurnAccepted(turn) if turn.operation == operation => turn.composition.notice_live,
        other => panic!("kind 21 expected: {other:?}"),
    }
}

/// The jobs of a notice read, in its order (newest kind 28 first), with
/// each command source's ordinal.
fn notice_jobs(notice: &JobNotice) -> Vec<(Id, u32)> {
    notice
        .settlements
        .iter()
        .map(|s| {
            let JobOrigin::Model { ordinal, .. } = s.listing.intent.origin else {
                panic!("model origin")
            };
            let call = s.command.model().expect("model command");
            assert_eq!(call.ordinal, ordinal);
            assert!(s.listing.settled.is_some());
            (s.listing.job, call.ordinal)
        })
        .collect()
}
/// J5T-02 (unit) and the storage part of J5T-01: the `Prepare` notice read
/// returns the newest settlements in (P, S] with their kind 14, at most 9,
/// and the live count at S; a settlement that commits between the read and
/// the kind 21 append has a sequence above S and is in the next turn's
/// range only. J5T-03 (restart): the cursor survives index runs and a
/// checkpoint taken during a turn, and a decreasing cursor after the
/// restart is rejected with nothing written.
#[test]
fn j5t_02_notice_read_is_exactly_once_and_cursor_survives_restart() {
    let mut h = Harness::new();
    let project = h.register(1);
    let (conversation, op1, gen1) = h.start(project, None);
    // The first turn starts 11 jobs and settles the first 9.
    let jobs: Vec<Id> = (1..=11).map(|n| h.job(op1, gen1, n)).collect();
    let mut settled_at = Vec::new();
    for job in &jobs[..9] {
        h.append(exited(*job)).unwrap();
        settled_at.push(h.state().sequence);
    }
    h.finish(conversation, op1, gen1, 0);
    // A conversation with no jobs reads no settlements and no live jobs.
    let (quiet, quiet_op, quiet_gen) = h.start(project, None);
    h.finish(quiet, quiet_op, quiet_gen, 0);
    // The `Prepare` read for turn 2: P = 0 from turn 1's kind 21.
    let after = h.state().conversations[&conversation]
        .latest
        .unwrap()
        .notice_through;
    assert_eq!(after, 0);
    let notice = {
        let (state, mut store) = h.journal.store();
        let quiet_notice = state.job_notice(quiet, 0, &mut store).unwrap();
        assert_eq!((quiet_notice.settlements.len(), quiet_notice.live), (0, 0));
        state.job_notice(conversation, after, &mut store).unwrap()
    };
    let through = h.state().sequence;
    assert_eq!((notice.after, notice.through), (0, through));
    // Nine settled: all nine returned, newest first (the composer shows
    // eight and the overflow line); two live.
    assert_eq!(
        notice_jobs(&notice),
        (1..=9)
            .rev()
            .map(|n| (jobs[n - 1], n as u32))
            .collect::<Vec<_>>()
    );
    assert_eq!(notice.live, 2);
    {
        let (state, mut store) = h.journal.store();
        // Ranges with 8, 1 and 0 settlements.
        let eight = state
            .job_settlements(conversation, settled_at[0], through, 9, &mut store)
            .unwrap();
        assert_eq!(eight.len(), 8);
        let one = state
            .job_settlements(conversation, settled_at[7], settled_at[8], 9, &mut store)
            .unwrap();
        assert_eq!(one.iter().map(|l| l.job).collect::<Vec<_>>(), [jobs[8]]);
        let none = state
            .job_settlements(conversation, settled_at[8], through, 9, &mut store)
            .unwrap();
        assert!(none.is_empty());
    }
    // A settlement commits between the read and the kind 21 append.
    h.append(exited(jobs[9])).unwrap();
    let late = h.state().sequence;
    assert!(late > through);
    let (_, op2, gen2) = h.start_with(project, Some(conversation), through, notice.live);
    {
        let (state, mut store) = h.journal.store();
        // Turn 2's range (0, S] never holds the late settlement.
        let shown = state
            .job_settlements(conversation, 0, through, 64, &mut store)
            .unwrap();
        assert!(shown.iter().all(|l| l.job != jobs[9]));
        assert_eq!(shown.len(), 9);
    }
    // Rolls during turn 2: turn 1's `T` entry moves to an index run and the
    // checkpoint holds the active turn with its cursor.
    let text = "z".repeat(61_440);
    for n in 0..300u64 {
        h.segment(op2, gen2, n * 61_440, &text).unwrap();
    }
    assert!(!h.journal.runs().is_empty());
    let files = h.files();
    let mut reopened = open(files.clone(), false).unwrap();
    assert!(reopened.state().base > 0);
    assert_eq!(canonical(reopened.state()), canonical(&genesis(&files)));
    assert_eq!(reopened.state().operations[&op2].notice_through, through);
    assert_eq!(accepted_live(&mut reopened, op2), 2);
    assert_eq!(
        reopened.state().conversations[&conversation]
            .latest
            .unwrap()
            .notice_through,
        through
    );
    h.finish(conversation, op2, gen2, 300 * 61_440);
    // More rolls: turn 2's `T` entry is in a run; a conversation loaded from
    // the index carries the cursor.
    let (other, op3, gen3) = h.start(project, None);
    for n in 0..300u64 {
        h.segment(op3, gen3, n * 61_440, &text).unwrap();
    }
    h.finish(other, op3, gen3, 300 * 61_440);
    let files = h.files();
    let mut reopened = open(files.clone(), true).unwrap();
    assert_eq!(canonical(reopened.state()), canonical(&genesis(&files)));
    assert_eq!(accepted_live(&mut reopened, op2), 2);
    {
        let (state, mut store) = reopened.store();
        assert!(
            state
                .delta_tag(Tag::Turn)
                .iter()
                .all(|(key, _)| key.id != conversation)
        );
        let mut evicted = (*state).clone();
        evicted.conversations.remove(&conversation);
        let found = evicted
            .find_conversation(conversation, &mut store)
            .unwrap()
            .unwrap();
        assert_eq!(found.latest.unwrap().notice_through, through);
        // Turn 3's read: (S, now] holds only the late settlement; one live.
        let next = state.job_notice(conversation, through, &mut store).unwrap();
        assert_eq!(notice_jobs(&next), [(jobs[9], 10)]);
        assert_eq!(next.live, 1);
        assert_eq!(next.settlements[0].listing.settled_frame.sequence, late);
    }
    // A decreasing cursor after the restart is rejected; nothing is written.
    let sequence = reopened.state().sequence;
    h.journal = reopened;
    let before = segments(&h.files());
    let (decreasing, ..) = h.accepted(project, Some(conversation), through - 1, 0);
    assert_eq!(h.append(decreasing), Err(Failure::Invalid));
    assert_eq!(h.state().sequence, sequence);
    assert_eq!(segments(&h.files()), before);
    // The equal cursor is accepted.
    let (_, op4, _) = h.start_with(project, Some(conversation), through, 0);
    assert_eq!(
        h.state().conversations[&conversation]
            .latest
            .unwrap()
            .operation,
        op4
    );
}

/// J5T-11 (storage part, notices in later turns): a prior turn's notice
/// inputs are rebuilt from records only, with P from the earlier `T` entry
/// and S and the live count from the turn's kind 21: the same settlements
/// as the binding-point read, before and after a restart from index runs.
/// A cursor below P, a live count above 64 or a missing earlier `T` entry
/// is `Corrupt`.
#[test]
fn j5t_11_prior_notice_rebuilds_the_binding_range() {
    let mut h = Harness::new();
    let project = h.register(1);
    let (conversation, op1, gen1) = h.start(project, None);
    let jobs: Vec<Id> = (1..=4).map(|n| h.job(op1, gen1, n)).collect();
    h.append(exited(jobs[0])).unwrap();
    h.finish(conversation, op1, gen1, 0);
    h.append(exited(jobs[1])).unwrap();
    // Turn 2's binding read: P = 0 (turn 1's cursor), S now, live 2.
    let bound = {
        let (state, mut store) = h.journal.store();
        state.job_notice(conversation, 0, &mut store).unwrap()
    };
    assert_eq!(notice_jobs(&bound), [(jobs[1], 2), (jobs[0], 1)]);
    assert_eq!(bound.live, 2);
    let (_, op2, gen2) = h.start_with(project, Some(conversation), bound.through, bound.live);
    h.finish(conversation, op2, gen2, 0);
    // Turn 3's binding read: P = turn 2's S.
    h.append(exited(jobs[2])).unwrap();
    let bound3 = {
        let (state, mut store) = h.journal.store();
        state
            .job_notice(conversation, bound.through, &mut store)
            .unwrap()
    };
    assert_eq!(notice_jobs(&bound3), [(jobs[2], 3)]);
    let (_, op3, gen3) = h.start_with(project, Some(conversation), bound3.through, bound3.live);
    h.finish(conversation, op3, gen3, 0);
    // A settlement after turn 3's S changes neither rebuilt notice.
    h.append(exited(jobs[3])).unwrap();
    let check = |state: &Replay, store: &mut dyn Index| {
        let first = state.prior_notice(conversation, gen1, 0, 0, store).unwrap();
        assert_eq!((first.after, first.through, first.live), (0, 0, 0));
        assert!(first.settlements.is_empty());
        let second = state
            .prior_notice(conversation, gen2, bound.through, bound.live, store)
            .unwrap();
        assert_eq!(second, bound, "turn 2 rebuilt as bound");
        let third = state
            .prior_notice(conversation, gen3, bound3.through, bound3.live, store)
            .unwrap();
        assert_eq!(third, bound3, "turn 3 rebuilt as bound");
        // S below P, a live count above the bound, no earlier `T` entry.
        assert_eq!(
            state.prior_notice(conversation, gen3, bound.through - 1, 0, store),
            Err(ReplayError::Corrupt)
        );
        assert_eq!(
            state.prior_notice(conversation, gen2, bound.through, 65, store),
            Err(ReplayError::Corrupt)
        );
        assert_eq!(
            state.prior_notice(conversation, gen3 + 2, bound3.through, 0, store),
            Err(ReplayError::Corrupt)
        );
    };
    {
        let (state, mut store) = h.journal.store();
        check(&state, &mut store);
    }
    // After a restart with the `T` entries in index runs.
    let (other, op4, gen4) = h.start(project, None);
    let text = "z".repeat(61_440);
    for n in 0..300u64 {
        h.segment(op4, gen4, n * 61_440, &text).unwrap();
    }
    h.finish(other, op4, gen4, 300 * 61_440);
    let mut reopened = open(h.files(), true).unwrap();
    let (state, mut store) = reopened.store();
    assert!(
        state
            .delta_tag(Tag::Turn)
            .iter()
            .all(|(key, _)| key.id != conversation)
    );
    check(&state, &mut store);
}

/// An index that counts the frames read through it (J5T-10).
struct CountingIndex<'a> {
    inner: &'a mut dyn Index,
    frames: usize,
}
impl Index for CountingIndex<'_> {
    fn get(&mut self, key: &Key) -> Result<Option<Value>, ReplayError> {
        self.inner.get(key)
    }
    fn range(
        &mut self,
        lo: &Key,
        hi: &Key,
        descending: bool,
        limit: usize,
    ) -> Result<Vec<(Key, Value)>, ReplayError> {
        self.inner.range(lo, hi, descending, limit)
    }
    fn frame(&mut self, locator: &Locator) -> Result<Record, ReplayError> {
        self.frames += 1;
        self.inner.frame(locator)
    }
    fn frame_owner(&mut self, locator: &Locator) -> Result<(u64, Record), ReplayError> {
        self.frames += 1;
        self.inner.frame_owner(locator)
    }
    fn frame_command(&mut self, locator: &Locator) -> Result<(Hash, Record), ReplayError> {
        self.frames += 1;
        self.inner.frame_command(locator)
    }
}

/// J5T-10 (unit, J5-E9): the notice read goes through the shared job
/// listing read. A settlement with a kind 27 reads 4 frames (kind 26, 27,
/// 28 and the kind 14 command source); 9 such settlements read 36, with
/// index runs and without.
#[test]
fn j5t_10_notice_read_reads_four_frames_per_settlement() {
    let mut h = Harness::new();
    let project = h.register(1);
    let (conversation, op, generation) = h.start(project, None);
    let jobs: Vec<Id> = (1..=10).map(|n| h.job(op, generation, n)).collect();
    for job in &jobs {
        h.append(cancel(*job, JobReason::ModelCancel)).unwrap();
        h.append(exited(*job)).unwrap();
    }
    h.finish(conversation, op, generation, 0);
    let count = |h: &mut Harness, after: u64, through: u64| {
        let (state, mut store) = h.journal.store();
        let mut counting = CountingIndex {
            inner: &mut store,
            frames: 0,
        };
        let one = state
            .job_settlements(conversation, after, through, 1, &mut counting)
            .unwrap();
        assert_eq!(one.len(), 1);
        assert!(one[0].cancel.is_some() && one[0].settled.is_some());
        assert_eq!(counting.frames, 4);
        counting.frames = 0;
        let notice = state
            .job_notice(conversation, after, &mut counting)
            .unwrap();
        assert_eq!(notice.settlements.len(), MAX_NOTICE_SETTLEMENTS);
        assert!(
            notice
                .settlements
                .iter()
                .all(|s| s.listing.cancel.is_some())
        );
        assert_eq!(counting.frames, 36);
        // Each listing carries its kind 28 header digest, read with the
        // frame: SHA-256 of the kind and the payload bytes, with no second
        // read for it (the 36 frames above).
        for settlement in &notice.settlements {
            let settled = settlement.listing.settled.clone().unwrap();
            let frame = encode_frame(
                &FrameContext {
                    installation_id: [7; 16],
                    transition_id: [8; 16],
                    sequence: 9,
                    prior_digest: [0; 32],
                    expected_revision: 0,
                    owner_generation: 1,
                },
                &Record::JobSettled(settled),
            )
            .unwrap();
            let mut expected = sha2::Sha256::new();
            expected.update(28u16.to_be_bytes());
            expected.update(&frame[144..frame.len() - 40]);
            let expected: Hash = expected.finalize().into();
            assert_eq!(settlement.listing.settled_digest, Some(expected));
        }
    };
    let through = h.state().sequence;
    count(&mut h, 0, through);
    // The same bound once the settled jobs' entries are in index runs.
    let (other, op2, gen2) = h.start(project, None);
    let text = "z".repeat(61_440);
    for n in 0..300u64 {
        h.segment(op2, gen2, n * 61_440, &text).unwrap();
    }
    h.finish(other, op2, gen2, 300 * 61_440);
    assert!(!h.journal.runs().is_empty());
    assert!(
        h.state()
            .delta_tag(Tag::Settlement)
            .iter()
            .all(|(key, _)| key.id != conversation)
    );
    let through = h.state().sequence;
    count(&mut h, 0, through);
}

/// A kind 31 `!` command (`!!` with `outside`) in the harness journal.
fn person31(
    h: &mut Harness,
    project: Id,
    conversation: Id,
    new_conversation: bool,
    outside: bool,
) -> (Record, Id) {
    let request = h.fresh();
    let input = h.fresh();
    let mut p = PersonCommandQueued {
        request,
        digest: [0; 32],
        input,
        project,
        conversation,
        expected_generation: 0,
        new_conversation,
        outside_sandbox: outside,
        command: "cargo test".into(),
        display_command: String::new(),
        cwd: "crates".into(),
        search_path: if outside { "" } else { "/usr/bin" }.into(),
        environment: vec![],
        order_revision: h.state().order_revision + 1,
        number_kinds: vec![],
        unnumbered: 0,
        command_cut: false,
        whole_command: false,
        rule_set_revision: 1,
        rule_set_digest: [9; 32],
        fallback: None,
    };
    p.digest = request_digest(Request::QueuePersonCommand(p.request())).unwrap();
    (Record::PersonCommandQueued(Box::new(p)), input)
}
fn person_intent(input: Id) -> Record {
    Record::JobIntent(JobIntent {
        origin: JobOrigin::Person { input },
        agent: 0,
        time: 1_760_000_000_000,
    })
}

/// P2 (unit): kind 31 inputs and person jobs through the real append path,
/// rolls and a checkpoint. The checkpoint lists each person input with its
/// kind 31 locator and rebuilds the outside flag from that frame; a
/// settled input's `I` entry and its job's `W`, `L` and `U` entries reach
/// the runs; the reopened state equals a genesis replay.
#[test]
fn p2_kind_31_checkpoint_and_index_entries() {
    let mut h = Harness::new();
    let project = h.register(1);
    let lane = h.fresh();
    let (first, a) = person31(&mut h, project, lane, true, false);
    h.append(first).unwrap();
    let (second, b) = person31(&mut h, project, lane, false, true);
    h.append(second).unwrap();
    h.append(person_intent(a)).unwrap();
    let job_a = job_id(&JobOrigin::Person { input: a });
    let job_b = job_id(&JobOrigin::Person { input: b });
    h.append(exited(job_a)).unwrap();
    h.append(person_intent(b)).unwrap();
    let (third, c) = person31(&mut h, project, lane, false, false);
    h.append(third).unwrap();
    // A restart: `c` is held (PR6); `b` keeps running until `interrupted`.
    h.append(Record::OwnerGeneration).unwrap();
    assert_eq!(h.state().input_status(a), Some(InputStatus::Complete));
    assert_eq!(h.state().input_status(b), Some(InputStatus::Running));
    assert_eq!(h.state().input_status(c), Some(InputStatus::Held));
    // Force rolls in another project.
    let other = h.register(2);
    let (_, operation, generation) = h.start(other, None);
    for n in 0..150u64 {
        h.segment(operation, generation, n * 61_440, &"p".repeat(61_440))
            .unwrap();
    }
    assert!(!h.journal.runs().is_empty());
    let files = h.files();
    let replayed = genesis(&files);
    assert_eq!(canonical(&replayed), canonical(h.state()));
    let mut reopened = open(files.clone(), false).unwrap();
    assert_eq!(canonical(reopened.state()), canonical(&replayed));
    {
        let (state, mut store) = reopened.store();
        assert!(state.base > 0);
        assert_eq!(state.input_status(b), Some(InputStatus::Running));
        assert_eq!(state.input_status(c), Some(InputStatus::Held));
        let person = state.inputs[&b].person.unwrap();
        assert!(person.outside_sandbox);
        assert_eq!(person.job, Some(unsettled(&state)[0].intent));
        assert_eq!(unsettled(&state)[0].job, job_b);
        // The settled job and input are answered from the runs.
        assert!(state.delta_tag(Tag::Job).is_empty());
        let Some(Entry::Input { status, queued, .. }) = state
            .entry(&Key::new(Tag::Input, a, 0), &mut store)
            .unwrap()
        else {
            panic!("I entry of the settled person input");
        };
        assert_eq!(status, input_status_code(InputStatus::Complete));
        let records = state.job_records(job_a, &mut store).unwrap().unwrap();
        assert_eq!(records.listing.command, queued);
        assert_eq!(records.listing.excerpt, "cargo test");
        assert_eq!(records.command.person().unwrap().cwd, "crates");
        // Once the settled input leaves the live state, its `I` entry names
        // the kind 31.
        let mut evicted: Replay = (*state).clone();
        evicted.inputs.remove(&a);
        let again = evicted.job_records(job_a, &mut store).unwrap().unwrap();
        assert_eq!(again.listing, records.listing);
        assert_eq!(again.command, records.command);
        assert!(matches!(
            state.job_state(job_a, &mut store).unwrap(),
            Some(JobState::Settled(SettledJob { origin: 2, .. }))
        ));
        let listed = state
            .jobs(JobScope::Conversation(lane), None, 9, &mut store)
            .unwrap();
        assert_eq!(
            listed.iter().map(|l| l.job).collect::<Vec<_>>(),
            vec![job_b, job_a]
        );
        let notice = state.job_notice(lane, 0, &mut store).unwrap();
        assert_eq!((notice.settlements.len(), notice.live), (1, 1));
        assert!(notice.settlements[0].command.person().is_some());
        // The request outcome of a kind 31 decodes from its frame.
        let (Record::PersonCommandQueued(p), _) = (store.frame(&queued).unwrap(), ()) else {
            panic!("kind 31");
        };
        assert_eq!(
            state
                .outcome(p.request, &mut store)
                .unwrap()
                .map(|r| r.result),
            Some(Outcome::QueueInput {
                input: a,
                conversation: lane,
                order_revision: 1,
            })
        );
    }
    // A checkpoint whose person input names a frame that is not its kind
    // 31 is corrupt.
    let mut damaged: Replay = (**reopened.state()).clone();
    damaged.clear_delta();
    let wrong = damaged.inputs[&b].frame;
    damaged.inputs.get_mut(&c).unwrap().frame = wrong;
    let census = checkpoint::Census {
        segments: 0,
        holding: 0,
        bytes: 0,
    };
    let bytes = checkpoint::encode(&damaged, &census, &[]).unwrap();
    let (_, mut store) = reopened.store();
    assert_eq!(
        checkpoint::decode(&bytes, &mut store).unwrap_err(),
        ReplayError::Corrupt
    );
    // The restart settles the running `!!` job `interrupted`.
    let mut writer = open(files, true).unwrap();
    let transition = id(0xfeed);
    writer.append(interrupted(job_b), transition).unwrap();
    assert_eq!(writer.state().input_status(b), Some(InputStatus::Failed));
}

// ----- P5 kind 32 (execution-path-guard.md, EPG-U3). -----

fn path32(
    action: PathAction,
    project: Id,
    reference: Id,
    through: u64,
    entries: &[(PathEntryKind, &str)],
    digest: Hash,
) -> Record {
    Record::ExecutionPath(Box::new(ExecutionPathRecord {
        action,
        project,
        reference,
        through,
        entries: entries
            .iter()
            .map(|(kind, path)| PathEntry {
                kind: *kind,
                path: (*path).into(),
            })
            .collect(),
        overflow: 0,
        digest,
    }))
}
/// EPG-U3 (unit, segmented journal): kind 32 entries move to index runs
/// at a roll; the guard read merges runs and delta; a reopened checkpoint
/// plus tail equals the genesis replay and reads the same guard state.
#[test]
fn epg_u3_kind_32_entries_survive_rolls_and_checkpoints() {
    use PathAction::*;
    use PathEntryKind as K;
    // A normal record: held at the quota like kind 26, never a closure.
    assert!(!is_closure(&path32(
        ModelChanged,
        id(1),
        id(2),
        0,
        &[],
        [0; 32]
    )));
    let mut h = Harness::new();
    let project = h.register(1);
    let (conversation, operation, generation) = h.start(project, None);
    let first = h.job(operation, generation, 1);
    h.append(exited(first)).unwrap();
    h.append(path32(
        ModelChanged,
        project,
        first,
        0,
        &[(K::ModelChanged, "build.rs")],
        [0; 32],
    ))
    .unwrap();
    let first_change = h.state().sequence;
    h.append(path32(
        Approved,
        project,
        id(900),
        0,
        &[(K::Approval, ".envrc")],
        [1; 32],
    ))
    .unwrap();
    h.finish(conversation, operation, generation, 0);
    // Force rolls: the entries above move to an index run.
    let (_, op2, gen2) = h.start(project, Some(conversation));
    let text = "z".repeat(61_440);
    for n in 0..150u64 {
        h.segment(op2, gen2, n * 61_440, &text).unwrap();
    }
    assert!(!h.journal.runs().is_empty());
    let second = h.job(op2, gen2, 1);
    h.append(exited(second)).unwrap();
    h.append(path32(
        ModelChanged,
        project,
        second,
        0,
        &[(K::ModelChanged, "Cargo.toml")],
        [0; 32],
    ))
    .unwrap();
    let second_change = h.state().sequence;
    let lane = h.fresh();
    let (record, input) = person31(&mut h, project, lane, true, false);
    h.append(record).unwrap();
    h.append(path32(
        Acknowledged,
        project,
        input,
        first_change,
        &[(K::ModelChanged, "build.rs")],
        [0; 32],
    ))
    .unwrap();
    h.append(path32(
        Approved,
        project,
        id(904),
        0,
        &[(K::Approval, ".git/hooks/pre-commit")],
        [2; 32],
    ))
    .unwrap();
    // A revocation of the approval held in the run.
    h.append(path32(
        Revoked,
        project,
        id(905),
        0,
        &[(K::Approval, ".envrc")],
        [0; 32],
    ))
    .unwrap();
    let check = |state: &Replay, store: &mut dyn Index| {
        let view = state.path_guard(project, store).unwrap();
        assert_eq!(view.acknowledged.unwrap().through, first_change);
        assert_eq!(view.changes.len(), 1);
        assert_eq!(view.changes[0].frame.sequence, second_change);
        assert_eq!(view.changes[0].paths, vec!["Cargo.toml"]);
        assert_eq!(view.omitted_changes, 0);
        assert_eq!(view.approvals.len(), 1);
        assert_eq!(view.approvals[0].path, ".git/hooks/pre-commit");
        assert_eq!(view.approval_records, 3);
        assert!(state.path_record(first, store).unwrap().is_some());
        view
    };
    let live = {
        let (state, mut store) = h.journal.store();
        // The first change and approval are in a run; the rest in the delta.
        assert_eq!(state.delta_tag(Tag::PathChange).len(), 1);
        assert_eq!(state.delta_tag(Tag::PathApproval).len(), 2);
        check(&state, &mut store)
    };
    // More rolls: everything moves to runs and the checkpoint.
    for n in 150..300u64 {
        h.segment(op2, gen2, n * 61_440, &text).unwrap();
    }
    let files = h.files();
    let mut reopened = open(files.clone(), false).unwrap();
    assert!(reopened.state().base > second_change);
    let replayed = genesis(&files);
    assert_eq!(canonical(reopened.state()), canonical(&replayed));
    {
        let (state, mut store) = reopened.store();
        assert!(state.delta_tag(Tag::PathReference).is_empty());
        assert_eq!(check(&state, &mut store), live);
    }
    let segments = segments(&files);
    let mut index = MemoryIndex {
        segments: segments.iter().map(|(f, b)| (*f, b.as_slice())).collect(),
    };
    assert_eq!(check(&replayed, &mut index), live);
}
