//! The execution path guard (execution-path-guard.md, packet EPG-W): the
//! fixed path lists, the bounded walk, fingerprint snapshots around model
//! jobs, the warnings before a person command, and the worker that reads
//! the project for them.
//!
//! - [`lists`]: the implicit-execution and build and test lists (EPG-E3).
//! - [`walk`]: the walk, fingerprints, snapshot differences and digests.
//! - [`check`]: the warning computation (pure).
//! - [`worker`]: the `asura-path-guard` thread (AE-01 to AE-06).
//!
//! Snapshots run on the slot setup and cleanup threads
//! (`exec_cmd_worker`); the controller records a job's change set as kind
//! 32 `model_changed` after its kind 28; the conversation owner checks the
//! project at each person dispatch and records kind 32 `acknowledged`,
//! `approved` and `revoked`. Records hold display-redacted project-relative
//! paths, never file content (EPG-R6).
pub(crate) mod check;
pub(crate) mod lists;
pub(crate) mod walk;
pub(crate) mod worker;

use asura_storage::authority::conversation::{MAX_PATH_ENTRIES, valid_guard_path};
use std::sync::atomic::AtomicBool;

/// A path as records and warnings hold it: display-redacted with the
/// secret detection display rule, or `None` when it cannot be recorded (the
/// rule set failed its self-check, or the redacted form is not a valid
/// guard path). Compiles the rule set on first use: worker and slot threads
/// only.
pub(crate) fn display(path: &str) -> Option<String> {
    let rules = asura_platform::secrets::detection().ok()?;
    let shown = asura_platform::secrets::display_redacted(rules, path);
    valid_guard_path(&shown).then_some(shown)
}

/// One model job's change set: the changed build and test paths, at most
/// 16 in byte order, and the count of changes not listed (a bound passed,
/// or a snapshot that failed counts one).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ModelChange {
    pub paths: Vec<String>,
    pub overflow: u32,
}

/// The build and test snapshot of a model job's project, taken on a slot
/// thread just before spawn and again after the settled notice.
pub(crate) type Snapshot = Result<walk::Walk, walk::WalkError>;

/// Takes one build and test snapshot of `root`. Blocking: slot threads only.
pub(crate) fn snapshot(root: &str, cancel: &AtomicBool) -> Snapshot {
    walk::walk(std::path::Path::new(root), lists::List::BuildTest, cancel)
}

/// The change set between a job's two snapshots, or `None` when nothing
/// changed and no bound was passed. A failed snapshot gives an empty list
/// with overflow 1, so the next person command warns `overflow`.
pub(crate) fn model_change(before: &Snapshot, after: &Snapshot) -> Option<ModelChange> {
    let (Ok(before), Ok(after)) = (before, after) else {
        return Some(ModelChange {
            paths: Vec::new(),
            overflow: 1,
        });
    };
    let mut overflow = u32::from(before.overflow || after.overflow);
    let mut shown: Vec<String> = Vec::new();
    for path in walk::changed(before, after) {
        match display(&path) {
            Some(path) => shown.push(path),
            None => overflow = overflow.saturating_add(1),
        }
    }
    shown.sort_unstable_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
    shown.dedup();
    if shown.len() > MAX_PATH_ENTRIES {
        let rest = u32::try_from(shown.len() - MAX_PATH_ENTRIES).unwrap_or(u32::MAX);
        overflow = overflow.saturating_add(rest);
        shown.truncate(MAX_PATH_ENTRIES);
    }
    (!shown.is_empty() || overflow > 0).then_some(ModelChange {
        paths: shown,
        overflow,
    })
}

#[cfg(test)]
mod tests;
