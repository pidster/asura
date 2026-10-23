//! One retained capture worker per exec_cmd job slot (J2, SL3). The store keys its reservation and staging by the job ID.
//!
//! The reactor side ([`Sink`]) receives raw pipe bytes from the platform
//! `ExecCmdJob` and moves them to the worker through a bounded transfer queue
//! of 8 chunks of at most 64 KiB and 512 KiB in total, counting the bytes
//! that wait on the reactor to form a chunk. It never blocks: `try_send`
//! only. The worker scans each chunk, stages the redacted bytes, and hashes
//! them. Raw bytes live only in zeroizing
//! buffers and the scanner's carry; nothing raw reaches staging, records,
//! logs or the model.
use asura_platform::events::WakeSender;
use asura_platform::exec_cmd::{OutputSink, READ_BUFFER};
use asura_platform::pacing::Fibonacci;
use asura_platform::secrets::{
    JobRedaction, JobSummary, LossSpan, NumberOrigin, Redacted, Stream, StreamStats,
};
use asura_storage::authority::conversation::{
    CaptureNumber, CaptureReason, CaptureState, CaptureStream, Hash, Id, JobStream,
};
use asura_storage::payload::{
    Capture, PayloadError, PayloadStore, RECOVERY_RESERVE, RESERVATION_STEP, Short, SpaceListener,
    StreamIndex, Victim,
};
use std::collections::VecDeque;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TryRecvError, TrySendError, sync_channel};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use zeroize::Zeroizing;

/// Transfer queue limits (R4).
pub(crate) const QUEUE_CHUNKS: usize = 8;
pub(crate) const CHUNK_BYTES: usize = 64 * 1024;
pub(crate) const QUEUE_BYTES: usize = 512 * 1024;
/// Messages from one worker to the reactor.
pub(crate) const MESSAGES: usize = 8;

fn index(stream: Stream) -> usize {
    match stream {
        Stream::Stdout => 0,
        Stream::Stderr => 1,
    }
}
fn store_index(stream: Stream) -> StreamIndex {
    match stream {
        Stream::Stdout => StreamIndex::Stdout,
        Stream::Stderr => StreamIndex::Stderr,
    }
}
const STREAMS: [Stream; 2] = [Stream::Stdout, Stream::Stderr];

enum Input {
    Chunk(Stream, Zeroizing<Vec<u8>>),
    /// EOF of a stream, with the raw bytes discarded after a cancel trigger.
    End(Stream, u64),
}
impl Input {
    fn len(&self) -> usize {
        match self {
            Self::Chunk(_, bytes) => bytes.len(),
            Self::End(..) => 0,
        }
    }
}

/// Why a capture paused.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PauseReason {
    Quota,
    Enospc,
}
impl PauseReason {
    fn reason(self) -> CaptureReason {
        match self {
            Self::Quota => CaptureReason::Quota,
            Self::Enospc => CaptureReason::Enospc,
        }
    }
}

/// Settlement facts of one stream.
#[derive(Clone, Debug)]
pub(crate) struct StreamReport {
    pub stored: u64,
    pub digest: Option<Hash>,
    pub stats: StreamStats,
    pub discarded: u64,
    pub eof: bool,
    /// Newline bytes (0x0A) in the stored, redacted bytes (kind 28, TJ-20).
    pub lines: u64,
}
/// The sealed report: everything kind 28 needs. It
/// holds no value: only redacted bytes, counts and digests of stored bytes.
#[derive(Clone, Debug)]
pub(crate) struct Report {
    pub state: CaptureState,
    pub reason: CaptureReason,
    pub streams: [StreamReport; 2],
    pub distinct_numbers: u32,
    pub numbers: Vec<CaptureNumber>,
    pub pause_count: u32,
    pub first_pause_ms: u64,
    pub paused_ms: u64,
    pub final_reservation: u64,
}
impl Report {
    pub(crate) fn stream(&self, stream: usize) -> CaptureStream {
        let s = &self.streams[stream];
        let spans: Vec<LossSpan> = s
            .stats
            .loss_spans
            .iter()
            .map(|span| LossSpan {
                // A write fault can leave fewer stored bytes than released.
                stored_offset: span.stored_offset.min(s.stored),
                ..*span
            })
            .collect();
        CaptureStream {
            observed_bytes: s.stats.observed_bytes,
            stored_bytes: s.stored,
            digest: s.digest,
            replacements: s.stats.replacements,
            unnumbered_replacements: s.stats.unnumbered_replacements,
            literal_replacements: s.stats.literal_replacements,
            kinds: s.stats.kind_counts().collect(),
            loss_span_count: s.stats.loss_span_count,
            withheld_bytes: s.stats.withheld_bytes,
            spans,
            degraded: s.stats.degraded,
            discarded_bytes: s.discarded,
            eof: s.eof,
        }
    }
    /// The kind 28 stream block: the capture block and the line count.
    pub(crate) fn job_stream(&self, stream: usize) -> JobStream {
        JobStream {
            capture: self.stream(stream),
            lines: self.streams[stream].lines,
        }
    }
}

