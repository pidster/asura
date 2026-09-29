use super::{database::Database, types::*};
use std::{
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const COMMAND_TIME: Duration = Duration::from_secs(2);
static GENERATION: AtomicU64 = AtomicU64::new(1);
// HM0 admits one engine instance per process. The worker owns this claim,
// not the public handle, so dropping a handle cannot permit replacement.
static CLAIM: Mutex<Option<PathBuf>> = Mutex::new(None);
// Serialize fixture lifetimes that exercise the process-wide production claim.
#[cfg(test)]
pub(super) static TEST_ENGINE: Mutex<()> = Mutex::new(());
pub(crate) struct Claim;
impl Claim {
    fn acquire(root: &std::path::Path) -> Result<Self> {
        super::database::validate_root_spelling(root)?;
        Self::acquire_validated(root)
    }
    pub(crate) fn acquire_validated(root: &std::path::Path) -> Result<Self> {
        let mut held = CLAIM.try_lock().map_err(|_| Error::Busy)?;
        if held.is_some() {
            return Err(Error::Busy);
        }
        *held = Some(root.to_owned());
        Ok(Self)
    }
}
impl Drop for Claim {
    fn drop(&mut self) {
        *CLAIM.lock().unwrap_or_else(|poison| poison.into_inner()) = None;
    }
}
enum Command {
    Put(PutNote),
    List(ListNotes),
    Get(Binding, Id, Id),
    Sources(Binding, Id, Id),
    Resolve(Id, [u8; 32]),
}
enum Reply {
    Binding(Binding),
    Receipt(Receipt),
    Note(Note),
    Page(NotePage),
    Sources(Vec<Note>),
}
struct Completion {
    generation: u64,
    counter: u64,
    result: Result<Reply>,
}
struct Request {
    command: Command,
    cancelled: Arc<AtomicBool>,
    deadline: Instant,
    counter: u64,
    reply: SyncSender<Completion>,
}

/// Pollable result; expiry never asserts that an in-flight engine operation stopped.
pub struct Pending<T> {
    receiver: Receiver<Completion>,
    cancelled: Arc<AtomicBool>,
    deadline: Instant,
    mutation: bool,
    generation: u64,
    counter: u64,
    done: bool,
    decode: fn(Reply) -> Result<T>,
}
impl<T> Pending<T> {
    pub fn poll(&mut self) -> Option<Result<T>> {
        if self.done {
            return None;
        }
        if !self.mutation && self.cancelled.load(Ordering::Acquire) {
            self.done = true;
            return Some(Err(Error::Cancelled));
        }
        match self.receiver.try_recv() {
            Ok(reply) => {
                self.done = true;
                Some(
                    if reply.generation != self.generation || reply.counter != self.counter {
                        Err(Error::InvalidRecord)
                    } else {
                        reply.result.and_then(self.decode)
                    },
                )
            }
            Err(TryRecvError::Disconnected) => {
                self.done = true;
                Some(Err(if self.mutation {
                    Error::OutcomeUnconfirmed
                } else {
                    Error::Unavailable
                }))
            }
            Err(TryRecvError::Empty) if Instant::now() >= self.deadline => {
                self.cancelled.store(true, Ordering::Release);
                self.done = true;
                Some(Err(if self.mutation {
                    Error::OutcomeUnconfirmed
                } else {
                    Error::Timeout
                }))
            }
            Err(TryRecvError::Empty) => None,
        }
    }
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }
}
impl<T> Drop for Pending<T> {
    fn drop(&mut self) {
        self.cancel();
    }
}

