//! Bounded local event delivery. Durable admission remains the caller's concern.
use std::{
    collections::VecDeque,
    io::{self, Read, Write},
    os::{
        fd::{AsRawFd, RawFd},
        unix::net::UnixStream,
    },
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

struct WakePair {
    reader: UnixStream,
    writer: UnixStream,
}
#[derive(Clone)]
pub struct WakeSender(Arc<WakePair>);
pub struct WakeReader(Arc<WakePair>);
/// The sender retains both endpoints, preventing broken-pipe signals during teardown.
pub fn wake_pair() -> io::Result<(WakeReader, WakeSender)> {
    let (reader, writer) = UnixStream::pair()?;
    reader.set_nonblocking(true)?;
    writer.set_nonblocking(true)?;
    let pair = Arc::new(WakePair { reader, writer });
    Ok((WakeReader(pair.clone()), WakeSender(pair)))
}
impl WakeSender {
    pub fn notify(&self) -> io::Result<()> {
        loop {
            match (&self.0.writer).write(&[1]) {
                Ok(1) => return Ok(()),
                Ok(_) => return Err(io::ErrorKind::WriteZero.into()),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
        }
    }
}
impl AsRawFd for WakeSender {
    fn as_raw_fd(&self) -> RawFd {
        self.0.writer.as_raw_fd()
    }
}
impl WakeReader {
    pub fn drain(&self) -> io::Result<()> {
        let mut bytes = [0; 256];
        for _ in 0..64 {
            match (&self.0.reader).read(&mut bytes) {
                Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
                Ok(_) => (),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => (),
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }
}
impl AsRawFd for WakeReader {
    fn as_raw_fd(&self) -> RawFd {
        self.0.reader.as_raw_fd()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Priority {
    Critical,
    Interaction,
    Completion,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Rejection {
    Full,
    Contended,
    Closed,
    Oversize,
    InvalidKey,
}
#[derive(Debug)]
pub struct SendError<T> {
    pub reason: Rejection,
    pub event: T,
}
struct Item<T> {
    value: T,
    bytes: usize,
    key: Option<u8>,
}
struct State<T> {
    lanes: [VecDeque<Item<T>>; 4],
    bytes: [usize; 4],
    cursor: usize,
}
impl<T> Default for State<T> {
    fn default() -> Self {
        Self {
            lanes: std::array::from_fn(|_| VecDeque::new()),
            bytes: [0; 4],
            cursor: 0,
        }
    }
}
struct Shared<T> {
    state: Mutex<State<T>>,
    closed: AtomicBool,
    wake: WakeSender,
}
pub struct Sender<T>(Arc<Shared<T>>);
impl<T> Clone for Sender<T> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}
pub struct Inbox<T> {
    shared: Arc<Shared<T>>,
    wake: WakeReader,
}
const CAPACITY: [usize; 4] = [8, 128, 32, 8];

const BYTE_LIMIT: usize = 64 * 1024;
const EVENT_LIMIT: usize = 32 * 1024;
pub fn inbox<T>() -> io::Result<(Inbox<T>, Sender<T>)> {
    let (wake, sender) = wake_pair()?;
    let shared = Arc::new(Shared {
        state: Mutex::new(State::default()),
        closed: AtomicBool::new(false),
        wake: sender,
    });
    Ok((
        Inbox {
            shared: shared.clone(),
            wake,
        },
        Sender(shared),
    ))
}
impl<T> Sender<T> {
    /// On rejection the caller retains the event. Never discard required outcomes.
    pub fn try_send(&self, priority: Priority, event: T, bytes: usize) -> Result<(), SendError<T>> {
        let lane = match priority {
            Priority::Critical => 0,
            Priority::Interaction => 1,
            Priority::Completion => 2,
        };
        self.push(lane, None, event, bytes)
    }
    pub fn latest(&self, key: u8, event: T, bytes: usize) -> Result<(), SendError<T>> {
        if key >= 8 {
            return Err(SendError {
                reason: Rejection::InvalidKey,
                event,
            });
        }
        self.push(3, Some(key), event, bytes)
    }
    pub fn wake(&self) -> WakeSender {
        self.0.wake.clone()
    }
    fn push(
        &self,
        lane: usize,
        key: Option<u8>,
        event: T,
        bytes: usize,
    ) -> Result<(), SendError<T>> {
        let reject = |reason, event| {
            let _ = self.0.wake.notify();
            Err(SendError { reason, event })
        };
        if bytes > EVENT_LIMIT {
            return reject(Rejection::Oversize, event);
        }
        if self.0.closed.load(Ordering::Acquire) {
            return reject(Rejection::Closed, event);
        }
        let Ok(mut state) = self.0.state.try_lock() else {
            return reject(Rejection::Contended, event);
        };
        if self.0.closed.load(Ordering::Acquire) {
            return reject(Rejection::Closed, event);
        }
        let replace = key.and_then(|key| {
            state.lanes[lane]
                .iter()
                .position(|item| item.key == Some(key))
        });
        let old_bytes = replace.map(|i| state.lanes[lane][i].bytes).unwrap_or(0);
        if (replace.is_none() && state.lanes[lane].len() >= CAPACITY[lane])
            || state.bytes[lane] - old_bytes + bytes > BYTE_LIMIT
        {
            return reject(Rejection::Full, event);
        }
        state.bytes[lane] = state.bytes[lane] - old_bytes + bytes;
        let item = Item {
            value: event,
            bytes,
            key,
        };
        let replaced = if let Some(index) = replace {
            Some(std::mem::replace(&mut state.lanes[lane][index], item))
        } else {
            state.lanes[lane].push_back(item);
            None
        };
        drop(state);
        // Drop replaced payload outside the queue lock.
        drop(replaced);
        let _ = self.0.wake.notify();
        Ok(())
    }
}
impl<T> AsRawFd for Inbox<T> {
    fn as_raw_fd(&self) -> RawFd {
        self.wake.as_raw_fd()
    }
}
impl<T> Inbox<T> {
    /// One bounded priority dispatch batch. Payload processing happens after return.
    pub fn drain(&self, limit: usize) -> io::Result<Vec<T>> {
        let limit = limit.min(64);
        self.wake.drain()?;
        let Ok(mut state) = self.shared.state.try_lock() else {
            self.shared.wake.notify()?;
            return Ok(Vec::new());
        };
        let mut result = Vec::with_capacity(limit);
        while result.len() < limit {
            let mut found = false;
            for _ in 0..21 {
                let lane = match state.cursor {
                    0..=7 => 0,
                    8..=15 => 1,
                    16..=19 => 2,
                    _ => 3,
                };
                state.cursor = (state.cursor + 1) % 21;
                if let Some(item) = state.lanes[lane].pop_front() {
                    state.bytes[lane] -= item.bytes;
                    result.push(item.value);
                    found = true;
                    break;
                }
            }
            if !found {
                break;
            }
        }
        let pending = state.lanes.iter().any(|lane| !lane.is_empty());
        if !pending {
            state.cursor = 0;
        }
        drop(state);
        if pending {
            self.shared.wake.notify()?;
        }
        Ok(result)
    }
}
impl<T> Drop for Inbox<T> {
    fn drop(&mut self) {
        self.shared.closed.store(true, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    #[test]
    fn priority_fifo_coalescing_and_partial_drain_rearm() {
        let (inbox, sender) = inbox().unwrap();
        sender.latest(0, 99, 1).unwrap();
        sender.latest(0, 100, 1).unwrap();
        sender.try_send(Priority::Completion, 30, 1).unwrap();
        sender.try_send(Priority::Interaction, 20, 1).unwrap();
        sender.try_send(Priority::Interaction, 21, 1).unwrap();
        sender.try_send(Priority::Critical, 10, 1).unwrap();
        assert_eq!(inbox.drain(2).unwrap(), vec![10, 20]);
        let ready = crate::poll(
            &[crate::PollInterest {
                fd: inbox.as_raw_fd(),
                read: true,
                write: false,
            }],
            Duration::from_millis(50),
        )
        .unwrap();
        assert!(!ready.is_empty());
        assert_eq!(inbox.drain(64).unwrap(), vec![21, 30, 100]);
    }
    #[test]
    fn bounds_and_closure_return_original_event() {
        let (inbox, sender) = inbox().unwrap();
        for value in 0..128 {
            sender.try_send(Priority::Interaction, value, 1).unwrap();
        }
        let rejected = sender.try_send(Priority::Interaction, 999, 1).unwrap_err();
        assert_eq!((rejected.reason, rejected.event), (Rejection::Full, 999));
        sender.try_send(Priority::Critical, 1000, 1).unwrap();
        assert_eq!(
            sender.latest(8, 1, 0).unwrap_err().reason,
            Rejection::InvalidKey
        );
        assert_eq!(
            sender.latest(0, 1, EVENT_LIMIT + 1).unwrap_err().reason,
            Rejection::Oversize
        );
        sender
            .try_send(Priority::Completion, 1, EVENT_LIMIT)
            .unwrap();
        sender
            .try_send(Priority::Completion, 2, EVENT_LIMIT)
            .unwrap();
        assert_eq!(
            sender
                .try_send(Priority::Completion, 3, 1)
                .unwrap_err()
                .reason,
            Rejection::Full
        );
        drop(inbox);
        assert_eq!(
            sender
                .try_send(Priority::Critical, 2, 0)
                .unwrap_err()
                .reason,
            Rejection::Closed
        );
    }
    #[test]
    fn saturated_interactions_allow_completions_and_telemetry() {
        let (inbox, sender) = inbox().unwrap();
        for value in 0..128 {
            sender.try_send(Priority::Interaction, value, 0).unwrap();
        }
        sender.try_send(Priority::Completion, 500, 0).unwrap();
        sender.latest(0, 600, 0).unwrap();
        let batch = inbox.drain(64).unwrap();
        assert_eq!(batch[8], 500);
        assert_eq!(batch[9], 600);
        assert_eq!(batch.len(), 64);
    }
    #[test]
    fn contention_is_nonblocking_and_keeps_notification() {
        let (inbox, sender) = inbox().unwrap();
        let guard = inbox.shared.state.lock().unwrap();
        assert_eq!(
            sender
                .try_send(Priority::Critical, 7, 1)
                .unwrap_err()
                .reason,
            Rejection::Contended
        );
        assert!(inbox.drain(64).unwrap().is_empty());
        drop(guard);
        sender.try_send(Priority::Critical, 7, 1).unwrap();
        assert_eq!(inbox.drain(64).unwrap(), vec![7]);
    }
    #[test]
    fn short_batches_preserve_fairness() {
        let (inbox, sender) = inbox().unwrap();
        for value in 0..128 {
            sender.try_send(Priority::Interaction, value, 0).unwrap();
        }
        sender.latest(0, 999, 0).unwrap();
        let values: Vec<_> = (0..9).flat_map(|_| inbox.drain(1).unwrap()).collect();
        assert_eq!(values[8], 999);
    }
    #[test]
    fn publication_racing_drain_cannot_lose_readiness() {
        let (inbox, sender) = inbox().unwrap();
        let worker = std::thread::spawn(move || {
            for value in 0..256 {
                let mut value = value;
                loop {
                    match sender.try_send(Priority::Completion, value, 1) {
                        Ok(()) => break,
                        Err(error) => {
                            value = error.event;
                            std::thread::yield_now();
                        }
                    }
                }
            }
        });
        let mut received = Vec::new();
        while received.len() < 256 {
            let ready = crate::poll(
                &[crate::PollInterest {
                    fd: inbox.as_raw_fd(),
                    read: true,
                    write: false,
                }],
                Duration::from_secs(2),
            )
            .unwrap();
            assert!(!ready.is_empty(), "publication lost its wakeup");
            received.extend(inbox.drain(7).unwrap());
        }
        worker.join().unwrap();
        assert_eq!(received, (0..256).collect::<Vec<_>>());
    }
    #[test]
    fn real_thread_publication_wakes_poll() {
        let (inbox, sender) = inbox().unwrap();
        let worker =
            std::thread::spawn(move || sender.try_send(Priority::Completion, 42, 1).unwrap());
        let events = crate::poll(
            &[crate::PollInterest {
                fd: inbox.as_raw_fd(),
                read: true,
                write: false,
            }],
            Duration::from_secs(2),
        )
        .unwrap();
        assert!(!events.is_empty());
        assert_eq!(inbox.drain(64).unwrap(), vec![42]);
        worker.join().unwrap();
    }
}
