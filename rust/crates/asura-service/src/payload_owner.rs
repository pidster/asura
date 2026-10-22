//! The conversation owner's payload state: the retained recovery worker,
//! the ready store and page-read pool, and the pending-install list of at
//! most 4 capture workers that outlive their job. It is the job
//! controller's payload port (J2): admission, release, settlement and
//! abandonment of each job's reservation and capture worker.
use crate::capture_worker::{self, Message};
use crate::job_controller::{Admitted, Origin, PayloadPort, Short};
use asura_platform::RuntimeDirectory;
use asura_platform::events::WakeSender;
use asura_storage::authority::conversation::{
    CaptureState, Id, JobCaptureState, JobSettled, Replay,
};
use asura_storage::authority::writer::{self, WriterClient};
use asura_storage::payload::{
    PagePool, PayloadStore, RecoveryError, RecoveryReport, SpaceListener, Victim,
    recover_with_facts,
};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// One `PayloadFacts` request from the recovery worker. On a full writer
/// queue it waits for the queue-room event (J1-0 queue room wait): a job
/// settles, the writer closes (`Journal`) or recovery is cancelled
/// (`Cancelled`). No time bound and no polling.
fn journal_facts(
    client: &WriterClient,
    payloads: Vec<[u8; 16]>,
    cancel: &AtomicBool,
) -> Result<Vec<writer::PayloadFact>, RecoveryError> {
    loop {
        if cancel.load(Ordering::Acquire) {
            return Err(RecoveryError::Cancelled);
        }
        match client.try_submit(writer::Command::PayloadFacts {
            payloads: payloads.clone(),
        }) {
            Ok(ticket) => {
                return ticket
                    .wait()
                    .map(|reply| reply.payload_facts)
                    .map_err(|_| RecoveryError::Journal);
            }
            Err(writer::Error::Busy) => match client.wait_for_room(cancel) {
                Ok(()) => {}
                Err(writer::Error::Cancelled) => return Err(RecoveryError::Cancelled),
                Err(_) => return Err(RecoveryError::Journal),
            },
            Err(_) => return Err(RecoveryError::Journal),
        }
    }
}
/// The recovery worker's cancel: sets the flag, then wakes a queue-room
/// wait so cancellation never depends on an unrelated job settling.
struct RecoveryCancel {
    flag: Arc<AtomicBool>,
    client: WriterClient,
}
impl RecoveryCancel {
    fn cancel(&self) {
        self.flag.store(true, Ordering::Release);
        self.client.wake_room_waiters();
    }
}
/// Capture workers kept after their job: installing or abandoned.
pub(crate) const PENDING_INSTALLS: usize = 4;
/// Shutdown cleanup bound for pending installs and live captures.
pub(crate) const SHUTDOWN_BOUND: Duration = Duration::from_secs(2);

/// The recovered store, its report and the configured `retention.quotaBytes`.
type Recovered = Result<(Arc<PayloadStore>, RecoveryReport, u64), &'static str>;

/// Counts the ledger's space events (settlement, expiry, deletion) for the
/// job controller's `space_epoch` and wakes the reactor. The ledger calls it
/// outside its lock; it does one atomic add and one nonblocking wake.
struct SpaceEpoch {
    count: AtomicU64,
    wake: Option<WakeSender>,
}
impl SpaceListener for SpaceEpoch {
    fn space_event(&self) {
        self.count.fetch_add(1, Ordering::AcqRel);
        if let Some(wake) = &self.wake {
            let _ = wake.notify();
        }
    }
}

enum State {
    Idle,
    Recovering {
        cancel: RecoveryCancel,
        result: Arc<Mutex<Option<Recovered>>>,
        _thread: JoinHandle<()>,
    },
    Ready {
        store: Arc<PayloadStore>,
        _pages: Option<PagePool>,
        /// The configured `retention.quotaBytes`: an effective quota below
        /// it means free disk space limits the store.
        configured: u64,
        /// Kept alive while the ledger holds its weak reference.
        epoch: Arc<SpaceEpoch>,
    },
    Unavailable,
}

