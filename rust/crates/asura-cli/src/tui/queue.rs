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
};
#[derive(Clone, Debug)]
pub(super) enum Request {
    Submit(pb::ConversationQueueSubmit),
    /// A person `!` or `!!` command (person-command-mode.md, field 68).
    Command(pb::ConversationQueueCommandSubmit),
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
    /// The person stopped waiting; reported once while the worker settles.
    stop_reported: bool,
}
#[derive(Default)]
pub(super) struct Worker {
    notice: super::events::Notice,
    job: Option<Job>,
}
/// Shown when the service does not list capability 8 (OD3): an enablement
/// gate, a definite rejection that keeps the text.
pub(super) const COMMANDS_NOT_ENABLED: &str = "Commands are not enabled in this service";
/// A person command's typed refusal as the queue reply that carries it: the
/// request ID, the refusal and its status, and nothing queued. A refusal the
/// client made before sending has the same form.
pub(super) fn refusal_reply(
    request: &pb::ConversationQueueCommandSubmit,
    refusal: pb::QueueRefusal,
    status: Option<u32>,
) -> pb::ConversationQueueReply {
    pb::ConversationQueueReply {
        request_id: request.request_id.clone(),
        full_text: Some(false),
        refusal: Some(refusal as i32),
        refusal_status: status,
        ..Default::default()
    }
}
fn command_reply(
    request: &pb::ConversationQueueCommandSubmit,
    outcome: asura_client::CommandSubmit,
) -> pb::ConversationQueueReply {
    match outcome {
        asura_client::CommandSubmit::Accepted(reply) => reply,
        asura_client::CommandSubmit::Refused { refusal, status } => {
            refusal_reply(request, refusal, status)
        }
    }
}
/// A definite service rejection keeps the text; a journal hold reason shows
/// its notice. Any other failure leaves the outcome unconfirmed.
fn failure_text(error: asura_client::Error) -> String {
    match &error {
        asura_client::Error::Remote(_, reason) => match crate::conversation::journal_notice(reason)
        {
            Some(notice) => format!("{notice} Draft retained."),
            None => format!("{error}; draft retained"),
        },
        _ => format!("{error}; queue outcome unconfirmed; retry the same request ID"),
    }
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
                        &|| cancelled.load(Ordering::Acquire),
                    )
                    .map_err(|e| format!("{e}; queue outcome unconfirmed"))?;
                    check()?;
                    // Reads, too, end only on the reply, /cancel or a lost connection.
                    let read_stop = cancelled.clone();
                    client.set_cancel(Arc::new(move || read_stop.load(Ordering::Acquire)));
                    // Queue mutations wait for the journal with no client
                    // budget (J1-0); /cancel stops the wait.
                    let stop = || cancelled.load(Ordering::Acquire);
                    let result = match command {
                        Request::Submit(request) => client.queue_conversation_input(request, &stop),
                        // The enablement gate is checked on this attachment;
                        // `stop` is the person's cancel, not a time bound.
                        Request::Command(_) if !client.supports(pb::Capability::PersonCommand) => {
                            return Err(format!("{COMMANDS_NOT_ENABLED}; draft retained"));
                        }
                        Request::Command(request) => {
                            let expected = request.clone();
                            client
                                .queue_person_command(request, &stop)
                                .map(|outcome| command_reply(&expected, outcome))
                        }
                        Request::Reorder(request) => {
                            client.reorder_conversation_input(request, &stop)
                        }
                        Request::List { project, input } => {
                            client.conversation_queue(project, input)
                        }
                        Request::Decision(request) => {
                            client.decide_conversation_input(request, &stop)
                        }
                    };
                    result.map_err(failure_text)
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
            stop_reported: false,
        });
        Ok(())
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
        // J1-0: the wait has no timer. Only cancel stops it; the slot stays
        // busy until the worker settles, so a second request gets busy.
        if job.stopped.load(Ordering::Acquire) && !job.stop_reported {
            job.stop_reported = true;
            return Some(Update {
                request: job.request.clone(),
                outcome: Err(
                    "Queue wait stopped; outcome unconfirmed; worker is still settling".into(),
                ),
                done: false,
            });
        }
        None
    }
    /// Stops waiting for the in-flight request. Its reply is not applied here;
    /// the service-owned queue projection shows the request if it committed.
    pub fn cancel(&mut self) {
        self.disconnect();
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
    /// SJ-15 (client): journal hold reasons show their notices and keep the
    /// text as a definite rejection, never as an unconfirmed outcome.
    #[test]
    fn journal_hold_reasons_are_definite_rejections_with_notices() {
        let quota = failure_text(asura_client::Error::Remote(3, "journal_quota".into()));
        assert_eq!(
            quota,
            "Journal space is full. Raise journal.quotaBytes or free disk space. Draft retained."
        );
        let disk = failure_text(asura_client::Error::Remote(3, "journal_disk_space".into()));
        assert_eq!(disk, "Disk space is low for the journal. Draft retained.");
        for text in [&quota, &disk] {
            assert!(!text.contains("unconfirmed"));
        }
        assert_eq!(
            failure_text(asura_client::Error::Remote(3, "conversation_busy".into())),
            "conversation_busy; draft retained"
        );
        assert!(failure_text(asura_client::Error::Unavailable).contains("unconfirmed"));
    }
    fn command_request() -> pb::ConversationQueueCommandSubmit {
        pb::ConversationQueueCommandSubmit {
            request_id: Some(vec![7; 16]),
            project_id: Some(vec![1; 16]),
            new_conversation: Some(true),
            expected_generation: Some(0),
            command: Some("ls".into()),
            cwd: Some(".".into()),
            outside_sandbox: Some(true),
            ..Default::default()
        }
    }
    /// A refused person command maps to a queue reply that carries the
    /// request ID, the refusal and its status, and nothing queued; an
    /// accepted one passes the service reply through unchanged.
    #[test]
    fn person_command_refusal_maps_to_a_refusal_reply() {
        let request = command_request();
        let refused = command_reply(
            &request,
            asura_client::CommandSubmit::Refused {
                refusal: pb::QueueRefusal::SecretStoreUnavailable,
                status: Some(25_307),
            },
        );
        assert_eq!(refused.request_id, request.request_id);
        assert_eq!(
            refused.refusal,
            Some(pb::QueueRefusal::SecretStoreUnavailable as i32)
        );
        assert_eq!(refused.refusal_status, Some(25_307));
        assert!(refused.entries.is_empty() && refused.accepted_input_id.is_none());
        assert_eq!(refused.full_text, Some(false));
        let accepted = pb::ConversationQueueReply {
            request_id: request.request_id.clone(),
            accepted_input_id: Some(vec![9; 16]),
            ..Default::default()
        };
        assert_eq!(
            command_reply(
                &request,
                asura_client::CommandSubmit::Accepted(accepted.clone())
            ),
            accepted
        );
        assert!(!COMMANDS_NOT_ENABLED.contains("unconfirmed"));
    }
    use std::time::{Duration, Instant};
    fn held_worker(cancelled: bool) -> (Worker, mpsc::SyncSender<()>) {
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
            let _ = wait.recv();
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
                    stopped: Arc::new(AtomicBool::new(cancelled)),
                    stop_reported: false,
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
    fn j1_0_05_held_wait_has_no_timer_second_request_is_busy_and_cancel_stops_it() {
        let (mut worker, release) = held_worker(false);
        // The removed 5 s observation bound: waiting longer reports nothing.
        std::thread::sleep(Duration::from_millis(5100));
        assert!(worker.poll().is_none());
        assert!(worker.is_busy());
        let resolve: Resolver = |_| Err(asura_platform::Error::Unavailable);
        let second = worker.submit(
            resolve,
            Request::List {
                project: [2; 16],
                input: None,
            },
        );
        assert!(second.unwrap_err().contains("pending"));
        let started = Instant::now();
        worker.cancel();
        let update = worker.poll().unwrap();
        assert!(started.elapsed() < Duration::from_millis(100));
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
