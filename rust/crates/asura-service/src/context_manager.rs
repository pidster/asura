//! Bounded subscription leases and cached context observations. No filesystem IO.
use crate::context_observer::{ContextObserver, ContextScope, ContextSnapshot};
use asura_control::pb::{self, envelope::Body};
use std::time::{Duration, Instant};

struct Entry {
    scope: ContextScope,
    subscription: [u8; 16],
    observer: ContextObserver,
    latest: Option<ContextSnapshot>,
    viewers: Vec<[u8; 16]>,
    retiring: bool,
}
struct Pending {
    request: pb::Envelope,
    subscription: [u8; 16],
    after: Option<u64>,
    deadline: Instant,
}
#[derive(Default)]
pub(crate) struct Manager {
    entries: Vec<Entry>,
    pending: Vec<Pending>,
    closing: bool,
    wake: Option<asura_platform::events::WakeSender>,
}
fn failure(mut request: pb::Envelope, reason: &str) -> pb::Envelope {
    request.body = Some(Body::Error(pb::Error {
        code: Some(pb::ErrorCode::InternalUnavailable as i32),
        message: Some(reason.into()),
    }));
    request
}
impl Manager {
    pub fn register(&mut self, wake: asura_platform::events::WakeSender) {
        self.wake = Some(wake);
    }
    pub fn next_deadline(&self, now: Instant) -> Option<Instant> {
        self.pending
            .iter()
            .map(|pending| pending.deadline)
            .chain(
                self.entries
                    .iter()
                    .filter_map(|entry| entry.observer.next_deadline(now)),
            )
            .min()
    }
    pub fn request(
        &mut self,
        request: pb::Envelope,
        identity: Option<(String, u64, u64)>,
        now: Instant,
    ) -> Option<pb::Envelope> {
        let Some(Body::ObserveContext(query)) = request.body.as_ref() else {
            return Some(failure(request, "context_invalid"));
        };
        let Some((root, device, inode)) = identity else {
            return Some(failure(request, "project_unavailable"));
        };
        let project: [u8; 16] = query
            .project_id
            .as_deref()
            .unwrap_or_default()
            .try_into()
            .expect("validated ID");
        let path = query.working_directory.clone().expect("validated path");
        if self.closing || !std::path::Path::new(&path).starts_with(&root) {
            return Some(failure(request, "context_scope_unavailable"));
        }
        let viewer: [u8; 16] = request
            .attachment_id
            .as_deref()
            .unwrap_or_default()
            .try_into()
            .expect("validated attachment");
        let scope = ContextScope {
            project,
            root,
            path,
            device,
            inode,
        };
        for entry in &mut self.entries {
            if entry.scope != scope {
                entry.viewers.retain(|id| *id != viewer);
            }
        }
        let index = self
            .entries
            .iter()
            .position(|entry| entry.scope == scope && !entry.retiring);
        let index = match index {
            Some(index) => index,
            None => {
                if self.entries.len() >= 2 {
                    return Some(failure(request, "context_busy"));
                }
                let observer = match ContextObserver::start(scope.clone()) {
                    Ok(observer) => observer,
                    Err(_) => return Some(failure(request, "context_unavailable")),
                };
                if let Some(wake) = &self.wake {
                    observer.register(wake.clone());
                }
                self.entries.push(Entry {
                    scope,
                    subscription: asura_platform::random_id(),
                    observer,
                    latest: None,
                    viewers: Vec::new(),
                    retiring: false,
                });
                self.entries.len() - 1
            }
        };
        let entry = &mut self.entries[index];
        if !entry.viewers.contains(&viewer) {
            entry.viewers.push(viewer);
        }
        let after = if query.after_subscription.as_deref() == Some(entry.subscription.as_slice()) {
            query.after_revision
        } else {
            None
        };
        if let Some(latest) = &entry.latest
            && after.is_none_or(|revision| latest.revision > revision)
        {
            return Some(observation_reply(request, entry, false));
        }
        if self.pending.len() >= 32 {
            return Some(failure(request, "context_busy"));
        }
        self.pending.push(Pending {
            request,
            subscription: entry.subscription,
            after,
            deadline: now + Duration::from_secs(1),
        });
        None
    }
    pub fn poll(&mut self, attachments: &[[u8; 16]], now: Instant) -> Vec<pb::Envelope> {
        self.pending.retain(|pending| {
            pending
                .request
                .attachment_id
                .as_deref()
                .is_some_and(|id| attachments.iter().any(|active| active.as_slice() == id))
        });
        for entry in &mut self.entries {
            entry.viewers.retain(|id| attachments.contains(id));
            if entry.viewers.is_empty() || self.closing {
                entry.retiring = true;
                entry.observer.cancel();
            }
            if let Some(snapshot) = entry.observer.take()
                && snapshot.scope == entry.scope
                && entry.latest.as_ref().is_none_or(|old| {
                    snapshot.revision > old.revision && snapshot.observed_at >= old.observed_at
                })
            {
                entry.latest = Some(snapshot);
            }
            // A heartbeat cannot establish freshness after the actual watcher stops.
            // Keep a terminal setup failure's specific reason, but invalidate any
            // previously successful projection (or a worker that produced nothing).
            if !entry.retiring
                && entry.observer.is_finished()
                && entry.latest.as_ref().is_none_or(|snapshot| {
                    snapshot.git.state != asura_platform::git_observer::GitState::Unknown
                })
            {
                if let Some(revision) = entry
                    .latest
                    .as_ref()
                    .map_or(Some(1), |snapshot| snapshot.revision.checked_add(1))
                {
                    entry.latest = Some(ContextSnapshot {
                        scope: entry.scope.clone(),
                        revision,
                        observed_at: now,
                        git: asura_platform::git_observer::GitSnapshot::unknown(
                            "context_observer_stopped",
                        ),
                    });
                } else {
                    entry.retiring = true;
                    entry.observer.cancel();
                }
            }
        }
        let mut replies = Vec::new();
        let mut waiting = Vec::new();
        for pending in self.pending.drain(..) {
            match self
                .entries
                .iter()
                .find(|entry| entry.subscription == pending.subscription && !entry.retiring)
            {
                Some(entry) => {
                    let changed = entry.latest.as_ref().is_some_and(|snapshot| {
                        pending
                            .after
                            .is_none_or(|revision| snapshot.revision > revision)
                    });
                    if changed || now >= pending.deadline {
                        replies.push(observation_reply(pending.request, entry, !changed));
                    } else {
                        waiting.push(pending);
                    }
                }
                None => replies.push(failure(pending.request, "context_subscription_closed")),
            }
        }
        self.pending = waiting;
        self.entries
            .retain_mut(|entry| !(entry.retiring && entry.observer.finish_if_stopped()));
        replies
    }
    pub fn shutdown(&mut self) {
        self.closing = true;
        for entry in &mut self.entries {
            entry.retiring = true;
            entry.observer.cancel();
        }
    }
    pub fn settled(&self) -> bool {
        self.entries.is_empty()
    }
}
fn observation_reply(mut request: pb::Envelope, entry: &Entry, pending: bool) -> pb::Envelope {
    let mut reply = pb::ContextObservation {
        project_id: Some(entry.scope.project.to_vec()),
        working_directory: Some(entry.scope.path.clone()),
        subscription_id: Some(entry.subscription.to_vec()),
        revision: Some(
            entry
                .latest
                .as_ref()
                .map_or(0, |snapshot| snapshot.revision),
        ),
        pending: Some(pending),
        ..Default::default()
    };
    if !pending && let Some(snapshot) = &entry.latest {
        reply.git_state = Some(snapshot.git.state as u32);
        reply.branch = snapshot.git.branch.clone();
        reply.detached = Some(snapshot.git.detached);
        reply.unborn = Some(snapshot.git.unborn);
        reply.conflicts = Some(snapshot.git.conflicts);
        reply.reason = snapshot.git.reason.map(str::to_owned);
        reply.files_changed = snapshot.git.files_changed;
        reply.added = snapshot.git.added;
        reply.deleted = snapshot.git.deleted;
    }
    request.body = Some(Body::ContextObservation(reply));
    request
}

