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
pub(super) fn valid(body: &Body) -> bool {
    match body {
        Body::ConversationQueueSubmit(v) => {
            id(&v.request_id)
                && id(&v.project_id)
                && match (
                    v.new_conversation,
                    &v.conversation_id,
                    v.expected_generation,
                ) {
                    (Some(true), None, Some(0)) => true,
                    (Some(false), Some(conversation), Some(generation)) => {
                        conversation.len() == 16
                            && conversation.iter().any(|byte| *byte != 0)
                            && generation < u64::MAX
                    }
                    _ => false,
                }
                && v.prompt
                    .as_ref()
                    .is_some_and(|p| !p.is_empty() && p.len() <= 32768)
        }
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
                            Some(1) => {
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
                        && e.sequence.is_some_and(|n| n > 0)
                        && e.text
                            .as_ref()
                            .is_some_and(|s| s.len() <= if full { 32768 } else { 256 })
                        && if matches!(e.state, Some(3..=5)) {
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
                        && v.generation.is_some_and(|n| n > 0)
                        && v.entries.len() <= 8
                        && (v.has_more == Some(false) || !v.entries.is_empty())
                        && v.entries.iter().all(|e| {
                            id(&e.operation_id)
                                && e.generation
                                    .is_some_and(|n| n > 0 && n <= v.generation.unwrap())
                                && e.accepted_frame.is_some_and(|n| n > 0)
                                && matches!(e.kind, Some(1 | 3..=6))
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
                        // Kind 14 decision and fault codes (TCR-D13).
                        && tool.policy_decision.is_none_or(|n| (1..=8).contains(&n))
                        && tool.fault.is_none_or(|n| (1..=15).contains(&n))
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
            let window = matches!(
                (&v.text, v.text_offset, v.text_bytes),
                (Some(text), Some(offset), Some(total))
                    if text.len() <= MAX_RESPONSE_WINDOW_BYTES
                        && offset.checked_add(text.len() as u64).is_some_and(|end| end <= total)
            );
            base && step
                && match v.kind {
                    Some(1) => {
                        v.text.is_none()
                            && v.text_offset.is_none()
                            && v.text_bytes.is_none()
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
            text: Some("a".repeat(MAX_RESPONSE_WINDOW_BYTES)),
            text_offset: Some(u64::MAX - MAX_RESPONSE_WINDOW_BYTES as u64),
            text_bytes: Some(u64::MAX),
            usage_known: Some(false),
            // The most recent window, ending at the over-limit ordinal (TCR-D31).
            tools: (crate::model::MAX_TOOL_ORDINAL + 1 - MAX_TOOL_PROGRESS_ENTRIES as u32
                ..=crate::model::MAX_TOOL_ORDINAL)
                .map(|ordinal| pb::ToolProgress {
                    ordinal: Some(ordinal),
                    name: Some("a".repeat(64)),
                    fault: Some(15),
                    state: Some(2),
                    status: Some(1),
                    arguments: Some("a".repeat(192)),
                    preview: Some("a".repeat(128)),
                    policy_decision: Some(8),
                    elapsed_ms: Some(u64::MAX),
                    result_bytes: Some(65536),
                    truncated: Some(true),
                    exit_code: Some(i32::MIN),
                })
                .collect(),
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
        let frame = crate::encode_frame(&message).unwrap();
        assert!(frame.len() <= crate::MAX_FRAME_BYTES + crate::HEADER_BYTES);
        assert_eq!(
            crate::decode_body(&frame[crate::HEADER_BYTES..]).unwrap(),
            message
        );
        let mut invalid = message.clone();
        if let Some(Body::ConversationEvent(v)) = invalid.body.as_mut() {
            v.tools[0].arguments = Some("a".repeat(193));
        }
        assert!(!valid(invalid.body.as_ref().unwrap()));
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
        };
        assert!(valid(&Body::ConversationObserve(observe)));
    }
}
