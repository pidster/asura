//! The payload store (J1): quota ledger, catalog, staging captures and the
//! interim quota-pressure expiry (J1-Q2). The state mutex is held only for
//! arithmetic and map updates, never across I/O (AE-01).
use super::id::{ObjectKind, payload_id};
use crate::authority::conversation::{CaptureState, Hash, Id};
use asura_platform::RuntimeDirectory;
use asura_platform::payload_io::{PayloadError, PayloadFile, PayloadRoot, StagingFile};
use asura_platform::secrets::Redacted;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};

pub const MIB: u64 = 1024 * 1024;
/// R1: the initial reservation and each growth step.
pub const RESERVATION_STEP: u64 = MIB;
/// Free disk space kept for recovery; capture pauses below it.
pub const RECOVERY_RESERVE: u64 = 16 * MIB;
/// `retention.quotaBytes` default (R1) and bounds (proposed).
pub const DEFAULT_QUOTA: u64 = 1024 * MIB;
pub const MIN_QUOTA: u64 = 64 * MIB;
pub const MAX_QUOTA: u64 = 1024 * 1024 * MIB;
/// Calls deleted per expiry attempt (proposed).
pub const MAX_EXPIRY_PER_ATTEMPT: usize = 128;

pub type PayloadId = Id;

/// The effective quota: the configured value or retained bytes plus free disk
/// space minus the recovery reserve, whichever is smaller.
pub fn effective_quota(configured: u64, retained: u64, free: u64) -> u64 {
    configured.min(
        retained
            .saturating_add(free)
            .saturating_sub(RECOVERY_RESERVE),
    )
}

/// A page or catalog state of one payload.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PayloadState {
    /// A capture is writing it; pages stop at the watermark.
    Live,
    Complete,
    Incomplete,
    /// Staging bytes of an interrupted or `unconfirmed` capture.
    UnverifiedPrefix,
    /// The recorded length or digest disagrees with the file.
    Corrupt,
    /// The journal names it but no file exists.
    Missing,
}
impl PayloadState {
    pub fn from_capture(state: CaptureState) -> Self {
        match state {
            CaptureState::Complete => Self::Complete,
            CaptureState::Incomplete => Self::Incomplete,
            CaptureState::Unconfirmed | CaptureState::NotStarted => Self::UnverifiedPrefix,
        }
    }
}
/// Where a payload's bytes are.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Location {
    Staging,
    Objects,
    None,
}

#[derive(Clone, Debug)]
pub(super) struct PayloadEntry {
    pub call: Id,
    pub operation: Id,
    pub state: PayloadState,
    pub location: Location,
    pub total: u64,
    pub digest: Option<Hash>,
    pub watermark: Option<Arc<AtomicU64>>,
}
#[derive(Clone, Debug)]
struct CallEntry {
    payloads: [PayloadId; 2],
    /// Reservation while live; 0 after settlement.
    reservation: u64,
    /// Bytes counted as retained after settlement or recovery.
    retained: u64,
    /// Journal sequence of kind 23 (or kind 22 for an unverified prefix).
    last_use: u64,
    settled: bool,
    expirable: bool,
    holds: u32,
    deleting: bool,
}

/// One call chosen for interim expiry. Its bytes already count as free; the
/// caller deletes both files and then calls [`PayloadStore::expired`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Victim {
    pub call: Id,
    pub payloads: [PayloadId; 2],
}

/// Why the ledger could not reserve.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Short {
    /// Quota is short and nothing expirable covers the step.
    Quota,
}

/// Notified on an event that may free space (J1 retry).
pub trait SpaceListener: Send + Sync {
    fn space_event(&self);
}

#[derive(Default)]
struct State {
    quota: u64,
    used: u64,
    calls: BTreeMap<Id, CallEntry>,
    payloads: BTreeMap<PayloadId, PayloadEntry>,
    expired: BTreeSet<PayloadId>,
    listeners: Vec<Weak<dyn SpaceListener>>,
}
impl State {
    fn available(&self) -> u64 {
        self.quota.saturating_sub(self.used)
    }
    /// Chooses expirable calls, oldest last use first, then larger size,
    /// then payload ID, until `need` is available; at most 128 calls.
    fn select(&self, need: u64) -> Option<Vec<Id>> {
        let mut available = self.available();
        if available >= need {
            return Some(Vec::new());
        }
        let mut candidates: Vec<(&Id, &CallEntry)> = self
            .calls
            .iter()
            .filter(|(_, c)| c.settled && c.expirable && c.holds == 0 && !c.deleting)
            .collect();
        candidates.sort_by(|(_, a), (_, b)| {
            a.last_use
                .cmp(&b.last_use)
                .then(b.retained.cmp(&a.retained))
                .then(a.payloads[0].cmp(&b.payloads[0]))
        });
        let mut chosen = Vec::new();
        for (id, call) in candidates.into_iter().take(MAX_EXPIRY_PER_ATTEMPT) {
            chosen.push(*id);
            available = available.saturating_add(call.retained);
            if available >= need {
                return Some(chosen);
            }
        }
        None
    }
    fn take_victims(&mut self, chosen: Vec<Id>) -> Vec<Victim> {
        chosen
            .into_iter()
            .map(|id| {
                let call = self.calls.get_mut(&id).expect("chosen call");
                call.deleting = true;
                self.used = self.used.saturating_sub(call.retained);
                Victim {
                    call: id,
                    payloads: call.payloads,
                }
            })
            .collect()
    }
    fn notify(&mut self) -> Vec<Arc<dyn SpaceListener>> {
        self.listeners.retain(|l| l.strong_count() > 0);
        self.listeners.iter().filter_map(Weak::upgrade).collect()
    }
}

