//! One bounded, coalescing conversation client worker. No admission policy.
use super::observation::Resolver;
use asura_control::pb;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
#[derive(Clone)]
pub(super) enum Request {
    DiscoverProject,
    Restore { project: [u8; 16], epoch: [u8; 16] },
    Setup(crate::conversation::Command),
}
#[derive(Clone)]
pub(super) struct RestoredTurn {
    pub prompt: String,
    pub event: pb::ConversationEvent,
}
#[derive(Clone)]
pub(super) struct Restored {
    pub conversation_id: Option<Vec<u8>>,
    pub generation: u64,
    pub turns: Vec<RestoredTurn>,
}
#[derive(Clone)]
pub(super) struct RestoreUpdate {
    pub project: [u8; 16],
    pub epoch: [u8; 16],
    pub result: Result<Restored, String>,
}
#[derive(Clone, Default)]
pub(super) struct Update {
    pub accepted: Option<pb::ConversationAccepted>,
    pub event: Option<pb::ConversationEvent>,
    pub message: Option<String>,
    pub project: Option<[u8; 16]>,
    pub project_offer: Option<String>,
    pub launch_directory: Option<String>,
    pub projects: Option<Vec<pb::ProjectReply>>,
    pub restore: Option<RestoreUpdate>,
    pub preserve_project_selection: bool,
    pub success: bool,
    pub done: bool,
}
fn restore(
    client: &mut asura_client::Client,
    project: [u8; 16],
    epoch: [u8; 16],
    stopped: &AtomicBool,
) -> Result<Restored, String> {
    if client.service_epoch() != epoch {
        return Err("Service changed during conversation restoration".into());
    }
    let deadline = Instant::now() + Duration::from_secs(18);
    let history = client
        .conversation_history(project, None, None, 8)
        .map_err(|e| e.to_string())?;
    let conversation_id = history.conversation_id;
    let generation = history.generation.unwrap_or(0);
    if history.entries.is_empty() {
        if conversation_id.is_some() || generation != 0 {
            return Err("Invalid empty conversation history".into());
        }
        return Ok(Restored {
            conversation_id: None,
            generation: 0,
            turns: Vec::new(),
        });
    }
    let conversation = conversation_id
        .as_ref()
        .filter(|id| id.len() == 16)
        .ok_or("Invalid restored conversation identity")?;
    if generation == 0 || history.entries.len() > 8 {
        return Err("Invalid restored conversation history".into());
    }
    let mut previous_frame = u64::MAX;
    let mut turns = Vec::with_capacity(history.entries.len());
    for entry in history.entries {
        if stopped.load(Ordering::Acquire) || Instant::now() >= deadline {
            return Err("Conversation restoration cancelled or expired".into());
        }
        let frame = entry.accepted_frame.ok_or("Missing history cursor")?;
        if frame >= previous_frame {
            return Err("Conversation history is not ordered".into());
        }
        previous_frame = frame;
        let operation: [u8; 16] = entry
            .operation_id
            .as_deref()
            .ok_or("Missing history operation")?
            .try_into()
            .map_err(|_| "Invalid history operation")?;
        let turn_generation = entry.generation.ok_or("Missing history generation")?;
        if turn_generation == 0 || turn_generation > generation {
            return Err("Invalid history generation".into());
        }
        let prompt = client
            .read_conversation_prompt(project, operation)
            .map_err(|e| e.to_string())?;
        if prompt.conversation_id.as_deref() != Some(conversation.as_slice())
            || prompt.generation != Some(turn_generation)
        {
            return Err("Conversation prompt changed identity".into());
        }
        let text = prompt.prompt.ok_or("Missing conversation prompt")?;
        if text.len() > 32768 {
            return Err("Conversation prompt exceeds limit".into());
        }
        let event = client
            .observe_conversation(operation, 0)
            .map_err(|e| e.to_string())?;
        if event.generation != Some(turn_generation)
            || !matches!(event.kind, Some(1..=6))
            || event.text.as_ref().is_some_and(|value| value.len() > 61440)
        {
            return Err("Conversation observation changed identity".into());
        }
        turns.push(RestoredTurn {
            prompt: text,
            event,
        });
    }
    turns.reverse();
    Ok(Restored {
        conversation_id,
        generation,
        turns,
    })
}
struct Job {
    handle: JoinHandle<()>,
    updates: Arc<Mutex<Option<Update>>>,
    stop: Arc<AtomicBool>,
    cancel: Arc<AtomicBool>,
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
        if self.job.is_some() {
            return Err("Conversation request busy; draft retained".into());
        }
        let updates: Arc<Mutex<Option<Update>>> = Arc::new(Mutex::new(None));
        let mailbox = updates.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let cancel = Arc::new(AtomicBool::new(false));
        let cancellation = cancel.clone();
        let notice = self.notice.clone();
        let handle = std::thread::Builder::new()
            .name("asura-conversation-client".into())
            .spawn(move || {
                let _settlement = notice.guard();
                let publish = |mut update: Update| {
                    let mut slot = mailbox.lock().unwrap_or_else(|e| e.into_inner());
                    if let Some(previous) = slot.as_ref() && update.accepted.is_none() {
                        update.accepted = previous.accepted.clone();
                    }
                    *slot = Some(update);
                    drop(slot);
                    notice.ready();
                };
                let attach = || {
                    let runtime = resolve(false)?;
                    asura_client::Client::attach(
                        &runtime,
                        concat!("asura/", env!("CARGO_PKG_VERSION")),
                        Instant::now() + Duration::from_secs(2),
                    )
                };
                let restore_scope = match &request {
                    Request::Restore { project, epoch } => Some((*project, *epoch)),
                    _ => None,
                };
                let run = || -> Result<(), String> {
                    let mut client = attach().map_err(|e| e.to_string())?;
                    let (operation, mut generation) = match request {
                        Request::Restore { project, epoch } => {
                            let result = restore(&mut client, project, epoch, &stopped);
                            publish(Update {
                                restore: Some(RestoreUpdate { project, epoch, result }),
                                done: true,
                                ..Default::default()
                            });
                            return Ok(());
                        }
                        Request::DiscoverProject => {
                            let deadline = Instant::now() + Duration::from_secs(18);
                            let projects = crate::conversation::project_registry(&mut client, || {
                                if stopped.load(Ordering::Acquire) { Err("Project discovery cancelled".into()) }
                                else if Instant::now() >= deadline { Err("Project discovery deadline".into()) }
                                else { Ok(()) }
                            })?;
                            let launch = crate::conversation::location(".")?;
                            let project = crate::conversation::initial_project(&projects, &launch);
                            let project_offer = projects.is_empty().then(|| launch.clone());
                            publish(Update{projects: Some(projects), preserve_project_selection: true, project, project_offer, launch_directory: Some(launch), success:true,done:true,..Default::default()});
                            return Ok(());
                        }
                        Request::Setup(crate::conversation::Command::Observe(id)) => (id, 0),
                        Request::Setup(crate::conversation::Command::List) => {
                            let deadline = Instant::now() + Duration::from_secs(18);
                            let projects = crate::conversation::project_registry(&mut client, || {
                                if stopped.load(Ordering::Acquire) { Err("Project discovery cancelled".into()) }
                                else if Instant::now() >= deadline { Err("Project discovery deadline".into()) }
                                else { Ok(()) }
                            })?;
                            let message = if projects.is_empty() { "No projects. Use /project add PATH".into() }
                            else { projects.iter().map(crate::conversation::project_list_line).collect::<Vec<_>>().join("\n") };
                            publish(Update { projects: Some(projects), preserve_project_selection: true, message: Some(message), success: true, done: true, ..Default::default() });
                            return Ok(());
                        }
                        Request::Setup(command) => {
                            let command = match command {
                                crate::conversation::Command::Register(id, path) => {
                                    crate::conversation::Command::Register(id, crate::conversation::location(&path)?)
                                }
                                other => other,
                            };
                            let (mut message, project) = crate::conversation::execute(&mut client, command)?;
                            let projects = if project.is_some() {
                                let deadline = Instant::now() + Duration::from_secs(18);
                                match crate::conversation::project_registry(&mut client, || {
                                    if stopped.load(Ordering::Acquire) || Instant::now() >= deadline { Err("Registry refresh stopped or expired".into()) } else { Ok(()) }
                                }) {
                                    Ok(projects) => Some(projects),
                                    Err(error) => { message.push_str(&format!("\nRegistration succeeded; registry refresh failed: {error}. Use /project list.")); None }
                                }
                            } else { None };
                            publish(Update { projects, message: Some(message), project, success: true, done: true, ..Default::default() });
                            return Ok(());
                        }
                    };
                    let deadline = Instant::now() + Duration::from_secs(65);
                    let mut cursor = 0;
                    while !stopped.load(Ordering::Acquire) && Instant::now() < deadline {
                        if generation > 0 && cancellation.swap(false, Ordering::AcqRel) {
                            client.cancel_conversation(operation, generation).map_err(|e| format!("Cancel outcome unconfirmed: {e}"))?;
                        }
                        match client.observe_conversation(operation, cursor) {
                            Ok(event) => {
                                generation = event.generation.unwrap_or(generation);
                                cursor = event.cursor.unwrap_or(cursor);
                                let done = matches!(event.kind, Some(3..=6));
                                publish(Update { event: Some(event), success: true, done, ..Default::default() });
                                if done { return Ok(()); }
                            }
                            Err(_) => {
                                client = attach().map_err(|e| format!("Disconnected: {e}. Use /observe {}", crate::conversation::hex(&operation)))?;
                            }
                        }
                    }
                    if stopped.load(Ordering::Acquire) { Ok(()) }
                    else { Err(format!("Observation deadline. Use /observe {}", crate::conversation::hex(&operation))) }
                };
                if let Err(message) = run() {
                    if let Some((project, epoch)) = restore_scope {
                        publish(Update {
                            restore: Some(RestoreUpdate { project, epoch, result: Err(message) }),
                            done: true,
                            ..Default::default()
                        });
                    } else {
                        publish(Update { message: Some(message), done: true, ..Default::default() });
                    }
                }
            })
            .map_err(|e| e.to_string())?;
        self.job = Some(Job {
            handle,
            updates,
            stop,
            cancel,
        });
        Ok(())
    }
    pub fn poll(&mut self) -> Option<Update> {
        let job = self.job.as_ref()?;
        let mut update = match job.updates.try_lock() {
            Ok(mut updates) => updates.take(),
            Err(std::sync::TryLockError::Poisoned(error)) => error.into_inner().take(),
            Err(std::sync::TryLockError::WouldBlock) => {
                self.notice.ready();
                return None;
            }
        };
        if job.handle.is_finished() {
            let job = self.job.take().unwrap();
            let joined = job.handle.join();
            // The sole producer has exited and released its lock. This final
            // drain cannot contend and catches a publication after our first read.
            if let Some(mut final_update) =
                job.updates.lock().unwrap_or_else(|e| e.into_inner()).take()
            {
                if final_update.accepted.is_none() {
                    final_update.accepted = update.as_ref().and_then(|v| v.accepted.clone());
                }
                update = Some(final_update);
            }
            if joined.is_err() {
                let mut failed = update.unwrap_or_default();
                failed.done = true;
                failed.success = false;
                failed.message = Some("Conversation worker failed; outcome unconfirmed".into());
                update = Some(failed);
            }
        }
        update
    }
    pub fn cancel(&self) {
        if let Some(job) = &self.job {
            job.cancel.store(true, Ordering::Release)
        }
    }
    pub fn disconnect(&self) {
        if let Some(job) = &self.job {
            job.stop.store(true, Ordering::Release)
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

    #[test]
    fn contended_mailbox_defers_without_waiting_or_losing_the_result() {
        let (release, wait) = std::sync::mpsc::channel();
        let handle = std::thread::spawn(move || {
            let _ = wait.recv_timeout(Duration::from_secs(5));
        });
        let updates = Arc::new(Mutex::new(Some(Update {
            message: Some("result retained".into()),
            ..Default::default()
        })));
        let mut worker = Worker {
            notice: Default::default(),
            job: Some(Job {
                handle,
                updates: updates.clone(),
                stop: Arc::new(AtomicBool::new(false)),
                cancel: Arc::new(AtomicBool::new(false)),
            }),
        };
        let guard = updates.lock().unwrap();
        let (send, receive) = std::sync::mpsc::channel();
        let consumer = std::thread::spawn(move || {
            let result = worker.poll();
            send.send((result, worker)).unwrap();
        });
        let observed = receive.recv_timeout(Duration::from_secs(2));
        drop(guard);
        let _ = release.send(());
        consumer.join().unwrap();
        let (result, mut worker) = observed.expect("UI waited for the producer lock");
        assert!(result.is_none());
        assert_eq!(
            worker.poll().unwrap().message.as_deref(),
            Some("result retained")
        );
        let deadline = Instant::now() + Duration::from_secs(2);
        while worker.is_busy() {
            worker.poll();
            assert!(Instant::now() < deadline, "producer did not settle");
            std::thread::yield_now();
        }
    }

    #[test]
    fn final_result_does_not_release_worker_slot_before_thread_exit() {
        let (release, wait) = std::sync::mpsc::channel();
        let handle = std::thread::spawn(move || {
            wait.recv_timeout(Duration::from_secs(2)).unwrap();
        });
        let mut worker = Worker {
            notice: Default::default(),
            job: Some(Job {
                handle,
                updates: Arc::new(Mutex::new(Some(Update {
                    success: true,
                    done: true,
                    ..Default::default()
                }))),
                stop: Arc::new(AtomicBool::new(false)),
                cancel: Arc::new(AtomicBool::new(false)),
            }),
        };
        assert!(worker.poll().unwrap().done);
        assert!(worker.is_busy());
        release.send(()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while !worker.job.as_ref().unwrap().handle.is_finished() {
            assert!(Instant::now() < deadline, "worker did not exit");
            std::thread::yield_now();
        }
        // Exit after poll is not settlement: the router must retain its hint.
        assert!(worker.running());
        while worker.is_busy() {
            worker.poll();
            assert!(Instant::now() < deadline, "worker did not settle");
            std::thread::yield_now();
        }
    }
}
