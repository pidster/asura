//! One bounded startup/status worker using the canonical client lifecycle.
use asura_client::{Client, Snapshot};
use asura_platform::RuntimeDirectory;
use std::io;
use std::sync::{
    Arc, Condvar, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime};

pub(super) type Resolver = fn(bool) -> asura_platform::Result<RuntimeDirectory>;
const BUILD: &str = concat!("asura/", env!("CARGO_PKG_VERSION"));
pub(super) struct Observation {
    generation: u64,
    configured_model: Option<String>,
    service: Option<super::ui::ServiceDetails>,
    started: Instant,
    result: Result<Snapshot, &'static str>,
}
pub(super) type LogResolver = fn() -> asura_platform::Result<std::fs::File>;
fn attach(
    resolve: Resolver,
    log_resolve: LogResolver,
    attempted: &mut bool,
    stop: &AtomicBool,
    lifetime: &asura_platform::ServiceLifetime,
) -> Result<(Client, bool), &'static str> {
    let cancelled = || stop.load(Ordering::Acquire);
    if cancelled() {
        return Err("cancelled");
    }
    let attached = resolve(false)
        .map_err(asura_client::Error::from)
        .and_then(|runtime| {
            if cancelled() {
                return Err(asura_client::Error::Unavailable);
            }
            Client::attach(&runtime, BUILD, Instant::now() + Duration::from_secs(2))
        });
    let client = match attached {
        Ok(client) => {
            // An existing backend retains its independent lifetime. Reconnect
            // after loss, but do not silently replace it with a TUI-owned child.
            *attempted = true;
            (client, false)
        }
        Err(error) if asura_client::is_absent(&error) && !*attempted && !cancelled() => {
            *attempted = true;
            let log = log_resolve().map_err(|_| "service_log_unavailable")?;
            if cancelled() {
                return Err("cancelled");
            }
            let runtime = resolve(true).map_err(|_| "service_unavailable")?;
            asura_client::start_owned(&runtime, BUILD, Some(&log), &cancelled, lifetime)
                .map_err(|error| crate::app::classify(&error).2)?
        }
        Err(error) => return Err(crate::app::classify(&error).2),
    };
    if cancelled() {
        return Err("cancelled");
    }
    Ok(client)
}
#[cfg(test)]
fn inspect(
    resolve: Resolver,
    log_resolve: LogResolver,
    attempted: &mut bool,
    stop: &AtomicBool,
    lifetime: &asura_platform::ServiceLifetime,
) -> Result<Snapshot, &'static str> {
    attach(resolve, log_resolve, attempted, stop, lifetime)?
        .0
        .inspect()
        .map_err(|error| crate::app::classify(&error).2)
}
struct Shared {
    latest: Mutex<Option<Observation>>,
    changed: Condvar,
    stop: AtomicBool,
}
pub(super) struct Worker {
    notice: super::events::Notice,
    shared: Arc<Shared>,
    handle: Option<JoinHandle<()>>,
    lease: Mutex<Option<asura_platform::ServiceLease>>,
}
impl Worker {
    pub(super) fn start(
        resolve: Resolver,
        log_resolve: LogResolver,
        notice: super::events::Notice,
    ) -> io::Result<Self> {
        let (lease, lifetime) = asura_platform::ServiceLifetime::new()
            .map_err(|error| io::Error::other(error.to_string()))?;
        let mut attempted = false;
        let mut client: Option<Client> = None;
        let mut revision = None;
        let mut owned_epoch = None;
        let mut spawned = false;
        let worker = Self::start_notified(
            move |stop| {
                if client.is_none() {
                    let attached = attach(resolve, log_resolve, &mut attempted, stop, &lifetime)?;
                    spawned = attached.1;
                    client = Some(attached.0);
                    revision = None;
                }
                match client.as_mut().unwrap().observe_service(revision) {
                    Ok(update) => {
                        revision = Some(update.revision);
                        if spawned {
                            owned_epoch = Some(update.snapshot.service_epoch);
                            spawned = false;
                        }
                        let details = super::ui::ServiceDetails {
                            owned: owned_epoch == Some(update.snapshot.service_epoch),
                            uptime_ms: update.uptime_ms,
                            stored_memory: update.stored_memory,
                        };
                        Ok((update.snapshot, update.configured_model, Some(details)))
                    }
                    Err(error) => {
                        client = None;
                        revision = None;
                        Err(crate::app::classify(&error).2)
                    }
                }
            },
            notice,
            false,
        )?;
        *worker.lease.lock().unwrap_or_else(|e| e.into_inner()) = Some(lease);
        Ok(worker)
    }
    #[cfg(test)]
    fn start_with<F>(mut inspect: F) -> io::Result<Self>
    where
        F: FnMut(&AtomicBool) -> Result<Snapshot, &'static str> + Send + 'static,
    {
        Self::start_notified(
            move |stop| inspect(stop).map(|snapshot| (snapshot, None, None)),
            Default::default(),
            true,
        )
    }
    fn start_notified<F>(
        mut inspect: F,
        notice: super::events::Notice,
        delay_success: bool,
    ) -> io::Result<Self>
    where
        F: FnMut(
                &AtomicBool,
            ) -> Result<
                (Snapshot, Option<String>, Option<super::ui::ServiceDetails>),
                &'static str,
            > + Send
            + 'static,
    {
        let shared = Arc::new(Shared {
            latest: Mutex::new(None),
            changed: Condvar::new(),
            stop: AtomicBool::new(false),
        });
        let worker = shared.clone();
        let notify = notice.clone();
        let handle = std::thread::Builder::new()
            .name("asura-tui-status".into())
            .spawn(move || {
                let _settlement = notify.guard();
                let mut generation = 0u64;
                while !worker.stop.load(Ordering::Acquire) {
                    let Some(next) = generation.checked_add(1) else {
                        break;
                    };
                    generation = next;
                    let started = Instant::now();
                    let (result, configured_model, service) = match inspect(&worker.stop) {
                        Ok((snapshot, selector, service)) => (Ok(snapshot), selector, service),
                        Err(error) => (Err(error), None, None),
                    };
                    if worker.stop.load(Ordering::Acquire) {
                        break;
                    }
                    let backoff = result.is_err() || delay_success;
                    let mut slot = worker.latest.lock().unwrap_or_else(|e| e.into_inner());
                    *slot = Some(Observation {
                        configured_model,
                        service,
                        generation,
                        started,
                        result,
                    });
                    notify.ready();
                    if backoff {
                        let _guard =
                            worker
                                .changed
                                .wait_timeout_while(slot, Duration::from_secs(1), |_| {
                                    !worker.stop.load(Ordering::Acquire)
                                });
                    }
                }
            })?;
        Ok(Self {
            notice,
            shared,
            handle: Some(handle),
            lease: Mutex::new(None),
        })
    }
    pub fn running(&self) -> bool {
        self.handle.as_ref().is_some_and(|h| !h.is_finished())
    }
    pub(super) fn take(&self) -> Option<Observation> {
        match self.shared.latest.try_lock() {
            Ok(mut slot) => slot.take(),
            Err(std::sync::TryLockError::Poisoned(error)) => error.into_inner().take(),
            Err(std::sync::TryLockError::WouldBlock) => {
                self.notice.ready();
                None
            }
        }
    }
    pub(super) fn cancel(&self) {
        self.lease.lock().unwrap_or_else(|e| e.into_inner()).take();
        self.shared.stop.store(true, Ordering::Release);
        // Cancellation must not wait for a producer before terminal restoration.
        // A notification racing the timed wait is bounded by its one-second timeout.
        self.shared.changed.notify_all();
    }
    pub(super) fn join(mut self) -> io::Result<()> {
        self.settle()
    }
    fn settle(&mut self) -> io::Result<()> {
        self.cancel();
        let Some(handle) = self.handle.take() else {
            return Ok(());
        };
        let deadline = Instant::now() + Duration::from_millis(100);
        while !handle.is_finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(2));
        }
        if handle.is_finished() {
            handle
                .join()
                .map_err(|_| io::Error::other("status worker failed"))
        } else {
            // Closing the lease stops only this TUI's spawned service. The binary exits
            // the process after CLI return, including any blocked OS lookup.
            drop(handle);
            Ok(())
        }
    }
}
impl Drop for Worker {
    fn drop(&mut self) {
        let _ = self.settle();
    }
}
pub(super) struct Model {
    pending_since: Instant,
    generation: u64,
    current: Option<Observation>,
    expired: bool,
    not_before: Option<Instant>,
}
impl Default for Model {
    fn default() -> Self {
        Self {
            pending_since: Instant::now(),
            generation: 0,
            current: None,
            expired: false,
            not_before: None,
        }
    }
}
impl Model {
    pub(super) fn apply(&mut self, value: Observation) -> bool {
        if self
            .not_before
            .is_some_and(|barrier| value.started < barrier)
            || value.generation < self.generation
            || self
                .current
                .as_ref()
                .is_some_and(|old| value.started < old.started)
        {
            return false;
        }
        self.generation = value.generation;
        self.current = Some(value);
        self.expired = false;
        true
    }
    pub(super) fn retire(&mut self, now: Instant) {
        self.generation = self.generation.saturating_add(1);
        self.current = None;
        self.expired = true;
        self.not_before = Some(now);
    }
    pub fn next_deadline(&self) -> Option<Instant> {
        (!self.expired).then(|| {
            self.current
                .as_ref()
                .map_or(self.pending_since, |value| value.started)
                + Duration::from_secs(5)
        })
    }
    pub(super) fn expire(&mut self, now: Instant) -> bool {
        if !self.expired
            && now.saturating_duration_since(
                self.current
                    .as_ref()
                    .map_or(self.pending_since, |value| value.started),
            ) >= Duration::from_secs(5)
        {
            self.expired = true;
            return true;
        }
        false
    }
    pub(super) fn view(&self) -> super::ui::View {
        let mut view = super::ui::View {
            configured_model: None,
            service: None,
            connection: "connecting",
            lifecycle: "unavailable",
            installation: "unavailable",
            reason: "inspection_pending",
            epoch: None,
        };
        if self.expired {
            view.connection = "unavailable";
            view.reason = "status_expired";
            return view;
        }
        match self.current.as_ref().map(|value| &value.result) {
            Some(Ok(snapshot)) => {
                view.configured_model = self
                    .current
                    .as_ref()
                    .and_then(|value| value.configured_model.clone());
                view.service = self
                    .current
                    .as_ref()
                    .and_then(|value| value.service.clone());
                view.connection = "connected";
                view.lifecycle = crate::app::lifecycle_name(snapshot.lifecycle);
                view.installation = crate::app::installation_name(snapshot.installation);
                view.reason = crate::app::reason_name(snapshot.reason);
                view.epoch = Some(
                    snapshot
                        .service_epoch
                        .iter()
                        .map(|byte| format!("{byte:02x}"))
                        .collect(),
                );
            }
            Some(Err(reason)) => {
                view.connection = "unavailable";
                view.reason = reason;
            }
            None => (),
        }
        view
    }
}
pub(super) struct Continuity {
    monotonic: Instant,
    wall: SystemTime,
}
impl Continuity {
    pub(super) fn new() -> Self {
        Self {
            monotonic: Instant::now(),
            wall: SystemTime::now(),
        }
    }
    pub(super) fn check(&mut self, monotonic: Instant, wall: SystemTime) -> bool {
        let uncertain = match (
            monotonic.checked_duration_since(self.monotonic),
            wall.duration_since(self.wall),
        ) {
            (Some(a), Ok(b)) => a.abs_diff(b) > Duration::from_secs(1),
            _ => true,
        };
        self.monotonic = monotonic;
        self.wall = wall;
        uncertain
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn contended_mailbox_defers_without_blocking_or_losing_observation() {
        let shared = Arc::new(Shared {
            latest: Mutex::new(Some(Observation {
                configured_model: None,
                service: None,
                generation: 7,
                started: Instant::now(),
                result: Err("test observation"),
            })),
            changed: Condvar::new(),
            stop: AtomicBool::new(false),
        });
        let worker = Worker {
            notice: Default::default(),
            shared: shared.clone(),
            handle: None,
            lease: Mutex::new(None),
        };
        let guard = shared.latest.lock().unwrap();
        let (send, receive) = std::sync::mpsc::sync_channel(1);
        let consumer = std::thread::spawn(move || {
            let deferred = worker.take().is_none();
            worker.cancel();
            let _ = send.send(deferred);
            worker
        });
        let result = receive.recv_timeout(Duration::from_secs(1));
        // Release before asserting/joining so a blocking regression can settle.
        drop(guard);
        let worker = consumer.join().unwrap();
        assert!(result.unwrap());
        assert!(shared.stop.load(Ordering::Acquire));
        assert_eq!(worker.take().unwrap().generation, 7);
        assert!(worker.take().is_none());
    }

    fn snapshot() -> Snapshot {
        Snapshot {
            service_epoch: [1; 16],
            service_build: "asura/test".into(),
            lifecycle: 2,
            installation: 2,
            reason: 2,
        }
    }
    #[test]
    fn first_observation_expires_even_when_worker_never_returns() {
        let mut model = Model::default();
        assert!(model.expire(model.pending_since + Duration::from_secs(5)));
        assert_eq!(model.view().connection, "unavailable");
    }

    #[test]
    fn autostart_refuses_unsafe_runtime_and_attempts_log_setup_only_once() {
        fn unsafe_runtime(_: bool) -> asura_platform::Result<RuntimeDirectory> {
            Err(asura_platform::Error::UnsafeRuntime)
        }
        fn no_log() -> asura_platform::Result<std::fs::File> {
            panic!("unexpected startup")
        }
        fn absent(create: bool) -> asura_platform::Result<RuntimeDirectory> {
            assert!(!create, "runtime created after failed log setup");
            Err(asura_platform::Error::Absent)
        }
        fn failed_log() -> asura_platform::Result<std::fs::File> {
            Err(asura_platform::Error::Unavailable)
        }
        let mut attempted = false;
        let (_lease, lifetime) = asura_platform::ServiceLifetime::new().unwrap();
        let stop = AtomicBool::new(false);
        assert!(inspect(unsafe_runtime, no_log, &mut attempted, &stop, &lifetime).is_err());
        assert!(!attempted);
        assert!(matches!(
            inspect(absent, failed_log, &mut attempted, &stop, &lifetime),
            Err("service_log_unavailable")
        ));
        assert!(attempted);
        assert!(inspect(absent, no_log, &mut attempted, &stop, &lifetime).is_err());
        stop.store(true, Ordering::Release);
        assert!(matches!(
            inspect(unsafe_runtime, no_log, &mut attempted, &stop, &lifetime),
            Err("cancelled")
        ));
    }

    #[test]
    fn settlement_does_not_wait_for_a_stalled_observation() {
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let worker = Worker::start_with(move |_| {
            entered_tx.send(()).unwrap();
            release_rx.recv().unwrap();
            done_tx.send(()).unwrap();
            Ok(snapshot())
        })
        .unwrap();
        entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let started = Instant::now();
        let result = worker.join();
        let elapsed = started.elapsed();
        release_tx.send(()).unwrap();
        done_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        result.unwrap();
        assert!(elapsed < Duration::from_millis(500), "{elapsed:?}");
    }

    #[test]
    fn failures_expiry_and_old_generations_never_retain_current_claims() {
        let now = Instant::now();
        let mut model = Model::default();
        assert!(model.apply(Observation {
            configured_model: None,
            service: None,
            generation: 1,
            started: now,
            result: Ok(snapshot())
        }));
        assert!(model.view().epoch.is_some());
        assert!(model.expire(now + Duration::from_secs(5)));
        assert!(model.view().epoch.is_none());
        assert!(model.apply(Observation {
            configured_model: None,
            service: None,
            generation: 2,
            started: now + Duration::from_secs(6),
            result: Err("service_unavailable")
        }));
        assert_eq!(model.view().connection, "unavailable");
        assert!(model.view().epoch.is_none());
        assert!(!model.apply(Observation {
            configured_model: None,
            service: None,
            generation: 1,
            started: now,
            result: Ok(snapshot())
        }));
        assert!(model.apply(Observation {
            configured_model: None,
            service: None,
            generation: 3,
            started: now + Duration::from_secs(7),
            result: Ok(snapshot())
        }));
        assert_eq!(model.view().connection, "connected");
    }
    #[test]
    fn clock_discontinuity_retires_pre_resume_results() {
        let now = Instant::now();
        let wall = SystemTime::now();
        let mut clock = Continuity {
            monotonic: now,
            wall,
        };
        assert!(!clock.check(now + Duration::from_secs(1), wall + Duration::from_secs(1)));
        assert!(clock.check(now + Duration::from_secs(2), wall + Duration::from_secs(20)));
        let mut model = Model::default();
        model.apply(Observation {
            configured_model: None,
            service: None,
            generation: 1,
            started: now,
            result: Ok(snapshot()),
        });
        model.retire(now + Duration::from_secs(2));
        assert!(!model.apply(Observation {
            configured_model: None,
            service: None,
            generation: 2,
            started: now + Duration::from_secs(1),
            result: Ok(snapshot())
        }));
        assert!(model.view().epoch.is_none());
        assert!(model.apply(Observation {
            configured_model: None,
            service: None,
            generation: 3,
            started: now + Duration::from_secs(3),
            result: Ok(snapshot())
        }));
    }
    #[test]
    fn cancellation_wakes_worker_without_starting_another_cycle() {
        let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen = count.clone();
        let worker = Worker::start_with(move |_| {
            seen.fetch_add(1, Ordering::SeqCst);
            Ok(snapshot())
        })
        .unwrap();
        let end = Instant::now() + Duration::from_secs(2);
        while worker.take().is_none() {
            assert!(Instant::now() < end);
            std::thread::yield_now();
        }
        worker.join().unwrap();
        assert_eq!(count.load(Ordering::SeqCst), 1);
    }
}