/// The in-memory quota ledger and catalog. Its mutex guards arithmetic and
/// map updates only.
#[derive(Default)]
pub struct Ledger {
    state: Mutex<State>,
}
fn lock(ledger: &Ledger) -> std::sync::MutexGuard<'_, State> {
    ledger.state.lock().unwrap_or_else(|e| e.into_inner())
}
impl Ledger {
    /// Sets the effective quota from the configured value and free space.
    pub fn set_effective_quota(&self, configured: u64, free: u64) -> u64 {
        let mut state = lock(self);
        let retained: u64 = state.calls.values().map(|c| c.retained).sum();
        state.quota = effective_quota(configured, retained, free);
        state.quota
    }
    pub fn quota(&self) -> u64 {
        lock(self).quota
    }
    /// Used bytes: retained bytes plus reservations.
    pub fn used(&self) -> u64 {
        lock(self).used
    }
    /// Registers a listener for space events for as long as it lives.
    pub fn listen(&self, listener: &Arc<dyn SpaceListener>) {
        lock(self).listeners.push(Arc::downgrade(listener));
    }
    /// An event that may free space: wakes every paused capture at once.
    pub fn space_event(&self) {
        let listeners = lock(self).notify();
        for listener in listeners {
            listener.space_event();
        }
    }
    /// Reactor admission (arithmetic only): reserves 1 MiB for a new call.
    /// Expirable calls may count as available; they are marked for deletion
    /// and returned for the new capture worker to delete before it creates
    /// its staging files.
    pub fn admit(
        &self,
        installation: Id,
        operation: Id,
        generation: u64,
        ordinal: u32,
        call: Id,
    ) -> Result<Vec<Victim>, Short> {
        let mut state = lock(self);
        let chosen = state.select(RESERVATION_STEP).ok_or(Short::Quota)?;
        let victims = state.take_victims(chosen);
        state.used += RESERVATION_STEP;
        let payloads = [
            payload_id(
                installation,
                operation,
                generation,
                ordinal,
                ObjectKind::Stdout,
            ),
            payload_id(
                installation,
                operation,
                generation,
                ordinal,
                ObjectKind::Stderr,
            ),
        ];
        state.calls.insert(
            call,
            CallEntry {
                payloads,
                reservation: RESERVATION_STEP,
                retained: 0,
                last_use: 0,
                settled: false,
                expirable: false,
                holds: 0,
                deleting: false,
            },
        );
        for id in payloads {
            state.payloads.insert(
                id,
                PayloadEntry {
                    call,
                    operation,
                    state: PayloadState::Live,
                    location: Location::None,
                    total: 0,
                    digest: None,
                    watermark: Some(Arc::new(AtomicU64::new(0))),
                },
            );
        }
        Ok(victims)
    }
    /// Grows a live call's reservation by one step; may choose victims.
    pub fn grow(&self, call: Id) -> Result<Vec<Victim>, Short> {
        let mut state = lock(self);
        // Only a live call grows; an unknown or settled call reserves nothing.
        if state.calls.get(&call).is_none_or(|entry| entry.settled) {
            return Err(Short::Quota);
        }
        let chosen = state.select(RESERVATION_STEP).ok_or(Short::Quota)?;
        let victims = state.take_victims(chosen);
        state.used += RESERVATION_STEP;
        if let Some(entry) = state.calls.get_mut(&call) {
            entry.reservation += RESERVATION_STEP;
        }
        Ok(victims)
    }
    /// The call's current reservation.
    pub fn reservation(&self, call: Id) -> u64 {
        lock(self).calls.get(&call).map_or(0, |c| c.reservation)
    }
    /// Records the outcome of a victim deletion: success expires both
    /// payloads for this service run; failure counts the bytes again and
    /// stops choosing the call.
    pub fn victim_deleted(&self, victim: Victim, deleted: bool) {
        let mut state = lock(self);
        if deleted {
            state.calls.remove(&victim.call);
            for id in victim.payloads {
                state.payloads.remove(&id);
                state.expired.insert(id);
            }
        } else if let Some(call) = state.calls.get_mut(&victim.call) {
            call.deleting = false;
            call.expirable = false;
            let retained = call.retained;
            state.used = state.used.saturating_add(retained);
        }
        drop(state);
        if deleted {
            self.space_event();
        }
    }
    /// An admission that never started (cancelled or failed before kind 22
    /// committed): its victims are restored, uncounted as free again.
    pub fn restore_victims(&self, victims: &[Victim]) {
        let mut state = lock(self);
        for victim in victims {
            if let Some(call) = state.calls.get_mut(&victim.call)
                && call.deleting
            {
                call.deleting = false;
                let retained = call.retained;
                state.used = state.used.saturating_add(retained);
            }
        }
    }
    /// Releases a call that never staged anything (`not_started`): its
    /// reservation returns and its payloads leave.
    pub fn release(&self, call: Id) {
        let mut state = lock(self);
        if let Some(entry) = state.calls.remove(&call) {
            state.used = state
                .used
                .saturating_sub(entry.reservation + entry.retained);
            for id in entry.payloads {
                state.payloads.remove(&id);
            }
        }
        drop(state);
        self.space_event();
    }
    /// Settlement after kind 23 commits: the reservation is released and the
    /// stored bytes count as retained. `held` keeps it from expiry while a
    /// pending install or an abandoned worker owns it.
    pub fn settle(
        &self,
        call: Id,
        capture: CaptureState,
        streams: [(u64, Option<Hash>); 2],
        last_use: u64,
        held: bool,
    ) {
        let mut state = lock(self);
        let Some(entry) = state.calls.get_mut(&call) else {
            return;
        };
        let reservation = std::mem::take(&mut entry.reservation);
        let retained = streams[0].0 + streams[1].0;
        entry.retained = retained;
        entry.last_use = last_use;
        entry.settled = true;
        entry.expirable = !matches!(capture, CaptureState::Unconfirmed);
        if held {
            entry.holds += 1;
        }
        let payloads = entry.payloads;
        state.used = state
            .used
            .saturating_sub(reservation)
            .saturating_add(retained);
        for (index, id) in payloads.into_iter().enumerate() {
            if let Some(payload) = state.payloads.get_mut(&id) {
                payload.state = PayloadState::from_capture(capture);
                payload.total = streams[index].0;
                payload.digest = streams[index].1;
                // An `unconfirmed` settlement leaves the worker writing: its
                // pages keep following the live watermark until `abandoned`.
                if capture != CaptureState::Unconfirmed {
                    payload.watermark = None;
                }
            }
        }
        drop(state);
        self.space_event();
    }
    /// The stored bytes of an abandoned worker's prefix, counted as retained.
    pub fn abandoned(&self, call: Id, stored: u64) {
        let mut state = lock(self);
        if let Some(entry) = state.calls.get_mut(&call) {
            let before = entry.retained;
            entry.retained = stored;
            state.used = state.used.saturating_sub(before).saturating_add(stored);
        }
        for payload in state.payloads.values_mut().filter(|p| p.call == call) {
            payload.total = payload
                .watermark
                .as_ref()
                .map_or(payload.total, |w| w.load(Ordering::Acquire));
            payload.watermark = None;
        }
    }
    /// Marks an unverified prefix expirable once its operation has ended.
    pub fn operation_ended(&self, operation: Id) {
        let mut state = lock(self);
        let calls: BTreeSet<Id> = state
            .payloads
            .values()
            .filter(|p| p.operation == operation)
            .map(|p| p.call)
            .collect();
        for call in calls {
            if let Some(entry) = state.calls.get_mut(&call)
                && entry.settled
            {
                entry.expirable = true;
            }
        }
        drop(state);
        self.space_event();
    }
    /// A pending-install entry, an abandoned worker or a page read released
    /// the call.
    pub fn unhold(&self, call: Id) {
        let mut state = lock(self);
        if let Some(entry) = state.calls.get_mut(&call) {
            entry.holds = entry.holds.saturating_sub(1);
        }
        drop(state);
        self.space_event();
    }
    /// Holds a payload's call against expiry for a page read.
    pub(super) fn hold_payload(&self, id: PayloadId) -> Option<PayloadEntry> {
        let mut state = lock(self);
        let entry = state.payloads.get(&id)?.clone();
        let call = state.calls.get_mut(&entry.call)?;
        if call.deleting {
            return None;
        }
        call.holds += 1;
        Some(entry)
    }
    pub(super) fn release_payload(&self, id: PayloadId) {
        let call = lock(self).payloads.get(&id).map(|p| p.call);
        if let Some(call) = call {
            self.unhold(call);
        }
    }
    pub(super) fn was_expired(&self, id: PayloadId) -> bool {
        lock(self).expired.contains(&id)
    }
    /// The catalog entry of one payload: state, location and total bytes.
    pub fn payload_state(&self, id: PayloadId) -> Option<(PayloadState, Location, u64)> {
        lock(self)
            .payloads
            .get(&id)
            .map(|p| (p.state, p.location, p.total))
    }
    fn set_location(&self, ids: &[PayloadId], location: Location) {
        let mut state = lock(self);
        for id in ids {
            if let Some(payload) = state.payloads.get_mut(id) {
                payload.location = location;
            }
        }
    }
    fn payloads_of(&self, call: Id) -> Option<[PayloadId; 2]> {
        lock(self).calls.get(&call).map(|c| c.payloads)
    }
    /// Recovery: registers a retained call found on disk.
    pub(super) fn recovered(
        &self,
        call: Id,
        operation: Id,
        payloads: [(PayloadId, PayloadState, Location, u64, Option<Hash>); 2],
        last_use: u64,
        bytes_on_disk: u64,
        expirable: bool,
    ) {
        let mut state = lock(self);
        state.calls.insert(
            call,
            CallEntry {
                payloads: [payloads[0].0, payloads[1].0],
                reservation: 0,
                retained: bytes_on_disk,
                last_use,
                settled: true,
                expirable,
                holds: 0,
                deleting: false,
            },
        );
        state.used += bytes_on_disk;
        for (id, payload_state, location, total, digest) in payloads {
            state.payloads.insert(
                id,
                PayloadEntry {
                    call,
                    operation,
                    state: payload_state,
                    location,
                    total,
                    digest,
                    watermark: None,
                },
            );
        }
    }
    /// The live watermark of a payload being captured.
    pub(super) fn watermark(&self, id: PayloadId) -> Option<Arc<AtomicU64>> {
        lock(self)
            .payloads
            .get(&id)
            .and_then(|p| p.watermark.clone())
    }
}

