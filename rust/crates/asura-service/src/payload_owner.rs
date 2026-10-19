//! The conversation owner's payload state (J1): the retained recovery worker,
//! the ready store and page-read pool, and the pending-install list of at
//! most 4 capture workers that outlive their call.
use crate::capture_worker::{self, Message};
use asura_platform::RuntimeDirectory;
use asura_platform::events::WakeSender;
use asura_storage::authority::conversation::{ExecCmdBusyReason, Replay};
use asura_storage::authority::writer::{self, WriterClient};
use asura_storage::payload::{
    PagePool, PayloadStore, RecoveryError, RecoveryReport, recover_with_facts,
};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// One `PayloadFacts` request from the recovery worker. A full writer
/// queue is retried with pacing; the reply or the writer's close ends the
/// wait (no time bound).
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
            Err(writer::Error::Busy) => std::thread::sleep(Duration::from_millis(10)),
            Err(_) => return Err(RecoveryError::Journal),
        }
    }
}
/// Capture workers kept after their call: installing or abandoned.
pub(crate) const PENDING_INSTALLS: usize = 4;
/// Shutdown cleanup bound for pending installs and live captures.
pub(crate) const SHUTDOWN_BOUND: Duration = Duration::from_secs(2);

type Recovered = Result<(Arc<PayloadStore>, RecoveryReport), &'static str>;

enum State {
    Idle,
    Recovering {
        cancel: Arc<AtomicBool>,
        result: Arc<Mutex<Option<Recovered>>>,
        _thread: JoinHandle<()>,
    },
    Ready {
        store: Arc<PayloadStore>,
        _pages: Option<PagePool>,
    },
    Unavailable,
}

pub(crate) struct Payloads {
    state: State,
    pending: Vec<capture_worker::Handle>,
    shutdown: Option<Instant>,
}
impl Default for Payloads {
    fn default() -> Self {
        Self {
            state: State::Idle,
            pending: Vec::new(),
            shutdown: None,
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
        let (Some(_), Some(client)) = (replay, client) else {
            return;
        };
        // The worker asks the journal for the facts of the payloads on disk
        // in batches of 128 (`PayloadFacts`); it never holds a snapshot.
        let runtime = runtime.clone();
        let cancel = Arc::new(AtomicBool::new(false));
        let result: Arc<Mutex<Option<Recovered>>> = Arc::new(Mutex::new(None));
        let (token, slot, wake) = (cancel.clone(), result.clone(), wake.cloned());
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
                    Ok((Arc::new(store), report))
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
    /// exec_cmd admission checks before the 1 MiB reservation, in order: a free
    /// pending-install slot, no paused capture, a ready payload store.
    pub(crate) fn admission(
        &self,
        active_paused: bool,
    ) -> Result<Arc<PayloadStore>, ExecCmdBusyReason> {
        if self.pending.len() >= PENDING_INSTALLS {
            return Err(ExecCmdBusyReason::PendingFull);
        }
        if active_paused || self.pending.iter().any(|h| h.paused) {
            return Err(ExecCmdBusyReason::CapturePaused);
        }
        self.store()
            .cloned()
            .ok_or(ExecCmdBusyReason::PayloadUnavailable)
    }
    /// Moves a worker that outlives its call onto the list.
    pub(crate) fn push_pending(&mut self, handle: capture_worker::Handle) {
        self.pending.push(handle);
    }
    /// Collects recovery and drains pending workers; an exited worker leaves
    /// the list and releases its hold.
    pub(crate) fn poll(&mut self) {
        if let State::Recovering { result, .. } = &self.state {
            let taken = result.lock().unwrap_or_else(|e| e.into_inner()).take();
            if let Some(outcome) = taken {
                self.state = match outcome {
                    Ok((store, report)) => {
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
                        State::Ready {
                            store,
                            _pages: pages,
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
                    store.unhold(handle.call);
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
            cancel.store(true, Ordering::Release);
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
