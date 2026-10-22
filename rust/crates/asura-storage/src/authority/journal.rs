//! The segmented journal driver: segment files, rolls, checkpoints, index
//! runs, run compaction and the journal quota ledger. Runs only on the single
//! journal worker (writer) or the inspection worker (read-only). Every file
//! operation goes through [`JournalFiles`]; the platform implements it over
//! the held control directory and tests use [`memory::MemoryFiles`].
use super::checkpoint::{self, Census, Checkpoint, MAX_CHECKPOINT, MAX_RUNS, RunRef};
use super::conversation::*;
use super::index::run::{
    self, FAN_IN, MERGE_STEP_PAGES, Merge, MergeInput, MergeIo, RunHeader, RunSink, RunSource,
    RunWriter,
};
use super::index::{Entry, Key, Tag, Value, sequence_id};
use super::{HEADER, MAX_FRAME, ReplayError, SEGMENT_BYTES, SEGMENT_FRAMES};
use asura_platform::{JournalEntry, JournalError, JournalName, JournalPoint};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

pub mod memory;
#[cfg(test)]
mod tests;

/// Quota bytes held back so one roll and the closure records always complete.
pub const ROLL_RESERVE: u64 = 32 * 1024 * 1024;
/// Free disk space the journal never uses.
pub const DISK_FLOOR: u64 = 16 * 1024 * 1024;
/// `journal.quotaBytes` default (OD-SJ-1): 4 GiB.
pub const DEFAULT_QUOTA: u64 = 4 * 1024 * 1024 * 1024;

/// File operations of the journal. Every method may block; call only on the
/// journal or inspection worker.
pub trait JournalFiles {
    fn census(&mut self) -> Result<Vec<JournalEntry>, JournalError>;
    fn read_at(
        &mut self,
        name: JournalName,
        offset: u64,
        buffer: &mut [u8],
    ) -> Result<(), JournalError>;
    fn create(&mut self, name: JournalName) -> Result<(), JournalError>;
    fn write_at(
        &mut self,
        name: JournalName,
        offset: u64,
        bytes: &[u8],
    ) -> Result<(), JournalError>;
    fn sync(&mut self, name: JournalName) -> Result<(), JournalError>;
    fn append(&mut self, segment: u64, expected: u64, bytes: &[u8]) -> Result<(), JournalError>;
    fn publish(&mut self, from: JournalName, to: JournalName) -> Result<(), JournalError>;
    fn remove(&mut self, name: JournalName) -> Result<(), JournalError>;
    fn free_bytes(&mut self) -> Result<u64, JournalError>;
    fn point(&mut self, point: JournalPoint) -> Result<(), JournalError>;
}
impl JournalFiles for asura_platform::JournalDirectory {
    fn census(&mut self) -> Result<Vec<JournalEntry>, JournalError> {
        asura_platform::JournalDirectory::census(self)
    }
    fn read_at(
        &mut self,
        name: JournalName,
        offset: u64,
        buffer: &mut [u8],
    ) -> Result<(), JournalError> {
        asura_platform::JournalDirectory::read_at(self, name, offset, buffer)
    }
    fn create(&mut self, name: JournalName) -> Result<(), JournalError> {
        asura_platform::JournalDirectory::create(self, name)
    }
    fn write_at(
        &mut self,
        name: JournalName,
        offset: u64,
        bytes: &[u8],
    ) -> Result<(), JournalError> {
        asura_platform::JournalDirectory::write_at(self, name, offset, bytes)
    }
    fn sync(&mut self, name: JournalName) -> Result<(), JournalError> {
        asura_platform::JournalDirectory::sync(self, name)
    }
    fn append(&mut self, segment: u64, expected: u64, bytes: &[u8]) -> Result<(), JournalError> {
        asura_platform::JournalDirectory::append(self, segment, expected, bytes)
    }
    fn publish(&mut self, from: JournalName, to: JournalName) -> Result<(), JournalError> {
        asura_platform::JournalDirectory::publish(self, from, to)
    }
    fn remove(&mut self, name: JournalName) -> Result<(), JournalError> {
        asura_platform::JournalDirectory::remove(self, name)
    }
    fn free_bytes(&mut self) -> Result<u64, JournalError> {
        asura_platform::JournalDirectory::free_bytes(self)
    }
    fn point(&mut self, point: JournalPoint) -> Result<(), JournalError> {
        asura_platform::JournalDirectory::point(self, point)
    }
}

/// Why normal records are held.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HoldReason {
    /// The configured `journal.quotaBytes` is reached: `journal_quota`.
    Quota,
    /// Free disk space reached the 16 MiB floor: `journal_disk_space`.
    Disk,
}
/// Journal failures, mapped to person-visible reasons by the service.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Failure {
    /// Unknown names, or `slot-0.log`: `unsupported_layout`.
    UnsupportedLayout,
    /// Unsafe file type, owner, mode, link or ACL: `unsafe_authority`.
    Unsafe,
    /// Control files without segment file 1: `installation_remnants`.
    Remnants,
    /// The read-only scan found no usable checkpoint for a long tail.
    RebuildPending,
    /// Another writer holds the lock.
    OwnerBusy,
    Cancelled,
    Io,
    /// Frame, checkpoint or run content failure.
    Replay(ReplayError),
    /// The transition is invalid; nothing was written.
    Invalid,
    /// Normal records are held; nothing was written.
    Held(HoldReason),
    /// A closure record does not fit: `capacity_exhausted`.
    Exhausted,
    /// A write or sync error left the outcome unknown; the journal is fenced.
    Unconfirmed,
}
fn from_file(error: JournalError) -> Failure {
    match error {
        JournalError::UnsupportedLayout => Failure::UnsupportedLayout,
        JournalError::Unsafe => Failure::Unsafe,
        JournalError::OwnerBusy => Failure::OwnerBusy,
        JournalError::Cancelled => Failure::Cancelled,
        JournalError::Absent => Failure::Replay(ReplayError::Corrupt),
        JournalError::NoSpace { .. } => Failure::Held(HoldReason::Disk),
        JournalError::Limit | JournalError::Io => Failure::Io,
    }
}
fn content(error: JournalError) -> ReplayError {
    match error {
        JournalError::Cancelled => ReplayError::Cancelled,
        _ => ReplayError::Corrupt,
    }
}
impl From<ReplayError> for Failure {
    fn from(error: ReplayError) -> Self {
        Self::Replay(error)
    }
}

