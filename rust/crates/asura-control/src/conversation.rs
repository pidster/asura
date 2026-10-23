//! Runtime-free validation of setup and conversation commands.
use crate::{MAX_RESPONSE_WINDOW_BYTES, MAX_TOOL_PROGRESS_ENTRIES, id, pb, text};
use pb::envelope::Body;
pub(super) fn handles(body: &Body) -> bool {
    matches!(
        body,
        Body::ConversationHistory(_)
            | Body::ConversationHistoryReply(_)
            | Body::ConversationReadPrompt(_)
            | Body::ConversationPromptReply(_)
            | Body::ConversationObserve(_)
            | Body::ConversationEvent(_)
            | Body::ConversationCancel(_)
            | Body::ConversationCancelAccepted(_)
            | Body::InitializeInstallation(_)
            | Body::InitializationReply(_)
            | Body::ResolveInitialization(_)
            | Body::ProjectRegister(_)
            | Body::ProjectReply(_)
            | Body::ProjectRename(_)
            | Body::ProjectRenameReply(_)
            | Body::ProjectList(_)
            | Body::ProjectsReply(_)
            | Body::ConversationQueueSubmit(_)
            | Body::ConversationQueueCommandSubmit(_)
            | Body::ConversationQueueReorder(_)
            | Body::ConversationQueueList(_)
            | Body::ConversationQueueDecision(_)
            | Body::ConversationQueueReply(_)
    )
}
fn location(value: &Option<String>) -> bool {
    value.as_ref().is_some_and(|s| {
        s.starts_with('/')
            && s.len() <= 4096
            && !s.contains('\0')
            && s.split('/').all(|p| p != "." && p != "..")
    })
}
fn project(v: &pb::ProjectReply) -> bool {
    if v.error.is_some() {
        text(&v.error, 256)
            && v.project_id.is_none()
            && v.location.is_none()
            && v.registry_revision.is_none()
            && v.current.is_none()
            && v.name.is_none()
            && v.name_revision.is_none()
    } else {
        id(&v.project_id)
            && location(&v.location)
            && v.registry_revision == Some(1)
            && v.current.is_some()
            && v.name
                .as_ref()
                .is_none_or(|s| !s.is_empty() && s.len() <= 128)
            && (v.name.is_some() == v.name_revision.is_some())
    }
}
/// A listed registration may name why it is unavailable. Only the overlap
/// reason is defined; it accompanies the registration fields.
fn listed(v: &pb::ProjectReply) -> bool {
    match v.error.as_deref() {
        None => project(v),
        Some("project_overlaps_asura_state") => {
            v.current == Some(false)
                && project(&pb::ProjectReply {
                    error: None,
                    ..v.clone()
                })
        }
        Some(_) => false,
    }
}
/// A lane target: a new conversation with generation zero, or an existing
/// conversation with its expected generation (kind 18 and kind 31 alike).
fn lane_target(
    new_conversation: Option<bool>,
    conversation_id: &Option<Vec<u8>>,
    expected_generation: Option<u64>,
) -> bool {
    match (new_conversation, conversation_id, expected_generation) {
        (Some(true), None, Some(0)) => true,
        (Some(false), Some(conversation), Some(generation)) => {
            conversation.len() == 16
                && conversation.iter().any(|byte| *byte != 0)
                && generation < u64::MAX
        }
        _ => false,
    }
}

