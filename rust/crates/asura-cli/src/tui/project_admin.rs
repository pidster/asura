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
    Reply(pb::ProjectRenameReply),
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

struct Job {
    handle: JoinHandle<Update>,
    request: Request,
    cancel: Arc<AtomicBool>,
    deadline: Instant,
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
                        Instant::now() + Duration::from_secs(2),
                    )?;
                    service_epoch = Some(client.service_epoch());
                    if stopped.load(Ordering::Acquire) {
                        return Err(asura_client::Error::Unavailable);
                    }
                    client.rename_project(request.command.clone())
                };
                let outcome = match run() {
                    Ok(reply) => Outcome::Reply(reply),
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
            deadline: Instant::now() + Duration::from_secs(5),
            reported: false,
        });
        Ok(())
    }
    pub fn next_deadline(&self) -> Option<Instant> {
        self.job
            .as_ref()
            .filter(|job| !job.reported)
            .map(|job| job.deadline)
    }
    pub fn poll(&mut self) -> Option<Update> {
        let job = self.job.as_mut()?;
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
        if !job.reported && Instant::now() >= job.deadline {
            job.reported = true;
            job.cancel.store(true, Ordering::Release);
            // The TUI retains the exact request ID and waits for settlement before retry.
            return Some(Update {
                request: job.request.clone(),
                outcome: Outcome::Unconfirmed("Project request deadline expired".into()),
                service_epoch: None,
                settled: false,
            });
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
                deadline: Instant::now() + Duration::from_secs(1),
                reported: false,
            }),
        };
        let update = worker.poll().expect("panic is a reported outcome");
        assert!(matches!(update.outcome, Outcome::Unconfirmed(_)));
        assert!(!worker.is_busy());
    }
}
