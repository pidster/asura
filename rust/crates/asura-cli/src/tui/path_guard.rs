//! One bounded `/paths` transport slot (execution-path-guard.md, TUI and
//! control messages). The service owns the lists, the checks and the
//! approvals; this worker only carries one request and its reply.
use super::observation::Resolver;
use asura_client::PathGuardRequest;
use asura_control::pb;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

#[derive(Clone, Debug)]
pub(super) struct Request {
    pub command: PathGuardRequest,
    /// The typed command, kept in the editor until the reply.
    pub draft: String,
}

#[derive(Debug)]
pub(super) enum Outcome {
    Reply(Box<pb::PathGuardReply>),
    /// A definite failure: nothing was applied.
    Definite(String),
    /// The wait ended without a reply: an approval may have been applied.
    Unconfirmed(String),
}
fn classify(error: asura_client::Error, mutation: bool) -> Outcome {
    match error {
        asura_client::Error::Remote(_, reason) if reason != "outcome_unconfirmed" => {
            Outcome::Definite(reason)
        }
        error if mutation => Outcome::Unconfirmed(error.to_string()),
        error => Outcome::Definite(error.to_string()),
    }
}

#[derive(Debug)]
pub(super) struct Update {
    pub request: Request,
    pub outcome: Outcome,
}

/// One `/paths` wait. It has no time bound (owner, 2026-10-16): it ends on
/// the reply, the person's `/cancel`, exit or a lost connection.
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
    pub fn submit(&mut self, resolve: Resolver, request: Request) -> Result<(), String> {
        if self.job.is_some() {
            return Err("Path request pending; draft retained".into());
        }
        let cancel = Arc::new(AtomicBool::new(false));
        let retained = request.clone();
        let stopped = cancel.clone();
        let notice = self.notice.clone();
        let handle = std::thread::Builder::new()
            .name("asura-path-guard-client".into())
            .spawn(move || {
                let _settlement = notice.guard();
                let mutation = request.command.mutation();
                let run = || -> Result<pb::PathGuardReply, asura_client::Error> {
                    if stopped.load(Ordering::Acquire) {
                        return Err(asura_client::Error::Unavailable);
                    }
                    let runtime = resolve(false).map_err(asura_client::Error::from)?;
                    let mut client = asura_client::Client::attach(
                        &runtime,
                        concat!("asura/", env!("CARGO_PKG_VERSION")),
                        &|| stopped.load(Ordering::Acquire),
                    )?;
                    // Part of capability 8: the gate is read on this
                    // attachment, as the queue sender reads it.
                    if !client.supports(pb::Capability::PersonCommand) {
                        return Err(asura_client::Error::Remote(
                            0,
                            super::queue::COMMANDS_NOT_ENABLED.into(),
                        ));
                    }
                    client.path_guard(request.command.clone(), &|| stopped.load(Ordering::Acquire))
                };
                let outcome = match run() {
                    Ok(reply) => Outcome::Reply(Box::new(reply)),
                    Err(error) => classify(error, mutation),
                };
                Update { request, outcome }
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
        if !job.handle.is_finished() {
            return None;
        }
        let job = self.job.take().unwrap();
        let mutation = job.request.command.mutation();
        Some(job.handle.join().unwrap_or_else(|_| Update {
            request: job.request,
            outcome: if mutation {
                Outcome::Unconfirmed("path request worker failed".into())
            } else {
                Outcome::Definite("path request worker failed".into())
            },
        }))
    }
    pub fn cancel(&self) {
        if let Some(job) = &self.job {
            job.cancel.store(true, Ordering::Release);
        }
    }
}
impl Drop for Worker {
    fn drop(&mut self) {
        // As the other transport slots: the cancel ends the client's wait;
        // a worker still settling after the grace is detached, not joined.
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
    fn a_lost_approval_is_unconfirmed_and_a_lost_list_is_not() {
        assert!(matches!(
            classify(asura_client::Error::Remote(3, "invalid".into()), true),
            Outcome::Definite(_)
        ));
        assert!(matches!(
            classify(asura_client::Error::OutcomeUnconfirmed, true),
            Outcome::Unconfirmed(_)
        ));
        assert!(matches!(
            classify(
                asura_client::Error::Remote(6, "outcome_unconfirmed".into()),
                true
            ),
            Outcome::Unconfirmed(_)
        ));
        assert!(matches!(
            classify(asura_client::Error::Unavailable, false),
            Outcome::Definite(_)
        ));
    }
}
