//! The warnings before a person command (execution-path-guard.md, "Warning
//! at person dispatch"; EPG-U4): pure, in-memory work over the project's
//! guard state ([`PathGuardView`]) and one check's file facts. Kind 1 comes
//! from the pending `model_changed` records; kinds 2 to 4 from the
//! implicit-execution files and their approvals; kind 5 from any bound.
use super::walk::FileState;
use asura_storage::authority::conversation::{
    ExecutionPathRecord, Id, MAX_PATH_ENTRIES, PathAction, PathEntry, PathEntryKind, PathGuardView,
};
use std::collections::{BTreeMap, BTreeSet};

/// The path of a kind 5 `overflow` warning: the project root.
pub(crate) const OVERFLOW_PATH: &str = ".";

/// One path warning.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Warning {
    pub kind: PathEntryKind,
    pub path: String,
}

/// The warnings one command shows: at most 16, then the count of the rest.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Warnings {
    pub listed: Vec<Warning>,
    pub overflow: u32,
}
impl Warnings {
    /// The first 16 of `all`, which is in (kind, path byte) order.
    pub(crate) fn from_all(mut all: Vec<Warning>) -> Self {
        let overflow = all.len().saturating_sub(MAX_PATH_ENTRIES);
        all.truncate(MAX_PATH_ENTRIES);
        Self {
            listed: all,
            overflow: u32::try_from(overflow).unwrap_or(u32::MAX),
        }
    }
    /// From a committed acknowledgement (a re-dispatch after a restart).
    pub(crate) fn from_record(record: &ExecutionPathRecord) -> Self {
        Self {
            listed: record
                .entries
                .iter()
                .map(|e| Warning {
                    kind: e.kind,
                    path: e.path.clone(),
                })
                .collect(),
            overflow: record.overflow,
        }
    }
    pub(crate) fn model_changed(&self) -> bool {
        self.listed
            .iter()
            .any(|w| w.kind == PathEntryKind::ModelChanged)
    }
}

/// One check's facts from the filesystem, gathered on the path guard
/// worker. Paths are display-redacted.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Facts {
    /// The implicit-execution files the walk found.
    pub files: BTreeMap<String, FileState>,
    /// Each approved path's current state, read by its path.
    pub approved: BTreeMap<String, FileState>,
    /// The walk passed a bound, or a path could not be recorded.
    pub overflow: bool,
}

/// Every warning, in (kind, path byte) order, each (kind, path) once.
pub(crate) fn all(view: &PathGuardView, facts: &Facts) -> Vec<Warning> {
    let mut out = BTreeSet::new();
    let mut overflow = facts.overflow || view.omitted_changes > 0;
    for change in &view.changes {
        overflow |= change.overflow > 0;
        for path in &change.paths {
            out.insert((PathEntryKind::ModelChanged, path.clone()));
        }
    }
    let approvals: BTreeMap<&str, [u8; 32]> = view
        .approvals
        .iter()
        .map(|a| (a.path.as_str(), a.digest))
        .collect();
    for (path, state) in &facts.files {
        if approvals.contains_key(path.as_str()) {
            continue;
        }
        match state {
            FileState::TooLarge => out.insert((PathEntryKind::TooLarge, path.clone())),
            FileState::Missing => false,
            FileState::Digest(_) | FileState::Unreadable => {
                out.insert((PathEntryKind::NotApproved, path.clone()))
            }
        };
    }
    for (path, digest) in approvals {
        let state = facts
            .approved
            .get(path)
            .or_else(|| facts.files.get(path))
            .copied()
            .unwrap_or(FileState::Missing);
        match state {
            FileState::Digest(d) if d == digest => {}
            FileState::TooLarge => {
                out.insert((PathEntryKind::TooLarge, path.to_owned()));
            }
            _ => {
                out.insert((PathEntryKind::ChangedSinceApproval, path.to_owned()));
            }
        }
    }
    if overflow {
        out.insert((PathEntryKind::Overflow, OVERFLOW_PATH.to_owned()));
    }
    let mut out: Vec<Warning> = out
        .into_iter()
        .map(|(kind, path)| Warning { kind, path })
        .collect();
    out.sort_by(|a, b| {
        a.kind
            .cmp(&b.kind)
            .then_with(|| a.path.as_bytes().cmp(b.path.as_bytes()))
    });
    out
}

/// The sequence of the newest pending `model_changed` record that lists a
/// path, or 0: a `!!` hold covers the kind 1 warnings up to it.
pub(crate) fn model_through(view: &PathGuardView) -> u64 {
    view.changes
        .iter()
        .filter(|c| !c.paths.is_empty())
        .map(|c| c.frame.sequence)
        .max()
        .unwrap_or(0)
}

/// The files `/paths approve all` approves: every implicit-execution file
/// shown as not approved or changed since approval, with its digest. A file
/// that cannot be read or is too large is left out.
pub(crate) fn approvable(view: &PathGuardView, facts: &Facts) -> Vec<(String, [u8; 32])> {
    let warned: BTreeSet<String> = all(view, facts)
        .into_iter()
        .filter(|w| {
            matches!(
                w.kind,
                PathEntryKind::NotApproved | PathEntryKind::ChangedSinceApproval
            )
        })
        .map(|w| w.path)
        .collect();
    warned
        .into_iter()
        .filter_map(|path| {
            let state = facts
                .approved
                .get(&path)
                .or_else(|| facts.files.get(&path))
                .copied()?;
            match state {
                FileState::Digest(digest) => Some((path, digest)),
                _ => None,
            }
        })
        .collect()
}

/// The kind 32 `acknowledged` record of `input`: the shown warnings and the
/// sequence of the last `model_changed` record it covers.
pub(crate) fn acknowledgement(
    project: Id,
    input: Id,
    view: &PathGuardView,
    warnings: &Warnings,
) -> ExecutionPathRecord {
    ExecutionPathRecord {
        action: PathAction::Acknowledged,
        project,
        reference: input,
        through: view.through(),
        entries: warnings
            .listed
            .iter()
            .map(|w| PathEntry {
                kind: w.kind,
                path: w.path.clone(),
            })
            .collect(),
        overflow: warnings.overflow,
        digest: [0; 32],
    }
}
