//! Dedicated bounded context-observation transport; rendering only sees typed state.
use super::observation::Resolver;
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
    pub path: String,
    pub epoch: String,
}
pub(super) struct Update {
    pub scope: Scope,
    pub received: Instant,
    pub source: u64,
    pub result: Result<pb::ContextObservation, &'static str>,
}
struct Job {
    scope: Scope,
    stop: Arc<AtomicBool>,
    latest: Arc<Mutex<Option<Update>>>,
    handle: JoinHandle<()>,
}
#[derive(Default)]
pub(super) struct Worker {
    notice: super::events::Notice,
    job: Option<Job>,
    retry_at: Option<Instant>,
    source: u64,
}
impl Worker {
    pub fn set_notice(&mut self, notice: super::events::Notice) {
        self.notice = notice;
    }
    pub fn running(&self) -> bool {
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
            // A final publication may race the earlier poll. Retain the finished
            // worker until that result has been consumed on the next event cycle.
            let pending = match job.latest.try_lock() {
                Ok(slot) => slot.is_some(),
                Err(TryLockError::Poisoned(error)) => error.into_inner().is_some(),
                Err(TryLockError::WouldBlock) => true,
            };
            if pending {
                return Ok(());
            }
            let job = self.job.take().unwrap();
            let same = scope.as_ref() == Some(&job.scope);
            let _ = job.handle.join();
            self.notice.settled();
            self.retry_at = same.then_some(now + Duration::from_secs(1));
        }
        let Some(scope) = scope else {
            self.retry_at = None;
            return Ok(());
        };
        if self.retry_at.is_some_and(|when| now < when) {
            return Ok(());
        }
        self.retry_at = Some(now + Duration::from_secs(1));
        self.source = self
            .source
            .checked_add(1)
            .ok_or_else(|| io::Error::other("context worker generation exhausted"))?;
        let source = self.source;
        let stop = Arc::new(AtomicBool::new(false));
        let latest = Arc::new(Mutex::new(None));
        let worker_scope = scope.clone();
        let token = stop.clone();
        let mailbox = latest.clone();
        let notice = self.notice.clone();
        let handle = std::thread::Builder::new()
            .name("asura-tui-context".into())
            .spawn(move || {
                let _settlement = notice.guard();
                let publish = |result| {
                    if !token.load(Ordering::Acquire) {
                        *mailbox.lock().unwrap_or_else(|e| e.into_inner()) = Some(Update {
                            scope: worker_scope.clone(),
                            received: Instant::now(),
                            source,
                            result,
                        });
                        notice.ready();
                    }
                };
                let run = || -> Result<(), &'static str> {
                    let runtime = resolve(false).map_err(|_| "context_service_unavailable")?;
                    if token.load(Ordering::Acquire) {
                        return Ok(());
                    }
                    let mut client = asura_client::Client::attach(
                        &runtime,
                        concat!("asura/", env!("CARGO_PKG_VERSION")),
                        Instant::now() + Duration::from_secs(2),
                    )
                    .map_err(|_| "context_service_unavailable")?;
                    if token.load(Ordering::Acquire) {
                        return Ok(());
                    }
                    let snapshot = client
                        .inspect()
                        .map_err(|_| "context_service_unavailable")?;
                    if crate::conversation::hex(&snapshot.service_epoch) != worker_scope.epoch {
                        return Err("context_service_changed");
                    }
                    let mut subscription = None;
                    let mut revision = None;
                    let mut retained = None;
                    while !token.load(Ordering::Acquire) {
                        let result = client
                            .observe_context(pb::ObserveContext {
                                project_id: Some(worker_scope.project.to_vec()),
                                working_directory: Some(worker_scope.path.clone()),
                                after_subscription: subscription.clone(),
                                after_revision: revision,
                            })
                            .map_err(|_| "context_observation_unavailable")?;
                        subscription = result.subscription_id.clone();
                        revision = result.revision;
                        publish(Ok(coalesce(&mut retained, result)));
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
        self.retry_at = None;
        Ok(())
    }
    pub fn next_deadline(&self) -> Option<Instant> {
        self.retry_at
    }
    pub fn poll(&self) -> Option<Update> {
        let job = self.job.as_ref()?;
        let value = match job.latest.try_lock() {
            Ok(mut value) => value.take(),
            Err(TryLockError::Poisoned(error)) => error.into_inner().take(),
            Err(TryLockError::WouldBlock) => {
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
        let deadline = Instant::now() + Duration::from_millis(100);
        while !job.handle.is_finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(2));
        }
        if job.handle.is_finished() {
            let _ = job.handle.join();
        }
        // After terminal restoration the CLI exits its process, containing a stalled resolver.
    }
}

// Heartbeats must not overwrite the only actual snapshot before the UI polls.
fn coalesce(
    retained: &mut Option<pb::ContextObservation>,
    value: pb::ContextObservation,
) -> pb::ContextObservation {
    if value.pending == Some(true) {
        if let Some(previous) = retained.as_ref().filter(|previous| {
            previous.subscription_id == value.subscription_id
                && previous.project_id == value.project_id
                && previous.working_directory == value.working_directory
                && previous.revision == value.revision
        }) {
            return previous.clone();
        }
        *retained = None;
    } else {
        *retained = Some(value.clone());
    }
    value
}

#[derive(Default)]
pub(super) struct Model {
    scope: Option<Scope>,
    subscription: Option<Vec<u8>>,
    revision: u64,
    seen: Option<Instant>,
    refresh_until: Option<Instant>,
    value: Option<pb::ContextObservation>,
    source: u64,
}
impl Model {
    pub fn scope(&mut self, scope: Option<Scope>) -> bool {
        if self.scope == scope {
            return false;
        }
        *self = Self {
            scope,
            ..Self::default()
        };
        true
    }
    pub fn apply(&mut self, update: Update) -> bool {
        if self.scope.as_ref() != Some(&update.scope)
            || update.source < self.source
            || self.seen.is_some_and(|seen| update.received < seen)
        {
            return false;
        }
        let Ok(value) = update.result else {
            self.refresh_until = None;
            self.seen = Some(update.received);
            self.source = update.source;
            return self.value.take().is_some();
        };
        if value.project_id.as_deref() != Some(update.scope.project.as_slice())
            || value.working_directory.as_deref() != Some(update.scope.path.as_str())
        {
            return false;
        }
        if update.source == self.source
            && self.subscription == value.subscription_id
            && value.revision.unwrap_or(0) < self.revision
        {
            return false;
        }
        let replaced = update.source != self.source || self.subscription != value.subscription_id;
        let cleared = replaced && self.value.is_some();
        if replaced {
            self.refresh_until = None;
            self.subscription = value.subscription_id.clone();
            self.revision = 0;
            self.value = None;
        }
        self.source = update.source;
        self.seen = Some(update.received);
        if value.pending == Some(true) {
            return cleared;
        }
        self.revision = value.revision.unwrap_or(0);
        if value.git_state == Some(0)
            && matches!(
                value.reason.as_deref(),
                Some("git_changed" | "git_changed_during_observation")
            )
            && self
                .value
                .as_ref()
                .is_some_and(|old| matches!(old.git_state, Some(1..=3)))
        {
            self.refresh_until
                .get_or_insert(update.received + Duration::from_secs(2));
            return self.expire(update.received);
        }
        self.refresh_until = None;
        let changed = self.value.as_ref() != Some(&value);
        self.value = Some(value);
        changed || cleared
    }
    pub fn unavailable(&mut self) -> bool {
        self.refresh_until = None;
        self.seen = None;
        self.value.take().is_some()
    }
    pub fn next_deadline(&self) -> Option<Instant> {
        self.value.as_ref()?;
        self.seen
            .map(|seen| seen + Duration::from_secs(5))
            .into_iter()
            .chain(self.refresh_until)
            .min()
    }
    pub fn expire(&mut self, now: Instant) -> bool {
        if self.value.is_some()
            && (self.refresh_until.is_some_and(|until| now >= until)
                || self.seen.is_none_or(|seen| {
                    now.saturating_duration_since(seen) >= Duration::from_secs(5)
                }))
        {
            self.value = None;
            self.refresh_until = None;
            return true;
        }
        false
    }
    pub fn report(&self) -> Option<Report> {
        let Some(value) = &self.value else {
            return Some(Report::unknown());
        };
        match value.git_state {
            Some(1) => None,
            Some(2 | 3) => Some(Report {
                branch: if value.detached == Some(true) {
                    "detached".into()
                } else {
                    value.branch.clone().unwrap_or_else(|| "?".into())
                },
                files: value.files_changed,
                added: value.added,
                deleted: value.deleted,
            }),
            _ => Some(Report::unknown()),
        }
    }
    #[cfg(test)]
    pub fn label(&self) -> Option<String> {
        self.report().map(|report| report.label())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct Report {
    pub branch: String,
    pub files: Option<u64>,
    pub added: Option<u64>,
    pub deleted: Option<u64>,
}
impl Report {
    pub fn unknown() -> Self {
        Self {
            branch: "?".into(),
            files: None,
            added: None,
            deleted: None,
        }
    }
    pub fn heading(&self) -> String {
        format!("{} {}", self.branch, number(self.files))
    }
    pub fn addition(&self) -> String {
        format!("+{}", number(self.added))
    }
    pub fn deletion(&self) -> String {
        format!("-{}", number(self.deleted))
    }
    pub fn label(&self) -> String {
        format!("{} {}{}", self.heading(), self.addition(), self.deletion())
    }
}
fn number(value: Option<u64>) -> String {
    value.map_or_else(|| "?".into(), |n| n.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn scope() -> Scope {
        Scope {
            project: [1; 16],
            path: "/project".into(),
            epoch: "aa".into(),
        }
    }
    fn observation(state: u32, revision: u64) -> pb::ContextObservation {
        pb::ContextObservation {
            project_id: Some(vec![1; 16]),
            working_directory: Some("/project".into()),
            subscription_id: Some(vec![2; 16]),
            revision: Some(revision),
            pending: Some(false),
            git_state: Some(state),
            branch: Some("main".into()),
            detached: Some(false),
            unborn: Some(false),
            conflicts: Some(false),
            reason: None,
            files_changed: Some(2),
            added: Some(12),
            deleted: Some(3),
        }
    }
    fn update(value: pb::ContextObservation, source: u64, received: Instant) -> Update {
        Update {
            scope: scope(),
            source,
            received,
            result: Ok(value),
        }
    }
    #[test]
    fn refresh_keeps_display_until_replacement_or_fixed_expiry() {
        let now = Instant::now();
        let mut model = Model::default();
        model.scope(Some(scope()));
        model.apply(update(observation(2, 1), 1, now));
        let mut refresh = observation(0, 2);
        refresh.reason = Some("git_changed".into());
        assert!(!model.apply(update(refresh.clone(), 1, now)));
        assert_eq!(model.label().as_deref(), Some("main 2 +12-3"));
        refresh.revision = Some(3);
        model.apply(update(refresh.clone(), 1, now + Duration::from_secs(1)));
        assert_eq!(model.next_deadline(), Some(now + Duration::from_secs(2)));
        assert!(model.apply(update(
            observation(3, 4),
            1,
            now + Duration::from_millis(1500)
        )));
        assert!(!model.expire(now + Duration::from_secs(2)));
        refresh.revision = Some(5);
        model.apply(update(refresh.clone(), 1, now + Duration::from_secs(3)));
        assert!(model.expire(now + Duration::from_secs(5)));
        assert_eq!(model.label().as_deref(), Some("? ? +?-?"));
        model.apply(update(observation(2, 6), 1, now + Duration::from_secs(6)));
        refresh.revision = Some(7);
        refresh.reason = Some("project_identity_changed".into());
        model.apply(update(refresh.clone(), 1, now + Duration::from_secs(6)));
        assert_eq!(model.label().as_deref(), Some("? ? +?-?"));
        model.apply(update(observation(2, 8), 1, now + Duration::from_secs(7)));
        refresh.reason = Some("git_changed".into());
        refresh.subscription_id = Some(vec![9; 16]);
        model.apply(update(refresh, 1, now + Duration::from_secs(7)));
        assert_eq!(model.label().as_deref(), Some("? ? +?-?"));
    }
    #[test]
    fn labels_distinguish_unknown_nonrepo_clean_dirty_detached_unborn_conflicts() {
        let mut model = Model::default();
        model.scope(Some(scope()));
        assert_eq!(model.label().as_deref(), Some("? ? +?-?"));
        let now = Instant::now();
        model.apply(update(observation(1, 1), 1, now));
        assert_eq!(model.label(), None);
        model.apply(update(observation(2, 2), 1, now));
        assert_eq!(model.label().as_deref(), Some("main 2 +12-3"));
        let mut dirty = observation(3, 3);
        dirty.detached = Some(true);
        dirty.unborn = Some(true);
        dirty.conflicts = Some(true);
        model.apply(update(dirty, 1, now));
        assert_eq!(model.label().as_deref(), Some("detached 2 +12-3"));
        model.apply(update(observation(0, 4), 1, now));
        assert_eq!(model.label().as_deref(), Some("? ? +?-?"));
    }
    #[test]
    fn scope_revision_timestamp_and_worker_generation_fence_stale_updates() {
        let now = Instant::now();
        let mut model = Model::default();
        model.scope(Some(scope()));
        assert!(model.apply(update(observation(2, 2), 2, now)));
        assert!(!model.apply(update(observation(3, 1), 2, now)));
        assert!(!model.apply(update(observation(3, 3), 1, now + Duration::from_secs(1))));
        assert!(!model.apply(update(observation(3, 3), 2, now - Duration::from_secs(1))));
        let mut foreign = update(observation(3, 3), 2, now);
        foreign.scope.project = [3; 16];
        assert!(!model.apply(foreign));
        assert_eq!(model.label().as_deref(), Some("main 2 +12-3"));
        let mut wrong = observation(3, 3);
        wrong.project_id = Some(vec![9; 16]);
        assert!(!model.apply(update(wrong, 2, now + Duration::from_secs(4))));
        assert!(model.expire(now + Duration::from_secs(5)));
        assert_eq!(model.label().as_deref(), Some("? ? +?-?"));
    }
    #[test]
    fn pending_new_subscription_clears_display_and_heartbeat_keeps_live_value() {
        let now = Instant::now();
        let mut model = Model::default();
        model.scope(Some(scope()));
        model.apply(update(observation(2, 1), 1, now));
        let mut heartbeat = observation(0, 1);
        heartbeat.pending = Some(true);
        assert!(!model.apply(update(heartbeat.clone(), 1, now + Duration::from_secs(4))));
        assert!(!model.expire(now + Duration::from_secs(5)));
        heartbeat.subscription_id = Some(vec![3; 16]);
        heartbeat.revision = Some(0);
        assert!(model.apply(update(heartbeat, 2, now + Duration::from_secs(5))));
        assert_eq!(model.label().as_deref(), Some("? ? +?-?"));
    }
    #[test]
    fn actual_snapshot_survives_heartbeat_before_ui_poll() {
        let actual = observation(2, 1);
        let mut retained = None;
        let mut mailbox = Some(coalesce(&mut retained, actual.clone()));
        assert_eq!(mailbox.as_ref(), Some(&actual));
        let mut heartbeat = observation(0, 1);
        heartbeat.pending = Some(true);
        mailbox = Some(coalesce(&mut retained, heartbeat.clone()));
        assert_eq!(mailbox.take(), Some(actual));
        heartbeat.subscription_id = Some(vec![9; 16]);
        assert_eq!(coalesce(&mut retained, heartbeat.clone()), heartbeat);
        assert!(retained.is_none());
    }
    #[test]
    fn contended_mailbox_and_cancel_do_not_block_ui() {
        let latest = Arc::new(Mutex::new(Some(update(
            observation(2, 1),
            1,
            Instant::now(),
        ))));
        let (send, wait) = std::sync::mpsc::sync_channel(1);
        let handle = std::thread::spawn(move || {
            let _ = wait.recv_timeout(Duration::from_secs(3));
        });
        let worker = Worker {
            notice: Default::default(),
            job: Some(Job {
                scope: scope(),
                stop: Arc::new(AtomicBool::new(false)),
                latest: latest.clone(),
                handle,
            }),
            retry_at: None,
            source: 1,
        };
        let guard = latest.lock().unwrap();
        let start = Instant::now();
        assert!(worker.poll().is_none());
        worker.cancel();
        assert!(start.elapsed() < Duration::from_millis(100));
        drop(guard);
        assert!(worker.poll().is_none(), "retired result must not reach UI");
        send.try_send(()).unwrap();
    }
}
