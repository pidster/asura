//! One bounded project-scoped audit request; recording and filtering belong to the service.
use super::observation::Resolver;
use asura_control::pb;
use std::{
    net::Shutdown,
    os::unix::net::UnixStream,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

pub(super) type Outcome = Result<pb::AuditReply, String>;
#[derive(Default)]
struct Cancellation {
    stopped: AtomicBool,
    socket: Mutex<Option<UnixStream>>,
}
impl Cancellation {
    fn cancel(&self) {
        self.stopped.store(true, Ordering::Release);
        // Never wait for the worker in the terminal event loop. The worker checks
        // the flag immediately after installing its handle if this lock is held.
        if let Ok(socket) = self.socket.try_lock()
            && let Some(socket) = socket.as_ref()
        {
            let _ = socket.shutdown(Shutdown::Both);
        }
    }
}
/// One audit read. It has no time bound (owner, 2026-10-16): it ends on the
/// reply, the person's `/cancel`, exit or a lost connection.
struct Job {
    handle: JoinHandle<Outcome>,
    cancel: Arc<Cancellation>,
    reported: bool,
}
#[derive(Default)]
pub(super) struct Worker {
    notice: super::events::Notice,
    job: Option<Job>,
}
impl Worker {
    pub fn set_notice(&mut self, notice: super::events::Notice) {
        self.notice = notice;
    }
    pub fn running(&self) -> bool {
        self.job.is_some()
    }
    pub fn submit(
        &mut self,
        resolve: Resolver,
        scope: super::queue_watch::Scope,
        limit: u32,
    ) -> Result<(), &'static str> {
        if self.running() {
            return Err("Audit inspection busy; draft retained");
        }
        let cancel = Arc::new(Cancellation::default());
        let token = cancel.clone();
        let notice = self.notice.clone();
        let handle = std::thread::Builder::new()
            .name("asura-audit-client".into())
            .spawn(move || {
                let _settlement = notice.guard();
                let run = || -> asura_client::Result<pb::AuditReply> {
                    if token.stopped.load(Ordering::Acquire) {
                        return Err(asura_client::Error::Unavailable);
                    }
                    let runtime = resolve(false)?;
                    if token.stopped.load(Ordering::Acquire) {
                        return Err(asura_client::Error::Unavailable);
                    }
                    let cancelled = || token.stopped.load(Ordering::Acquire);
                    let mut client = asura_client::Client::attach(
                        &runtime,
                        concat!("asura/", env!("CARGO_PKG_VERSION")),
                        &cancelled,
                    )?;
                    *token
                        .socket
                        .lock()
                        .map_err(|_| asura_client::Error::Unavailable)? =
                        Some(client.disconnect_handle()?);
                    if token.stopped.load(Ordering::Acquire) {
                        token.cancel();
                        return Err(asura_client::Error::Unavailable);
                    }
                    if crate::conversation::hex(&client.service_epoch()) != scope.epoch {
                        return Err(asura_client::Error::Unavailable);
                    }
                    client.audit(
                        pb::AuditRead {
                            project_id: Some(scope.project.to_vec()),
                            limit: Some(limit),
                        },
                        &cancelled,
                    )
                };
                let result = run().map_err(|error| error.to_string());
                // Release the cloned descriptor so success/error disconnects promptly.
                if let Ok(mut socket) = token.socket.lock() {
                    socket.take();
                }
                result.and_then(|reply| match &reply.error {
                    Some(error) => Err(error.clone()),
                    None => Ok(reply),
                })
            })
            .map_err(|_| "Cannot start audit inspection; draft retained")?;
        self.job = Some(Job {
            handle,
            cancel,
            reported: false,
        });
        Ok(())
    }
    pub fn poll(&mut self) -> Option<Outcome> {
        let job = self.job.as_mut()?;
        if job.handle.is_finished() {
            let job = self.job.take().unwrap();
            let result = job
                .handle
                .join()
                .unwrap_or_else(|_| Err("Audit inspection failed".into()));
            return (!job.reported).then_some(result);
        }
        // The person's cancel reports at once; the slot stays busy until the
        // worker settles, and a late reply is discarded.
        if !job.reported && job.cancel.stopped.load(Ordering::Acquire) {
            job.reported = true;
            return Some(Err("Audit inspection cancelled".into()));
        }
        None
    }
    pub fn cancel(&self) {
        if let Some(job) = &self.job {
            job.cancel.cancel();
        }
    }
}
impl Drop for Worker {
    fn drop(&mut self) {
        self.cancel();
        let deadline = Instant::now() + Duration::from_millis(100);
        while self
            .job
            .as_ref()
            .is_some_and(|job| !job.handle.is_finished())
            && Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(2));
        }
        if self
            .job
            .as_ref()
            .is_some_and(|job| job.handle.is_finished())
        {
            let _ = self.job.take().unwrap().handle.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn held_audit_waits_and_cancel_retains_slot_and_discards_late_audit() {
        let (release, wait) = std::sync::mpsc::channel();
        let mut worker = Worker {
            notice: Default::default(),
            job: Some(Job {
                handle: std::thread::spawn(move || {
                    wait.recv().unwrap();
                    Ok(pb::AuditReply::default())
                }),
                cancel: Arc::new(Cancellation::default()),
                reported: false,
            }),
        };
        // CW-01: a held audit reply stays pending; no timer reports it.
        std::thread::sleep(Duration::from_millis(50));
        assert!(worker.poll().is_none());
        // CW-02: /cancel ends the wait at once.
        worker.cancel();
        assert_eq!(
            worker.poll().unwrap().unwrap_err(),
            "Audit inspection cancelled"
        );
        assert!(worker.running());
        assert!(
            worker
                .job
                .as_ref()
                .unwrap()
                .cancel
                .stopped
                .load(Ordering::Acquire)
        );
        assert!(worker.poll().is_none());
        release.send(()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(1);
        while !worker.job.as_ref().unwrap().handle.is_finished() {
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
        assert!(worker.poll().is_none());
        assert!(!worker.running());
    }
    #[test]
    fn cancellation_disconnects_a_pending_transport() {
        use std::io::Read;
        let (client, mut peer) = UnixStream::pair().unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
        let cancel = Cancellation {
            stopped: AtomicBool::new(false),
            socket: Mutex::new(Some(client)),
        };
        cancel.cancel();
        assert_eq!(peer.read(&mut [0; 1]).unwrap(), 0);
    }
}
