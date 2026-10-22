//! Deterministic fixtures for controller unit tests (J2 validation): a
//! scripted `SlotPort`, a scripted `PayloadPort` and an audit recorder. No
//! fixture starts a thread, a process or a timer.
use super::*;
use asura_storage::payload::PayloadStore;
use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

/// What every fixture slot saw, and the events the test scripts for it.
#[derive(Default)]
pub(super) struct SlotLog {
    pub started: Vec<Id>,
    pub stops: Vec<(Id, StopKind)>,
    pub discards: Vec<Id>,
    pub closes: Vec<Id>,
    /// Events each slot reports on its next poll, at most 8 per poll.
    pub scripts: BTreeMap<Id, VecDeque<SlotEvent>>,
    /// Slots reported idle; a slot is busy until the test marks it.
    pub idle: Vec<Id>,
    /// The next start fails with this error.
    pub start_error: Option<SlotStartError>,
    /// Slots that the controller dropped.
    pub dropped: Vec<Id>,
}
pub(super) type Slots = Rc<RefCell<SlotLog>>;

pub(super) struct FixtureSlot {
    log: Slots,
    job: Option<Id>,
}
impl Drop for FixtureSlot {
    fn drop(&mut self) {
        if let Some(job) = self.job {
            self.log.borrow_mut().dropped.push(job);
        }
    }
}
impl SlotPort for FixtureSlot {
    fn start(&mut self, start: SlotStart) -> Result<(), SlotStartError> {
        let mut log = self.log.borrow_mut();
        if let Some(error) = log.start_error.take() {
            return Err(error);
        }
        if self.job.is_some() {
            return Err(SlotStartError::Busy);
        }
        assert_eq!(start.capture.job, start.job, "capture keyed by job");
        let _ = &start.runtime;
        self.job = Some(start.job);
        log.started.push(start.job);
        Ok(())
    }
    fn stop(&mut self, kind: StopKind) {
        if let Some(job) = self.job {
            self.log.borrow_mut().stops.push((job, kind));
        }
    }
    fn discard_capture(&mut self) {
        if let Some(job) = self.job {
            self.log.borrow_mut().discards.push(job);
        }
    }
    fn close_capture(&mut self) {
        if let Some(job) = self.job {
            self.log.borrow_mut().closes.push(job);
        }
    }
    fn interests(&self, _out: &mut Vec<PollInterest>) {}
    fn next_deadline(&self, _now: Instant) -> Option<Instant> {
        None
    }
    fn poll(&mut self, _now: Instant, out: &mut Vec<SlotEvent>) {
        let Some(job) = self.job else {
            return;
        };
        let mut log = self.log.borrow_mut();
        if let Some(script) = log.scripts.get_mut(&job) {
            while out.len() < SLOT_EVENTS {
                let Some(event) = script.pop_front() else {
                    break;
                };
                out.push(event);
            }
        }
    }
    fn idle(&self) -> bool {
        self.job
            .is_some_and(|job| self.log.borrow().idle.contains(&job))
    }
}
pub(super) fn factory(log: Slots) -> SlotFactory {
    Box::new(move || {
        Box::new(FixtureSlot {
            log: log.clone(),
            job: None,
        })
    })
}

