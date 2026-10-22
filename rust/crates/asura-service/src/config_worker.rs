//! One bounded filesystem job. Its slot and cancellation live until settlement.
//! The job has no time bound (owner, 2026-10-16): it ends on completion, an
//! error, or cancellation by drain; the client's wait ends on its reply,
//! the person's cancel or a lost connection.
use asura_control::pb;
use asura_platform::RuntimeDirectory;
use asura_storage::config::Reply;
use std::{
    io,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::Instant,
};

pub(crate) struct Worker {
    task: Option<JoinHandle<Result<Reply, &'static str>>>,
    cancel: Arc<AtomicBool>,
    request: pb::Envelope,
    replied: bool,
    mutation: bool,
    audit_event: Option<asura_storage::audit::Event>,
    journal_quota: Option<u64>,
    completion: crate::completion::Completion,
}
impl Worker {
    pub(crate) fn start(runtime: RuntimeDirectory, request: pb::Envelope) -> io::Result<Self> {
        let (key, value) = match request.body.as_ref() {
            Some(pb::envelope::Body::ConfigGet(v)) => (v.key.clone().unwrap_or_default(), None),
            Some(pb::envelope::Body::ConfigSet(v)) => {
                (v.key.clone().unwrap(), v.value_yaml.clone())
            }
            _ => unreachable!("validated config request"),
        };
        let cancel = Arc::new(AtomicBool::new(false));
        let token = cancel.clone();
        let mutation = value.is_some();
        let completion = crate::completion::Completion::default();
        let signal = completion.clone();
        let task = thread::Builder::new()
            .name("asura-config".into())
            .spawn(move || {
                let _completion = signal.guard();
                asura_storage::config::execute(runtime, &key, value.as_deref(), &token)
            })?;
        Ok(Self {
            task: Some(task),
            cancel,
            request,
            replied: false,
            mutation,
            audit_event: None,
            journal_quota: None,
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
    /// The new effective `journal.quotaBytes` after a persisted set of
    /// `journal` or `journal.quotaBytes`. The set applies at once, so the
    /// caller forwards it to the journal writer. Taken once.
    pub(crate) fn take_journal_quota(&mut self) -> Option<u64> {
        self.journal_quota.take()
    }
    pub(crate) fn poll(&mut self) -> Option<pb::Envelope> {
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
            // One mapping from a key to its audit setting (J2-G).
            let setting = key.and_then(Setting::for_config_key);
            if let (Some(setting), Some(outcome)) = (setting, outcome) {
                self.audit_event = Some(Event::ConfigChanged { setting, outcome });
            }
            if let Ok(reply) = result {
                self.journal_quota = reply.journal_quota;
            }
        }
        if self.replied {
            return None;
        }
        let result = result?;
        self.replied = true;
        let (value_yaml, error) = match result {
            Ok(reply) => (Some(reply.value_yaml), None),
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
    use std::time::Duration;

    fn blocked(mutation: bool) -> (Worker, mpsc::SyncSender<()>) {
        blocked_with(
            mutation,
            "audit.enabled",
            Ok(Reply {
                value_yaml: "true\n".into(),
                journal_quota: None,
                execution: None,
            }),
        )
    }
    fn blocked_with(
        mutation: bool,
        key: &str,
        result: Result<Reply, &'static str>,
    ) -> (Worker, mpsc::SyncSender<()>) {
        let (send, receive) = mpsc::sync_channel(1);
        let task = thread::spawn(move || {
            receive.recv().unwrap();
            result
        });
        let request = pb::Envelope {
            service_epoch: Some(vec![1; 16]),
            attachment_id: Some(vec![2; 16]),
            request_counter: Some(7),
            body: Some(if mutation {
                pb::envelope::Body::ConfigSet(pb::ConfigSet {
                    key: Some(key.into()),
                    value_yaml: Some("true".into()),
                })
            } else {
                pb::envelope::Body::ConfigGet(pb::ConfigGet {
                    key: Some(key.into()),
                })
            }),
        };
        (
            Worker {
                task: Some(task),
                cancel: Arc::new(AtomicBool::new(false)),
                request,
                replied: false,
                mutation,
                audit_event: None,
                journal_quota: None,
                completion: Default::default(),
            },
            send,
        )
    }
    fn settle(worker: &mut Worker, release: mpsc::SyncSender<()>) -> pb::ConfigReply {
        release.send(()).unwrap();
        let end = Instant::now() + Duration::from_secs(5);
        let reply = loop {
            if let Some(reply) = worker.poll() {
                break reply;
            }
            assert!(Instant::now() < end);
            thread::yield_now();
        };
        let Some(pb::envelope::Body::ConfigReply(reply)) = reply.body else {
            panic!()
        };
        reply
    }
    /// SJ-14: a persisted set of `journal.quotaBytes` or the `journal`
    /// mapping carries the new quota to the caller once, with the audit
    /// setting; gets, failures and other keys carry none.
    #[test]
    fn persisted_journal_set_reports_the_new_quota_once() {
        use asura_storage::audit::{ConfigOutcome, Event, Setting};
        for key in ["journal.quotaBytes", "journal"] {
            let (mut worker, release) = blocked_with(
                true,
                key,
                Ok(Reply {
                    value_yaml: "67108864\n".into(),
                    journal_quota: Some(67_108_864),
                    execution: None,
                }),
            );
            assert_eq!(worker.take_journal_quota(), None);
            let reply = settle(&mut worker, release);
            assert_eq!(reply.value_yaml.as_deref(), Some("67108864\n"));
            assert_eq!(worker.take_journal_quota(), Some(67_108_864));
            assert_eq!(worker.take_journal_quota(), None);
            assert_eq!(
                worker.take_audit_event(),
                Some(Event::ConfigChanged {
                    setting: Setting::JournalQuotaBytes,
                    outcome: ConfigOutcome::Persisted,
                })
            );
        }
        for error in ["config_invalid_value", "config_outcome_unconfirmed"] {
            let (mut worker, release) = blocked_with(true, "journal.quotaBytes", Err(error));
            let reply = settle(&mut worker, release);
            assert_eq!(reply.error.as_deref(), Some(error));
            assert_eq!(worker.take_journal_quota(), None);
        }
        let (mut worker, release) = blocked_with(
            false,
            "journal.quotaBytes",
            Ok(Reply {
                value_yaml: "4294967296\n".into(),
                journal_quota: None,
                execution: None,
            }),
        );
        settle(&mut worker, release);
        assert_eq!(worker.take_journal_quota(), None);
        assert_eq!(worker.take_audit_event(), None);
        let (mut worker, release) = blocked(true);
        settle(&mut worker, release);
        assert_eq!(worker.take_journal_quota(), None);
    }
    /// J2T-21 (worker part): every `jobs` key, and the whole mapping, emits
    /// `config.changed` with its own setting through `for_config_key`.
    #[test]
    fn j2t_21_jobs_keys_emit_config_changed_with_their_setting() {
        use asura_storage::audit::{ConfigOutcome, Event, Setting};
        for (key, setting) in [
            ("jobs", Setting::Jobs),
            ("jobs.liveService", Setting::JobsLiveService),
            ("jobs.liveAgent", Setting::JobsLiveAgent),
            ("jobs.queuedAgent", Setting::JobsQueuedAgent),
            ("jobs.clientViewers", Setting::JobsClientViewers),
            (
                "jobs.modelReservedPageReads",
                Setting::JobsModelReservedPageReads,
            ),
        ] {
            let (mut worker, release) = blocked_with(
                true,
                key,
                Ok(Reply {
                    value_yaml: "2\n".into(),
                    journal_quota: None,
                    execution: None,
                }),
            );
            settle(&mut worker, release);
            assert_eq!(
                worker.take_audit_event(),
                Some(Event::ConfigChanged {
                    setting,
                    outcome: ConfigOutcome::Persisted,
                }),
                "{key}"
            );
        }
        // A refused set emits nothing.
        let (mut worker, release) =
            blocked_with(true, "jobs.liveAgent", Err("config_invalid_value"));
        settle(&mut worker, release);
        assert_eq!(worker.take_audit_event(), None);
    }
    #[test]
    fn held_job_has_no_deadline_and_replies_on_completion() {
        for mutation in [false, true] {
            let (mut worker, release) = blocked(mutation);
            // Held well past the removed 2 s bound (injected clock): pending.
            let later = Instant::now() + Duration::from_secs(60);
            assert!(worker.poll().is_none());
            assert_eq!(worker.next_deadline(later), None);
            assert!(!worker.settled());
            assert!(!worker.cancel.load(Ordering::Acquire));
            release.send(()).unwrap();
            let end = Instant::now() + Duration::from_secs(1);
            let reply = loop {
                if let Some(reply) = worker.poll() {
                    break reply;
                }
                assert!(Instant::now() < end);
                thread::yield_now();
            };
            assert_eq!(reply.request_counter, Some(7));
            assert_eq!(reply.attachment_id, Some(vec![2; 16]));
            let Some(pb::envelope::Body::ConfigReply(reply)) = reply.body else {
                panic!()
            };
            assert_eq!(reply.value_yaml.as_deref(), Some("true\n"));
            assert!(worker.settled());
            assert!(worker.poll().is_none());
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
    use std::time::Duration;
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
                move |_, request| {
                    let receive = release.take().unwrap();
                    let completion = crate::completion::Completion::default();
                    let signal = completion.clone();
                    let task = thread::spawn(move || {
                        let _completion = signal.guard();
                        receive.recv().unwrap();
                        Ok(Reply {
                            value_yaml: "true\n".into(),
                            journal_quota: None,
                            execution: None,
                        })
                    });
                    admitted_tx.send(()).unwrap();
                    Ok(Worker {
                        task: Some(task),
                        cancel: Arc::new(AtomicBool::new(false)),
                        request,
                        replied: false,
                        mutation: true,
                        audit_event: None,
                        journal_quota: None,
                        completion,
                    })
                },
            );
            done_tx.send(result).unwrap();
        });
        let end = Instant::now() + Duration::from_secs(3);
        let attach = |budget: Duration| {
            let end = Instant::now() + budget;
            let cancel: asura_client::Cancel = Arc::new(move || Instant::now() >= end);
            let mut client = Client::attach(&runtime, "asura/test", &*cancel)?;
            client.set_cancel(cancel);
            Ok::<_, asura_client::Error>(client)
        };
        let mut client = loop {
            match attach(Duration::from_secs(10)) {
                Ok(client) => break client,
                Err(_) if Instant::now() < end => thread::sleep(Duration::from_millis(5)),
                Err(error) => panic!("attach: {error}"),
            }
        };
        let request =
            thread::spawn(move || client.config("audit.enabled", Some("true"), &|| false));
        admitted_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let mut control = attach(Duration::from_secs(10)).unwrap();
        let started = Instant::now();
        assert_eq!(
            control.inspect().unwrap().lifecycle,
            pb::Lifecycle::Serving as i32
        );
        assert!(started.elapsed() < Duration::from_secs(1));
        assert_eq!(
            control
                .config("model", None, &|| false)
                .unwrap()
                .error
                .as_deref(),
            Some("config_busy")
        );
        // CW-01: the held set is still waiting past the removed 2 s job and
        // 3 s client bounds; no timer answers it.
        thread::sleep(Duration::from_millis(3_200));
        assert!(!request.is_finished());
        assert_eq!(
            control
                .config("model", None, &|| false)
                .unwrap()
                .error
                .as_deref(),
            Some("config_busy")
        );
        // Q-B: Stop waits for the held job with no deadline; Inspect reports
        // Draining, never RepairOnly, and the lock is retained.
        let stop = thread::spawn(move || control.stop());
        let held = Instant::now() + Duration::from_millis(1_500);
        let mut draining = false;
        while Instant::now() < held {
            if let Ok(mut observer) = attach(Duration::from_millis(500))
                && let Ok(snapshot) = observer.inspect()
            {
                assert_ne!(snapshot.lifecycle, pb::Lifecycle::RepairOnly as i32);
                draining |= snapshot.lifecycle == pb::Lifecycle::Draining as i32;
            }
            thread::sleep(Duration::from_millis(20));
        }
        assert!(draining);
        assert!(matches!(
            runtime.acquire_owner(),
            Err(asura_platform::Error::OwnerBusy)
        ));
        // CW-03: drain closed the held set's connection: its wait ended as a
        // lost connection with an unconfirmed outcome.
        assert!(matches!(
            request.join().unwrap(),
            Err(asura_client::Error::OutcomeUnconfirmed)
        ));
        assert!(done_rx.try_recv().is_err());
        release_tx.send(()).unwrap();
        // The original Stop completes after settlement.
        done_rx
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap();
        reactor.join().unwrap();
        stop.join().unwrap().unwrap();
        assert!(runtime.acquire_owner().is_ok());
        drop(runtime);
        fs::remove_dir_all(path).unwrap();
    }
}