/// The payload store. Shared by the reactor (arithmetic only), capture
/// workers, page-read workers and the recovery worker.
pub struct PayloadStore {
    root: PayloadRoot,
    ledger: Ledger,
}
impl std::ops::Deref for PayloadStore {
    type Target = Ledger;
    fn deref(&self) -> &Ledger {
        &self.ledger
    }
}
impl PayloadStore {
    /// Blocking: opens (and creates) the payload directories. Recovery or
    /// worker thread only. The quota starts at 0 until [`Self::set_quota`].
    pub fn open(runtime: &RuntimeDirectory) -> Result<Self, PayloadError> {
        Ok(Self {
            root: PayloadRoot::open(runtime, true)?,
            ledger: Ledger::default(),
        })
    }
    pub(super) fn root(&self) -> &PayloadRoot {
        &self.root
    }
    /// Sets the effective quota from the configured value, the retained bytes
    /// and the volume's free space (blocking `fstatfs`).
    pub fn set_quota(&self, configured: u64) -> Result<u64, PayloadError> {
        let free = self.root.free_bytes()?;
        Ok(self.ledger.set_effective_quota(configured, free))
    }
    /// Blocking: free bytes on the payload volume.
    pub fn free_bytes(&self) -> Result<u64, PayloadError> {
        self.root.free_bytes()
    }
    /// Blocking: deletes a victim's files (objects or staging) and records
    /// both payloads as expired for this service run.
    pub fn delete_victim(&self, victim: Victim) -> Result<(), PayloadError> {
        let mut result = Ok(());
        for id in victim.payloads {
            for removal in [PayloadRoot::remove_object, PayloadRoot::remove_staging] {
                match removal(&self.root, id) {
                    Ok(()) | Err(PayloadError::Absent) => {}
                    Err(error) => result = Err(error),
                }
            }
        }
        self.ledger.victim_deleted(victim, result.is_ok());
        result
    }
    /// Blocking: installs both staging files of a settled call.
    pub fn install(&self, call: Id) -> Result<(), PayloadError> {
        let Some(payloads) = self.ledger.payloads_of(call) else {
            return Ok(());
        };
        for id in payloads {
            match self.root.install(id) {
                Ok(()) => {}
                // Absent with an object present: already installed.
                Err(PayloadError::Absent) if self.root.open_object(id).is_ok() => {}
                Err(error) => return Err(error),
            }
            self.ledger.set_location(&[id], Location::Objects);
        }
        Ok(())
    }
    /// Blocking: removes both staging files of a call that never ran
    /// (`not_started`). A missing file is fine.
    pub fn discard_staging(&self, call: Id) {
        if let Some(payloads) = self.ledger.payloads_of(call) {
            for id in payloads {
                let _ = self.root.remove_staging(id);
            }
            self.ledger.set_location(&payloads, Location::None);
        }
    }
    /// Blocking: creates both staging files of an admitted call (exclusive).
    pub fn create_capture(self: &Arc<Self>, call: Id) -> Result<Capture, PayloadError> {
        let payloads = self.ledger.payloads_of(call).ok_or(PayloadError::Absent)?;
        let watermarks = [
            self.ledger.watermark(payloads[0]).unwrap_or_default(),
            self.ledger.watermark(payloads[1]).unwrap_or_default(),
        ];
        let stdout = self.root.create_staging(payloads[0])?;
        let stderr = match self.root.create_staging(payloads[1]) {
            Ok(file) => file,
            Err(error) => {
                drop(stdout);
                let _ = self.root.remove_staging(payloads[0]);
                return Err(error);
            }
        };
        self.ledger.set_location(&payloads, Location::Staging);
        let [a, b] = watermarks;
        Ok(Capture {
            store: self.clone(),
            call,
            streams: [StreamFile::new(stdout, a), StreamFile::new(stderr, b)],
        })
    }
    /// Blocking: opens a payload for reading from wherever it is.
    pub(super) fn open_read(
        &self,
        id: PayloadId,
        location: Location,
    ) -> Result<PayloadFile, PayloadError> {
        match location {
            Location::Objects => self
                .root
                .open_object(id)
                .or_else(|_| self.root.open_staging(id)),
            Location::Staging => self
                .root
                .open_staging(id)
                .or_else(|_| self.root.open_object(id)),
            Location::None => Err(PayloadError::Absent),
        }
    }
}

