//! The durable secret store worker (`docs/designs/secret-store.md`, packet
//! SS1-S).
//!
//! One retained thread, the secret store worker, makes every Keychain call
//! through [`port::KeychainPort`]. Callers submit requests without blocking:
//! at most [`QUEUE_BOUND`] requests are outstanding (queued or running), and
//! the next one is refused with [`Refused::Busy`] before it reaches the
//! worker. Each accepted request returns a [`Ticket`] the caller polls; the
//! worker notifies the caller's wake after every reply. No wait in this
//! module has a time bound (AE-04): a request ends by its reply, and a
//! stalled securityd shows as `busy` while every other path continues.
//!
//! The worker owns the item payload codec and its 55,413-byte read bound,
//! the platform-result to reply mapping, the item count, the startup sweep
//! and the stranded and unrecognized counts. It publishes the facts for
//! `ServiceObservation` field 8 as an [`Observation`]. Conversation wiring,
//! audit emission and the observer field are the integrator's (P3-E).
//!
//! Shutdown does not wait for the worker: dropping the [`Store`] closes
//! admission, and the worker exits after the requests already queued. Tests
//! join it with [`Store::close`] before deleting their scratch keychain
//! (testing rule 2).
mod codec;
mod port;
mod worker;

pub use codec::{
    CodecError, DIGEST_BYTES, EnvironmentEntry, MAX_ENVIRONMENT_BYTES, MAX_ENVIRONMENT_ENTRIES,
    MAX_ENVIRONMENT_NAME_BYTES, MAX_VARIABLE_BYTES, MAX_VARIABLES, MAX_WHOLE_COMMAND_BYTES,
    SecretValue, Secrets, VERSION, binding,
};
#[cfg(feature = "test-support")]
pub use port::{Call, Gate};

use asura_platform::events::WakeSender;
use asura_platform::keychain::{Keychain, Target};
use std::collections::BTreeSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

/// Requests outstanding at once, queued or running (AE-03).
pub const QUEUE_BOUND: usize = 16;
/// Secret items per installation. A put is refused with `full` at this count.
pub const ITEM_LIMIT: u32 = 256;
/// The item count reported for a truncated listing: "more than 256".
pub const COUNT_TRUNCATED: u32 = asura_platform::keychain::LIST_BOUND as u32;
/// Stranded input IDs the observation carries; the count keeps the total.
pub const OBSERVED_STRANDED: usize = 16;
/// Longest gap of the locked or unavailable recheck at dispatch.
pub const RECHECK_MAX: Duration = Duration::from_secs(60);

/// Why a Keychain request did not succeed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StoreFailure {
    /// The keychain is locked: `secret_store_locked` or `keychain_locked`.
    Locked,
    /// macOS denied this binary access to the item: `secret_store_denied`.
    Denied,
    /// The target or the Security framework failed. `status` goes to audit
    /// and `last_status`; `reason` is `invalid_request` for a request that
    /// broke a codec or item-key rule (status 0), which is a defect.
    Unavailable {
        status: i32,
        reason: Option<&'static str>,
    },
}

impl StoreFailure {
    /// The audit reply code.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Locked => "locked",
            Self::Denied => "denied",
            Self::Unavailable { .. } => "unavailable",
        }
    }
}

/// Reply to [`Store::put`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PutReply {
    Stored,
    /// An item exists for this input with equal values (exact retry).
    ExistsSame,
    /// An item exists for this input with other values or another binding.
    ExistsDifferent,
    /// The item count is 256 or more.
    Full,
    Failed(StoreFailure),
}

/// Reply to [`Store::get`].
#[derive(Debug, PartialEq, Eq)]
pub enum GetReply {
    Values(Secrets),
    Missing,
    /// The data is oversize, malformed or bound to another input; the owner
    /// deletes the item.
    Mismatch,
    Failed(StoreFailure),
}

/// Reply to [`Store::delete`]. `Failed(Denied)` leaves a stranded item.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeleteReply {
    Deleted,
    Absent,
    Failed(StoreFailure),
}

/// Reply to [`Store::sweep`]: the counts of the sweep's last listing pass,
/// with `deleted` summed over every pass.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SweepReport {
    pub kept: u32,
    pub deleted: u32,
    /// Deletes that failed locked or unavailable; retried by the next sweep.
    pub failed: u32,
    /// Deletes that were denied: stranded items.
    pub stranded: u32,
    pub unrecognized: u32,
    /// The last listing returned 257 results.
    pub truncated: bool,
    /// Listings made, each at most 257 results.
    pub listings: u32,
    /// `Err` when a listing failed: the sweep is pending and the worker
    /// retries it before its next request.
    pub outcome: Result<(), StoreFailure>,
}

