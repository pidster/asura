//! Payload recovery at service start. Replay facts and the payload
//! directories derive each job payload's state from the job's kinds 26 and
//! 28 (TJ-Q5) and its retention state from kind 30 (J4): a pinned job keeps
//! its pin hold, a deleted job's files are deleted. Payloads are keyed by
//! job ID; a file no job derives is an orphan, including a file of the
//! retired J1 derivation. Runs on the retained recovery worker with no time
//! bound; shutdown cancels it between steps.
use super::store::{Location, PayloadId, PayloadState, PayloadStore};
use crate::authority::conversation::{CaptureState, Hash, Id, JobOrigin, Replay};
use asura_platform::payload_io::{PayloadDirectory, PayloadError, PayloadFile};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, Ordering};

/// What recovery did, for logs and tests.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RecoveryReport {
    /// Staging files verified against kind 28 and installed.
    pub installed: u32,
    /// Payloads whose length or digest disagrees with kind 28.
    pub corrupt: u32,
    /// Payloads a settlement names with no file.
    pub missing: u32,
    /// Unverified prefixes kept (interrupted, unsettled or `unconfirmed`).
    pub unverified: u32,
    /// Files no job derives, deleted (identity violation).
    pub orphans_deleted: u32,
    /// Stray staging files of `not_started` jobs or of installed objects, deleted.
    pub strays_deleted: u32,
    /// Files of jobs with a committed kind 30 `delete`, deleted: the
    /// completion of a deletion that a crash interrupted (J4).
    pub deleted: u32,
    /// Retained bytes after recovery.
    pub retained: u64,
}

/// Recovery failed: the payload store is unavailable; job starts wait with
/// `payload_unavailable`.
#[derive(Debug)]
pub enum RecoveryError {
    Cancelled,
    /// The journal could not answer a facts request.
    Journal,
    Payload(PayloadError),
}
impl From<PayloadError> for RecoveryError {
    fn from(error: PayloadError) -> Self {
        Self::Payload(error)
    }
}

fn list_all(
    store: &PayloadStore,
    directory: PayloadDirectory,
    cancel: &AtomicBool,
) -> Result<BTreeMap<PayloadId, u64>, RecoveryError> {
    let mut found = BTreeMap::new();
    let mut after = None;
    loop {
        if cancel.load(Ordering::Acquire) {
            return Err(RecoveryError::Cancelled);
        }
        let batch = store.root().list(directory, after)?;
        let Some(last) = batch.last() else {
            return Ok(found);
        };
        after = Some(last.id);
        for entry in batch {
            found.insert(entry.id, entry.bytes);
        }
    }
}
fn digest(file: &PayloadFile, cancel: &AtomicBool) -> Result<(u64, Hash), RecoveryError> {
    let mut hash = Sha256::new();
    let mut offset = 0u64;
    let mut buffer = vec![0u8; 64 * 1024];
    loop {
        if cancel.load(Ordering::Acquire) {
            return Err(RecoveryError::Cancelled);
        }
        let n = file.read_at(offset, &mut buffer)?;
        if n == 0 {
            return Ok((offset, hash.finalize().into()));
        }
        hash.update(&buffer[..n]);
        offset += n as u64;
    }
}

/// A settlement as recovery uses it: state, journal sequence, and stored
/// bytes and digest per stream, from the job's kind 28. An `interrupted`
/// kind 28 gives none.
type Settled = (CaptureState, u64, [(u64, Option<Hash>); 2]);

/// The replay facts recovery needs for one job. Built from a replay or the
/// journal's `PayloadFacts` on the caller's thread, so the recovery worker
/// never holds a replay snapshot (the journal writer allows only one
/// retained snapshot).
#[derive(Clone, Debug)]
pub struct Known {
    /// The job ID.
    call: Id,
    operation: Id,
    payloads: [PayloadId; 2],
    /// The kind 26 sequence.
    intent_sequence: u64,
    settled: Option<Settled>,
    /// The person pin hold from the job's kind 30 records (J4).
    pinned: bool,
    /// A committed kind 30 `delete` (J4).
    deleted: bool,
}

