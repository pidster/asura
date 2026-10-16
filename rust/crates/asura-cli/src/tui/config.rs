//! Bounded client-only command worker. Configuration ownership stays in the service.
use super::observation::Resolver;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Request {
    pub draft: String,
    pub key: String,
    pub value: Option<String>,
}
pub(super) fn parse(draft: &str) -> Result<Request, &'static str> {
    let usage = "Use /config, /config get name or /config set name value";
    let mut rest = draft;
    fn word<'a>(rest: &mut &'a str) -> &'a str {
        *rest = rest.trim_start();
        let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
        let result = &rest[..end];
        *rest = &rest[end..];
        result
    }
    if word(&mut rest) != "/config" {
        return Err(usage);
    }
    let operation = word(&mut rest);
    if operation.is_empty() {
        return Ok(Request {
            draft: draft.into(),
            key: String::new(),
            value: None,
        });
    }
    let key = word(&mut rest);
    if key.is_empty()
        || key.len() > 128
        || key.split('.').any(|part| {
            part.is_empty() || !part.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_')
        })
    {
        return Err("Invalid dotted configuration key");
    }
    let value = match (operation, rest.trim()) {
        ("get", "") => None,
        ("set", value) if !value.is_empty() && value.len() <= 4096 => Some(value.to_owned()),
        _ => return Err(usage),
    };
    Ok(Request {
        draft: draft.into(),
        key: key.into(),
        value,
    })
}

pub(super) type Outcome = Result<String, String>;
/// One configuration wait. It has no time bound (owner, 2026-10-16): it ends
/// on the reply, the person's `/cancel`, exit or a lost connection.
struct Job {
    handle: JoinHandle<Outcome>,
    cancel: Arc<AtomicBool>,
    /// A set may still commit after its wait is cancelled.
    mutation: bool,
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
    pub fn submit(&mut self, resolve: Resolver, request: Request) -> Result<(), &'static str> {
        if self.job.is_some() {
            return Err("Configuration request busy; draft retained");
        }
        let cancel = Arc::new(AtomicBool::new(false));
        let token = cancel.clone();
        let mutation = request.value.is_some();
        let notice = self.notice.clone();
        let handle = std::thread::Builder::new()
            .name("asura-config-client".into())
            .spawn(move || {
                let _settlement = notice.guard();
                let run = || {
                    if token.load(Ordering::Acquire) {
                        return Err(asura_client::Error::Unavailable);
                    }
                    let runtime = resolve(false)?;
                    let cancelled = || token.load(Ordering::Acquire);
                    let mut client = asura_client::Client::attach(
                        &runtime,
                        concat!("asura/", env!("CARGO_PKG_VERSION")),
                        &cancelled,
                    )?;
                    if cancelled() {
                        return Err(asura_client::Error::Unavailable);
                    }
                    client.config(&request.key, request.value.as_deref(), &cancelled)
                };
                match run() {
                    Ok(reply) => match (reply.value_yaml, reply.error) {
                        (Some(value), None) if request.key.is_empty() => Ok(value),
                        (Some(value), None) => Ok(format!(
                            "{}{}\n{}",
                            if request.value.is_some() {
                                "Saved "
                            } else {
                                ""
                            },
                            request.key,
                            value
                        )),
                        (None, Some(error)) => Err(error),
                        _ => Err("Invalid configuration response".into()),
                    },
                    Err(error) => Err(error.to_string()),
                }
            })
            .map_err(|_| "Cannot start configuration request; draft retained")?;
        self.job = Some(Job {
            handle,
            cancel,
            mutation,
            reported: false,
        });
        Ok(())
    }
    pub fn poll(&mut self) -> Option<Outcome> {
        let job = self.job.as_mut()?;
        if job.handle.is_finished() {
            let job = self.job.take().unwrap();
            let outcome = job
                .handle
                .join()
                .unwrap_or_else(|_| Err("Configuration outcome unconfirmed".into()));
            return (!job.reported).then_some(outcome);
        }
        // The person's cancel reports at once; the slot stays busy until the
        // worker settles, and its late reply is discarded.
        if !job.reported && job.cancel.load(Ordering::Acquire) {
            job.reported = true;
            return Some(Err(if job.mutation {
                "Configuration wait stopped; outcome unconfirmed, use /config get to check".into()
            } else {
                "Configuration request cancelled".into()
            }));
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
    fn command_preserves_yaml_and_rejects_bad_arity() {
        assert_eq!(
            parse("/config set model 'provider: my model'")
                .unwrap()
                .value
                .as_deref(),
            Some("'provider: my model'")
        );
        assert_eq!(
            parse("/config get audit.keepFiles").unwrap().key,
            "audit.keepFiles"
        );
        for bad in [
            "/config get",
            "/config set model",
            "/config get model value",
            "/config list model",
            "/config get audit..enabled",
            "/config get .model",
            "/config get audit/enabled",
        ] {
            assert!(parse(bad).is_err(), "{bad}");
        }
        for draft in ["/config", " /config  "] {
            let request = parse(draft).unwrap();
            assert!(request.key.is_empty());
            assert!(request.value.is_none());
        }
    }
    #[test]
    fn held_reply_waits_and_cancel_keeps_slot_and_discards_late_reply() {
        let (release, wait) = std::sync::mpsc::channel();
        let handle = std::thread::spawn(move || {
            wait.recv().unwrap();
            Ok("late".into())
        });
        let mut worker = Worker {
            notice: Default::default(),
            job: Some(Job {
                handle,
                cancel: Arc::new(AtomicBool::new(false)),
                mutation: true,
                reported: false,
            }),
        };
        // CW-01: a held reply past the removed 5 s job bound stays pending.
        std::thread::sleep(Duration::from_millis(50));
        assert!(worker.poll().is_none());
        assert!(worker.running());
        // CW-02: the person's cancel ends the wait as unconfirmed for a set.
        worker.cancel();
        assert!(worker.poll().unwrap().unwrap_err().contains("unconfirmed"));
        assert!(worker.job.is_some());
        assert!(worker.poll().is_none());
        release.send(()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(1);
        while !worker.job.as_ref().unwrap().handle.is_finished() {
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
        assert!(worker.poll().is_none());
        assert!(worker.job.is_none());
    }
}