struct StreamFile {
    file: StagingFile,
    hash: Sha256,
    watermark: Arc<AtomicU64>,
}
impl StreamFile {
    fn new(file: StagingFile, watermark: Arc<AtomicU64>) -> Self {
        Self {
            file,
            hash: Sha256::new(),
            watermark,
        }
    }
}

/// Stream index: 0 stdout, 1 stderr.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StreamIndex {
    Stdout = 0,
    Stderr = 1,
}

/// The staging pair of one live call. The only write path is
/// [`Capture::append`], which takes [`Redacted`] bytes; `Redacted` has no
/// public constructor, so raw output can never be staged.
pub struct Capture {
    store: Arc<PayloadStore>,
    call: Id,
    streams: [StreamFile; 2],
}
impl Capture {
    pub fn call(&self) -> Id {
        self.call
    }
    /// Appends redacted bytes to one stream, updates its SHA-256 over exactly
    /// the bytes written and raises the watermark after the write returns.
    /// After an error the file and digest hold the written prefix.
    ///
    /// Writes `bytes[from..]` and returns how many bytes this call wrote,
    /// with the error if it stopped early (`NoSpace` pauses the capture and
    /// a retry continues from the returned position).
    pub fn append(
        &mut self,
        stream: StreamIndex,
        bytes: &Redacted,
        from: usize,
    ) -> (usize, Result<(), PayloadError>) {
        let s = &mut self.streams[stream as usize];
        let source = &bytes.as_bytes()[from.min(bytes.len())..];
        let before = s.file.len();
        let result = s.file.append(source);
        let written = (s.file.len() - before) as usize;
        s.hash.update(&source[..written]);
        s.watermark.store(s.file.len(), Ordering::Release);
        (written, result)
    }
    /// Stored bytes per stream.
    pub fn stored(&self, stream: StreamIndex) -> u64 {
        self.streams[stream as usize].file.len()
    }
    pub fn total(&self) -> u64 {
        self.stored(StreamIndex::Stdout) + self.stored(StreamIndex::Stderr)
    }
    /// `fsync` of both staging files.
    pub fn sync(&self) -> Result<(), PayloadError> {
        self.streams[0].file.sync()?;
        self.streams[1].file.sync()
    }
    /// Closes both files and returns stored bytes and digests per stream.
    pub fn seal(self) -> [(u64, Hash); 2] {
        let [a, b] = self.streams;
        [
            (a.file.len(), a.hash.finalize().into()),
            (b.file.len(), b.hash.finalize().into()),
        ]
    }
    pub fn store(&self) -> &Arc<PayloadStore> {
        &self.store
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn effective_quota_is_bounded_by_disk_and_configuration() {
        assert_eq!(effective_quota(DEFAULT_QUOTA, 0, 100 * MIB), 84 * MIB);
        assert_eq!(effective_quota(DEFAULT_QUOTA, 0, 10 * MIB), 0);
        assert_eq!(effective_quota(64 * MIB, 10 * MIB, 10_000 * MIB), 64 * MIB);
        assert_eq!(
            effective_quota(DEFAULT_QUOTA, 40 * MIB, 100 * MIB),
            124 * MIB
        );
    }

    const INSTALLATION: Id = [1; 16];
    fn call_id(n: u32) -> Id {
        let mut id = [0xC0; 16];
        id[12..].copy_from_slice(&n.to_be_bytes());
        id
    }
    fn operation(n: u32) -> Id {
        let mut id = [0x0F; 16];
        id[12..].copy_from_slice(&n.to_be_bytes());
        id
    }
    fn stream_id(n: u32, kind: ObjectKind) -> PayloadId {
        payload_id(INSTALLATION, operation(n), 1, 1, kind)
    }
    fn admit(ledger: &Ledger, n: u32) -> Result<Vec<Victim>, Short> {
        ledger.admit(INSTALLATION, operation(n), 1, 1, call_id(n))
    }
    fn streams(stdout: u64, stderr: u64) -> [(u64, Option<Hash>); 2] {
        [(stdout, Some([1; 32])), (stderr, Some([2; 32]))]
    }
    fn settle(ledger: &Ledger, n: u32, bytes: u64, last_use: u64) {
        ledger.settle(
            call_id(n),
            CaptureState::Complete,
            streams(bytes, 0),
            last_use,
            false,
        );
    }
    /// Sets the quota to the used bytes plus `spare`.
    fn tighten(ledger: &Ledger, spare: u64) {
        let quota = ledger.used() + spare;
        assert_eq!(ledger.set_effective_quota(quota, u64::MAX / 4), quota);
    }
    fn victim_calls(victims: &[Victim]) -> Vec<Id> {
        victims.iter().map(|v| v.call).collect()
    }
    fn delete_all(ledger: &Ledger, victims: &[Victim]) {
        for victim in victims {
            ledger.victim_deleted(*victim, true);
        }
    }

    #[test]
    fn j1t_08_effective_quota_saturates_and_counts_retained_bytes() {
        assert_eq!(
            effective_quota(u64::MAX, u64::MAX, u64::MAX),
            u64::MAX - RECOVERY_RESERVE
        );
        assert_eq!(effective_quota(MIN_QUOTA, 0, RECOVERY_RESERVE), 0);
        assert_eq!(effective_quota(MIN_QUOTA, 0, RECOVERY_RESERVE + 1), 1);
        assert_eq!(effective_quota(MIN_QUOTA, 5, RECOVERY_RESERVE), 5);
        let ledger = Ledger::default();
        assert_eq!(ledger.quota(), 0);
        assert_eq!(admit(&ledger, 1), Err(Short::Quota));
        assert_eq!(
            ledger.set_effective_quota(DEFAULT_QUOTA, u64::MAX),
            DEFAULT_QUOTA
        );
        admit(&ledger, 1).unwrap();
        settle(&ledger, 1, 3 * MIB, 1);
        // Retained bytes count; the live reservation of call 2 does not.
        admit(&ledger, 2).unwrap();
        assert_eq!(
            ledger.set_effective_quota(DEFAULT_QUOTA, 20 * MIB),
            3 * MIB + 20 * MIB - RECOVERY_RESERVE
        );
        assert_eq!(ledger.quota(), 7 * MIB);
    }

    #[test]
    fn j1t_08_concurrent_admission_and_growth_never_over_reserve() {
        use std::sync::atomic::{AtomicBool, AtomicU32};
        let ledger = Arc::new(Ledger::default());
        ledger.set_effective_quota(32 * MIB, u64::MAX / 4);
        let done = Arc::new(AtomicBool::new(false));
        let monitor = {
            let (ledger, done) = (ledger.clone(), done.clone());
            std::thread::spawn(move || {
                let mut samples = 0u64;
                while !done.load(Ordering::Acquire) {
                    assert!(ledger.used() <= ledger.quota());
                    samples += 1;
                }
                samples
            })
        };
        let next = Arc::new(AtomicU32::new(0));
        let workers: Vec<_> = (0..8)
            .map(|_| {
                let (ledger, next) = (ledger.clone(), next.clone());
                std::thread::spawn(move || {
                    let mut grants = Vec::new();
                    loop {
                        let n = next.fetch_add(1, Ordering::Relaxed);
                        match admit(&ledger, n) {
                            Ok(victims) => assert!(victims.is_empty()),
                            Err(Short::Quota) => return grants,
                        }
                        let mut steps = 1u64;
                        while steps < 4 && ledger.grow(call_id(n)).is_ok() {
                            steps += 1;
                        }
                        grants.push((n, steps));
                    }
                })
            })
            .collect();
        let grants: Vec<(u32, u64)> = workers
            .into_iter()
            .flat_map(|w| w.join().unwrap())
            .collect();
        done.store(true, Ordering::Release);
        assert!(monitor.join().unwrap() > 0);
        let granted: u64 = grants.iter().map(|(_, steps)| steps).sum();
        assert_eq!(granted, 32, "every MiB of the quota is granted once");
        assert_eq!(ledger.used(), 32 * MIB);
        for (n, steps) in &grants {
            assert_eq!(ledger.reservation(call_id(*n)), steps * MIB);
        }
        assert_eq!(ledger.grow(call_id(grants[0].0)), Err(Short::Quota));
    }

    #[test]
    fn j1t_08_concurrent_admission_growth_settlement_and_expiry_stay_within_quota() {
        use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64};
        let ledger = Arc::new(Ledger::default());
        ledger.set_effective_quota(48 * MIB, u64::MAX / 4);
        let done = Arc::new(AtomicBool::new(false));
        let monitor = {
            let (ledger, done) = (ledger.clone(), done.clone());
            std::thread::spawn(move || {
                while !done.load(Ordering::Acquire) {
                    assert!(ledger.used() <= ledger.quota());
                }
            })
        };
        let next = Arc::new(AtomicU32::new(0));
        let sequence = Arc::new(AtomicU64::new(0));
        // Retained bytes per surviving settled call, recorded before settling
        // so that a concurrent victim choice always finds the entry.
        let retained = Arc::new(Mutex::new(BTreeMap::<Id, u64>::new()));
        let expire = |ledger: &Ledger, retained: &Mutex<BTreeMap<Id, u64>>, victims: &[Victim]| {
            for victim in victims {
                retained.lock().unwrap().remove(&victim.call);
                ledger.victim_deleted(*victim, true);
            }
        };
        let workers: Vec<_> = (0..8u64)
            .map(|t| {
                let (ledger, next, sequence, retained) = (
                    ledger.clone(),
                    next.clone(),
                    sequence.clone(),
                    retained.clone(),
                );
                std::thread::spawn(move || {
                    for i in 0..60u64 {
                        let n = next.fetch_add(1, Ordering::Relaxed);
                        let victims = loop {
                            match admit(&ledger, n) {
                                Ok(victims) => break victims,
                                Err(Short::Quota) => std::thread::yield_now(),
                            }
                        };
                        expire(&ledger, &retained, &victims);
                        let mut reservation = MIB;
                        for _ in 0..(i % 3) {
                            match ledger.grow(call_id(n)) {
                                Ok(victims) => {
                                    expire(&ledger, &retained, &victims);
                                    reservation += MIB;
                                }
                                Err(Short::Quota) => break,
                            }
                        }
                        assert_eq!(ledger.reservation(call_id(n)), reservation);
                        let bytes = reservation - (t * 4096 + i);
                        retained.lock().unwrap().insert(call_id(n), bytes);
                        let last_use = sequence.fetch_add(1, Ordering::Relaxed);
                        ledger.settle(
                            call_id(n),
                            CaptureState::Complete,
                            streams(bytes / 2, bytes - bytes / 2),
                            last_use,
                            false,
                        );
                    }
                })
            })
            .collect();
        for worker in workers {
            worker.join().unwrap();
        }
        done.store(true, Ordering::Release);
        monitor.join().unwrap();
        let retained = retained.lock().unwrap();
        assert_eq!(ledger.used(), retained.values().sum::<u64>());
        assert!(ledger.used() <= ledger.quota());
        let state = lock(&ledger);
        assert_eq!(state.calls.len(), retained.len());
        assert!(retained.keys().all(|call| state.calls.contains_key(call)));
        assert_eq!(state.payloads.len(), 2 * retained.len());
        assert_eq!(state.expired.len(), 2 * (8 * 60 - retained.len()));
    }