/// The stream blocks of an `unconfirmed` capture: raw bytes accepted and
/// discarded by the sink and EOF per stream; nothing stored is claimed.
pub(crate) fn unconfirmed_streams(sink: &SinkHandle) -> [CaptureStream; 2] {
    let (accepted, discarded, ended) = sink.counts();
    std::array::from_fn(|i| CaptureStream {
        observed_bytes: accepted[i],
        discarded_bytes: discarded[i],
        eof: ended[i],
        ..CaptureStream::default()
    })
}
/// Newline bytes (0x0A) in stored bytes.
fn newlines(bytes: &[u8]) -> u64 {
    bytes.iter().filter(|b| **b == b'\n').count() as u64
}

/// Worker to reactor messages (bounded channel of 8 plus a wake).
#[derive(Debug)]
pub(crate) enum Message {
    Room,
    Paused(PauseReason),
    Resumed,
    Fault(CaptureReason),
    Sealed(Box<Report>),
    Installed,
    InstallPending,
    Abandoned,
}

#[derive(Default)]
struct Control {
    cancel: bool,
    abandon: bool,
    install: bool,
    /// J2: no process ran; remove staging instead of installing.
    discard: bool,
    /// J2 `Draining`: the leader exited, so capture never pauses again; a
    /// short is a capture fault with reason `quota` or `enospc`.
    draining: bool,
    /// J2: a cancel trigger in `Draining`; later chunks are discarded and
    /// counted.
    drop_output: bool,
    /// OD2: the slot closed the capture read ends of a released `!!` job
    /// (`SlotPort::close_capture`). The producer ends without EOF; the
    /// seal is `incomplete` with reason 7 `closed_after_cancel`.
    closed: bool,
    space: u64,
}
/// State shared by the reactor side and the worker.
struct Shared {
    queued_bytes: AtomicUsize,
    queued_chunks: AtomicUsize,
    backpressured: AtomicBool,
    /// The stored signal (TJ-Q10): set after a write that stored at least
    /// one byte, cleared by [`Handle::take_stored`]. At most one signal is
    /// outstanding per worker.
    stored: AtomicBool,
    control: Mutex<Control>,
    changed: Condvar,
}
impl Shared {
    /// The stored signal (TJ-Q10), after a staging write that has already
    /// raised the watermark. A write of no byte signals nothing. The reactor
    /// wakes only when no signal was outstanding; the signal ends nothing.
    /// Returns whether it woke the reactor.
    fn signal_stored(&self, written: usize, wake: Option<&WakeSender>) -> bool {
        if written == 0 || self.stored.swap(true, Ordering::AcqRel) {
            return false;
        }
        if let Some(wake) = wake {
            let _ = wake.notify();
        }
        true
    }
    fn control(&self) -> std::sync::MutexGuard<'_, Control> {
        self.control.lock().unwrap_or_else(|e| e.into_inner())
    }
    fn set(&self, update: impl FnOnce(&mut Control)) {
        update(&mut self.control());
        self.changed.notify_all();
    }
}
impl SpaceListener for Shared {
    fn space_event(&self) {
        self.set(|c| c.space = c.space.wrapping_add(1));
    }
}

