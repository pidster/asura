//! One bounded `/policy` transport slot (security-policy.md, PL3 control
//! and presentation). The service owns admission and its results; this
//! worker reads the file the person named, carries one request and returns
//! its immediate reply. The candidate's result arrives in the observed
//! `PolicyStatus`, not here.
use super::observation::Resolver;
use asura_client::PolicyRequest;
use asura_control::pb;
use asura_control::policy::SCOPE_USER;
use std::path::PathBuf;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// What `/policy` asks the service for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Command {
    /// `/policy admit FILE`: the worker reads FILE, then submits it.
    Admit(PathBuf),
    Rollback(u64),
    Cancel([u8; 16]),
}

#[derive(Clone, Debug)]
pub(super) struct Request {
    pub command: Command,
    /// The typed command, kept in the editor until the reply.
    pub draft: String,
}

#[derive(Debug)]
pub(super) enum Outcome {
    Reply(Box<pb::PolicyReply>),
    /// A definite failure: nothing was queued or cancelled.
    Definite(String),
    /// The wait ended without a reply: the request may have been applied.
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

#[derive(Debug)]
pub(super) struct Update {
    pub request: Request,
    pub outcome: Outcome,
}

/// The request a command sends; an admit reads its file here, off the
/// render path.
fn request(command: &Command) -> Result<PolicyRequest, String> {
    let request_id = Some(asura_platform::random_id().to_vec());
    Ok(match command {
        Command::Admit(path) => PolicyRequest::Submit(pb::PolicyCandidateSubmit {
            request_id,
            scope: Some(SCOPE_USER),
            text: Some(crate::policy_text::read_text(path)?),
        }),
        Command::Rollback(revision) => PolicyRequest::Rollback(pb::PolicyRollback {
            request_id,
            scope: Some(SCOPE_USER),
            revision: Some(*revision),
        }),
        Command::Cancel(candidate) => PolicyRequest::Cancel(pb::PolicyCancel {
            request_id,
            candidate_id: Some(candidate.to_vec()),
        }),
    })
}

/// One `/policy` wait. It has no time bound (owner, 2026-10-16): it ends on
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
            return Err("Policy request pending; draft retained".into());
        }
        let cancel = Arc::new(AtomicBool::new(false));
        let retained = request.clone();
        let stopped = cancel.clone();
        let notice = self.notice.clone();
        let handle = std::thread::Builder::new()
            .name("asura-policy-client".into())
            .spawn(move || {
                let _settlement = notice.guard();
                let message = match self::request(&request.command) {
                    Ok(message) => message,
                    Err(error) => {
                        return Update {
                            request,
                            outcome: Outcome::Definite(error),
                        };
                    }
                };
                let run = || -> Result<pb::PolicyReply, asura_client::Error> {
                    if stopped.load(Ordering::Acquire) {
                        return Err(asura_client::Error::Unavailable);
                    }
                    let runtime = resolve(false).map_err(asura_client::Error::from)?;
                    let mut client = asura_client::Client::attach(
                        &runtime,
                        concat!("asura/", env!("CARGO_PKG_VERSION")),
                        &|| stopped.load(Ordering::Acquire),
                    )?;
                    client.policy(message, &|| stopped.load(Ordering::Acquire))
                };
                let outcome = match run() {
                    Ok(reply) => Outcome::Reply(Box::new(reply)),
                    Err(error) => classify(error),
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
        Some(job.handle.join().unwrap_or_else(|_| Update {
            request: job.request,
            outcome: Outcome::Unconfirmed("policy request worker failed".into()),
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
    fn requests_and_outcomes() {
        assert!(matches!(
            request(&Command::Rollback(3)),
            Ok(PolicyRequest::Rollback(pb::PolicyRollback {
                revision: Some(3),
                scope: Some(SCOPE_USER),
                ..
            }))
        ));
        assert!(matches!(
            request(&Command::Cancel([1; 16])),
            Ok(PolicyRequest::Cancel(_))
        ));
        // An unreadable file never reaches the service.
        let missing = std::env::temp_dir().join("asura-policy-no-such-file.cedar");
        assert!(request(&Command::Admit(missing)).is_err());
        assert!(matches!(
            classify(asura_client::Error::Remote(3, "invalid".into())),
            Outcome::Definite(_)
        ));
        assert!(matches!(
            classify(asura_client::Error::OutcomeUnconfirmed),
            Outcome::Unconfirmed(_)
        ));
    }
}