pub(crate) struct Payloads {
    state: State,
    pending: Vec<capture_worker::Handle>,
    shutdown: Option<Instant>,
    /// The installation ID from replay: the payload ID derivation input.
    installation: Id,
    wake: Option<WakeSender>,
}
impl Default for Payloads {
    fn default() -> Self {
        Self {
            state: State::Idle,
            pending: Vec::new(),
            shutdown: None,
            installation: [0; 16],
            wake: None,
        }
    }
}
impl Payloads {
    /// Starts the retained recovery worker once a replay exists. Recovery has
    /// no time bound; shutdown cancels it. It also warms the secret
    /// detection rule set off the reactor.
    pub(crate) fn ensure_recovery(
        &mut self,
        runtime: &RuntimeDirectory,
        replay: Option<&Arc<Replay>>,
        client: Option<WriterClient>,
        wake: Option<&WakeSender>,
    ) {
        if !matches!(self.state, State::Idle) || self.shutdown.is_some() {
            return;
        }
        let (Some(replay), Some(client)) = (replay, client) else {
            return;
        };
        self.installation = replay.installation_id;
        self.wake = wake.cloned();
        // The worker asks the journal for the facts of the payloads on disk
        // in batches of 128 (`PayloadFacts`); it never holds a snapshot.
        let runtime = runtime.clone();
        let cancel = Arc::new(AtomicBool::new(false));
        let result: Arc<Mutex<Option<Recovered>>> = Arc::new(Mutex::new(None));
        let cancel = RecoveryCancel {
            flag: cancel,
            client: client.clone(),
        };
        let (token, slot, wake) = (cancel.flag.clone(), result.clone(), wake.cloned());
        let thread = std::thread::Builder::new()
            .name("asura-payload-recovery".into())
            .spawn(move || {
                let _ = asura_platform::secrets::detection();
                let outcome = (|| {
                    let quota = asura_storage::config::retention_quota(runtime.clone(), &token)
                        .map_err(|_| "payload_config_unavailable")?;
                    let store = PayloadStore::open(&runtime).map_err(|_| "payload_unavailable")?;
                    let mut facts =
                        |payloads: Vec<[u8; 16]>| journal_facts(&client, payloads, &token);
                    let report = recover_with_facts(&store, &mut facts, quota, &token)
                        .map_err(|_| "payload_unavailable")?;
                    Ok((Arc::new(store), report, quota))
                })();
                *slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(outcome);
                if let Some(wake) = wake {
                    let _ = wake.notify();
                }
            });
        self.state = match thread {
            Ok(thread) => State::Recovering {
                cancel,
                result,
                _thread: thread,
            },
            Err(_) => State::Unavailable,
        };
    }
    /// The ready store, if recovery finished.
    pub(crate) fn store(&self) -> Option<&Arc<PayloadStore>> {
        match &self.state {
            State::Ready { store, .. } => Some(store),
            _ => None,
        }
    }
    /// Collects recovery and drains pending workers; an exited worker leaves
    /// the list and releases its hold.
    pub(crate) fn poll(&mut self) {
        if let State::Recovering { result, .. } = &self.state {
            let taken = result.lock().unwrap_or_else(|e| e.into_inner()).take();
            if let Some(outcome) = taken {
                self.state = match outcome {
                    Ok((store, report, configured)) => {
                        tracing::debug!(
                            installed = report.installed,
                            corrupt = report.corrupt,
                            missing = report.missing,
                            unverified = report.unverified,
                            orphans = report.orphans_deleted,
                            strays = report.strays_deleted,
                            retained = report.retained,
                            quota = store.quota(),
                            "payload_recovery_complete"
                        );
                        let pages = PagePool::start(store.clone()).ok();
                        let epoch = Arc::new(SpaceEpoch {
                            count: AtomicU64::new(0),
                            wake: self.wake.clone(),
                        });
                        store.listen(&(epoch.clone() as Arc<dyn SpaceListener>));
                        State::Ready {
                            store,
                            _pages: pages,
                            configured,
                            epoch,
                        }
                    }
                    Err(reason) => {
                        // Shutdown cancels recovery; that is not a fault.
                        if self.shutdown.is_none() {
                            tracing::warn!(reason, "payload_store_unavailable");
                        } else {
                            tracing::debug!(reason, "payload_recovery_cancelled");
                        }
                        State::Unavailable
                    }
                };
            }
        }
        let store = self.store().cloned();
        self.pending.retain_mut(|handle| {
            for message in handle.drain() {
                if matches!(message, Message::InstallPending) {
                    tracing::warn!("payload_install_pending_recovery");
                }
            }
            if handle.exited {
                if let Some(store) = &store {
                    store.unhold(handle.job);
                }
                false
            } else {
                true
            }
        });
    }
    pub(crate) fn shutdown(&mut self, now: Instant) {
        self.shutdown.get_or_insert(now);
        if let State::Recovering { cancel, .. } = &self.state {
            cancel.cancel();
        }
    }
    /// Settled when the list is empty, or the 2 s shutdown bound has passed:
    /// recovery completes any install that has not finished.
    pub(crate) fn settled(&self, now: Instant) -> bool {
        self.pending.is_empty()
            || self
                .shutdown
                .is_some_and(|started| now >= started + SHUTDOWN_BOUND)
    }
    pub(crate) fn next_deadline(&self) -> Option<Instant> {
        if self.pending.is_empty() {
            return None;
        }
        self.shutdown.map(|started| started + SHUTDOWN_BOUND)
    }
}
/// The job controller's admission (conditions 4, 7 and 8) and the
/// settlement of each job's reservation and capture worker. Every method is
/// arithmetic on the ledger or a flag on a worker: nothing blocks (AE-01).
impl PayloadPort for Payloads {
    fn ready(&self) -> bool {
        self.store().is_some()
    }
    fn pending_installs(&self) -> usize {
        self.pending.len()
    }
    fn space_epoch(&self) -> u64 {
        match &self.state {
            State::Ready { epoch, .. } => epoch.count.load(Ordering::Acquire),
            _ => 0,
        }
    }
    /// Reserves 1 MiB after an expiry attempt. A short is `DiskSpace` when
    /// free space, not the configured quota, sets the effective quota.
    fn admit(&mut self, job: Id, origin: &Origin) -> Result<Admitted, Short> {
        let State::Ready {
            store, configured, ..
        } = &self.state
        else {
            return Err(Short::Quota);
        };
        let owner = match *origin {
            Origin::Model { operation, .. } => operation,
            Origin::Person { input } => input,
        };
        match store.admit(self.installation, job, owner) {
            Ok(victims) => Ok(Admitted {
                store: store.clone(),
                victims,
            }),
            Err(_) => Err(short(store.quota(), *configured)),
        }
    }
    fn release(&mut self, job: Id, victims: &[Victim]) {
        if let Some(store) = self.store() {
            store.restore_victims(victims);
            store.release(job);
        }
    }
    /// Kind 28 committed: the ledger settles (or releases a job that never
    /// staged), then the worker installs (R2), discards its `not_started`
    /// staging, or stays abandoned after `unconfirmed`; it joins the
    /// pending-install list until it exits.
    fn committed(
        &mut self,
        job: Id,
        settled: &JobSettled,
        sequence: u64,
        capture: Option<capture_worker::Handle>,
    ) {
        let state = ledger_state(settled.capture_state);
        if let Some(store) = self.store() {
            match state {
                Some(state) => store.settle(
                    job,
                    state,
                    [
                        (
                            settled.stdout.capture.stored_bytes,
                            settled.stdout.capture.digest,
                        ),
                        (
                            settled.stderr.capture.stored_bytes,
                            settled.stderr.capture.digest,
                        ),
                    ],
                    sequence,
                    capture.is_some(),
                ),
                None => store.release(job),
            }
        }
        if let Some(handle) = capture {
            match state {
                Some(CaptureState::Unconfirmed) => handle.abandon(),
                Some(_) => handle.install(),
                None => handle.discard(),
            }
            self.pending.push(handle);
        }
    }
    /// The journal became unavailable before kind 28: no settlement. The
    /// worker stops and keeps its staging as an unverified prefix for the
    /// next start's recovery.
    fn abandon(&mut self, _job: Id, capture: Option<capture_worker::Handle>) {
        if let Some(handle) = capture {
            handle.abandon();
            self.pending.push(handle);
        }
    }
}