/// One optional scratch database owner. No method waits for storage or shutdown.
pub struct Memory {
    sender: SyncSender<Option<Request>>,
    closing: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
    generation: u64,
    counter: AtomicU64,
    close_started: Option<Instant>,
}
impl Memory {
    pub fn initialize(root: PathBuf, binding: Binding) -> Result<(Self, Pending<Binding>)> {
        Self::start(root, binding, true)
    }
    pub fn open(root: PathBuf, binding: Binding) -> Result<(Self, Pending<Binding>)> {
        Self::start(root, binding, false)
    }
    fn start(
        root: PathBuf,
        binding: Binding,
        initialize: bool,
    ) -> Result<(Self, Pending<Binding>)> {
        let claim = Claim::acquire(&root)?;
        let generation = GENERATION
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1))
            .map_err(|_| Error::Unavailable)?;
        let (sender, receiver) = mpsc::sync_channel(8);
        let (reply, response) = mpsc::sync_channel(1);
        let closing = Arc::new(AtomicBool::new(false));
        let stop = closing.clone();
        let cancelled = Arc::new(AtomicBool::new(false));
        let startup_cancel = cancelled.clone();
        let deadline = Instant::now() + Duration::from_secs(10);
        let handle = thread::Builder::new()
            .name("asura-memory".into())
            .spawn(move || {
                let _claim = claim;
                let runtime = match tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(2)
                    .max_blocking_threads(2)
                    .thread_stack_size(10 * 1024 * 1024)
                    .enable_time()
                    .build()
                {
                    Ok(runtime) => runtime,
                    Err(_) => {
                        let _ = reply.try_send(Completion {
                            generation,
                            counter: 0,
                            result: Err(Error::Unavailable),
                        });
                        return;
                    }
                };
                if startup_cancel.load(Ordering::Acquire) || stop.load(Ordering::Acquire) {
                    let _ = reply.try_send(Completion {
                        generation,
                        counter: 0,
                        result: Err(Error::Cancelled),
                    });
                    return;
                }
                let connected = runtime.block_on(Database::connect(
                    &root,
                    binding.clone(),
                    initialize,
                    deadline,
                ));
                match connected {
                    Ok(database) => {
                        if startup_cancel.load(Ordering::Acquire) {
                            stop.store(true, Ordering::Release);
                        }
                        let _ = reply.try_send(Completion {
                            generation,
                            counter: 0,
                            result: Ok(Reply::Binding(binding)),
                        });
                        while !stop.load(Ordering::Acquire) {
                            let request: Request = match receiver.recv() {
                                Ok(Some(request)) => request,
                                Ok(None) | Err(_) => break,
                            };
                            let result = if stop.load(Ordering::Acquire)
                                || request.cancelled.load(Ordering::Acquire)
                            {
                                Err(Error::Cancelled)
                            } else if Instant::now() >= request.deadline {
                                Err(Error::Timeout)
                            } else {
                                {
                                    database.set_deadline(request.deadline);
                                    runtime.block_on(execute(&database, request.command))
                                }
                            };
                            let _ = request.reply.try_send(Completion {
                                generation,
                                counter: request.counter,
                                result,
                            });
                        }
                        drop(database);
                    }
                    Err(error) => {
                        let _ = reply.try_send(Completion {
                            generation,
                            counter: 0,
                            result: Err(error),
                        });
                    }
                }
                while let Ok(message) = receiver.try_recv() {
                    let Some(request) = message else { continue };
                    let _ = request.reply.try_send(Completion {
                        generation,
                        counter: request.counter,
                        result: Err(Error::Cancelled),
                    });
                }
                // SDK drop closes the router. Keep its private executor alive until its
                // shutdown task and engine background tasks finish. Never abort them to
                // manufacture successful close; the caller retains this unsettled owner.
                while runtime.metrics().num_alive_tasks() != 0 {
                    thread::sleep(Duration::from_millis(10));
                }
                drop(runtime);
            })
            .map_err(|_| Error::Unavailable)?;
        let owner = Self {
            sender,
            closing,
            handle: Some(handle),
            generation,
            counter: AtomicU64::new(1),
            close_started: None,
        };
        let pending = Pending {
            receiver: response,
            cancelled,
            deadline,
            mutation: initialize,
            generation,
            counter: 0,
            done: false,
            decode: |reply| match reply {
                Reply::Binding(binding) => Ok(binding),
                _ => Err(Error::InvalidRecord),
            },
        };
        Ok((owner, pending))
    }
    fn submit<T>(
        &self,
        command: Command,
        mutation: bool,
        decode: fn(Reply) -> Result<T>,
    ) -> Result<Pending<T>> {
        if self.closing.load(Ordering::Acquire)
            || self.handle.as_ref().is_none_or(JoinHandle::is_finished)
        {
            return Err(Error::Closed);
        }
        let counter = self
            .counter
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1))
            .map_err(|_| Error::Unavailable)?;
        let (reply, receiver) = mpsc::sync_channel(1);
        let cancelled = Arc::new(AtomicBool::new(false));
        let deadline = Instant::now() + COMMAND_TIME;
        self.sender
            .try_send(Some(Request {
                command,
                cancelled: cancelled.clone(),
                deadline,
                counter,
                reply,
            }))
            .map_err(|error| match error {
                TrySendError::Full(_) => Error::Busy,
                TrySendError::Disconnected(_) => Error::Closed,
            })?;
        Ok(Pending {
            receiver,
            cancelled,
            deadline,
            mutation,
            generation: self.generation,
            counter,
            done: false,
            decode,
        })
    }
    pub fn put_note(&self, command: PutNote) -> Result<Pending<Receipt>> {
        command.command_digest()?;
        self.submit(Command::Put(command), true, |reply| match reply {
            Reply::Receipt(v) => Ok(v),
            _ => Err(Error::InvalidRecord),
        })
    }
    pub fn list_notes(&self, command: ListNotes) -> Result<Pending<NotePage>> {
        command.validate()?;
        self.submit(Command::List(command), false, |reply| match reply {
            Reply::Page(page) => Ok(page),
            _ => Err(Error::InvalidRecord),
        })
    }
    pub fn get_note(&self, binding: Binding, context: Id, version: Id) -> Result<Pending<Note>> {
        self.submit(
            Command::Get(binding, context, version),
            false,
            |reply| match reply {
                Reply::Note(v) => Ok(v),
                _ => Err(Error::InvalidRecord),
            },
        )
    }
    pub fn get_sources(
        &self,
        binding: Binding,
        context: Id,
        version: Id,
    ) -> Result<Pending<Vec<Note>>> {
        self.submit(
            Command::Sources(binding, context, version),
            false,
            |reply| match reply {
                Reply::Sources(v) => Ok(v),
                _ => Err(Error::InvalidRecord),
            },
        )
    }
    pub fn resolve(&self, operation: Id, digest: [u8; 32]) -> Result<Pending<Receipt>> {
        self.submit(
            Command::Resolve(operation, digest),
            false,
            |reply| match reply {
                Reply::Receipt(v) => Ok(v),
                _ => Err(Error::InvalidRecord),
            },
        )
    }
    /// Returns true only after runtime destruction. On timeout retain this owner
    /// and keep polling; timeout does not release its database or claim settlement.
    pub fn close(&mut self) -> Result<bool> {
        self.closing.store(true, Ordering::Release);
        // A full queue already wakes the receiver; closing is checked before new work.
        let _ = self.sender.try_send(None);
        let started = *self.close_started.get_or_insert_with(Instant::now);
        if self.handle.as_ref().is_some_and(JoinHandle::is_finished) {
            return self
                .handle
                .take()
                .unwrap()
                .join()
                .map(|_| true)
                .map_err(|_| Error::Unavailable);
        }
        if self.handle.is_none() {
            return Ok(true);
        }
        if started.elapsed() >= COMMAND_TIME {
            Err(Error::Timeout)
        } else {
            Ok(false)
        }
    }
}
impl Drop for Memory {
    fn drop(&mut self) {
        self.closing.store(true, Ordering::Release);
        // A full queue already wakes the receiver; closing is checked before new work.
        let _ = self.sender.try_send(None);
    }
}
async fn execute(database: &Database, command: Command) -> Result<Reply> {
    match command {
        Command::List(command) => database.list(command).await.map(Reply::Page),
        Command::Put(command) => database.put(command).await.map(Reply::Receipt),
        Command::Get(binding, context, version) => database
            .get(binding, context, version)
            .await
            .map(Reply::Note),
        Command::Sources(binding, context, version) => database
            .sources(binding, context, version)
            .await
            .map(Reply::Sources),
        Command::Resolve(operation, digest) => database
            .resolve(operation, digest)
            .await
            .map(Reply::Receipt),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn dropped_handle_keeps_claim_until_stalled_thread_settles() {
        let _fixture = TEST_ENGINE.lock().unwrap();
        let path = PathBuf::from("/private/tmp/asura-memory-claim-unit");
        let claim = Claim::acquire(&path).unwrap();
        let (sender, receiver) = mpsc::sync_channel(8);
        let (release, gate) = mpsc::sync_channel(1);
        let (finished, done) = mpsc::sync_channel(1);
        let handle = thread::spawn(move || {
            let claim = claim;
            gate.recv().unwrap();
            drop(receiver);
            drop(claim);
            finished.send(()).unwrap();
        });
        let memory = Memory {
            sender,
            closing: Arc::new(AtomicBool::new(false)),
            handle: Some(handle),
            generation: 18,
            counter: AtomicU64::new(1),
            close_started: None,
        };
        drop(memory);
        assert!(matches!(Claim::acquire(&path), Err(Error::Busy)));
        release.send(()).unwrap();
        done.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(Claim::acquire(&path).is_ok());
    }
    #[test]
    fn bounded_queue_and_pending_expiry_do_not_join_stalled_owner() {
        let (sender, receiver) = mpsc::sync_channel(8);
        let (release, gate) = mpsc::sync_channel(1);
        let handle = thread::spawn(move || {
            gate.recv().unwrap();
            drop(receiver);
        });
        let mut memory = Memory {
            sender,
            closing: Arc::new(AtomicBool::new(false)),
            handle: Some(handle),
            generation: 17,
            counter: AtomicU64::new(1),
            close_started: None,
        };
        let id = Id::new([1; 16]).unwrap();
        let mut pending = Vec::new();
        for _ in 0..8 {
            pending.push(memory.resolve(id, [1; 32]).unwrap());
        }
        assert!(matches!(memory.resolve(id, [1; 32]), Err(Error::Busy)));
        pending[0].deadline = Instant::now();
        assert_eq!(pending[0].poll(), Some(Err(Error::Timeout)));
        assert!(matches!(memory.resolve(id, [1; 32]), Err(Error::Busy)));
        let now = Instant::now();
        assert_eq!(memory.close(), Ok(false));
        assert!(now.elapsed() < Duration::from_millis(100));
        assert!(matches!(memory.resolve(id, [1; 32]), Err(Error::Closed)));
        release.send(()).unwrap();
        let end = Instant::now() + Duration::from_secs(1);
        while memory.close() != Ok(true) {
            assert!(Instant::now() < end);
            thread::yield_now();
        }
    }
    #[test]
    fn idle_command_receiver_wakes_on_close_and_drop() {
        for explicit_close in [false, true] {
            let (sender, receiver) = mpsc::sync_channel(8);
            let closing = Arc::new(AtomicBool::new(false));
            let stop = closing.clone();
            let (done, observed) = mpsc::channel();
            let handle = thread::spawn(move || {
                assert!(matches!(receiver.recv(), Ok(None)));
                assert!(stop.load(Ordering::Acquire));
                done.send(()).unwrap();
            });
            let mut memory = Memory {
                sender,
                closing,
                handle: Some(handle),
                generation: 1,
                counter: AtomicU64::new(1),
                close_started: None,
            };
            if explicit_close {
                let _ = memory.close();
                observed.recv_timeout(Duration::from_secs(1)).unwrap();
                while memory
                    .handle
                    .as_ref()
                    .is_some_and(|handle| !handle.is_finished())
                {
                    thread::yield_now();
                }
                assert_eq!(memory.close(), Ok(true));
            } else {
                // Retain the test's exact worker handle while exercising production Drop.
                let handle = memory.handle.take().unwrap();
                drop(memory);
                observed.recv_timeout(Duration::from_secs(1)).unwrap();
                handle.join().unwrap();
            }
        }
    }
}