#[cfg(test)]
mod tests {
    use super::*;
    fn query(path: &str) -> pb::Envelope {
        pb::Envelope {
            service_epoch: Some(vec![1; 16]),
            attachment_id: Some(vec![2; 16]),
            request_counter: Some(1),
            body: Some(Body::ObserveContext(pb::ObserveContext {
                project_id: Some(vec![3; 16]),
                working_directory: Some(path.into()),
                ..Default::default()
            })),
        }
    }
    #[test]
    fn unauthorized_missing_and_closing_scopes_do_not_start_workers() {
        let mut manager = Manager::default();
        for (path, identity) in [
            ("/project/src", None),
            ("/project-other", Some(("/project".into(), 1, 1))),
        ] {
            let reply = manager
                .request(query(path), identity, Instant::now())
                .unwrap();
            assert!(matches!(reply.body, Some(Body::Error(_))));
            assert!(manager.entries.is_empty());
        }
        manager.shutdown();
        assert!(matches!(
            manager
                .request(
                    query("/project"),
                    Some(("/project".into(), 1, 1)),
                    Instant::now()
                )
                .unwrap()
                .body,
            Some(Body::Error(_))
        ));
        assert!(manager.settled());
    }
    fn finished_entry(state: Option<asura_platform::git_observer::GitState>) -> Entry {
        let scope = ContextScope {
            project: [3; 16],
            root: "/project".into(),
            path: "/project".into(),
            device: 1,
            inode: 1,
        };
        let latest = state.map(|state| {
            let mut git = asura_platform::git_observer::GitSnapshot::unknown("watch_unavailable");
            git.state = state;
            if state != asura_platform::git_observer::GitState::Unknown {
                git.reason = None;
            }
            ContextSnapshot {
                scope: scope.clone(),
                revision: 7,
                observed_at: Instant::now(),
                git,
            }
        });
        Entry {
            scope,
            subscription: [4; 16],
            observer: ContextObserver::finished_for_test(),
            latest,
            viewers: vec![[2; 16]],
            retiring: false,
        }
    }
    #[test]
    fn finished_observer_invalidates_success_once_and_preserves_setup_failure() {
        use asura_platform::git_observer::GitState;
        for state in [GitState::Clean, GitState::Dirty, GitState::NonRepository] {
            let mut manager = Manager {
                entries: vec![finished_entry(Some(state))],
                ..Default::default()
            };
            manager.poll(&[[2; 16]], Instant::now());
            let observed = manager.entries[0].latest.as_ref().unwrap();
            assert_eq!(observed.revision, 8);
            assert_eq!(observed.git.state, GitState::Unknown);
            assert_eq!(observed.git.reason, Some("context_observer_stopped"));
            manager.poll(&[[2; 16]], Instant::now());
            assert_eq!(manager.entries[0].latest.as_ref().unwrap().revision, 8);
            let mut request = query("/project");
            if let Some(Body::ObserveContext(value)) = request.body.as_mut() {
                value.after_subscription = Some(vec![4; 16]);
                value.after_revision = Some(7);
            }
            let reply = manager
                .request(request, Some(("/project".into(), 1, 1)), Instant::now())
                .unwrap();
            let Some(Body::ContextObservation(value)) = reply.body else {
                panic!("missing observation");
            };
            assert_eq!(value.git_state, Some(0));
            assert_eq!(value.pending, Some(false));
        }
        let mut manager = Manager {
            entries: vec![finished_entry(Some(GitState::Unknown))],
            ..Default::default()
        };
        manager.poll(&[[2; 16]], Instant::now());
        assert_eq!(
            manager.entries[0].latest.as_ref().unwrap().git.reason,
            Some("watch_unavailable")
        );
        assert_eq!(manager.entries[0].latest.as_ref().unwrap().revision, 7);
        let mut manager = Manager {
            entries: vec![finished_entry(None)],
            ..Default::default()
        };
        manager.poll(&[[2; 16]], Instant::now());
        assert_eq!(manager.entries[0].latest.as_ref().unwrap().revision, 1);
        assert_eq!(
            manager.entries[0].latest.as_ref().unwrap().git.reason,
            Some("context_observer_stopped")
        );
    }
}
