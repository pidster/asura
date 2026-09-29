//! Cached service observations with retained cursor requests; heartbeat performs no IO.
use asura_control::pb::{self, envelope::Body};
use asura_platform::{RuntimeDirectory, events::WakeSender};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};
struct ReadValue {
    model: Option<String>,
    size: Result<u64, asura_storage::stored_memory::SizeError>,
    sampled: Instant,
}
struct Read {
    handle: JoinHandle<ReadValue>,
    completion: crate::completion::Completion,
    cancel: Arc<AtomicBool>,
    deadline: Instant,
    expired: bool,
}
pub(crate) struct Observer {
    runtime: RuntimeDirectory,
    wake: WakeSender,
    status: pb::InspectReply,
    started: Instant,
    next_sample: Instant,
    size: Option<(u64, u64)>,
    size_reason: u32,
    revision: u64,
    model: Option<String>,
    read: Option<Read>,
    refresh: bool,
    closing: bool,
    waiting: Vec<(pb::Envelope, Instant)>,
}
impl Observer {
    pub fn new(runtime: RuntimeDirectory, wake: WakeSender, status: pb::InspectReply) -> Self {
        Self {
            runtime,
            wake,
            status,
            started: Instant::now(),
            next_sample: Instant::now(),
            size: None,
            size_reason: 2,
            revision: 1,
            model: None,
            read: None,
            refresh: true,
            closing: false,
            waiting: Vec::new(),
        }
    }
    pub fn refresh(&mut self) {
        self.refresh = true;
        let _ = self.wake.notify();
    }
    pub fn request(&mut self, request: pb::Envelope, now: Instant) -> Option<pb::Envelope> {
        let Some(Body::ObserveService(query)) = request.body.as_ref() else {
            unreachable!()
        };
        if query.after_revision != Some(self.revision) {
            return Some(self.reply(request, false));
        }
        if self.waiting.len() >= 32 {
            return Some(unavailable(request));
        }
        self.waiting.push((request, now + Duration::from_secs(1)));
        None
    }
    fn reply(&self, mut request: pb::Envelope, pending: bool) -> pb::Envelope {
        request.body = Some(Body::ServiceObservation(pb::ServiceObservation {
            revision: Some(self.revision),
            pending: Some(pending),
            status: Some(self.status),
            configured_model: self.model.clone(),
            uptime_ms: Some(millis(
                Instant::now().saturating_duration_since(self.started),
            )),
            stored_memory: Some(pb::StoredMemoryStatus {
                available: Some(
                    self.status.installation == Some(6) && self.status.reason == Some(16),
                ),
                size_bytes: self.size.map(|s| s.0),
                sampled_uptime_ms: self.size.map(|s| s.1),
                stale: Some(self.size_reason != 1),
                size_reason: Some(self.size_reason),
            }),
        }));
        request
    }
    pub fn poll(
        &mut self,
        status: pb::InspectReply,
        attachments: &[[u8; 16]],
        now: Instant,
    ) -> Vec<pb::Envelope> {
        if self.status.installation != status.installation || self.status.reason != status.reason {
            self.refresh = true;
        }
        let previous_model = self.model.clone();
        let previous_size = (self.size, self.size_reason);
        if let Some(read) = self.read.as_mut() {
            if now >= read.deadline && !read.expired {
                read.expired = true;
                read.cancel.store(true, Ordering::Release);
                self.size_reason = 3;
            }
            if read.handle.is_finished() {
                let read = self.read.take().unwrap();
                let result = read.handle.join();
                if !read.expired {
                    match result {
                        Ok(value) => {
                            self.model = value.model;
                            match value.size {
                                Ok(bytes) => {
                                    self.size = Some((
                                        bytes,
                                        millis(
                                            value.sampled.saturating_duration_since(self.started),
                                        ),
                                    ));
                                    self.size_reason = 1;
                                }
                                Err(error) => {
                                    self.size_reason = size_reason(error);
                                }
                            }
                        }
                        Err(_) => self.size_reason = 5,
                    }
                }
            }
        }
        if (self.refresh || now >= self.next_sample) && self.read.is_none() && !self.closing {
            self.refresh = false;
            self.next_sample = now + Duration::from_secs(30);
            let runtime = self.runtime.clone();
            let deadline = now + Duration::from_secs(2);
            let completion = crate::completion::Completion::default();
            completion.register(self.wake.clone());
            let signal = completion.clone();
            let cancel = Arc::new(AtomicBool::new(false));
            let token = cancel.clone();
            if let Ok(handle) = std::thread::Builder::new()
                .name("asura-status-config".into())
                .spawn(move || {
                    let _done = signal.guard();
                    let model = asura_storage::config::conversation_snapshot(
                        runtime.clone(),
                        deadline,
                        &token,
                    )
                    .ok()
                    .map(|snapshot| snapshot.model);
                    let size =
                        asura_storage::stored_memory::stored_bytes(runtime, deadline, &token);
                    ReadValue {
                        model,
                        size,
                        sampled: Instant::now(),
                    }
                })
            {
                self.read = Some(Read {
                    handle,
                    completion,
                    cancel,
                    deadline,
                    expired: false,
                });
            } else {
                self.size_reason = 5;
            }
        }
        if self.status != status
            || previous_model != self.model
            || previous_size != (self.size, self.size_reason)
        {
            self.status = status;
            self.revision = self
                .revision
                .checked_add(1)
                .expect("service revision exhausted");
        }
        let mut replies = Vec::new();
        for (request, deadline) in std::mem::take(&mut self.waiting) {
            if !attachments
                .iter()
                .any(|id| request.attachment_id.as_deref() == Some(id.as_slice()))
            {
                continue;
            }
            let Some(Body::ObserveService(query)) = request.body.as_ref() else {
                unreachable!()
            };
            if query.after_revision != Some(self.revision) {
                replies.push(self.reply(request, false));
            } else if now >= deadline {
                replies.push(self.reply(request, true));
            } else {
                self.waiting.push((request, deadline));
            }
        }
        replies
    }
    pub fn next_deadline(&self, now: Instant) -> Option<Instant> {
        self.waiting
            .iter()
            .map(|(_, deadline)| *deadline)
            .chain((self.read.is_none() && !self.closing).then_some(self.next_sample))
            .chain(self.read.as_ref().and_then(|read| {
                if read.completion.completed() {
                    read.completion.settlement_deadline(now)
                } else if !read.expired {
                    Some(read.deadline)
                } else {
                    None
                }
            }))
            .min()
    }
    pub fn shutdown(&mut self) {
        self.closing = true;
        self.waiting.clear();
        if let Some(read) = &self.read {
            read.cancel.store(true, Ordering::Release);
        }
    }
    pub fn settled(&self) -> bool {
        self.read.is_none()
    }
}
fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}
fn size_reason(error: asura_storage::stored_memory::SizeError) -> u32 {
    use asura_storage::stored_memory::SizeError;
    match error {
        SizeError::Timeout => 3,
        SizeError::Unsafe => 4,
        SizeError::Unavailable => 5,
        SizeError::Limit => 6,
        SizeError::Cancelled => 7,
    }
}

