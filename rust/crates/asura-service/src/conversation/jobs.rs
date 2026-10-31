//! The conversation owner's job wiring (tool-job-controller.md, J2 to J4):
//! the start-time read of the job limits and the execution setting with
//! its paced retry (TJ-F5), the job controller and the job control handler
//! beside `Payloads`, start-time recovery, the journal hold and outage,
//! revocation and the shutdown bound. The owner appends the controller's
//! records through its serialized journal ticket; this module never
//! submits to the writer.
use crate::job_control::JobControl;
use crate::job_controller::{JobController, JobEvent, Pacing, WaitDone};
use crate::payload_owner::{Payloads, SHUTDOWN_BOUND};
use asura_platform::events::WakeSender;
use asura_platform::{PollInterest, RuntimeDirectory};
use asura_storage::authority::conversation::{self as journal, Id, JobReason};
use asura_storage::authority::writer;
use asura_storage::config::{ExecutionSetting, JobLimits};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Instant;

/// Controller events kept for a test reader (`test-support` only).
#[cfg(feature = "test-support")]
const KEPT_EVENTS: usize = 1024;

/// What the start-time read returns: the limits, the execution setting,
/// `personCommands.allowOutsideSandbox` and the secret store target.
type Read = Result<Started, ()>;

/// The start-time configuration (TJ-F5, PCM switch check, secret store
/// target). `store` is `None` when this runtime has no Keychain target
/// ([`super::person_command::store_target`]).
pub(super) struct Started {
    pub limits: JobLimits,
    pub execution: ExecutionSetting,
    pub outside: bool,
    /// `personCommands.environment` (ENV1).
    pub declared: asura_storage::config::DeclaredVariables,
    pub store: Option<asura_platform::keychain::Target>,
}
impl Started {
    /// The defaults with no Keychain target, for tests.
    #[cfg(test)]
    pub(super) fn defaults() -> Self {
        Self {
            limits: JobLimits::default(),
            execution: ExecutionSetting::default(),
            outside: true,
            declared: Default::default(),
            store: None,
        }
    }
}
type ReadSlot = Arc<Mutex<Option<Read>>>;
/// The outside switch, the declared variables and the secret store target of
/// a successful start-time read.
type Person = (
    bool,
    asura_storage::config::DeclaredVariables,
    Option<asura_platform::keychain::Target>,
);

/// One read of `jobs.*` and `execution.*` on a retained worker thread
/// (AE-02: the configuration file read blocks). It has no time bound;
/// shutdown cancels it.
struct LimitsRead {
    cancel: Arc<AtomicBool>,
    result: ReadSlot,
    _thread: JoinHandle<()>,
}

/// The read of the start-time configuration, on its own thread. Tests
/// replace it to script failures (TJ-F5).
pub(super) type Reader = Arc<dyn Fn(RuntimeDirectory, &AtomicBool) -> Read + Send + Sync>;

fn read_configuration(runtime: RuntimeDirectory, cancel: &AtomicBool) -> Read {
    // Test-support gate (TJ-F5 integration): the read fails while
    // `HOME/job-limits-fail` exists in a scratch home.
    #[cfg(feature = "test-support")]
    if runtime
        .scratch_home_for_test()
        .is_some_and(|home| home.join("job-limits-fail").exists())
    {
        return Err(());
    }
    let limits = asura_storage::config::job_limits(runtime.clone(), cancel).map_err(|_| ())?;
    let execution =
        asura_storage::config::execution_setting(runtime.clone(), cancel).map_err(|_| ())?;
    let outside =
        asura_storage::config::allow_outside_sandbox(runtime.clone(), cancel).map_err(|_| ())?;
    let declared =
        asura_storage::config::person_environment(runtime.clone(), cancel).map_err(|_| ())?;
    let store = super::person_command::store_target(&runtime);
    Ok(Started {
        limits,
        execution,
        outside,
        declared,
        store,
    })
}