    #[test]
    fn j1t_08_victims_are_oldest_then_larger_then_payload_id_with_both_streams() {
        let ledger = Ledger::default();
        ledger.set_effective_quota(DEFAULT_QUOTA, u64::MAX / 4);
        for n in 1..=5 {
            admit(&ledger, n).unwrap();
        }
        settle(&ledger, 1, 2 * MIB, 10);
        settle(&ledger, 2, MIB, 5);
        settle(&ledger, 3, 2 * MIB, 5);
        settle(&ledger, 4, MIB, 7);
        settle(&ledger, 5, MIB, 7);
        let (low, high) = if stream_id(4, ObjectKind::Stdout) < stream_id(5, ObjectKind::Stdout) {
            (4, 5)
        } else {
            (5, 4)
        };
        tighten(&ledger, 0);
        let mut order = Vec::new();
        for n in 100.. {
            match admit(&ledger, n) {
                Ok(victims) => {
                    for victim in &victims {
                        let index = (1..=5).find(|k| call_id(*k) == victim.call).unwrap();
                        // Both streams of the call are chosen together.
                        assert_eq!(
                            victim.payloads,
                            [
                                stream_id(index, ObjectKind::Stdout),
                                stream_id(index, ObjectKind::Stderr)
                            ]
                        );
                        order.push(index);
                    }
                    delete_all(&ledger, &victims);
                    assert!(ledger.used() <= ledger.quota());
                }
                Err(Short::Quota) => break,
            }
        }
        assert_eq!(order, [3, 2, low, high, 1]);
        for n in 1..=5 {
            for kind in [ObjectKind::Stdout, ObjectKind::Stderr] {
                assert_eq!(ledger.payload_state(stream_id(n, kind)), None);
                assert!(ledger.was_expired(stream_id(n, kind)));
            }
        }
    }