/// Journal usage and quota for status displays.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct JournalStatus {
    pub usage: u64,
    pub effective_quota: u64,
    pub configured_quota: u64,
    pub held: Option<HoldReason>,
    pub runs: usize,
    pub segments: u64,
    /// Rolls and completed run merges since the writer opened (for logs).
    pub rolls: u64,
    pub merges: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Segment {
    first: u64,
    /// Last frame sequence; `first - 1` when empty.
    last: u64,
    bytes: u64,
    frames: u64,
    last_digest: Hash,
}

/// Index reads over the listed runs and verified frame reads.
pub struct Store<'a, F: JournalFiles> {
    files: &'a mut F,
    runs: &'a [RunRef],
    installation: Id,
}
struct FileRun<'a, F: JournalFiles> {
    files: &'a mut F,
    name: JournalName,
}
impl<F: JournalFiles> RunSource for FileRun<'_, F> {
    fn read_exact_at(&mut self, offset: u64, buffer: &mut [u8]) -> Result<(), ReplayError> {
        self.files
            .read_at(self.name, offset, buffer)
            .map_err(content)
    }
}
fn run_name(run: &RunRef) -> JournalName {
    JournalName::Run(run.first, run.last)
}
impl<F: JournalFiles> Store<'_, F> {
    fn run_header(&mut self, run: &RunRef) -> Result<RunHeader, ReplayError> {
        let mut source = FileRun {
            files: &mut *self.files,
            name: run_name(run),
        };
        let header = RunHeader::read(&mut source)?;
        if header.level != run.level
            || header.first != run.first
            || header.last != run.last
            || header.entries != run.entries
            || header.file_bytes() != run.bytes
            || header.installation_id != self.installation
        {
            return Err(ReplayError::Corrupt);
        }
        Ok(header)
    }
    /// Reads one frame through a locator and checks it.
    pub fn read_frame(
        &mut self,
        locator: &Locator,
    ) -> Result<(FrameContext, Record, Hash), ReplayError> {
        if locator.length as usize > MAX_FRAME || locator.length < (HEADER + 40) as u32 {
            return Err(ReplayError::Corrupt);
        }
        let mut bytes = vec![0; locator.length as usize];
        self.files
            .read_at(
                JournalName::Segment(locator.segment),
                u64::from(locator.offset),
                &mut bytes,
            )
            .map_err(content)?;
        let (context, record, digest) = super::verified_frame(&bytes, locator)?;
        if context.installation_id != self.installation {
            return Err(ReplayError::Corrupt);
        }
        Ok((context, record, digest))
    }
}
impl<F: JournalFiles> Index for Store<'_, F> {
    fn get(&mut self, key: &Key) -> Result<Option<Value>, ReplayError> {
        if self.runs.is_empty() {
            return Ok(None);
        }
        self.files.point(JournalPoint::Lookup).map_err(content)?;
        for run in self.runs.iter().rev() {
            let header = self.run_header(run)?;
            let mut source = FileRun {
                files: &mut *self.files,
                name: run_name(run),
            };
            if let Some(value) = run::get(&mut source, &header, key)? {
                return Ok(Some(value));
            }
        }
        Ok(None)
    }
    fn range(
        &mut self,
        lo: &Key,
        hi: &Key,
        descending: bool,
        limit: usize,
    ) -> Result<Vec<(Key, Value)>, ReplayError> {
        if self.runs.is_empty() {
            return Ok(Vec::new());
        }
        self.files.point(JournalPoint::Lookup).map_err(content)?;
        let mut out = Vec::new();
        for run in self.runs {
            let header = self.run_header(run)?;
            let mut source = FileRun {
                files: &mut *self.files,
                name: run_name(run),
            };
            out.extend(run::range(&mut source, &header, lo, hi, descending, limit)?);
        }
        out.sort_by_key(|(key, _)| *key);
        if out.windows(2).any(|pair| pair[0].0 == pair[1].0) {
            return Err(ReplayError::Corrupt);
        }
        if descending {
            out.reverse();
        }
        out.truncate(limit);
        Ok(out)
    }
    fn frame(&mut self, locator: &Locator) -> Result<Record, ReplayError> {
        Ok(self.read_frame(locator)?.1)
    }
    fn frame_owner(&mut self, locator: &Locator) -> Result<(u64, Record), ReplayError> {
        let (context, record, _) = self.read_frame(locator)?;
        Ok((context.owner_generation, record))
    }
}

/// An in-progress run compaction.
struct MergeJob {
    inputs: Vec<RunRef>,
    output: (u64, u64),
    level: u8,
    merge: Merge,
}
struct FilesMerge<'a, F: JournalFiles> {
    files: &'a mut F,
    inputs: Vec<JournalName>,
    output: JournalName,
}
impl<F: JournalFiles> MergeIo for FilesMerge<'_, F> {
    fn read_input(
        &mut self,
        input: usize,
        offset: u64,
        buffer: &mut [u8],
    ) -> Result<(), ReplayError> {
        self.files
            .read_at(self.inputs[input], offset, buffer)
            .map_err(content)
    }
    fn write_output(&mut self, offset: u64, bytes: &[u8]) -> Result<(), ReplayError> {
        self.files
            .write_at(self.output, offset, bytes)
            .map_err(|_| ReplayError::Limit)
    }
}
struct FilesSink<'a, F: JournalFiles> {
    files: &'a mut F,
    name: JournalName,
    failure: Option<JournalError>,
}
impl<F: JournalFiles> RunSink for FilesSink<'_, F> {
    fn write_all_at(&mut self, offset: u64, bytes: &[u8]) -> Result<(), ReplayError> {
        self.files
            .write_at(self.name, offset, bytes)
            .map_err(|error| {
                self.failure = Some(error);
                ReplayError::Limit
            })
    }
}

