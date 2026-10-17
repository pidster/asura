//! Derived recovery (J1): restart writes no record for an exec_cmd call. Replay
//! and the payload directories derive each call's capture state. Runs on the
//! retained recovery worker with no time bound; shutdown cancels it between
//! steps.
use super::store::{Location, PayloadId, PayloadState, PayloadStore};
use crate::authority::conversation::{CaptureState, ExecCmdDisposition, Hash, Id, Replay};
use asura_platform::payload_io::{PayloadDirectory, PayloadError, PayloadFile};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, Ordering};

/// What recovery did, for logs and tests.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RecoveryReport {
    /// Staging files verified against kind 23 and installed.
    pub installed: u32,
    /// Payloads whose length or digest disagrees with kind 23.
    pub corrupt: u32,
    /// Payloads kind 23 names with no file.
    pub missing: u32,
    /// Unverified prefixes kept (interrupted or `unconfirmed`).
    pub unverified: u32,
    /// Files no kind 22 names, deleted (identity violation).
    pub orphans_deleted: u32,
    /// Stray staging files of `not_started` calls or of installed objects, deleted.
    pub strays_deleted: u32,
    /// Retained bytes after recovery.
    pub retained: u64,
}

/// Recovery failed: the payload store is unavailable, and exec_cmd admission
/// answers `busy` with `payload_unavailable`.
#[derive(Debug)]
pub enum RecoveryError {
    Cancelled,
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

/// Kind 23 as recovery uses it: state, journal sequence, and stored bytes
/// and digest per stream.
type Settled = (CaptureState, u64, [(u64, Option<Hash>); 2]);

/// The replay facts recovery needs for one admitted exec_cmd call. Built from a
/// replay on the caller's thread, so the recovery worker never holds a
/// replay snapshot (the journal writer allows only one retained snapshot).
#[derive(Clone, Debug)]
pub struct Known {
    call: Id,
    operation: Id,
    payloads: [PayloadId; 2],
    intent_sequence: u64,
    settled: Option<Settled>,
}

/// The exec_cmd calls a replay names, for [`recover_known`].
pub fn known_calls(replay: &Replay) -> Vec<Known> {
    let mut known = Vec::new();
    for (operation, op) in &replay.operations {
        for tool in &op.tools {
            let Some(call) = tool.exec_cmd.as_deref() else {
                continue;
            };
            if call.disposition != ExecCmdDisposition::Admitted {
                continue;
            }
            known.push(Known {
                call: call.job,
                operation: *operation,
                payloads: [call.stdout_payload, call.stderr_payload],
                intent_sequence: call.intent_sequence,
                settled: call
                    .settled
                    .as_ref()
                    .map(|s| (s.state, s.sequence, s.streams)),
            });
        }
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
    // Orphans: no kind 22 names them.
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
            // Interrupted (no kind 23) or `unconfirmed`: an unverified prefix.
            // Its last use is the kind 22 sequence.
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
        // After a restart no call is live, so every recovered call is
        // settled, unheld and expirable (its operation has ended).
        store.recovered(call.call, call.operation, payloads, last_use, on_disk, true);
        report.retained += on_disk;
    }
    store.set_quota(configured_quota)?;
    Ok(report)
}
