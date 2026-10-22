//! The conversation owner's job wiring (tool-job-controller.md, J2): the
//! start-time read of the job limits, the job controller beside `Payloads`,
//! start-time recovery, the journal hold and outage, revocation and the
//! shutdown bound. The owner appends the controller's records through its
//! serialized journal ticket; this module never submits to the writer.
use crate::job_controller::{JobController, JobEvent};
use crate::payload_owner::{Payloads, SHUTDOWN_BOUND};
use asura_platform::events::WakeSender;
use asura_platform::{PollInterest, RuntimeDirectory};
use asura_storage::authority::conversation::{self as journal, Id, JobReason};
use asura_storage::authority::writer;
use asura_storage::config::JobLimits;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Instant;

/// Controller events kept for a test reader (`test-support` only). J3 and
/// J4 consume them; until then the owner drains and drops them.
#[cfg(feature = "test-support")]
const KEPT_EVENTS: usize = 1024;

type LimitsResult = Arc<Mutex<Option<Result<JobLimits, ()>>>>;

/// The start-time read of `jobs.*` on a retained worker thread (AE-02:
/// the configuration file read blocks). It has no time bound; shutdown
/// cancels it.
struct LimitsRead {
    cancel: Arc<AtomicBool>,
    result: LimitsResult,
    _thread: JoinHandle<()>,
}

#[derive(Default)]
pub(super) struct Jobs {
    read: Option<LimitsRead>,
    /// The controller, once the limits are read; never rebuilt.
    controller: Option<JobController>,
    /// The limits read failed: no controller in this run.
    failed: bool,
    /// The unsettled jobs from the start-time replay, until the controller
    /// takes them (J2-E5).
    unsettled: Option<Vec<journal::UnsettledJob>>,
    replay_seen: bool,
    /// The journal held normal records at the last poll.
    held: bool,
    /// The journal became unavailable; reported once.
    unavailable: bool,
    /// Shutdown began: the 2 s cleanup bound runs from here (AE-06).
    closing: Option<Instant>,
    #[cfg(feature = "test-support")]
    events: std::collections::VecDeque<JobEvent>,
}

impl Jobs {
    /// Starts the one read of the job limits at service start.
    pub(super) fn start(&mut self, runtime: &RuntimeDirectory, wake: Option<&WakeSender>) {
        if self.read.is_some() || self.controller.is_some() || self.failed {
            return;
        }
        let cancel = Arc::new(AtomicBool::new(false));
        let result: LimitsResult = Arc::new(Mutex::new(None));
        let (runtime, token, slot, wake) = (
            runtime.clone(),
            cancel.clone(),
            result.clone(),
            wake.cloned(),
        );
        let thread = std::thread::Builder::new()
            .name("asura-job-limits".into())
            .spawn(move || {
                let limits = asura_storage::config::job_limits(runtime, &token).map_err(|_| ());
                *slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(limits);
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
            Err(_) => {
                self.failed = true;
                tracing::warn!("job_limits_unavailable");
            }
        }
    }

    /// Whether the start-time replay was taken.
    pub(super) fn replay_taken(&self) -> bool {
        self.replay_seen
    }
    /// The start-time replay's unsettled jobs (the first replay of this
    /// run, after Open or the automatic initialization): each gets an
    /// explicit `interrupted` settlement before admission opens.
    pub(super) fn replayed(&mut self, unsettled: Vec<journal::UnsettledJob>) {
        if !std::mem::replace(&mut self.replay_seen, true) {
            self.unsettled = Some(unsettled);
        }
    }

    /// Builds the controller once the limits are known, hands it the
    /// start-time recovery, and polls it with the payload port.
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
                Some(Ok(limits)) => {
                    self.read = None;
                    let mut controller = JobController::new(
                        limits,
                        emitter.cloned(),
                        runtime.clone(),
                        crate::exec_cmd_worker::slot_factory(wake.cloned()),
                    );
                    if self.unavailable {
                        controller.journal_unavailable();
                    }
                    if self.closing.is_some() {
                        controller.close();
                    }
                    self.controller = Some(controller);
                }
                Some(Err(())) => {
                    self.read = None;
                    self.failed = true;
                    tracing::warn!("job_limits_unavailable");
                }
                None => {}
            }
        }
        let Some(controller) = &mut self.controller else {
            return;
        };
        if let Some(unsettled) = self.unsettled.take() {
            controller.recover(unsettled);
        }
        controller.poll(now, payloads);
        let mut events: Vec<JobEvent> = Vec::new();
        loop {
            let before = events.len();
            controller.drain_events(&mut events);
            if events.len() == before {
                break;
            }
        }
        #[cfg(feature = "test-support")]
        for event in events {
            if self.events.len() == KEPT_EVENTS {
                self.events.pop_front();
            }
            self.events.push_back(event);
        }
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
    /// get `T`; the limits read is cancelled.
    pub(super) fn close(&mut self, now: Instant) {
        self.closing.get_or_insert(now);
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
        self.controller
            .as_ref()
            .and_then(|c| c.next_deadline(now))
            .into_iter()
            .chain(bound)
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

    #[cfg(feature = "test-support")]
    pub(super) fn controller(&mut self) -> Option<&mut JobController> {
        self.controller.as_mut()
    }
    #[cfg(feature = "test-support")]
    pub(super) fn take_events(&mut self) -> Vec<JobEvent> {
        self.events.drain(..).collect()
    }
}