pub(super) fn valid(body: &Body) -> bool {
    match body {
        Body::ConversationQueueSubmit(v) => {
            id(&v.request_id)
                && id(&v.project_id)
                && lane_target(
                    v.new_conversation,
                    &v.conversation_id,
                    v.expected_generation,
                )
                && v.prompt
                    .as_ref()
                    .is_some_and(|p| !p.is_empty() && p.len() <= 32768)
        }
        Body::ConversationQueueCommandSubmit(v) => person::submit_shape(v),
        Body::ConversationQueueReorder(v) => {
            id(&v.request_id)
                && id(&v.input_id)
                && (v.after_input_id.is_none() || id(&v.after_input_id))
                && v.after_input_id != v.input_id
                && v.expected_order_revision.is_some()
        }
        Body::ConversationQueueList(v) => {
            id(&v.project_id)
                && (v.input_id.is_none() || id(&v.input_id))
                && !(v.input_id.is_some() && v.after_revision.is_some())
        }
        Body::ConversationQueueDecision(v) => {
            id(&v.request_id)
                && id(&v.input_id)
                && match v.action {
                    Some(1 | 2 | 4) => {
                        v.target_operation_id.is_none() && v.target_generation.is_none()
                    }
                    Some(3) => {
                        id(&v.target_operation_id) && v.target_generation.is_some_and(|g| g > 0)
                    }
                    _ => false,
                }
        }
        Body::ConversationQueueReply(v) => {
            let full = v.full_text == Some(true);
            (v.pending != Some(true)
                || (v.entries.is_empty()
                    && v.request_id.is_none()
                    && !full
                    && v.revision.is_some()))
                && v.full_text.is_some()
                && (v.request_id.is_none() || id(&v.request_id))
                && person::refusal_shape(v)
                && (v.accepted_input_id.is_none()
                    || (id(&v.accepted_input_id)
                        && v.request_id.is_some()
                        && v.order_revision.is_some()
                        && v.stale_order == Some(false)
                        && v.entries.iter().any(|e| e.input_id == v.accepted_input_id)))
                && (v.stale_order != Some(true)
                    || (v.request_id.is_some()
                        && v.order_revision.is_some()
                        && v.pending != Some(true)
                        && !full))
                && v.entries.len() <= if full { 1 } else { 16 }
                && v.entries.iter().all(|e| {
                    id(&e.input_id)
                        && id(&e.project_id)
                        && id(&e.conversation_id)
                        && e.new_conversation.is_some()
                        // A promoted Steer entry names its exact target; a Queue
                        // entry carries its expected generation.
                        && match e.kind {
                            // A person command (kind 31) carries its expected
                            // generation as a Queue entry does.
                            Some(1 | 3) => {
                                e.target_operation_id.is_none()
                                    && e.target_generation.is_some_and(|generation| {
                                        if e.new_conversation == Some(true) {
                                            generation == 0
                                        } else {
                                            generation < u64::MAX
                                        }
                                    })
                            }
                            Some(2) => {
                                id(&e.target_operation_id)
                                    && e.target_generation.is_some_and(|n| n > 0)
                            }
                            _ => false,
                        }
                        && e.order_position.is_some_and(|position| position > 0)
                        && e.state.is_some_and(|n| (1..=6).contains(&n))
                        && e.hold_reason.as_ref().is_none_or(|reason| {
                            e.state == Some(2)
                                && !reason.is_empty()
                                && reason.len() <= 64
                                && reason.bytes().all(|b| b.is_ascii_lowercase() || b == b'_')
                        })
                        // Live pre-admission step: Queued only, preparing or starting.
                        && e.waiting_step.is_none_or(|step| e.state == Some(1) && (1..=2).contains(&step))
                        && person::entry_shape(e)
                        && e.sequence.is_some_and(|n| n > 0)
                        && e.text
                            .as_ref()
                            .is_some_and(|s| s.len() <= if full { 32768 } else { 256 })
                        // A person command runs as a job, never a model
                        // operation: no operation or generation in any state.
                        && if e.kind == Some(3) {
                            e.operation_id.is_none() && e.generation.is_none()
                        } else if matches!(e.state, Some(3..=5)) {
                            id(&e.operation_id) && e.generation.is_some_and(|n| n > 0)
                        } else {
                            e.operation_id.is_none() && e.generation.is_none()
                        }
                })
        }
        Body::ConversationHistory(v) => {
            id(&v.project_id)
                && (v.conversation_id.is_none() || id(&v.conversation_id))
                && v.before_accepted_frame.is_none_or(|n| n > 0)
                && (v.before_accepted_frame.is_none() || v.conversation_id.is_some())
                && v.limit.is_some_and(|n| (1..=8).contains(&n))
        }
        Body::ConversationHistoryReply(v) => {
            id(&v.project_id)
                && v.authority_revision.is_some()
                && v.has_more.is_some()
                && if v.conversation_id.is_none() {
                    v.generation.is_none() && v.entries.is_empty() && v.has_more == Some(false)
                } else {
                    id(&v.conversation_id)
                        // Generation 0: a conversation that holds only person
                        // commands, queued before its first turn.
                        && v.generation.is_some_and(|n| {
                            n > 0
                                || v.entries.iter().all(|e| e.person_command_input_id.is_some())
                        })
                        && v.entries.len() <= 8
                        && (v.has_more == Some(false) || !v.entries.is_empty())
                        && v.entries.iter().all(|e| {
                            e.accepted_frame.is_some_and(|n| n > 0)
                                && if e.person_command_input_id.is_some() {
                                    // A person command: its kind 31 input and
                                    // the generation it was queued against; the
                                    // log reads the job's state from the job API.
                                    id(&e.person_command_input_id)
                                        && e.operation_id.is_none()
                                        && e.kind.is_none()
                                        && e.generation.is_some_and(|n| n <= v.generation.unwrap())
                                } else {
                                    id(&e.operation_id)
                                        && e.generation
                                            .is_some_and(|n| n > 0 && n <= v.generation.unwrap())
                                        && matches!(e.kind, Some(1 | 3..=6))
                                }
                        })
                        && v.entries.windows(2).all(|pair| {
                            pair[0].accepted_frame.unwrap() > pair[1].accepted_frame.unwrap()
                        })
                }
        }
        Body::ConversationReadPrompt(v) => id(&v.project_id) && id(&v.operation_id),
        Body::ConversationPromptReply(v) => {
            id(&v.project_id)
                && id(&v.operation_id)
                && id(&v.conversation_id)
                && v.generation.is_some_and(|n| n > 0)
                && v.prompt
                    .as_ref()
                    .is_some_and(|s| !s.is_empty() && s.len() <= 32768)
        }
        Body::ConversationObserve(v) => {
            id(&v.operation_id) && v.after_cursor.is_some() && v.wait_ms.is_some_and(|n| n <= 1000)
        }
        Body::ConversationCancel(v) => id(&v.operation_id) && v.generation.is_some_and(|n| n > 0),
        Body::ConversationCancelAccepted(v) => id(&v.operation_id) && v.terminal.is_some(),
        Body::ConversationEvent(v) => {
            let base = id(&v.operation_id)
                && v.cursor.is_some()
                && v.generation.is_some_and(|n| n > 0)
                && v.reason.is_some_and(|n| (0..=12).contains(&n))
                && v.model_context.as_ref().is_none_or(|m| {
                    m.model_name.as_ref().is_none_or(|n| {
                        !n.is_empty() && n.len() <= 256 && !n.chars().any(char::is_control)
                    }) && match (m.capacity_tokens, m.reported_max_tokens, m.capacity_source) {
                        (None, None, None) => {
                            m.model_name.is_some() && m.input_tokens.is_none() && m.basis.is_none()
                        }
                        (Some(c), Some(max), Some(1..=4)) if c > 512 && max >= c => {
                            match (m.input_tokens, m.basis) {
                                (None, None) => true,
                                (Some(n), Some(1)) => n <= c,
                                _ => false,
                            }
                        }
                        _ => false,
                    }
                    // Thinking controls need reasoning (thinking-level.md).
                    && m.thinking_controls.is_none_or(|controls| {
                        controls <= crate::thinking::ALL
                            && (controls == 0 || m.reasoning == Some(true))
                    })
                })
                && v.statistics.as_ref().is_none_or(|s| {
                    // No request count or token total per turn (owner,
                    // 2026-10-18): the counts are bounded only by their types.
                    s.request_count.is_some()
                        && s.cached_input_tokens
                            .is_none_or(|n| s.input_tokens.is_some_and(|i| n <= i))
                        && s.complete.is_some()
                        && (s.complete != Some(true)
                            || (s.request_count.is_some_and(|n| n > 0)
                                && s.input_tokens.is_some()
                                && s.output_tokens.is_some()))
                })
                // The most recent kind 14 records: consecutive ordinals that
                // end at most at the over-limit ordinal (TCR-D31).
                && v.tools.len() <= MAX_TOOL_PROGRESS_ENTRIES
                && v.tools.first().is_none_or(|first| {
                    first.ordinal.is_some_and(|n| {
                        n >= 1
                            && u64::from(n) + v.tools.len() as u64
                                <= u64::from(crate::model::MAX_TOOL_ORDINAL) + 1
                    })
                })
                && v.tools.iter().enumerate().all(|(index, tool)| {
                    tool.arguments.as_ref().is_none_or(|s| s.len() <= 192)
                        && tool.preview.as_ref().is_none_or(|s| s.len() <= 128)
                        // Kind 14 decision and fault codes (TCR-D13); fault
                        // 16 is `invalid_job_id` (J3 kind 14 changes).
                        && tool.policy_decision.is_none_or(|n| (1..=8).contains(&n))
                        && tool.fault.is_none_or(|n| (1..=16).contains(&n))
                        // J4T-09: the job an admitted exec_cmd or job call
                        // names; only on an admitted call.
                        && tool.job_id.as_ref().is_none_or(|_| {
                            id(&tool.job_id) && tool.policy_decision == Some(1)
                        })
                        // SL7: set only, and only true, on an admitted call.
                        && tool.command_filtered.is_none_or(|filtered| {
                            filtered && tool.policy_decision == Some(1)
                        })
                        && tool.result_bytes.is_none_or(|n| n <= 65_536)
                        && tool.ordinal
                            == v.tools[0].ordinal.map(|first| first + index as u32)
                        // A display prefix of the redacted name as the model
                        // sent it; empty when not retained (TCR-D26).
                        && tool.name.as_ref().is_some_and(|name| name.len() <= 64)
                        && match tool.state {
                            Some(1 | 4) => tool.status.is_none(),
                            Some(2) => tool.status == Some(1),
                            Some(3) => tool.status.is_some_and(|n| (2..=7).contains(&n)),
                            _ => false,
                        }
                });
            // Live post-admission step: PENDING only, sending input, output,
            // or paused at the journal quota (segmented journal, OD-SJ-2).
            let step = v
                .waiting_step
                .is_none_or(|step| v.kind == Some(1) && (3..=5).contains(&step));
            // One response window: keep text_offset bytes, append text, giving
            // text_bytes (response-text-transfer.md). No response total exists.
            // The thinking window comes first; both texts share one budget
            // (thinking-level.md).
            let thinking_len = v.thinking_text.as_ref().map_or(0, String::len);
            let window = matches!(
                (&v.text, v.text_offset, v.text_bytes),
                (Some(text), Some(offset), Some(total))
                    if text.len() + thinking_len <= MAX_RESPONSE_WINDOW_BYTES
                        && offset.checked_add(text.len() as u64).is_some_and(|end| end <= total)
            );
            let thinking = match (&v.thinking_text, v.thinking_offset, v.thinking_bytes) {
                (None, None, None) => true,
                (Some(text), Some(offset), Some(total)) => offset
                    .checked_add(text.len() as u64)
                    .is_some_and(|end| end <= total),
                _ => false,
            };
            // EX12: jobs that outlived the turn, on terminal kinds only; at
            // most 8 distinct IDs, never more than the count.
            let outlived = if matches!(v.kind, Some(3..=6)) {
                v.outlived_job_ids.len() <= crate::jobs::MAX_OUTLIVED_JOB_IDS
                    && v.outlived_job_ids.len() as u64 <= u64::from(v.outlived_jobs.unwrap_or(0))
                    && v.outlived_job_ids.iter().enumerate().all(|(i, job)| {
                        crate::id_slice(job) && !v.outlived_job_ids[..i].contains(job)
                    })
            } else {
                v.outlived_jobs.is_none() && v.outlived_job_ids.is_empty()
            };
            base && step
                && thinking
                && outlived
                && match v.kind {
                    Some(1) => {
                        v.text.is_none()
                            && v.text_offset.is_none()
                            && v.text_bytes.is_none()
                            && v.thinking_text.is_none()
                            && v.usage_known.is_none()
                            && v.usage_tokens.is_none()
                            && v.reason == Some(0)
                    }
                    Some(2) => {
                        v.cursor.is_some_and(|n| n > 1 && n < u64::MAX)
                            && window
                            && v.reason == Some(0)
                            && v.usage_known.is_none()
                            && v.usage_tokens.is_none()
                    }
                    Some(3..=6) => {
                        v.cursor == Some(u64::MAX)
                            && window
                            && (v.kind != Some(3) || v.text_bytes.is_some_and(|n| n > 0))
                            && (v.kind != Some(3) || v.reason == Some(0))
                            && match v.usage_known {
                                Some(true) => v
                                    .usage_tokens
                                    .is_some_and(|n| n <= crate::model::MAX_COUNTED_TOKENS),
                                Some(false) => v.usage_tokens.is_none(),
                                None => false,
                            }
                    }
                    _ => false,
                }
        }
        Body::InitializeInstallation(v) => {
            id(&v.request_id)
                && v.mode.as_deref() == Some("embedded")
                && v.expected_authority_revision == Some(0)
        }
        Body::ResolveInitialization(v) => {
            id(&v.request_id) && v.request_digest.as_ref().is_some_and(|d| d.len() == 32)
        }
        Body::InitializationReply(v) => {
            if v.error.is_some() {
                text(&v.error, 256)
                    && v.installation_id.is_none()
                    && v.graph_id.is_none()
                    && v.phase.is_none()
                    && v.authority_revision.is_none()
            } else {
                id(&v.installation_id)
                    && id(&v.graph_id)
                    && matches!(v.phase, Some(1 | 2))
                    && v.authority_revision.is_some_and(|n| n > 0)
            }
        }
        Body::ProjectRegister(v) => id(&v.request_id) && location(&v.location),
        Body::ProjectReply(v) => project(v),
        Body::ProjectRename(v) => {
            id(&v.request_id)
                && id(&v.project_id)
                && v.expected_name_revision.is_some()
                && v.name
                    .as_ref()
                    .is_some_and(|s| !s.is_empty() && s.len() <= 128)
        }
        Body::ProjectRenameReply(v) => {
            if v.error.is_some() {
                text(&v.error, 256)
                    && v.changed.is_none()
                    && if v.error.as_deref() == Some("stale_project_name_revision") {
                        v.project.as_ref().is_some_and(|p| {
                            p.error.is_none()
                                && p.name.is_some()
                                && p.name_revision.is_some()
                                && project(p)
                        }) && v.current_project.as_ref().is_some_and(|p| {
                            p.error.is_none()
                                && p.name.is_some()
                                && p.name_revision.is_some()
                                && project(p)
                        })
                    } else {
                        v.project.is_none() && v.current_project.is_none()
                    }
            } else {
                v.changed.is_some()
                    && v.project.as_ref().is_some_and(|p| {
                        p.error.is_none()
                            && p.name.is_some()
                            && p.name_revision.is_some()
                            && project(p)
                    })
                    && v.current_project.as_ref().is_some_and(|p| {
                        p.error.is_none()
                            && p.name.is_some()
                            && p.name_revision.is_some()
                            && project(p)
                    })
                    && v.project
                        .as_ref()
                        .zip(v.current_project.as_ref())
                        .is_some_and(|(result, current)| {
                            result.project_id == current.project_id
                                && current.name_revision >= result.name_revision
                        })
            }
        }
        Body::ProjectList(v) => {
            [
                &v.after_project_id,
                &v.before_project_id,
                &v.only_project_id,
            ]
            .iter()
            .filter(|cursor| cursor.is_some())
            .count()
                <= 1
                && (v.after_project_id.is_none() || id(&v.after_project_id))
                && (v.before_project_id.is_none() || id(&v.before_project_id))
                && (v.only_project_id.is_none() || id(&v.only_project_id))
                && v.limit.is_some_and(|n| (1..=8).contains(&n))
        }
        Body::ProjectsReply(v) => {
            if v.error.is_some() {
                text(&v.error, 256) && v.projects.is_empty() && v.next_cursor.is_none()
            } else {
                v.projects.len() <= 8
                    && v.projects.iter().all(listed)
                    && (v.next_cursor.is_none() || id(&v.next_cursor))
            }
        }
        _ => false,
    }
}

/// Person command mode (person-command-mode.md, control API additions): the
/// submit's shape, its environment refusals, and the kind 3 queue entry and
/// refusal reply rules. Runtime-free.
pub mod person {
    use super::lane_target;
    use crate::{id, pb};
    use std::collections::HashSet;

