//! Kind 32, ExecutionPathRecord (execution-path-guard.md, EPG-E4): the
//! model changes of build and test paths, the acknowledgements of person
//! dispatch warnings, and the approvals and revocations of implicit-execution
//! files. Records hold project-relative path names and an approval digest,
//! never file content (EPG-R6).
//!
//! Replay derives the guard state of a project from index entries only: `B`
//! model changes, `F` acknowledgements, `Q` approvals and revocations, and
//! `Z` one entry per record reference (AE-05). The live state and the
//! checkpoint hold no per-project copy; the checkpoint's index runs cover the
//! entries, so replay memory stays independent of the number of projects.
use super::codec::{Reader, Writer};
use super::*;
use crate::authority::index::{Entry, Key, Tag};
use crate::authority::{HEADER, ReplayError, TRAILER};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

type Result<T> = std::result::Result<T, ReplayError>;

/// Field 5: listed entries per record.
pub const MAX_PATH_ENTRIES: usize = 16;
/// Field 6: one project-relative path, 1 to 1,024 bytes.
pub const MAX_GUARD_PATH: usize = 1024;
/// The kind 32 payload bound: 1 + 16 + 16 + 8 + 1 + 16 × 1,027 + 4 + 32.
pub const MAX_EXECUTION_PATH_PAYLOAD: usize = 16_510;
/// The largest kind 32 frame.
pub const MAX_EXECUTION_PATH_FRAME: usize = HEADER + TRAILER + MAX_EXECUTION_PATH_PAYLOAD;
/// Approved paths per project; another distinct path is `Limit`.
pub const MAX_PATH_APPROVALS: usize = 256;
/// Pending `model_changed` records a guard read lists, newest kept; older
/// ones are counted in [`PathGuardView::omitted_changes`].
pub const MAX_PENDING_PATH_CHANGES: usize = 64;
/// Approval and revocation records per project. The approval set is folded
/// from them on each approval and each guard read, so the read stays
/// bounded; another record is `Limit` (engineering bound, reported as a
/// design addition).
pub const MAX_PATH_APPROVAL_RECORDS: usize = 4096;
const FIXED: usize = 1 + 16 + 16 + 8 + 1 + 4 + 32;
const _: () =
    assert!(FIXED + MAX_PATH_ENTRIES * (3 + MAX_GUARD_PATH) == MAX_EXECUTION_PATH_PAYLOAD);

fn require(ok: bool) -> Result<()> {
    if ok {
        Ok(())
    } else {
        Err(ReplayError::Corrupt)
    }
}

/// Field 1.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum PathAction {
    /// A settled model job changed build and test paths (reference: job).
    ModelChanged = 1,
    /// A person command was shown the warnings (reference: input).
    Acknowledged = 2,
    /// The person approved an implicit-execution file (reference: request).
    Approved = 3,
    /// The person revoked an approval (reference: request).
    Revoked = 4,
}
impl PathAction {
    pub const ALL: [Self; 4] = [
        Self::ModelChanged,
        Self::Acknowledged,
        Self::Approved,
        Self::Revoked,
    ];
    pub fn from_code(code: u8) -> Option<Self> {
        Self::ALL.get(usize::from(code).checked_sub(1)?).copied()
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ModelChanged => "model_changed",
            Self::Acknowledged => "acknowledged",
            Self::Approved => "approved",
            Self::Revoked => "revoked",
        }
    }
}
/// The kind of one field 6 entry: 0 for an approval or revocation, or the
/// path warning kind 1 to 5.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum PathEntryKind {
    Approval = 0,
    ModelChanged = 1,
    NotApproved = 2,
    ChangedSinceApproval = 3,
    TooLarge = 4,
    Overflow = 5,
}
impl PathEntryKind {
    pub const ALL: [Self; 6] = [
        Self::Approval,
        Self::ModelChanged,
        Self::NotApproved,
        Self::ChangedSinceApproval,
        Self::TooLarge,
        Self::Overflow,
    ];
    pub fn from_code(code: u8) -> Option<Self> {
        Self::ALL.get(usize::from(code)).copied()
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Approval => "approval",
            Self::ModelChanged => "model_changed",
            Self::NotApproved => "not_approved",
            Self::ChangedSinceApproval => "changed_since_approval",
            Self::TooLarge => "too_large",
            Self::Overflow => "overflow",
        }
    }
}
/// One field 6 entry.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PathEntry {
    pub kind: PathEntryKind,
    /// Project-relative and display-redacted by the writer of the record.
    pub path: String,
}
/// Kind 32, ExecutionPathRecord. A normal record, not a closure record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExecutionPathRecord {
    /// Field 1.
    pub action: PathAction,
    /// Field 2: a registered project.
    pub project: Id,
    /// Field 3: the settled model job (action 1), the person input (action
    /// 2) or the request (actions 3 and 4). One record per reference.
    pub reference: Id,
    /// Field 4: action 2 only, the sequence of the last action 1 record of
    /// the project it covers, or 0.
    pub through: u64,
    /// Fields 5 and 6.
    pub entries: Vec<PathEntry>,
    /// Field 7: entries not listed.
    pub overflow: u32,
    /// Field 8: action 3 only, the SHA-256 of the approved content.
    pub digest: Hash,
}

