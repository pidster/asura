//! The secret store worker thread: one request at a time, the reply mapping
//! of `docs/designs/secret-store.md` (status mapping, worker reply table),
//! the item count and the sweep (SS-F1).
use super::codec::{self, DIGEST_BYTES};
use super::port::KeychainPort;
use super::{
    COUNT_TRUNCATED, DeleteReply, GetReply, ITEM_LIMIT, OBSERVED_STRANDED, Observation, PutReply,
    Request, Shared, StoreFailure, SweepReport,
};
use asura_platform::keychain::{Added, Failure, Found, ItemKey, LIST_BOUND, Listing, Removed};
use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::sync::mpsc::{Receiver, SyncSender};
use zeroize::Zeroizing;

/// `errSecItemNotFound`: reported when a duplicate vanished before its read.
const ITEM_NOT_FOUND: i32 = -25_300;

pub(super) struct Worker<P, O> {
    installation: [u8; 16],
    open: O,
    port: Option<P>,
    shared: Arc<Shared>,
    /// The item count: every listed item plus stores minus deletes since.
    /// `None` until a listing; [`COUNT_TRUNCATED`] while the last listing
    /// was truncated, which no store or delete changes.
    count: Option<u32>,
    truncated: bool,
    /// A sweep completed, so the observation carries the count.
    swept: bool,
    /// The live set of a sweep whose listing failed; retried before the
    /// next request.
    pending: Option<BTreeSet<[u8; 16]>>,
    /// Inputs this worker stored and has not deleted. A sweep keeps them:
    /// each is referenced, or its kind 31 outcome is not yet known (ordering
    /// rule 3). Bounded by the item limit.
    written: BTreeSet<[u8; 16]>,
    /// Inputs whose delete was denied, at most [`LIST_BOUND`].
    stranded: BTreeSet<[u8; 16]>,
    unrecognized: u32,
    last_status: u32,
}

