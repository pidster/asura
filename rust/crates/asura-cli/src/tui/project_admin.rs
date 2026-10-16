//! One bounded project administration transport slot; registry policy stays in the service.
use super::observation::Resolver;
use asura_control::pb;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

#[derive(Clone, Debug)]
pub(super) struct Request {
    pub command: pb::ProjectRename,
    pub draft: String,
    pub epoch: String,
    pub editor_generation: u64,
}

pub(super) enum Outcome {
    Reply(Box<pb::ProjectRenameReply>),
    Definite(String),
    Unconfirmed(String),
}
fn classify(error: asura_client::Error) -> Outcome {
    match error {
        asura_client::Error::Remote(_, reason) if reason != "outcome_unconfirmed" => {
            Outcome::Definite(reason)
        }
        error => Outcome::Unconfirmed(error.to_string()),
    }
}

pub(super) struct Update {
    pub request: Request,
    pub outcome: Outcome,
    pub service_epoch: Option<[u8; 16]>,
    pub settled: bool,
}

/// One project administration wait. It has no time bound (owner, 2026-10-16):
/// it ends on the reply, the person's `/cancel`, exit or a lost connection.
struct Job {
    handle: JoinHandle<Update>,
    request: Request,
    cancel: Arc<AtomicBool>,
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
    pub fn is_busy(&self) -> bool {
        self.job.is_some()
    }
    pub fn submit(&mut self, resolve: Resolver, request: Request) -> Result<(), String> {
        if self.is_busy() {
            return Err("Project administration busy; draft retained".into());
        }
        let cancel = Arc::new(AtomicBool::new(false));
        let retained = request.clone();
        let stopped = cancel.clone();
        let notice = self.notice.clone();
        let handle = std::thread::Builder::new()
            .name("asura-project-admin-client".into())
            .spawn(move || {
                let _settlement = notice.guard();
                let mut service_epoch = None;
                let mut run = || -> Result<pb::ProjectRenameReply, asura_client::Error> {
                    if stopped.load(Ordering::Acquire) {
                        return Err(asura_client::Error::Unavailable);
                    }
                    let runtime = resolve(false).map_err(asura_client::Error::from)?;
                    let mut client = asura_client::Client::attach(
                        &runtime,
                        concat!("asura/", env!("CARGO_PKG_VERSION")),
                        &|| stopped.load(Ordering::Acquire),
                    )?;
                    service_epoch = Some(client.service_epoch());
                    if stopped.load(Ordering::Acquire) {
                        return Err(asura_client::Error::Unavailable);
                    }
                    // The rename waits for the journal with no client budget;
                    // the person's /cancel, exit or a lost connection stops the wait.
                    client.rename_project(request.command.clone(), &|| {
                        stopped.load(Ordering::Acquire)
                    })
                };
                let outcome = match run() {
                    Ok(reply) => Outcome::Reply(Box::new(reply)),
                    Err(error) => classify(error),
                };
                Update {
                    request,
                    outcome,
                    service_epoch,
                    settled: true,
                }
            })
            .map_err(|e| e.to_string())?;
        self.job = Some(Job {
            handle,
            request: retained,
            cancel,
        });
        Ok(())
    }
    pub fn poll(&mut self) -> Option<Update> {
        let job = self.job.as_ref()?;
        if job.handle.is_finished() {
            let job = self.job.take().unwrap();
            let update = job.handle.join().unwrap_or_else(|_| Update {
                request: job.request,
                outcome: Outcome::Unconfirmed("Project worker failed".into()),
                service_epoch: None,
                settled: true,
            });
            return Some(update);
        }
        None
    }
    pub fn cancel(&self) {
        if let Some(job) = &self.job {
            job.cancel.store(true, Ordering::Release);
        }
    }
}
impl Drop for Worker {
    fn drop(&mut self) {
        self.cancel();
        let end = Instant::now() + Duration::from_millis(100);
        while self
            .job
            .as_ref()
            .is_some_and(|job| !job.handle.is_finished())
            && Instant::now() < end
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
    fn remote_rejection_is_definite_but_unknown_commit_is_retained() {
        assert!(matches!(
            classify(asura_client::Error::Remote(
                3,
                "invalid_project_name".into()
            )),
            Outcome::Definite(_)
        ));
        assert!(matches!(
            classify(asura_client::Error::Remote(6, "outcome_unconfirmed".into())),
            Outcome::Unconfirmed(_)
        ));
        assert!(matches!(
            classify(asura_client::Error::OutcomeUnconfirmed),
            Outcome::Unconfirmed(_)
        ));
    }
    #[test]
    fn panicked_worker_reports_uncertain_completion_and_releases_slot() {
        let request = Request {
            command: pb::ProjectRename {
                request_id: Some(vec![1; 16]),
                project_id: Some(vec![2; 16]),
                expected_name_revision: Some(0),
                name: Some("Test".into()),
            },
            draft: "/project rename Test".into(),
            epoch: "epoch".into(),
            editor_generation: 1,
        };
        let handle = std::thread::spawn(|| -> Update { panic!("fixture failure") });
        while !handle.is_finished() {
            std::thread::yield_now();
        }
        let mut worker = Worker {
            notice: Default::default(),
            job: Some(Job {
                handle,
                request,
                cancel: Arc::new(AtomicBool::new(false)),
            }),
        };
        let update = worker.poll().expect("panic is a reported outcome");
        assert!(matches!(update.outcome, Outcome::Unconfirmed(_)));
        assert!(!worker.is_busy());
    }
    #[test]
    fn held_rename_waits_until_cancel_settles_it_unconfirmed() {
        let request = Request {
            command: pb::ProjectRename {
                request_id: Some(vec![1; 16]),
                project_id: Some(vec![2; 16]),
                expected_name_revision: Some(0),
                name: Some("Test".into()),
            },
            draft: "/project rename Test".into(),
            epoch: "epoch".into(),
            editor_generation: 1,
        };
        let cancel = Arc::new(AtomicBool::new(false));
        let stopped = cancel.clone();
        let held = request.clone();
        // The worker waits like the client: only the cancel flag ends it.
        let handle = std::thread::spawn(move || {
            while !stopped.load(Ordering::Acquire) {
                std::thread::sleep(Duration::from_millis(5));
            }
            Update {
                request: held,
                outcome: classify(asura_client::Error::OutcomeUnconfirmed),
                service_epoch: None,
                settled: true,
            }
        });
        let mut worker = Worker {
            notice: Default::default(),
            job: Some(Job {
                handle,
                request,
                cancel,
            }),
        };
        // CW-01: the removed 5 s job bound no longer reports anything.
        std::thread::sleep(Duration::from_millis(50));
        assert!(worker.poll().is_none());
        assert!(worker.is_busy());
        // CW-02: /cancel ends the wait; the outcome is unconfirmed.
        worker.cancel();
        let update = loop {
            if let Some(update) = worker.poll() {
                break update;
            }
            std::thread::sleep(Duration::from_millis(5));
        };
        assert!(matches!(update.outcome, Outcome::Unconfirmed(_)));
        assert!(update.settled);
        assert!(!worker.is_busy());
    }
}
