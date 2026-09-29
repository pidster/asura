//! Bounded transport worker for service-owned input queues. No scheduling policy.
use super::observation::Resolver;
use asura_control::pb;
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};
#[derive(Clone, Debug)]
pub(super) enum Request {
    Enqueue(pb::ConversationEnqueue),
    Submit(pb::ConversationQueueSubmit),
    Reorder(pb::ConversationQueueReorder),
    List {
        project: [u8; 16],
        input: Option<[u8; 16]>,
    },
    Decision(pb::ConversationQueueDecision),
}
pub(super) struct Update {
    pub request: Request,
    pub outcome: Result<pb::ConversationQueueReply, String>,
    /// True only after the worker has settled and released its slot.
    pub done: bool,
}
struct Job {
    request: Request,
    handle: JoinHandle<()>,
    receiver: Receiver<Result<pb::ConversationQueueReply, String>>,
    result: Option<Result<pb::ConversationQueueReply, String>>,
    stopped: Arc<AtomicBool>,
    deadline: Instant,
    timeout_reported: bool,
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
    pub fn is_busy(&self) -> bool {
        self.job.is_some()
    }
    pub fn submit(&mut self, resolve: Resolver, request: Request) -> Result<(), String> {
        if self.is_busy() {
            return Err("Queue request pending; draft retained".into());
        }
        let (sender, receiver) = mpsc::sync_channel(1);
        let stopped = Arc::new(AtomicBool::new(false));
        let cancelled = stopped.clone();
        let command = request.clone();
        let notice = self.notice.clone();
        let handle = std::thread::Builder::new()
            .name("asura-input-queue-client".into())
            .spawn(move || {
                let _settlement = notice.guard();
                let run = || -> Result<pb::ConversationQueueReply, String> {
                    let check = || {
                        if cancelled.load(Ordering::Acquire) {
                            Err("Queue request stopped; outcome unconfirmed".to_string())
                        } else {
                            Ok(())
                        }
                    };
                    check()?;
                    let runtime =
                        resolve(false).map_err(|e| format!("{e}; queue outcome unconfirmed"))?;
                    check()?;
                    let mut client = asura_client::Client::attach(
                        &runtime,
                        concat!("asura/", env!("CARGO_PKG_VERSION")),
                        Instant::now() + Duration::from_secs(2),
                    )
                    .map_err(|e| format!("{e}; queue outcome unconfirmed"))?;
                    check()?;
                    let result = match command {
                        Request::Enqueue(request) => client.enqueue_conversation(request),
                        Request::Submit(request) => client.queue_conversation_input(request),
                        Request::Reorder(request) => client.reorder_conversation_input(request),
                        Request::List { project, input } => {
                            client.conversation_queue(project, input)
                        }
                        Request::Decision(request) => client.decide_conversation_input(request),
                    };
                    result.map_err(|e| {
                        if matches!(e, asura_client::Error::Remote(..)) {
                            format!("{e}; draft retained")
                        } else {
                            format!("{e}; queue outcome unconfirmed; retry the same request ID")
                        }
                    })
                };
                let _ = sender.try_send(run());
            })
            .map_err(|e| e.to_string())?;
        self.job = Some(Job {
            request,
            handle,
            receiver,
            result: None,
            stopped,
            deadline: Instant::now() + Duration::from_secs(5),
            timeout_reported: false,
        });
        Ok(())
    }
    pub fn next_deadline(&self) -> Option<Instant> {
        self.job
            .as_ref()
            .filter(|job| !job.timeout_reported)
            .map(|job| job.deadline)
    }
    pub fn poll(&mut self) -> Option<Update> {
        let job = self.job.as_mut()?;
        if job.result.is_none() {
            job.result = job.receiver.try_recv().ok();
        }
        if job.handle.is_finished() {
            let mut job = self.job.take().unwrap();
            let joined = job.handle.join();
            if job.result.is_none() {
                job.result = job.receiver.try_recv().ok();
            }
            return Some(Update {
                request: job.request,
                outcome: if joined.is_ok() {
                    job.result.unwrap_or_else(|| {
                        Err("Queue worker ended without a result; outcome unconfirmed".into())
                    })
                } else {
                    Err("Queue worker failed; outcome unconfirmed".into())
                },
                done: true,
            });
        }
        if Instant::now() >= job.deadline && !job.timeout_reported {
            job.timeout_reported = true;
            return Some(Update {
                request: job.request.clone(),
                outcome: Err(
                    "Queue observation deadline; outcome unconfirmed; worker is still settling"
                        .into(),
                ),
                done: false,
            });
        }
        None
    }
    pub fn disconnect(&self) {
        if let Some(job) = &self.job {
            job.stopped.store(true, Ordering::Release);
        }
    }
}
impl Drop for Worker {
    fn drop(&mut self) {
        self.disconnect();
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn held_worker(expired: bool) -> (Worker, mpsc::SyncSender<()>) {
        let (sender, receiver) = mpsc::sync_channel(1);
        let (release, wait) = mpsc::sync_channel(1);
        let handle = std::thread::spawn(move || {
            sender
                .try_send(Ok(pb::ConversationQueueReply {
                    entries: vec![],
                    request_id: None,
                    full_text: Some(false),
                    revision: Some(1),
                    pending: Some(false),
                    ..Default::default()
                }))
                .unwrap();
            let _ = wait.recv_timeout(Duration::from_secs(3));
        });
        (
            Worker {
                notice: Default::default(),
                job: Some(Job {
                    request: Request::List {
                        project: [1; 16],
                        input: None,
                    },
                    handle,
                    receiver,
                    result: None,
                    stopped: Arc::new(AtomicBool::new(false)),
                    deadline: if expired {
                        Instant::now()
                    } else {
                        Instant::now() + Duration::from_secs(5)
                    },
                    timeout_reported: false,
                }),
            },
            release,
        )
    }
    #[test]
    fn early_result_does_not_release_worker_and_poll_never_waits() {
        let (mut worker, release) = held_worker(false);
        let start = Instant::now();
        assert!(worker.poll().is_none());
        assert!(start.elapsed() < Duration::from_millis(100));
        assert!(worker.is_busy());
        worker.disconnect();
        assert!(worker.job.as_ref().unwrap().stopped.load(Ordering::Acquire));
        release.try_send(()).unwrap();
        let end = Instant::now() + Duration::from_secs(3);
        let outcome = loop {
            if let Some(result) = worker.poll() {
                break result;
            }
            assert!(Instant::now() < end);
            std::thread::yield_now();
        };
        assert!(outcome.done && outcome.outcome.is_ok());
        assert!(!worker.is_busy());
    }
    #[test]
    fn observation_timeout_reports_once_and_retains_slot_until_settlement() {
        let (mut worker, release) = held_worker(true);
        let update = worker.poll().unwrap();
        assert!(!update.done && update.outcome.unwrap_err().contains("unconfirmed"));
        assert!(worker.poll().is_none());
        assert!(worker.is_busy());
        release.try_send(()).unwrap();
        let end = Instant::now() + Duration::from_secs(3);
        loop {
            if let Some(update) = worker.poll() {
                assert!(update.done);
                break;
            }
            assert!(Instant::now() < end);
            std::thread::yield_now();
        }
        assert!(!worker.is_busy());
    }
}