    #[test]
    fn j1t_08_held_live_and_unconfirmed_calls_are_skipped() {
        let ledger = Ledger::default();
        ledger.set_effective_quota(DEFAULT_QUOTA, u64::MAX / 4);
        for n in 1..=5 {
            admit(&ledger, n).unwrap();
        }
        // 1: held by a page read; 2: pending install (held settlement);
        // 3: unconfirmed; 4: still live; 5: expirable.
        settle(&ledger, 1, MIB, 1);
        let read = stream_id(1, ObjectKind::Stderr);
        assert_eq!(ledger.hold_payload(read).unwrap().call, call_id(1));
        ledger.settle(call_id(2), CaptureState::Complete, streams(MIB, 0), 2, true);
        ledger.settle(
            call_id(3),
            CaptureState::Unconfirmed,
            [(MIB, None), (0, None)],
            3,
            false,
        );
        settle(&ledger, 5, MIB, 9);
        tighten(&ledger, 0);
        let victims = admit(&ledger, 10).unwrap();
        assert_eq!(victim_calls(&victims), [call_id(5)]);
        delete_all(&ledger, &victims);
        assert_eq!(admit(&ledger, 11), Err(Short::Quota));
        // The read ends: call 1 becomes expirable.
        ledger.release_payload(read);
        let victims = admit(&ledger, 11).unwrap();
        assert_eq!(victim_calls(&victims), [call_id(1)]);
        delete_all(&ledger, &victims);
        // The pending install ends.
        assert_eq!(admit(&ledger, 12), Err(Short::Quota));
        ledger.unhold(call_id(2));
        let victims = admit(&ledger, 12).unwrap();
        assert_eq!(victim_calls(&victims), [call_id(2)]);
        // A page read cannot hold a call already chosen for deletion.
        assert!(
            ledger
                .hold_payload(stream_id(2, ObjectKind::Stdout))
                .is_none()
        );
        delete_all(&ledger, &victims);
        // The unconfirmed prefix waits for its operation to end.
        assert_eq!(admit(&ledger, 13), Err(Short::Quota));
        ledger.operation_ended(operation(3));
        let victims = admit(&ledger, 13).unwrap();
        assert_eq!(victim_calls(&victims), [call_id(3)]);
        delete_all(&ledger, &victims);
        // The live call 4 is never chosen, even after its operation ends.
        ledger.operation_ended(operation(4));
        assert_eq!(admit(&ledger, 14), Err(Short::Quota));
        assert!(ledger.used() <= ledger.quota());
    }

