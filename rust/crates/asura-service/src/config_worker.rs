//! One bounded filesystem job. Its slot and cancellation live until settlement.
use asura_control::pb;
use asura_platform::RuntimeDirectory;
use std::{
    io,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

pub(crate) struct Worker {
    task: Option<JoinHandle<Result<String, &'static str>>>,
    cancel: Arc<AtomicBool>,
    request: pb::Envelope,
    deadline: Instant,
    replied: bool,
    mutation: bool,
    audit_event: Option<asura_storage::audit::Event>,
    completion: crate::completion::Completion,
}
impl Worker {
    pub(crate) fn start(
        runtime: RuntimeDirectory,
        request: pb::Envelope,
        now: Instant,
    ) -> io::Result<Self> {
        let (key, value) = match request.body.as_ref() {
            Some(pb::envelope::Body::ConfigGet(v)) => (v.key.clone().unwrap_or_default(), None),
            Some(pb::envelope::Body::ConfigSet(v)) => {
                (v.key.clone().unwrap(), v.value_yaml.clone())
            }
            _ => unreachable!("validated config request"),
        };
        let deadline = now + Duration::from_secs(2);
        let cancel = Arc::new(AtomicBool::new(false));
        let token = cancel.clone();
        let mutation = value.is_some();
        let completion = crate::completion::Completion::default();
        let signal = completion.clone();
        let task = thread::Builder::new()
            .name("asura-config".into())
            .spawn(move || {
                let _completion = signal.guard();
                asura_storage::config::execute(runtime, &key, value.as_deref(), deadline, &token)
            })?;
        Ok(Self {
            task: Some(task),
            cancel,
            request,
            deadline,
            replied: false,
            mutation,
            audit_event: None,
            completion,
        })
    }
    pub(crate) fn register(&self, wake: asura_platform::events::WakeSender) {
        self.completion.register(wake);
    }
    pub(crate) fn next_deadline(&self, now: Instant) -> Option<Instant> {
        if self.task.is_none() {
            None
        } else if self.completion.completed() {
            self.completion.settlement_deadline(now)
        } else if !self.replied {
            Some(self.deadline)
        } else {
            None
        }
    }
    pub(crate) fn cancel(&self) {
        self.cancel.store(true, Ordering::Release);
    }
    pub(crate) fn settled(&self) -> bool {
        self.task.is_none()
    }
    pub(crate) fn take_audit_event(&mut self) -> Option<asura_storage::audit::Event> {
        self.audit_event.take()
    }
    pub(crate) fn poll(&mut self, now: Instant) -> Option<pb::Envelope> {
        let done = self.task.as_ref().is_some_and(JoinHandle::is_finished);
        let result = if done {
            Some(
                self.task
                    .take()
                    .unwrap()
                    .join()
                    .unwrap_or(Err(if self.mutation {
                        "outcome_unconfirmed"
                    } else {
                        "config_unavailable"
                    })),
            )
        } else if now >= self.deadline && !self.replied {
            self.cancel();
            Some(Err(if self.mutation {
                "outcome_unconfirmed"
            } else {
                "config_timeout"
            }))
        } else {
            None
        };
        if done
            && self.mutation
            && let Some(result) = &result
        {
            use asura_storage::audit::{ConfigOutcome, Event, Setting};
            let outcome = match result {
                Ok(_) => Some(ConfigOutcome::Persisted),
                Err("outcome_unconfirmed" | "config_outcome_unconfirmed") => {
                    Some(ConfigOutcome::Unconfirmed)
                }
                _ => None,
            };
            let key = match &self.request.body {
                Some(pb::envelope::Body::ConfigSet(v)) => v.key.as_deref(),
                _ => None,
            };
            let setting = match key {
                Some("model") => Some(Setting::Model),
                Some("audit") => Some(Setting::Audit),
                Some("audit.enabled") => Some(Setting::AuditEnabled),
                Some("audit.keepFiles") => Some(Setting::AuditKeepFiles),
                Some("audit.maxFileBytes") => Some(Setting::AuditMaxFileBytes),
                Some("providers.mlx.models") => Some(Setting::ProvidersMlxModels),
                _ => None,
            };
            if let (Some(setting), Some(outcome)) = (setting, outcome) {
                self.audit_event = Some(Event::ConfigChanged { setting, outcome });
            }
        }
        if self.replied {
            return None;
        }
        let result = result?;
        self.replied = true;
        let (value_yaml, error) = match result {
            Ok(value) => (Some(value), None),
            Err(error) => (None, Some(error.into())),
        };
        let mut reply = self.request.clone();
        reply.body = Some(pb::envelope::Body::ConfigReply(pb::ConfigReply {
            value_yaml,
            error,
        }));
        Some(reply)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    fn blocked(mutation: bool) -> (Worker, mpsc::SyncSender<()>) {
        let (send, receive) = mpsc::sync_channel(1);
        let task = thread::spawn(move || {
            receive.recv().unwrap();
            Ok("true\n".into())
        });
        let request = pb::Envelope {
            service_epoch: Some(vec![1; 16]),
            attachment_id: Some(vec![2; 16]),
            request_counter: Some(7),
            body: Some(if mutation {
                pb::envelope::Body::ConfigSet(pb::ConfigSet {
                    key: Some("audit.enabled".into()),
                    value_yaml: Some("true".into()),
                })
            } else {
                pb::envelope::Body::ConfigGet(pb::ConfigGet {
                    key: Some("audit.enabled".into()),
                })
            }),
        };
        (
            Worker {
                task: Some(task),
                cancel: Arc::new(AtomicBool::new(false)),
                request,
                deadline: Instant::now(),
                replied: false,
                mutation,
                audit_event: None,
                completion: Default::default(),
            },
            send,
        )
    }
    #[test]
    fn expiry_retains_slot_until_settlement_and_never_publishes_late_success() {
        for mutation in [false, true] {
            let (mut worker, release) = blocked(mutation);
            let reply = worker.poll(Instant::now()).unwrap();
            assert_eq!(reply.request_counter, Some(7));
            assert_eq!(reply.attachment_id, Some(vec![2; 16]));
            let Some(pb::envelope::Body::ConfigReply(reply)) = reply.body else {
                panic!()
            };
            assert_eq!(
                reply.error.as_deref(),
                Some(if mutation {
                    "outcome_unconfirmed"
                } else {
                    "config_timeout"
                })
            );
            assert!(!worker.settled());
            assert!(worker.take_audit_event().is_none());
            assert!(worker.cancel.load(Ordering::Acquire));
            release.send(()).unwrap();
            let end = Instant::now() + Duration::from_secs(1);
            while !worker.settled() {
                assert!(Instant::now() < end);
                assert!(worker.poll(Instant::now()).is_none());
                thread::yield_now();
            }
            let event = worker.take_audit_event();
            if mutation {
                assert!(matches!(
                    event,
                    Some(asura_storage::audit::Event::ConfigChanged {
                        setting: asura_storage::audit::Setting::AuditEnabled,
                        outcome: asura_storage::audit::ConfigOutcome::Persisted,
                    })
                ));
            } else {
                assert!(event.is_none());
            }
            assert!(worker.take_audit_event().is_none());
        }
    }
}

#[cfg(test)]
mod reactor_tests {
    use super::*;
    use asura_client::Client;
    use std::{fs, os::unix::fs::DirBuilderExt, sync::mpsc};

    #[test]
    fn stalled_config_keeps_controls_responsive_and_owner_until_settlement() {
        let _writer_owner = crate::TEST_WRITER
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let path = std::path::PathBuf::from(format!(
            "/private/tmp/asura-config-reactor-{:x}",
            u128::from_ne_bytes(asura_platform::random_id())
        ));
        fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
        let runtime = RuntimeDirectory::scratch(&path, true).unwrap();
        let child_runtime = runtime.clone();
        let (admitted_tx, admitted_rx) = mpsc::sync_channel(1);
        let (release_tx, release_rx) = mpsc::sync_channel(1);
        let mut release = Some(release_rx);
        let (done_tx, done_rx) = mpsc::sync_channel(1);
        let reactor = thread::spawn(move || {
            let result = crate::run_reactor_with_config(
                child_runtime,
                "asura/test",
                None,
                None,
                crate::installation::Worker::start,
                move |_, request, now| {
                    let receive = release.take().unwrap();
                    let completion = crate::completion::Completion::default();
                    let signal = completion.clone();
                    let task = thread::spawn(move || {
                        let _completion = signal.guard();
                        receive.recv().unwrap();
                        Ok("true\n".into())
                    });
                    admitted_tx.send(()).unwrap();
                    Ok(Worker {
                        task: Some(task),
                        cancel: Arc::new(AtomicBool::new(false)),
                        request,
                        deadline: now + Duration::from_secs(2),
                        replied: false,
                        mutation: true,
                        audit_event: None,
                        completion,
                    })
                },
            );
            done_tx.send(result).unwrap();
        });
        let end = Instant::now() + Duration::from_secs(3);
        let mut client = loop {
            match Client::attach(&runtime, "asura/test", end) {
                Ok(client) => break client,
                Err(_) if Instant::now() < end => thread::sleep(Duration::from_millis(5)),
                Err(error) => panic!("attach: {error}"),
            }
        };
        let request = thread::spawn(move || client.config("audit.enabled", Some("true")));
        admitted_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let mut control = Client::attach(
            &runtime,
            "asura/test",
            Instant::now() + Duration::from_secs(2),
        )
        .unwrap();
        let started = Instant::now();
        assert_eq!(
            control.inspect().unwrap().lifecycle,
            pb::Lifecycle::Serving as i32
        );
        assert!(started.elapsed() < Duration::from_secs(1));
        assert_eq!(
            control.config("model", None).unwrap().error.as_deref(),
            Some("config_busy")
        );
        assert_eq!(
            request.join().unwrap().unwrap().error.as_deref(),
            Some("outcome_unconfirmed")
        );
        assert_eq!(
            control.config("model", None).unwrap().error.as_deref(),
            Some("config_busy")
        );
        let stop = thread::spawn(move || control.stop());
        let end = Instant::now() + Duration::from_secs(3);
        loop {
            if let Ok(mut observer) = Client::attach(&runtime, "asura/test", end)
                && let Ok(snapshot) = observer.inspect()
                && snapshot.lifecycle == pb::Lifecycle::RepairOnly as i32
            {
                break;
            }
            assert!(Instant::now() < end);
            thread::sleep(Duration::from_millis(5));
        }
        assert!(matches!(
            runtime.acquire_owner(),
            Err(asura_platform::Error::OwnerBusy)
        ));
        assert!(done_rx.try_recv().is_err());
        release_tx.send(()).unwrap();
        // The original Stop remains effective after delayed settlement.
        done_rx
            .recv_timeout(Duration::from_secs(3))
            .unwrap()
            .unwrap();
        reactor.join().unwrap();
        let _ = stop.join().unwrap();
        assert!(runtime.acquire_owner().is_ok());
        drop(runtime);
        fs::remove_dir_all(path).unwrap();
    }
}