/// The reactor side of the sink: bytes that wait to form a chunk, and the
/// counts the unconfirmed settlement reports.
struct SinkState {
    tx: Option<SyncSender<Input>>,
    pending: VecDeque<Input>,
    pending_bytes: usize,
    accepted: [u64; 2],
    discarded: [u64; 2],
    ended: [bool; 2],
}
/// The reactor-side handle of one capture's sink. Both the boxed sink given
/// to `ExecCmdJob` and the exec_cmd worker share it, on the reactor thread only,
/// so pending bytes keep moving after the job settles.
#[derive(Clone)]
pub(crate) struct SinkHandle {
    state: Arc<Mutex<SinkState>>,
    shared: Arc<Shared>,
}
impl SinkHandle {
    fn lock(&self) -> std::sync::MutexGuard<'_, SinkState> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
    /// Moves waiting bytes into the transfer queue; never blocks.
    pub(crate) fn flush(&self) {
        let mut state = self.lock();
        let SinkState {
            tx,
            pending,
            pending_bytes,
            ..
        } = &mut *state;
        let Some(sender) = tx.as_ref() else {
            pending.clear();
            *pending_bytes = 0;
            return;
        };
        while let Some(input) = pending.pop_front() {
            let len = input.len();
            if len > 0 {
                self.shared.queued_bytes.fetch_add(len, Ordering::AcqRel);
                self.shared.queued_chunks.fetch_add(1, Ordering::AcqRel);
            }
            match sender.try_send(input) {
                Ok(()) => *pending_bytes -= len,
                Err(TrySendError::Full(input)) => {
                    if len > 0 {
                        self.shared.queued_bytes.fetch_sub(len, Ordering::AcqRel);
                        self.shared.queued_chunks.fetch_sub(1, Ordering::AcqRel);
                    }
                    self.shared.backpressured.store(true, Ordering::Release);
                    pending.push_front(input);
                    break;
                }
                Err(TrySendError::Disconnected(_)) => {
                    // The worker has exited: dropped buffers zeroize.
                    if len > 0 {
                        self.shared.queued_bytes.fetch_sub(len, Ordering::AcqRel);
                        self.shared.queued_chunks.fetch_sub(1, Ordering::AcqRel);
                    }
                    pending.clear();
                    *pending_bytes = 0;
                    *tx = None;
                    break;
                }
            }
        }
    }
    /// Bytes still waiting on the reactor.
    pub(crate) fn pending(&self) -> bool {
        !self.lock().pending.is_empty()
    }
    /// Raw bytes accepted, raw bytes discarded and EOF per stream.
    pub(crate) fn counts(&self) -> ([u64; 2], [u64; 2], [bool; 2]) {
        let state = self.lock();
        (state.accepted, state.discarded, state.ended)
    }
    /// Drops the sender, so the worker sees the producer end once the queue
    /// is drained.
    pub(crate) fn close(&self) {
        self.lock().tx = None;
    }
    fn room(&self) -> usize {
        let pending = self.lock().pending_bytes;
        let room = if self.shared.queued_chunks.load(Ordering::Acquire) >= QUEUE_CHUNKS {
            0
        } else {
            QUEUE_BYTES
                .saturating_sub(self.shared.queued_bytes.load(Ordering::Acquire))
                .saturating_sub(pending)
        };
        if room < READ_BUFFER {
            self.shared.backpressured.store(true, Ordering::Release);
        }
        room
    }
}
struct Sink(SinkHandle);
impl OutputSink for Sink {
    fn room(&self) -> usize {
        self.0.room()
    }
    fn accept(&mut self, stream: Stream, bytes: &[u8]) {
        {
            let mut state = self.0.lock();
            state.accepted[index(stream)] += bytes.len() as u64;
            state.pending_bytes += bytes.len();
            let merge = matches!(
                state.pending.back(),
                Some(Input::Chunk(last, chunk)) if *last == stream && chunk.len() + bytes.len() <= CHUNK_BYTES
            );
            if merge {
                if let Some(Input::Chunk(_, chunk)) = state.pending.back_mut() {
                    chunk.extend_from_slice(bytes);
                }
            } else {
                let mut chunk = Zeroizing::new(Vec::with_capacity(bytes.len().max(READ_BUFFER)));
                chunk.extend_from_slice(bytes);
                state.pending.push_back(Input::Chunk(stream, chunk));
            }
        }
        self.0.flush();
    }
    fn discard(&mut self, stream: Stream, bytes: u64) {
        self.0.lock().discarded[index(stream)] += bytes;
    }
    fn end(&mut self, stream: Stream) {
        {
            let mut state = self.0.lock();
            state.ended[index(stream)] = true;
            let discarded = state.discarded[index(stream)];
            state.pending.push_back(Input::End(stream, discarded));
        }
        self.0.flush();
    }
    fn flush(&mut self) {
        self.0.flush();
    }
}

/// What the worker needs: its store, its job, victims to delete before it
/// creates staging, and the per-job redaction holding the command literals.
pub(crate) struct Start {
    pub store: Arc<PayloadStore>,
    /// The job ID: the store keys the reservation and staging by it and by
    /// its derived payload IDs (J2-E6).
    pub job: Id,
    pub victims: Vec<Victim>,
    pub redaction: JobRedaction,
    pub wake: Option<WakeSender>,
    /// Test-support gates read files in this scratch home.
    pub gate_home: Option<std::path::PathBuf>,
}