impl Default for SweepReport {
    fn default() -> Self {
        Self {
            kept: 0,
            deleted: 0,
            failed: 0,
            stranded: 0,
            unrecognized: 0,
            truncated: false,
            listings: 0,
            outcome: Ok(()),
        }
    }
}

/// The facts for `ServiceObservation` field 8 (`SecretStoreStatus`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Observation {
    /// Every listed item, 0 to 257 (257 means more than 256). Absent until
    /// the first sweep completes.
    pub item_count: Option<u32>,
    /// The startup sweep was deferred and awaits a retry.
    pub sweep_pending: bool,
    /// 0, or the last `unavailable` status as its absolute value; cleared by
    /// the next port call that succeeds.
    pub last_status: u32,
    /// Items whose delete was denied.
    pub stranded_count: u32,
    /// Items whose account is not `input.<32 hex>`.
    pub unrecognized_count: u32,
    /// At most [`OBSERVED_STRANDED`] stranded input IDs, in ascending order.
    pub stranded_inputs: Vec<[u8; 16]>,
}

/// A request was not admitted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Refused {
    /// [`QUEUE_BOUND`] requests are outstanding: `secret_store_busy` at
    /// submit, `keychain_busy` at dispatch; a settlement delete is left for
    /// the next sweep.
    Busy,
    /// The worker has exited; treat as `unavailable`.
    Stopped,
}

/// A ticket's state when polled.
#[derive(Debug, PartialEq, Eq)]
pub enum Polled<T> {
    Pending,
    Ready(T),
    /// The worker exited without replying.
    Lost,
}

/// One accepted request's reply slot. Polling never blocks.
pub struct Ticket<T>(Receiver<T>);

impl<T> Ticket<T> {
    /// Takes the reply if the worker has sent it.
    pub fn poll(&self) -> Polled<T> {
        match self.0.try_recv() {
            Ok(reply) => Polled::Ready(reply),
            Err(TryRecvError::Empty) => Polled::Pending,
            Err(TryRecvError::Disconnected) => Polled::Lost,
        }
    }
    /// Waits for the reply. Tests only: it blocks without a time bound and
    /// ends by the reply or the worker's exit.
    #[cfg(any(test, feature = "test-support"))]
    pub fn wait(self) -> Option<T> {
        self.0.recv().ok()
    }
}

pub(crate) enum Request {
    Put {
        input: [u8; 16],
        binding: [u8; DIGEST_BYTES],
        secrets: Secrets,
        reply: SyncSender<PutReply>,
    },
    Get {
        input: [u8; 16],
        binding: [u8; DIGEST_BYTES],
        reply: SyncSender<GetReply>,
    },
    Delete {
        input: [u8; 16],
        reply: SyncSender<DeleteReply>,
    },
    Sweep {
        live: BTreeSet<[u8; 16]>,
        reply: SyncSender<SweepReport>,
    },
}

/// State the worker shares with callers: the admission count and the
/// observation. Neither lock is held across a Keychain call.
pub(crate) struct Shared {
    outstanding: AtomicUsize,
    observation: Mutex<Observation>,
    wake: Option<WakeSender>,
}

impl Shared {
    fn notify(&self) {
        if let Some(wake) = &self.wake {
            let _ = wake.notify();
        }
    }
}

/// The secret store of one installation: the request queue and the handle of
/// its worker thread.
pub struct Store {
    sender: SyncSender<Request>,
    shared: Arc<Shared>,
    thread: JoinHandle<()>,
}

impl Store {
    /// Starts the worker on `target`. The worker opens the keychain on its
    /// own thread (which disables Keychain user interaction for the process
    /// first) and retries the open before each request while it fails.
    /// Production passes `Target::Default`.
    pub fn new(
        target: Target,
        installation: [u8; 16],
        wake: Option<WakeSender>,
    ) -> std::io::Result<Self> {
        Self::with_port(installation, wake, move || open_target(&target))
    }

    /// Starts the worker over the gated port (SS-W4, SS-W5, SS-W8).
    #[cfg(feature = "test-support")]
    pub fn gated(
        target: Target,
        installation: [u8; 16],
        wake: Option<WakeSender>,
        gate: Gate,
    ) -> std::io::Result<Self> {
        Self::with_port(installation, wake, move || {
            Ok(port::GatedPort {
                keychain: open_target(&target)?,
                gate: gate.clone(),
            })
        })
    }