pub(super) struct Jobs {
    read: Option<LimitsRead>,
    reader: Reader,
    /// The controller, once the limits are read; never rebuilt.
    controller: Option<JobController>,
    /// The job control handler, built with the controller.
    control: Option<JobControl>,
    /// A run of failed reads: the next read waits for this pacing
    /// (TJ-F5). Pacing ends nothing.
    retry: Option<Pacing>,
    /// Reads started, for tests.
    reads: u32,
    /// The execution setting the read returned, for the owner to take once.
    execution: Option<ExecutionSetting>,
    /// The outside switch and secret store target the read returned, for
    /// the owner to take once.
    person: Option<Person>,
    /// The limits read thread could not start or failed; logged once per
    /// run of failures.
    failing: bool,
    /// The unsettled jobs from the start-time replay, until the controller
    /// takes them (J2-E5), with the inputs whose kind 31 has field 7 bit 1.
    unsettled: Option<(Vec<journal::UnsettledJob>, std::collections::BTreeSet<Id>)>,
    replay_seen: bool,
    /// The journal held normal records at the last poll.
    held: bool,
    /// The journal became unavailable; reported once.
    unavailable: bool,
    /// Shutdown began: the 2 s cleanup bound runs from here (AE-06).
    closing: Option<Instant>,
    /// Ended controller waits for the owner, oldest first.
    waits: Vec<WaitDone>,
    /// Controller events for the owner since its last take (person
    /// commands: settlement, `ShellExited`, withdrawal). The controller
    /// coalesces state events per job, so at most one per unsettled job
    /// and kind waits here.
    owner_events: Vec<JobEvent>,
    /// A scripted slot factory for owner unit tests (no process starts).
    #[cfg(test)]
    slots: Option<crate::job_controller::SlotFactory>,
    #[cfg(feature = "test-support")]
    events: std::collections::VecDeque<JobEvent>,
}
impl Default for Jobs {
    fn default() -> Self {
        Self::with_reader(Arc::new(read_configuration))
    }
}

impl Jobs {
    pub(super) fn with_reader(reader: Reader) -> Self {
        Self {
            read: None,
            reader,
            controller: None,
            control: None,
            retry: None,
            reads: 0,
            execution: None,
            person: None,
            failing: false,
            unsettled: None,
            replay_seen: false,
            held: false,
            unavailable: false,
            closing: None,
            waits: Vec::new(),
            owner_events: Vec::new(),
            #[cfg(test)]
            slots: None,
            #[cfg(feature = "test-support")]
            events: std::collections::VecDeque::new(),
        }
    }
    /// Owner unit tests: the controller's slots come from `factory`.
    #[cfg(test)]
    pub(super) fn set_slots(&mut self, factory: crate::job_controller::SlotFactory) {
        self.slots = Some(factory);
    }

    /// Starts one read of the job limits and the execution setting: at
    /// service start, and then after each paced delay while reads fail.
    pub(super) fn start(&mut self, runtime: &RuntimeDirectory, wake: Option<&WakeSender>) {
        if self.read.is_some() || self.controller.is_some() || self.closing.is_some() {
            return;
        }
        self.reads += 1;
        let cancel = Arc::new(AtomicBool::new(false));
        let result: ReadSlot = Arc::new(Mutex::new(None));
        let (runtime, token, slot, wake, reader) = (
            runtime.clone(),
            cancel.clone(),
            result.clone(),
            wake.cloned(),
            self.reader.clone(),
        );
        let thread = std::thread::Builder::new()
            .name("asura-job-limits".into())
            .spawn(move || {
                let read = reader(runtime, &token);
                *slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(read);
                if let Some(wake) = wake {
                    let _ = wake.notify();
                }
            });
        match thread {
            Ok(thread) => {
                self.read = Some(LimitsRead {
                    cancel,
                    result,
                    _thread: thread,
                })
            }
            Err(_) => self.failed(Instant::now()),
        }
    }

    /// A read failed: log at the first failure of a run, then wait the
    /// next paced delay (1, 1, 2, 3, 5 ... s, at most 60 s).
    fn failed(&mut self, now: Instant) {
        if self.closing.is_some() {
            return;
        }
        if !std::mem::replace(&mut self.failing, true) {
            tracing::warn!("job_limits_unavailable");
        }
        match &mut self.retry {
            Some(pacing) => pacing.advance(now),
            None => self.retry = Some(Pacing::start(now)),
        }
    }