/// The reactor's handle of one capture worker.
pub(crate) struct Handle {
    pub job: Id,
    shared: Arc<Shared>,
    messages: Receiver<Message>,
    thread: JoinHandle<()>,
    pub sink: SinkHandle,
    pub exited: bool,
    pub install_pending: bool,
}
impl Handle {
    /// A cancel trigger: a paused worker discards queued and later bytes.
    pub(crate) fn cancel(&self) {
        self.shared.set(|c| c.cancel = true);
    }
    /// After `unconfirmed`: when its blocked call returns, the worker stops,
    /// keeps its staging as an unverified prefix and exits.
    pub(crate) fn abandon(&self) {
        self.shared.set(|c| {
            c.cancel = true;
            c.abandon = true;
        });
    }
    /// After kind 28 commits: install both payloads
    /// and exit.
    pub(crate) fn install(&self) {
        self.shared.set(|c| c.install = true);
    }
    /// J2: the job settles `not_started`. After sealing, the worker removes
    /// its staging files instead of installing them, and exits.
    pub(crate) fn discard(&self) {
        self.shared.set(|c| c.discard = true);
    }
    /// J2: the leader exited (`Draining`). A paused worker retries at once;
    /// from now on a quota or space short does not pause: the worker sends
    /// `Fault(Quota)` or `Fault(Enospc)` and discards later bytes.
    pub(crate) fn leader_exited(&self) {
        self.shared.set(|c| c.draining = true);
    }
    /// J2: a cancel trigger in `Draining` (TJ-S1): the worker discards and
    /// counts every chunk it has not yet staged; staged bytes are kept.
    pub(crate) fn discard_output(&self) {
        self.shared.set(|c| {
            c.cancel = true;
            c.drop_output = true;
        });
    }
    /// OD2: the slot closed the job's capture read ends after a trigger on
    /// a released `!!` job. The reactor still moves the bytes it already
    /// read, then ends the producer; the worker stages them and seals
    /// `incomplete` with reason 7 `closed_after_cancel`, also when no byte
    /// ever arrived (a process ran, so it is never `not_started`).
    pub(crate) fn close_output(&self) {
        self.shared.set(|c| c.closed = true);
    }
    /// Clears the stored signal (TJ-Q10) and returns whether one was
    /// outstanding. The slot calls it at the start of every step, before the
    /// controller reads the ledger, so stored bytes written after the clear
    /// signal again.
    pub(crate) fn take_stored(&self) -> bool {
        self.shared.stored.swap(false, Ordering::AcqRel)
    }
    /// Whether a stored signal is outstanding, without clearing it.
    #[cfg(test)]
    pub(crate) fn stored_pending(&self) -> bool {
        self.shared.stored.load(Ordering::Acquire)
    }
    /// Drains messages without blocking. The thread's own exit also counts.
    pub(crate) fn drain(&mut self) -> Vec<Message> {
        let mut out = Vec::new();
        loop {
            match self.messages.try_recv() {
                Ok(message) => {
                    match &message {
                        Message::Installed | Message::Abandoned => self.exited = true,
                        Message::InstallPending => {
                            self.exited = true;
                            self.install_pending = true;
                        }
                        _ => {}
                    }
                    out.push(message);
                }
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.exited = true;
                    break;
                }
            }
        }
        if self.thread.is_finished() && !self.exited {
            // Drain once more: the last messages precede the exit.
            while let Ok(message) = self.messages.try_recv() {
                out.push(message);
            }
            self.exited = true;
        }
        out
    }
    /// A boxed sink for `ExecCmdJob`, sharing this handle's state.
    pub(crate) fn output_sink(&self) -> Box<dyn OutputSink> {
        Box::new(Sink(self.sink.clone()))
    }
}

impl Drop for Handle {
    /// A handle dropped before its worker exits (the owner became unavailable
    /// or shut down) abandons it: staging stays an unverified prefix.
    fn drop(&mut self) {
        if !self.exited {
            self.abandon();
        }
    }
}

/// Starts the worker thread. Fails only if the thread cannot start.
pub(crate) fn start(spec: Start) -> std::io::Result<Handle> {
    let (tx, rx) = sync_channel(QUEUE_CHUNKS);
    let (messages_tx, messages) = sync_channel(MESSAGES);
    let shared = Arc::new(Shared {
        queued_bytes: AtomicUsize::new(0),
        queued_chunks: AtomicUsize::new(0),
        backpressured: AtomicBool::new(false),
        stored: AtomicBool::new(false),
        control: Mutex::new(Control::default()),
        changed: Condvar::new(),
    });
    let listener: Arc<dyn SpaceListener> = shared.clone();
    spec.store.listen(&listener);
    let sink = SinkHandle {
        state: Arc::new(Mutex::new(SinkState {
            tx: Some(tx),
            pending: VecDeque::new(),
            pending_bytes: 0,
            accepted: [0; 2],
            discarded: [0; 2],
            ended: [false; 2],
        })),
        shared: shared.clone(),
    };
    let job = spec.job;
    let worker_shared = shared.clone();
    let thread = std::thread::Builder::new()
        .name("asura-capture".into())
        .spawn(move || {
            let _listener = listener;
            Worker::new(spec, rx, worker_shared, messages_tx).run();
        })?;
    Ok(Handle {
        job,
        shared,
        messages,
        thread,
        sink,
        exited: false,
        install_pending: false,
    })
}