/// The open segmented journal.
pub struct Journal<F: JournalFiles> {
    files: F,
    writer: bool,
    state: Arc<Replay>,
    segments: Vec<Segment>,
    runs: Vec<RunRef>,
    /// Valid checkpoints the writer keeps, oldest first, with their run lists.
    checkpoints: BTreeMap<u64, Vec<RunRef>>,
    /// Derived files on disk and their sizes.
    derived: BTreeMap<JournalName, u64>,
    /// The newest checkpoint sealed the active segment: the next frame goes
    /// to a new segment file (SJ-F05).
    sealed: bool,
    configured_quota: u64,
    free: u64,
    held: Option<HoldReason>,
    merge: Option<MergeJob>,
    /// A complete merge output not yet listed by a checkpoint, and its inputs.
    merged: Option<(RunRef, Vec<RunRef>)>,
    fenced: bool,
    installation: Id,
    rolls: u64,
    merges: u64,
}

/// Result of opening a control directory.
pub enum Opened<F: JournalFiles> {
    /// No journal file exists (lock only or empty): a runtime without authority.
    Empty(F),
    Journal(Box<Journal<F>>),
}

/// Reads the frame at `offset` of a segment of `size` bytes. `None` at end
/// of file. A partial frame is `IncompleteTail`.
fn read_next(
    files: &mut impl JournalFiles,
    segment: u64,
    offset: u64,
    size: u64,
) -> Result<Option<(Vec<u8>, Locator)>, ReplayError> {
    if offset == size {
        return Ok(None);
    }
    let remaining = size - offset;
    let mut head = vec![0; (HEADER as u64).min(remaining) as usize];
    files
        .read_at(JournalName::Segment(segment), offset, &mut head)
        .map_err(content)?;
    if head.len() < 16 {
        return Err(ReplayError::IncompleteTail);
    }
    let length = u32::from_be_bytes(head[12..16].try_into().expect("4")) as u64;
    if length > MAX_FRAME as u64 || length < (HEADER + 40) as u64 {
        return Err(ReplayError::Corrupt);
    }
    if remaining < length {
        return Err(ReplayError::IncompleteTail);
    }
    let mut frame = vec![0; length as usize];
    files
        .read_at(JournalName::Segment(segment), offset, &mut frame)
        .map_err(content)?;
    let sequence = u64::from_be_bytes(frame[16..24].try_into().expect("8"));
    Ok(Some((
        frame,
        Locator {
            sequence,
            segment,
            offset: offset as u32,
            length: length as u32,
        },
    )))
}

