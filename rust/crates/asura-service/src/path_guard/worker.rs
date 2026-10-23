//! The path guard worker (execution-path-guard.md, AE-01 to AE-06): one
//! retained thread, `asura-path-guard`, that walks a project's
//! implicit-execution list and hashes its files for the conversation owner.
//! The owner never blocks on it: a request is admitted only while fewer
//! than [`QUEUE_BOUND`] are outstanding, and its reply is polled from a
//! ticket. No request has a time bound: it ends by its reply, its ticket's
//! drop (a Hold or Drop of the waiting entry cancels it), or shutdown.
//! A stalled filesystem keeps the worker, and the entries that wait on it,
//! waiting; every other lane and the TUI stay responsive.
use super::check::Facts;
use super::lists::List;
use super::walk::{self, FileState, WalkError};
use asura_platform::events::WakeSender;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError};
use std::thread::JoinHandle;

/// Outstanding requests, the running one included (AE-03).
pub(crate) const QUEUE_BOUND: usize = 8;

/// The project a request reads: its registered location and identity. The
/// worker reopens the location without following links and compares the
/// identity before it walks.
#[derive(Clone)]
pub(crate) struct Root {
    pub path: String,
    pub device: u64,
    pub inode: u64,
    pub boundary: asura_platform::StateBoundary,
}

/// One request.
pub(crate) enum Work {
    /// The implicit-execution files and the state of each approved path.
    Check { approved: Vec<String> },
    /// The state of one implicit-execution file, for an approval.
    Digest { path: String },
}

/// One reply.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Reply {
    Checked(Facts),
    /// The file's state and its display-redacted record path (`None`
    /// when it cannot be recorded).
    Digest(FileState, Option<String>),
    /// The project is not the registered directory, or cannot be read.
    Unavailable,
    Cancelled,
}

/// A ticket's state when polled.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Polled {
    Pending,
    Ready(Reply),
    /// The worker exited without replying.
    Lost,
}

/// One admitted request's reply slot. Polling never blocks; dropping the
/// ticket cancels the request.
pub(crate) struct Ticket {
    receiver: Receiver<Reply>,
    cancel: Arc<AtomicBool>,
}
impl Ticket {
    pub(crate) fn poll(&self) -> Polled {
        match self.receiver.try_recv() {
            Ok(reply) => Polled::Ready(reply),
            Err(TryRecvError::Empty) => Polled::Pending,
            Err(TryRecvError::Disconnected) => Polled::Lost,
        }
    }
}
impl Drop for Ticket {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Release);
    }
}

/// A request was not admitted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Refused {
    /// [`QUEUE_BOUND`] requests are outstanding.
    Busy,
    /// The worker has exited.
    Stopped,
}

struct Job {
    root: Root,
    work: Work,
    cancel: Arc<AtomicBool>,
    reply: SyncSender<Reply>,
}

/// The path guard worker's queue and thread.
pub(crate) struct Guard {
    sender: Option<SyncSender<Job>>,
    outstanding: Arc<AtomicUsize>,
    thread: Option<JoinHandle<()>>,
}

impl Guard {
    /// Starts the worker. `gate`, in test-support builds only, holds each
    /// request while that file exists (EPG-I5, a stalled filesystem).
    pub(crate) fn start(
        wake: Option<WakeSender>,
        gate: Option<std::path::PathBuf>,
    ) -> std::io::Result<Self> {
        let (sender, receiver) = mpsc::sync_channel::<Job>(QUEUE_BOUND);
        let outstanding = Arc::new(AtomicUsize::new(0));
        let count = outstanding.clone();
        let thread = std::thread::Builder::new()
            .name("asura-path-guard".into())
            .spawn(move || {
                while let Ok(job) = receiver.recv() {
                    hold(gate.as_deref(), &job.cancel);
                    let reply = run(&job.root, &job.work, &job.cancel);
                    let _ = job.reply.try_send(reply);
                    count.fetch_sub(1, Ordering::AcqRel);
                    if let Some(wake) = &wake {
                        let _ = wake.notify();
                    }
                }
            })?;
        Ok(Self {
            sender: Some(sender),
            outstanding,
            thread: Some(thread),
        })
    }

    /// Whether another request would be admitted now.
    pub(crate) fn has_room(&self) -> bool {
        self.sender.is_some() && self.outstanding.load(Ordering::Acquire) < QUEUE_BOUND
    }

    /// Admits one request, or refuses it at once.
    pub(crate) fn submit(&self, root: Root, work: Work) -> Result<Ticket, Refused> {
        let sender = self.sender.as_ref().ok_or(Refused::Stopped)?;
        if self
            .outstanding
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                (n < QUEUE_BOUND).then_some(n + 1)
            })
            .is_err()
        {
            return Err(Refused::Busy);
        }
        let (reply, receiver) = mpsc::sync_channel(1);
        let cancel = Arc::new(AtomicBool::new(false));
        let job = Job {
            root,
            work,
            cancel: cancel.clone(),
            reply,
        };
        match sender.try_send(job) {
            Ok(()) => Ok(Ticket { receiver, cancel }),
            Err(error) => {
                self.outstanding.fetch_sub(1, Ordering::AcqRel);
                Err(match error {
                    mpsc::TrySendError::Full(_) => Refused::Busy,
                    mpsc::TrySendError::Disconnected(_) => Refused::Stopped,
                })
            }
        }
    }

    /// Shutdown (AE-06): closes the queue. A running request finishes, or
    /// is dropped with its ticket; the thread is not joined, because a
    /// stalled filesystem may hold it.
    pub(crate) fn close(&mut self) {
        self.sender = None;
        drop(self.thread.take());
    }
}

/// Test-support gate: waits while the gate file exists and the request is
/// not cancelled. Production passes no gate.
fn hold(gate: Option<&Path>, cancel: &AtomicBool) {
    let Some(gate) = gate else {
        return;
    };
    while gate.exists() && !cancel.load(Ordering::Acquire) {
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}

/// One request on the worker thread.
fn run(root: &Root, work: &Work, cancel: &AtomicBool) -> Reply {
    if cancel.load(Ordering::Acquire) {
        return Reply::Cancelled;
    }
    // The registered directory, reopened without following links.
    match asura_platform::ProjectIdentity::open(&root.path, &root.boundary) {
        Ok(p) if p.device == root.device && p.inode == root.inode => {}
        _ => return Reply::Unavailable,
    }
    let path = Path::new(&root.path);
    match work {
        Work::Digest { path: relative } => Reply::Digest(
            walk::digest(path, relative, cancel),
            super::display(relative),
        ),
        Work::Check { approved } => match walk::walk(path, List::Implicit, cancel) {
            Err(WalkError::Cancelled) => Reply::Cancelled,
            Err(WalkError::Unavailable) => Reply::Unavailable,
            Ok(found) => {
                let mut facts = Facts {
                    overflow: found.overflow,
                    ..Facts::default()
                };
                for raw in found.paths.keys() {
                    let Some(shown) = super::display(raw) else {
                        facts.overflow = true;
                        continue;
                    };
                    let state = walk::digest(path, raw, cancel);
                    facts.files.insert(shown, state);
                }
                for shown in approved {
                    if !facts.files.contains_key(shown) {
                        let state = walk::digest(path, shown, cancel);
                        facts.approved.insert(shown.clone(), state);
                    }
                }
                if cancel.load(Ordering::Acquire) {
                    return Reply::Cancelled;
                }
                Reply::Checked(facts)
            }
        },
    }
}
