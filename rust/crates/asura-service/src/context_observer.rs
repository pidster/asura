//! Service-owned signal -> observation worker. No client-side Git policy or IO.
use asura_platform::git_observer::{
    GitSnapshot, GitWatch, GitWatchWake, collect_git, git_executable, validate_scope,
};
use std::{
    io,
    sync::{
        Arc, Mutex, OnceLock, TryLockError,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ContextScope {
    pub project: [u8; 16],
    pub root: String,
    pub path: String,
    pub device: u64,
    pub inode: u64,
}
#[derive(Clone, Debug)]
pub struct ContextSnapshot {
    pub scope: ContextScope,
    pub revision: u64,
    pub observed_at: Instant,
    pub git: GitSnapshot,
}

/// Root's context manager caps active and retiring workers together at two.
pub struct ContextObserver {
    cancelled: Arc<AtomicBool>,
    latest: Arc<Mutex<Option<ContextSnapshot>>>,
    worker: Option<JoinHandle<()>>,
    completion: crate::completion::Completion,
    watch_wake: Arc<OnceLock<GitWatchWake>>,
}
impl ContextObserver {
    #[cfg(test)]
    pub(crate) fn finished_for_test() -> Self {
        Self {
            cancelled: Arc::new(AtomicBool::new(false)),
            latest: Arc::new(Mutex::new(None)),
            worker: None,
            completion: Default::default(),
            watch_wake: Arc::new(OnceLock::new()),
        }
    }

    pub fn start(scope: ContextScope) -> io::Result<Self> {
        let cancelled = Arc::new(AtomicBool::new(false));
        let latest = Arc::new(Mutex::new(None));
        let watch_wake = Arc::new(OnceLock::new());
        let watcher_signal = watch_wake.clone();
        let token = cancelled.clone();
        let mailbox = latest.clone();
        let completion = crate::completion::Completion::default();
        let signal = completion.clone();
        let worker = thread::Builder::new()
            .name("asura-context-observer".into())
            .spawn(move || {
                let _finished = signal.guard();
                observe(scope, token, mailbox, signal.clone(), watcher_signal)
            })?;
        Ok(Self {
            cancelled,
            latest,
            worker: Some(worker),
            completion,
            watch_wake,
        })
    }
    pub fn register(&self, wake: asura_platform::events::WakeSender) {
        self.completion.register(wake);
    }
    pub fn next_deadline(&self, now: Instant) -> Option<Instant> {
        self.worker.as_ref().and_then(|worker| {
            if worker.is_finished() {
                Some(now)
            } else {
                self.completion.settlement_deadline(now)
            }
        })
    }
    pub fn take(&self) -> Option<ContextSnapshot> {
        match self.latest.try_lock() {
            Ok(mut slot) => slot.take(),
            Err(TryLockError::Poisoned(error)) => error.into_inner().take(),
            Err(TryLockError::WouldBlock) => None,
        }
    }
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        if let Some(wake) = self.watch_wake.get() {
            wake.signal();
        }
    }
    pub fn is_finished(&self) -> bool {
        self.worker.as_ref().is_none_or(JoinHandle::is_finished)
    }
    pub fn finish_if_stopped(&mut self) -> bool {
        if !self.is_finished() {
            return false;
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
        true
    }
}
impl Drop for ContextObserver {
    fn drop(&mut self) {
        self.cancel();
        // The context manager must retain unfinished workers through shutdown settlement.
        // Drop cannot block the reactor; process exit is the final containment boundary.
        self.finish_if_stopped();
    }
}

fn observe(
    scope: ContextScope,
    cancelled: Arc<AtomicBool>,
    latest: Arc<Mutex<Option<ContextSnapshot>>>,
    signal: crate::completion::Completion,
    wake: Arc<OnceLock<GitWatchWake>>,
) {
    let mut revision = 0u64;
    let mut publish = |git: GitSnapshot| {
        revision += 1;
        let value = ContextSnapshot {
            scope: scope.clone(),
            revision,
            observed_at: Instant::now(),
            git,
        };
        *latest.lock().unwrap_or_else(|error| error.into_inner()) = Some(value);
        signal.notify();
    };
    if let Err(reason) = validate_scope(&scope.root, &scope.path, scope.device, scope.inode) {
        publish(GitSnapshot::unknown(reason));
        return;
    }
    let watch = match GitWatch::new(&scope.root) {
        Ok(watch) => watch,
        Err(reason) => {
            publish(GitSnapshot::unknown(reason));
            return;
        }
    };
    let _ = wake.set(watch.wake_handle());
    if cancelled.load(Ordering::Acquire) {
        return;
    }
    let git = match git_executable(&cancelled) {
        Ok(path) => path,
        Err(reason) => {
            publish(GitSnapshot::unknown(reason));
            return;
        }
    };
    let mut dirty = Some((
        Instant::now() - Duration::from_secs(2),
        Instant::now() - Duration::from_secs(2),
    ));
    while !cancelled.load(Ordering::Acquire) {
        let now = Instant::now();
        if dirty.is_some_and(|(first, last)| {
            now.duration_since(last) >= Duration::from_millis(250)
                || now.duration_since(first) >= Duration::from_secs(2)
        }) {
            dirty = None;
            let snapshot = collect_git(
                &scope.root,
                &scope.path,
                scope.device,
                scope.inode,
                &git,
                &cancelled,
            );
            if cancelled.load(Ordering::Acquire) {
                break;
            }
            if snapshot.reason == Some("project_identity_changed") {
                publish(snapshot);
                break;
            }
            if watch.poll(0.0) {
                publish(GitSnapshot::unknown("git_changed_during_observation"));
                let now = Instant::now();
                dirty = Some((now, now));
            } else {
                publish(snapshot);
            }
        }
        let wait = dirty
            .map(|(first, last)| {
                (last + Duration::from_millis(250))
                    .min(first + Duration::from_secs(2))
                    .saturating_duration_since(Instant::now())
                    .as_secs_f64()
            })
            .unwrap_or(f64::MAX);
        if cancelled.load(Ordering::Acquire) {
            break;
        }
        if watch.poll(wait) {
            if dirty.is_none() {
                publish(GitSnapshot::unknown("git_changed"));
            }
            let now = Instant::now();
            dirty = Some((dirty.map_or(now, |(first, _)| first), now));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn mailbox_contention_never_blocks_consumer_or_cancel() {
        let mut observer = ContextObserver {
            cancelled: Arc::new(AtomicBool::new(false)),
            latest: Arc::new(Mutex::new(None)),
            worker: None,
            completion: Default::default(),
            watch_wake: Arc::new(OnceLock::new()),
        };
        let mailbox = observer.latest.clone();
        let guard = mailbox.lock().unwrap();
        assert!(observer.take().is_none());
        observer.cancel();
        assert!(observer.cancelled.load(Ordering::Acquire));
        drop(guard);
        assert!(observer.finish_if_stopped());
    }
    #[test]
    fn finished_retained_worker_keeps_settlement_deadline() {
        let mut observer = ContextObserver::finished_for_test();
        let worker = thread::spawn(|| {});
        while !worker.is_finished() {
            thread::yield_now();
        }
        observer.worker = Some(worker);
        let now = Instant::now();
        assert_eq!(observer.next_deadline(now), Some(now));
        observer.finish_if_stopped();
        assert_eq!(observer.next_deadline(now), None);
    }
}