#[derive(Debug, Eq, PartialEq)]
enum Wake {
    Abandon,
    Cancel,
    /// The leader exited: retry once at once; a short then faults.
    Drain,
    Space,
    Timer,
}
/// Waits for the next retry: abandonment, a cancel trigger, the leader's
/// exit, a space event (retry at once; the sequence restarts) or the
/// backoff delay. The paused-capture retry backoff (owner, 2026-10-15) is
/// the shared [`Fibonacci`] pacing: 1, 1, 2, 3, 5, 8 s and so on, at most
/// 60 s apart. It is pacing only: it never ends or refuses work. The
/// sequence advances only when the delay elapses.
fn wait_retry(shared: &Shared, space_seen: &mut u64, backoff: &mut Fibonacci) -> Wake {
    let mut advanced = *backoff;
    let end = Instant::now() + advanced.next_delay();
    let mut control = shared.control();
    loop {
        if control.abandon {
            return Wake::Abandon;
        }
        if control.cancel {
            return Wake::Cancel;
        }
        if control.draining {
            return Wake::Drain;
        }
        if control.space != *space_seen {
            *space_seen = control.space;
            backoff.reset();
            return Wake::Space;
        }
        let now = Instant::now();
        if now >= end {
            *backoff = advanced;
            return Wake::Timer;
        }
        control = shared
            .changed
            .wait_timeout(control, end - now)
            .unwrap_or_else(|e| e.into_inner())
            .0;
    }
}
enum Step {
    Short(PauseReason),
    Fault,
}
enum Written {
    Ok,
    Cancelled,
    Fault,
    Abandoned,
}

struct Worker {
    store: Arc<PayloadStore>,
    call: Id,
    victims: Vec<Victim>,
    redaction: Option<JobRedaction>,
    wake: Option<WakeSender>,
    gate_home: Option<std::path::PathBuf>,
    input: Receiver<Input>,
    shared: Arc<Shared>,
    messages: SyncSender<Message>,
    capture: Option<Capture>,
    reservation: u64,
    discarding: [bool; 2],
    discarded: [u64; 2],
    eof: [bool; 2],
    degraded: [bool; 2],
    lines: [u64; 2],
    fault: Option<CaptureReason>,
    cancelled_while_paused: Option<PauseReason>,
    received: bool,
    started: Instant,
    pause: Option<(PauseReason, Instant)>,
    pause_count: u32,
    first_pause_ms: u64,
    paused_ms: u64,
    delays: Fibonacci,
    space_seen: u64,
    disconnected: bool,
}
impl Worker {
    fn new(
        spec: Start,
        input: Receiver<Input>,
        shared: Arc<Shared>,
        messages: SyncSender<Message>,
    ) -> Self {
        Self {
            store: spec.store,
            call: spec.job,
            victims: spec.victims,
            redaction: Some(spec.redaction),
            wake: spec.wake,
            gate_home: spec.gate_home,
            input,
            shared,
            messages,
            capture: None,
            reservation: RESERVATION_STEP,
            discarding: [false; 2],
            discarded: [0; 2],
            eof: [false; 2],
            degraded: [false; 2],
            lines: [0; 2],
            fault: None,
            cancelled_while_paused: None,
            received: false,
            started: Instant::now(),
            pause: None,
            pause_count: 0,
            first_pause_ms: 0,
            paused_ms: 0,
            delays: Fibonacci::new(),
            space_seen: 0,
            disconnected: false,
        }
    }
    /// Blocking send to the reactor; a closed reactor side means abandon.
    fn send(&self, message: Message) -> bool {
        let ok = self.messages.send(message).is_ok();
        if let Some(wake) = &self.wake {
            let _ = wake.notify();
        }
        ok
    }
    fn try_send(&self, message: Message) {
        let _ = self.messages.try_send(message);
        if let Some(wake) = &self.wake {
            let _ = wake.notify();
        }
    }
    fn abandoned(&self) -> bool {
        self.shared.control().abandon
    }
    fn draining(&self) -> bool {
        self.shared.control().draining
    }
    /// A short in `Draining` (TJ-S1): leave any pause and fault with the
    /// short's reason; later bytes are discarded and counted.
    fn short_while_draining(&mut self, reason: PauseReason) {
        self.leave_pause();
        self.fault(reason.reason());
    }
    fn wait_retry(&mut self) -> Wake {
        wait_retry(&self.shared, &mut self.space_seen, &mut self.delays)
    }
    fn enter_pause(&mut self, reason: PauseReason) {
        match &mut self.pause {
            Some((current, _)) => *current = reason,
            None => {
                self.pause_count += 1;
                if self.pause_count == 1 {
                    self.first_pause_ms = self.started.elapsed().as_millis() as u64;
                }
                self.space_seen = self.shared.control().space;
                self.delays.reset();
                self.pause = Some((reason, Instant::now()));
                self.send(Message::Paused(reason));
            }
        }
    }
    fn leave_pause(&mut self) {
        if let Some((_, since)) = self.pause.take() {
            self.paused_ms += since.elapsed().as_millis() as u64;
            self.send(Message::Resumed);
        }
    }
    fn end_pause_cancelled(&mut self) -> PauseReason {
        let (reason, since) = self.pause.take().expect("paused");
        self.paused_ms += since.elapsed().as_millis() as u64;
        self.cancelled_while_paused.get_or_insert(reason);
        reason
    }
    fn fault(&mut self, reason: CaptureReason) {
        if self.fault.is_none() {
            self.fault = Some(reason);
            self.discarding = [true; 2];
            self.send(Message::Fault(reason));
        }
    }

