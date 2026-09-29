//! Read-only retained queue subscription; mutation ownership stays in queue.rs.
use super::{events::Notice, observation::Resolver};
use asura_control::pb;
use std::{
    io,
    sync::{
        Arc, Mutex, TryLockError,
        atomic::{AtomicBool, Ordering},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct Scope {
    pub project: [u8; 16],
    pub epoch: String,
}
pub(super) struct Update {
    pub scope: Scope,
    pub source: u64,
    pub received: Instant,
    pub result: Result<pb::ConversationQueueReply, &'static str>,
}
struct Job {
    scope: Scope,
    stop: Arc<AtomicBool>,
    latest: Arc<Mutex<Option<Update>>>,
    handle: JoinHandle<()>,
}
#[derive(Default)]
pub(super) struct Worker {
    job: Option<Job>,
    retry: Option<Instant>,
    source: u64,
    notice: Notice,
}
impl Worker {
    pub fn set_notice(&mut self, notice: Notice) {
        self.notice = notice;
    }
    pub fn next_deadline(&self) -> Option<Instant> {
        self.retry
    }
    pub fn retained(&self) -> bool {
        self.job.is_some()
    }
    pub fn synchronize(
        &mut self,
        resolve: Resolver,
        scope: Option<Scope>,
        now: Instant,
    ) -> io::Result<()> {
        if let Some(job) = &self.job {
            if scope.as_ref() != Some(&job.scope) {
                job.stop.store(true, Ordering::Release);
            }
            if !job.handle.is_finished() {
                return Ok(());
            }
            let pending = match job.latest.try_lock() {
                Ok(value) => value.is_some(),
                Err(TryLockError::Poisoned(e)) => e.into_inner().is_some(),
                Err(_) => true,
            };
            if pending {
                self.notice.ready();
                return Ok(());
            }
            let job = self.job.take().unwrap();
            let same = scope.as_ref() == Some(&job.scope);
            let _ = job.handle.join();
            self.notice.settled();
            self.retry = same.then_some(now + Duration::from_secs(1));
        }
        let Some(scope) = scope else {
            self.retry = None;
            return Ok(());
        };
        if self.retry.is_some_and(|at| now < at) {
            return Ok(());
        }
        self.retry = Some(now + Duration::from_secs(1));
        self.source = self
            .source
            .checked_add(1)
            .ok_or_else(|| io::Error::other("Queue subscription generation exhausted"))?;
        let source = self.source;
        let stop = Arc::new(AtomicBool::new(false));
        let token = stop.clone();
        let latest = Arc::new(Mutex::new(None));
        let mailbox = latest.clone();
        let worker_scope = scope.clone();
        let notice = self.notice.clone();
        let handle = std::thread::Builder::new()
            .name("asura-queue-watch".into())
            .spawn(move || {
                let _settlement = notice.guard();
                let publish = |result| {
                    if !token.load(Ordering::Acquire) {
                        *mailbox.lock().unwrap_or_else(|e| e.into_inner()) = Some(Update {
                            scope: worker_scope.clone(),
                            source,
                            received: Instant::now(),
                            result,
                        });
                        notice.ready();
                    }
                };
                let run = || -> Result<(), &'static str> {
                    let runtime = resolve(false).map_err(|_| "queue_service_unavailable")?;
                    if token.load(Ordering::Acquire) {
                        return Ok(());
                    }
                    let mut client = asura_client::Client::attach(
                        &runtime,
                        concat!("asura/", env!("CARGO_PKG_VERSION")),
                        Instant::now() + Duration::from_secs(2),
                    )
                    .map_err(|_| "queue_service_unavailable")?;
                    let snapshot = client.inspect().map_err(|_| "queue_service_unavailable")?;
                    if crate::conversation::hex(&snapshot.service_epoch) != worker_scope.epoch {
                        return Err("queue_service_changed");
                    }
                    let mut revision = None;
                    let mut retained = None;
                    while !token.load(Ordering::Acquire) {
                        let value = client
                            .observe_queue(worker_scope.project, revision)
                            .map_err(|_| "queue_observation_unavailable")?;
                        revision = value.revision;
                        publish(Ok(coalesce(&mut retained, value)));
                    }
                    Ok(())
                };
                if let Err(reason) = run() {
                    publish(Err(reason));
                }
            })?;
        self.job = Some(Job {
            scope,
            stop,
            latest,
            handle,
        });
        self.retry = None;
        Ok(())
    }
    pub fn poll(&self) -> Option<Update> {
        let job = self.job.as_ref()?;
        let value = match job.latest.try_lock() {
            Ok(mut v) => v.take(),
            Err(TryLockError::Poisoned(e)) => e.into_inner().take(),
            Err(_) => {
                self.notice.ready();
                None
            }
        };
        if job.stop.load(Ordering::Acquire) {
            None
        } else {
            value
        }
    }
    pub fn cancel(&self) {
        if let Some(job) = &self.job {
            job.stop.store(true, Ordering::Release);
        }
    }
}
impl Drop for Worker {
    fn drop(&mut self) {
        self.cancel();
        let Some(job) = self.job.take() else {
            return;
        };
        let end = Instant::now() + Duration::from_millis(100);
        while !job.handle.is_finished() && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(2));
        }
        if job.handle.is_finished() {
            let _ = job.handle.join();
        }
    }
}
fn coalesce(
    retained: &mut Option<pb::ConversationQueueReply>,
    value: pb::ConversationQueueReply,
) -> pb::ConversationQueueReply {
    if value.pending == Some(true) {
        if let Some(previous) = retained
            .as_ref()
            .filter(|previous| previous.revision == value.revision)
        {
            return previous.clone();
        }
    } else {
        *retained = Some(value.clone());
    }
    value
}
#[derive(Default)]
pub(super) struct Projection {
    scope: Option<Scope>,
    source: u64,
    revision: Option<u64>,
    order_revision: Option<u64>,
    seen: Option<Instant>,
    entries: Vec<pb::ConversationQueueEntry>,
}
impl Projection {
    pub fn scope(&mut self, scope: Option<Scope>) -> bool {
        if self.scope == scope {
            return false;
        }
        *self = Self {
            scope,
            ..Default::default()
        };
        true
    }
    pub fn apply(&mut self, value: Update) -> bool {
        if self.scope.as_ref() != Some(&value.scope)
            || value.source < self.source
            || self.seen.is_some_and(|seen| value.received < seen)
        {
            return false;
        }
        let replaced = value.source > self.source;
        if let Ok(reply) = &value.result
            && reply.revision < self.revision
        {
            return false;
        }
        self.source = value.source;
        self.seen = Some(value.received);
        let previous = self.entries.clone();
        if replaced {
            self.entries.clear();
            self.revision = None;
            self.order_revision = None;
        }
        match value.result {
            Err(_) => {
                self.entries.clear();
                self.revision = None;
                self.order_revision = None;
            }
            Ok(reply) => {
                self.revision = reply.revision;
                if reply.pending != Some(true) {
                    self.order_revision = reply.order_revision;
                    self.entries = reply
                        .entries
                        .into_iter()
                        .filter(|e| e.project_id.as_deref() == Some(value.scope.project.as_slice()))
                        .collect();
                }
            }
        }
        previous != self.entries
    }
    pub fn advance_revision(&mut self, revision: Option<u64>) {
        if revision > self.revision {
            self.revision = revision;
        }
    }
    pub fn has_snapshot(&self) -> bool {
        self.revision.is_some() && self.seen.is_some()
    }
    pub fn order_revision(&self) -> Option<u64> {
        self.order_revision
    }
    pub fn entries(&self) -> Vec<pb::ConversationQueueEntry> {
        self.entries.clone()
    }
    pub fn next_deadline(&self) -> Option<Instant> {
        self.seen.map(|seen| seen + Duration::from_secs(5))
    }
    pub fn expire(&mut self, now: Instant) -> bool {
        if self.next_deadline().is_some_and(|at| now >= at) {
            self.seen = None;
            return !std::mem::take(&mut self.entries).is_empty();
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn scope() -> Scope {
        Scope {
            project: [1; 16],
            epoch: "epoch".into(),
        }
    }
    fn reply(revision: u64, pending: bool) -> pb::ConversationQueueReply {
        pb::ConversationQueueReply {
            revision: Some(revision),
            pending: Some(pending),
            entries: if pending {
                vec![]
            } else {
                vec![pb::ConversationQueueEntry {
                    project_id: Some(vec![1; 16]),
                    input_id: Some(vec![2; 16]),
                    ..Default::default()
                }]
            },
            ..Default::default()
        }
    }
    fn update(source: u64, revision: u64, received: Instant) -> Update {
        Update {
            scope: scope(),
            source,
            received,
            result: Ok(reply(revision, false)),
        }
    }
    #[test]
    fn heartbeat_coalesces_without_losing_pending_snapshot() {
        let mut retained = None;
        let initial = coalesce(&mut retained, reply(7, false));
        assert_eq!(coalesce(&mut retained, reply(7, true)), initial);
    }
    #[test]
    fn projection_fences_scope_source_revision_and_expires() {
        let now = Instant::now();
        let mut model = Projection::default();
        model.scope(Some(scope()));
        assert!(model.apply(update(2, 7, now)));
        assert!(!model.apply(update(1, 8, now)));
        model.advance_revision(Some(9));
        assert!(!model.apply(update(3, 8, now)));
        let mut foreign = update(3, 10, now);
        foreign.scope.epoch = "old".into();
        assert!(!model.apply(foreign));
        assert_eq!(model.entries().len(), 1);
        assert!(model.expire(now + Duration::from_secs(5)));
        assert!(model.entries().is_empty());
        assert_eq!(model.next_deadline(), None);
    }
    #[test]
    fn finished_worker_still_owns_slot_until_synchronize_reaps() {
        let scope = scope();
        let handle = std::thread::spawn(|| {});
        let end = Instant::now() + Duration::from_secs(2);
        while !handle.is_finished() {
            assert!(Instant::now() < end);
            std::thread::yield_now();
        }
        let mut worker = Worker {
            job: Some(Job {
                scope,
                handle,
                stop: Arc::new(AtomicBool::new(false)),
                latest: Arc::new(Mutex::new(None)),
            }),
            retry: None,
            source: 0,
            notice: Notice::default(),
        };
        assert!(worker.retained());
        worker
            .synchronize(|_| panic!("no new scope"), None, Instant::now())
            .unwrap();
        assert!(!worker.retained());
    }
}