    /// Whether the start-time replay was taken.
    pub(super) fn replay_taken(&self) -> bool {
        self.replay_seen
    }
    /// The start-time replay's unsettled jobs (the first replay of this
    /// run, after Open or the automatic initialization): each gets an
    /// explicit `interrupted` settlement before admission opens.
    /// `outside` holds the inputs whose kind 31 has field 7 bit 1, so an
    /// interrupted `!!` job's `job.settled` carries `outside_sandbox`
    /// (PCR25, `recover_profiles`).
    pub(super) fn replayed(
        &mut self,
        unsettled: Vec<journal::UnsettledJob>,
        outside: std::collections::BTreeSet<Id>,
    ) {
        if !std::mem::replace(&mut self.replay_seen, true) {
            self.unsettled = Some((unsettled, outside));
        }
    }

    /// Takes a finished read: builds the controller and the handler, or
    /// schedules the next read; starts a due read; then polls the
    /// controller with the payload port and collects its ended waits.
    pub(super) fn poll(
        &mut self,
        now: Instant,
        runtime: &RuntimeDirectory,
        wake: Option<&WakeSender>,
        emitter: Option<&crate::audit::Emitter>,
        payloads: &mut Payloads,
    ) {
        if let Some(read) = &self.read {
            let taken = read.result.lock().unwrap_or_else(|e| e.into_inner()).take();
            match taken {
                Some(Ok(Started {
                    limits,
                    execution,
                    outside,
                    declared,
                    store,
                })) => {
                    self.read = None;
                    self.retry = None;
                    if std::mem::replace(&mut self.failing, false) {
                        tracing::info!("job_limits_read");
                    }
                    payloads.set_model_reserved(limits.model_reserved_page_reads);
                    #[cfg(test)]
                    let slots = self
                        .slots
                        .take()
                        .unwrap_or_else(|| crate::exec_cmd_worker::slot_factory(wake.cloned()));
                    #[cfg(not(test))]
                    let slots = crate::exec_cmd_worker::slot_factory(wake.cloned());
                    let mut controller =
                        JobController::new(limits, emitter.cloned(), runtime.clone(), slots);
                    if self.unavailable {
                        controller.journal_unavailable();
                    }
                    if self.closing.is_some() {
                        controller.close();
                    }
                    self.control = Some(JobControl::new(limits.client_viewers));
                    self.controller = Some(controller);
                    self.execution = Some(execution);
                    self.person = Some((outside, declared, store));
                }
                Some(Err(())) => {
                    self.read = None;
                    self.failed(now);
                }
                None => {}
            }
        }
        if self.controller.is_none()
            && self.read.is_none()
            && self.closing.is_none()
            && self.retry.is_some_and(|r| now >= r.at())
        {
            self.start(runtime, wake);
        }
        let Some(controller) = &mut self.controller else {
            return;
        };
        if let Some((unsettled, outside)) = self.unsettled.take() {
            controller.recover_profiles(unsettled, &|input| outside.contains(&input));
        }
        controller.poll(now, payloads);
        controller.drain_waits(&mut self.waits);
        let mut events: Vec<JobEvent> = Vec::new();
        loop {
            let before = events.len();
            controller.drain_events(&mut events);
            if events.len() == before {
                break;
            }
        }
        self.owner_events.extend(events.iter().copied());
        #[cfg(feature = "test-support")]
        for event in events {
            if self.events.len() == KEPT_EVENTS {
                self.events.pop_front();
            }
            self.events.push_back(event);
        }
    }
    /// The controller events since the last call, oldest first.
    pub(super) fn take_owner_events(&mut self) -> Vec<JobEvent> {
        std::mem::take(&mut self.owner_events)
    }

    /// The execution setting of the successful read, once.
    pub(super) fn take_execution(&mut self) -> Option<ExecutionSetting> {
        self.execution.take()
    }
    /// The outside switch and the secret store target of the successful
    /// read, once.
    pub(super) fn take_person(&mut self) -> Option<Person> {
        self.person.take()
    }
    /// Ended controller waits since the last call, oldest first.
    pub(super) fn take_waits(&mut self) -> Vec<WaitDone> {
        std::mem::take(&mut self.waits)
    }

