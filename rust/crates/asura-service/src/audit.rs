//! One retained audit writer. Reactor handles never perform filesystem IO.
use asura_platform::{RuntimeDirectory, events::WakeSender};
use asura_storage::audit::{
    self as record, ConfigLoad, Event, Health, HealthReason, HealthState, Record, Settings,
    StopReason,
};
use std::{
    cell::RefCell,
    collections::VecDeque,
    io,
    rc::Rc,
    sync::{
        Arc, Mutex, TryLockError,
        atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
        mpsc::{self, Receiver, SyncSender, TrySendError},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
const CAPACITY: usize = 256;
const CYCLE: Duration = Duration::from_secs(2);
const DRAIN: Duration = Duration::from_secs(1);
const IDLE: Duration = Duration::from_millis(100);

#[derive(Clone, Debug)]
pub(crate) struct Snapshot {
    pub health: Health,
    pub records: Arc<Vec<Record>>,
}
impl Snapshot {
    fn starting() -> Self {
        Self {
            health: Health {
                state: HealthState::Starting,
                enabled: false,
                keep_files: None,
                max_file_bytes: None,
                dropped: 0,
                window_capacity: CAPACITY as u32,
                hydrated: false,
                older_omitted: false,
                reason: None,
            },
            records: Arc::new(Vec::new()),
        }
    }
}
struct Shared {
    publication: Mutex<Option<Snapshot>>,
    state: AtomicU32,
    fatal: AtomicU32,
    dropped: AtomicU64,
    deadline_ms: AtomicU64,
    bootstrap_complete: AtomicBool,
    closing: AtomicBool,
    cancel: AtomicBool,
    anchor: Instant,
    completion: crate::completion::Completion,
}
impl Shared {
    fn finish_bootstrap(&self) {
        self.bootstrap_complete.store(true, Ordering::Release);
        self.completion.notify();
    }
    fn publish(&self, snapshot: &Snapshot) {
        self.state
            .store(snapshot.health.state as u32, Ordering::Release);
        *self.publication.lock().unwrap_or_else(|e| e.into_inner()) = Some(snapshot.clone());
        self.completion.notify();
    }
    fn begin_cycle(&self) -> Instant {
        let deadline = Instant::now() + CYCLE;
        self.deadline_ms.store(
            millis(deadline.saturating_duration_since(self.anchor)).max(1),
            Ordering::Release,
        );
        self.completion.notify();
        deadline
    }
    fn idle(&self) {
        self.deadline_ms.store(0, Ordering::Release);
    }
    fn dropped(&self, count: u64) {
        let _ = self
            .dropped
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |old| {
                Some(old.saturating_add(count))
            });
        self.completion.notify();
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Admission {
    Accepted,
    Dropped,
    Disabled,
    Unavailable,
    Closed,
    Invalid,
    SequenceExhausted,
}
#[derive(Clone, Copy)]
struct Gap {
    count: u64,
    first: u64,
    last: u64,
}
struct AdmissionState {
    sender: SyncSender<Record>,
    epoch: [u8; 16],
    build: String,
    next: u64,
    gap: Option<Gap>,
    closed: bool,
}
/// Clones belong to the same reactor. Rc prevents accidental cross-thread producers.
#[derive(Clone)]
pub(crate) struct Emitter {
    admission: Rc<RefCell<AdmissionState>>,
    shared: Arc<Shared>,
}
impl Emitter {
    pub(crate) fn try_emit(&self, event: Event) -> Admission {
        let Ok(mut state) = self.admission.try_borrow_mut() else {
            return Admission::Unavailable;
        };
        if state.closed {
            return Admission::Closed;
        }
        match self.shared.state.load(Ordering::Acquire) {
            3 => return Admission::Disabled,
            5 => {
                self.shared.dropped(1);
                return Admission::Unavailable;
            }
            _ => {}
        }
        if event.validate().is_err() {
            return Admission::Invalid;
        }
        let sequence = state.next;
        let Some(next) = sequence.checked_add(1) else {
            state.closed = true;
            self.shared
                .fatal
                .store(HealthReason::SequenceExhausted as u32, Ordering::Release);
            return Admission::SequenceExhausted;
        };
        state.next = next;
        if let Some(gap) = state.gap {
            let record = make_record(
                &state.build,
                state.epoch,
                gap.last,
                Event::JournalGap {
                    count: gap.count,
                    first_sequence: gap.first,
                    last_sequence: gap.last,
                    reason: HealthReason::QueueFull,
                },
            );
            match state.sender.try_send(record) {
                Ok(()) => state.gap = None,
                Err(TrySendError::Full(_)) => return lost(&mut state, &self.shared, sequence),
                Err(TrySendError::Disconnected(_)) => {
                    self.shared.dropped(1);
                    return Admission::Unavailable;
                }
            }
        }
        let record = make_record(&state.build, state.epoch, sequence, event);
        match state.sender.try_send(record) {
            Ok(()) => Admission::Accepted,
            Err(TrySendError::Full(_)) => lost(&mut state, &self.shared, sequence),
            Err(TrySendError::Disconnected(_)) => {
                self.shared.dropped(1);
                Admission::Unavailable
            }
        }
    }
}
fn lost(state: &mut AdmissionState, shared: &Shared, sequence: u64) -> Admission {
    state.gap = Some(match state.gap {
        Some(gap) => Gap {
            count: gap.count.saturating_add(1),
            last: sequence,
            ..gap
        },
        None => Gap {
            count: 1,
            first: sequence,
            last: sequence,
        },
    });
    shared.dropped(1);
    Admission::Dropped
}

pub(crate) struct Worker {
    task: Option<JoinHandle<()>>,
    emitter: Emitter,
    latest: Snapshot,
    closing_at: Option<Instant>,
}
impl Worker {
    pub(crate) fn start(
        runtime: RuntimeDirectory,
        epoch: [u8; 16],
        build: String,
        wake: WakeSender,
    ) -> io::Result<Self> {
        Self::launch(epoch, build, wake, move |shared| {
            let deadline = shared.begin_cycle();
            let (settings, configuration) =
                asura_storage::config::audit_settings(runtime.clone(), deadline, &shared.cancel)
                    .map_err(|code| match code {
                        "config_cancelled_or_expired" => {
                            StartError::Storage(if shared.cancel.load(Ordering::Acquire) {
                                record::Error::Cancelled
                            } else {
                                record::Error::Deadline
                            })
                        }
                        "config_io_or_unsafe_file" | "config_changed" => {
                            StartError::Storage(record::Error::UnsafeStorage)
                        }
                        "config_read_failed" => StartError::Storage(record::Error::Io),
                        _ => StartError::Config,
                    })?;
            let mut snapshot = Snapshot::starting();
            snapshot.health.enabled = settings.enabled;
            snapshot.health.keep_files = Some(settings.keep_files);
            snapshot.health.max_file_bytes = Some(settings.max_file_bytes);
            shared.publish(&snapshot);
            let opened =
                record::AuditStore::open(runtime, settings, epoch, deadline, &shared.cancel)
                    .map_err(StartError::Storage)?;
            Ok(Bootstrap {
                settings,
                configuration,
                sink: Box::new(opened.store),
                recent: opened.recent,
                older_omitted: opened.older_omitted,
                lost_partial: opened.lost_partial,
            })
        })
    }
    fn launch<F>(epoch: [u8; 16], build: String, wake: WakeSender, bootstrap: F) -> io::Result<Self>
    where
        F: FnOnce(&Shared) -> Result<Bootstrap, StartError> + Send + 'static,
    {
        if epoch == [0; 16]
            || build.is_empty()
            || build.len() > 128
            || build.chars().any(char::is_control)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "audit_identity_invalid",
            ));
        }
        let (sender, receiver) = mpsc::sync_channel(CAPACITY);
        let completion = crate::completion::Completion::default();
        completion.register(wake);
        let shared = Arc::new(Shared {
            publication: Mutex::new(None),
            state: AtomicU32::new(1),
            fatal: AtomicU32::new(0),
            dropped: AtomicU64::new(0),
            deadline_ms: AtomicU64::new(2_000),
            bootstrap_complete: AtomicBool::new(false),
            closing: AtomicBool::new(false),
            cancel: AtomicBool::new(false),
            anchor: Instant::now(),
            completion,
        });
        let emitter = Emitter {
            admission: Rc::new(RefCell::new(AdmissionState {
                sender,
                epoch,
                build: build.clone(),
                next: 2,
                gap: None,
                closed: false,
            })),
            shared: shared.clone(),
        };
        let task = thread::Builder::new()
            .name("asura-audit".into())
            .spawn(move || {
                let _settled = shared.completion.guard();
                if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    run(receiver, epoch, build, &shared, bootstrap)
                }))
                .is_err()
                {
                    // All bootstrap stack frames have unwound before releasing the gate.
                    shared.finish_bootstrap();
                    let mut failed = Snapshot::starting();
                    failed.health.state = HealthState::Unavailable;
                    failed.health.reason = Some(HealthReason::Io);
                    shared.publish(&failed);
                }
            })?;
        Ok(Self {
            task: Some(task),
            emitter,
            latest: Snapshot::starting(),
            closing_at: None,
        })
    }
    /// Actual bootstrap settlement, independent of health deadlines/publication.
    pub(crate) fn bootstrap_settled(&self) -> bool {
        self.emitter
            .shared
            .bootstrap_complete
            .load(Ordering::Acquire)
    }
    pub(crate) fn emitter(&self) -> Emitter {
        self.emitter.clone()
    }
    /// Nonblocking snapshot transfer and finished-handle release; never joins.
    pub(crate) fn poll(&mut self, now: Instant) -> Option<Snapshot> {
        if self.settlement_expired(now) {
            self.emitter.shared.cancel.store(true, Ordering::Release);
        }
        let next = match self.emitter.shared.publication.try_lock() {
            Ok(mut slot) => slot.take(),
            Err(TryLockError::Poisoned(error)) => error.into_inner().take(),
            Err(TryLockError::WouldBlock) => None,
        };
        let changed = next.is_some();
        if let Some(next) = next {
            self.latest = next;
        }
        if self.task.as_ref().is_some_and(JoinHandle::is_finished) {
            self.task.take();
        }
        changed.then(|| self.snapshot(now))
    }
    pub(crate) fn snapshot(&self, now: Instant) -> Snapshot {
        let mut snapshot = self.latest.clone();
        snapshot.health = self.health(now);
        if snapshot.health.state == HealthState::Unavailable {
            snapshot.records = Arc::new(Vec::new());
        }
        snapshot
    }
    pub(crate) fn health(&self, now: Instant) -> Health {
        let mut health = self.latest.health.clone();
        health.dropped = self.emitter.shared.dropped.load(Ordering::Acquire);
        if self.emitter.shared.fatal.load(Ordering::Acquire)
            == HealthReason::SequenceExhausted as u32
        {
            health.state = HealthState::Unavailable;
            health.reason = Some(HealthReason::SequenceExhausted);
        }
        let deadline = self.emitter.shared.deadline_ms.load(Ordering::Acquire);
        if deadline > 0
            && health.state != HealthState::Unavailable
            && !self.settled()
            && millis(now.saturating_duration_since(self.emitter.shared.anchor)) >= deadline
        {
            health.state = HealthState::Stale;
            health.reason = Some(HealthReason::Deadline);
        }
        health
    }
    pub(crate) fn next_deadline(&self, now: Instant) -> Option<Instant> {
        let work = self.emitter.shared.deadline_ms.load(Ordering::Acquire);
        let work = (work > 0)
            .then(|| self.emitter.shared.anchor + Duration::from_millis(work))
            .filter(|d| *d > now);
        let drain = self
            .closing_at
            .map(|at| at + DRAIN)
            .filter(|d| *d > now && !self.settled());
        work.into_iter()
            .chain(drain)
            .chain(
                self.task
                    .as_ref()
                    .and_then(|_| self.emitter.shared.completion.settlement_deadline(now)),
            )
            .min()
    }
    pub(crate) fn close(&mut self, now: Instant) {
        self.close_with_reason(now, StopReason::OwnerLifetimeEnded);
    }
    pub(crate) fn close_with_reason(&mut self, now: Instant, reason: StopReason) {
        if self.closing_at.is_some() {
            return;
        }
        let _ = self.emitter.try_emit(Event::ServiceStopping { reason });
        if let Ok(mut state) = self.emitter.admission.try_borrow_mut() {
            state.closed = true;
        }
        self.closing_at = Some(now);
        self.emitter.shared.closing.store(true, Ordering::Release);
    }
    pub(crate) fn settled(&self) -> bool {
        self.task.as_ref().is_none_or(JoinHandle::is_finished)
    }
    pub(crate) fn settlement_expired(&self, now: Instant) -> bool {
        self.closing_at
            .is_some_and(|at| now.saturating_duration_since(at) >= DRAIN)
            && !self.settled()
    }
}
impl Drop for Worker {
    fn drop(&mut self) {
        self.close(Instant::now());
        self.emitter.shared.cancel.store(true, Ordering::Release);
        // The service must retain its owner lock until settled(), including repair drain.
    }
}
trait Sink: Send {
    fn append(
        &mut self,
        records: &[Record],
        deadline: Instant,
        cancel: &AtomicBool,
    ) -> Result<(), record::Error>;
    fn maintain(&mut self, deadline: Instant, cancel: &AtomicBool) -> Result<(), record::Error>;
    fn sync(&mut self, deadline: Instant, cancel: &AtomicBool) -> Result<(), record::Error>;
}
impl Sink for record::AuditStore {
    fn append(
        &mut self,
        records: &[Record],
        deadline: Instant,
        cancel: &AtomicBool,
    ) -> Result<(), record::Error> {
        self.append_batch(records, deadline, cancel)
    }
    fn maintain(&mut self, deadline: Instant, cancel: &AtomicBool) -> Result<(), record::Error> {
        self.maintain(deadline, cancel)
    }
    fn sync(&mut self, deadline: Instant, cancel: &AtomicBool) -> Result<(), record::Error> {
        self.sync(deadline, cancel)
    }
}
struct Bootstrap {
    settings: Settings,
    configuration: ConfigLoad,
    sink: Box<dyn Sink>,
    recent: Vec<Record>,
    older_omitted: bool,
    lost_partial: bool,
}
enum StartError {
    Config,
    Storage(record::Error),
}
fn run<F>(receiver: Receiver<Record>, epoch: [u8; 16], build: String, shared: &Shared, bootstrap: F)
where
    F: FnOnce(&Shared) -> Result<Bootstrap, StartError>,
{
    let mut snapshot = Snapshot::starting();
    let bootstrap = bootstrap(shared);
    shared.finish_bootstrap();
    let mut opened = match bootstrap {
        Ok(value) => value,
        Err(error) => {
            snapshot.health.state = HealthState::Unavailable;
            snapshot.health.reason = Some(match error {
                StartError::Config => HealthReason::ConfigInvalid,
                StartError::Storage(error) => reason(error),
            });
            shared.dropped(
                receiver
                    .try_iter()
                    .filter(|r| !matches!(r.event, Event::JournalGap { .. }))
                    .count() as u64,
            );
            shared.publish(&snapshot);
            shared.idle();
            return;
        }
    };
    snapshot.health.enabled = opened.settings.enabled;
    snapshot.health.keep_files = Some(opened.settings.keep_files);
    snapshot.health.max_file_bytes = Some(opened.settings.max_file_bytes);
    snapshot.health.hydrated = true;
    snapshot.health.older_omitted = opened.older_omitted || opened.recent.len() > CAPACITY;
    opened.recent.truncate(CAPACITY);
    snapshot.records = Arc::new(opened.recent);
    snapshot.health.state = if opened.settings.enabled {
        HealthState::Active
    } else {
        HealthState::Disabled
    };
    if opened.lost_partial {
        shared.dropped(1);
    }
    shared.publish(&snapshot);
    if !opened.settings.enabled {
        shared.idle();
        return;
    }
    let mut last_diagnostic = None;
    let mut first = Some(make_record(
        &build,
        epoch,
        1,
        Event::ServiceStarted {
            configuration: opened.configuration,
            keep_files: opened.settings.keep_files,
            max_file_bytes: opened.settings.max_file_bytes,
        },
    ));
    loop {
        shared.idle();
        let incoming = if let Some(first) = first.take() {
            Some(first)
        } else {
            match receiver.recv_timeout(IDLE) {
                Ok(record) => Some(record),
                Err(mpsc::RecvTimeoutError::Timeout) if !shared.closing.load(Ordering::Acquire) => {
                    continue;
                }
                Err(_) => None,
            }
        };
        let Some(incoming) = incoming else {
            break;
        };
        let deadline = shared.begin_cycle();
        if shared.dropped.load(Ordering::Acquire) > 0
            && last_diagnostic.is_none_or(|at: Instant| at.elapsed() >= Duration::from_secs(60))
        {
            last_diagnostic = Some(Instant::now());
            tracing::warn!(
                dropped = shared.dropped.load(Ordering::Acquire),
                reason = "audit_records_dropped",
                "Audit records were dropped"
            );
        }
        let mut batch = Vec::with_capacity(16);
        batch.push(incoming);
        while batch.len() < 16 {
            match receiver.try_recv() {
                Ok(record) => batch.push(record),
                Err(_) => break,
            }
        }
        for record in &mut batch {
            if record
                .encode()
                .is_ok_and(|bytes| bytes.len() as u64 <= opened.settings.max_file_bytes)
            {
                continue;
            }
            if !matches!(record.event, Event::JournalGap { .. }) {
                shared.dropped(1);
            }
            record.event = Event::JournalGap {
                count: 1,
                first_sequence: record.sequence,
                last_sequence: record.sequence,
                reason: HealthReason::Limit,
            };
        }
        batch.retain(|record| {
            record
                .encode()
                .is_ok_and(|bytes| bytes.len() as u64 <= opened.settings.max_file_bytes)
        });
        if !batch.is_empty() {
            if let Err(error) = opened.sink.append(&batch, deadline, &shared.cancel) {
                shared.dropped(
                    batch
                        .iter()
                        .filter(|r| !matches!(r.event, Event::JournalGap { .. }))
                        .count() as u64,
                );
                snapshot.health.state = HealthState::Unavailable;
                snapshot.health.reason = Some(reason(error));
                snapshot.records = Arc::new(Vec::new());
                shared.dropped(
                    receiver
                        .try_iter()
                        .filter(|r| !matches!(r.event, Event::JournalGap { .. }))
                        .count() as u64,
                );
                shared.publish(&snapshot);
                shared.idle();
                return;
            }
            let mut recent: VecDeque<_> = Arc::make_mut(&mut snapshot.records).drain(..).collect();
            for record in batch {
                recent.push_front(record);
            }
            if recent.len() > CAPACITY {
                snapshot.health.older_omitted = true;
                recent.truncate(CAPACITY);
            }
            snapshot.records = Arc::new(recent.into());
            shared.publish(&snapshot);
        }
        if let Err(error) = opened.sink.maintain(deadline, &shared.cancel) {
            snapshot.health.state = HealthState::Unavailable;
            snapshot.health.reason = Some(reason(error));
            snapshot.records = Arc::new(Vec::new());
            shared.dropped(
                receiver
                    .try_iter()
                    .filter(|r| !matches!(r.event, Event::JournalGap { .. }))
                    .count() as u64,
            );
            shared.publish(&snapshot);
            shared.idle();
            return;
        }
    }
    if let Err(error) = opened.sink.sync(shared.begin_cycle(), &shared.cancel) {
        snapshot.health.state = HealthState::Unavailable;
        snapshot.health.reason = Some(reason(error));
        snapshot.records = Arc::new(Vec::new());
        shared.publish(&snapshot);
    }
    shared.idle();
}
fn reason(error: record::Error) -> HealthReason {
    match error {
        record::Error::Invalid | record::Error::MalformedRecord => HealthReason::MalformedRecord,
        record::Error::Limit => HealthReason::Limit,
        record::Error::Disabled => HealthReason::Closed,
        record::Error::Cancelled => HealthReason::Cancelled,
        record::Error::Deadline => HealthReason::Deadline,
        record::Error::UnsafeStorage => HealthReason::UnsafeStorage,
        record::Error::Io | record::Error::Unconfirmed => HealthReason::Io,
    }
}
fn make_record(build: &str, epoch: [u8; 16], sequence: u64, event: Event) -> Record {
    Record {
        schema: 1,
        service_build: build.into(),
        service_epoch: epoch,
        sequence,
        unix_time_ms: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .ok()
            .and_then(|d| u64::try_from(d.as_millis()).ok()),
        event,
    }
}
fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests;
