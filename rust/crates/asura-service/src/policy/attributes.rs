//! The attribute reader (security-policy.md, PL2 attribute reader): one
//! platform thread owned by the policy owner for the blocking attribute
//! reads, path canonicalisation of project roots and the static code check
//! of the entry program. Every call here on the reactor is a bounded
//! channel operation; the file system and Security framework calls run on
//! the thread.
use asura_platform::PathRefusal;
use asura_platform::code_identity::{CodeIdentity, IdentityCache};
use asura_platform::events::WakeSender;
use asura_storage::authority::conversation as journal;
use std::sync::mpsc::{self, Receiver, SyncSender, TrySendError};
use std::thread::JoinHandle;

/// Lookups queued for the thread (AE-03).
pub(crate) const QUEUE: usize = 64;

/// A registered project root: its ID, location and retained identity.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) struct RootKey {
    pub project: journal::Id,
    pub location: String,
    pub device: u64,
    pub inode: u64,
}

pub(super) enum Lookup {
    /// The entry program's identity; read again only on a changed
    /// fingerprint.
    Entry,
    Root(RootKey),
}
pub(super) enum Found {
    /// The entry identity.
    Entry(CodeIdentity),
    Root(RootKey, Result<String, PathRefusal>),
}

pub(super) struct Reader {
    sender: Option<SyncSender<Lookup>>,
    handle: Option<JoinHandle<()>>,
    replies: Receiver<Found>,
}
impl Reader {
    pub(super) fn start(wake: Option<WakeSender>) -> Self {
        let (sender, receiver) = mpsc::sync_channel::<Lookup>(QUEUE);
        let (reply, replies) = mpsc::channel();
        let handle = std::thread::Builder::new()
            .name("asura-policy-attributes".into())
            .spawn(move || reader_main(receiver, reply, wake))
            .ok();
        Self {
            sender: handle.as_ref().map(|_| sender),
            handle,
            replies,
        }
    }
    /// Queue one lookup; `false` when the queue is full or the thread is
    /// gone (the owner tries again on a later step).
    pub(super) fn send(&self, lookup: Lookup) -> bool {
        match self.sender.as_ref().map(|s| s.try_send(lookup)) {
            Some(Ok(())) => true,
            Some(Err(TrySendError::Full(_) | TrySendError::Disconnected(_))) | None => false,
        }
    }
    pub(super) fn try_recv(&self) -> Option<Found> {
        self.replies.try_recv().ok()
    }
    /// The thread ended (a panic): it is joined; the caller replaces it.
    pub(super) fn lost(&mut self) -> bool {
        if self.handle.as_ref().is_some_and(JoinHandle::is_finished) {
            if let Some(handle) = self.handle.take() {
                let _ = handle.join();
            }
            return self.sender.is_some();
        }
        false
    }
    /// Shutdown: close the queue; the thread ends after its current read.
    pub(super) fn close(&mut self) {
        self.sender = None;
    }
    /// The thread has finished and been joined.
    pub(super) fn settled(&mut self) -> bool {
        if self.handle.as_ref().is_some_and(JoinHandle::is_finished)
            && let Some(handle) = self.handle.take()
        {
            let _ = handle.join();
        }
        self.handle.is_none()
    }
}

fn reader_main(receiver: Receiver<Lookup>, reply: mpsc::Sender<Found>, wake: Option<WakeSender>) {
    let mut cache = IdentityCache::new();
    // The loop ends when the owner drops the sender (shutdown or
    // replacement); every lookup is one bounded walk or check.
    while let Ok(lookup) = receiver.recv() {
        let (found, notify) = match lookup {
            Lookup::Entry => {
                let (identity, changed) = cache.read(asura_policy::ENTRY);
                (Found::Entry(identity), changed)
            }
            Lookup::Root(key) => {
                let result = asura_platform::canonical_root(&key.location, key.device, key.inode);
                (Found::Root(key, result), true)
            }
        };
        if reply.send(found).is_err() {
            return;
        }
        if notify && let Some(wake) = &wake {
            let _ = wake.notify();
        }
    }
}