    fn delete_victims(&mut self, victims: Vec<Victim>) {
        for victim in victims {
            // A failed deletion counts its bytes again; growth retries later.
            let _ = self.store.delete_victim(victim);
        }
    }

    fn run(mut self) {
        // Opening: delete victims, then create both staging files.
        let victims = std::mem::take(&mut self.victims);
        self.delete_victims(victims);
        loop {
            match self.store.create_capture(self.call) {
                Ok(capture) => {
                    self.capture = Some(capture);
                    self.leave_pause();
                    break;
                }
                Err(PayloadError::NoSpace) if self.draining() => {
                    self.short_while_draining(PauseReason::Enospc);
                    break;
                }
                Err(PayloadError::NoSpace) => {
                    self.enter_pause(PauseReason::Enospc);
                    match self.wait_retry() {
                        Wake::Abandon => return self.abandon(),
                        Wake::Cancel => {
                            self.end_pause_cancelled();
                            self.discarding = [true; 2];
                            break;
                        }
                        Wake::Drain | Wake::Space | Wake::Timer => {}
                    }
                }
                Err(_) => {
                    self.fault(CaptureReason::StorageFault);
                    break;
                }
            }
        }
        // Capturing.
        while !(self.eof[0] && self.eof[1]) {
            if self.abandoned() {
                return self.abandon();
            }
            let input = match self.input.recv() {
                Ok(input) => input,
                Err(_) => {
                    self.disconnected = true;
                    break;
                }
            };
            match input {
                Input::Chunk(stream, bytes) => {
                    self.received = true;
                    self.shared
                        .queued_bytes
                        .fetch_sub(bytes.len(), Ordering::AcqRel);
                    self.shared.queued_chunks.fetch_sub(1, Ordering::AcqRel);
                    if self.shared.backpressured.swap(false, Ordering::AcqRel) {
                        self.try_send(Message::Room);
                    }
                    if !self.discarding[index(stream)] && self.shared.control().drop_output {
                        // A cancel trigger in `Draining` (J2): discard from here.
                        self.discarding = [true; 2];
                    }
                    if self.discarding[index(stream)] || self.capture.is_none() {
                        self.discarded[index(stream)] += bytes.len() as u64;
                        continue;
                    }
                    self.gates();
                    let Some(redacted) = self.scan(stream, &bytes) else {
                        self.discarded[index(stream)] += bytes.len() as u64;
                        continue;
                    };
                    drop(bytes);
                    match self.write(stream, &redacted) {
                        Written::Ok => {}
                        Written::Abandoned => return self.abandon(),
                        Written::Cancelled | Written::Fault => {}
                    }
                }
                Input::End(stream, discarded) => {
                    self.received = true;
                    self.eof[index(stream)] = true;
                    self.discarded[index(stream)] += discarded;
                }
            }
        }
        if self.abandoned() {
            return self.abandon();
        }
        self.seal();
    }

    /// Test-support gate, throttle and panic hooks before each scan (G18).
    fn gates(&self) {
        #[cfg(feature = "test-support")]
        if let Some(home) = &self.gate_home {
            // The held scan is a blocked call: abandonment takes effect
            // only when it returns.
            while home.join("capture-hold-scan").exists() {
                std::thread::sleep(Duration::from_millis(2));
            }
            if let Ok(text) = std::fs::read_to_string(home.join("capture-throttle-scan"))
                && let Ok(micros) = text.trim().parse::<u64>()
            {
                std::thread::sleep(Duration::from_micros(micros));
            }
        }
        #[cfg(not(feature = "test-support"))]
        let _ = &self.gate_home;
    }

    /// Scans one chunk inside `catch_unwind`. After a panic the stream is
    /// degraded: this and every later chunk go to `detector_error` (SD2). A
    /// panic inside `detector_error` is a capture fault. Unchecked bytes are
    /// never staged.
    fn scan(&mut self, stream: Stream, chunk: &[u8]) -> Option<Redacted> {
        if !self.degraded[index(stream)] {
            let redaction = self.redaction.as_mut()?;
            let gate = &self.gate_home;
            let result = catch_unwind(AssertUnwindSafe(|| {
                #[cfg(feature = "test-support")]
                if let Some(home) = gate
                    && home.join("capture-panic-scan").exists()
                {
                    panic!("test-support detector panic");
                }
                #[cfg(not(feature = "test-support"))]
                let _ = gate;
                redaction.scan(stream, chunk)
            }));
            match result {
                Ok(redacted) => return Some(redacted),
                Err(_) => self.degraded[index(stream)] = true,
            }
        }
        self.detector_error(stream, chunk.len() as u64)
    }
    fn detector_error(&mut self, stream: Stream, undecided: u64) -> Option<Redacted> {
        let redaction = self.redaction.as_mut()?;
        let gate = &self.gate_home;
        let result = catch_unwind(AssertUnwindSafe(|| {
            // Test-support: `twice` in the panic file also fails here.
            #[cfg(feature = "test-support")]
            if let Some(home) = gate
                && std::fs::read_to_string(home.join("capture-panic-scan"))
                    .is_ok_and(|t| t.trim() == "twice")
            {
                panic!("test-support detector_error panic");
            }
            #[cfg(not(feature = "test-support"))]
            let _ = gate;
            redaction.detector_error(stream, undecided)
        }));
        match result {
            Ok(redacted) => Some(redacted),
            Err(_) => {
                self.fault(CaptureReason::DetectorFault);
                None
            }
        }
    }