    #[test]
    fn j1t_08_restore_release_and_failed_deletion_return_space() {
        let ledger = Ledger::default();
        ledger.set_effective_quota(DEFAULT_QUOTA, u64::MAX / 4);
        admit(&ledger, 1).unwrap();
        settle(&ledger, 1, 3 * MIB, 1);
        tighten(&ledger, 0);
        let before = ledger.used();
        assert_eq!(before, 3 * MIB);
        // An admission that never started: release it and restore its victims.
        let victims = admit(&ledger, 2).unwrap();
        assert_eq!(victim_calls(&victims), [call_id(1)]);
        assert_eq!(ledger.used(), MIB);
        ledger.release(call_id(2));
        assert_eq!(ledger.used(), 0);
        assert_eq!(ledger.reservation(call_id(2)), 0);
        assert_eq!(ledger.payload_state(stream_id(2, ObjectKind::Stdout)), None);
        ledger.restore_victims(&victims);
        assert_eq!(ledger.used(), before);
        ledger.restore_victims(&victims);
        assert_eq!(ledger.used(), before, "restoring twice counts once");
        // The restored call is chosen again; a failed deletion counts its
        // bytes again and stops choosing it.
        let victims = admit(&ledger, 3).unwrap();
        assert_eq!(victim_calls(&victims), [call_id(1)]);
        ledger.victim_deleted(victims[0], false);
        assert_eq!(ledger.used(), before + MIB);
        let stdout1 = stream_id(1, ObjectKind::Stdout);
        assert_eq!(
            ledger.payload_state(stdout1),
            Some((PayloadState::Complete, Location::None, 3 * MIB))
        );
        assert!(!ledger.was_expired(stdout1));
        ledger.release(call_id(3));
        assert_eq!(ledger.used(), before);
        tighten(&ledger, 0);
        assert_eq!(admit(&ledger, 4), Err(Short::Quota));
        // Releasing a settled call returns its retained bytes too.
        ledger.release(call_id(1));
        assert_eq!(ledger.used(), 0);
        assert_eq!(ledger.payload_state(stdout1), None);
    }