impl<P, O> Worker<P, O>
where
    P: KeychainPort,
    O: FnMut() -> Result<P, Failure> + Send + 'static,
{
    pub(super) fn new(installation: [u8; 16], open: O, shared: Arc<Shared>) -> Self {
        Self {
            installation,
            open,
            port: None,
            shared,
            count: None,
            truncated: false,
            swept: false,
            pending: None,
            written: BTreeSet::new(),
            stranded: BTreeSet::new(),
            unrecognized: 0,
            last_status: 0,
        }
    }

    /// Serves requests until admission closes and the queue is empty.
    pub(super) fn run(mut self, requests: Receiver<Request>) {
        while let Ok(request) = requests.recv() {
            if let Some(live) = self.pending.take() {
                let _ = self.sweep(live);
            }
            match request {
                Request::Put {
                    input,
                    binding,
                    secrets,
                    reply,
                } => {
                    let answer = self.put(input, &binding, &secrets);
                    drop(secrets);
                    self.answer(reply, answer);
                }
                Request::Get {
                    input,
                    binding,
                    reply,
                } => {
                    let answer = self.get(input, &binding);
                    self.answer(reply, answer);
                }
                Request::Delete { input, reply } => {
                    let answer = self.delete(input);
                    self.answer(reply, answer);
                }
                Request::Sweep { live, reply } => {
                    let answer = self.sweep(live);
                    self.answer(reply, answer);
                }
            }
        }
    }

    /// Frees the admission slot before the reply is visible, publishes the
    /// observation, then wakes the caller.
    fn answer<T>(&self, reply: SyncSender<T>, answer: T) {
        self.publish();
        self.shared.outstanding.fetch_sub(1, Ordering::AcqRel);
        let _ = reply.try_send(answer);
        self.shared.notify();
    }

    fn publish(&self) {
        let observation = Observation {
            item_count: self.swept.then_some(self.count).flatten(),
            sweep_pending: self.pending.is_some(),
            last_status: self.last_status,
            stranded_count: self.stranded.len() as u32,
            unrecognized_count: self.unrecognized,
            stranded_inputs: self
                .stranded
                .iter()
                .take(OBSERVED_STRANDED)
                .copied()
                .collect(),
        };
        *self
            .shared
            .observation
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = observation;
    }

    /// One port call, opening the target first while it is not open. Every
    /// result goes through the reply table here and nowhere else.
    fn call<T>(&mut self, call: impl FnOnce(&P) -> Result<T, Failure>) -> Result<T, StoreFailure> {
        if self.port.is_none() {
            self.port = Some((self.open)().map_err(|failure| self.failure(failure))?);
        }
        let port = self.port.as_ref().expect("port opened above");
        match call(port) {
            Ok(value) => {
                self.last_status = 0;
                Ok(value)
            }
            Err(failure) => Err(self.failure(failure)),
        }
    }

    /// The worker reply table for a platform `Failure`.
    fn failure(&mut self, failure: Failure) -> StoreFailure {
        match failure {
            Failure::Locked => StoreFailure::Locked,
            Failure::Denied => StoreFailure::Denied,
            Failure::Unavailable { status, .. } => {
                self.last_status = status.unsigned_abs();
                StoreFailure::Unavailable {
                    status,
                    reason: None,
                }
            }
            // The codec and the item key rules make this unreachable.
            Failure::Invalid => StoreFailure::Unavailable {
                status: 0,
                reason: Some("invalid_request"),
            },
        }
    }

    fn key(&self, input: [u8; 16]) -> ItemKey {
        ItemKey {
            installation: self.installation,
            input,
        }
    }

    /// The item count, listing once when no listing has set it yet.
    fn known_count(&mut self) -> Result<u32, StoreFailure> {
        if let Some(count) = self.count {
            return Ok(count);
        }
        let installation = self.installation;
        let listing = self.call(|port| port.list(&installation))?;
        self.unrecognized = listing.unrecognized as u32;
        let count = self.set_count(&listing, 0);
        Ok(count)
    }

    fn set_count(&mut self, listing: &Listing, removed: u32) -> u32 {
        self.truncated = listing.truncated;
        let count = if listing.truncated {
            COUNT_TRUNCATED
        } else {
            ((listing.inputs.len() + listing.unrecognized) as u32).saturating_sub(removed)
        };
        self.count = Some(count);
        count
    }

    fn adjust_count(&mut self, stored: bool) {
        if self.truncated {
            return;
        }
        if let Some(count) = &mut self.count {
            *count = if stored {
                count.saturating_add(1).min(COUNT_TRUNCATED)
            } else {
                count.saturating_sub(1)
            };
        }
    }

    fn put(
        &mut self,
        input: [u8; 16],
        binding: &[u8; DIGEST_BYTES],
        secrets: &codec::Secrets,
    ) -> PutReply {
        let mut key = Zeroizing::new([0u8; DIGEST_BYTES]);
        key[..16].copy_from_slice(&asura_platform::random_id());
        key[16..].copy_from_slice(&asura_platform::random_id());
        let Ok(payload) = codec::encode(binding, secrets, &key) else {
            return PutReply::Failed(self.failure(Failure::Invalid));
        };
        match self.known_count() {
            Ok(count) if count >= ITEM_LIMIT => return PutReply::Full,
            Ok(_) => {}
            Err(failure) => return PutReply::Failed(failure),
        }
        let item = self.key(input);
        match self.call(|port| port.add(&item, &payload)) {
            Ok(Added::Stored) => {
                self.adjust_count(true);
                self.written.insert(input);
                PutReply::Stored
            }
            Ok(Added::Duplicate) => {
                let reply = self.compare(&item, binding, secrets);
                if reply == PutReply::ExistsSame {
                    self.written.insert(input);
                }
                reply
            }
            Err(failure) => PutReply::Failed(failure),
        }
    }

    /// An exact retry: read the existing item and compare keyed digests.
    fn compare(
        &mut self,
        item: &ItemKey,
        binding: &[u8; DIGEST_BYTES],
        secrets: &codec::Secrets,
    ) -> PutReply {
        match self.call(|port| port.get(item)) {
            Ok(Found::Data(data)) => match codec::decode(&data, binding) {
                Ok(existing) => {
                    let stored = codec::value_digest(&existing.key, &existing.secrets);
                    let retried = codec::value_digest(&existing.key, secrets);
                    if codec::digests_equal(&stored, &retried) {
                        PutReply::ExistsSame
                    } else {
                        PutReply::ExistsDifferent
                    }
                }
                Err(_) => PutReply::ExistsDifferent,
            },
            // The duplicate vanished between the add and the read.
            Ok(Found::Missing) => PutReply::Failed(self.failure(Failure::Unavailable {
                status: ITEM_NOT_FOUND,
                item_call: true,
            })),
            Err(failure) => PutReply::Failed(failure),
        }
    }

    fn get(&mut self, input: [u8; 16], binding: &[u8; DIGEST_BYTES]) -> GetReply {
        let item = self.key(input);
        match self.call(|port| port.get(&item)) {
            Ok(Found::Data(data)) => match codec::decode(&data, binding) {
                Ok(decoded) => GetReply::Values(decoded.secrets),
                Err(_) => GetReply::Mismatch,
            },
            Ok(Found::Missing) => GetReply::Missing,
            Err(failure) => GetReply::Failed(failure),
        }
    }

    fn delete(&mut self, input: [u8; 16]) -> DeleteReply {
        let item = self.key(input);
        let reply = match self.call(|port| port.delete(&item)) {
            Ok(Removed::Deleted) => {
                self.adjust_count(false);
                DeleteReply::Deleted
            }
            Ok(Removed::Absent) => DeleteReply::Absent,
            Err(failure) => DeleteReply::Failed(failure),
        };
        self.settle_delete(input, reply);
        reply
    }

    /// A delete's effect on the stranded set and the written set.
    fn settle_delete(&mut self, input: [u8; 16], reply: DeleteReply) {
        match reply {
            DeleteReply::Deleted | DeleteReply::Absent => {
                self.stranded.remove(&input);
                self.written.remove(&input);
            }
            DeleteReply::Failed(StoreFailure::Denied) => {
                self.written.remove(&input);
                if self.stranded.len() < LIST_BOUND {
                    self.stranded.insert(input);
                }
            }
            DeleteReply::Failed(_) => {}
        }
    }

    /// SS-F1. Lists again only while the listing was truncated and the pass
    /// deleted an item, so each repeat shrinks a finite set.
    fn sweep(&mut self, live: BTreeSet<[u8; 16]>) -> SweepReport {
        let installation = self.installation;
        let mut report = SweepReport::default();
        loop {
            let listing = match self.call(|port| port.list(&installation)) {
                Ok(listing) => listing,
                Err(failure) => {
                    report.outcome = Err(failure);
                    self.pending = Some(live);
                    return report;
                }
            };
            report.listings += 1;
            let mut pass = SweepReport::default();
            let mut removed = 0u32;
            let mut stranded = BTreeSet::new();
            for &input in &listing.inputs {
                if live.contains(&input) || self.written.contains(&input) {
                    pass.kept += 1;
                    continue;
                }
                let item = self.key(input);
                let reply = match self.call(|port| port.delete(&item)) {
                    Ok(Removed::Deleted) => {
                        pass.deleted += 1;
                        removed += 1;
                        DeleteReply::Deleted
                    }
                    Ok(Removed::Absent) => {
                        removed += 1;
                        DeleteReply::Absent
                    }
                    Err(StoreFailure::Denied) => {
                        stranded.insert(input);
                        DeleteReply::Failed(StoreFailure::Denied)
                    }
                    Err(failure) => {
                        pass.failed += 1;
                        DeleteReply::Failed(failure)
                    }
                };
                self.settle_delete(input, reply);
            }
            report.deleted += pass.deleted;
            if listing.truncated && pass.deleted > 0 {
                continue;
            }
            self.unrecognized = listing.unrecognized as u32;
            self.stranded = stranded;
            self.set_count(&listing, removed);
            self.swept = true;
            self.pending = None;
            report.kept = pass.kept;
            report.failed = pass.failed;
            report.stranded = self.stranded.len() as u32;
            report.unrecognized = self.unrecognized;
            report.truncated = listing.truncated;
            report.outcome = Ok(());
            return report;
        }
    }
}