impl<F: JournalFiles> Journal<F> {
    /// Opens the journal: census, newest valid checkpoint, then the tail. A
    /// writer removes tmp and unlisted derived files and rebuilds checkpoints
    /// for a long tail; the read-only scan never writes and reports
    /// [`Failure::RebuildPending`] instead.
    pub fn open(mut files: F, writer: bool, cancel: &AtomicBool) -> Result<Opened<F>, Failure> {
        let active = |cancel: &AtomicBool| {
            if cancel.load(Ordering::Acquire) {
                Err(Failure::Cancelled)
            } else {
                Ok(())
            }
        };
        active(cancel)?;
        let entries = files.census().map_err(from_file)?;
        let mut segments = Vec::new();
        let mut derived = BTreeMap::new();
        for entry in &entries {
            match entry.name {
                JournalName::Segment(first) => segments.push((first, entry.bytes)),
                JournalName::Lock => {}
                name => {
                    derived.insert(name, entry.bytes);
                }
            }
        }
        if segments.is_empty() {
            return if derived.is_empty() {
                Ok(Opened::Empty(files))
            } else {
                Err(Failure::Remnants)
            };
        }
        if segments[0].0 != 1 {
            return Err(Failure::Remnants);
        }
        if writer {
            let tmps: Vec<_> = derived.keys().filter(|n| n.is_tmp()).copied().collect();
            for name in tmps {
                files.remove(name).map_err(from_file)?;
                derived.remove(&name);
            }
        }
        // Installation ID from the first frame of segment file 1.
        let installation = {
            let (bytes, locator) = read_next(&mut files, 1, 0, segments[0].1)?
                .ok_or(Failure::Replay(ReplayError::Empty))?;
            let (context, record, _) = super::verified_frame(&bytes, &locator)?;
            if locator.sequence != 1 || !matches!(record, Record::PendingInit(_)) {
                return Err(Failure::Replay(ReplayError::UnsupportedFormat));
            }
            context.installation_id
        };
        let mut journal = Journal {
            files,
            writer,
            state: Arc::new(Replay::empty(
                installation,
                PendingInit {
                    request: [0; 16],
                    digest: [0; 32],
                    mode: 0,
                    configuration_revision: 0,
                    configuration_digest: [0; 32],
                    graph: [0; 16],
                },
            )),
            segments: Vec::new(),
            runs: Vec::new(),
            checkpoints: BTreeMap::new(),
            derived,
            sealed: false,
            configured_quota: DEFAULT_QUOTA,
            free: 0,
            held: None,
            merge: None,
            merged: None,
            fenced: false,
            installation,
            rolls: 0,
            merges: 0,
        };
        // Newest valid checkpoint; never chosen by name alone.
        let candidates: Vec<u64> = journal
            .derived
            .keys()
            .filter_map(|n| match n {
                JournalName::Checkpoint(s) => Some(*s),
                _ => None,
            })
            .rev()
            .collect();
        let mut base: Option<Checkpoint> = None;
        for sequence in candidates {
            active(cancel)?;
            match journal.select(sequence, &segments) {
                Ok(found) => {
                    base = Some(found);
                    break;
                }
                Err(ReplayError::Cancelled) => return Err(Failure::Cancelled),
                Err(_) => continue,
            }
        }
        let head = base.as_ref().map_or(0, |c| c.state.sequence);
        // Segments that hold frames after the base.
        // A checkpoint is written at a segment end, so the tail is the
        // segment files after the head.
        let tail: Vec<(u64, u64)> = segments
            .iter()
            .filter(|(f, _)| *f > head)
            .copied()
            .collect();
        if tail.len() > 2 && !writer {
            return Err(Failure::RebuildPending);
        }
        let rebuild = tail.len() > 2;
        if let Some(found) = base {
            journal.runs = found.runs.clone();
            journal
                .checkpoints
                .insert(found.state.sequence, found.runs.clone());
            journal.state = Arc::new(found.state);
            // Every segment up to the head is sealed.
            for (first, bytes) in segments.iter().filter(|(f, _)| *f <= head) {
                journal.segments.push(Segment {
                    first: *first,
                    last: 0,
                    bytes: *bytes,
                    frames: 0,
                    last_digest: [0; 32],
                });
            }
            let holding = journal
                .segments
                .last_mut()
                .ok_or(Failure::Replay(ReplayError::Corrupt))?;
            let at = journal.state.last_frame;
            // The head frame ends its segment file.
            if at.segment != holding.first
                || u64::from(at.offset) + u64::from(at.length) != holding.bytes
            {
                return Err(Failure::Replay(ReplayError::Corrupt));
            }
            holding.last = head;
            holding.last_digest = journal.state.last_digest;
            journal.sealed = true;
        }
        // The tail, frame by frame.
        let mut state: Option<Replay> = (head > 0).then(|| (*journal.state).clone());
        let newest = segments.last().map(|(f, _)| *f).unwrap_or(1);
        for (position, &(first, size)) in tail.iter().enumerate() {
            active(cancel)?;
            let mut offset = 0;
            let mut segment = Segment {
                first,
                last: first - 1,
                bytes: size,
                frames: 0,
                last_digest: [0; 32],
            };
            loop {
                active(cancel)?;
                let next = match read_next(&mut journal.files, first, offset, size) {
                    Ok(next) => next,
                    Err(ReplayError::IncompleteTail) if first != newest => {
                        return Err(Failure::Replay(ReplayError::Corrupt));
                    }
                    Err(error) => return Err(error.into()),
                };
                let Some((bytes, locator)) = next else {
                    break;
                };
                let envelope = super::envelope(&bytes)?;
                if segment.frames == 0 && offset == 0 && locator.sequence != first {
                    return Err(Failure::Replay(ReplayError::Corrupt));
                }
                let record = decode_record(envelope.kind, envelope.payload)?;
                let mut store = Store {
                    files: &mut journal.files,
                    runs: &journal.runs,
                    installation,
                };
                state = Some(apply_next(
                    state.take(),
                    &envelope.context,
                    record,
                    envelope.digest,
                    locator,
                    &mut store,
                )?);
                segment.last = locator.sequence;
                segment.frames += 1;
                segment.last_digest = envelope.digest;
                offset += u64::from(locator.length);
            }
            let current = state.as_ref().ok_or(Failure::Replay(ReplayError::Empty))?;
            if segment.frames == 0 && offset == 0 {
                // Only the newest segment may be empty, named head plus 1.
                if first != newest || first != current.sequence + 1 {
                    return Err(Failure::Replay(ReplayError::Corrupt));
                }
            } else if first != newest && !segments.iter().any(|(f, _)| *f == segment.last + 1) {
                return Err(Failure::Replay(ReplayError::Corrupt));
            }
            journal.state = Arc::new(current.clone());
            journal.segments.push(segment);
            journal.sealed = false;
            // Rebuild: roll at each sealed segment end, without creating files.
            if rebuild && position + 1 < tail.len() && segment.frames > 0 {
                journal.roll_derived()?;
                state = Some((*journal.state).clone());
            }
        }
        // Census: every segment file is accounted for, in order.
        let census: Vec<u64> = journal.segments.iter().map(|s| s.first).collect();
        if census != segments.iter().map(|(f, _)| *f).collect::<Vec<_>>() {
            return Err(Failure::Replay(ReplayError::Corrupt));
        }
        for pair in journal.segments.windows(2) {
            if pair[0].frames > 0 && pair[1].first != pair[0].last + 1 {
                return Err(Failure::Replay(ReplayError::Corrupt));
            }
        }
        if journal.sealed
            && journal
                .segments
                .last()
                .is_some_and(|s| s.last != journal.state.sequence)
        {
            return Err(Failure::Replay(ReplayError::Corrupt));
        }
        active(cancel)?;
        if writer {
            journal.free = journal.files.free_bytes().map_err(from_file)?;
            journal.retire()?;
        }
        Ok(Opened::Journal(Box::new(journal)))
    }
    /// Creates the journal: segment file 1 holding the kind 10 frame.
    /// `files` must hold no journal file yet.
    pub fn initialize(
        mut files: F,
        context: &FrameContext,
        init: PendingInit,
    ) -> Result<Self, Failure> {
        let record = Record::PendingInit(init.clone());
        let frame = encode_frame(context, &record).map_err(|_| Failure::Invalid)?;
        let digest: Hash = frame[frame.len() - 40..frame.len() - 8]
            .try_into()
            .expect("32");
        let locator = Locator {
            sequence: 1,
            segment: 1,
            offset: 0,
            length: frame.len() as u32,
        };
        let state = apply_next(
            None,
            context,
            record,
            digest,
            locator,
            &mut MemoryIndex {
                segments: BTreeMap::from([(1, frame.as_slice())]),
            },
        )?;
        files.create(JournalName::Segment(1)).map_err(from_file)?;
        let mut journal = Journal {
            files,
            writer: true,
            state: Arc::new(state),
            segments: vec![Segment {
                first: 1,
                last: 0,
                bytes: 0,
                frames: 0,
                last_digest: [0; 32],
            }],
            runs: Vec::new(),
            checkpoints: BTreeMap::new(),
            derived: BTreeMap::new(),
            sealed: false,
            configured_quota: DEFAULT_QUOTA,
            free: 0,
            held: None,
            merge: None,
            merged: None,
            fenced: false,
            installation: context.installation_id,
            rolls: 0,
            merges: 0,
        };
        journal.free = journal.files.free_bytes().map_err(from_file)?;
        if let Err(error) = journal.files.append(1, 0, &frame) {
            journal.fenced = true;
            return Err(match error {
                JournalError::NoSpace { unchanged: true } => Failure::Held(HoldReason::Disk),
                _ => Failure::Unconfirmed,
            });
        }
        let segment = &mut journal.segments[0];
        segment.bytes = frame.len() as u64;
        segment.frames = 1;
        segment.last = 1;
        segment.last_digest = digest;
        Ok(journal)
    }
    /// The checkpoint selection checks (newest valid wins).
    fn select(
        &mut self,
        sequence: u64,
        segments: &[(u64, u64)],
    ) -> Result<Checkpoint, ReplayError> {
        let name = JournalName::Checkpoint(sequence);
        let size = *self.derived.get(&name).ok_or(ReplayError::Corrupt)?;
        if size as usize > MAX_CHECKPOINT {
            return Err(ReplayError::Corrupt);
        }
        let mut bytes = vec![0; size as usize];
        self.files.read_at(name, 0, &mut bytes).map_err(content)?;
        // Runs listed by the checkpoint, read before decoding (tool frames).
        let runs = checkpoint_runs(&bytes)?;
        for run in &runs {
            if self.derived.get(&run_name(run)) != Some(&run.bytes) {
                return Err(ReplayError::Corrupt);
            }
        }
        let mut store = Store {
            files: &mut self.files,
            runs: &runs,
            installation: self.installation,
        };
        for run in &runs {
            store.run_header(run)?;
        }
        let found = checkpoint::decode(&bytes, &mut store)?;
        if found.state.installation_id != self.installation || found.state.sequence != sequence {
            return Err(ReplayError::Corrupt);
        }
        let covered: Vec<_> = segments.iter().filter(|(f, _)| *f <= sequence).collect();
        if found.census.segments != covered.len() as u64
            || covered.last().map(|(f, _)| *f) != Some(found.census.holding)
            || found.census.bytes != covered.iter().map(|(_, b)| *b).sum::<u64>()
            || found.state.last_frame.segment != found.census.holding
        {
            return Err(ReplayError::Corrupt);
        }
        let (_, _, digest) = store.read_frame(&found.state.last_frame)?;
        if digest != found.state.last_digest {
            return Err(ReplayError::Corrupt);
        }
        Ok(found)
    }