    /// One growth or write attempt: reserve in 1 MiB steps (deleting chosen
    /// victims) while keeping 16 MiB free, then append the rest.
    fn try_write(
        &mut self,
        stream: Stream,
        redacted: &Redacted,
        written: &mut usize,
    ) -> Result<(), Step> {
        let capture = self.capture.as_ref().ok_or(Step::Fault)?;
        let remaining = (redacted.len() - *written) as u64;
        while capture.total() + remaining > self.reservation {
            let free = self.store.free_bytes().map_err(|_| Step::Fault)?;
            if free < RECOVERY_RESERVE + RESERVATION_STEP {
                return Err(Step::Short(PauseReason::Enospc));
            }
            match self.store.grow(self.call) {
                Ok(victims) => {
                    self.reservation += RESERVATION_STEP;
                    for victim in victims {
                        let _ = self.store.delete_victim(victim);
                    }
                }
                Err(Short::Quota) => return Err(Step::Short(PauseReason::Quota)),
            }
        }
        let capture = self.capture.as_mut().ok_or(Step::Fault)?;
        let (n, result) = capture.append(store_index(stream), redacted, *written);
        // `append` has raised the watermark: signal the stored bytes.
        self.shared.signal_stored(n, self.wake.as_ref());
        self.lines[index(stream)] += newlines(&redacted.as_bytes()[*written..*written + n]);
        *written += n;
        match result {
            Ok(()) => Ok(()),
            Err(PayloadError::NoSpace) => Err(Step::Short(PauseReason::Enospc)),
            Err(_) => Err(Step::Fault),
        }
    }
    fn write(&mut self, stream: Stream, redacted: &Redacted) -> Written {
        let mut written = 0;
        loop {
            match self.try_write(stream, redacted, &mut written) {
                Ok(()) => {
                    self.leave_pause();
                    return Written::Ok;
                }
                Err(Step::Fault) => {
                    self.fault(CaptureReason::StorageFault);
                    return Written::Fault;
                }
                Err(Step::Short(reason)) if self.draining() => {
                    self.short_while_draining(reason);
                    return Written::Fault;
                }
                Err(Step::Short(reason)) => {
                    self.enter_pause(reason);
                    match self.wait_retry() {
                        Wake::Abandon => return Written::Abandoned,
                        Wake::Cancel => {
                            self.end_pause_cancelled();
                            // Queued chunks and later bytes are discarded.
                            self.discarding = [true; 2];
                            return Written::Cancelled;
                        }
                        // Draining: retry once now; a short then faults.
                        Wake::Drain | Wake::Space | Wake::Timer => {}
                    }
                }
            }
        }
    }