/// Condition 8's short: the effective quota below the configured one means
/// free disk space limits the store (`disk_space`); otherwise `quota`.
fn short(effective: u64, configured: u64) -> Short {
    if effective < configured {
        Short::DiskSpace
    } else {
        Short::Quota
    }
}

/// The ledger state of a kind 28 capture state; `None` for a job that
/// staged nothing (`not_started`, `interrupted`).
fn ledger_state(state: JobCaptureState) -> Option<CaptureState> {
    match state {
        JobCaptureState::Complete => Some(CaptureState::Complete),
        JobCaptureState::Incomplete => Some(CaptureState::Incomplete),
        JobCaptureState::Unconfirmed => Some(CaptureState::Unconfirmed),
        JobCaptureState::NotStarted | JobCaptureState::Interrupted => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use asura_storage::authority::writer::test_support::detached_queue;
    use std::sync::mpsc;
    use std::thread;

    /// Condition 8: the payload owner decides `quota` or `disk_space`
    /// from the effective and configured quota (J2 interfaces).
    #[test]
    fn short_is_disk_space_only_when_free_space_sets_the_quota() {
        assert_eq!(short(64 << 20, 64 << 20), Short::Quota);
        assert_eq!(short(10 << 20, 64 << 20), Short::DiskSpace);
        assert_eq!(short(0, 64 << 20), Short::DiskSpace);
    }
    /// Kind 28 capture states map to the ledger's: a job that staged
    /// nothing releases its reservation instead of settling.
    #[test]
    fn job_capture_states_map_to_the_ledger() {
        assert_eq!(
            ledger_state(JobCaptureState::Complete),
            Some(CaptureState::Complete)
        );
        assert_eq!(
            ledger_state(JobCaptureState::Incomplete),
            Some(CaptureState::Incomplete)
        );
        assert_eq!(
            ledger_state(JobCaptureState::Unconfirmed),
            Some(CaptureState::Unconfirmed)
        );
        assert_eq!(ledger_state(JobCaptureState::NotStarted), None);
        assert_eq!(ledger_state(JobCaptureState::Interrupted), None);
    }
    /// The space epoch counts each ledger space event.
    #[test]
    fn space_epoch_counts_ledger_space_events() {
        let epoch = Arc::new(SpaceEpoch {
            count: AtomicU64::new(0),
            wake: None,
        });
        let ledger = asura_storage::payload::Ledger::default();
        ledger.listen(&(epoch.clone() as Arc<dyn SpaceListener>));
        ledger.space_event();
        ledger.release([1; 16]);
        assert_eq!(epoch.count.load(Ordering::Acquire), 2);
    }
    type Facts = Result<Vec<writer::PayloadFact>, RecoveryError>;
    fn fact(payload: [u8; 16]) -> writer::PayloadFact {
        writer::PayloadFact {
            payload,
            call: [2; 16],
            operation: [3; 16],
            payloads: [payload, [4; 16]],
            intent_sequence: 5,
            settled: None,
            active: false,
            pinned: false,
            deleted: false,
        }
    }
    /// Six ordinary requests fill the queue; returns their tickets.
    fn fill(client: &WriterClient) -> Vec<writer::Ticket> {
        let tickets: Vec<_> = (0..6)
            .map(|_| client.try_submit(writer::Command::Open).unwrap())
            .collect();
        assert!(matches!(
            client.try_submit(writer::Command::Open),
            Err(writer::Error::Busy)
        ));
        tickets
    }
    fn spawn_facts(client: WriterClient, cancel: Arc<AtomicBool>) -> mpsc::Receiver<Facts> {
        let (sender, result) = mpsc::channel();
        thread::spawn(move || {
            sender
                .send(journal_facts(&client, vec![[1; 16]], &cancel))
                .unwrap();
        });
        result
    }
    fn still_waiting(result: &mpsc::Receiver<Facts>) -> bool {
        matches!(
            result.recv_timeout(Duration::from_millis(200)),
            Err(mpsc::RecvTimeoutError::Timeout)
        )
    }
    /// J1-0-10: a full queue holds the request until a job settles; then it
    /// is admitted and answered.
    #[test]
    fn j1_0_10_full_queue_facts_complete_once_room_appears() {
        let (client, queue) = detached_queue();
        let _tickets = fill(&client);
        let result = spawn_facts(client.clone(), Arc::default());
        assert!(still_waiting(&result));
        for _ in 0..6 {
            assert!(queue.answer_next(|command| {
                assert!(matches!(command, writer::Command::Open));
                Ok(writer::Reply::default())
            }));
        }
        assert!(queue.answer_next(|command| {
            let writer::Command::PayloadFacts { payloads } = command else {
                panic!("expected PayloadFacts, got {command:?}");
            };
            assert_eq!(payloads, vec![[1; 16]]);
            Ok(writer::Reply {
                payload_facts: vec![fact([1; 16])],
                ..Default::default()
            })
        }));
        let facts = result.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(facts.unwrap(), vec![fact([1; 16])]);
    }
    /// J1-0-10: shutdown's recovery cancel wakes the wait although no job
    /// settles.
    #[test]
    fn j1_0_10_recovery_cancel_wakes_a_full_queue_wait() {
        let (client, _queue) = detached_queue();
        let _tickets = fill(&client);
        let cancel = RecoveryCancel {
            flag: Arc::default(),
            client: client.clone(),
        };
        let result = spawn_facts(client, cancel.flag.clone());
        assert!(still_waiting(&result));
        cancel.cancel();
        assert!(matches!(
            result.recv_timeout(Duration::from_secs(5)).unwrap(),
            Err(RecoveryError::Cancelled)
        ));
    }
    /// J1-0-10: the writer's close ends a full-queue wait with `Journal`.
    #[test]
    fn j1_0_10_writer_close_ends_a_full_queue_wait() {
        let (client, queue) = detached_queue();
        let _tickets = fill(&client);
        let result = spawn_facts(client, Arc::default());
        assert!(still_waiting(&result));
        queue.close();
        assert!(matches!(
            result.recv_timeout(Duration::from_secs(5)).unwrap(),
            Err(RecoveryError::Journal)
        ));
    }
}