    pub fn state(&self) -> &Arc<Replay> {
        &self.state
    }
    pub fn installation(&self) -> Id {
        self.installation
    }
    pub fn fenced(&self) -> bool {
        self.fenced
    }
    pub fn files_mut(&mut self) -> &mut F {
        &mut self.files
    }
    /// Index reads for the current state.
    pub fn store(&mut self) -> (Arc<Replay>, Store<'_, F>) {
        (
            self.state.clone(),
            Store {
                files: &mut self.files,
                runs: &self.runs,
                installation: self.installation,
            },
        )
    }
    /// Test support: lowers the single-response piece bound (SJ-30).
    #[cfg(any(test, feature = "test-support"))]
    pub fn set_piece_limit_for_tests(&mut self, limit: usize) {
        let mut state = (*self.state).clone();
        state.set_piece_limit_for_tests(limit);
        self.state = Arc::new(state);
    }
    pub fn runs(&self) -> &[RunRef] {
        &self.runs
    }
    pub fn segment_count(&self) -> usize {
        self.segments.len()
    }

    // ----- Quota ledger. -----

    pub fn set_quota(&mut self, quota: u64) {
        self.configured_quota = quota;
    }
    /// Journal bytes: sealed segments, the active segment as 8 MiB, and every
    /// derived file including tmp files.
    pub fn usage(&self) -> u64 {
        let segments: u64 = self
            .segments
            .iter()
            .enumerate()
            .map(|(i, s)| {
                if i + 1 == self.segments.len() && !self.sealed {
                    SEGMENT_BYTES as u64
                } else {
                    s.bytes
                }
            })
            .sum();
        segments + self.derived.values().sum::<u64>()
    }
    pub fn effective_quota(&self) -> u64 {
        self.configured_quota
            .min((self.usage() + self.free).saturating_sub(DISK_FLOOR))
    }
    pub fn status(&self) -> JournalStatus {
        JournalStatus {
            usage: self.usage(),
            effective_quota: self.effective_quota(),
            configured_quota: self.configured_quota,
            held: self.held,
            runs: self.runs.len(),
            segments: self.segments.len() as u64,
            rolls: self.rolls,
            merges: self.merges,
        }
    }
    pub fn held(&self) -> Option<HoldReason> {
        self.held
    }
    fn reason(&self) -> HoldReason {
        if self.configured_quota <= (self.usage() + self.free).saturating_sub(DISK_FLOOR) {
            HoldReason::Quota
        } else {
            HoldReason::Disk
        }
    }
    /// Whether a normal record that needs `needed` bytes fits beside the
    /// roll reserve and the closure reservation.
    fn normal_fits(&self, needed: u64) -> bool {
        self.fits_with(needed, self.state.reserved_bytes)
    }
    /// The closure reservation is the one the state holds after the record:
    /// a `run` kind 14 reserves its kind 15 and kind 23 (TCR-D16).
    fn fits_with(&self, needed: u64, reserved: usize) -> bool {
        self.usage() + needed + ROLL_RESERVE + reserved as u64 <= self.effective_quota()
    }
    /// Reads free space again and leaves the held state when a normal
    /// record and the reserves fit. Returns true when not held.
    pub fn recheck(&mut self) -> bool {
        if let Ok(free) = self.files.free_bytes() {
            self.free = free;
        }
        if self.held.is_some() && self.normal_fits(ROLL_RESERVE) && self.runs.len() < MAX_RUNS {
            self.held = None;
        }
        self.held.is_none()
    }

    // ----- Appends and rolls. -----