    pub(crate) fn with_port<P, O>(
        installation: [u8; 16],
        wake: Option<WakeSender>,
        open: O,
    ) -> std::io::Result<Self>
    where
        P: port::KeychainPort,
        O: FnMut() -> Result<P, asura_platform::keychain::Failure> + Send + 'static,
    {
        let shared = Arc::new(Shared {
            outstanding: AtomicUsize::new(0),
            observation: Mutex::new(Observation::default()),
            wake,
        });
        // Admission bounds the queue first, so the channel is never full.
        let (sender, receiver) = mpsc::sync_channel(QUEUE_BOUND);
        let worker = worker::Worker::new(installation, open, shared.clone());
        let thread = std::thread::Builder::new()
            .name("asura-secret-store".into())
            .spawn(move || worker.run(receiver))?;
        Ok(Self {
            sender,
            shared,
            thread,
        })
    }

    fn submit<T>(
        &self,
        request: impl FnOnce(SyncSender<T>) -> Request,
    ) -> Result<Ticket<T>, Refused> {
        let admitted = self.shared.outstanding.fetch_update(
            Ordering::AcqRel,
            Ordering::Acquire,
            |outstanding| (outstanding < QUEUE_BOUND).then_some(outstanding + 1),
        );
        if admitted.is_err() {
            return Err(Refused::Busy);
        }
        let (reply, receiver) = mpsc::sync_channel(1);
        match self.sender.try_send(request(reply)) {
            Ok(()) => Ok(Ticket(receiver)),
            Err(error) => {
                self.shared.outstanding.fetch_sub(1, Ordering::AcqRel);
                Err(match error {
                    TrySendError::Full(_) => Refused::Busy,
                    TrySendError::Disconnected(_) => Refused::Stopped,
                })
            }
        }
    }

    /// Stores one input's values, bound by `binding`. Send before kind 31
    /// and wait for `stored` (ordering rule 1).
    pub fn put(
        &self,
        input: [u8; 16],
        binding: [u8; DIGEST_BYTES],
        secrets: Secrets,
    ) -> Result<Ticket<PutReply>, Refused> {
        self.submit(|reply| Request::Put {
            input,
            binding,
            secrets,
            reply,
        })
    }

    /// Reads one input's values at dispatch and checks the binding.
    pub fn get(
        &self,
        input: [u8; 16],
        binding: [u8; DIGEST_BYTES],
    ) -> Result<Ticket<GetReply>, Refused> {
        self.submit(|reply| Request::Get {
            input,
            binding,
            reply,
        })
    }

    /// Deletes one input's item after its deciding record is durable.
    pub fn delete(&self, input: [u8; 16]) -> Result<Ticket<DeleteReply>, Refused> {
        self.submit(|reply| Request::Delete { input, reply })
    }

    /// The startup sweep after replay: deletes every input item not in
    /// `live` and not written by this worker since it started. Items of
    /// unrecognized accounts are counted and never touched.
    pub fn sweep(&self, live: BTreeSet<[u8; 16]>) -> Result<Ticket<SweepReport>, Refused> {
        self.submit(|reply| Request::Sweep { live, reply })
    }

    /// True while a request would be admitted.
    pub fn has_room(&self) -> bool {
        self.shared.outstanding.load(Ordering::Acquire) < QUEUE_BOUND
    }

    /// The current facts for `ServiceObservation` field 8.
    pub fn observation(&self) -> Observation {
        self.shared
            .observation
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Closes admission and returns the worker's handle. The worker exits
    /// after the requests already queued. Production drops the handle
    /// (shutdown does not wait); tests join it before deleting the keychain.
    pub fn close(self) -> JoinHandle<()> {
        drop(self.sender);
        self.thread
    }
}

/// Opens the target. Unit tests of this crate never open the default
/// keychain (SSR6, SS-U7).
fn open_target(target: &Target) -> Result<Keychain, asura_platform::keychain::Failure> {
    if cfg!(test) && matches!(target, Target::Default) {
        return Err(asura_platform::keychain::Failure::Invalid);
    }
    Keychain::open(target)
}

/// The paced recheck of a command waiting at dispatch with
/// `keychain_locked` or `keychain_unavailable`: 1, 1, 2, 3, 5, 8 s and so
/// on, at most [`RECHECK_MAX`] apart. Pacing only; it ends nothing. The
/// owner also rechecks at once on the person's next action in that
/// conversation, and resets the pacing when the command leaves the wait.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Recheck {
    previous: Duration,
    current: Duration,
}

impl Default for Recheck {
    fn default() -> Self {
        Self {
            previous: Duration::ZERO,
            current: Duration::from_secs(1),
        }
    }
}

impl Recheck {
    /// The delay before the next recheck; advances the pacing.
    pub fn next_delay(&mut self) -> Duration {
        let delay = self.current;
        self.current = (self.previous + self.current).min(RECHECK_MAX);
        self.previous = delay;
        delay
    }
}

#[cfg(test)]
mod tests;
