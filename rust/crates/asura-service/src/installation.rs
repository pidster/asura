//! Read-only installation scanning. Only the service reactor publishes results.
use asura_control::pb::{self, InstallationInspectionReason as Reason, InstallationState as State};
use asura_platform::{AuthorityError, AuthoritySource, RuntimeDirectory};
use asura_storage::authority::journal::{Failure, Journal, Opened};
use asura_storage::authority::{self, ReplayError, ReplayState};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::thread::JoinHandle;
use std::time::Instant;

pub(crate) fn snapshot(state: State, reason: Reason) -> pb::InspectInstallationReply {
    pb::InspectInstallationReply {
        new_installation: None,
        installation: Some(state as i32),
        reason: Some(reason as i32),
        installation_id: None,
        authority_revision: None,
        recorded_owner_generation: None,
        binding_generation: None,
        authority_format: None,
    }
}
fn failure(reason: Reason) -> pb::InspectInstallationReply {
    let state = match reason {
        Reason::InstallationRemnants
        | Reason::UnknownContent
        | Reason::UnsupportedLayout
        | Reason::UnsupportedFormat
        | Reason::IncompleteTail
        | Reason::CorruptAuthority => State::RepairRequired,
        _ => State::Unavailable,
    };
    snapshot(state, reason)
}
fn platform_failure(error: AuthorityError) -> pb::InspectInstallationReply {
    failure(match error {
        AuthorityError::Unsafe => Reason::UnsafeAuthority,
        AuthorityError::Changed => Reason::AuthorityChanged,
        AuthorityError::Limit => Reason::InspectionLimit,
        AuthorityError::Cancelled => Reason::InspectionCancelled,
        AuthorityError::Io => Reason::InspectionIo,
    })
}
pub(crate) struct Candidate {
    snapshot: pb::InspectInstallationReply,
    witness: Option<asura_platform::AuthorityWitness>,
}
impl From<pb::InspectInstallationReply> for Candidate {
    fn from(snapshot: pb::InspectInstallationReply) -> Self {
        Self {
            snapshot,
            witness: None,
        }
    }
}
impl Candidate {
    fn publish(self) -> pb::InspectInstallationReply {
        if let Some(witness) = self.witness
            && let Err(error) = witness.validate()
        {
            return platform_failure(error);
        }
        self.snapshot
    }
}
/// Startup inspection replay has no time bound (J1-0); shutdown cancels it.
pub(crate) fn scan(runtime: RuntimeDirectory, cancelled: Arc<AtomicBool>) -> Candidate {
    let scan = match runtime.authority_source(&cancelled) {
        Ok(scan) => scan,
        Err(error) => return platform_failure(error).into(),
    };
    let result = scan_source(scan.source, &cancelled);
    Candidate {
        snapshot: result,
        witness: Some(scan.witness),
    }
}
fn scan_source(
    source: AuthoritySource,
    cancelled: &Arc<AtomicBool>,
) -> pb::InspectInstallationReply {
    let reader = match source {
        AuthoritySource::RuntimeOnly => return snapshot(State::Uninitialized, Reason::RuntimeOnly),
        AuthoritySource::Remnants => return failure(Reason::InstallationRemnants),
        AuthoritySource::UnsupportedLayout => return failure(Reason::UnsupportedLayout),
        AuthoritySource::Journal(reader) => reader,
    };
    let directory = match reader.directory(cancelled.clone()) {
        Ok(directory) => directory,
        Err(error) => return platform_failure(error),
    };
    // The read-only scan: one checkpoint and the tail; it never writes.
    let opened = Journal::open(directory, false, cancelled);
    if let Err(error) = reader.validate() {
        return platform_failure(error);
    }
    let replay = match opened {
        Ok(Opened::Journal(journal)) => authority::summary(journal.state()),
        Ok(Opened::Empty(_)) => return failure(Reason::InstallationRemnants),
        Err(error) => {
            return match error {
                Failure::RebuildPending => {
                    snapshot(State::Recovering, Reason::CheckpointRebuildPending)
                }
                Failure::Cancelled => failure(Reason::InspectionCancelled),
                Failure::Unsafe => failure(Reason::UnsafeAuthority),
                Failure::OwnerBusy | Failure::Io => failure(Reason::InspectionIo),
                Failure::UnsupportedLayout => failure(Reason::UnsupportedLayout),
                Failure::Remnants => failure(Reason::InstallationRemnants),
                Failure::Replay(error) => failure(match error {
                    ReplayError::Empty => Reason::InstallationRemnants,
                    ReplayError::IncompleteTail => Reason::IncompleteTail,
                    ReplayError::UnsupportedFormat => Reason::UnsupportedFormat,
                    ReplayError::Corrupt => Reason::CorruptAuthority,
                    ReplayError::Limit => Reason::InspectionLimit,
                    ReplayError::Cancelled => Reason::InspectionCancelled,
                }),
                _ => failure(Reason::CorruptAuthority),
            };
        }
    };
    if reader.has_unknown_root_content() {
        return failure(Reason::UnknownContent);
    }
    let (state, reason) = match replay.state {
        ReplayState::PendingInit => (State::Recovering, Reason::InitializationPending),
        ReplayState::ActiveBinding => (
            State::GraphUnavailable,
            Reason::GraphVerificationUnavailable,
        ),
    };
    let mut result = snapshot(state, reason);
    result.installation_id = Some(replay.installation_id.to_vec());
    result.authority_revision = Some(replay.revision);
    result.recorded_owner_generation = Some(replay.owner_generation);
    result.binding_generation = replay.binding_generation;
    result.authority_format = Some(1);
    result
}
struct ResultEnvelope {
    epoch: [u8; 16],
    scan_id: [u8; 16],
    candidate: Candidate,
}
pub(crate) struct Worker {
    epoch: [u8; 16],
    scan_id: [u8; 16],
    cancelled: Arc<AtomicBool>,
    handle: Option<JoinHandle<ResultEnvelope>>,
    retired: bool,
    completion: crate::completion::Completion,
}
impl Worker {
    pub(crate) fn start(runtime: RuntimeDirectory, epoch: [u8; 16]) -> std::io::Result<Self> {
        Self::start_with(epoch, move |cancelled| scan(runtime, cancelled))
    }
    /// The scan runs until it finishes, fails on content, or is cancelled by
    /// shutdown or an epoch change. It has no time bound (J1-0).
    fn start_with<F, T>(epoch: [u8; 16], execute: F) -> std::io::Result<Self>
    where
        F: FnOnce(Arc<AtomicBool>) -> T + Send + 'static,
        T: Into<Candidate>,
    {
        let scan_id = asura_platform::random_id();
        let cancelled = Arc::new(AtomicBool::new(false));
        let token = cancelled.clone();
        let completion = crate::completion::Completion::default();
        let signal = completion.clone();
        let handle = std::thread::Builder::new()
            .name("asura-inspection".into())
            .spawn(move || {
                let _completion = signal.guard();
                let candidate: Candidate = execute(token).into();
                ResultEnvelope {
                    epoch,
                    scan_id,
                    candidate: candidate.publish().into(),
                }
            })?;
        Ok(Self {
            epoch,
            scan_id,
            cancelled,
            handle: Some(handle),
            retired: false,
            completion,
        })
    }
    pub(crate) fn register(&self, wake: asura_platform::events::WakeSender) {
        self.completion.register(wake);
    }
    pub(crate) fn next_deadline(&self, now: Instant) -> Option<Instant> {
        if self.handle.is_none() {
            None
        } else if self.completion.completed() {
            self.completion.settlement_deadline(now)
        } else {
            None
        }
    }
    pub(crate) fn cancel(&mut self) {
        self.retired = true;
        self.cancelled.store(true, Ordering::Release);
    }
    pub(crate) fn poll(
        &mut self,
        epoch: [u8; 16],
        _now: Instant,
    ) -> Option<pb::InspectInstallationReply> {
        let mut result = None;
        if !self.retired && epoch != self.epoch {
            self.cancel();
            result = Some(failure(Reason::InspectionCancelled));
        }
        if self.handle.as_ref().is_some_and(JoinHandle::is_finished) {
            let finished = self.handle.take().unwrap().join();
            if !self.retired {
                result = Some(match finished {
                    Ok(value) if value.epoch == epoch && value.scan_id == self.scan_id => {
                        value.candidate.publish()
                    }
                    _ => failure(Reason::InspectionIo),
                });
                self.retired = true;
            }
        }
        result
    }
    pub(crate) fn settled(&self) -> bool {
        self.handle.is_none()
    }
}