/// The path rule of field 6: 1 to 1,024 bytes, project relative (no
/// leading `/`), no NUL and no `..` component.
pub fn valid_guard_path(path: &str) -> bool {
    (1..=MAX_GUARD_PATH).contains(&path.len())
        && !path.starts_with('/')
        && !path.contains('\0')
        && path.split('/').all(|part| part != "..")
}
/// The key of one approved path: SHA-256 over a domain tag and the path
/// bytes. Two paths are the same approval exactly when their keys are equal.
pub fn path_key(path: &str) -> Hash {
    let mut hash = Sha256::new();
    hash.update(b"asura.path\0");
    hash.update(path.as_bytes());
    hash.finalize().into()
}

impl ExecutionPathRecord {
    /// Every kind 32 field rule that the record alone determines. Replay
    /// adds the project, the reference and the approval state.
    pub fn validate(&self) -> Result<()> {
        use PathAction as A;
        use PathEntryKind as K;
        require(
            self.project != [0; 16]
                && self.reference != [0; 16]
                && self.entries.len() <= MAX_PATH_ENTRIES
                && self.entries.iter().all(|e| valid_guard_path(&e.path)),
        )?;
        let kinds = |allowed: &[K]| self.entries.iter().all(|e| allowed.contains(&e.kind));
        let shape = match self.action {
            A::ModelChanged => kinds(&[K::ModelChanged]),
            A::Acknowledged => kinds(&[
                K::ModelChanged,
                K::NotApproved,
                K::ChangedSinceApproval,
                K::TooLarge,
                K::Overflow,
            ]),
            A::Approved | A::Revoked => {
                self.entries.len() == 1 && kinds(&[K::Approval]) && self.overflow == 0
            }
        };
        require(
            shape
                && ((self.action == A::Acknowledged) || self.through == 0)
                && ((self.action == A::Approved) == (self.digest != [0; 32])),
        )
    }
    /// The one path of an approval or revocation.
    pub fn approval_path(&self) -> Option<&str> {
        match self.action {
            PathAction::Approved | PathAction::Revoked => {
                self.entries.first().map(|e| e.path.as_str())
            }
            _ => None,
        }
    }
}