fn unavailable(mut request: pb::Envelope) -> pb::Envelope {
    request.body = Some(Body::Error(pb::Error {
        code: Some(pb::ErrorCode::InternalUnavailable as i32),
        message: Some("observation_busy".into()),
    }));
    request
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unchanged_heartbeat_changed_projection_disconnect_and_capacity() {
        use std::os::unix::fs::DirBuilderExt;
        let path = std::path::PathBuf::from(format!(
            "/private/tmp/asura-status-subscription-{:x}",
            u128::from_ne_bytes(asura_platform::random_id())
        ));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .unwrap();
        let runtime = RuntimeDirectory::scratch(&path, true).unwrap();
        let (_reader, sender) = asura_platform::events::wake_pair().unwrap();
        let status = pb::InspectReply {
            lifecycle: Some(1),
            installation: Some(1),
            reason: Some(1),
            unavailable_reason: None,
        };
        let mut owner = Observer::new(runtime, sender, status);
        owner.closing = true; // This unit exercises cache delivery without starting source IO.
        let request = |after| pb::Envelope {
            service_epoch: Some(vec![1; 16]),
            attachment_id: Some(vec![2; 16]),
            request_counter: Some(1),
            body: Some(Body::ObserveService(pb::ObserveService {
                after_revision: after,
            })),
        };
        let now = Instant::now();
        assert!(matches!(
            owner.request(request(None), now).unwrap().body,
            Some(Body::ServiceObservation(pb::ServiceObservation {
                revision: Some(1),
                pending: Some(false),
                ..
            }))
        ));
        assert!(owner.request(request(Some(1)), now).is_none());
        assert!(owner.poll(status, &[[2; 16]], now).is_empty());
        let heartbeat = owner.poll(status, &[[2; 16]], now + Duration::from_secs(1));
        assert!(matches!(
            heartbeat[0].body,
            Some(Body::ServiceObservation(pb::ServiceObservation {
                revision: Some(1),
                pending: Some(true),
                ..
            }))
        ));
        owner.request(request(Some(1)), now);
        let mut changed = status;
        changed.lifecycle = Some(2);
        let reply = owner.poll(changed, &[[2; 16]], now);
        assert!(matches!(
            reply[0].body,
            Some(Body::ServiceObservation(pb::ServiceObservation {
                revision: Some(2),
                pending: Some(false),
                ..
            }))
        ));
        for _ in 0..32 {
            assert!(owner.request(request(Some(2)), now).is_none());
        }
        assert!(matches!(
            owner.request(request(Some(2)), now).unwrap().body,
            Some(Body::Error(_))
        ));
        assert!(owner.poll(changed, &[], now).is_empty());
        assert!(owner.waiting.is_empty());
        assert!(owner.read.is_none());
        drop(owner);
        std::fs::remove_dir_all(path).unwrap();
    }
    #[test]
    fn stalled_size_refresh_retains_last_sample_and_one_worker_until_settlement() {
        use std::os::unix::fs::DirBuilderExt;
        let path = std::path::PathBuf::from(format!(
            "/private/tmp/asura-size-observer-{:x}",
            u128::from_ne_bytes(asura_platform::random_id())
        ));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .unwrap();
        let runtime = RuntimeDirectory::scratch(&path, true).unwrap();
        let (_, wake) = asura_platform::events::wake_pair().unwrap();
        let status = pb::InspectReply {
            lifecycle: Some(2),
            installation: Some(6),
            reason: Some(16),
            unavailable_reason: None,
        };
        let mut owner = Observer::new(runtime, wake, status);
        owner.refresh = false;
        owner.size = Some((42, 0));
        owner.size_reason = 1;
        let now = Instant::now();
        owner.next_sample = now + Duration::from_secs(30);
        let (release, blocked) = std::sync::mpsc::sync_channel(1);
        let completion = crate::completion::Completion::default();
        let done = completion.clone();
        let cancel = Arc::new(AtomicBool::new(false));
        let token = cancel.clone();
        let handle = std::thread::spawn(move || {
            let _guard = done.guard();
            blocked.recv_timeout(Duration::from_secs(2)).unwrap();
            assert!(token.load(Ordering::Acquire));
            ReadValue {
                model: Some("late".into()),
                size: Ok(999),
                sampled: Instant::now(),
            }
        });
        owner.read = Some(Read {
            handle,
            completion,
            cancel,
            deadline: now,
            expired: false,
        });
        let before = Instant::now();
        owner.poll(status, &[], now);
        assert!(before.elapsed() < Duration::from_millis(100));
        assert!(!owner.settled());
        assert_eq!(owner.size, Some((42, 0)));
        assert_eq!(owner.size_reason, 3);
        owner.refresh();
        owner.poll(status, &[], now + Duration::from_secs(31));
        assert!(!owner.settled());
        assert!(owner.refresh);
        owner.shutdown();
        release.send(()).unwrap();
        let end = Instant::now() + Duration::from_secs(2);
        while !owner.settled() {
            owner.poll(status, &[], Instant::now());
            assert!(Instant::now() < end);
            std::thread::yield_now();
        }
        assert_eq!(owner.size, Some((42, 0)));
        assert_eq!(owner.size_reason, 3);
        assert!(owner.model.is_none());
        assert!(owner.next_deadline(Instant::now()).is_none());
        let request = pb::Envelope {
            service_epoch: Some(vec![1; 16]),
            attachment_id: Some(vec![2; 16]),
            request_counter: Some(1),
            body: Some(Body::ObserveService(pb::ObserveService {
                after_revision: None,
            })),
        };
        let response = owner.request(request.clone(), Instant::now()).unwrap();
        assert!(
            asura_control::validate_semantics(&response, asura_control::Direction::ServerToClient)
                .is_ok()
        );
        let Some(Body::ServiceObservation(first)) = response.body else {
            panic!()
        };
        let memory = first.stored_memory.unwrap();
        assert_eq!(memory.available, Some(true));
        assert_eq!(memory.stale, Some(true));
        assert_eq!(memory.size_bytes, Some(42));
        let Some(Body::ServiceObservation(second)) =
            owner.request(request, Instant::now()).unwrap().body
        else {
            panic!()
        };
        assert!(second.uptime_ms >= first.uptime_ms);
        drop(owner);
        std::fs::remove_dir_all(path).unwrap();
    }
}