    fn active(&self) -> Option<&Segment> {
        self.segments.last().filter(|_| !self.sealed)
    }
    /// Appends one record. Every append is durable before it returns `Ok`.
    pub fn append(&mut self, record: Record, transition: Id) -> Result<(), Failure> {
        if self.fenced || !self.writer {
            return Err(Failure::Unconfirmed);
        }
        let closure = is_closure(&record);
        let context = FrameContext {
            installation_id: self.installation,
            transition_id: transition,
            sequence: self
                .state
                .sequence
                .checked_add(1)
                .ok_or(Failure::Exhausted)?,
            prior_digest: self.state.last_digest,
            expected_revision: self.state.revision,
            owner_generation: self
                .state
                .owner_generation
                .checked_add(u64::from(matches!(record, Record::OwnerGeneration)))
                .ok_or(Failure::Exhausted)?,
        };
        let frame = encode_frame(&context, &record).map_err(|_| Failure::Invalid)?;
        let digest: Hash = frame[frame.len() - 40..frame.len() - 8]
            .try_into()
            .expect("32");
        let mut roll = match self.active() {
            Some(segment) => {
                segment.bytes + frame.len() as u64 > SEGMENT_BYTES as u64
                    || segment.frames >= SEGMENT_FRAMES as u64
            }
            None => true,
        };
        let mut proposed = self.propose(&context, &record, digest, frame.len(), roll)?;
        if !roll && proposed.delta_charge() > DELTA_CHARGE && self.state.delta_charge() > 0 {
            roll = true;
            proposed = self.propose(&context, &record, digest, frame.len(), roll)?;
        }
        let needed = if roll { ROLL_RESERVE } else { 0 };
        if closure {
            if self.usage() + needed > self.effective_quota() {
                return Err(Failure::Exhausted);
            }
        } else if self.held.is_some()
            || !self.fits_with(
                needed,
                proposed.reserved_bytes.max(self.state.reserved_bytes),
            )
            || (roll && self.state.base < self.state.sequence && self.runs.len() >= MAX_RUNS)
        {
            let reason = if self.runs.len() >= MAX_RUNS {
                HoldReason::Quota
            } else {
                self.reason()
            };
            self.held = Some(reason);
            return Err(Failure::Held(reason));
        }
        if roll {
            self.roll()?;
            proposed = self.propose(&context, &record, digest, frame.len(), false)?;
        }
        let segment = *self.active().ok_or(Failure::Unconfirmed)?;
        match self.files.append(segment.first, segment.bytes, &frame) {
            Ok(()) => {}
            Err(JournalError::NoSpace { unchanged: true }) => {
                // No byte of the frame is in the file (SJ-F16).
                self.held = Some(HoldReason::Disk);
                return Err(Failure::Held(HoldReason::Disk));
            }
            Err(_) => {
                self.fenced = true;
                return Err(Failure::Unconfirmed);
            }
        }
        let active = self.segments.last_mut().expect("active segment");
        active.bytes += frame.len() as u64;
        active.frames += 1;
        active.last = context.sequence;
        active.last_digest = digest;
        self.state = Arc::new(proposed);
        // Roll now when the delta is full, so the next append does not wait.
        if self.state.delta_charge() > DELTA_CHARGE {
            match self.roll() {
                Ok(()) | Err(Failure::Held(_)) => {}
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }
    /// The state after `record` at the next locator, on a copy.
    fn propose(
        &mut self,
        context: &FrameContext,
        record: &Record,
        digest: Hash,
        length: usize,
        roll: bool,
    ) -> Result<Replay, Failure> {
        let locator = match (roll, self.active()) {
            (false, Some(segment)) => Locator {
                sequence: context.sequence,
                segment: segment.first,
                offset: segment.bytes as u32,
                length: length as u32,
            },
            _ => Locator {
                sequence: context.sequence,
                segment: context.sequence,
                offset: 0,
                length: length as u32,
            },
        };
        let mut proposed = (*self.state).clone();
        let mut store = Store {
            files: &mut self.files,
            runs: &self.runs,
            installation: self.installation,
        };
        match proposed.apply_frame(context, record.clone(), digest, locator, &mut store) {
            Ok(()) => Ok(proposed),
            Err(ReplayError::Limit) => Err(Failure::Exhausted),
            Err(ReplayError::Cancelled) => Err(Failure::Cancelled),
            Err(_) => Err(Failure::Invalid),
        }
    }
    /// Roll steps 1 and 2: the index run of the delta and the checkpoint at
    /// the head. Skipped when a checkpoint at the head exists.
    fn roll_derived(&mut self) -> Result<(), Failure> {
        let head = self.state.sequence;
        if self.state.base == head && self.checkpoints.contains_key(&head) {
            return Ok(());
        }
        let first = self.state.base + 1;
        // Step 1: the run.
        let mut entries = self.state.delta_entries();
        for segment in self
            .segments
            .iter()
            .filter(|s| s.frames > 0 && s.last <= head && s.last >= first)
        {
            entries.push((
                Key::new(Tag::Segment, sequence_id(segment.first), 0),
                Entry::Segment {
                    last: segment.last,
                    bytes: segment.bytes,
                    digest: segment.last_digest,
                }
                .encode(),
            ));
        }
        entries.sort_by_key(|(key, _)| *key);
        let run = self.write_run(0, first, head, &entries)?;
        // Step 2: the checkpoint.
        let mut runs = self.runs.clone();
        if let Some((output, inputs)) = self.merged.take() {
            runs.retain(|r| !inputs.contains(r));
            runs.push(output);
        }
        runs.push(run);
        runs.sort_by_key(|r| r.first);
        let mut cleared = (*self.state).clone();
        cleared.clear_delta();
        let covered: Vec<_> = self.segments.iter().filter(|s| s.first <= head).collect();
        let census = Census {
            segments: covered.len() as u64,
            holding: covered.last().map_or(0, |s| s.first),
            bytes: covered.iter().map(|s| s.bytes).sum(),
        };
        let bytes = checkpoint::encode(&cleared, &census, &runs)?;
        let tmp = JournalName::CheckpointTmp(head);
        self.write_file(tmp, &bytes, JournalPoint::CheckpointSync)?;
        self.files
            .publish(tmp, JournalName::Checkpoint(head))
            .map_err(|e| self.fence_file(e))?;
        self.derived.remove(&tmp);
        self.derived
            .insert(JournalName::Checkpoint(head), bytes.len() as u64);
        self.files
            .point(JournalPoint::CheckpointRenamed)
            .map_err(from_file)?;
        self.checkpoints.insert(head, runs.clone());
        self.runs = runs;
        self.rolls += 1;
        self.state = Arc::new(cleared);
        self.sealed = true;
        Ok(())
    }
    /// One complete roll: run, checkpoint, next segment, retire.
    fn roll(&mut self) -> Result<(), Failure> {
        self.free = self.files.free_bytes().map_err(from_file)?;
        self.roll_derived()?;
        let next = self.state.sequence + 1;
        self.files
            .point(JournalPoint::SegmentCreate)
            .map_err(from_file)?;
        if !self.segments.iter().any(|s| s.first == next) {
            self.files
                .create(JournalName::Segment(next))
                .map_err(|e| self.fence_file(e))?;
            self.segments.push(Segment {
                first: next,
                last: next - 1,
                bytes: 0,
                frames: 0,
                last_digest: [0; 32],
            });
        }
        self.sealed = false;
        self.files
            .point(JournalPoint::SegmentCreated)
            .map_err(from_file)?;
        self.retire()
    }
    fn fence_file(&mut self, error: JournalError) -> Failure {
        match error {
            JournalError::NoSpace { .. } => {
                self.held = Some(HoldReason::Disk);
                Failure::Held(HoldReason::Disk)
            }
            JournalError::Cancelled => Failure::Cancelled,
            _ => {
                self.fenced = true;
                Failure::Unconfirmed
            }
        }
    }
    /// Writes a derived tmp file and makes it durable. `ENOSPC` removes the
    /// tmp file and holds normal records.
    fn write_file(
        &mut self,
        tmp: JournalName,
        bytes: &[u8],
        point: JournalPoint,
    ) -> Result<(), Failure> {
        self.files.create(tmp).map_err(|e| self.fence_file(e))?;
        self.derived.insert(tmp, bytes.len() as u64);
        let written = self.files.write_at(tmp, 0, bytes);
        if let Err(error) = written {
            let _ = self.files.remove(tmp);
            self.derived.remove(&tmp);
            return Err(self.fence_file(error));
        }
        self.files.point(point).map_err(from_file)?;
        self.files.sync(tmp).map_err(|e| self.fence_file(e))
    }
    fn write_run(
        &mut self,
        level: u8,
        first: u64,
        last: u64,
        entries: &[(Key, Value)],
    ) -> Result<RunRef, Failure> {
        let header = RunHeader::new(level, self.installation, first, last, entries.len() as u64);
        let tmp = JournalName::RunTmp(first, last);
        self.files.create(tmp).map_err(|e| self.fence_file(e))?;
        self.derived.insert(tmp, header.file_bytes());
        let mut sink = FilesSink {
            files: &mut self.files,
            name: tmp,
            failure: None,
        };
        let written = (|| {
            let mut writer = RunWriter::new(header, &mut sink)?;
            for (key, value) in entries {
                writer.push(*key, *value, &mut sink)?;
            }
            writer.finish(&mut sink)
        })();
        let failure = sink.failure;
        let digest = match written {
            Ok(digest) => digest,
            Err(error) => {
                let _ = self.files.remove(tmp);
                self.derived.remove(&tmp);
                return Err(match failure {
                    Some(error) => self.fence_file(error),
                    None => Failure::Replay(error),
                });
            }
        };
        self.files
            .point(JournalPoint::RunWrite)
            .map_err(from_file)?;
        self.files.sync(tmp).map_err(|e| self.fence_file(e))?;
        let name = JournalName::Run(first, last);
        self.files
            .publish(tmp, name)
            .map_err(|e| self.fence_file(e))?;
        self.derived.remove(&tmp);
        self.derived.insert(name, header.file_bytes());
        self.files
            .point(JournalPoint::RunRenamed)
            .map_err(from_file)?;
        Ok(RunRef {
            first,
            last,
            level,
            entries: header.entries,
            bytes: header.file_bytes(),
            digest,
        })
    }
    /// Keeps the newest two checkpoints and every run either lists, plus a
    /// complete merge output; removes every other derived file older than
    /// the newest valid checkpoint. Never removes a segment file.
    fn retire(&mut self) -> Result<(), Failure> {
        while self.checkpoints.len() > 2 {
            let oldest = *self.checkpoints.keys().next().expect("checkpoint");
            self.checkpoints.remove(&oldest);
        }
        let newest = self.checkpoints.keys().next_back().copied().unwrap_or(0);
        let mut keep: Vec<JournalName> = self
            .checkpoints
            .iter()
            .flat_map(|(s, runs)| {
                std::iter::once(JournalName::Checkpoint(*s)).chain(runs.iter().map(run_name))
            })
            .collect();
        if let Some((output, _)) = &self.merged {
            keep.push(run_name(output));
        }
        if let Some(job) = &self.merge {
            keep.push(JournalName::RunTmp(job.output.0, job.output.1));
        }
        let remove: Vec<JournalName> = self
            .derived
            .keys()
            .filter(|name| !keep.contains(name))
            .filter(|name| match name {
                JournalName::Checkpoint(s) => *s < newest,
                _ => true,
            })
            .copied()
            .collect();
        for name in remove {
            self.files.remove(name).map_err(|e| self.fence_file(e))?;
            self.derived.remove(&name);
        }
        Ok(())
    }

    // ----- Run compaction. -----

    /// Whether a merge step is waiting.
    pub fn merge_pending(&self) -> bool {
        self.merge.is_some() || (self.merged.is_none() && self.merge_candidate().is_some())
    }
    fn merge_candidate(&self) -> Option<Vec<RunRef>> {
        let mut runs = self.runs.clone();
        runs.sort_by_key(|r| r.first);
        for window in runs.windows(FAN_IN) {
            let level = window[0].level;
            if window.iter().all(|r| r.level == level)
                && window
                    .windows(2)
                    .all(|pair| pair[1].first == pair[0].last + 1)
            {
                return Some(window.to_vec());
            }
        }
        None
    }
    /// Runs one merge step of at most 256 output pages. A queued command
    /// always runs before the next step. Returns true while work remains.
    pub fn merge_step(&mut self) -> Result<bool, Failure> {
        if self.fenced || !self.writer || self.merged.is_some() {
            return Ok(false);
        }
        if self.merge.is_none() {
            let Some(inputs) = self.merge_candidate() else {
                return Ok(false);
            };
            self.free = self.files.free_bytes().map_err(from_file)?;
            let output_bytes: u64 = inputs.iter().map(|r| r.bytes).sum();
            if !self.normal_fits(output_bytes) {
                // Waits for space.
                return Ok(false);
            }
            let level = inputs[0].level + 1;
            let output = (inputs[0].first, inputs[inputs.len() - 1].last);
            let tmp = JournalName::RunTmp(output.0, output.1);
            self.files.create(tmp).map_err(|e| self.fence_file(e))?;
            let mut merge_inputs = Vec::new();
            for run in &inputs {
                let mut source = FileRun {
                    files: &mut self.files,
                    name: run_name(run),
                };
                merge_inputs.push(MergeInput::new(RunHeader::read(&mut source)?, run.digest));
            }
            let entries: u64 = inputs.iter().map(|r| r.entries).sum();
            self.derived.insert(
                tmp,
                RunHeader::new(level, self.installation, output.0, output.1, entries).file_bytes(),
            );
            let mut io = FilesMerge {
                files: &mut self.files,
                inputs: inputs.iter().map(run_name).collect(),
                output: tmp,
            };
            let merge = Merge::new(merge_inputs, level, self.installation, &mut io)?;
            self.merge = Some(MergeJob {
                inputs,
                output,
                level,
                merge,
            });
        }
        self.files
            .point(JournalPoint::MergeStep)
            .map_err(from_file)?;
        let job = self.merge.as_mut().expect("merge");
        let mut io = FilesMerge {
            files: &mut self.files,
            inputs: job.inputs.iter().map(run_name).collect(),
            output: JournalName::RunTmp(job.output.0, job.output.1),
        };
        let done = match job.merge.step(&mut io, MERGE_STEP_PAGES) {
            Ok(done) => done,
            Err(error) => {
                let tmp = JournalName::RunTmp(job.output.0, job.output.1);
                self.merge = None;
                let _ = self.files.remove(tmp);
                self.derived.remove(&tmp);
                if error == ReplayError::Corrupt {
                    // A duplicate key or damaged input fences the worker (SJ-12).
                    self.fenced = true;
                }
                return Err(Failure::Replay(error));
            }
        };
        let Some(digest) = done else {
            return Ok(true);
        };
        let job = self.merge.take().expect("merge");
        let tmp = JournalName::RunTmp(job.output.0, job.output.1);
        self.files.sync(tmp).map_err(|e| self.fence_file(e))?;
        let name = JournalName::Run(job.output.0, job.output.1);
        self.files
            .publish(tmp, name)
            .map_err(|e| self.fence_file(e))?;
        let bytes = self.derived.remove(&tmp).unwrap_or(0);
        self.derived.insert(name, bytes);
        let entries = job.inputs.iter().map(|r| r.entries).sum();
        self.merges += 1;
        self.merged = Some((
            RunRef {
                first: job.output.0,
                last: job.output.1,
                level: job.level,
                entries,
                bytes,
                digest,
            },
            job.inputs,
        ));
        Ok(false)
    }
    /// Shutdown: a merge stops at its step boundary and leaves its tmp file.
    pub fn abandon_merge(&mut self) {
        self.merge = None;
    }
}

/// Closure records always fit beside the roll reserve: kind 8, 7, 6 and 3,
/// plus the kind 23 and kind 15 of the pending `run` call, whose bytes the
/// closure reservation already holds, and a kind 14 `cancelled` record,
/// which a cancel needs before its terminal (SJ-D18). Kinds 27 and 28 are
/// closure records too: the job ledger holds their bytes (J2-E4). Kind 26
/// is a normal record.
pub fn is_closure(record: &Record) -> bool {
    match record {
        Record::TurnTerminal(_)
        | Record::CancelRequested(_)
        | Record::OwnerGeneration
        | Record::StartAuthorized(_)
        | Record::CaptureSettled(_)
        | Record::ToolResult(_)
        | Record::JobCancelRequested(_)
        | Record::JobSettled(_) => true,
        Record::ToolCall(call) => call.decision == ToolDecision::Cancelled,
        _ => false,
    }
}
/// The run list of a checkpoint, read from its header before decoding.
fn checkpoint_runs(bytes: &[u8]) -> Result<Vec<RunRef>, ReplayError> {
    // Magic 8, version 2, installation 16, head 8+32+24+8+8, census 24.
    let at = 8 + 2 + 16 + 8 + 32 + 24 + 8 + 8 + 24;
    let count = u32::from_be_bytes(
        bytes
            .get(at..at + 4)
            .ok_or(ReplayError::Corrupt)?
            .try_into()
            .expect("4"),
    ) as usize;
    if count > MAX_RUNS {
        return Err(ReplayError::Corrupt);
    }
    let mut runs = Vec::with_capacity(count);
    let mut offset = at + 4;
    for _ in 0..count {
        let field = bytes.get(offset..offset + 65).ok_or(ReplayError::Corrupt)?;
        let u64_at = |i: usize| u64::from_be_bytes(field[i..i + 8].try_into().expect("8"));
        runs.push(RunRef {
            first: u64_at(0),
            last: u64_at(8),
            level: field[16],
            entries: u64_at(17),
            bytes: u64_at(25),
            digest: field[33..65].try_into().expect("32"),
        });
        offset += 65;
    }
    Ok(runs)
}