/// Kind 32 field order. Callers validate first; tests use it to build
/// invalid bytes.
pub(crate) fn execution_path_fields(w: &mut Writer, p: &ExecutionPathRecord) {
    w.byte(p.action as u8);
    w.bytes(&p.project);
    w.bytes(&p.reference);
    w.u64(p.through);
    w.byte(p.entries.len() as u8);
    for entry in &p.entries {
        w.byte(entry.kind as u8);
        w.u16(entry.path.len() as u16);
        w.bytes(entry.path.as_bytes());
    }
    w.u32(p.overflow);
    w.bytes(&p.digest);
}
pub(crate) fn write_execution_path(w: &mut Writer, p: &ExecutionPathRecord) -> Result<()> {
    if p.entries.len() > MAX_PATH_ENTRIES || p.entries.iter().any(|e| e.path.len() > MAX_GUARD_PATH)
    {
        return Err(ReplayError::Limit);
    }
    p.validate()?;
    execution_path_fields(w, p);
    Ok(())
}
pub(crate) fn read_execution_path(r: &mut Reader<'_>) -> Result<ExecutionPathRecord> {
    let action = PathAction::from_code(r.byte()?).ok_or(ReplayError::Corrupt)?;
    let project = r.array()?;
    let reference = r.array()?;
    let through = r.u64()?;
    let count = usize::from(r.byte()?);
    require(count <= MAX_PATH_ENTRIES)?;
    let mut entries = Vec::with_capacity(count);
    for _ in 0..count {
        let kind = PathEntryKind::from_code(r.byte()?).ok_or(ReplayError::Corrupt)?;
        let length = usize::from(r.u16()?);
        require((1..=MAX_GUARD_PATH).contains(&length))?;
        let path = std::str::from_utf8(r.take(length)?)
            .map_err(|_| ReplayError::Corrupt)?
            .to_owned();
        entries.push(PathEntry { kind, path });
    }
    let value = ExecutionPathRecord {
        action,
        project,
        reference,
        through,
        entries,
        overflow: r.u32()?,
        digest: r.array()?,
    };
    value.validate()?;
    Ok(value)
}

// ----- Replay rules and guard reads. -----

/// The newest acknowledgement of a project.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PathAcknowledgement {
    /// The kind 32 frame.
    pub frame: Locator,
    pub input: Id,
    /// Field 4.
    pub through: u64,
}
/// One pending `model_changed` record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PathChange {
    pub frame: Locator,
    pub job: Id,
    pub paths: Vec<String>,
    pub overflow: u32,
}
/// One current approval.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PathApproval {
    pub path: String,
    pub digest: Hash,
    /// The approving kind 32 frame.
    pub frame: Locator,
}
/// The guard state of one project, derived from its kind 32 records.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PathGuardView {
    /// The newest acknowledgement, if any.
    pub acknowledged: Option<PathAcknowledgement>,
    /// The newest pending `model_changed` records (after the
    /// acknowledgement's `through`), oldest first, at most
    /// [`MAX_PENDING_PATH_CHANGES`].
    pub changes: Vec<PathChange>,
    /// Pending `model_changed` records older than those listed.
    pub omitted_changes: u64,
    /// Current approvals in byte order of the path, at most
    /// [`MAX_PATH_APPROVALS`].
    pub approvals: Vec<PathApproval>,
    /// Approval and revocation records of the project so far, at most
    /// [`MAX_PATH_APPROVAL_RECORDS`].
    pub approval_records: usize,
}
impl PathGuardView {
    /// Field 4 of an acknowledgement that covers every pending change: the
    /// newest pending record's sequence, else the previous `through`.
    pub fn through(&self) -> u64 {
        self.changes.last().map_or_else(
            || self.acknowledged.map_or(0, |a| a.through),
            |c| c.frame.sequence,
        )
    }
    /// Whether another approval or revocation record fits the per-project
    /// record bound, and an approval of `path` the approval bound.
    pub fn has_room(&self, action: PathAction, path: &str) -> bool {
        self.approval_records < MAX_PATH_APPROVAL_RECORDS
            && (action != PathAction::Approved
                || self.approvals.len() < MAX_PATH_APPROVALS
                || self.approvals.iter().any(|a| a.path == path))
    }
}

/// The folded approval set: path key to (digest, approving frame), and the
/// number of approval and revocation records read.
type ApprovalSet = (BTreeMap<Hash, (Hash, Locator)>, usize);