    /// The writer's hold state at this poll; the end of a hold lets held
    /// kind 26 records go again.
    pub(super) fn hold(&mut self, held: bool) {
        let ended = self.held && !held;
        self.held = held;
        if ended && let Some(controller) = &mut self.controller {
            controller.journal_hold_ended();
        }
    }

    /// The owner became storage-unavailable: live jobs get `T`, nothing
    /// starts, the outbox is dropped (RV1).
    pub(super) fn journal_unavailable(&mut self) {
        if std::mem::replace(&mut self.unavailable, true) {
            return;
        }
        if let Some(controller) = &mut self.controller {
            controller.journal_unavailable();
        }
    }

    /// An identity recheck found the project replaced (J2-E10).
    pub(super) fn identity_changed(&mut self, project: Id) {
        if let Some(controller) = &mut self.controller {
            controller.revoke_project(project, JobReason::IdentityChanged);
        }
    }

    /// Shutdown (AE-06): admission closes, queued starts settle, live jobs
    /// get `T`; the limits read is cancelled and no retry starts.
    pub(super) fn close(&mut self, now: Instant) {
        self.closing.get_or_insert(now);
        self.retry = None;
        if let Some(read) = &self.read {
            read.cancel.store(true, Ordering::Release);
        }
        if let Some(controller) = &mut self.controller {
            controller.close();
        }
    }

    /// The next job record for the owner's ticket: kinds 28 and 27, then
    /// kind 26.
    pub(super) fn next_record(&mut self) -> Option<(Id, journal::Record)> {
        self.controller.as_mut()?.next_record()
    }

    pub(super) fn record_done(&mut self, job: Id, result: writer::Result<u64>) {
        if let Some(controller) = &mut self.controller {
            controller.record_done(job, result);
        }
    }

    pub(super) fn interests(&self, out: &mut Vec<PollInterest>) {
        if let Some(controller) = &self.controller {
            controller.interests(out);
        }
    }

    pub(super) fn next_deadline(&self, now: Instant) -> Option<Instant> {
        let bound = self
            .closing
            .filter(|_| !self.settled(now))
            .map(|started| started + SHUTDOWN_BOUND);
        let retry = self
            .retry
            .filter(|_| self.read.is_none() && self.controller.is_none() && self.closing.is_none())
            .map(|r| r.at());
        self.controller
            .as_ref()
            .and_then(|c| c.next_deadline(now))
            .into_iter()
            .chain(bound)
            .chain(retry)
            .min()
    }

    /// Shutdown may close the writer: no live slot and no unsent closure
    /// record, or the 2 s bound passed. The lease EOF stops what remains
    /// and the next start writes `interrupted`.
    pub(super) fn settled(&self, now: Instant) -> bool {
        self.read.is_none() && self.controller.as_ref().is_none_or(JobController::settled)
            || self
                .closing
                .is_some_and(|started| now >= started + SHUTDOWN_BOUND)
    }

