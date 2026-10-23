//! One bounded, coalescing conversation client worker. No admission policy.
use super::observation::Resolver;
use asura_control::pb;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::thread::JoinHandle;
#[cfg(test)]
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
/// Restored person entries: the turns before each and its input ID.
type RestoredPersons = Vec<(usize, [u8; 16])>;
/// One validated history entry, as listed (newest first).
#[derive(Clone, Debug, PartialEq, Eq)]
enum HistoryItem {
    Turn {
        operation: [u8; 16],
        generation: u64,
    },
    Person([u8; 16]),
}
/// Validates a history reply's entries without reading anything else. A
/// person entry has its input ID, the frame and a generation of at most the
/// reply's, and no operation or kind; generation 0 is allowed only when
/// every entry is a person entry (control API additions).
fn history_items(history: &pb::ConversationHistoryReply) -> Result<Vec<HistoryItem>, String> {
    let generation = history.generation.unwrap_or(0);
    if history.entries.len() > 8 {
        return Err("Invalid restored conversation history".into());
    }
    let mut previous_frame = u64::MAX;
    let mut items = Vec::with_capacity(history.entries.len());
    for entry in &history.entries {
        let frame = entry.accepted_frame.ok_or("Missing history cursor")?;
        if frame >= previous_frame {
            return Err("Conversation history is not ordered".into());
        }
        previous_frame = frame;
        if let Some(input) = entry.person_command_input_id.as_deref() {
            let input: [u8; 16] = input.try_into().map_err(|_| "Invalid history input")?;
            if entry.operation_id.is_some()
                || entry.kind.is_some()
                || entry.generation.is_none_or(|g| g > generation)
            {
                return Err("Invalid person history entry".into());
            }
            items.push(HistoryItem::Person(input));
            continue;
        }
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
        items.push(HistoryItem::Turn {
            operation,
            generation: turn_generation,
        });
    }
    Ok(items)
}
#[derive(Clone)]
pub(super) struct RestoreUpdate {
    pub project: [u8; 16],
    pub epoch: [u8; 16],
    pub result: Result<Restored, String>,
    /// Person command entries (history field 5) of a restored history,
    /// oldest first: the number of turns before each and its input ID. The
    /// log reads each job through the job API.
    pub persons: Vec<(usize, [u8; 16])>,
}
#[derive(Clone, Default)]
pub(super) struct Update {
    pub event: Option<pb::ConversationEvent>,
    pub message: Option<String>,
    pub project: Option<[u8; 16]>,
    pub project_offer: Option<String>,
    pub launch_directory: Option<String>,
    /// A new project window (CF06b): at most 64 consecutive entries.
    pub projects: Option<Vec<pb::ProjectReply>>,
    /// Whether reads before the window's first or after its last entry
    /// may find more projects.
    pub more_before: bool,
    pub more_after: bool,
    /// Rows outside the window this worker read: launch matches and a
    /// registered project. The selected project's row may come from here.
    pub known_rows: Vec<pb::ProjectReply>,
    pub restore: Option<RestoreUpdate>,
    pub preserve_project_selection: bool,
    pub other_matches: Vec<[u8; 16]>,
    /// The service acknowledged the durable `CancelRequested` record.
    pub cancel_accepted: bool,
    pub success: bool,
    pub done: bool,
}
fn restore(
    client: &mut asura_client::Client,
    project: [u8; 16],
    epoch: [u8; 16],
    stopped: &AtomicBool,
) -> Result<(Restored, RestoredPersons), String> {
    if client.service_epoch() != epoch {
        return Err("Service changed during conversation restoration".into());
    }
    // Restoration has no time bound (owner, 2026-10-16): each read ends on its
    // reply, exit or scope change (`stopped`), or a lost connection.
    let history = client
        .conversation_history(project, None, None, 8)
        .map_err(|e| e.to_string())?;
    let conversation_id = history.conversation_id.clone();
    let generation = history.generation.unwrap_or(0);
    if history.entries.is_empty() {
        if conversation_id.is_some() || generation != 0 {
            return Err("Invalid empty conversation history".into());
        }
        return Ok((
            Restored {
                conversation_id: None,
                generation: 0,
                turns: Vec::new(),
            },
            Vec::new(),
        ));
    }
    let conversation = conversation_id
        .as_ref()
        .filter(|id| id.len() == 16)
        .ok_or("Invalid restored conversation identity")?;
    let items = history_items(&history)?;
    let mut turns = Vec::with_capacity(items.len());
    // (turns listed after it, input), newest first.
    let mut persons = Vec::new();
    for item in items {
        if stopped.load(Ordering::Acquire) {
            return Err("Conversation restoration stopped".into());
        }
        let (operation, turn_generation) = match item {
            HistoryItem::Person(input) => {
                persons.push((turns.len(), input));
                continue;
            }
            HistoryItem::Turn {
                operation,
                generation,
            } => (operation, generation),
        };
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
        // The whole response, assembled from windows of at most 61,440
        // bytes; a response has no size total (response-text-transfer.md).
        let event = client
            .read_response(operation, &mut asura_client::ResponseAssembly::default())
            .map_err(|e| e.to_string())?;
        if event.generation != Some(turn_generation) || !matches!(event.kind, Some(1..=6)) {
            return Err("Conversation observation changed identity".into());
        }
        turns.push(RestoredTurn {
            prompt: text,
            event,
        });
    }
    turns.reverse();
    let count = turns.len();
    let persons = persons
        .into_iter()
        .rev()
        .map(|(newer, input)| (count - newer, input))
        .collect();
    Ok((
        Restored {
            conversation_id,
            generation,
            turns,
        },
        persons,
    ))
}
// Each transport call remains bounded by Client. Turn age does not terminate
// subscription; the existing owner keeps the worker slot until it settles.
/// Each event carries one window; the worker assembles the whole response
/// and its thinking text and publishes them (response-text-transfer.md,
/// thinking-level.md). `observe` takes the cursor, the held response and
/// thinking byte counts, the generation and a cancel request.
fn observe_until_terminal(
    stopped: &AtomicBool,
    cancellation: &AtomicBool,
    mut generation: u64,
    mut observe: impl FnMut(u64, (u64, u64), u64, bool) -> Result<Option<pb::ConversationEvent>, String>,
    mut publish: impl FnMut(Update),
) -> Result<(), String> {
    let mut assembly = asura_client::ResponseAssembly::default();
    while !stopped.load(Ordering::Acquire) {
        let cancel = generation > 0 && cancellation.swap(false, Ordering::AcqRel);
        if let Some(event) = observe(
            assembly.cursor(),
            (assembly.held(), assembly.held_thinking()),
            generation,
            cancel,
        )? {
            generation = event.generation.unwrap_or(generation);
            assembly
                .apply(&event)
                .map_err(|_| "Invalid conversation observation".to_string())?;
            let done = assembly.done();
            // A terminal kind is published only with its whole response, so
            // the UI never settles a turn on a partial terminal text.
            if done || !matches!(event.kind, Some(3..=6)) {
                publish(Update {
                    event: Some(assembly.whole(event)),
                    success: true,
                    done,
                    ..Default::default()
                });
            }
            if done {
                return Ok(());
            }
        }
    }
    Ok(())
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
                    if slot.as_ref().is_some_and(|previous| previous.cancel_accepted) {
                        update.cancel_accepted = true;
                    }
                    *slot = Some(update);
                    drop(slot);
                    notice.ready();
                };
                // Waits have no time bound (owner, 2026-10-16): each ends on
                // the reply, exit or scope change (`stopped`), or a lost connection.
                let attach = || {
                    let runtime = resolve(false)?;
                    let mut client = asura_client::Client::attach(
                        &runtime,
                        concat!("asura/", env!("CARGO_PKG_VERSION")),
                        &|| stopped.load(Ordering::Acquire),
                    )?;
                    let read_stop = stopped.clone();
                    client.set_cancel(Arc::new(move || read_stop.load(Ordering::Acquire)));
                    Ok::<_, asura_client::Error>(client)
                };
                let restore_scope = match &request {
                    Request::Restore { project, epoch } => Some((*project, *epoch)),
                    _ => None,
                };
                let run = || -> Result<(), String> {
                    let mut client = attach().map_err(|e| e.to_string())?;
                    let (operation, generation) = match request {
                        Request::Restore { project, epoch } => {
                            let (result, persons) =
                                match restore(&mut client, project, epoch, &stopped) {
                                    Ok((restored, persons)) => (Ok(restored), persons),
                                    Err(error) => (Err(error), Vec::new()),
                                };
                            publish(Update {
                                restore: Some(RestoreUpdate {
                                    project,
                                    epoch,
                                    result,
                                    persons,
                                }),
                                done: true,
                                ..Default::default()
                            });
                            return Ok(());
                        }
                        Request::DiscoverProject => {
                            // Discovery reads every page (no page total) to find
                            // the launch match; it keeps the first window and
                            // the matches only.
                            let launch = crate::conversation::location(".")?;
                            let discovery = crate::conversation::discover(
                                |read, limit| client.projects(read, limit).map_err(|e| e.to_string()),
                                || if stopped.load(Ordering::Acquire) { Err("Project discovery cancelled".into()) } else { Ok(()) },
                                &launch,
                            )?;
                            let (project, other_matches) = crate::conversation::initial_project(&discovery.matches, &launch);
                            let project_offer = discovery.empty.then(|| launch.clone());
                            publish(Update{projects: Some(discovery.window.rows), more_after: discovery.window.more_after, known_rows: discovery.matches, preserve_project_selection: true, project, other_matches, project_offer, launch_directory: Some(launch), success:true,done:true,..Default::default()});
                            return Ok(());
                        }
                        Request::Setup(crate::conversation::Command::Observe(id)) => (id, 0),
                        Request::Setup(crate::conversation::Command::List(after)) => {
                            let window = crate::conversation::fill_window(
                                |read, limit| client.projects(read, limit).map_err(|e| e.to_string()),
                                || if stopped.load(Ordering::Acquire) { Err("Project list cancelled".into()) } else { Ok(()) },
                                after,
                            )?;
                            let message = window.message();
                            publish(Update { projects: Some(window.rows), more_before: window.more_before, more_after: window.more_after, preserve_project_selection: true, message: Some(message), success: true, done: true, ..Default::default() });
                            return Ok(());
                        }
                        Request::Setup(command) => {
                            let command = match command {
                                crate::conversation::Command::Register(id, path) => {
                                    crate::conversation::Command::Register(id, crate::conversation::location(&path)?)
                                }
                                other => other,
                            };
                            let stop_waiting = || cancellation.load(Ordering::Acquire) || stopped.load(Ordering::Acquire);
                            let (mut message, project) = crate::conversation::execute(&mut client, command, &stop_waiting)?;
                            let mut update = Update { message: None, project, success: true, done: true, ..Default::default() };
                            if let Some(id) = project {
                                // Refresh the window; read the registered row
                                // exactly when it falls outside the window.
                                let refreshed = crate::conversation::fill_window(
                                    |read, limit| client.projects(read, limit).map_err(|e| e.to_string()),
                                    || if stopped.load(Ordering::Acquire) { Err("Registry refresh stopped".into()) } else { Ok(()) },
                                    None,
                                )
                                .and_then(|window| {
                                    let listed = window.rows.iter().any(|row| row.project_id.as_deref() == Some(id.as_slice()));
                                    let known = if listed { Vec::new() } else {
                                        crate::conversation::project_page(&mut client, crate::conversation::ProjectPage::Only(id), 1)?.rows
                                    };
                                    Ok((window, known))
                                });
                                match refreshed {
                                    Ok((window, known)) => {
                                        update.projects = Some(window.rows);
                                        update.more_after = window.more_after;
                                        update.known_rows = known;
                                    }
                                    Err(error) => message.push_str(&format!("\nRegistration succeeded; registry refresh failed: {error}. Use /project list.")),
                                }
                            }
                            update.message = Some(message);
                            publish(update);
                            return Ok(());
                        }
                    };
                    observe_until_terminal(
                        &stopped,
                        &cancellation,
                        generation,
                        |cursor, held, generation, cancel| {
                            if cancel {
                                // The reply follows the durable CancelRequested record (J1-0).
                                client.cancel_conversation(operation, generation, &|| stopped.load(Ordering::Acquire)).map_err(|e| format!("Cancel outcome unconfirmed: {e}"))?;
                                publish(Update { cancel_accepted: true, success: true, ..Default::default() });
                            }
                            match client.observe_with_thinking(operation, cursor, held.0, held.1) {
                                Ok(event) => Ok(Some(event)),
                                Err(_) => {
                                    client = attach().map_err(|e| format!("Disconnected: {e}. Use /observe {}", crate::conversation::hex(&operation)))?;
                                    Ok(None)
                                }
                            }
                        },
                        &publish,
                    )
                };
                if let Err(message) = run() {
                    if let Some((project, epoch)) = restore_scope {
                        publish(Update {
                            restore: Some(RestoreUpdate {
                                project,
                                epoch,
                                result: Err(message),
                                persons: Vec::new(),
                            }),
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
                final_update.cancel_accepted |= update.as_ref().is_some_and(|v| v.cancel_accepted);
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
    fn person_entry(frame: u64, input: u8, generation: u64) -> pb::ConversationHistoryEntry {
        pb::ConversationHistoryEntry {
            accepted_frame: Some(frame),
            generation: Some(generation),
            person_command_input_id: Some(vec![input; 16]),
            ..Default::default()
        }
    }
    fn turn_entry(frame: u64, operation: u8, generation: u64) -> pb::ConversationHistoryEntry {
        pb::ConversationHistoryEntry {
            operation_id: Some(vec![operation; 16]),
            generation: Some(generation),
            accepted_frame: Some(frame),
            kind: Some(3),
            ..Default::default()
        }
    }
    /// History field 5: person entries are listed in order with turns; a
    /// conversation of only person entries may have generation 0; a person
    /// entry with an operation, a kind or a later generation is invalid.
    #[test]
    fn restored_history_lists_person_entries_with_turns() {
        let reply = |generation, entries| pb::ConversationHistoryReply {
            conversation_id: Some(vec![9; 16]),
            generation: Some(generation),
            entries,
            ..Default::default()
        };
        let items = history_items(&reply(
            2,
            vec![
                person_entry(50, 3, 2),
                turn_entry(40, 2, 2),
                person_entry(30, 1, 1),
                turn_entry(20, 1, 1),
            ],
        ))
        .unwrap();
        assert_eq!(
            items,
            [
                HistoryItem::Person([3; 16]),
                HistoryItem::Turn {
                    operation: [2; 16],
                    generation: 2
                },
                HistoryItem::Person([1; 16]),
                HistoryItem::Turn {
                    operation: [1; 16],
                    generation: 1
                },
            ]
        );
        assert_eq!(
            history_items(&reply(0, vec![person_entry(9, 4, 0)])).unwrap(),
            [HistoryItem::Person([4; 16])]
        );
        assert!(history_items(&reply(0, vec![turn_entry(9, 4, 0)])).is_err());
        let mut with_operation = person_entry(9, 4, 0);
        with_operation.operation_id = Some(vec![1; 16]);
        assert!(history_items(&reply(1, vec![with_operation])).is_err());
        let mut with_kind = person_entry(9, 4, 0);
        with_kind.kind = Some(3);
        assert!(history_items(&reply(1, vec![with_kind])).is_err());
        assert!(history_items(&reply(1, vec![person_entry(9, 4, 2)])).is_err());
        let mut short = person_entry(9, 4, 0);
        short.person_command_input_id = Some(vec![1; 15]);
        assert!(history_items(&reply(1, vec![short])).is_err());
        // Order still holds across kinds.
        assert!(
            history_items(&reply(1, vec![person_entry(5, 1, 1), turn_entry(6, 1, 1)])).is_err()
        );
    }

    #[test]
    fn pending_subscription_delivers_terminal_and_preserves_cursor() {
        let stopped = AtomicBool::new(false);
        let cancellation = AtomicBool::new(false);
        let mut updates = Vec::new();
        observe_until_terminal(
            &stopped,
            &cancellation,
            0,
            |cursor, _, generation, cancel| {
                assert!(!cancel);
                assert_eq!(generation, if cursor == 0 { 0 } else { 7 });
                let text = if cursor == 2 { "complete" } else { "pending" };
                Ok(Some(pb::ConversationEvent {
                    generation: Some(7),
                    cursor: Some(cursor + 1),
                    kind: Some(if cursor == 2 { 3 } else { 2 }),
                    text: Some(text.into()),
                    // Each window replaces the held text.
                    text_offset: Some(0),
                    text_bytes: Some(text.len() as u64),
                    ..Default::default()
                }))
            },
            |update| updates.push(update),
        )
        .unwrap();
        assert_eq!(updates.len(), 3);
        assert!(!updates[1].done);
        assert!(updates[2].done);
        assert_eq!(
            updates[2].event.as_ref().unwrap().text.as_deref(),
            Some("complete")
        );
    }

    #[test]
    fn cancellation_waits_for_generation_and_disconnect_stops_subscription() {
        let stopped = AtomicBool::new(false);
        let cancellation = AtomicBool::new(true);
        let mut calls = 0;
        observe_until_terminal(
            &stopped,
            &cancellation,
            0,
            |cursor, _, generation, cancel| {
                calls += 1;
                if calls == 1 {
                    assert_eq!(generation, 0);
                    assert!(!cancel);
                    assert!(cancellation.load(Ordering::Acquire));
                } else {
                    assert_eq!(generation, 7);
                    assert!(cancel);
                    assert!(!cancellation.load(Ordering::Acquire));
                    stopped.store(true, Ordering::Release);
                }
                Ok(Some(pb::ConversationEvent {
                    generation: Some(7),
                    cursor: Some(cursor + 1),
                    kind: Some(2),
                    ..Default::default()
                }))
            },
            |_| {},
        )
        .unwrap();
        assert_eq!(calls, 2);
    }

    /// RT-03 (TUI worker): a 200,000-byte response arrives in windows of at
    /// most 61,440 bytes; the worker asks for each next window by its held
    /// count and publishes the whole response.
    #[test]
    fn rt03_worker_assembles_a_response_larger_than_one_window() {
        let full: String = "a😀é".repeat(200_000 / 7);
        let stopped = AtomicBool::new(false);
        let cancellation = AtomicBool::new(false);
        let mut updates = Vec::new();
        let mut requests = Vec::new();
        observe_until_terminal(
            &stopped,
            &cancellation,
            0,
            |cursor, (held, _), _, _| {
                requests.push((cursor, held));
                let start = held as usize;
                let mut end = (start + asura_control::MAX_RESPONSE_WINDOW_BYTES).min(full.len());
                while !full.is_char_boundary(end) {
                    end -= 1;
                }
                Ok(Some(pb::ConversationEvent {
                    generation: Some(1),
                    cursor: Some(u64::MAX),
                    kind: Some(3),
                    text: Some(full[start..end].into()),
                    text_offset: Some(held),
                    text_bytes: Some(full.len() as u64),
                    ..Default::default()
                }))
            },
            |update| updates.push(update),
        )
        .unwrap();
        assert!(requests.len() >= full.len() / asura_control::MAX_RESPONSE_WINDOW_BYTES);
        assert_eq!(requests[1], (u64::MAX, requests[1].1));
        // Partial terminal windows are not published.
        assert_eq!(updates.len(), 1);
        let last = updates.last().unwrap();
        assert!(last.done);
        assert_eq!(
            last.event.as_ref().unwrap().text.as_deref(),
            Some(full.as_str())
        );
    }

    /// TL-09 and TL-12 (TUI worker): thinking windows arrive before the
    /// response in one shared budget; the worker asks by both held counts and
    /// publishes the whole thinking text with the terminal.
    #[test]
    fn tl09_worker_assembles_thinking_before_the_response() {
        let thinking: String = "t\u{e9}".repeat(100_000 / 3);
        let answer = "Answer.";
        let stopped = AtomicBool::new(false);
        let cancellation = AtomicBool::new(false);
        let mut updates = Vec::new();
        let mut requests = Vec::new();
        observe_until_terminal(
            &stopped,
            &cancellation,
            0,
            |_, (held, held_thinking), _, _| {
                requests.push((held, held_thinking));
                let start = held_thinking as usize;
                let mut end =
                    (start + asura_control::MAX_RESPONSE_WINDOW_BYTES).min(thinking.len());
                while !thinking.is_char_boundary(end) {
                    end -= 1;
                }
                let room = asura_control::MAX_RESPONSE_WINDOW_BYTES - (end - start);
                let text = &answer[held as usize..answer.len().min(held as usize + room)];
                Ok(Some(pb::ConversationEvent {
                    generation: Some(1),
                    cursor: Some(u64::MAX),
                    kind: Some(3),
                    text: Some(text.into()),
                    text_offset: Some(held),
                    text_bytes: Some(answer.len() as u64),
                    thinking_text: Some(thinking[start..end].into()),
                    thinking_offset: Some(held_thinking),
                    thinking_bytes: Some(thinking.len() as u64),
                    ..Default::default()
                }))
            },
            |update| updates.push(update),
        )
        .unwrap();
        assert_eq!(requests[0], (0, 0));
        // The first window is full up to a character boundary.
        assert!(
            requests[1].1 >= (asura_control::MAX_RESPONSE_WINDOW_BYTES - 3) as u64
                && requests[1].0 == 0
        );
        assert_eq!(updates.len(), 1);
        let event = updates[0].event.as_ref().unwrap();
        assert_eq!(event.thinking_text.as_deref(), Some(thinking.as_str()));
        assert_eq!(event.text.as_deref(), Some(answer));
    }

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