impl Replay {
    fn project_known(&self, project: Id, index: &mut dyn Index) -> Result<bool> {
        Ok(self.projects.contains_key(&project)
            || self
                .get(&Key::new(Tag::Project, project, 0), index)?
                .is_some())
    }
    fn last_change(&self, project: Id, index: &mut dyn Index) -> Result<Option<(u64, u64)>> {
        match self.last_under(Tag::PathChange, project, index)? {
            Some((sequence, Entry::PathChange { number, .. })) => Ok(Some((sequence, number))),
            Some(_) => Err(ReplayError::Corrupt),
            None => Ok(None),
        }
    }
    /// The newest acknowledgement entry: (sequence, input, through,
    /// covered, frame).
    fn last_acknowledgement(
        &self,
        project: Id,
        index: &mut dyn Index,
    ) -> Result<Option<(Id, u64, u64, Locator)>> {
        match self.last_under(Tag::PathAck, project, index)? {
            Some((
                _,
                Entry::PathAck {
                    input,
                    through,
                    covered,
                    frame,
                },
            )) => Ok(Some((input, through, covered, frame))),
            Some(_) => Err(ReplayError::Corrupt),
            None => Ok(None),
        }
    }
    /// Folds a project's approval and revocation entries; more than
    /// [`MAX_PATH_APPROVAL_RECORDS`] is `Limit`.
    fn approval_set(&self, project: Id, index: &mut dyn Index) -> Result<ApprovalSet> {
        let entries = self.range(
            &Key::new(Tag::PathApproval, project, 0),
            &Key::new(Tag::PathApproval, project, u64::MAX),
            false,
            MAX_PATH_APPROVAL_RECORDS + 1,
            index,
        )?;
        if entries.len() > MAX_PATH_APPROVAL_RECORDS {
            return Err(ReplayError::Limit);
        }
        let mut set = BTreeMap::new();
        for (_, value) in &entries {
            let Entry::PathApproval {
                action,
                path_key,
                digest,
                frame,
            } = Entry::decode(Tag::PathApproval, value)?
            else {
                return Err(ReplayError::Corrupt);
            };
            match PathAction::from_code(action) {
                Some(PathAction::Approved) => {
                    set.insert(path_key, (digest, frame));
                }
                Some(PathAction::Revoked) => {
                    require(set.remove(&path_key).is_some())?;
                }
                _ => return Err(ReplayError::Corrupt),
            }
        }
        require(set.len() <= MAX_PATH_APPROVALS)?;
        Ok((set, entries.len()))
    }
    /// Kind 32 under the replay rules of execution-path-guard.md: a
    /// registered project; one record per reference; action 1 for a
    /// settled model-origin job of the project; action 2 for an
    /// undispatched kind 31 input of the project, covering a later
    /// `model_changed` of the project or none; action 4 for an approved
    /// path; at most 256 approved paths and 4,096 approval records.
    pub(super) fn execution_path(
        &mut self,
        p: ExecutionPathRecord,
        frame: Locator,
        index: &mut dyn Index,
    ) -> Result<()> {
        p.validate()?;
        let sequence = frame.sequence;
        require(self.project_known(p.project, index)?)?;
        let reference = Key::new(Tag::PathReference, p.reference, 0);
        require(self.get(&reference, index)?.is_none())?;
        let conversation = match p.action {
            PathAction::ModelChanged => {
                let Some(JobState::Settled(job)) = self.job_state(p.reference, index)? else {
                    return Err(ReplayError::Corrupt);
                };
                let project = self
                    .find_conversation(job.conversation, index)?
                    .ok_or(ReplayError::Corrupt)?
                    .project;
                require(job.origin == 1 && project == p.project)?;
                let number = match self.last_change(p.project, index)? {
                    Some((_, number)) => number.checked_add(1).ok_or(ReplayError::Corrupt)?,
                    None => 1,
                };
                self.emit(
                    Key::new(Tag::PathChange, p.project, sequence),
                    Entry::PathChange {
                        job: p.reference,
                        number,
                        frame,
                    },
                )?;
                Some(job.conversation)
            }
            PathAction::Acknowledged => {
                let q = self.inputs.get(&p.reference).ok_or(ReplayError::Corrupt)?;
                require(
                    q.project == p.project
                        && !q.dropped
                        && q.person.is_some_and(|person| person.job.is_none()),
                )?;
                let conversation = q.conversation;
                let previous = self.last_acknowledgement(p.project, index)?;
                let (prior_through, prior_covered) = previous.map_or((0, 0), |a| (a.1, a.2));
                let covered = if p.through == prior_through {
                    prior_covered
                } else {
                    require(p.through > prior_through)?;
                    match self.entry(&Key::new(Tag::PathChange, p.project, p.through), index)? {
                        Some(Entry::PathChange { number, .. }) => number,
                        _ => return Err(ReplayError::Corrupt),
                    }
                };
                self.emit(
                    Key::new(Tag::PathAck, p.project, sequence),
                    Entry::PathAck {
                        input: p.reference,
                        through: p.through,
                        covered,
                        frame,
                    },
                )?;
                Some(conversation)
            }
            PathAction::Approved | PathAction::Revoked => {
                let (set, records) = self.approval_set(p.project, index)?;
                if records >= MAX_PATH_APPROVAL_RECORDS {
                    return Err(ReplayError::Limit);
                }
                let key = path_key(&p.entries[0].path);
                if p.action == PathAction::Revoked {
                    require(set.contains_key(&key))?;
                } else if !set.contains_key(&key) && set.len() >= MAX_PATH_APPROVALS {
                    return Err(ReplayError::Limit);
                }
                self.emit(
                    Key::new(Tag::PathApproval, p.project, sequence),
                    Entry::PathApproval {
                        action: p.action as u8,
                        path_key: key,
                        digest: p.digest,
                        frame,
                    },
                )?;
                None
            }
        };
        self.emit(
            reference,
            Entry::PathReference {
                action: p.action as u8,
                project: p.project,
                frame,
            },
        )?;
        self.record_activity(p.project, conversation, sequence);
        Ok(())
    }
    /// The kind 32 record of one reference (job, input or request), with
    /// its frame.
    pub fn path_record(
        &self,
        reference: Id,
        index: &mut dyn Index,
    ) -> Result<Option<(Locator, ExecutionPathRecord)>> {
        let frame = match self.entry(&Key::new(Tag::PathReference, reference, 0), index)? {
            Some(Entry::PathReference { frame, .. }) => frame,
            Some(_) => return Err(ReplayError::Corrupt),
            None => return Ok(None),
        };
        match index.frame(&frame)? {
            Record::ExecutionPath(p) if p.reference == reference => Ok(Some((frame, *p))),
            _ => Err(ReplayError::Corrupt),
        }
    }
    /// The guard state of a project: its newest acknowledgement, the
    /// newest pending `model_changed` records with their paths, and the
    /// current approvals with their paths. At most 64 change frames and 256
    /// approval frames are read.
    pub fn path_guard(&self, project: Id, index: &mut dyn Index) -> Result<PathGuardView> {
        let mut view = PathGuardView::default();
        let (through, covered) = match self.last_acknowledgement(project, index)? {
            Some((input, through, covered, frame)) => {
                view.acknowledged = Some(PathAcknowledgement {
                    frame,
                    input,
                    through,
                });
                (through, covered)
            }
            None => (0, 0),
        };
        let newest = self.range(
            &Key::new(Tag::PathChange, project, through.saturating_add(1)),
            &Key::new(Tag::PathChange, project, u64::MAX),
            true,
            MAX_PENDING_PATH_CHANGES,
            index,
        )?;
        if let Some((_, value)) = newest.first() {
            let Entry::PathChange { number, .. } = Entry::decode(Tag::PathChange, value)? else {
                return Err(ReplayError::Corrupt);
            };
            let pending = number.checked_sub(covered).ok_or(ReplayError::Corrupt)?;
            view.omitted_changes = pending
                .checked_sub(newest.len() as u64)
                .ok_or(ReplayError::Corrupt)?;
        }
        for (key, value) in newest.iter().rev() {
            let Entry::PathChange { job, frame, .. } = Entry::decode(Tag::PathChange, value)?
            else {
                return Err(ReplayError::Corrupt);
            };
            let Record::ExecutionPath(p) = index.frame(&frame)? else {
                return Err(ReplayError::Corrupt);
            };
            require(
                p.action == PathAction::ModelChanged
                    && p.project == project
                    && p.reference == job
                    && frame.sequence == key.disc,
            )?;
            view.changes.push(PathChange {
                frame,
                job,
                paths: p.entries.into_iter().map(|e| e.path).collect(),
                overflow: p.overflow,
            });
        }
        let (set, records) = self.approval_set(project, index)?;
        view.approval_records = records;
        for (key, (digest, frame)) in set {
            let Record::ExecutionPath(p) = index.frame(&frame)? else {
                return Err(ReplayError::Corrupt);
            };
            let path = p.approval_path().ok_or(ReplayError::Corrupt)?;
            require(
                p.action == PathAction::Approved
                    && p.project == project
                    && p.digest == digest
                    && path_key(path) == key,
            )?;
            view.approvals.push(PathApproval {
                path: path.to_owned(),
                digest,
                frame,
            });
        }
        view.approvals
            .sort_by(|a, b| a.path.as_bytes().cmp(b.path.as_bytes()));
        Ok(view)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::authority::conversation::{decode_record, encode_frame};

    fn record(action: PathAction, entries: Vec<PathEntry>) -> ExecutionPathRecord {
        ExecutionPathRecord {
            action,
            project: [6; 16],
            reference: [7; 16],
            through: 0,
            entries,
            overflow: 0,
            digest: if action == PathAction::Approved {
                [9; 32]
            } else {
                [0; 32]
            },
        }
    }
    fn entry(kind: PathEntryKind, path: &str) -> PathEntry {
        PathEntry {
            kind,
            path: path.into(),
        }
    }
    fn raw(p: &ExecutionPathRecord) -> Vec<u8> {
        let mut w = Writer(Vec::new());
        execution_path_fields(&mut w, p);
        w.0
    }
    fn payload(p: &ExecutionPathRecord) -> Result<Vec<u8>> {
        let mut w = Writer(Vec::new());
        write_execution_path(&mut w, p)?;
        Ok(w.0)
    }
    fn round(p: ExecutionPathRecord) -> Vec<u8> {
        let bytes = payload(&p).unwrap();
        assert_eq!(bytes, raw(&p));
        let record = Record::ExecutionPath(Box::new(p));
        assert_eq!(record.kind(), 32);
        assert_eq!(decode_record(32, &bytes).unwrap(), record);
        bytes
    }
    fn invalid(p: ExecutionPathRecord) {
        assert_eq!(payload(&p), Err(ReplayError::Corrupt), "{p:?}");
        assert_eq!(
            decode_record(32, &raw(&p)),
            Err(ReplayError::Corrupt),
            "{p:?}"
        );
    }
    fn context() -> FrameContext {
        FrameContext {
            installation_id: [1; 16],
            transition_id: [2; 16],
            sequence: 3,
            prior_digest: [4; 32],
            expected_revision: 2,
            owner_generation: 1,
        }
    }

    /// EPG-U3 (unit): kind 32 round trips at every bound, at the maximum
    /// payload of 16,510 bytes and the maximum frame of 16,694 bytes.
    #[test]
    fn epg_u3_kind_32_round_trips_every_bound_and_maximum_size() {
        use PathAction as A;
        use PathEntryKind as K;
        // The smallest records: no entries (actions 1 and 2) and one entry
        // of one byte (actions 3 and 4).
        for action in [A::ModelChanged, A::Acknowledged] {
            let bytes = round(record(action, vec![]));
            assert_eq!(bytes.len(), FIXED);
            assert_eq!(bytes[0], action as u8);
        }
        for action in [A::Approved, A::Revoked] {
            let bytes = round(record(action, vec![entry(K::Approval, "x")]));
            assert_eq!(bytes.len(), FIXED + 3 + 1);
        }
        // Every warning kind in an acknowledgement, with field 4 and 7.
        let ack = ExecutionPathRecord {
            through: u64::MAX,
            overflow: u32::MAX,
            ..record(
                A::Acknowledged,
                vec![
                    entry(K::ModelChanged, "build.rs"),
                    entry(K::NotApproved, ".git/hooks/pre-commit"),
                    entry(K::ChangedSinceApproval, "sub/.envrc"),
                    entry(K::TooLarge, ".gitattributes"),
                    entry(K::Overflow, "."),
                ],
            )
        };
        let bytes = round(ack);
        assert_eq!(&bytes[33..41], &u64::MAX.to_be_bytes());
        assert_eq!(bytes[41], 5);
        // A redacted path keeps its placeholder; any UTF-8 is kept.
        round(record(
            A::ModelChanged,
            vec![
                entry(K::ModelChanged, "tests/$ASURA_SECRET_0/é.rs"),
                entry(K::ModelChanged, "a..b/..c/.d"),
            ],
        ));
        // The maximum: 16 entries of 1,024 bytes.
        let max = ExecutionPathRecord {
            overflow: u32::MAX,
            through: 1,
            ..record(
                A::Acknowledged,
                (0..MAX_PATH_ENTRIES)
                    .map(|n| entry(K::NotApproved, &format!("{n:x}").repeat(MAX_GUARD_PATH)))
                    .collect(),
            )
        };
        assert_eq!(round(max.clone()).len(), MAX_EXECUTION_PATH_PAYLOAD);
        assert_eq!(MAX_EXECUTION_PATH_PAYLOAD, 16_510);
        let frame = encode_frame(&context(), &Record::ExecutionPath(Box::new(max))).unwrap();
        assert_eq!(frame.len(), 16_694);
        assert_eq!(MAX_EXECUTION_PATH_FRAME, 16_694);
        // Codes.
        for (code, name) in [
            (1, "model_changed"),
            (2, "acknowledged"),
            (3, "approved"),
            (4, "revoked"),
        ] {
            let action = A::from_code(code).unwrap();
            assert_eq!((action as u8, action.as_str()), (code, name));
        }
        assert_eq!((A::from_code(0), A::from_code(5)), (None, None));
        for kind in K::ALL {
            assert_eq!(K::from_code(kind as u8), Some(kind));
        }
        assert_eq!(K::from_code(6), None);
        assert_eq!(K::Overflow.as_str(), "overflow");
    }

    /// EPG-U3 (unit): each invalid field is `Corrupt` to encode and to
    /// decode; an oversize path or entry list is over the limit to encode.
    #[test]
    fn epg_u3_kind_32_rejects_each_invalid_field() {
        use PathAction as A;
        use PathEntryKind as K;
        let approval = || record(A::Approved, vec![entry(K::Approval, ".envrc")]);
        for p in [
            ExecutionPathRecord {
                project: [0; 16],
                ..record(A::ModelChanged, vec![])
            },
            ExecutionPathRecord {
                reference: [0; 16],
                ..record(A::Acknowledged, vec![])
            },
            // Field 4 only on an acknowledgement.
            ExecutionPathRecord {
                through: 1,
                ..record(A::ModelChanged, vec![])
            },
            ExecutionPathRecord {
                through: 1,
                ..approval()
            },
            // Entry kinds per action.
            record(A::ModelChanged, vec![entry(K::NotApproved, "build.rs")]),
            record(A::ModelChanged, vec![entry(K::Approval, "build.rs")]),
            record(A::Acknowledged, vec![entry(K::Approval, "build.rs")]),
            record(A::Approved, vec![entry(K::NotApproved, ".envrc")]),
            record(A::Revoked, vec![entry(K::ModelChanged, ".envrc")]),
            // Exactly one entry for actions 3 and 4, and no overflow.
            record(A::Approved, vec![]),
            record(A::Revoked, vec![]),
            record(
                A::Approved,
                vec![
                    entry(K::Approval, ".envrc"),
                    entry(K::Approval, ".gitmodules"),
                ],
            ),
            ExecutionPathRecord {
                overflow: 1,
                ..approval()
            },
            // Field 8: nonzero exactly for an approval.
            ExecutionPathRecord {
                digest: [0; 32],
                ..approval()
            },
            ExecutionPathRecord {
                digest: [1; 32],
                ..record(A::Revoked, vec![entry(K::Approval, ".envrc")])
            },
            ExecutionPathRecord {
                digest: [1; 32],
                ..record(A::Acknowledged, vec![])
            },
            // Path rules.
            record(
                A::ModelChanged,
                vec![entry(K::ModelChanged, "/abs/build.rs")],
            ),
            record(A::ModelChanged, vec![entry(K::ModelChanged, "../build.rs")]),
            record(
                A::ModelChanged,
                vec![entry(K::ModelChanged, "a/../build.rs")],
            ),
            record(A::ModelChanged, vec![entry(K::ModelChanged, "a/..")]),
            record(A::ModelChanged, vec![entry(K::ModelChanged, "..")]),
            record(A::ModelChanged, vec![entry(K::ModelChanged, "nul\0x")]),
        ] {
            invalid(p);
        }
        // An empty path is Corrupt to encode and decode.
        let empty = record(A::ModelChanged, vec![entry(K::ModelChanged, "")]);
        invalid(empty);
        // Over the bounds: Limit to encode, Corrupt to decode.
        let long = record(
            A::ModelChanged,
            vec![entry(K::ModelChanged, &"p".repeat(MAX_GUARD_PATH + 1))],
        );
        assert_eq!(payload(&long), Err(ReplayError::Limit));
        assert_eq!(decode_record(32, &raw(&long)), Err(ReplayError::Corrupt));
        let many = record(
            A::Acknowledged,
            vec![entry(K::NotApproved, ".envrc"); MAX_PATH_ENTRIES + 1],
        );
        assert_eq!(payload(&many), Err(ReplayError::Limit));
        assert_eq!(decode_record(32, &raw(&many)), Err(ReplayError::Corrupt));
        // Wire-only values: action, entry kind, length, UTF-8 and size.
        let good = raw(&record(A::ModelChanged, vec![entry(K::ModelChanged, "ab")]));
        let entry_at = 1 + 16 + 16 + 8 + 1;
        for (at, value) in [(0, 0), (0, 5), (entry_at, 0), (entry_at, 6)] {
            let mut bytes = good.clone();
            bytes[at] = value;
            assert_eq!(decode_record(32, &bytes), Err(ReplayError::Corrupt), "{at}");
        }
        let mut bytes = good.clone();
        bytes[entry_at + 3] = 0xff;
        assert_eq!(decode_record(32, &bytes), Err(ReplayError::Corrupt));
        let mut bytes = good.clone();
        bytes[entry_at - 1] = 2;
        assert_eq!(decode_record(32, &bytes), Err(ReplayError::Corrupt));
        let mut bytes = good.clone();
        bytes.push(0);
        assert_eq!(decode_record(32, &bytes), Err(ReplayError::Corrupt));
        let mut bytes = good;
        bytes.pop();
        assert_eq!(decode_record(32, &bytes), Err(ReplayError::Corrupt));
        // A payload over the kind 32 bound is Corrupt before decoding.
        let mut bytes = raw(&record(A::Acknowledged, vec![]));
        bytes.resize(MAX_EXECUTION_PATH_PAYLOAD + 1, 0);
        assert_eq!(decode_record(32, &bytes), Err(ReplayError::Corrupt));
    }

    /// EPG-U3 (unit): path keys and the room rules the writer applies.
    #[test]
    fn epg_u3_path_keys_and_room() {
        assert_eq!(path_key(".envrc"), path_key(".envrc"));
        assert_ne!(path_key(".envrc"), path_key("./.envrc"));
        assert_ne!(path_key(".envrc"), path_key(".envrc\u{301}"));
        let mut view = PathGuardView::default();
        assert!(view.has_room(PathAction::Approved, ".envrc"));
        assert_eq!(view.through(), 0);
        view.approvals = (0..MAX_PATH_APPROVALS)
            .map(|n| PathApproval {
                path: format!("p{n}"),
                digest: [1; 32],
                frame: Locator::NONE,
            })
            .collect();
        assert!(!view.has_room(PathAction::Approved, ".envrc"));
        assert!(view.has_room(PathAction::Approved, "p7"));
        assert!(view.has_room(PathAction::Revoked, "p7"));
        view.approval_records = MAX_PATH_APPROVAL_RECORDS;
        assert!(!view.has_room(PathAction::Revoked, "p7"));
        view.acknowledged = Some(PathAcknowledgement {
            frame: Locator::NONE,
            input: [1; 16],
            through: 9,
        });
        assert_eq!(view.through(), 9);
        view.changes.push(PathChange {
            frame: Locator {
                sequence: 12,
                ..Locator::NONE
            },
            job: [2; 16],
            paths: vec![],
            overflow: 0,
        });
        assert_eq!(view.through(), 12);
    }
}