    pub(super) fn controller(&mut self) -> Option<&mut JobController> {
        self.controller.as_mut()
    }
    pub(super) fn controller_ref(&self) -> Option<&JobController> {
        self.controller.as_ref()
    }
    /// The controller and the handler together, for the handler's context.
    pub(super) fn parts(&mut self) -> Option<(&mut JobController, &mut JobControl)> {
        Some((self.controller.as_mut()?, self.control.as_mut()?))
    }
    /// No controller yet: a start-time read is pending or retrying.
    #[cfg(test)]
    pub(super) fn awaiting_controller(&self) -> bool {
        self.controller.is_none() && self.closing.is_none()
    }
    #[cfg(test)]
    pub(super) fn reads(&self) -> u32 {
        self.reads
    }
    #[cfg(feature = "test-support")]
    pub(super) fn take_events(&mut self) -> Vec<JobEvent> {
        self.events.drain(..).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicU32;
    use std::time::Duration;

    fn scratch() -> (std::path::PathBuf, RuntimeDirectory) {
        use std::os::unix::fs::DirBuilderExt;
        let path = std::path::PathBuf::from(format!(
            "/private/tmp/asura-jobs-{:x}",
            u128::from_ne_bytes(asura_platform::random_id())
        ));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .unwrap();
        let runtime = RuntimeDirectory::scratch(&path, true).unwrap();
        (path, runtime)
    }
    fn settle(jobs: &mut Jobs, runtime: &RuntimeDirectory, now: Instant) {
        let mut payloads = Payloads::default();
        while jobs.read.is_some() {
            jobs.poll(now, runtime, None, None, &mut payloads);
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    /// TJ-F5 (unit): a read that fails twice and then succeeds gives reads
    /// after 1 s and 1 s, one at a time, and a controller after the third.
    #[test]
    fn tj_f5_failed_reads_retry_after_paced_delays() {
        let (path, runtime) = scratch();
        let calls = Arc::new(AtomicU32::new(0));
        let counter = calls.clone();
        let mut jobs = Jobs::with_reader(Arc::new(move |_, _| {
            if counter.fetch_add(1, Ordering::AcqRel) < 2 {
                Err(())
            } else {
                Ok(Started::defaults())
            }
        }));
        let start = Instant::now();
        jobs.start(&runtime, None);
        settle(&mut jobs, &runtime, start);
        assert!(jobs.controller.is_none() && jobs.awaiting_controller());
        assert_eq!(jobs.retry.unwrap().at(), start + Duration::from_secs(1));
        assert_eq!(
            jobs.next_deadline(start),
            Some(start + Duration::from_secs(1))
        );
        // Not due: no second read.
        let mut payloads = Payloads::default();
        jobs.poll(start, &runtime, None, None, &mut payloads);
        assert_eq!(jobs.reads(), 1);
        let second = start + Duration::from_secs(1);
        jobs.poll(second, &runtime, None, None, &mut payloads);
        assert_eq!(jobs.reads(), 2);
        // One read at a time: a due poll while it runs starts none.
        jobs.poll(
            second + Duration::from_secs(5),
            &runtime,
            None,
            None,
            &mut payloads,
        );
        assert!(jobs.reads() == 2 || jobs.read.is_none());
        settle(&mut jobs, &runtime, second);
        assert_eq!(jobs.retry.unwrap().at(), second + Duration::from_secs(1));
        let third = second + Duration::from_secs(1);
        jobs.poll(third, &runtime, None, None, &mut payloads);
        assert_eq!(jobs.reads(), 3);
        settle(&mut jobs, &runtime, third);
        jobs.poll(third, &runtime, None, None, &mut payloads);
        assert!(jobs.controller.is_some() && jobs.control.is_some());
        assert_eq!(jobs.take_execution(), Some(ExecutionSetting::default()));
        assert!(jobs.retry.is_none() && !jobs.failing);
        assert_eq!(calls.load(Ordering::Acquire), 3);
        let _ = std::fs::remove_dir_all(path);
    }
    /// TJ-F5 (unit): shutdown during a retry stops the retries; the
    /// running read is cancelled.
    #[test]
    fn tj_f5_shutdown_stops_the_retry() {
        let (path, runtime) = scratch();
        let mut jobs = Jobs::with_reader(Arc::new(|_, cancel| {
            while !cancel.load(Ordering::Acquire) {
                std::thread::sleep(Duration::from_millis(1));
            }
            Err(())
        }));
        let start = Instant::now();
        jobs.start(&runtime, None);
        jobs.close(start);
        settle(&mut jobs, &runtime, start);
        let mut payloads = Payloads::default();
        jobs.poll(
            start + Duration::from_secs(120),
            &runtime,
            None,
            None,
            &mut payloads,
        );
        assert_eq!(jobs.reads(), 1);
        assert!(jobs.controller.is_none() && !jobs.awaiting_controller());
        assert_eq!(
            jobs.next_deadline(start + Duration::from_secs(120)),
            None,
            "no retry deadline after shutdown"
        );
        let _ = std::fs::remove_dir_all(path);
    }
}