impl From<&crate::authority::writer::PayloadFact> for Known {
    fn from(fact: &crate::authority::writer::PayloadFact) -> Self {
        Self {
            call: fact.call,
            operation: fact.operation,
            payloads: fact.payloads,
            intent_sequence: fact.intent_sequence,
            settled: fact.settled,
            pinned: fact.pinned,
            deleted: fact.deleted,
        }
    }
}
/// Payload IDs asked of the journal in one `PayloadFacts` request.
pub const FACT_BATCH: usize = 128;
/// Recovery from the payload directories and the journal's `PayloadFacts`:
/// each file on disk is looked up in batches of 128. A fact of the job form
/// carries the job ID as `call`, the kind 26 sequence, the kind 28 capture
/// facts (none while unsettled or when `interrupted`) and the pin and
/// delete state. A file the journal does not derive is an orphan (segmented
/// journal, `PayloadFacts`).
pub fn recover_with_facts(
    store: &PayloadStore,
    facts: &mut dyn FnMut(
        Vec<PayloadId>,
    ) -> Result<Vec<crate::authority::writer::PayloadFact>, RecoveryError>,
    configured_quota: u64,
    cancel: &AtomicBool,
) -> Result<RecoveryReport, RecoveryError> {
    let staging = list_all(store, PayloadDirectory::Staging, cancel)?;
    let objects = list_all(store, PayloadDirectory::Objects, cancel)?;
    let ids: Vec<PayloadId> = staging
        .keys()
        .chain(objects.keys())
        .copied()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let mut known: BTreeMap<Id, Known> = BTreeMap::new();
    for batch in ids.chunks(FACT_BATCH) {
        if cancel.load(Ordering::Acquire) {
            return Err(RecoveryError::Cancelled);
        }
        for fact in facts(batch.to_vec())? {
            known.entry(fact.call).or_insert_with(|| Known::from(&fact));
        }
    }
    recover_known(
        store,
        known.into_values().collect(),
        configured_quota,
        cancel,
    )
}
/// The unsettled jobs a replay holds, for [`recover_known`] in tests. Every
/// `run` exec_cmd call is a job (J3-E9); a job settled in the delta is not
/// included, because a replay holds only its locators, and the production
/// path answers it through [`recover_with_facts`]. An unsettled job has no
/// kind 30 `delete`, and its pin is not read here: the replay holds no
/// index for the job's `E` entries.
pub fn known_calls(replay: &Replay) -> Vec<Known> {
    let mut known = Vec::new();
    // Unsettled jobs: an unverified prefix whose last use is kind 26.
    for job in replay.jobs.values() {
        // Person origin replays only after P2.
        let JobOrigin::Model { operation, .. } = job.origin else {
            continue;
        };
        known.push(Known {
            call: job.job,
            operation,
            payloads: job.payloads,
            intent_sequence: job.intent.sequence,
            settled: None,
            pinned: false,
            deleted: false,
        });
    }
    known
}

/// Rebuilds the ledger from replay and the payload directories and applies
/// the derived recovery actions, then sets the effective quota.
pub fn recover(
    store: &PayloadStore,
    replay: Option<&Replay>,
    configured_quota: u64,
    cancel: &AtomicBool,
) -> Result<RecoveryReport, RecoveryError> {
    let known = replay.map(known_calls).unwrap_or_default();
    recover_known(store, known, configured_quota, cancel)
}

