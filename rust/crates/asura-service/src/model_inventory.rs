//! Service-owned read-only discovery. Blocking dependencies stay behind existing isolation owners.
use crate::model_owner::{ModelEvent, ModelOwner, PackageIdentity, Selection};
use asura_control::pb;
use asura_platform::{PollInterest, RuntimeDirectory, events::WakeSender};
use asura_storage::config::ConversationSnapshot;
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

type SnapshotResult = Result<ConversationSnapshot, &'static str>;
struct Job {
    request: pb::Envelope,
    task: Option<JoinHandle<SnapshotResult>>,
    cancel: Arc<AtomicBool>,
    completion: crate::completion::Completion,
    deadline: Instant,
    reported: bool,
    model: Option<ModelOwner>,
    configured: Option<String>,
}
impl Job {
    fn cancel(&mut self, now: Instant) {
        self.cancel.store(true, Ordering::Release);
        self.reported = true;
        if let Some(model) = &mut self.model {
            model.cancel(3, now);
        }
    }
    fn settled(&self) -> bool {
        self.task.is_none() && self.model.as_ref().is_none_or(ModelOwner::is_settled)
    }
}

pub(crate) struct Owner {
    runtime: RuntimeDirectory,
    package: Option<PackageIdentity>,
    wake: WakeSender,
    job: Option<Job>,
}
impl Owner {
    pub(crate) fn new(
        runtime: RuntimeDirectory,
        package: Option<PackageIdentity>,
        wake: WakeSender,
    ) -> Self {
        Self {
            runtime,
            package,
            wake,
            job: None,
        }
    }
    pub(crate) fn request(&mut self, request: pb::Envelope, now: Instant) -> Option<pb::Envelope> {
        if self.job.is_some() {
            return Some(error(request, "models_busy"));
        }
        if self.package.is_none() {
            return Some(error(request, "models_helper_unavailable"));
        }
        let runtime = self.runtime.clone();
        let cancel = Arc::new(AtomicBool::new(false));
        let token = cancel.clone();
        let completion = crate::completion::Completion::default();
        completion.register(self.wake.clone());
        let signal = completion.clone();
        let deadline = now + Duration::from_secs(2);
        let task = thread::Builder::new()
            .name("asura-models-config".into())
            .spawn(move || {
                let _completion = signal.guard();
                asura_storage::config::conversation_snapshot(runtime, deadline, &token)
            });
        match task {
            Ok(task) => {
                self.job = Some(Job {
                    request,
                    task: Some(task),
                    cancel,
                    completion,
                    deadline,
                    reported: false,
                    model: None,
                    configured: None,
                });
                None
            }
            Err(_) => Some(error(request, "models_unavailable")),
        }
    }
    pub(crate) fn next_deadline(&self, now: Instant) -> Option<Instant> {
        let job = self.job.as_ref()?;
        let config = if job.task.is_some() {
            if job.completion.completed() {
                job.completion.settlement_deadline(now)
            } else if !job.reported {
                Some(job.deadline)
            } else {
                None
            }
        } else {
            None
        };
        config
            .into_iter()
            .chain(
                job.model
                    .as_ref()
                    .and_then(|model| model.next_deadline(now)),
            )
            .min()
    }
    pub(crate) fn interests(&self) -> Vec<PollInterest> {
        self.job
            .as_ref()
            .and_then(|job| job.model.as_ref())
            .map(ModelOwner::interests)
            .unwrap_or_default()
    }
    pub(crate) fn settled(&self) -> bool {
        self.job.is_none()
    }
    pub(crate) fn shutdown(&mut self, now: Instant) {
        if let Some(job) = &mut self.job {
            job.cancel(now);
        }
    }
    pub(crate) fn poll(&mut self, attachments: &[[u8; 16]], now: Instant) -> Option<pb::Envelope> {
        let job = self.job.as_mut()?;
        if !attachments
            .iter()
            .any(|id| job.request.attachment_id.as_deref() == Some(id))
        {
            job.cancel(now);
        }
        let mut reply = None;
        if job.task.as_ref().is_some_and(JoinHandle::is_finished) {
            let result = job
                .task
                .take()
                .unwrap()
                .join()
                .unwrap_or(Err("models_unavailable"));
            if !job.reported {
                let result = if now >= job.deadline {
                    Err("models_timeout")
                } else {
                    result
                };
                match result {
                    Ok(config) => {
                        job.configured = Some(canonical_selection(config.model));
                        match ModelOwner::inventory(
                            self.runtime.clone(),
                            self.package.unwrap(),
                            Selection {
                                model_capabilities: None,
                                model: "system".into(),
                                asset_root: Some(config.asset_root),
                                endpoint: config.ollama_endpoint,
                            },
                            now,
                        ) {
                            Ok(mut model) => {
                                model.register(self.wake.clone());
                                job.model = Some(model);
                            }
                            Err(_) => {
                                reply = Some(error(job.request.clone(), "models_unavailable"));
                                job.reported = true;
                            }
                        }
                    }
                    Err(reason) => {
                        reply = Some(error(job.request.clone(), reason));
                        job.reported = true;
                    }
                }
            }
        } else if job.task.is_some() && !job.reported && now >= job.deadline {
            job.cancel.store(true, Ordering::Release);
            job.reported = true;
            reply = Some(error(job.request.clone(), "models_timeout"));
        }
        if let Some(model) = &mut job.model {
            for event in model.poll(now) {
                if job.reported {
                    continue;
                }
                let result = match event {
                    ModelEvent::Inventory { models, issues } => {
                        let configured = job.configured.clone().unwrap();
                        let mut rows: Vec<_> = models
                            .into_iter()
                            .map(|row| pb::ModelInventoryEntry {
                                selector: row.selector,
                                provider: row.provider,
                                status: row.status,
                                detail: row.detail,
                            })
                            .collect();
                        include_selection(&configured, &mut rows);
                        pb::ModelsReply {
                            configured_model: Some(configured),
                            models: rows,
                            issues: issues
                                .into_iter()
                                .map(|issue| pb::ModelInventoryIssue {
                                    provider: issue.provider,
                                    reason: issue.reason,
                                })
                                .collect(),
                            error: None,
                        }
                    }
                    ModelEvent::Failed(reason) => pb::ModelsReply {
                        error: Some(reason.into()),
                        ..Default::default()
                    },
                    _ => {
                        model.cancel(4, now);
                        pb::ModelsReply {
                            error: Some("models_protocol_fault".into()),
                            ..Default::default()
                        }
                    }
                };
                job.reported = true;
                let mut envelope = job.request.clone();
                envelope.body = Some(pb::envelope::Body::ModelsReply(result));
                reply = Some(envelope);
            }
        }
        if job.settled() {
            self.job = None;
        }
        reply
    }
}
fn canonical_selection(configured: String) -> String {
    configured
        .parse::<crate::model::ModelIdentifier>()
        .map(|id| id.to_string())
        .unwrap_or(configured)
}
fn include_selection(configured: &str, rows: &mut Vec<pb::ModelInventoryEntry>) {
    if !rows
        .iter()
        .any(|row| row.selector.as_deref() == Some(configured))
    {
        let provider = configured
            .parse::<crate::model::ModelIdentifier>()
            .ok()
            .filter(|id| {
                matches!(id.provider(), "system" | "ollama" | "coreai" | "mlx")
                    && id.to_string() == configured
            })
            .map(|id| id.provider().to_owned());
        rows.push(pb::ModelInventoryEntry {
            selector: Some(configured.into()),
            provider: Some(provider.clone().unwrap_or_else(|| "unknown".into())),
            status: Some(if provider.is_some() { 5 } else { 4 }),
            detail: Some(
                if provider.is_some() {
                    "not_discovered"
                } else {
                    "unsupported_selection"
                }
                .into(),
            ),
        });
    }
    rows.sort_by(|a, b| {
        a.provider
            .cmp(&b.provider)
            .then(a.selector.cmp(&b.selector))
    });
}
fn error(mut request: pb::Envelope, reason: &str) -> pb::Envelope {
    request.body = Some(pb::envelope::Body::ModelsReply(pb::ModelsReply {
        error: Some(reason.into()),
        ..Default::default()
    }));
    request
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn missing_selection_is_not_claimed_available_and_invalid_config_is_visible() {
        assert_eq!(
            canonical_selection("OLLAMA:TagCase".into()),
            "ollama:TagCase"
        );
        for (selector, provider, status) in [
            ("mlx:missing", "mlx", 5),
            ("unknown:x", "unknown", 4),
            ("bad model", "unknown", 4),
        ] {
            let mut rows = vec![];
            include_selection(selector, &mut rows);
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].provider.as_deref(), Some(provider));
            assert_eq!(rows[0].status, Some(status));
            include_selection(selector, &mut rows);
            assert_eq!(rows.len(), 1);
        }
    }
    #[test]
    fn timed_out_configuration_retains_slot_and_suppresses_late_result() {
        use std::{os::unix::fs::DirBuilderExt, sync::mpsc};
        struct Scratch(std::path::PathBuf);
        impl Drop for Scratch {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let root = Scratch(
            format!(
                "/private/tmp/asura-models-owner-{:x}",
                u128::from_ne_bytes(asura_platform::random_id())
            )
            .into(),
        );
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&root.0)
            .unwrap();
        let runtime = RuntimeDirectory::scratch(&root.0, true).unwrap();
        let (_reader, wake) = asura_platform::events::wake_pair().unwrap();
        let mut owner = Owner::new(runtime, None, wake);
        let request = pb::Envelope {
            service_epoch: Some(vec![1; 16]),
            attachment_id: Some(vec![2; 16]),
            request_counter: Some(7),
            body: Some(pb::envelope::Body::ModelsList(pb::ModelsList {})),
        };
        let (send, receive) = mpsc::sync_channel(1);
        let completion = crate::completion::Completion::default();
        let signal = completion.clone();
        let task = thread::spawn(move || {
            let _done = signal.guard();
            receive.recv().unwrap();
            Err("late_failure")
        });
        owner.job = Some(Job {
            request: request.clone(),
            task: Some(task),
            cancel: Arc::new(AtomicBool::new(false)),
            completion,
            deadline: Instant::now(),
            reported: false,
            model: None,
            configured: None,
        });
        let reply = owner.poll(&[[2; 16]], Instant::now()).unwrap();
        let Some(pb::envelope::Body::ModelsReply(reply)) = reply.body else {
            panic!()
        };
        assert_eq!(reply.error.as_deref(), Some("models_timeout"));
        assert!(!owner.settled());
        let busy = owner.request(request, Instant::now()).unwrap();
        let Some(pb::envelope::Body::ModelsReply(busy)) = busy.body else {
            panic!()
        };
        assert_eq!(busy.error.as_deref(), Some("models_busy"));
        assert!(owner.job.as_ref().unwrap().cancel.load(Ordering::Acquire));
        send.send(()).unwrap();
        let end = Instant::now() + Duration::from_secs(1);
        while !owner.settled() {
            assert!(Instant::now() < end);
            assert!(owner.poll(&[[2; 16]], Instant::now()).is_none());
            thread::yield_now();
        }
    }
}
