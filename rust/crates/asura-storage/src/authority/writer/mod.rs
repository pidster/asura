//! Serialized, pollable journal ownership. All filesystem work stays on its worker.
use super::conversation::*;
use asura_platform::{JournalFile, ProjectIdentity, RuntimeDirectory};
use std::ops::Range;
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
    mpsc::{self, Receiver, SyncSender, TryRecvError},
};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
mod state;
#[cfg(feature = "test-support")]
pub mod test_support;
/// Journal owner allocation ceiling; embedded engine resources are separate.
pub const MAX_JOURNAL_OWNER_MEMORY: usize = 64 * 1024 * 1024;
// Retired/current/proposed replay + journal + queue + frame + history + project page.
const _: () = assert!(
    3 * 16 * 1024 * 1024 + 8 * 1024 * 1024 + 8 * 65536 + 65536 + 65536 + 40960
        < MAX_JOURNAL_OWNER_MEMORY
);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Busy,
    Unavailable,
    NotInitialized,
    AlreadyInitialized,
    RepairRequired,
    OutcomeUnconfirmed,
    Deadline,
    Cancelled,
    Conflict,
    Invalid,
    StaleProject,
    ModelUnavailable,
    Limit,
}
pub type Result<T> = std::result::Result<T, Error>;
#[derive(Debug)]
pub enum Command {
    #[cfg(feature = "embedded-memory")]
    MemoryCreate {
        cancel: Arc<AtomicBool>,
        turn: Id,
        generation: u64,
        ordinal: u32,
        body: String,
    },
    #[cfg(feature = "embedded-memory")]
    MemoryResolveCreate {
        turn: Id,
        generation: u64,
        ordinal: u32,
    },
    #[cfg(feature = "embedded-memory")]
    MemoryRead {
        project: Id,
        query: crate::memory::ReadNotes,
    },
    Open,
    SensorLoad {
        project: Id,
    },
    SensorStore {
        expected_revision: u64,
        state: crate::sensors::ProjectState,
    },
    Enqueue {
        request: Id,
        project: Id,
        conversation: Id,
        target_operation: Id,
        target_generation: u64,
        kind: InputKind,
        prompt: String,
    },
    QueueInput {
        request: Id,
        project: Id,
        conversation: Option<Id>,
        expected_generation: u64,
        new_conversation: bool,
        prompt: String,
    },
    ReorderInput {
        request: Id,
        input: Id,
        after: Option<Id>,
        expected_order_revision: u64,
    },
    Inputs {
        project: Id,
        input: Option<Id>,
    },
    InputDecision {
        request: Id,
        input: Id,
        action: InputAction,
    },
    PromoteInput {
        request: Id,
        input: Id,
        target_operation: Id,
        target_generation: u64,
    },
    PrepareInput {
        input: Id,
    },
    Initialize {
        request: Id,
    },
    Projects {
        after: Option<Id>,
        limit: usize,
    },
    Register {
        request: Id,
        location: String,
    },
    RenameProject {
        request: Id,
        project: Id,
        expected_name_revision: u64,
        name: String,
    },
    Prepare {
        project: Id,
        conversation: Option<Id>,
        expected_generation: u64,
        prompt: String,
    },
    Append {
        expected_revision: u64,
        record: Record,
        admission: Option<AdmissionFence>,
    },
    ReadRecord {
        range: Range<usize>,
    },
    History {
        project: Id,
        conversation: Option<Id>,
        before_accepted_frame: Option<usize>,
        limit: usize,
    },
    ReadPrompt {
        project: Id,
        operation: Id,
    },
}
#[derive(Clone, Debug)]
pub struct AdmissionFence {
    pub project: Id,
    pub configuration_digest: Hash,
}
#[derive(Debug)]
pub struct Prepared {
    pub model: String,
    pub asset_root: String,
    pub ollama_endpoint: Option<String>,
    pub model_capabilities: Option<u32>,
    pub configuration_digest: Hash,
    pub history: Vec<(TurnAccepted, TurnTerminal)>,
}
#[derive(Debug)]
pub struct Reply {
    #[cfg(feature = "embedded-memory")]
    pub memory_create_result: Option<crate::memory::Result<crate::memory::MemoryCreateResult>>,
    #[cfg(feature = "embedded-memory")]
    pub memory_result: Option<crate::memory::Result<crate::memory::ReadResult>>,
    pub sensor_state: Option<crate::sensors::ProjectState>,
    pub replay: Option<Arc<Replay>>,
    pub record: Option<Record>,
    pub history: Option<HistoryPage>,
    pub prepared: Option<Prepared>,
    pub projects: Vec<(ProjectRegistered, bool)>,
    pub inputs: Vec<InputView>,
    pub queued: Option<QueuedPreparation>,
    pub order_revision: Option<u64>,
    pub stale_order: bool,
    pub accepted_input_id: Option<Id>,
    pub project_name: Option<String>,
    pub project_name_revision: Option<u64>,
    pub project_name_changed: Option<bool>,
    pub stale_project_name_revision: bool,
}
#[derive(Clone, Debug)]
pub struct HistoryPage {
    pub project: Id,
    pub conversation: Option<Id>,
    pub generation: Option<u64>,
    pub revision: u64,
    pub has_more: bool,
    pub entries: Vec<HistoryEntry>,
}
#[derive(Clone, Debug)]
pub struct HistoryEntry {
    pub operation: Id,
    pub generation: u64,
    pub accepted_frame: usize,
    pub terminal: Option<TerminalKind>,
}
#[derive(Debug)]
pub struct InputView {
    pub input: InputQueued,
    pub v2: Option<InputQueuedV2>,
    pub order_position: Option<u32>,
    pub status: InputStatus,
    pub operation: Option<(Id, u64)>,
    pub sequence: u64,
}
#[derive(Debug)]
pub struct QueuedPreparation {
    pub request: Id,
    pub project: Id,
    pub conversation: Id,
    pub generation: u64,
    pub prompt: String,
}
struct Job {
    command: Command,
    deadline: Instant,
    reply: SyncSender<Result<Reply>>,
    settlement: JobSettlement,
}
/// A job retains its queue reservation and completion proof through execution or drop.
struct JobSettlement {
    settled: Arc<AtomicBool>,
    count: Arc<AtomicUsize>,
    wake: Option<asura_platform::events::WakeSender>,
}
impl Drop for JobSettlement {
    fn drop(&mut self) {
        self.count.fetch_sub(1, Ordering::AcqRel);
        self.settled.store(true, Ordering::Release);
        if let Some(wake) = &self.wake {
            let _ = wake.notify();
        }
    }
}
pub struct Ticket {
    receiver: Receiver<Result<Reply>>,
    deadline: Instant,
    mutation: bool,
    done: bool,
    settled: Arc<AtomicBool>,
}
impl Ticket {
    /// True only when the accepted job finished or was dropped without execution.
    /// A response deadline or closed result receiver alone cannot establish this.
    pub fn is_settled(&self) -> bool {
        self.settled.load(Ordering::Acquire)
    }
    pub fn deadline(&self) -> Instant {
        self.deadline
    }
    pub fn poll(&mut self) -> Option<Result<Reply>> {
        if self.done {
            return None;
        }
        let result = match self.receiver.try_recv() {
            Ok(result) => Some(result),
            Err(TryRecvError::Disconnected) => Some(Err(if self.mutation {
                Error::OutcomeUnconfirmed
            } else {
                Error::Unavailable
            })),
            Err(TryRecvError::Empty) if Instant::now() >= self.deadline => {
                Some(Err(if self.mutation {
                    Error::OutcomeUnconfirmed
                } else {
                    Error::Deadline
                }))
            }
            Err(TryRecvError::Empty) => None,
        };
        if result.is_some() {
            self.done = true;
        }
        result
    }
}
static CLAIM: AtomicBool = AtomicBool::new(false);
struct Claim;
impl Drop for Claim {
    fn drop(&mut self) {
        CLAIM.store(false, Ordering::Release);
    }
}
pub struct WriterHandle {
    sender: SyncSender<Option<Job>>,
    count: Arc<AtomicUsize>,
    closing: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
    wake: Option<asura_platform::events::WakeSender>,
}
impl WriterHandle {
    pub fn start(runtime: RuntimeDirectory) -> Result<Self> {
        CLAIM
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| Error::Busy)?;
        let claim = Claim;
        let (sender, receiver) = mpsc::sync_channel::<Option<Job>>(8);
        let count = Arc::new(AtomicUsize::new(0));
        let closing = Arc::new(AtomicBool::new(false));
        let stop = closing.clone();
        let handle = thread::Builder::new()
            .name("asura-journal".into())
            .spawn(move || {
                let _claim = claim;
                let mut state = state::State::new(runtime);
                while !stop.load(Ordering::Acquire) {
                    let job = match receiver.recv() {
                        Ok(Some(job)) => job,
                        Ok(None) | Err(_) => break,
                    };
                    let result = if Instant::now() >= job.deadline {
                        Err(Error::Deadline)
                    } else {
                        state.execute(job.command, job.deadline)
                    };
                    let _ = job.reply.try_send(result);
                    drop(job.settlement);
                }
            })
            .map_err(|_| Error::Unavailable)?;
        Ok(Self {
            sender,
            count,
            closing,
            handle: Some(handle),
            wake: None,
        })
    }
    pub fn set_wake(&mut self, wake: asura_platform::events::WakeSender) {
        self.wake = Some(wake);
    }
    pub fn try_submit(&self, command: Command) -> Result<Ticket> {
        self.submit(command, None)
    }
    /// Preserve a caller's earlier operation deadline through queueing and database work.
    /// A caller cannot extend the owner's normal command budget.
    pub fn try_submit_before(&self, command: Command, deadline: Instant) -> Result<Ticket> {
        self.submit(command, Some(deadline))
    }
    fn submit(&self, command: Command, before: Option<Instant>) -> Result<Ticket> {
        if let Command::SensorStore {
            expected_revision,
            state,
        } = &command
        {
            state.validate().map_err(|_| Error::Invalid)?;
            if expected_revision.checked_add(1) != Some(state.revision) {
                return Err(Error::Invalid);
            }
        }
        if self.closing.load(Ordering::Acquire) {
            return Err(Error::Unavailable);
        }
        match &command {
            #[cfg(feature = "embedded-memory")]
            Command::MemoryCreate {
                turn,
                generation,
                ordinal,
                body,
                ..
            } => {
                if *turn == [0; 16]
                    || *generation == 0
                    || !(1..=8).contains(ordinal)
                    || body.is_empty()
                    || body.len() > 16384
                {
                    return Err(Error::Invalid);
                }
            }
            #[cfg(feature = "embedded-memory")]
            Command::MemoryResolveCreate {
                turn,
                generation,
                ordinal,
            } => {
                if *turn == [0; 16] || *generation == 0 || !(1..=8).contains(ordinal) {
                    return Err(Error::Invalid);
                }
            }
            #[cfg(feature = "embedded-memory")]
            Command::MemoryRead { project, query } => {
                if *project == [0; 16] || query.validate().is_err() {
                    return Err(Error::Invalid);
                }
            }
            Command::Enqueue {
                request, prompt, ..
            } if *request == [0; 16] || prompt.is_empty() || prompt.len() > MAX_PROMPT => {
                return Err(Error::Invalid);
            }
            Command::QueueInput {
                request,
                project,
                conversation,
                expected_generation,
                new_conversation,
                prompt,
            } if *request == [0; 16]
                || request_digest(Request::QueueInput {
                    project: *project,
                    conversation: *conversation,
                    expected_generation: *expected_generation,
                    new_conversation: *new_conversation,
                    prompt,
                })
                .is_err() =>
            {
                return Err(Error::Invalid);
            }
            Command::ReorderInput {
                request,
                input,
                after,
                expected_order_revision,
            } if *request == [0; 16]
                || request_digest(Request::ReorderInput {
                    input: *input,
                    after: *after,
                    expected_order_revision: *expected_order_revision,
                })
                .is_err() =>
            {
                return Err(Error::Invalid);
            }
            Command::InputDecision { request, input, .. }
            | Command::PromoteInput { request, input, .. }
                if *request == [0; 16] || *input == [0; 16] =>
            {
                return Err(Error::Invalid);
            }
            Command::Initialize { request } if *request == [0; 16] => return Err(Error::Invalid),
            Command::Register { request, location }
                if *request == [0; 16] || location.len() > 4096 =>
            {
                return Err(Error::Invalid);
            }
            Command::RenameProject {
                request,
                project,
                expected_name_revision,
                name,
            } if *request == [0; 16]
                || request_digest(Request::RenameProject {
                    project: *project,
                    expected_name_revision: *expected_name_revision,
                    name,
                })
                .is_err() =>
            {
                return Err(Error::Invalid);
            }
            Command::Prepare { prompt, .. } if prompt.is_empty() || prompt.len() > MAX_PROMPT => {
                return Err(Error::Invalid);
            }
            Command::Append { record, .. } => {
                encode_frame(
                    &FrameContext {
                        installation_id: [1; 16],
                        transition_id: [2; 16],
                        sequence: 1,
                        prior_digest: [0; 32],
                        expected_revision: 0,
                        owner_generation: 1,
                    },
                    record,
                )
                .map_err(|_| Error::Invalid)?;
            }
            _ => {}
        }

        let now = Instant::now();
        let default_deadline = now
            + Duration::from_secs(match command {
                Command::Initialize { .. } => 10,
                Command::Open => 5,
                _ => 2,
            });
        let deadline = before.map_or(default_deadline, |cap| cap.min(default_deadline));
        if deadline <= now {
            return Err(Error::Deadline);
        }

        let control = matches!(
            &command,
            Command::Append {
                record: Record::TurnTerminal(_)
                    | Record::CancelRequested(_)
                    | Record::OwnerGeneration,
                ..
            }
        );
        let limit = if control { 8 } else { 6 };
        self.count
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                if n < limit { Some(n + 1) } else { None }
            })
            .map_err(|_| Error::Busy)?;
        let mutation = matches!(
            command,
            Command::Open
                | Command::Initialize { .. }
                | Command::Register { .. }
                | Command::RenameProject { .. }
                | Command::Enqueue { .. }
                | Command::QueueInput { .. }
                | Command::ReorderInput { .. }
                | Command::InputDecision { .. }
                | Command::PromoteInput { .. }
                | Command::Append { .. }
                | Command::SensorStore { .. }
        );
        #[cfg(feature = "embedded-memory")]
        let mutation = mutation || matches!(command, Command::MemoryCreate { .. });
        let (reply, receiver) = mpsc::sync_channel(1);
        let settled = Arc::new(AtomicBool::new(false));
        if self
            .sender
            .try_send(Some(Job {
                command,
                deadline,
                reply,
                settlement: JobSettlement {
                    settled: settled.clone(),
                    count: self.count.clone(),
                    wake: self.wake.clone(),
                },
            }))
            .is_err()
        {
            // Failed send drops its job and releases the reservation exactly once.
            return Err(Error::Busy);
        }
        Ok(Ticket {
            receiver,
            deadline,
            mutation,
            done: false,
            settled,
        })
    }
    pub fn close(&self) {
        if !self.closing.swap(true, Ordering::AcqRel) {
            let _ = self.sender.try_send(None);
        }
    }
    pub fn settled(&mut self) -> bool {
        if self.handle.as_ref().is_some_and(|h| h.is_finished()) {
            let _ = self.handle.take().expect("finished").join();
        }
        self.handle.is_none()
    }
}
impl Drop for WriterHandle {
    fn drop(&mut self) {
        self.close();
        if self.handle.as_ref().is_some_and(|h| h.is_finished()) {
            let _ = self.handle.take().expect("finished").join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn queued_writer() -> (WriterHandle, Receiver<Option<Job>>) {
        let (sender, receiver) = mpsc::sync_channel(8);
        (
            WriterHandle {
                sender,
                count: Arc::new(AtomicUsize::new(0)),
                closing: Arc::new(AtomicBool::new(false)),
                handle: None,
                wake: None,
            },
            receiver,
        )
    }
    #[test]
    fn expired_caller_deadline_never_reserves_or_enqueues_work() {
        let (writer, receiver) = queued_writer();
        assert!(matches!(
            writer.try_submit_before(Command::Open, Instant::now()),
            Err(Error::Deadline)
        ));
        assert_eq!(writer.count.load(Ordering::Acquire), 0);
        assert!(matches!(receiver.try_recv(), Err(TryRecvError::Empty)));
    }
    #[test]
    fn caller_deadline_caps_both_ticket_and_job_without_extending_default() {
        let (writer, receiver) = queued_writer();
        let cap = Instant::now() + Duration::from_millis(500);
        let ticket = writer.try_submit_before(Command::Open, cap).unwrap();
        let job = receiver.try_recv().unwrap().unwrap();
        assert_eq!(ticket.deadline(), cap);
        assert_eq!(job.deadline, cap);
        assert!(!ticket.is_settled());
        drop(job);
        assert!(ticket.is_settled());
        for (command, seconds) in [
            (Command::Open, 5),
            (Command::Initialize { request: [1; 16] }, 10),
            (Command::SensorLoad { project: [1; 16] }, 2),
        ] {
            let start = Instant::now();
            let ticket = writer
                .try_submit_before(command, start + Duration::from_secs(60))
                .unwrap();
            let end = Instant::now();
            let job = receiver.try_recv().unwrap().unwrap();
            assert_eq!(ticket.deadline(), job.deadline);
            assert!(job.deadline >= start + Duration::from_secs(seconds));
            assert!(job.deadline <= end + Duration::from_secs(seconds));
            drop(job);
            assert!(ticket.is_settled());
        }
        assert_eq!(writer.count.load(Ordering::Acquire), 0);
    }
    #[test]
    fn expired_mutation_observation_is_uncertain_and_never_becomes_success() {
        let (sender, receiver) = mpsc::sync_channel(1);
        let mut ticket = Ticket {
            receiver,
            deadline: Instant::now(),
            mutation: true,
            done: false,
            settled: Arc::new(AtomicBool::new(false)),
        };
        let started = Instant::now();
        assert!(matches!(
            ticket.poll(),
            Some(Err(Error::OutcomeUnconfirmed))
        ));
        assert!(started.elapsed() < Duration::from_millis(100));
        sender
            .try_send(Ok(Reply {
                #[cfg(feature = "embedded-memory")]
                memory_result: None,
                #[cfg(feature = "embedded-memory")]
                memory_create_result: None,
                sensor_state: None,
                replay: None,
                record: None,
                history: None,
                prepared: None,
                projects: Vec::new(),
                inputs: Vec::new(),
                queued: None,
                order_revision: None,
                stale_order: false,
                accepted_input_id: None,
                project_name: None,
                project_name_revision: None,
                project_name_changed: None,
                stale_project_name_revision: false,
            }))
            .unwrap();
        assert!(ticket.poll().is_none());
    }
    #[test]
    fn timed_out_read_retains_its_slot_until_real_job_settlement() {
        let (reply, receiver) = mpsc::sync_channel(1);
        let settled = Arc::new(AtomicBool::new(false));
        let count = Arc::new(AtomicUsize::new(1));
        let settlement = JobSettlement {
            settled: settled.clone(),
            count: count.clone(),
            wake: None,
        };
        let mut ticket = Ticket {
            receiver,
            deadline: Instant::now(),
            mutation: false,
            done: false,
            settled,
        };
        assert!(matches!(ticket.poll(), Some(Err(Error::Deadline))));
        assert!(!ticket.is_settled());
        assert_eq!(count.load(Ordering::Acquire), 1);
        let (release, released) = mpsc::sync_channel(1);
        let (sent, observed) = mpsc::sync_channel(1);
        let worker = thread::spawn(move || {
            let _settlement = settlement;
            reply.try_send(Err(Error::Unavailable)).unwrap();
            sent.send(()).unwrap();
            released.recv_timeout(Duration::from_secs(2)).unwrap();
        });
        observed.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(
            ticket.poll().is_none(),
            "a late response must not overwrite timeout"
        );
        assert!(
            !ticket.is_settled(),
            "response delivery is not job settlement"
        );
        release.send(()).unwrap();
        worker.join().unwrap();
        assert!(ticket.is_settled());
        assert_eq!(count.load(Ordering::Acquire), 0);
    }
    #[test]
    fn dropped_queued_jobs_and_unwinding_release_exactly_one_reservation() {
        use std::os::fd::AsRawFd;
        for panic in [false, true] {
            let settled = Arc::new(AtomicBool::new(false));
            let count = Arc::new(AtomicUsize::new(1));
            let (reply, receiver) = mpsc::sync_channel(1);
            let (wake_reader, wake_sender) = asura_platform::events::wake_pair().unwrap();
            let job = Job {
                command: Command::Open,
                deadline: Instant::now(),
                reply,
                settlement: JobSettlement {
                    settled: settled.clone(),
                    count: count.clone(),
                    wake: Some(wake_sender),
                },
            };
            if panic {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(move || {
                    let _job = job;
                    panic!("controlled worker unwind");
                }));
                assert!(result.is_err());
            } else {
                let (sender, inbox) = mpsc::sync_channel(1);
                sender.try_send(job).ok().unwrap();
                drop(inbox);
            }
            let ready = asura_platform::poll(
                &[asura_platform::PollInterest {
                    fd: wake_reader.as_raw_fd(),
                    read: true,
                    write: false,
                }],
                Duration::from_secs(1),
            )
            .unwrap();
            assert!(!ready.is_empty(), "settlement must wake its owner");
            assert!(
                settled.load(Ordering::Acquire),
                "token must precede its wake"
            );
            assert_eq!(count.load(Ordering::Acquire), 0);
            assert!(matches!(
                receiver.try_recv(),
                Err(TryRecvError::Disconnected)
            ));
        }
    }
    #[test]
    fn failed_queue_send_releases_reservation_without_an_execution() {
        let settled = Arc::new(AtomicBool::new(false));
        let count = Arc::new(AtomicUsize::new(1));
        let (reply, _) = mpsc::sync_channel(1);
        let job = Job {
            command: Command::Open,
            deadline: Instant::now(),
            reply,
            settlement: JobSettlement {
                settled: settled.clone(),
                count: count.clone(),
                wake: None,
            },
        };
        let (sender, receiver) = mpsc::sync_channel::<Job>(1);
        drop(receiver);
        assert!(sender.try_send(job).is_err());
        assert!(settled.load(Ordering::Acquire));
        assert_eq!(count.load(Ordering::Acquire), 0);
    }
    #[test]
    fn closed_completion_channel_is_not_commit_acknowledgement() {
        let (sender, receiver) = mpsc::sync_channel(1);
        drop(sender);
        let mut ticket = Ticket {
            receiver,
            deadline: Instant::now() + Duration::from_secs(2),
            mutation: true,
            done: false,
            settled: Arc::new(AtomicBool::new(false)),
        };
        assert!(matches!(
            ticket.poll(),
            Some(Err(Error::OutcomeUnconfirmed))
        ));
    }
}