    /// UTF-8 bytes of a command (field 6).
    pub const MAX_COMMAND_BYTES: usize = 8_192;
    /// Bytes of a project-relative working directory (field 7).
    pub const MAX_CWD_BYTES: usize = 1_024;
    /// Bytes of the client PATH (field 8, `!` only).
    pub const MAX_SEARCH_PATH_BYTES: usize = 4_096;
    /// PC3 client variables (field 9, `!` only): one per listed name.
    pub const MAX_CLIENT_VARIABLES: usize = 9;
    /// `!!` launch environment entries (field 11, OD4).
    pub const MAX_LAUNCH_VARIABLES: usize = 1_024;
    /// Name and value bytes of the `!!` launch environment together (OD4).
    pub const MAX_LAUNCH_ENVIRONMENT_BYTES: usize = 40_960;
    /// Bytes of one launch variable name.
    pub const MAX_LAUNCH_NAME_BYTES: usize = 1_024;
    /// Bytes of a queue entry's brief waiting text.
    pub const MAX_WAITING_TEXT_BYTES: usize = 128;
    /// Bytes of a PC3 locale value.
    pub const MAX_LOCALE_VALUE_BYTES: usize = 64;
    /// Bytes of an `SSH_AUTH_SOCK` value: a `sun_path` holds 104 with its NUL.
    pub const MAX_AUTH_SOCKET_BYTES: usize = 103;
    /// The PC3 names a client may send for a `!` command, in kind 31 name
    /// code order (code = index + 1).
    pub const CLIENT_VARIABLES: [&str; 9] = [
        "LANG",
        "LC_ALL",
        "LC_COLLATE",
        "LC_CTYPE",
        "LC_MESSAGES",
        "LC_MONETARY",
        "LC_NUMERIC",
        "LC_TIME",
        "SSH_AUTH_SOCK",
    ];
    /// Name prefixes the service sets itself; a launch environment that
    /// names one is `environment_invalid`.
    pub const RESERVED_PREFIXES: [&[u8]; 2] = [b"ASURA_SECRET_", b"ASURA_JOB_"];
    /// The highest `QueueWaitReason` value in this protocol revision.
    pub const MAX_WAIT_REASON: i32 = 15;
    /// The highest `QueueRefusal` value.
    pub const MAX_REFUSAL: i32 = 13;
    /// Command entry kind in `ConversationQueueEntry.kind`.
    pub const ENTRY_KIND: u32 = 3;

    /// Protocol shape of a `ConversationQueueCommandSubmit`: presence, lane
    /// target and sizes. A shape failure is a protocol fault. Environment
    /// content is not shape: [`environment_refusal`] reports it as a typed
    /// refusal the service returns.
    pub fn submit_shape(v: &pb::ConversationQueueCommandSubmit) -> bool {
        id(&v.request_id)
            && id(&v.project_id)
            && lane_target(
                v.new_conversation,
                &v.conversation_id,
                v.expected_generation,
            )
            && v.command
                .as_ref()
                .is_some_and(|c| !c.is_empty() && c.len() <= MAX_COMMAND_BYTES && !c.contains('\0'))
            && v.cwd
                .as_ref()
                .is_some_and(|c| !c.is_empty() && c.len() <= MAX_CWD_BYTES)
            && v.search_path
                .as_ref()
                .is_none_or(|p| p.len() <= MAX_SEARCH_PATH_BYTES)
            && v.environment.len() <= MAX_CLIENT_VARIABLES
    }

    /// The environment refusal of a well-shaped submit, or `None`. With
    /// `outside_sandbox` absent or false, `launch_environment` must be empty
    /// and fields 8 and 9 follow the `!` (PC3) rules; with it true, fields 8
    /// and 9 must be absent and the launch environment follows the outside
    /// bounds. Size is checked before form, so an oversized environment is
    /// `environment_too_large` whatever its names. Values are bytes and need
    /// not be UTF-8.
    pub fn environment_refusal(v: &pb::ConversationQueueCommandSubmit) -> Option<pb::QueueRefusal> {
        use pb::QueueRefusal::{EnvironmentInvalid, EnvironmentTooLarge};
        if v.outside_sandbox != Some(true) {
            if !v.launch_environment.is_empty() || !client_environment(&v.environment) {
                return Some(EnvironmentInvalid);
            }
            return None;
        }
        if v.search_path.is_some() || !v.environment.is_empty() {
            return Some(EnvironmentInvalid);
        }
        let bytes = v.launch_environment.iter().fold(0usize, |total, entry| {
            total
                .saturating_add(entry.name.as_ref().map_or(0, Vec::len))
                .saturating_add(entry.value.as_ref().map_or(0, Vec::len))
        });
        if v.launch_environment.len() > MAX_LAUNCH_VARIABLES || bytes > MAX_LAUNCH_ENVIRONMENT_BYTES
        {
            return Some(EnvironmentTooLarge);
        }
        let mut names = HashSet::with_capacity(v.launch_environment.len());
        for entry in &v.launch_environment {
            let (Some(name), Some(value)) = (&entry.name, &entry.value) else {
                return Some(EnvironmentInvalid);
            };
            if name.is_empty()
                || name.len() > MAX_LAUNCH_NAME_BYTES
                || name.contains(&b'=')
                || name.contains(&0)
                || value.contains(&0)
                || RESERVED_PREFIXES.iter().any(|p| name.starts_with(p))
                || !names.insert(name.as_slice())
            {
                return Some(EnvironmentInvalid);
            }
        }
        None
    }

