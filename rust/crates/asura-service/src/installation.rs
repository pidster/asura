//! Read-only installation scanning. Only the service reactor publishes results.
use asura_control::pb::{self, InstallationInspectionReason as Reason, InstallationState as State};
use asura_platform::{AuthorityError, AuthoritySource, RuntimeDirectory};
use asura_storage::authority::{self, ReplayError, ReplayState};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

pub(crate) fn snapshot(state: State, reason: Reason) -> pb::InspectInstallationReply {
    pb::InspectInstallationReply {
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
        AuthorityError::Timeout | AuthorityError::Cancelled => Reason::InspectionTimeout,
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
pub(crate) fn scan(
    runtime: RuntimeDirectory,
    deadline: Instant,
    cancelled: Arc<AtomicBool>,
) -> Candidate {
    let scan = match runtime.authority_source(deadline, &cancelled) {
        Ok(scan) => scan,
        Err(error) => return platform_failure(error).into(),
    };
    let result = scan_source(scan.source, deadline, &cancelled);
    Candidate {
        snapshot: result,
        witness: Some(scan.witness),
    }
}
fn scan_source(
    source: AuthoritySource,
    deadline: Instant,
    cancelled: &AtomicBool,
) -> pb::InspectInstallationReply {
    let mut reader = match source {
        AuthoritySource::RuntimeOnly => return snapshot(State::Uninitialized, Reason::RuntimeOnly),
        AuthoritySource::Remnants => return failure(Reason::InstallationRemnants),
        AuthoritySource::UnsupportedLayout => return failure(Reason::UnsupportedLayout),
        AuthoritySource::Journal(reader) => reader,
    };
    let bytes = match reader.read_all(deadline, cancelled) {
        Ok(bytes) => bytes,
        Err(error) => return platform_failure(error),
    };
    let replay = authority::replay(&bytes, deadline, cancelled);
    if let Err(error) = reader.validate() {
        return platform_failure(error);
    }
    let replay = match replay {
        Ok(replay) => replay,
        Err(error) => {
            return failure(match error {
                ReplayError::Empty => Reason::InstallationRemnants,
                ReplayError::IncompleteTail => Reason::IncompleteTail,
                ReplayError::UnsupportedFormat => Reason::UnsupportedFormat,
                ReplayError::Corrupt => Reason::CorruptAuthority,
                ReplayError::Limit => Reason::InspectionLimit,
                ReplayError::Timeout | ReplayError::Cancelled => Reason::InspectionTimeout,
            });
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
    deadline: Instant,
    cancelled: Arc<AtomicBool>,
    handle: Option<JoinHandle<ResultEnvelope>>,
    retired: bool,
    completion: crate::completion::Completion,
}
impl Worker {
    pub(crate) fn start(runtime: RuntimeDirectory, epoch: [u8; 16]) -> std::io::Result<Self> {
        Self::start_with(epoch, Duration::from_secs(5), move |deadline, cancelled| {
            scan(runtime, deadline, cancelled)
        })
    }
    fn start_with<F, T>(epoch: [u8; 16], timeout: Duration, execute: F) -> std::io::Result<Self>
    where
        F: FnOnce(Instant, Arc<AtomicBool>) -> T + Send + 'static,
        T: Into<Candidate>,
    {
        let scan_id = asura_platform::random_id();
        let deadline = Instant::now() + timeout;
        let cancelled = Arc::new(AtomicBool::new(false));
        let token = cancelled.clone();
        let completion = crate::completion::Completion::default();
        let signal = completion.clone();
        let handle = std::thread::Builder::new()
            .name("asura-inspection".into())
            .spawn(move || {
                let _completion = signal.guard();
                let candidate: Candidate = execute(deadline, token).into();
                ResultEnvelope {
                    epoch,
                    scan_id,
                    candidate: candidate.publish().into(),
                }
            })?;
        Ok(Self {
            epoch,
            scan_id,
            deadline,
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
        } else if !self.retired {
            Some(self.deadline)
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
        now: Instant,
    ) -> Option<pb::InspectInstallationReply> {
        let mut result = None;
        if !self.retired && (now >= self.deadline || epoch != self.epoch) {
            self.cancel();
            result = Some(failure(Reason::InspectionTimeout));
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

/// A single deferred scan factory; deadlines never substitute for bootstrap settlement.
pub(crate) struct Deferred<F> {
    runtime: RuntimeDirectory,
    epoch: [u8; 16],
    factory: Option<F>,
    worker: Option<Worker>,
    wake: Option<asura_platform::events::WakeSender>,
    deadline: Instant,
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
            deadline: Instant::now() + Duration::from_secs(2),
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
        if !self.cancelled
            && !self.timeout_reported
            && (now >= self.deadline || epoch != self.epoch)
        {
            self.timeout_reported = true;
            return Some(failure(Reason::InspectionTimeout));
        }
        None
    }
    pub(crate) fn next_deadline(&self, now: Instant) -> Option<Instant> {
        if let Some(worker) = &self.worker {
            worker.next_deadline(now)
        } else if !self.cancelled && !self.timeout_reported {
            Some(self.deadline)
        } else {
            None
        }
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
    #[test]
    fn timeout_and_cancellation_discard_late_results_without_blocking_join() {
        for timeout in [true, false] {
            let (release, gate) = std::sync::mpsc::channel();
            let mut worker = Worker::start_with([1; 16], Duration::from_secs(5), move |_, _| {
                gate.recv().unwrap();
                snapshot(State::Uninitialized, Reason::RuntimeOnly)
            })
            .unwrap();
            if timeout {
                let reply = worker.poll([1; 16], worker.deadline).unwrap();
                assert_eq!(reply.reason, Some(Reason::InspectionTimeout as i32));
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
        let candidate = scan(
            runtime,
            Instant::now() + Duration::from_secs(2),
            Arc::new(AtomicBool::new(false)),
        );
        assert_eq!(candidate.snapshot.reason, Some(Reason::RuntimeOnly as i32));
        std::fs::write(path.join(".asura/new-entry"), b"changed").unwrap();
        let mut worker =
            Worker::start_with([1; 16], Duration::from_secs(2), |_, _| candidate).unwrap();
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
    fn deferred_scan_timeout_does_not_invoke_factory_and_stop_discards_it() {
        for stop in [false, true] {
            let scratch = Scratch::new();
            let invoked = Arc::new(AtomicBool::new(false));
            let flag = invoked.clone();
            let mut deferred = Deferred::new(scratch.runtime(), [1; 16], move |_, epoch| {
                assert!(!flag.swap(true, Ordering::AcqRel));
                Worker::start_with(epoch, Duration::from_secs(2), |_, _| {
                    snapshot(State::Uninitialized, Reason::RuntimeOnly)
                })
            });
            let expiry = deferred.deadline;
            assert_eq!(
                deferred.poll([1; 16], expiry).unwrap().reason,
                Some(Reason::InspectionTimeout as i32)
            );
            assert!(deferred.poll([1; 16], expiry).is_none());
            assert!(!invoked.load(Ordering::Acquire));
            assert!(!deferred.settled());
            assert!(deferred.next_deadline(expiry).is_none());
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
                    .open(scratch.0.join(".asura/state/control/slot-0.log"))
                    .unwrap();
                file.write_all(include_bytes!(
                    "../../asura-storage/tests/fixtures/pending.bin"
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
        let mut worker = Worker::start_with([1; 16], Duration::from_secs(5), |_, _| {
            snapshot(State::Uninitialized, Reason::RuntimeOnly)
        })
        .unwrap();
        assert_eq!(
            worker.poll([2; 16], Instant::now()).unwrap().reason,
            Some(Reason::InspectionTimeout as i32)
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
    fn attach(runtime: &RuntimeDirectory) -> Client {
        let end = Instant::now() + Duration::from_secs(3);
        loop {
            if let Ok(client) = Client::attach(
                runtime,
                "asura/test",
                Instant::now() + Duration::from_millis(200),
            ) {
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

    #[test]
    fn lifetime_eof_preserves_drain_deadline_and_retries_after_scan_settles() {
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
                    Worker::start_with(epoch, Duration::from_secs(5), move |_, _| {
                        gate.recv_timeout(Duration::from_secs(8)).unwrap();
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
        // Repeated EOF cannot reset the one-second drain budget: the service must
        // enter responsive repair inspection while the worker is still blocked.
        let end = Instant::now() + Duration::from_secs(3);
        loop {
            if let Ok(mut client) = Client::attach(
                &runtime,
                "asura/test",
                Instant::now() + Duration::from_millis(200),
            ) && let Ok(status) = client.inspect()
                && status.lifecycle == pb::Lifecycle::RepairOnly as i32
            {
                break;
            }
            assert!(Instant::now() < end, "lifetime drain deadline was extended");
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(runtime.acquire_owner().is_err());
        assert_eq!(
            attach(&runtime).inspect_installation().unwrap().reason,
            Reason::InspectionTimeout
        );
        release.send(()).unwrap();
        let end = Instant::now() + Duration::from_secs(3);
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
                Worker::start_with(epoch, Duration::from_secs(5), move |_, _| {
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
                .config("model", Some("system"))
                .unwrap()
                .error
                .as_deref(),
            Some("config_busy")
        );
        assert!(!path.join(".asura/config.yaml").exists());
        assert_eq!(
            client
                .config("audit.enabled", None)
                .unwrap()
                .value_yaml
                .as_deref(),
            Some("true\n")
        );
        let stopper = attach(&runtime);
        let stop_thread = std::thread::spawn(move || stopper.stop());
        let end = Instant::now() + Duration::from_secs(3);
        loop {
            let mut client = attach(&runtime);
            if client.inspect().unwrap().lifecycle == pb::Lifecycle::RepairOnly as i32 {
                break;
            }
            assert!(Instant::now() < end);
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(runtime.acquire_owner().is_err());
        assert_eq!(
            attach(&runtime).inspect_installation().unwrap().reason,
            Reason::InspectionTimeout
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
        let _ = stop_thread.join().unwrap();
        assert!(runtime.acquire_owner().is_ok());
        std::fs::remove_dir_all(path).unwrap();
    }
}