/// Inspect shows `recovering`/`inspection_pending` while the writer's own
/// startup replay runs (owner, 2026-10-16). A first run with no journal
/// shows no loading state. Afterwards the conversation owner's snapshot
/// applies, or the inspected snapshot returns if the owner has none.
#[derive(Default)]
pub(crate) struct ReplayLoading {
    inspected: Option<pb::InspectInstallationReply>,
}
impl ReplayLoading {
    pub(crate) fn update(
        &mut self,
        replaying: bool,
        current: &pb::InspectInstallationReply,
        owner_has_snapshot: bool,
    ) -> Option<pb::InspectInstallationReply> {
        if replaying {
            if self.inspected.is_none() && current.reason != Some(Reason::RuntimeOnly as i32) {
                self.inspected = Some(current.clone());
                return Some(snapshot(State::Recovering, Reason::InspectionPending));
            }
            return None;
        }
        let inspected = self.inspected.take()?;
        (!owner_has_snapshot).then_some(inspected)
    }
}

/// A single deferred scan factory. The wait for audit bootstrap has no time
/// bound (owner, 2026-10-16): Inspect reports the pending inspection until
/// bootstrap completes and the scan runs, or Stop discards the factory.
pub(crate) struct Deferred<F> {
    runtime: RuntimeDirectory,
    epoch: [u8; 16],
    factory: Option<F>,
    worker: Option<Worker>,
    wake: Option<asura_platform::events::WakeSender>,
    timeout_reported: bool,
    cancelled: bool,
}
impl<F> Deferred<F>
where
    F: FnOnce(RuntimeDirectory, [u8; 16]) -> std::io::Result<Worker>,
{
    pub(crate) fn new(runtime: RuntimeDirectory, epoch: [u8; 16], factory: F) -> Self {
        Self {
            runtime,
            epoch,
            factory: Some(factory),
            worker: None,
            wake: None,
            timeout_reported: false,
            cancelled: false,
        }
    }
    pub(crate) fn register(&mut self, wake: asura_platform::events::WakeSender) {
        if let Some(worker) = &mut self.worker {
            worker.register(wake.clone());
        }
        self.wake = Some(wake);
    }
    /// Call only after the audit bootstrap's actual completion signal.
    pub(crate) fn release(&mut self) -> std::io::Result<()> {
        if self.cancelled {
            return Ok(());
        }
        if let Some(factory) = self.factory.take() {
            let worker = factory(self.runtime.clone(), self.epoch)?;
            if let Some(wake) = &self.wake {
                worker.register(wake.clone());
            }
            self.worker = Some(worker);
        }
        Ok(())
    }
    pub(crate) fn poll(
        &mut self,
        epoch: [u8; 16],
        now: Instant,
    ) -> Option<pb::InspectInstallationReply> {
        if let Some(worker) = &mut self.worker {
            return worker.poll(epoch, now);
        }
        if !self.cancelled && !self.timeout_reported && epoch != self.epoch {
            self.timeout_reported = true;
            return Some(failure(Reason::InspectionCancelled));
        }
        None
    }
    pub(crate) fn next_deadline(&self, now: Instant) -> Option<Instant> {
        self.worker
            .as_ref()
            .and_then(|worker| worker.next_deadline(now))
    }
    pub(crate) fn cancel(&mut self) {
        self.cancelled = true;
        self.factory.take();
        if let Some(worker) = &mut self.worker {
            worker.cancel();
        }
    }
    pub(crate) fn settled(&self) -> bool {
        self.worker.as_ref().map_or(self.cancelled, Worker::settled)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    #[test]
    fn writer_replay_shows_loading_only_when_a_journal_exists() {
        let mut loading = ReplayLoading::default();
        let fresh = snapshot(State::Uninitialized, Reason::RuntimeOnly);
        assert!(loading.update(true, &fresh, false).is_none());
        assert!(loading.update(false, &fresh, false).is_none());
        let inspected = snapshot(
            State::GraphUnavailable,
            Reason::GraphVerificationUnavailable,
        );
        let shown = loading.update(true, &inspected, false).unwrap();
        assert_eq!(shown.installation, Some(State::Recovering as i32));
        assert_eq!(shown.reason, Some(Reason::InspectionPending as i32));
        let current = shown.clone();
        assert!(loading.update(true, &current, false).is_none());
        // Replay finished and the owner publishes its own snapshot.
        assert!(loading.update(false, &current, true).is_none());
        // Replay failed without an owner snapshot: the inspected state returns.
        assert!(loading.update(true, &inspected, false).is_some());
        assert_eq!(loading.update(false, &current, false), Some(inspected));
    }
    #[test]
    fn epoch_change_and_cancellation_discard_late_results_without_blocking_join() {
        for epoch_change in [true, false] {
            let (release, gate) = std::sync::mpsc::channel();
            let mut worker = Worker::start_with([1; 16], move |_| {
                gate.recv().unwrap();
                snapshot(State::Uninitialized, Reason::RuntimeOnly)
            })
            .unwrap();
            // J1-0: a held scan never expires; it reports nothing until it ends.
            let later = Instant::now() + Duration::from_secs(3600);
            assert!(worker.poll([1; 16], later).is_none());
            assert!(worker.next_deadline(later).is_none());
            if epoch_change {
                let reply = worker.poll([2; 16], later).unwrap();
                assert_eq!(reply.reason, Some(Reason::InspectionCancelled as i32));
            } else {
                worker.cancel();
            }
            assert!(!worker.settled());
            assert!(worker.poll([1; 16], Instant::now()).is_none());
            release.send(()).unwrap();
            let end = Instant::now() + Duration::from_secs(2);
            while !worker.settled() {
                assert!(Instant::now() < end);
                assert!(worker.poll([1; 16], Instant::now()).is_none());
                std::thread::yield_now();
            }
        }
    }
    #[test]
    fn changed_authority_cannot_publish_completed_scan() {
        use std::os::unix::fs::DirBuilderExt;
        let path = std::path::PathBuf::from("/private/tmp").join(format!(
            "asura-publication-{:x}",
            u128::from_ne_bytes(asura_platform::random_id())
        ));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .unwrap();
        let runtime = RuntimeDirectory::scratch(&path, true).unwrap();
        let candidate = scan(runtime, Arc::new(AtomicBool::new(false)));
        assert_eq!(candidate.snapshot.reason, Some(Reason::RuntimeOnly as i32));
        std::fs::write(path.join(".asura/new-entry"), b"changed").unwrap();
        let mut worker = Worker::start_with([1; 16], |_| candidate).unwrap();
        let end = Instant::now() + Duration::from_secs(2);
        loop {
            if let Some(reply) = worker.poll([1; 16], Instant::now()) {
                assert_eq!(reply.reason, Some(Reason::AuthorityChanged as i32));
                assert!(reply.installation_id.is_none());
                break;
            }
            assert!(Instant::now() < end);
            std::thread::yield_now();
        }
        std::fs::remove_dir_all(path).unwrap();
    }
    struct Scratch(std::path::PathBuf);
    impl Scratch {
        fn new() -> Self {
            use std::os::unix::fs::DirBuilderExt;
            let path = std::path::PathBuf::from(format!(
                "/private/tmp/asura-order-{:x}",
                u128::from_ne_bytes(asura_platform::random_id())
            ));
            std::fs::DirBuilder::new()
                .mode(0o700)
                .create(&path)
                .unwrap();
            Self(path)
        }
        fn runtime(&self) -> RuntimeDirectory {
            RuntimeDirectory::scratch(&self.0, true).unwrap()
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    #[test]
    fn deferred_scan_waits_for_bootstrap_without_a_deadline_and_stop_discards_it() {
        for stop in [false, true] {
            let scratch = Scratch::new();
            let invoked = Arc::new(AtomicBool::new(false));
            let flag = invoked.clone();
            let mut deferred = Deferred::new(scratch.runtime(), [1; 16], move |_, epoch| {
                assert!(!flag.swap(true, Ordering::AcqRel));
                Worker::start_with(epoch, |_| {
                    snapshot(State::Uninitialized, Reason::RuntimeOnly)
                })
            });
            // Held far past the removed 2 s bootstrap bound (injected clock):
            // no timeout is published; Inspect keeps the pending inspection.
            let later = Instant::now() + Duration::from_secs(3600);
            assert!(deferred.poll([1; 16], later).is_none());
            assert!(!invoked.load(Ordering::Acquire));
            assert!(!deferred.settled());
            assert!(deferred.next_deadline(later).is_none());
            if stop {
                deferred.cancel();
                deferred.release().unwrap();
                assert!(deferred.settled());
                assert!(!invoked.load(Ordering::Acquire));
            } else {
                deferred.release().unwrap();
                deferred.release().unwrap();
                assert!(invoked.load(Ordering::Acquire));
                let end = Instant::now() + Duration::from_secs(2);
                let mut reply = None;
                while !deferred.settled() {
                    if let Some(value) = deferred.poll([1; 16], Instant::now()) {
                        reply = Some(value);
                    }
                    assert!(Instant::now() < end);
                    std::thread::yield_now();
                }
                assert_eq!(reply.unwrap().reason, Some(Reason::RuntimeOnly as i32));
            }
        }
    }
    #[test]
    fn real_audit_bootstrap_precedes_pristine_and_pending_authority_scan() {
        use std::{
            io::Write,
            os::unix::fs::{DirBuilderExt, OpenOptionsExt},
        };
        for pending in [false, true] {
            let scratch = Scratch::new();
            let runtime = scratch.runtime();
            if pending {
                for name in [".asura/state", ".asura/state/control"] {
                    std::fs::DirBuilder::new()
                        .mode(0o700)
                        .create(scratch.0.join(name))
                        .unwrap();
                }
                let mut file = std::fs::OpenOptions::new()
                    .create_new(true)
                    .write(true)
                    .mode(0o600)
                    .open(
                        scratch
                            .0
                            .join(".asura/state/control/seg-0000000000000001.log"),
                    )
                    .unwrap();
                file.write_all(&asura_storage::authority::fixture::initialization_journal(
                    false,
                ))
                .unwrap();
                file.sync_all().unwrap();
            }
            let logs = scratch.0.join(".asura/logs");
            assert!(!logs.exists());
            let mut inspection = Deferred::new(runtime.clone(), [1; 16], move |runtime, epoch| {
                assert!(
                    logs.is_dir(),
                    "scan started before root bootstrap mutation finished"
                );
                Worker::start(runtime, epoch)
            });
            let (_, wake) = asura_platform::events::wake_pair().unwrap();
            let mut audit =
                crate::audit::Worker::start(runtime, [1; 16], "asura/test".into(), wake).unwrap();
            let end = Instant::now() + Duration::from_secs(5);
            let mut reply = None;
            while !inspection.settled() {
                audit.poll(Instant::now());
                if audit.bootstrap_settled() {
                    inspection.release().unwrap();
                }
                if let Some(value) = inspection.poll([1; 16], Instant::now()) {
                    reply = Some(value);
                }
                assert!(Instant::now() < end);
                std::thread::yield_now();
            }
            let reply = reply.unwrap();
            assert_eq!(
                reply.reason,
                Some(if pending {
                    Reason::InitializationPending
                } else {
                    Reason::RuntimeOnly
                } as i32)
            );
            audit.close(Instant::now());
            while !audit.settled() {
                audit.poll(Instant::now());
                assert!(Instant::now() < end);
                std::thread::yield_now();
            }
        }
    }

    #[test]
    fn obsolete_epoch_cannot_publish() {
        let mut worker = Worker::start_with([1; 16], |_| {
            snapshot(State::Uninitialized, Reason::RuntimeOnly)
        })
        .unwrap();
        assert_eq!(
            worker.poll([2; 16], Instant::now()).unwrap().reason,
            Some(Reason::InspectionCancelled as i32)
        );
        let end = Instant::now() + Duration::from_secs(2);
        while !worker.settled() {
            assert!(Instant::now() < end);
            assert!(worker.poll([2; 16], Instant::now()).is_none());
            std::thread::yield_now();
        }
    }
}

#[cfg(test)]
mod reactor_tests {
    use super::*;
    use asura_client::Client;
    use std::os::unix::fs::DirBuilderExt;
    use std::time::Duration;
    /// Test-only harness budget for one client's waits.
    fn within(runtime: &RuntimeDirectory, budget: Duration) -> Option<Client> {
        let end = Instant::now() + budget;
        let cancel: asura_client::Cancel = Arc::new(move || Instant::now() >= end);
        let mut client = Client::attach(runtime, "asura/test", &*cancel).ok()?;
        client.set_cancel(cancel);
        Some(client)
    }
    fn attach(runtime: &RuntimeDirectory) -> Client {
        let end = Instant::now() + Duration::from_secs(3);
        loop {
            if let Some(client) = within(runtime, Duration::from_secs(3)) {
                return client;
            }
            assert!(Instant::now() < end);
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    #[test]
    fn lifetime_eof_drains_endpoint_and_releases_owner() {
        let _writer_owner = crate::TEST_WRITER
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let path = std::path::PathBuf::from("/private/tmp").join(format!(
            "asura-lifetime-{:x}",
            u128::from_ne_bytes(asura_platform::random_id())
        ));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .unwrap();
        let runtime = RuntimeDirectory::scratch(&path, true).unwrap();
        let for_service = runtime.clone();
        let (lease, lifetime) = asura_platform::ServiceLifetime::new().unwrap();
        let service =
            std::thread::spawn(move || crate::run_owned(for_service, "asura/test", None, lifetime));
        let client = attach(&runtime);
        assert!(runtime.acquire_owner().is_err());
        drop(client);
        // An attachment closing does not end the owner's lifetime.
        let mut client = attach(&runtime);
        assert_eq!(
            client.inspect().unwrap().lifecycle,
            pb::Lifecycle::Serving as i32
        );
        let started = Instant::now();
        drop(lease);
        let end = started + Duration::from_secs(3);
        while !service.is_finished() {
            assert!(Instant::now() < end, "lifetime EOF did not settle service");
            std::thread::sleep(Duration::from_millis(10));
        }
        service.join().unwrap().unwrap();
        assert!(runtime.acquire_owner().is_ok());
        assert!(!path.join(".asura/run/control.sock").exists());
        std::fs::remove_dir_all(path).unwrap();
    }

    /// Q-B: a lifetime drain waits for a stalled scan with no deadline; the
    /// owner stays `Draining` (never `RepairOnly`) and keeps its lock until
    /// the worker settles, then releases it.
    #[test]
    fn lifetime_eof_drain_waits_for_stalled_scan_without_repair() {
        let _writer_owner = crate::TEST_WRITER
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let path = std::path::PathBuf::from("/private/tmp").join(format!(
            "asura-lifetime-scan-{:x}",
            u128::from_ne_bytes(asura_platform::random_id())
        ));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .unwrap();
        let runtime = RuntimeDirectory::scratch(&path, true).unwrap();
        let for_service = runtime.clone();
        let (lease, lifetime) = asura_platform::ServiceLifetime::new().unwrap();
        let (release, gate) = std::sync::mpsc::channel();
        let service = std::thread::spawn(move || {
            crate::run_reactor(
                for_service,
                "asura/test",
                None,
                Some(lifetime),
                move |_, epoch| {
                    Worker::start_with(epoch, move |_| {
                        gate.recv_timeout(Duration::from_secs(20)).unwrap();
                        snapshot(State::Uninitialized, Reason::RuntimeOnly)
                    })
                },
            )
        });
        assert_eq!(
            attach(&runtime).inspect_installation().unwrap().reason,
            Reason::InspectionPending
        );
        drop(lease);
        // Held past the removed one-second drain bound: still draining.
        let held = Instant::now() + Duration::from_millis(1_500);
        let mut draining = false;
        while Instant::now() < held {
            if let Some(mut client) = within(&runtime, Duration::from_millis(500))
                && let Ok(status) = client.inspect()
            {
                assert_ne!(status.lifecycle, pb::Lifecycle::RepairOnly as i32);
                assert_ne!(status.reason, Reason::InspectionCancelled as i32);
                draining |= status.lifecycle == pb::Lifecycle::Draining as i32;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(draining, "Inspect did not report Draining");
        assert!(runtime.acquire_owner().is_err());
        assert!(!service.is_finished());
        release.send(()).unwrap();
        let end = Instant::now() + Duration::from_secs(5);
        while !service.is_finished() {
            assert!(
                Instant::now() < end,
                "settled worker did not finish owned shutdown"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        service.join().unwrap().unwrap();
        assert!(runtime.acquire_owner().is_ok());
        std::fs::remove_dir_all(path).unwrap();
    }

    /// Q-B / CW-04: while the first Stop waits for a stalled worker, a second
    /// Stop for the same epoch forces exit without further waiting. The owner
    /// lock stays held until process exit (crash-like), so no successor can
    /// start while this process's threads still run.
    #[test]
    fn second_stop_forces_exit_while_a_worker_is_stalled() {
        let _writer_owner = crate::TEST_WRITER
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let path = std::path::PathBuf::from("/private/tmp").join(format!(
            "asura-forced-stop-{:x}",
            u128::from_ne_bytes(asura_platform::random_id())
        ));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .unwrap();
        let runtime = RuntimeDirectory::scratch(&path, true).unwrap();
        let for_service = runtime.clone();
        let (release, gate) = std::sync::mpsc::channel::<()>();
        let service = std::thread::spawn(move || {
            crate::run_with_worker(for_service, "asura/test", None, move |_, epoch| {
                Worker::start_with(epoch, move |_| {
                    let _ = gate.recv_timeout(Duration::from_secs(30));
                    snapshot(State::Uninitialized, Reason::RuntimeOnly)
                })
            })
        });
        let first = attach(&runtime);
        let first_stop = std::thread::spawn(move || first.stop());
        let end = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(mut client) = within(&runtime, Duration::from_millis(500))
                && let Ok(status) = client.inspect()
                && status.lifecycle == pb::Lifecycle::Draining as i32
            {
                break;
            }
            assert!(Instant::now() < end, "first Stop did not start draining");
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(!service.is_finished());
        // Second stop request: the reactor exits without waiting for the worker.
        let second = within(&runtime, Duration::from_secs(3)).unwrap();
        let second_stop = std::thread::spawn(move || second.stop());
        let end = Instant::now() + Duration::from_secs(5);
        while !service.is_finished() {
            assert!(Instant::now() < end, "second Stop did not force exit");
            std::thread::sleep(Duration::from_millis(10));
        }
        service.join().unwrap().unwrap();
        // Crash-like: the lock is retained until process exit; the stalled
        // worker has not settled, and its work is left for replay.
        assert!(matches!(
            runtime.acquire_owner(),
            Err(asura_platform::Error::OwnerBusy)
        ));
        // Both stop waits end on their harness budgets (lock never released
        // in-process); neither reports success.
        assert!(second_stop.join().unwrap().is_err());
        release.send(()).unwrap();
        assert!(first_stop.join().unwrap().is_err());
        let _ = std::fs::remove_dir_all(path);
    }

    #[test]
    fn delayed_scan_keeps_control_responsive_and_owner_until_settlement() {
        let _writer_owner = crate::TEST_WRITER
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let path = std::path::PathBuf::from("/private/tmp").join(format!(
            "asura-scan-{:x}",
            u128::from_ne_bytes(asura_platform::random_id())
        ));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .unwrap();
        let runtime = RuntimeDirectory::scratch(&path, true).unwrap();
        let for_service = runtime.clone();
        let (release, gate) = std::sync::mpsc::channel();
        let service = std::thread::spawn(move || {
            crate::run_with_worker(for_service, "asura/test", None, move |_, epoch| {
                Worker::start_with(epoch, move |_| {
                    gate.recv_timeout(Duration::from_secs(8)).unwrap();
                    snapshot(State::Uninitialized, Reason::RuntimeOnly)
                })
            })
        });
        let mut client = attach(&runtime);
        assert_eq!(
            client.inspect_installation().unwrap().reason,
            Reason::InspectionPending
        );
        assert_eq!(
            client
                .config("model", Some("system"), &|| false)
                .unwrap()
                .error
                .as_deref(),
            Some("config_busy")
        );
        assert!(!path.join(".asura/config.yaml").exists());
        assert_eq!(
            client
                .config("audit.enabled", None, &|| false)
                .unwrap()
                .value_yaml
                .as_deref(),
            Some("true\n")
        );
        // CW-04 / Q-B: Stop waits for the stalled scan with no deadline. The
        // stop client keeps waiting and receives progress; Inspect reports
        // `Draining`, never `RepairOnly`, and the owner keeps its lock.
        let stopper = attach(&runtime);
        let progress = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen = progress.clone();
        let stop_thread = std::thread::spawn(move || {
            stopper.stop_observed(&mut |snapshot| {
                if snapshot.lifecycle == pb::Lifecycle::Draining as i32 {
                    seen.fetch_add(1, Ordering::AcqRel);
                }
            })
        });
        let held = Instant::now() + Duration::from_millis(2_500);
        let mut draining = false;
        while Instant::now() < held {
            if let Some(mut client) = within(&runtime, Duration::from_millis(500))
                && let Ok(status) = client.inspect()
            {
                assert_ne!(status.lifecycle, pb::Lifecycle::RepairOnly as i32);
                draining |= status.lifecycle == pb::Lifecycle::Draining as i32;
                // Work requests are refused while draining.
                assert!(matches!(
                    client.config("audit.enabled", None, &|| false),
                    Err(asura_client::Error::Remote(..))
                ));
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(draining);
        assert!(!stop_thread.is_finished(), "stop wait ended before drain");
        assert!(
            progress.load(Ordering::Acquire) > 0,
            "stop showed no progress"
        );
        assert!(runtime.acquire_owner().is_err());
        assert_ne!(
            attach(&runtime).inspect_installation().unwrap().reason,
            Reason::InspectionCancelled
        );
        release.send(()).unwrap();
        // The original Stop finishes without a second client command.
        let end = Instant::now() + Duration::from_secs(3);
        while !service.is_finished() {
            assert!(
                Instant::now() < end,
                "shutdown did not resume after settlement"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        service.join().unwrap().unwrap();
        stop_thread.join().unwrap().unwrap();
        assert!(runtime.acquire_owner().is_ok());
        std::fs::remove_dir_all(path).unwrap();
    }
}