    /// PC3: only listed names, each at most once, with checked values.
    fn client_environment(entries: &[pb::PersonEnvironment]) -> bool {
        let mut seen = [false; CLIENT_VARIABLES.len()];
        entries.iter().all(|entry| {
            let (Some(name), Some(value)) = (&entry.name, &entry.value) else {
                return false;
            };
            let Some(code) = CLIENT_VARIABLES.iter().position(|n| n == name) else {
                return false;
            };
            if std::mem::replace(&mut seen[code], true) {
                return false;
            }
            if name == "SSH_AUTH_SOCK" {
                value.starts_with('/')
                    && value.len() <= MAX_AUTH_SOCKET_BYTES
                    && !value.contains('\0')
            } else {
                !value.is_empty()
                    && value.len() <= MAX_LOCALE_VALUE_BYTES
                    && value.bytes().all(|b| {
                        b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b'@')
                    })
            }
        })
    }

    /// The PCR9 working directory form: relative, no NUL, no `..`
    /// component, "." for the root. The service answers `cwd_invalid`
    /// otherwise, and resolves the path under the project without symlinks.
    pub fn cwd_form_valid(cwd: &str) -> bool {
        !cwd.is_empty()
            && cwd.len() <= MAX_CWD_BYTES
            && !cwd.starts_with('/')
            && !cwd.contains('\0')
            && cwd.split('/').all(|part| part != "..")
    }

    /// Fields 16-20 of a queue entry: only on kind 3; the waiting reason and
    /// its text only together and only in state 1 (Queued).
    pub(crate) fn entry_shape(e: &pb::ConversationQueueEntry) -> bool {
        if e.kind != Some(ENTRY_KIND) {
            return e.command_cwd.is_none()
                && e.command_outside_sandbox.is_none()
                && e.waiting_reason.is_none()
                && e.waiting_text.is_none()
                && e.command_filtered.is_none()
                && e.path_warnings.is_empty()
                && e.path_overflow.is_none();
        }
        e.command_cwd
            .as_ref()
            .is_some_and(|c| !c.is_empty() && c.len() <= MAX_CWD_BYTES && !c.contains('\0'))
            && e.command_outside_sandbox.is_some()
            // A command waits with a reason, never a model wait step.
            && e.waiting_step.is_none()
            && match (e.waiting_reason, &e.waiting_text) {
                (None, None) => true,
                (Some(reason), Some(text)) => {
                    e.state == Some(1)
                        && (1..=MAX_WAIT_REASON).contains(&reason)
                        && !text.is_empty()
                        && text.len() <= MAX_WAITING_TEXT_BYTES
                        && !text.chars().any(char::is_control)
                }
                _ => false,
            }
            // Set only, and only true, as ToolProgress.command_filtered.
            && e.command_filtered.is_none_or(|filtered| filtered)
            // Fields 21-22: the path warnings shown before the command.
            && crate::path_guard::warnings_valid(&e.path_warnings, e.path_overflow)
    }

    /// A refusal reply carries the request ID and the refusal, and nothing
    /// queued: no entries, no receipt. `refusal_status` comes only with
    /// `secret_store_unavailable`, as a nonzero |OSStatus|.
    pub(crate) fn refusal_shape(v: &pb::ConversationQueueReply) -> bool {
        match v.refusal {
            None => v.refusal_status.is_none(),
            Some(refusal) => {
                (1..=MAX_REFUSAL).contains(&refusal)
                    && id(&v.request_id)
                    && v.entries.is_empty()
                    && v.accepted_input_id.is_none()
                    && v.full_text == Some(false)
                    && v.pending != Some(true)
                    && v.stale_order != Some(true)
                    && if refusal == pb::QueueRefusal::SecretStoreUnavailable as i32 {
                        v.refusal_status.is_some_and(|status| status > 0)
                    } else {
                        v.refusal_status.is_none()
                    }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Direction, HEADER_BYTES, decode_body, encode_frame, validate_semantics};
    fn envelope(body: Body) -> pb::Envelope {
        pb::Envelope {
            service_epoch: Some(vec![1; 16]),
            attachment_id: Some(vec![2; 16]),
            request_counter: Some(1),
            body: Some(body),
        }
    }
    #[test]
    fn queue_submit_presence_generation_bounds_and_direction() {
        let request = pb::ConversationQueueSubmit {
            request_id: Some(vec![3; 16]),
            project_id: Some(vec![4; 16]),
            conversation_id: None,
            expected_generation: Some(0),
            new_conversation: Some(true),
            prompt: Some("Hello".into()),
        };
        let value = envelope(Body::ConversationQueueSubmit(request.clone()));
        assert!(validate_semantics(&value, Direction::ClientToServer).is_ok());
        assert!(validate_semantics(&value, Direction::ServerToClient).is_err());
        assert_eq!(
            decode_body(&encode_frame(&value).unwrap()[HEADER_BYTES..]).unwrap(),
            value
        );
        for changed in [
            pb::ConversationQueueSubmit {
                conversation_id: Some(vec![]),
                ..request.clone()
            },
            pb::ConversationQueueSubmit {
                expected_generation: None,
                ..request.clone()
            },
            pb::ConversationQueueSubmit {
                prompt: Some("x".repeat(32769)),
                ..request.clone()
            },
            pb::ConversationQueueSubmit {
                conversation_id: Some(vec![5; 16]),
                ..request
            },
        ] {
            assert!(!valid(&Body::ConversationQueueSubmit(changed)));
        }
    }
    /// Removed operations 22 (Submit), 23 (Accepted) and 35 (Enqueue) carry
    /// no body; the service never treats them as a request.
    #[test]
    fn removed_conversation_operations_are_not_requests() {
        for field in [22u8, 23, 35] {
            let mut bytes = vec![0x0a, 16];
            bytes.extend([1; 16]);
            bytes.extend([0x12, 16]);
            bytes.extend([2; 16]);
            bytes.extend([0x18, 1]);
            // Tag (field << 3 | 2) as a two-byte varint, then an empty body.
            let tag = (u16::from(field) << 3) | 2;
            bytes.extend([(tag as u8 & 0x7f) | 0x80, (tag >> 7) as u8, 0]);
            let decoded = decode_body(&bytes);
            assert!(
                decoded.is_err()
                    || validate_semantics(&decoded.unwrap(), Direction::ClientToServer).is_err(),
                "field {field}"
            );
        }
    }
    #[test]
    fn restoration_reads_are_bounded_and_directional() {
        let request = pb::ConversationHistory {
            project_id: Some(vec![3; 16]),
            conversation_id: None,
            before_accepted_frame: None,
            limit: Some(8),
        };
        let frame = envelope(Body::ConversationHistory(request.clone()));
        assert!(validate_semantics(&frame, Direction::ClientToServer).is_ok());
        assert!(validate_semantics(&frame, Direction::ServerToClient).is_err());
        assert_eq!(
            decode_body(&encode_frame(&frame).unwrap()[HEADER_BYTES..]).unwrap(),
            frame
        );
        for changed in [
            pb::ConversationHistory {
                limit: Some(9),
                ..request.clone()
            },
            pb::ConversationHistory {
                before_accepted_frame: Some(1),
                ..request.clone()
            },
            pb::ConversationHistory {
                project_id: Some(vec![3; 15]),
                ..request.clone()
            },
        ] {
            assert!(!valid(&Body::ConversationHistory(changed)));
        }
        let entry = pb::ConversationHistoryEntry {
            operation_id: Some(vec![7; 16]),
            generation: Some(2),
            accepted_frame: Some(123),
            kind: Some(3),
            person_command_input_id: None,
        };
        let reply = pb::ConversationHistoryReply {
            project_id: Some(vec![3; 16]),
            conversation_id: Some(vec![4; 16]),
            generation: Some(2),
            authority_revision: Some(8),
            has_more: Some(false),
            entries: vec![entry.clone()],
        };
        assert!(
            validate_semantics(
                &envelope(Body::ConversationHistoryReply(reply.clone())),
                Direction::ServerToClient
            )
            .is_ok()
        );
        assert!(!valid(&Body::ConversationHistoryReply(
            pb::ConversationHistoryReply {
                entries: vec![entry.clone(), entry],
                ..reply
            }
        )));
        let read = pb::ConversationReadPrompt {
            project_id: Some(vec![3; 16]),
            operation_id: Some(vec![7; 16]),
        };
        assert!(valid(&Body::ConversationReadPrompt(read)));
        assert!(!valid(&Body::ConversationPromptReply(
            pb::ConversationPromptReply {
                project_id: Some(vec![3; 16]),
                operation_id: Some(vec![7; 16]),
                conversation_id: Some(vec![4; 16]),
                generation: Some(2),
                prompt: Some("x".repeat(32769)),
            }
        )));
    }
    #[test]
    fn terminal_cursor_and_usage_are_explicit() {
        let event = pb::ConversationEvent {
            model_context: None,
            statistics: None,
            waiting_step: None,
            operation_id: Some(vec![1; 16]),
            cursor: Some(u64::MAX),
            generation: Some(1),
            kind: Some(3),
            text: Some("Complete".into()),
            reason: Some(0),
            usage_tokens: None,
            usage_known: Some(false),
            tools: Vec::new(),
            text_offset: Some(0),
            text_bytes: Some(8),
            thinking_text: None,
            thinking_offset: None,
            thinking_bytes: None,
            thinking_level: None,
            thinking_request: None,
            outlived_jobs: None,
            outlived_job_ids: Vec::new(),
        };
        assert!(valid(&Body::ConversationEvent(event.clone())));
        for changed in [
            pb::ConversationEvent {
                cursor: Some(3),
                ..event.clone()
            },
            pb::ConversationEvent {
                usage_known: None,
                ..event.clone()
            },
            pb::ConversationEvent {
                usage_tokens: Some(0),
                ..event.clone()
            },
            pb::ConversationEvent {
                usage_known: Some(true),
                usage_tokens: Some(crate::model::MAX_COUNTED_TOKENS + 1),
                ..event.clone()
            },
        ] {
            assert!(!valid(&Body::ConversationEvent(changed)));
        }
        // No output cap (owner, 2026-10-19): a count above the former 2,048 is valid.
        assert!(valid(&Body::ConversationEvent(pb::ConversationEvent {
            usage_known: Some(true),
            usage_tokens: Some(5_000),
            ..event
        })));
    }
    #[test]
    fn project_pages_and_error_exclusivity() {
        let project = pb::ProjectReply {
            project_id: Some(vec![1; 16]),
            location: Some("/tmp/project".into()),
            registry_revision: Some(1),
            current: Some(true),
            error: None,
            name: Some("Project".into()),
            name_revision: Some(0),
        };
        assert!(valid(&Body::ProjectsReply(pb::ProjectsReply {
            projects: vec![project.clone(); 8],
            next_cursor: None,
            error: None
        })));
        assert!(!valid(&Body::ProjectsReply(pb::ProjectsReply {
            projects: vec![project.clone(); 9],
            next_cursor: None,
            error: None
        })));
        assert!(!valid(&Body::ProjectReply(pb::ProjectReply {
            error: Some("busy".into()),
            ..project.clone()
        })));
    }
    /// SI-U5: only the overlap reason may accompany a listed registration.
    #[test]
    fn listed_projects_may_name_the_asura_state_overlap_only() {
        let project = pb::ProjectReply {
            project_id: Some(vec![1; 16]),
            location: Some("/Users/person".into()),
            registry_revision: Some(1),
            current: Some(false),
            error: Some("project_overlaps_asura_state".into()),
            name: Some("Person".into()),
            name_revision: Some(0),
        };
        let page = |project: pb::ProjectReply| {
            valid(&Body::ProjectsReply(pb::ProjectsReply {
                projects: vec![project],
                next_cursor: None,
                error: None,
            }))
        };
        assert!(page(project.clone()));
        assert!(!page(pb::ProjectReply {
            current: Some(true),
            ..project.clone()
        }));
        assert!(!page(pb::ProjectReply {
            error: Some("project_stale".into()),
            ..project.clone()
        }));
        assert!(!page(pb::ProjectReply {
            location: None,
            ..project.clone()
        }));
        assert!(!valid(&Body::ProjectReply(project)));
    }
    #[test]
    fn rename_has_bounded_identity_and_stale_projection() {
        let command = pb::ProjectRename {
            request_id: Some(vec![1; 16]),
            project_id: Some(vec![2; 16]),
            expected_name_revision: Some(0),
            name: Some("my project".into()),
        };
        assert!(valid(&Body::ProjectRename(command.clone())));
        assert!(!valid(&Body::ProjectRename(pb::ProjectRename {
            name: Some("x".repeat(129)),
            ..command.clone()
        })));
        assert!(!valid(&Body::ProjectRename(pb::ProjectRename {
            project_id: Some(vec![0; 16]),
            ..command
        })));
        let project = pb::ProjectReply {
            project_id: Some(vec![2; 16]),
            location: Some("/tmp/project".into()),
            registry_revision: Some(1),
            current: Some(true),
            name: Some("My project".into()),
            name_revision: Some(1),
            error: None,
        };
        assert!(valid(&Body::ProjectRenameReply(pb::ProjectRenameReply {
            project: Some(project.clone()),
            current_project: Some(project.clone()),
            changed: Some(true),
            error: None,
        })));
        assert!(valid(&Body::ProjectRenameReply(pb::ProjectRenameReply {
            project: Some(project.clone()),
            current_project: Some(project.clone()),
            changed: None,
            error: Some("stale_project_name_revision".into()),
        })));
        assert!(!valid(&Body::ProjectRenameReply(pb::ProjectRenameReply {
            project: Some(project.clone()),
            current_project: Some(project),
            changed: Some(true),
            error: Some("stale_project_name_revision".into()),
        })));
    }
    #[test]
    fn model_context_preserves_measurement_basis() {
        let mut event = pb::ConversationEvent {
            operation_id: Some(vec![1; 16]),
            cursor: Some(1),
            generation: Some(1),
            kind: Some(1),
            reason: Some(0),
            model_context: Some(pb::ModelContext {
                model_name: Some("Native model".into()),
                input_tokens: Some(123),
                capacity_tokens: Some(4096),
                basis: Some(1),
                reported_max_tokens: Some(4096),
                capacity_source: Some(1),
                reasoning: Some(true),
                thinking_controls: Some(3),
            }),
            ..Default::default()
        };
        assert!(valid(&Body::ConversationEvent(event.clone())));
        let context = event.model_context.as_mut().unwrap();
        context.input_tokens = None;
        context.basis = None;
        assert!(valid(&Body::ConversationEvent(event.clone())));
        let context = event.model_context.as_mut().unwrap();
        context.input_tokens = Some(123);
        context.basis = Some(1);
        event.model_context.as_mut().unwrap().basis = None;
        assert!(!valid(&Body::ConversationEvent(event.clone())));
        event.model_context.as_mut().unwrap().basis = Some(1);
        event.model_context.as_mut().unwrap().input_tokens = Some(4097);
        assert!(!valid(&Body::ConversationEvent(event.clone())));
        event.model_context.as_mut().unwrap().input_tokens = Some(123);
        event.model_context.as_mut().unwrap().reported_max_tokens = Some(100);
        assert!(!valid(&Body::ConversationEvent(event.clone())));
        event.model_context.as_mut().unwrap().reported_max_tokens = Some(4096);
        event.model_context.as_mut().unwrap().capacity_source = Some(9);
        assert!(!valid(&Body::ConversationEvent(event)));
    }
    #[test]
    fn held_queue_reason_is_bounded_and_only_for_held_state() {
        let mut reply = pb::ConversationQueueReply {
            entries: vec![pb::ConversationQueueEntry {
                input_id: Some(vec![3; 16]),
                project_id: Some(vec![4; 16]),
                conversation_id: Some(vec![5; 16]),
                target_generation: Some(1),
                kind: Some(1),
                state: Some(2),
                text: Some("prompt".into()),
                sequence: Some(1),
                new_conversation: Some(false),
                order_position: Some(1),
                hold_reason: Some("model_unavailable".into()),
                ..Default::default()
            }],
            full_text: Some(false),
            revision: Some(1),
            pending: Some(false),
            order_revision: Some(1),
            stale_order: Some(false),
            ..Default::default()
        };
        assert!(valid(&Body::ConversationQueueReply(reply.clone())));
        reply.entries[0].state = Some(1);
        assert!(!valid(&Body::ConversationQueueReply(reply.clone())));
        reply.entries[0].state = Some(2);
        reply.entries[0].hold_reason = Some("bad reason".into());
        assert!(!valid(&Body::ConversationQueueReply(reply)));
    }
    /// SJ-37: a project page names at most one cursor or exact ID.
    #[test]
    fn project_list_accepts_one_of_after_before_or_only() {
        let base = pb::ProjectList {
            limit: Some(8),
            ..Default::default()
        };
        assert!(valid(&Body::ProjectList(base.clone())));
        for (after, before, only) in [
            (Some(vec![1; 16]), None, None),
            (None, Some(vec![1; 16]), None),
            (None, None, Some(vec![1; 16])),
        ] {
            let page = pb::ProjectList {
                after_project_id: after,
                before_project_id: before,
                only_project_id: only,
                ..base.clone()
            };
            assert!(valid(&Body::ProjectList(page)));
        }
        for (after, before, only) in [
            (Some(vec![1; 16]), Some(vec![2; 16]), None),
            (Some(vec![1; 16]), None, Some(vec![2; 16])),
            (None, Some(vec![1; 16]), Some(vec![2; 16])),
            (None, Some(vec![0; 16]), None),
            (None, None, Some(vec![1; 15])),
        ] {
            let page = pb::ProjectList {
                after_project_id: after,
                before_project_id: before,
                only_project_id: only,
                ..base.clone()
            };
            assert!(!valid(&Body::ProjectList(page)));
        }
    }
    /// WS-02: a waiting step is live display state with exact placement.
    #[test]
    fn waiting_steps_are_bounded_to_pending_events_and_queued_entries() {
        let mut reply = pb::ConversationQueueReply {
            entries: vec![pb::ConversationQueueEntry {
                input_id: Some(vec![3; 16]),
                project_id: Some(vec![4; 16]),
                conversation_id: Some(vec![5; 16]),
                target_generation: Some(1),
                kind: Some(1),
                state: Some(1),
                text: Some("prompt".into()),
                sequence: Some(1),
                new_conversation: Some(false),
                order_position: Some(1),
                waiting_step: Some(2),
                ..Default::default()
            }],
            full_text: Some(false),
            revision: Some(1),
            pending: Some(false),
            order_revision: Some(1),
            stale_order: Some(false),
            ..Default::default()
        };
        for step in [1, 2] {
            reply.entries[0].waiting_step = Some(step);
            assert!(valid(&Body::ConversationQueueReply(reply.clone())));
        }
        for step in [0, 3, 4, 5] {
            reply.entries[0].waiting_step = Some(step);
            assert!(!valid(&Body::ConversationQueueReply(reply.clone())));
        }
        reply.entries[0].waiting_step = Some(1);
        reply.entries[0].state = Some(2);
        assert!(!valid(&Body::ConversationQueueReply(reply)));

        let mut event = pb::ConversationEvent {
            operation_id: Some(vec![1; 16]),
            generation: Some(1),
            cursor: Some(1),
            kind: Some(1),
            reason: Some(0),
            ..Default::default()
        };
        for step in [3, 4, 5] {
            event.waiting_step = Some(step);
            assert!(valid(&Body::ConversationEvent(event.clone())));
        }
        for step in [0, 1, 2, 6] {
            event.waiting_step = Some(step);
            assert!(!valid(&Body::ConversationEvent(event.clone())));
        }
        let terminal = pb::ConversationEvent {
            cursor: Some(u64::MAX),
            kind: Some(5),
            text: Some(String::new()),
            text_offset: Some(0),
            text_bytes: Some(0),
            usage_known: Some(false),
            waiting_step: Some(4),
            ..event
        };
        assert!(!valid(&Body::ConversationEvent(terminal.clone())));
        assert!(valid(&Body::ConversationEvent(pb::ConversationEvent {
            waiting_step: None,
            ..terminal
        })));
        let decision = |action| {
            Body::ConversationQueueDecision(pb::ConversationQueueDecision {
                request_id: Some(vec![1; 16]),
                input_id: Some(vec![2; 16]),
                action: Some(action),
                ..Default::default()
            })
        };
        assert!(valid(&decision(4)));
        assert!(!valid(&decision(5)));
    }
    #[test]
    fn maximum_activity_frame_retains_complete_response_and_signed_exit() {
        // RT-10: a full window with the largest offset and total varints.
        let event = pb::ConversationEvent {
            operation_id: Some(vec![1; 16]),
            generation: Some(1),
            cursor: Some(u64::MAX),
            kind: Some(3),
            reason: Some(0),
            // TL-12: the window split between thinking and response, so both
            // strings carry three-byte lengths, with the largest offsets.
            text: Some("a".repeat(MAX_RESPONSE_WINDOW_BYTES / 2)),
            text_offset: Some(u64::MAX - MAX_RESPONSE_WINDOW_BYTES as u64),
            text_bytes: Some(u64::MAX),
            thinking_text: Some("t".repeat(MAX_RESPONSE_WINDOW_BYTES / 2)),
            thinking_offset: Some(u64::MAX - MAX_RESPONSE_WINDOW_BYTES as u64),
            thinking_bytes: Some(u64::MAX),
            thinking_level: Some(4),
            thinking_request: Some(5),
            usage_known: Some(false),
            // The most recent window, ending at the over-limit ordinal (TCR-D31).
            tools: (crate::model::MAX_TOOL_ORDINAL + 1 - MAX_TOOL_PROGRESS_ENTRIES as u32
                ..=crate::model::MAX_TOOL_ORDINAL)
                .map(|ordinal| pb::ToolProgress {
                    ordinal: Some(ordinal),
                    name: Some("a".repeat(64)),
                    fault: Some(16),
                    state: Some(2),
                    status: Some(1),
                    arguments: Some("a".repeat(192)),
                    preview: Some("a".repeat(128)),
                    policy_decision: Some(1),
                    command_filtered: Some(true),
                    elapsed_ms: Some(u64::MAX),
                    result_bytes: Some(65536),
                    truncated: Some(true),
                    exit_code: Some(i32::MIN),
                    // J4T-09: the job an admitted call names.
                    job_id: Some(vec![0xff; 16]),
                })
                .collect(),
            // EX12: the largest outlived count and its 8 IDs.
            outlived_jobs: Some(u32::MAX),
            outlived_job_ids: (1..=8u8).map(|n| vec![n; 16]).collect(),
            statistics: Some(pb::TurnStatistics {
                elapsed_ms: Some(u64::MAX),
                // No request count or token total (owner, 2026-10-18).
                request_count: Some(u32::MAX),
                input_tokens: Some(u64::MAX),
                output_tokens: Some(u64::MAX),
                cached_input_tokens: Some(u64::MAX),
                complete: Some(true),
            }),
            ..Default::default()
        };
        let message = envelope(Body::ConversationEvent(event));
        assert!(valid(message.body.as_ref().unwrap()));
        // RT-10 and TCR-D26: the worst case with the J4 fields is 65,322
        // bytes at the 60,928-byte window, within the 65,536-byte frame.
        assert_eq!(MAX_RESPONSE_WINDOW_BYTES, 60_928);
        assert!(prost::Message::encoded_len(&message) <= 65_322);
        let frame = crate::encode_frame(&message).unwrap();
        assert!(frame.len() <= crate::MAX_FRAME_BYTES + crate::HEADER_BYTES);
        // A window one byte over the bound is refused.
        let mut over = message.clone();
        if let Some(Body::ConversationEvent(v)) = over.body.as_mut() {
            v.text = Some("a".repeat(MAX_RESPONSE_WINDOW_BYTES / 2 + 1));
        }
        assert!(!valid(over.body.as_ref().unwrap()));
        assert_eq!(
            crate::decode_body(&frame[crate::HEADER_BYTES..]).unwrap(),
            message
        );
        let mut invalid = message.clone();
        if let Some(Body::ConversationEvent(v)) = invalid.body.as_mut() {
            v.tools[0].arguments = Some("a".repeat(193));
        }
        assert!(!valid(invalid.body.as_ref().unwrap()));
        // SL7: the filtered mark is only `true`, only on an admitted call.
        for (decision, filtered) in [
            (Some(1), Some(false)),
            (Some(4), Some(true)),
            (None, Some(true)),
        ] {
            let mut invalid = message.clone();
            if let Some(Body::ConversationEvent(v)) = invalid.body.as_mut() {
                v.tools[0].policy_decision = decision;
                v.tools[0].command_filtered = filtered;
            }
            assert!(!valid(invalid.body.as_ref().unwrap()), "{decision:?}");
        }
        // TCR-U19: more entries, a gap, a first ordinal of 0 or an ordinal past
        // the over-limit ordinal are rejected; a window starting at 1 passes.
        let tools = |event: &pb::Envelope| match event.body.as_ref() {
            Some(Body::ConversationEvent(v)) => v.tools.clone(),
            _ => unreachable!(),
        };
        let with = |tools: Vec<pb::ToolProgress>| {
            let mut changed = message.clone();
            if let Some(Body::ConversationEvent(v)) = changed.body.as_mut() {
                v.tools = tools;
            }
            valid(changed.body.as_ref().unwrap())
        };
        let window = tools(&message);
        let renumber = |first: u32, count: usize| {
            (0..count)
                .map(|i| pb::ToolProgress {
                    ordinal: Some(first + i as u32),
                    ..window[0].clone()
                })
                .collect::<Vec<_>>()
        };
        assert!(with(renumber(1, MAX_TOOL_PROGRESS_ENTRIES)));
        assert!(with(renumber(150, 3)));
        assert!(with(Vec::new()));
        assert!(!with(renumber(1, MAX_TOOL_PROGRESS_ENTRIES + 1)));
        assert!(!with(renumber(0, 2)));
        assert!(!with(renumber(crate::model::MAX_TOOL_ORDINAL, 2)));
        let mut gap = renumber(5, 3);
        gap[2].ordinal = Some(8);
        assert!(!with(gap));
    }
    /// RT-03 and RT-04 (wire): a window keeps `text_offset` bytes and appends
    /// at most 61,440; no response total exists.
    #[test]
    fn response_windows_are_bounded_per_event_not_per_response() {
        let event = pb::ConversationEvent {
            operation_id: Some(vec![1; 16]),
            generation: Some(1),
            cursor: Some(5),
            kind: Some(2),
            reason: Some(0),
            text: Some("b".repeat(MAX_RESPONSE_WINDOW_BYTES)),
            text_offset: Some(4 * 1024 * 1024),
            text_bytes: Some(10 * 1024 * 1024),
            ..Default::default()
        };
        assert!(valid(&Body::ConversationEvent(event.clone())));
        for changed in [
            pb::ConversationEvent {
                text: Some("b".repeat(MAX_RESPONSE_WINDOW_BYTES + 1)),
                ..event.clone()
            },
            pb::ConversationEvent {
                text_offset: None,
                ..event.clone()
            },
            pb::ConversationEvent {
                text_bytes: Some(4 * 1024 * 1024 + 10),
                ..event.clone()
            },
            pb::ConversationEvent {
                text_offset: Some(u64::MAX),
                text_bytes: Some(u64::MAX),
                ..event.clone()
            },
            // A pending event carries no window.
            pb::ConversationEvent {
                kind: Some(1),
                ..event.clone()
            },
            // Complete needs a nonempty response, not a nonempty window.
            pb::ConversationEvent {
                kind: Some(3),
                cursor: Some(u64::MAX),
                text: Some(String::new()),
                text_offset: Some(0),
                text_bytes: Some(0),
                usage_known: Some(false),
                ..event.clone()
            },
        ] {
            assert!(!valid(&Body::ConversationEvent(changed)));
        }
        assert!(valid(&Body::ConversationEvent(pb::ConversationEvent {
            kind: Some(3),
            cursor: Some(u64::MAX),
            text: Some(String::new()),
            text_offset: Some(70_000),
            text_bytes: Some(70_000),
            usage_known: Some(false),
            ..event
        })));
        let observe = pb::ConversationObserve {
            operation_id: Some(vec![1; 16]),
            after_cursor: Some(u64::MAX),
            wait_ms: Some(0),
            text_offset: Some(9 * 1024 * 1024),
            thinking_offset: Some(70_000),
        };
        assert!(valid(&Body::ConversationObserve(observe)));
    }
    /// TL-12 (wire): the thinking window comes first and shares the
    /// 61,440-byte budget with the response window; its three fields come
    /// together; a pending event carries none.
    #[test]
    fn thinking_windows_share_the_event_budget() {
        let event = pb::ConversationEvent {
            operation_id: Some(vec![1; 16]),
            generation: Some(1),
            cursor: Some(5),
            kind: Some(2),
            reason: Some(0),
            text: Some("b".repeat(1_000)),
            text_offset: Some(0),
            text_bytes: Some(1_000),
            thinking_text: Some("t".repeat(MAX_RESPONSE_WINDOW_BYTES - 1_000)),
            thinking_offset: Some(200_000),
            thinking_bytes: Some(300_000),
            thinking_level: Some(4),
            thinking_request: Some(2),
            ..Default::default()
        };
        assert!(valid(&Body::ConversationEvent(event.clone())));
        // A whole budget of thinking leaves an empty response window.
        assert!(valid(&Body::ConversationEvent(pb::ConversationEvent {
            text: Some(String::new()),
            text_offset: Some(1_000),
            thinking_text: Some("t".repeat(MAX_RESPONSE_WINDOW_BYTES)),
            ..event.clone()
        })));
        for changed in [
            pb::ConversationEvent {
                text: Some("b".repeat(1_001)),
                text_bytes: Some(1_001),
                ..event.clone()
            },
            pb::ConversationEvent {
                thinking_offset: None,
                ..event.clone()
            },
            pb::ConversationEvent {
                thinking_bytes: None,
                ..event.clone()
            },
            pb::ConversationEvent {
                thinking_bytes: Some(200_001),
                ..event.clone()
            },
            pb::ConversationEvent {
                kind: Some(1),
                text: None,
                text_offset: None,
                text_bytes: None,
                ..event.clone()
            },
        ] {
            assert!(!valid(&Body::ConversationEvent(changed)));
        }
        // Thinking controls need reasoning.
        let context = |reasoning, controls| pb::ConversationEvent {
            kind: Some(1),
            cursor: Some(1),
            text: None,
            text_offset: None,
            text_bytes: None,
            thinking_text: None,
            thinking_offset: None,
            thinking_bytes: None,
            model_context: Some(pb::ModelContext {
                model_name: Some("ollama:qwen3".into()),
                reasoning,
                thinking_controls: controls,
                ..Default::default()
            }),
            ..event.clone()
        };
        assert!(valid(&Body::ConversationEvent(context(
            Some(false),
            Some(0)
        ))));
        assert!(valid(&Body::ConversationEvent(context(
            Some(true),
            Some(31)
        ))));
        assert!(!valid(&Body::ConversationEvent(context(
            Some(false),
            Some(3)
        ))));
        assert!(!valid(&Body::ConversationEvent(context(
            Some(true),
            Some(32)
        ))));
    }

    fn launch(name: &[u8], value: &[u8]) -> pb::LaunchVariable {
        pb::LaunchVariable {
            name: Some(name.to_vec()),
            value: Some(value.to_vec()),
        }
    }
    fn command_submit() -> pb::ConversationQueueCommandSubmit {
        pb::ConversationQueueCommandSubmit {
            request_id: Some(vec![3; 16]),
            project_id: Some(vec![4; 16]),
            conversation_id: Some(vec![5; 16]),
            expected_generation: Some(2),
            new_conversation: Some(false),
            command: Some("ls -l".into()),
            cwd: Some(".".into()),
            search_path: Some("/usr/bin:/bin".into()),
            environment: vec![pb::PersonEnvironment {
                name: Some("LANG".into()),
                value: Some("en_GB.UTF-8".into()),
            }],
            outside_sandbox: None,
            launch_environment: Vec::new(),
        }
    }
    fn outside_submit(
        launch_environment: Vec<pb::LaunchVariable>,
    ) -> pb::ConversationQueueCommandSubmit {
        pb::ConversationQueueCommandSubmit {
            search_path: None,
            environment: Vec::new(),
            outside_sandbox: Some(true),
            launch_environment,
            ..command_submit()
        }
    }
    /// Names `V0000`..; values fill `bytes` names and values together.
    fn sized_launch(entries: usize, bytes: usize) -> Vec<pb::LaunchVariable> {
        let names = entries * 5;
        let mut values = vec![0usize; entries];
        let rest = bytes - names;
        // Two large values carry 3-byte lengths; the rest are empty.
        values[entries - 1] = rest / 2;
        values[entries - 2] = rest - rest / 2;
        (0..entries)
            .map(|i| launch(format!("V{i:04}").as_bytes(), &vec![b'x'; values[i]]))
            .collect()
    }
    fn round_trips(body: Body) -> bool {
        let value = envelope(body);
        let Ok(frame) = encode_frame(&value) else {
            return false;
        };
        decode_body(&frame[HEADER_BYTES..]).is_ok_and(|decoded| decoded == value)
    }

    /// PC-U41: the submit codec's environment rules. Each invalid form is a
    /// typed refusal, not a protocol fault: the submit stays well-shaped.
    #[test]
    fn pc_u41_submit_environment_refusals() {
        use pb::QueueRefusal::{EnvironmentInvalid as Invalid, EnvironmentTooLarge as TooLarge};
        use person::environment_refusal as refusal;
        let sandboxed = command_submit();
        assert!(person::submit_shape(&sandboxed));
        assert_eq!(refusal(&sandboxed), None);
        let outside = outside_submit(vec![launch(b"HOME", b"/Users/p"), launch(b"EMPTY", b"")]);
        assert_eq!(refusal(&outside), None);
        let mut cases: Vec<(&str, pb::ConversationQueueCommandSubmit, pb::QueueRefusal)> = vec![
            (
                "outside with field 8",
                pb::ConversationQueueCommandSubmit {
                    search_path: Some("/bin".into()),
                    ..outside.clone()
                },
                Invalid,
            ),
            (
                "outside with field 9",
                pb::ConversationQueueCommandSubmit {
                    environment: sandboxed.environment.clone(),
                    ..outside.clone()
                },
                Invalid,
            ),
            (
                "sandboxed with a launch environment",
                pb::ConversationQueueCommandSubmit {
                    launch_environment: vec![launch(b"A", b"1")],
                    ..sandboxed.clone()
                },
                Invalid,
            ),
            (
                "false flag with a launch environment",
                pb::ConversationQueueCommandSubmit {
                    outside_sandbox: Some(false),
                    launch_environment: vec![launch(b"A", b"1")],
                    ..sandboxed.clone()
                },
                Invalid,
            ),
            (
                "duplicate names",
                outside_submit(vec![launch(b"A", b"1"), launch(b"A", b"2")]),
                Invalid,
            ),
            (
                "name with =",
                outside_submit(vec![launch(b"A=B", b"1")]),
                Invalid,
            ),
            (
                "empty name",
                outside_submit(vec![launch(b"", b"1")]),
                Invalid,
            ),
            (
                "NUL name",
                outside_submit(vec![launch(b"A\0", b"1")]),
                Invalid,
            ),
            (
                "NUL value",
                outside_submit(vec![launch(b"A", b"x\0y")]),
                Invalid,
            ),
            (
                "ASURA_SECRET_1",
                outside_submit(vec![launch(b"ASURA_SECRET_1", b"v")]),
                Invalid,
            ),
            (
                "ASURA_SECRET_COMMAND",
                outside_submit(vec![launch(b"ASURA_SECRET_COMMAND", b"v")]),
                Invalid,
            ),
            (
                "ASURA_JOB_x",
                outside_submit(vec![launch(b"ASURA_JOB_x", b"1")]),
                Invalid,
            ),
            (
                "absent value",
                outside_submit(vec![pb::LaunchVariable {
                    name: Some(b"A".to_vec()),
                    value: None,
                }]),
                Invalid,
            ),
            (
                "1,025-byte name",
                outside_submit(vec![launch(&[b'N'; 1025], b"")]),
                Invalid,
            ),
            (
                "1,025 entries",
                outside_submit(sized_launch(1025, 40_960)),
                TooLarge,
            ),
            (
                "40,961 bytes",
                outside_submit(sized_launch(1024, 40_961)),
                TooLarge,
            ),
        ];
        // `!` (PC3) forms: only listed names, once each, with checked values.
        for environment in [
            vec![("HOME", "/Users/p")],
            vec![("LANG", "C"), ("LANG", "C")],
            vec![("LANG", "bad value")],
            vec![("LANG", "")],
            vec![("LC_ALL", &"a".repeat(65) as &str)],
            vec![("SSH_AUTH_SOCK", "relative/agent")],
            vec![("SSH_AUTH_SOCK", &format!("/{}", "s".repeat(103)) as &str)],
            vec![("ASURA_SECRET_1", "v")],
        ] {
            cases.push((
                "PC3",
                pb::ConversationQueueCommandSubmit {
                    environment: environment
                        .into_iter()
                        .map(|(name, value)| pb::PersonEnvironment {
                            name: Some(name.into()),
                            value: Some(value.into()),
                        })
                        .collect(),
                    ..sandboxed.clone()
                },
                Invalid,
            ));
        }
        for (name, case, expected) in cases {
            assert!(person::submit_shape(&case), "{name}: shape");
            assert_eq!(refusal(&case), Some(expected), "{name}");
            // A refused form still crosses the wire, so the service can
            // answer it with its typed refusal.
            assert!(
                round_trips(Body::ConversationQueueCommandSubmit(case)),
                "{name}: wire"
            );
        }
        // Accepted bounds: a non-UTF-8 value, a 1,024-byte name, the
        // largest SSH_AUTH_SOCK, every PC3 name once.
        let accepted = outside_submit(vec![
            launch(b"BYTES", &[0xff, 0xfe, b'a']),
            launch(&[b'N'; 1024], b""),
        ]);
        assert_eq!(refusal(&accepted), None);
        assert!(round_trips(Body::ConversationQueueCommandSubmit(accepted)));
        let every = pb::ConversationQueueCommandSubmit {
            environment: person::CLIENT_VARIABLES
                .iter()
                .map(|name| pb::PersonEnvironment {
                    name: Some((*name).into()),
                    value: Some(if *name == "SSH_AUTH_SOCK" {
                        format!("/{}", "s".repeat(102))
                    } else {
                        "en_GB.UTF-8@euro".into()
                    }),
                })
                .collect(),
            ..sandboxed.clone()
        };
        assert_eq!(refusal(&every), None);
        assert!(round_trips(Body::ConversationQueueCommandSubmit(every)));
    }

    /// The submit's protocol shape: presence, lane target, sizes, direction.
    #[test]
    fn command_submit_shape_and_direction() {
        let request = command_submit();
        let value = envelope(Body::ConversationQueueCommandSubmit(request.clone()));
        assert!(validate_semantics(&value, Direction::ClientToServer).is_ok());
        assert_eq!(
            validate_semantics(&value, Direction::ServerToClient),
            Err(crate::ProtocolError::WrongDirection)
        );
        let fresh = pb::ConversationQueueCommandSubmit {
            conversation_id: None,
            expected_generation: Some(0),
            new_conversation: Some(true),
            ..request.clone()
        };
        assert!(person::submit_shape(&fresh));
        for (name, changed) in [
            (
                "request",
                pb::ConversationQueueCommandSubmit {
                    request_id: Some(vec![0; 16]),
                    ..request.clone()
                },
            ),
            (
                "project",
                pb::ConversationQueueCommandSubmit {
                    project_id: None,
                    ..request.clone()
                },
            ),
            (
                "generation",
                pb::ConversationQueueCommandSubmit {
                    expected_generation: None,
                    ..request.clone()
                },
            ),
            (
                "new with id",
                pb::ConversationQueueCommandSubmit {
                    new_conversation: Some(true),
                    ..request.clone()
                },
            ),
            (
                "no command",
                pb::ConversationQueueCommandSubmit {
                    command: None,
                    ..request.clone()
                },
            ),
            (
                "empty command",
                pb::ConversationQueueCommandSubmit {
                    command: Some(String::new()),
                    ..request.clone()
                },
            ),
            (
                "long command",
                pb::ConversationQueueCommandSubmit {
                    command: Some("x".repeat(8193)),
                    ..request.clone()
                },
            ),
            (
                "NUL command",
                pb::ConversationQueueCommandSubmit {
                    command: Some("a\0b".into()),
                    ..request.clone()
                },
            ),
            (
                "no cwd",
                pb::ConversationQueueCommandSubmit {
                    cwd: None,
                    ..request.clone()
                },
            ),
            (
                "long cwd",
                pb::ConversationQueueCommandSubmit {
                    cwd: Some("d".repeat(1025)),
                    ..request.clone()
                },
            ),
            (
                "long PATH",
                pb::ConversationQueueCommandSubmit {
                    search_path: Some("p".repeat(4097)),
                    ..request.clone()
                },
            ),
        ] {
            assert!(!person::submit_shape(&changed), "{name}");
            assert!(
                !valid(&Body::ConversationQueueCommandSubmit(changed)),
                "{name}"
            );
        }
        assert!(person::submit_shape(&pb::ConversationQueueCommandSubmit {
            command: Some("x".repeat(8192)),
            cwd: Some("d".repeat(1024)),
            search_path: Some("p".repeat(4096)),
            ..request.clone()
        }));
        // Ten PC3 entries exceed the wire bound of nine.
        let ten = pb::ConversationQueueCommandSubmit {
            environment: vec![request.environment[0].clone(); 10],
            ..request
        };
        assert!(encode_frame(&envelope(Body::ConversationQueueCommandSubmit(ten))).is_err());
        assert!(person::cwd_form_valid("."));
        assert!(person::cwd_form_valid("src/ui"));
        for cwd in ["", "/abs", "a/../b", "..", "a\0b"] {
            assert!(!person::cwd_form_valid(cwd), "{cwd:?}");
        }
        assert!(!person::cwd_form_valid(&"d".repeat(1025)));
    }

    /// PC-U49: a submit at every maximum together fits one control frame,
    /// and the over-bound environments still fit, to be refused.
    #[test]
    fn pc_u49_submit_at_every_maximum_fits_one_frame() {
        // The frame-bound table: fixed 9,343 bytes, then 1,024 entries of at
        // most 11 header bytes and 40,960 name and value bytes.
        let bound =
            9_343 + person::MAX_LAUNCH_VARIABLES * 11 + person::MAX_LAUNCH_ENVIRONMENT_BYTES;
        assert!(bound <= crate::MAX_FRAME_BYTES);
        let launch_environment = sized_launch(1024, 40_960);
        // Largest value lengths take 3-byte varints.
        assert!(
            launch_environment
                .iter()
                .any(|e| e.value.as_ref().unwrap().len() >= 16_384)
        );
        let submit = pb::ConversationQueueCommandSubmit {
            request_id: Some(vec![0xff; 16]),
            project_id: Some(vec![0xff; 16]),
            conversation_id: Some(vec![0xff; 16]),
            expected_generation: Some(u64::MAX - 1),
            new_conversation: Some(false),
            command: Some("c".repeat(8_192)),
            cwd: Some("d".repeat(1_024)),
            ..outside_submit(launch_environment)
        };
        assert_eq!(person::environment_refusal(&submit), None);
        let value = pb::Envelope {
            service_epoch: Some(vec![0xff; 16]),
            attachment_id: Some(vec![0xff; 16]),
            request_counter: Some(u64::MAX),
            body: Some(Body::ConversationQueueCommandSubmit(submit.clone())),
        };
        use prost::Message;
        let length = value.encoded_len();
        assert!(
            length <= bound && length <= crate::MAX_FRAME_BYTES,
            "{length}"
        );
        assert!(validate_semantics(&value, Direction::ClientToServer).is_ok());
        let frame = encode_frame(&value).unwrap();
        assert_eq!(decode_body(&frame[HEADER_BYTES..]).unwrap(), value);
        for over in [sized_launch(1025, 40_960), sized_launch(1024, 40_961)] {
            let over = pb::ConversationQueueCommandSubmit {
                launch_environment: over,
                ..submit.clone()
            };
            assert_eq!(
                person::environment_refusal(&over),
                Some(pb::QueueRefusal::EnvironmentTooLarge)
            );
            let value = pb::Envelope {
                body: Some(Body::ConversationQueueCommandSubmit(over)),
                ..value.clone()
            };
            assert!(value.encoded_len() <= crate::MAX_FRAME_BYTES);
            assert!(encode_frame(&value).is_ok());
        }
    }

    fn command_entry() -> pb::ConversationQueueEntry {
        pb::ConversationQueueEntry {
            input_id: Some(vec![3; 16]),
            project_id: Some(vec![4; 16]),
            conversation_id: Some(vec![5; 16]),
            target_generation: Some(2),
            kind: Some(person::ENTRY_KIND),
            state: Some(1),
            text: Some("make test".into()),
            sequence: Some(1),
            new_conversation: Some(false),
            order_position: Some(1),
            command_cwd: Some(".".into()),
            command_outside_sandbox: Some(true),
            waiting_reason: Some(pb::QueueWaitReason::ModelTurn as i32),
            waiting_text: Some("waiting: model turn running".into()),
            ..Default::default()
        }
    }
    fn entry_reply(entry: pb::ConversationQueueEntry) -> pb::ConversationQueueReply {
        pb::ConversationQueueReply {
            entries: vec![entry],
            full_text: Some(false),
            revision: Some(1),
            pending: Some(false),
            order_revision: Some(1),
            stale_order: Some(false),
            ..Default::default()
        }
    }
    fn refusal_reply(refusal: pb::QueueRefusal) -> pb::ConversationQueueReply {
        pb::ConversationQueueReply {
            request_id: Some(vec![6; 16]),
            full_text: Some(false),
            refusal: Some(refusal as i32),
            refusal_status: (refusal == pb::QueueRefusal::SecretStoreUnavailable).then_some(25_308),
            ..Default::default()
        }
    }

    /// PC-U55: queue entry fields 16-20 and the refusal reply.
    #[test]
    fn pc_u55_queue_command_entry_and_refusal_codec() {
        let entry = command_entry();
        assert!(valid(&Body::ConversationQueueReply(entry_reply(
            entry.clone()
        ))));
        let reject = |name: &str, entry: pb::ConversationQueueEntry| {
            assert!(
                !valid(&Body::ConversationQueueReply(entry_reply(entry))),
                "{name}"
            );
        };
        // Fields 16-20 on kind 3 only.
        let message = pb::ConversationQueueEntry {
            kind: Some(1),
            command_cwd: None,
            command_outside_sandbox: None,
            waiting_reason: None,
            waiting_text: None,
            ..entry.clone()
        };
        assert!(valid(&Body::ConversationQueueReply(entry_reply(
            message.clone()
        ))));
        reject(
            "cwd on kind 1",
            pb::ConversationQueueEntry {
                command_cwd: Some(".".into()),
                ..message.clone()
            },
        );
        reject(
            "flag on kind 1",
            pb::ConversationQueueEntry {
                command_outside_sandbox: Some(false),
                ..message.clone()
            },
        );
        reject(
            "reason on kind 1",
            pb::ConversationQueueEntry {
                waiting_reason: entry.waiting_reason,
                waiting_text: entry.waiting_text.clone(),
                ..message.clone()
            },
        );
        reject(
            "filtered on kind 1",
            pb::ConversationQueueEntry {
                command_filtered: Some(true),
                ..message.clone()
            },
        );
        reject(
            "filtered on kind 2",
            pb::ConversationQueueEntry {
                kind: Some(2),
                target_operation_id: Some(vec![8; 16]),
                command_filtered: Some(true),
                ..message
            },
        );
        // Waiting reason and text only together, only in state 1.
        reject(
            "reason without text",
            pb::ConversationQueueEntry {
                waiting_text: None,
                ..entry.clone()
            },
        );
        reject(
            "text without reason",
            pb::ConversationQueueEntry {
                waiting_reason: None,
                ..entry.clone()
            },
        );
        for state in 2..=6 {
            reject(
                "outside state 1",
                pb::ConversationQueueEntry {
                    state: Some(state),
                    ..entry.clone()
                },
            );
        }
        reject(
            "reason 0",
            pb::ConversationQueueEntry {
                waiting_reason: Some(0),
                ..entry.clone()
            },
        );
        reject(
            "empty text",
            pb::ConversationQueueEntry {
                waiting_text: Some(String::new()),
                ..entry.clone()
            },
        );
        reject(
            "long text",
            pb::ConversationQueueEntry {
                waiting_text: Some("w".repeat(129)),
                ..entry.clone()
            },
        );
        reject(
            "control text",
            pb::ConversationQueueEntry {
                waiting_text: Some("a\nb".into()),
                ..entry.clone()
            },
        );
        reject(
            "model wait step",
            pb::ConversationQueueEntry {
                waiting_reason: None,
                waiting_text: None,
                waiting_step: Some(1),
                ..entry.clone()
            },
        );
        reject(
            "no cwd",
            pb::ConversationQueueEntry {
                command_cwd: None,
                ..entry.clone()
            },
        );
        reject(
            "long cwd",
            pb::ConversationQueueEntry {
                command_cwd: Some("d".repeat(1025)),
                ..entry.clone()
            },
        );
        reject(
            "no outside flag",
            pb::ConversationQueueEntry {
                command_outside_sandbox: None,
                ..entry.clone()
            },
        );
        reject(
            "filtered false",
            pb::ConversationQueueEntry {
                command_filtered: Some(false),
                ..entry.clone()
            },
        );
        reject(
            "operation",
            pb::ConversationQueueEntry {
                state: Some(3),
                waiting_reason: None,
                waiting_text: None,
                operation_id: Some(vec![9; 16]),
                generation: Some(2),
                ..entry.clone()
            },
        );
        // Accepted variants: running with no reason, a held reason, filtered.
        for accepted in [
            pb::ConversationQueueEntry {
                state: Some(3),
                waiting_reason: None,
                waiting_text: None,
                ..entry.clone()
            },
            pb::ConversationQueueEntry {
                state: Some(2),
                waiting_reason: None,
                waiting_text: None,
                hold_reason: Some("secrets_lost".into()),
                ..entry.clone()
            },
            pb::ConversationQueueEntry {
                command_filtered: Some(true),
                ..entry.clone()
            },
            pb::ConversationQueueEntry {
                conversation_id: Some(vec![5; 16]),
                target_generation: Some(0),
                new_conversation: Some(true),
                ..entry.clone()
            },
        ] {
            assert!(valid(&Body::ConversationQueueReply(entry_reply(accepted))));
        }
        // Every wait reason round-trips; one past the last is not on the wire.
        for reason in 1..=person::MAX_WAIT_REASON {
            assert!(pb::QueueWaitReason::try_from(reason).is_ok());
            let reply = entry_reply(pb::ConversationQueueEntry {
                waiting_reason: Some(reason),
                ..entry.clone()
            });
            assert!(
                round_trips(Body::ConversationQueueReply(reply)),
                "reason {reason}"
            );
        }
        assert!(!round_trips(Body::ConversationQueueReply(entry_reply(
            pb::ConversationQueueEntry {
                waiting_reason: Some(person::MAX_WAIT_REASON + 1),
                ..entry.clone()
            }
        ))));
        // Every refusal round-trips; the status only with unavailable.
        for code in 1..=person::MAX_REFUSAL {
            let refusal = pb::QueueRefusal::try_from(code).unwrap();
            let reply = refusal_reply(refusal);
            assert!(
                valid(&Body::ConversationQueueReply(reply.clone())),
                "{refusal:?}"
            );
            assert!(
                round_trips(Body::ConversationQueueReply(reply.clone())),
                "{refusal:?}"
            );
            let flipped = pb::ConversationQueueReply {
                refusal_status: if reply.refusal_status.is_some() {
                    None
                } else {
                    Some(1)
                },
                ..reply
            };
            assert!(
                !valid(&Body::ConversationQueueReply(flipped)),
                "{refusal:?} status"
            );
        }
        let mut wire = refusal_reply(pb::QueueRefusal::OutsideSandboxOff);
        wire.refusal = Some(person::MAX_REFUSAL + 1);
        assert!(!round_trips(Body::ConversationQueueReply(wire)));
        let refused = refusal_reply(pb::QueueRefusal::EnvironmentInvalid);
        let accepted = entry_reply(entry.clone());
        for (name, reply) in [
            (
                "refusal and receipt",
                pb::ConversationQueueReply {
                    accepted_input_id: entry.input_id.clone(),
                    entries: accepted.entries.clone(),
                    order_revision: Some(1),
                    stale_order: Some(false),
                    ..refused.clone()
                },
            ),
            (
                "refusal and entries",
                pb::ConversationQueueReply {
                    entries: accepted.entries.clone(),
                    ..refused.clone()
                },
            ),
            (
                "refusal 0",
                pb::ConversationQueueReply {
                    refusal: Some(0),
                    ..refused.clone()
                },
            ),
            (
                "no request",
                pb::ConversationQueueReply {
                    request_id: None,
                    ..refused.clone()
                },
            ),
            (
                "status alone",
                pb::ConversationQueueReply {
                    refusal_status: Some(1),
                    ..accepted.clone()
                },
            ),
            (
                "zero status",
                pb::ConversationQueueReply {
                    refusal_status: Some(0),
                    ..refusal_reply(pb::QueueRefusal::SecretStoreUnavailable)
                },
            ),
        ] {
            assert!(!valid(&Body::ConversationQueueReply(reply)), "{name}");
        }
        assert!(round_trips(Body::ConversationQueueReply(
            pb::ConversationQueueReply {
                entries: vec![pb::ConversationQueueEntry {
                    command_filtered: Some(true),
                    ..entry
                }],
                ..accepted
            }
        )));
    }

    /// History field 5: a person command entry names its input, not an
    /// operation, and is ordered with turns by its accepted frame.
    #[test]
    fn history_person_command_entry() {
        let turn = pb::ConversationHistoryEntry {
            operation_id: Some(vec![7; 16]),
            generation: Some(2),
            accepted_frame: Some(200),
            kind: Some(3),
            person_command_input_id: None,
        };
        let command = pb::ConversationHistoryEntry {
            operation_id: None,
            generation: Some(2),
            accepted_frame: Some(150),
            kind: None,
            person_command_input_id: Some(vec![8; 16]),
        };
        let reply = |entries| pb::ConversationHistoryReply {
            project_id: Some(vec![3; 16]),
            conversation_id: Some(vec![4; 16]),
            generation: Some(2),
            authority_revision: Some(9),
            has_more: Some(false),
            entries,
        };
        let body = Body::ConversationHistoryReply(reply(vec![turn.clone(), command.clone()]));
        assert!(valid(&body));
        assert!(round_trips(body));
        // Queued against generation 0, before the first turn.
        assert!(valid(&Body::ConversationHistoryReply(reply(vec![
            pb::ConversationHistoryEntry {
                generation: Some(0),
                ..command.clone()
            }
        ]))));
        // A conversation with only person commands has generation 0; a turn
        // entry needs a generation above 0.
        let only_commands = |entries| pb::ConversationHistoryReply {
            generation: Some(0),
            ..reply(entries)
        };
        let first = pb::ConversationHistoryEntry {
            generation: Some(0),
            ..command.clone()
        };
        assert!(valid(&Body::ConversationHistoryReply(only_commands(vec![
            first.clone()
        ]))));
        assert!(!valid(&Body::ConversationHistoryReply(only_commands(
            vec![turn.clone(), first]
        ))));
        for (name, changed) in [
            (
                "operation",
                pb::ConversationHistoryEntry {
                    operation_id: Some(vec![7; 16]),
                    ..command.clone()
                },
            ),
            (
                "kind",
                pb::ConversationHistoryEntry {
                    kind: Some(3),
                    ..command.clone()
                },
            ),
            (
                "zero input",
                pb::ConversationHistoryEntry {
                    person_command_input_id: Some(vec![0; 16]),
                    ..command.clone()
                },
            ),
            (
                "short input",
                pb::ConversationHistoryEntry {
                    person_command_input_id: Some(vec![8; 15]),
                    ..command.clone()
                },
            ),
            (
                "later generation",
                pb::ConversationHistoryEntry {
                    generation: Some(3),
                    ..command.clone()
                },
            ),
            (
                "no frame",
                pb::ConversationHistoryEntry {
                    accepted_frame: None,
                    ..command.clone()
                },
            ),
        ] {
            assert!(
                !valid(&Body::ConversationHistoryReply(reply(vec![changed]))),
                "{name}"
            );
        }
    }
}