/// A scripted payload owner over one real (empty) payload store, which the
/// capture start needs; no fixture slot starts a capture worker.
pub(super) struct FixturePayloads {
    pub ready: bool,
    pub pending: usize,
    pub epoch: u64,
    pub short: Option<Short>,
    pub admitted: Vec<Id>,
    pub released: Vec<(Id, usize)>,
    pub committed: Vec<(Id, u64, Outcome, bool)>,
    pub abandoned: Vec<(Id, bool)>,
    pub victims: Vec<Victim>,
    /// Stored bytes per job (the watermark the payload owner reads).
    pub stored: BTreeMap<Id, [u64; 2]>,
    /// `refresh_space` calls (TJ-Q9 disk-space recheck).
    pub refreshes: usize,
    store: Arc<PayloadStore>,
}
impl FixturePayloads {
    pub(super) fn new(runtime: &RuntimeDirectory) -> Self {
        Self {
            ready: true,
            pending: 0,
            epoch: 0,
            short: None,
            admitted: Vec::new(),
            released: Vec::new(),
            committed: Vec::new(),
            abandoned: Vec::new(),
            victims: Vec::new(),
            stored: BTreeMap::new(),
            refreshes: 0,
            store: Arc::new(PayloadStore::open(runtime).expect("payload store")),
        }
    }
}
impl PayloadPort for FixturePayloads {
    fn ready(&self) -> bool {
        self.ready
    }
    fn pending_installs(&self) -> usize {
        self.pending
    }
    fn space_epoch(&self) -> u64 {
        self.epoch
    }
    fn admit(&mut self, job: Id, _origin: &Origin) -> Result<Admitted, Short> {
        if let Some(short) = self.short {
            return Err(short);
        }
        self.admitted.push(job);
        Ok(Admitted {
            store: self.store.clone(),
            victims: self.victims.clone(),
        })
    }
    fn release(&mut self, job: Id, victims: &[Victim]) {
        self.released.push((job, victims.len()));
    }
    fn committed(
        &mut self,
        job: Id,
        settled: &JobSettled,
        sequence: u64,
        capture: Option<capture_worker::Handle>,
    ) {
        self.committed
            .push((job, sequence, settled.outcome, capture.is_some()));
    }
    fn abandon(&mut self, job: Id, capture: Option<capture_worker::Handle>) {
        self.abandoned.push((job, capture.is_some()));
    }
    fn stored(&self, job: Id) -> [u64; 2] {
        self.stored.get(&job).copied().unwrap_or_default()
    }
    fn refresh_space(&mut self) {
        self.refreshes += 1;
    }
}

/// Records every audit event in order.
#[derive(Clone, Default)]
pub(super) struct AuditLog(pub Rc<RefCell<Vec<Event>>>);
impl AuditSink for AuditLog {
    fn emit(&self, event: Event) {
        self.0.borrow_mut().push(event);
    }
}

/// A private scratch home, never `~/.asura`.
pub(super) struct Scratch(pub std::path::PathBuf);
impl Scratch {
    pub(super) fn new() -> Self {
        use std::os::unix::fs::DirBuilderExt;
        let path = std::path::PathBuf::from(format!(
            "/private/tmp/asura-jobs-{:x}",
            u128::from_ne_bytes(asura_platform::random_id())
        ));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .expect("scratch home");
        Self(path)
    }
    pub(super) fn runtime(&self) -> RuntimeDirectory {
        RuntimeDirectory::scratch(&self.0, true).expect("scratch runtime")
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A sealed capture of `stdout` bytes: complete with digests, or the
/// given state.
pub(super) fn capture(state: CaptureState, stdout: u64, lines: u64) -> Box<CaptureOutcome> {
    let digest = matches!(state, CaptureState::Complete | CaptureState::Incomplete);
    let stream = |bytes: u64, lines: u64| JobStream {
        capture: journal::CaptureStream {
            observed_bytes: bytes,
            stored_bytes: bytes,
            digest: digest.then_some([7; 32]),
            eof: state == CaptureState::Complete,
            ..Default::default()
        },
        lines,
    };
    Box::new(CaptureOutcome {
        state,
        reason: match state {
            CaptureState::Complete | CaptureState::NotStarted => CaptureReason::None,
            CaptureState::Incomplete => CaptureReason::DiscardedAfterCancel,
            CaptureState::Unconfirmed => CaptureReason::SettlementUnconfirmed,
        },
        stdout: stream(stdout, lines),
        stderr: stream(0, 0),
        distinct_numbers: 0,
        numbers: Vec::new(),
        pause_count: 0,
        first_pause_ms: 0,
        paused_ms: 0,
        final_reservation: 1024 * 1024,
        capture: None,
    })
}
