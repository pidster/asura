//! Conversation input enters only through the input queue, as the TUI sends
//! it (owner, 2026-10-17). These helpers queue one input and wait for the
//! turn it becomes. They are test-harness polls with a fixed budget; the
//! service itself has no time bound on queued work.
use asura_client::Client;
use asura_control::pb;
use std::thread;
use std::time::{Duration, Instant};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

/// The turn a queued input became.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Accepted {
    pub input_id: [u8; 16],
    pub operation_id: Option<Vec<u8>>,
    pub conversation_id: Option<Vec<u8>>,
    pub generation: Option<u64>,
}

/// One queue submission for `conversation`, or a new conversation when absent.
pub fn queue_request(
    project: [u8; 16],
    conversation: Option<Vec<u8>>,
    generation: u64,
    prompt: &str,
) -> pb::ConversationQueueSubmit {
    pb::ConversationQueueSubmit {
        request_id: Some(asura_platform::random_id().to_vec()),
        project_id: Some(project.to_vec()),
        new_conversation: Some(conversation.is_none()),
        conversation_id: conversation,
        expected_generation: Some(generation),
        prompt: Some(prompt.into()),
    }
}

/// Retries only `conversation_busy`, which the serialized writer returns while
/// another foreground command holds the journal ticket.
pub fn queue_call(
    attach: &dyn Fn() -> Result<Client>,
    request: pb::ConversationQueueSubmit,
) -> Result<pb::ConversationQueueReply> {
    let end = Instant::now() + Duration::from_secs(10);
    loop {
        match attach()?.queue_conversation_input(request.clone(), &|| false) {
            Ok(reply) => return Ok(reply),
            Err(asura_client::Error::Remote(_, message))
                if message == "conversation_busy" && Instant::now() < end =>
            {
                thread::sleep(Duration::from_millis(10));
            }
            Err(error) => return Err(error.into()),
        }
    }
}

/// Queues `prompt` and waits until the service dispatches it. A preparation
/// failure holds the input: the helper drops it, so later submissions are not
/// blocked, and reports the hold reason as the remote error.
pub fn submit_queued(
    attach: &dyn Fn() -> Result<Client>,
    project: [u8; 16],
    conversation: Option<Vec<u8>>,
    generation: u64,
    prompt: &str,
) -> Result<Accepted> {
    let reply = queue_call(
        attach,
        queue_request(project, conversation, generation, prompt),
    )?;
    let input: [u8; 16] = reply
        .accepted_input_id
        .as_deref()
        .ok_or("queue receipt lacks input")?
        .try_into()?;
    dispatched(attach, project, input)
}

/// Waits for the exact input to become a turn (Running or later).
pub fn dispatched(
    attach: &dyn Fn() -> Result<Client>,
    project: [u8; 16],
    input: [u8; 16],
) -> Result<Accepted> {
    let end = Instant::now() + Duration::from_secs(150);
    loop {
        let entry = match attach()?.conversation_queue(project, Some(input)) {
            Ok(reply) => reply.entries.into_iter().next(),
            Err(asura_client::Error::Remote(_, message)) if message == "conversation_busy" => None,
            Err(error) => return Err(error.into()),
        };
        if let Some(entry) = entry {
            if entry.operation_id.is_some() {
                return Ok(Accepted {
                    input_id: input,
                    operation_id: entry.operation_id,
                    conversation_id: entry.conversation_id,
                    generation: entry.generation,
                });
            }
            if matches!(entry.state, Some(2 | 6)) {
                let reason = entry.hold_reason.unwrap_or_else(|| "input_held".into());
                if entry.state == Some(2) {
                    attach()?.decide_conversation_input(
                        pb::ConversationQueueDecision {
                            request_id: Some(asura_platform::random_id().to_vec()),
                            input_id: Some(input.to_vec()),
                            action: Some(2),
                            ..Default::default()
                        },
                        &|| false,
                    )?;
                }
                return Err(asura_client::Error::Remote(0, reason).into());
            }
        }
        if Instant::now() >= end {
            return Err("queued input was not dispatched".into());
        }
        thread::sleep(Duration::from_millis(20));
    }
}