/// [`recover`] from facts extracted with [`known_calls`].
pub fn recover_known(
    store: &PayloadStore,
    known: Vec<Known>,
    configured_quota: u64,
    cancel: &AtomicBool,
) -> Result<RecoveryReport, RecoveryError> {
    let mut staging = list_all(store, PayloadDirectory::Staging, cancel)?;
    let mut objects = list_all(store, PayloadDirectory::Objects, cancel)?;
    let mut report = RecoveryReport::default();
    let named: BTreeSet<PayloadId> = known.iter().flat_map(|k| k.payloads).collect();
    // Orphans: no job derives them.
    for (directory, files) in [
        (PayloadDirectory::Staging, &mut staging),
        (PayloadDirectory::Objects, &mut objects),
    ] {
        let orphans: Vec<PayloadId> = files
            .keys()
            .filter(|id| !named.contains(*id))
            .copied()
            .collect();
        for id in orphans {
            match directory {
                PayloadDirectory::Staging => store.root().remove_staging(id)?,
                PayloadDirectory::Objects => store.root().remove_object(id)?,
            }
            files.remove(&id);
            report.orphans_deleted += 1;
        }
    }
    for call in known {
        if cancel.load(Ordering::Acquire) {
            return Err(RecoveryError::Cancelled);
        }
        let mut payloads = [(
            call.payloads[0],
            PayloadState::Missing,
            Location::None,
            0,
            None,
        ); 2];
        // A committed kind 30 `delete`: complete the deletion that a crash
        // interrupted. Pages answer `deleted`; nothing is retained.
        if call.deleted {
            for id in call.payloads {
                if staging.remove(&id).is_some() {
                    store.root().remove_staging(id)?;
                    report.deleted += 1;
                }
                if objects.remove(&id).is_some() {
                    store.root().remove_object(id)?;
                    report.deleted += 1;
                }
            }
            store.recovered_deleted(call.payloads);
            continue;
        }
        let mut on_disk = 0u64;
        let mut last_use = call.intent_sequence;
        match call.settled {
            Some((CaptureState::NotStarted, _, _)) => {
                for id in call.payloads {
                    if staging.remove(&id).is_some() {
                        store.root().remove_staging(id)?;
                        report.strays_deleted += 1;
                    }
                    if objects.remove(&id).is_some() {
                        store.root().remove_object(id)?;
                        report.strays_deleted += 1;
                    }
                }
                continue;
            }
            Some((
                state @ (CaptureState::Complete | CaptureState::Incomplete),
                sequence,
                streams,
            )) => {
                last_use = sequence;
                for (index, id) in call.payloads.into_iter().enumerate() {
                    let (recorded, digest_value) = streams[index];
                    let mut entry = (
                        id,
                        PayloadState::from_capture(state),
                        Location::None,
                        recorded,
                        digest_value,
                    );
                    if let Some(bytes) = objects.get(&id).copied() {
                        entry.2 = Location::Objects;
                        on_disk += bytes;
                        if bytes != recorded {
                            entry.1 = PayloadState::Corrupt;
                            report.corrupt += 1;
                        }
                        if staging.remove(&id).is_some() {
                            store.root().remove_staging(id)?;
                            report.strays_deleted += 1;
                        }
                    } else if let Some(bytes) = staging.get(&id).copied() {
                        let file = store.root().open_staging(id)?;
                        let (length, actual) = digest(&file, cancel)?;
                        drop(file);
                        if length == recorded && Some(actual) == digest_value {
                            store.root().install(id)?;
                            entry.2 = Location::Objects;
                            report.installed += 1;
                        } else {
                            // The file stays for expiry and never claims verified content.
                            entry.1 = PayloadState::Corrupt;
                            entry.2 = Location::Staging;
                            report.corrupt += 1;
                        }
                        on_disk += bytes;
                    } else {
                        entry.1 = PayloadState::Missing;
                        report.missing += 1;
                    }
                    payloads[index] = entry;
                }
            }
            // Interrupted (no kind 28, or an `interrupted` one) or
            // `unconfirmed`: an unverified prefix. Its last use is the
            // kind 26 sequence.
            None | Some((CaptureState::Unconfirmed, _, _)) => {
                for (index, id) in call.payloads.into_iter().enumerate() {
                    let (location, bytes) = if let Some(bytes) = staging.get(&id).copied() {
                        (Location::Staging, bytes)
                    } else if let Some(bytes) = objects.get(&id).copied() {
                        (Location::Objects, bytes)
                    } else {
                        (Location::None, 0)
                    };
                    on_disk += bytes;
                    payloads[index] = (
                        id,
                        if location == Location::None {
                            PayloadState::Missing
                        } else {
                            PayloadState::UnverifiedPrefix
                        },
                        location,
                        bytes,
                        None,
                    );
                    if location != Location::None {
                        report.unverified += 1;
                    }
                }
            }
        }
        // After a restart no job is live, so every recovered one is settled,
        // unheld and expirable (its operation has ended; a job left
        // unsettled is settled `interrupted` by the controller), unless the
        // person pinned it.
        store.recovered(call.call, call.operation, payloads, last_use, on_disk, true);
        if call.pinned {
            store.set_pinned(call.call, true);
        }
        report.retained += on_disk;
    }
    store.set_quota(configured_quota)?;
    Ok(report)
}
