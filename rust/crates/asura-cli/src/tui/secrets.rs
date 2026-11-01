//! One bounded `/secrets` transport slot (named-secrets.md, control API).
//! The service owns the store; this worker carries one request and its
//! reply. A store's value moves into this thread in zeroizing memory and
//! the client clears the request message after the write; nothing here
//! logs or prints it.
use super::observation::Resolver;
use asura_control::pb;
use asura_control::secrets::Zeroizing;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// What a `/secrets` command asks.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Kind {
    List,
    Store(String),
    Replace(String),
    Remove(String),
}
impl Kind {
    fn mutation(&self) -> bool {
        !matches!(self, Self::List)
    }
}

/// One request: what it asks and the typed command, kept in the editor
/// until the reply. The value travels beside it, never in it.
#[derive(Clone, Debug)]
pub(super) struct Request {
    pub kind: Kind,
    pub draft: String,
}

#[derive(Debug)]
pub(super) enum Outcome {
    Reply(Box<pb::SecretReply>),
    /// A definite failure: nothing changed.
    Definite(String),
    /// The wait ended without a reply: a store or remove may have applied.
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

/// One `/secrets` wait. No time bound (owner, 2026-10-16): it ends on the
/// reply, the person's `/cancel`, exit or a lost connection.
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
    /// Starts one request. `value` is the captured value of a store or
    /// replace; it is dropped (zeroed) when the thread ends.
    pub fn submit(
        &mut self,
        resolve: Resolver,
        request: Request,
        value: Option<Zeroizing<Vec<u8>>>,
    ) -> Result<(), String> {
        if self.job.is_some() {
            return Err("Secrets request pending; draft retained".into());
        }
        let cancel = Arc::new(AtomicBool::new(false));
        let retained = request.clone();
        let stopped = cancel.clone();
        let notice = self.notice.clone();
        let handle = std::thread::Builder::new()
            .name("asura-secrets-client".into())
            .spawn(move || {
                let _settlement = notice.guard();
                let mutation = request.kind.mutation();
                let run = || -> Result<pb::SecretReply, asura_client::Error> {
                    if stopped.load(Ordering::Acquire) {
                        return Err(asura_client::Error::Unavailable);
                    }
                    let runtime = resolve(false).map_err(asura_client::Error::from)?;
                    let mut client = asura_client::Client::attach(
                        &runtime,
                        concat!("asura/", env!("CARGO_PKG_VERSION")),
                        &|| stopped.load(Ordering::Acquire),
                    )?;
                    if !client.supports(pb::Capability::PersonCommand) {
                        return Err(asura_client::Error::Remote(
                            0,
                            super::queue::COMMANDS_NOT_ENABLED.into(),
                        ));
                    }
                    let cancelled = || stopped.load(Ordering::Acquire);
                    match &request.kind {
                        Kind::List => client.secret_list(&cancelled),
                        Kind::Remove(name) => client.secret_remove(name.clone(), &cancelled),
                        Kind::Store(name) | Kind::Replace(name) => {
                            let value = value.as_ref().ok_or(asura_client::Error::Protocol)?;
                            client.secret_put(
                                pb::SecretPut {
                                    name: Some(name.clone()),
                                    value: Some(value.to_vec()),
                                    replace: Some(matches!(request.kind, Kind::Replace(_))),
                                },
                                &cancelled,
                            )
                        }
                    }
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
        let job = self.job.take()?;
        let mutation = job.request.kind.mutation();
        Some(job.handle.join().unwrap_or_else(|_| Update {
            request: job.request,
            outcome: if mutation {
                Outcome::Unconfirmed("secrets request worker failed".into())
            } else {
                Outcome::Definite("secrets request worker failed".into())
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
        if let Some(job) = self.job.take_if(|job| job.handle.is_finished()) {
            let _ = job.handle.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn a_lost_store_is_unconfirmed_and_a_lost_list_is_not() {
        assert!(matches!(
            classify(asura_client::Error::OutcomeUnconfirmed, true),
            Outcome::Unconfirmed(_)
        ));
        assert!(matches!(
            classify(asura_client::Error::Unavailable, false),
            Outcome::Definite(_)
        ));
        assert!(matches!(
            classify(
                asura_client::Error::Remote(3, "service_draining".into()),
                true
            ),
            Outcome::Definite(_)
        ));
    }
}