    /// Flushing: the scanner's final bytes are staged, staging is synced,
    /// the report is sealed and the redaction state is dropped (SD4). Then
    /// the worker waits for install or abandonment.
    fn seal(mut self) {
        let before: [StreamStats; 2] = match &self.redaction {
            Some(r) => [
                r.stats(Stream::Stdout).clone(),
                r.stats(Stream::Stderr).clone(),
            ],
            None => [StreamStats::default(), StreamStats::default()],
        };
        let redaction = self.redaction.take();
        let settled =
            redaction.map(|redaction| catch_unwind(AssertUnwindSafe(move || redaction.settle())));
        let summary: Option<JobSummary> = match settled {
            Some(Ok((summary, finals))) => {
                for (stream, redacted) in STREAMS.into_iter().zip(finals) {
                    if self.discarding[index(stream)]
                        || redacted.is_empty()
                        || self.capture.is_none()
                    {
                        continue;
                    }
                    match self.write(stream, &redacted) {
                        Written::Abandoned => return self.abandon(),
                        Written::Ok | Written::Cancelled | Written::Fault => {}
                    }
                }
                Some(summary)
            }
            Some(Err(_)) => {
                self.fault(CaptureReason::DetectorFault);
                None
            }
            None => None,
        };
        if let Some(capture) = &self.capture
            && capture.sync().is_err()
        {
            self.fault(CaptureReason::StorageFault);
        }
        let closed = self.shared.control().closed;
        let not_started = !self.received && self.disconnected && !closed;
        let sealed = self.capture.take().map(Capture::seal);
        if not_started {
            // No process ran: the staging files are removed now; recovery
            // also deletes a stray one.
            self.store.discard_staging(self.call);
        }
        let stats = match &summary {
            Some(summary) => [summary.stdout.clone(), summary.stderr.clone()],
            None => before,
        };
        let empty: Hash = sha2_empty();
        let streams: [StreamReport; 2] = std::array::from_fn(|i| {
            let (stored, digest) = match &sealed {
                Some(pair) => (pair[i].0, Some(pair[i].1)),
                None => (0, Some(empty)),
            };
            StreamReport {
                stored,
                digest: if not_started { None } else { digest },
                stats: stats[i].clone(),
                discarded: self.discarded[i],
                eof: self.eof[i],
                lines: if not_started { 0 } else { self.lines[i] },
            }
        });
        let (state, reason) = if not_started {
            (
                CaptureState::NotStarted,
                if self.fault.is_some() {
                    CaptureReason::StorageFault
                } else {
                    CaptureReason::None
                },
            )
        } else if let Some(reason) = self.fault {
            (CaptureState::Incomplete, reason)
        } else if let Some(reason) = self.cancelled_while_paused {
            (CaptureState::Incomplete, reason.reason())
        } else if closed && !(self.eof[0] && self.eof[1]) {
            // OD2: the read ends were closed before EOF; a fault or a
            // pause cancel above names its own cause first.
            (CaptureState::Incomplete, CaptureReason::ClosedAfterCancel)
        } else if self.discarded.iter().any(|d| *d > 0) {
            (
                CaptureState::Incomplete,
                CaptureReason::DiscardedAfterCancel,
            )
        } else if !(self.eof[0] && self.eof[1]) {
            // The producer ended without EOF: never claimed complete.
            (CaptureState::Incomplete, CaptureReason::StorageFault)
        } else {
            (CaptureState::Complete, CaptureReason::None)
        };
        let streams = if not_started {
            streams.map(|mut s| {
                s.stored = 0;
                s.stats.observed_bytes = 0;
                s.stats.stored_bytes = 0;
                s
            })
        } else {
            streams
        };
        let mut numbers: Vec<CaptureNumber> = summary
            .as_ref()
            .map(|s| {
                s.numbers
                    .iter()
                    .filter_map(|n| {
                        Some(CaptureNumber {
                            number: u16::try_from(n.number).ok()?,
                            kind: n.kind,
                            origin: match n.origin {
                                NumberOrigin::Command => NumberOrigin::Command,
                                NumberOrigin::Output => NumberOrigin::Output,
                            },
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        numbers.sort_by_key(|n| n.number);
        numbers.dedup_by_key(|n| n.number);
        numbers.truncate(256);
        let distinct_numbers = summary
            .as_ref()
            .map_or(0, |s| s.distinct_numbers)
            .max(numbers.len() as u32);
        let report = Report {
            state,
            reason,
            streams,
            distinct_numbers,
            numbers,
            pause_count: self.pause_count,
            first_pause_ms: self.first_pause_ms,
            paused_ms: self.paused_ms,
            final_reservation: self.reservation,
        };
        if !self.send(Message::Sealed(Box::new(report))) {
            return;
        }
        // Installing: wait for kind 28 to commit, a not_started
        // discard, or abandonment.
        let (install, discard) = {
            let mut control = self.shared.control();
            loop {
                if control.abandon {
                    break (false, false);
                }
                if control.discard {
                    break (false, true);
                }
                if control.install {
                    break (true, false);
                }
                control = self
                    .shared
                    .changed
                    .wait(control)
                    .unwrap_or_else(|e| e.into_inner());
            }
        };
        if discard {
            // No process ran (J2 `not_started`): nothing is retained.
            self.store.discard_staging(self.call);
            self.send(Message::Installed);
            return;
        }
        if !install {
            self.send(Message::Abandoned);
            return;
        }
        if not_started {
            self.send(Message::Installed);
            return;
        }
        match self.store.install(self.call) {
            Ok(()) => self.send(Message::Installed),
            // Staging stays for recovery to install after the digest check.
            Err(_) => self.send(Message::InstallPending),
        };
    }
    /// Abandoned after `unconfirmed`: keep staging as an unverified prefix.
    fn abandon(mut self) {
        let stored = self.capture.as_ref().map_or(0, Capture::total);
        self.capture = None;
        self.redaction = None;
        self.store.abandoned(self.call, stored);
        self.send(Message::Abandoned);
    }
}

fn sha2_empty() -> Hash {
    use sha2::{Digest, Sha256};
    Sha256::digest([]).into()
}

#[cfg(test)]
mod tests;
