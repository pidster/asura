//! Serialized, pollable journal ownership. All filesystem work stays on its worker.
use super::conversation::*;
pub use super::journal::{HoldReason, JournalStatus};
use asura_platform::{ProjectIdentity, RuntimeDirectory};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
    mpsc::{self, Receiver, SyncSender, TryRecvError},
};
use std::thread::{self, JoinHandle};
mod state;
#[cfg(feature = "test-support")]
pub mod test_support;
/// Journal owner allocation ceiling; embedded engine resources are separate.
pub const MAX_JOURNAL_OWNER_MEMORY: usize = 64 * 1024 * 1024;
// Retired/current/proposed replay + Prepare history + queue + frame, checkpoint
// and segment buffers + merge pages + project and lookup pages (segmented journal).
const _: () = assert!(
    3 * 16 * 1024 * 1024 + 8 * 1024 * 1024 + 8 * 65536 + 192 * 1024 + 36 * 1024 + 64 * 1024
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
    Cancelled,
    Conflict,
    Invalid,
    StaleProject,
    /// The project equals, lies inside, or contains the Asura home.
    OverlapsAsuraState,
    ModelUnavailable,
    Limit,
    /// Normal records are held at the configured journal quota.
    JournalQuota,
    /// Normal records are held at the disk floor.
    JournalDiskSpace,
}
pub type Result<T> = std::result::Result<T, Error>;
#[derive(Debug)]
pub enum Command {
    #[cfg(feature = "embedded-memory")]
    /// Put one note for a committed `run` create_note kind 14. The body and
    /// optional source version come from the service's parse; the identities
    /// are derived from the call (HM3, TCR-D2).
    MemoryCreate {
        cancel: Arc<AtomicBool>,
        turn: Id,
        generation: u64,
        ordinal: u32,
        body: String,
        source: Option<Id>,
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
    /// One page of projects: ascending after `after`, or descending below `before`.
    Projects {
        after: Option<Id>,
        before: Option<Id>,
        limit: usize,
    },
    /// One exact project, or none.
    Project {
        project: Id,
    },
    /// One newest-first page of a project's conversations.
    Conversations {
        project: Id,
        before: Option<u64>,
        limit: usize,
    },
    /// The `O` facts of any operation.
    Lookup {
        operation: Id,
    },
    /// Journal facts of at most 128 retained payloads.
    PayloadFacts {
        payloads: Vec<Id>,
    },
    /// Usage, quota and hold state; also rechecks free space.
    JournalStatus,
    /// A successful `journal.quotaBytes` set applies at once.
    SetQuota {
        bytes: u64,
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
        locator: Locator,
    },
    /// One window of a terminal operation's response: the kind 8 record with
    /// `text` replaced by at most `MAX_TEXT` bytes starting at the largest
    /// character boundary not after `offset` (response-text-transfer.md).
    ReadResponse {
        operation: Id,
        offset: u64,
        /// Thinking bytes the reader holds; the thinking window comes first
        /// and shares the window budget (thinking-level.md).
        thinking_offset: u64,
    },
    History {
        project: Id,
        conversation: Option<Id>,
        /// The accepted frame sequence of the oldest entry already shown.
        before_accepted_frame: Option<u64>,
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
    /// The configured thinking level code (thinking-level.md).
    pub thinking: u8,
    pub configuration_digest: Hash,
    /// Completed prior turns of the conversation, newest first, until 8 MiB
    /// of text (SJ-D10).
    pub history: Vec<(TurnAccepted, TurnTerminal)>,
    /// Older completed turns not returned.
    pub omitted: u32,
}
/// One project as a page lists it.
#[derive(Clone, Debug)]
pub struct ProjectListing {
    pub registration: ProjectRegistered,
    pub name: String,
    pub name_revision: u64,
    /// The open failure; `None` means current.
    pub error: Option<Error>,
}
/// The facts of one operation for observation and cancel.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct OperationView {
    pub conversation: Id,
    pub generation: u64,
    pub terminal: Option<(TerminalKind, Cause)>,
    pub cancel: Option<Cause>,
}
/// Kind 23 facts as payload recovery uses them: state, sequence, and stored
/// bytes and digest per stream.
pub type Settlement = (CaptureState, u64, [(u64, Option<Hash>); 2]);
/// Journal facts of one retained exec_cmd payload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PayloadFact {
    pub payload: Id,
    pub call: Id,
    pub operation: Id,
    pub payloads: [Id; 2],
    pub intent_sequence: u64,
    pub settled: Option<Settlement>,
    /// The call belongs to the active operation.
    pub active: bool,
}
#[derive(Debug, Default)]
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
    /// One page of projects.
    pub projects: Vec<ProjectListing>,
    /// Another page exists in the same order.
    pub more: bool,
    /// The registered or renamed project.
    pub project: Option<ProjectListing>,
    /// One page of a project's conversations with their creation sequence.
    pub conversations: Vec<(Id, u64)>,
    pub operation: Option<OperationView>,
    /// The operation's tool calls, for `ReadResponse`.
    pub tools: Option<Vec<ToolRecord>>,
    pub payload_facts: Vec<PayloadFact>,
    pub status: Option<JournalStatus>,
    pub inputs: Vec<InputView>,
    pub queued: Option<QueuedPreparation>,
    pub order_revision: Option<u64>,
    pub stale_order: bool,
    pub accepted_input_id: Option<Id>,
    pub project_name: Option<String>,
    pub project_name_revision: Option<u64>,
    pub project_name_changed: Option<bool>,
    pub stale_project_name_revision: bool,
    /// The whole response length for `ReadResponse`.
    pub response_bytes: Option<u64>,
    /// The turn's thinking level, request and one thinking window, for
    /// `ReadResponse`.
    pub thinking: Option<ThinkingWindow>,
}
/// One window of a terminal operation's thinking text (thinking-level.md).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ThinkingWindow {
    /// Kind 21 thinking level code.
    pub level: u8,
    /// Kind 21 thinking request code.
    pub request: u8,
    /// Start of `text` in the thinking text.
    pub offset: u64,
    pub text: String,
    /// The whole thinking length.
    pub bytes: u64,
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
    /// The accepted frame's sequence (SJ-D14).
    pub accepted_frame: u64,
    pub terminal: Option<TerminalKind>,
}
#[derive(Debug)]
pub struct InputView {
    /// The durable kind 18 record; the prompt is a bounded excerpt unless full.
    pub input: InputQueued,
    /// Queue, or Steer after a durable promotion.
    pub kind: InputKind,
    /// The promotion target; present only for Steer.
    pub target_operation: Option<Id>,
    /// Expected generation, or the promotion target generation for Steer.
    pub target_generation: u64,
    pub order_position: u32,
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
/// One accepted writer request. Observation has no time bound (J1-0): only the
/// worker's reply or its disconnection settles the ticket. A caller that stops
/// waiting, for example after a turn cancel, may keep or drop the ticket; the
/// job still runs in order and its reply is available when it arrives.
pub struct Ticket {
    receiver: Receiver<Result<Reply>>,
    mutation: bool,
    done: bool,
    settled: Arc<AtomicBool>,
}
impl Ticket {
    /// Blocks until the reply or the worker's disconnection. Only a
    /// retained worker thread calls this, never the reactor; the writer's
    /// close disconnects it.
    pub fn wait(mut self) -> Result<Reply> {
        let result = match self.receiver.recv() {
            Ok(result) => result,
            Err(_) => Err(if self.mutation {
                Error::OutcomeUnconfirmed
            } else {
                Error::Unavailable
            }),
        };
        self.done = true;
        result
    }
    /// True only when the accepted job finished or was dropped without execution.
    /// A closed result receiver alone cannot establish this.
    pub fn is_settled(&self) -> bool {
        self.settled.load(Ordering::Acquire)
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
/// A submitter of journal requests that a retained worker thread (payload
/// recovery) may hold; it shares the 8-slot queue and its admission rules.
#[derive(Clone)]
pub struct WriterClient {
    sender: SyncSender<Option<Job>>,
    count: Arc<AtomicUsize>,
    closing: Arc<AtomicBool>,
    wake: Option<asura_platform::events::WakeSender>,
}
pub struct WriterHandle {
    client: WriterClient,
    handle: Option<JoinHandle<()>>,
    status: Arc<std::sync::Mutex<JournalStatus>>,
    wake_slot: Arc<std::sync::Mutex<Option<asura_platform::events::WakeSender>>>,
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
        let status = Arc::new(std::sync::Mutex::new(JournalStatus::default()));
        let wake_slot: Arc<std::sync::Mutex<Option<asura_platform::events::WakeSender>>> =
            Arc::default();
        let (shared, slot) = (status.clone(), wake_slot.clone());
        let handle = thread::Builder::new()
            .name("asura-journal".into())
            .spawn(move || {
                let _claim = claim;
                let mut state = state::State::new(runtime, stop.clone(), shared);
                let notify = || {
                    if let Some(wake) = slot.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
                        let _ = wake.notify();
                    }
                };
                run_worker(&receiver, &stop, &mut state, notify);
            })
            .map_err(|_| Error::Unavailable)?;
        Ok(Self {
            client: WriterClient {
                sender,
                count,
                closing,
                wake: None,
            },
            handle: Some(handle),
            status,
            wake_slot,
        })
    }
    pub fn set_wake(&mut self, wake: asura_platform::events::WakeSender) {
        *self.wake_slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(wake.clone());
        self.client.wake = Some(wake);
    }
    /// A submitter for a retained worker thread.
    pub fn client(&self) -> WriterClient {
        self.client.clone()
    }
    /// Admits one request to the 8-slot queue or returns `Busy`.
    pub fn try_submit(&self, command: Command) -> Result<Ticket> {
        self.client.try_submit(command)
    }
    /// Journal usage and the hold state, as the worker last published them.
    /// Reading it never waits for the worker.
    pub fn status(&self) -> JournalStatus {
        *self.status.lock().unwrap_or_else(|e| e.into_inner())
    }
}
impl WriterClient {
    /// Admits one request to the 8-slot queue or returns `Busy`. The writer's work
    /// has no time bound; a caller's own deadline applies only to its own wait.
    pub fn try_submit(&self, command: Command) -> Result<Ticket> {
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
                source,
                ..
            } => {
                if *turn == [0; 16]
                    || *generation == 0
                    || !(1..=MAX_RECORDED_CALLS).contains(ordinal)
                    || body.is_empty()
                    || body.len() > 16384
                    || source.is_some_and(|source| source == [0; 16])
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
                if *turn == [0; 16]
                    || *generation == 0
                    || !(1..=MAX_RECORDED_CALLS).contains(ordinal)
                {
                    return Err(Error::Invalid);
                }
            }
            #[cfg(feature = "embedded-memory")]
            Command::MemoryRead { project, query } => {
                if *project == [0; 16] || query.validate().is_err() {
                    return Err(Error::Invalid);
                }
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

        let control = matches!(
            &command,
            Command::Append {
                record: Record::TurnTerminal(_)
                    | Record::CancelRequested(_)
                    | Record::OwnerGeneration,
                ..
            }
        );
        if let Command::PayloadFacts { payloads } = &command
            && payloads.len() > 128
        {
            return Err(Error::Invalid);
        }
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
            mutation,
            done: false,
            settled,
        })
    }
}
impl WriterHandle {
    pub fn close(&self) {
        if !self.client.closing.swap(true, Ordering::AcqRel) {
            let _ = self.client.sender.try_send(None);
        }
    }
    pub fn settled(&mut self) -> bool {
        if self.handle.as_ref().is_some_and(|h| h.is_finished()) {
            let _ = self.handle.take().expect("finished").join();
        }
        self.handle.is_none()
    }
}
/// Longest gap between free-space rechecks while held (owner, 2026-10-19).
const RECHECK_CAP_SECS: u64 = 60;
/// The next recheck gaps: each the sum of the two before, at most
/// `RECHECK_CAP_SECS`.
fn next_pacing((a, b): (u64, u64)) -> (u64, u64) {
    (b, a.saturating_add(b).min(RECHECK_CAP_SECS))
}
/// The journal worker loop: queued commands in order; between commands, run
/// compaction steps, and while normal records are held, recheck free space
/// after 1, 1, 2, 3, 5, 8 seconds and so on, at most 60 seconds apart. The
/// recheck is pacing: it never ends or refuses work. Close ends the loop.
fn run_worker(
    receiver: &Receiver<Option<Job>>,
    stop: &AtomicBool,
    state: &mut state::State,
    notify: impl Fn(),
) {
    use std::sync::mpsc::RecvTimeoutError;
    use std::time::Duration;
    let mut pacing = (1u64, 1u64);
    state.publish_status();
    while !stop.load(Ordering::Acquire) {
        let job = match receiver.try_recv() {
            Ok(job) => Some(job),
            Err(TryRecvError::Disconnected) => break,
            Err(TryRecvError::Empty) => {
                if state.merge_pending() && state.idle() {
                    if state.publish_status() {
                        notify();
                    }
                    continue;
                }
                if state.held() {
                    match receiver.recv_timeout(Duration::from_secs(pacing.0)) {
                        Ok(job) => Some(job),
                        Err(RecvTimeoutError::Disconnected) => break,
                        Err(RecvTimeoutError::Timeout) => {
                            pacing = next_pacing(pacing);
                            state.recheck();
                            if state.publish_status() {
                                notify();
                            }
                            continue;
                        }
                    }
                } else {
                    match receiver.recv() {
                        Ok(job) => Some(job),
                        Err(_) => break,
                    }
                }
            }
        };
        let Some(Some(job)) = job else {
            break;
        };
        let result = state.execute(job.command);
        let changed = state.publish_status();
        if !state.held() {
            pacing = (1, 1);
        }
        let _ = job.reply.try_send(result);
        drop(job.settlement);
        if changed {
            notify();
        }
    }
    state.shutdown();
}
/// Runs queued jobs in order until close. No job is dropped for age (J1-0).
#[cfg(test)]
fn run_jobs(
    receiver: &Receiver<Option<Job>>,
    stop: &AtomicBool,
    mut execute: impl FnMut(Command) -> Result<Reply>,
) {
    while !stop.load(Ordering::Acquire) {
        let job = match receiver.recv() {
            Ok(Some(job)) => job,
            Ok(None) | Err(_) => break,
        };
        let result = execute(job.command);
        let _ = job.reply.try_send(result);
        drop(job.settlement);
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
    use std::time::{Duration, Instant};
    /// SJ-14: the held recheck gaps grow as 1, 1, 2, 3, 5 ... and stop at 60 s.
    #[test]
    fn held_recheck_pacing_is_capped() {
        let mut pacing = (1, 1);
        let mut gaps = Vec::new();
        for _ in 0..14 {
            gaps.push(pacing.0);
            pacing = next_pacing(pacing);
        }
        assert_eq!(gaps, [1, 1, 2, 3, 5, 8, 13, 21, 34, 55, 60, 60, 60, 60]);
    }
    fn queued_writer() -> (WriterHandle, Receiver<Option<Job>>) {
        let (sender, receiver) = mpsc::sync_channel(8);
        (
            WriterHandle {
                client: WriterClient {
                    sender,
                    count: Arc::new(AtomicUsize::new(0)),
                    closing: Arc::new(AtomicBool::new(false)),
                    wake: None,
                },
                handle: None,
                status: Arc::default(),
                wake_slot: Arc::default(),
            },
            receiver,
        )
    }
    fn empty_reply() -> Reply {
        Reply::default()
    }
    fn control() -> Command {
        Command::Append {
            expected_revision: 1,
            record: Record::OwnerGeneration,
            admission: None,
        }
    }
    #[test]
    fn j1_0_06_ninth_writer_request_is_busy_and_six_ordinary_slots_remain() {
        let (writer, receiver) = queued_writer();
        let ordinary: Vec<_> = (0..6)
            .map(|_| writer.try_submit(Command::Open).unwrap())
            .collect();
        assert!(matches!(writer.try_submit(Command::Open), Err(Error::Busy)));
        let controls: Vec<_> = (0..2)
            .map(|_| writer.try_submit(control()).unwrap())
            .collect();
        assert!(matches!(writer.try_submit(control()), Err(Error::Busy)));
        assert_eq!(writer.client.count.load(Ordering::Acquire), 8);
        drop((ordinary, controls));
        // Queued jobs keep their slots until the worker settles them.
        assert_eq!(writer.client.count.load(Ordering::Acquire), 8);
        drop(receiver);
        assert_eq!(writer.client.count.load(Ordering::Acquire), 0);
    }
    #[test]
    fn j1_0_02_ticket_without_reply_never_reports_uncertainty_and_late_reply_applies() {
        let (sender, receiver) = mpsc::sync_channel(1);
        let mut ticket = Ticket {
            receiver,
            mutation: true,
            done: false,
            settled: Arc::new(AtomicBool::new(false)),
        };
        // Past the removed 10 s initialization budget, the ticket still waits.
        let end = Instant::now() + Duration::from_millis(10_100);
        while Instant::now() < end {
            assert!(ticket.poll().is_none());
            std::thread::sleep(Duration::from_millis(50));
        }
        // The caller stopped caring (a turn cancel) but kept the ticket: the
        // late reply is still delivered and applied.
        sender.try_send(Ok(empty_reply())).unwrap();
        assert!(matches!(ticket.poll(), Some(Ok(_))));
        assert!(ticket.poll().is_none());
    }
    #[test]
    fn j1_0_03_job_queued_behind_a_gated_job_runs_after_release_in_order() {
        let (writer, receiver) = queued_writer();
        let mut tickets: Vec<_> = [
            Command::Open,
            control(),
            Command::Projects {
                after: None,
                before: None,
                limit: 1,
            },
        ]
        .into_iter()
        .map(|command| writer.try_submit(command).unwrap())
        .collect();
        let (release, gate) = mpsc::channel::<()>();
        let (order_sender, order) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let worker = {
            let stop = stop.clone();
            thread::spawn(move || {
                run_jobs(&receiver, &stop, |command| {
                    let name = match command {
                        Command::Open => "open",
                        Command::Append { .. } => "append",
                        _ => "projects",
                    };
                    if name == "open" {
                        gate.recv().unwrap();
                    }
                    order_sender.send(name).unwrap();
                    Ok(empty_reply())
                })
            })
        };
        // Stall the first job past every removed default deadline.
        thread::sleep(Duration::from_millis(2100));
        assert!(tickets.iter_mut().all(|ticket| ticket.poll().is_none()));
        release.send(()).unwrap();
        let ran: Vec<_> = (0..3)
            .map(|_| order.recv_timeout(Duration::from_secs(5)).unwrap())
            .collect();
        assert_eq!(ran, ["open", "append", "projects"]);
        for ticket in &mut tickets {
            let end = Instant::now() + Duration::from_secs(5);
            loop {
                if let Some(result) = ticket.poll() {
                    assert!(result.is_ok());
                    break;
                }
                assert!(Instant::now() < end);
                thread::yield_now();
            }
        }
        stop.store(true, Ordering::Release);
        writer.close();
        worker.join().unwrap();
    }
    #[test]
    fn response_delivery_is_not_job_settlement() {
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
            mutation: false,
            done: false,
            settled,
        };
        assert!(ticket.poll().is_none());
        let (release, released) = mpsc::sync_channel(1);
        let (sent, observed) = mpsc::sync_channel(1);
        let worker = thread::spawn(move || {
            let _settlement = settlement;
            reply.try_send(Err(Error::Unavailable)).unwrap();
            sent.send(()).unwrap();
            released.recv().unwrap();
        });
        observed.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(matches!(ticket.poll(), Some(Err(Error::Unavailable))));
        assert!(
            !ticket.is_settled(),
            "response delivery is not job settlement"
        );
        assert_eq!(count.load(Ordering::Acquire), 1);
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