    #[test]
    fn j1t_08_at_most_128_victims_per_attempt() {
        for (size, expected) in [(8 * 1024, true), (4 * 1024, false)] {
            let ledger = Ledger::default();
            ledger.set_effective_quota(DEFAULT_QUOTA, u64::MAX / 4);
            for n in 0..200 {
                admit(&ledger, n).unwrap();
                settle(&ledger, n, size, u64::from(n));
            }
            tighten(&ledger, 0);
            let used = ledger.used();
            match admit(&ledger, 1000) {
                Ok(victims) => {
                    assert!(expected);
                    assert_eq!(victims.len(), MAX_EXPIRY_PER_ATTEMPT);
                    // Oldest first: calls 0 to 127.
                    let oldest: Vec<Id> = (0..128).map(call_id).collect();
                    assert_eq!(victim_calls(&victims), oldest);
                    assert_eq!(ledger.used(), used - 128 * size + MIB);
                }
                Err(Short::Quota) => {
                    assert!(!expected);
                    // A failed attempt marks nothing and frees nothing.
                    assert_eq!(ledger.used(), used);
                    assert!(lock(&ledger).calls.values().all(|c| !c.deleting));
                }
            }
        }
    }

    #[test]
    fn j1t_08_settlement_releases_the_reservation_and_counts_stored_bytes() {
        let ledger = Ledger::default();
        ledger.set_effective_quota(DEFAULT_QUOTA, u64::MAX / 4);
        admit(&ledger, 1).unwrap();
        ledger.grow(call_id(1)).unwrap();
        assert_eq!(ledger.reservation(call_id(1)), 2 * MIB);
        assert_eq!(ledger.used(), 2 * MIB);
        ledger.settle(
            call_id(1),
            CaptureState::Incomplete,
            streams(100, 23),
            4,
            false,
        );
        assert_eq!((ledger.reservation(call_id(1)), ledger.used()), (0, 123));
        let stdout = stream_id(1, ObjectKind::Stdout);
        assert_eq!(
            ledger.payload_state(stdout),
            Some((PayloadState::Incomplete, Location::None, 100))
        );
        assert_eq!(
            ledger.payload_state(stream_id(1, ObjectKind::Stderr)),
            Some((PayloadState::Incomplete, Location::None, 23))
        );
        assert!(ledger.watermark(stdout).is_none());
    }

    #[test]
    fn unconfirmed_settlement_keeps_the_watermark_until_abandoned() {
        let ledger = Ledger::default();
        ledger.set_effective_quota(DEFAULT_QUOTA, u64::MAX / 4);
        admit(&ledger, 1).unwrap();
        let stdout = stream_id(1, ObjectKind::Stdout);
        let watermark = ledger.watermark(stdout).unwrap();
        watermark.store(10, Ordering::Release);
        ledger.settle(
            call_id(1),
            CaptureState::Unconfirmed,
            [(10, None), (0, None)],
            2,
            false,
        );
        // The abandoned worker keeps writing; pages follow the same counter.
        assert!(Arc::ptr_eq(&ledger.watermark(stdout).unwrap(), &watermark));
        assert!(ledger.watermark(stream_id(1, ObjectKind::Stderr)).is_some());
        assert_eq!(
            ledger.payload_state(stdout),
            Some((PayloadState::UnverifiedPrefix, Location::None, 10))
        );
        watermark.store(25, Ordering::Release);
        ledger.abandoned(call_id(1), 25);
        assert!(ledger.watermark(stdout).is_none());
        assert_eq!(
            ledger.payload_state(stdout),
            Some((PayloadState::UnverifiedPrefix, Location::None, 25))
        );
        assert_eq!(ledger.used(), 25);
    }
}
