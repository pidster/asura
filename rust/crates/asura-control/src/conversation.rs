//! Runtime-free validation of setup and conversation commands.
use crate::{id, pb, text};
use pb::envelope::Body;
pub(super) fn handles(body: &Body) -> bool {
    matches!(
        body,
        Body::ConversationSubmit(_)
            | Body::ConversationHistory(_)
            | Body::ConversationHistoryReply(_)
            | Body::ConversationReadPrompt(_)
            | Body::ConversationPromptReply(_)
            | Body::ConversationAccepted(_)
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
            | Body::ConversationEnqueue(_)
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
pub(super) fn valid(body: &Body) -> bool {
    match body {
        Body::ConversationEnqueue(v) => {
            id(&v.request_id)
                && id(&v.project_id)
                && id(&v.conversation_id)
                && id(&v.target_operation_id)
                && v.target_generation.is_some_and(|n| n > 0)
                && matches!(v.kind, Some(1 | 2))
                && v.prompt
                    .as_ref()
                    .is_some_and(|p| !p.is_empty() && p.len() <= 32768)
        }
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
                    Some(1 | 2) => v.target_operation_id.is_none() && v.target_generation.is_none(),
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
                        && match e.new_conversation {
                            None => {
                                id(&e.target_operation_id)
                                    && e.target_generation.is_some_and(|n| n > 0)
                                    && e.order_position.is_none()
                            }
                            Some(new) => {
                                e.target_operation_id.is_none()
                                    && e.target_generation.is_some_and(|generation| {
                                        (new && generation == 0) || (!new && generation < u64::MAX)
                                    })
                                    && if matches!(e.state, Some(1 | 2)) {
                                        e.order_position.is_some_and(|position| position > 0)
                                    } else {
                                        e.order_position.is_none_or(|position| position > 0)
                                    }
                            }
                        }
                        && matches!(e.kind, Some(1 | 2))
                        && e.state.is_some_and(|n| (1..=6).contains(&n))
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
        Body::ConversationSubmit(v) => {
            id(&v.request_id)
                && id(&v.project_id)
                && if v.conversation_id.is_none() {
                    v.expected_generation == Some(0)
                } else {
                    id(&v.conversation_id) && v.expected_generation.is_some_and(|n| n > 0)
                }
                && v.prompt
                    .as_ref()
                    .is_some_and(|s| !s.is_empty() && s.len() <= 32768)
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
        Body::ConversationAccepted(v) => {
            id(&v.operation_id)
                && id(&v.conversation_id)
                && v.generation.is_some_and(|n| n > 0)
                && v.cursor == Some(1)
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
                    }) && match (m.input_tokens, m.capacity_tokens, m.basis) {
                        (None, None, None) => m.model_name.is_some(),
                        (Some(n), Some(c), Some(1)) => c > 0 && n <= c,
                        _ => false,
                    }
                })
                && v.tools.len() <= 8
                && v.tools.iter().enumerate().all(|(index, tool)| {
                    tool.ordinal == Some(index as u32 + 1)
                        && tool.name.as_ref().is_some_and(|name| {
                            !name.is_empty()
                                && name.len() <= 64
                                && name.bytes().all(|b| b.is_ascii_lowercase() || b == b'_')
                        })
                        && match tool.state {
                            Some(1 | 4) => tool.status.is_none(),
                            Some(2) => tool.status == Some(1),
                            Some(3) => tool.status.is_some_and(|n| (2..=7).contains(&n)),
                            _ => false,
                        }
                });
            base && match v.kind {
                Some(1) => {
                    v.text.is_none()
                        && v.usage_known.is_none()
                        && v.usage_tokens.is_none()
                        && v.reason == Some(0)
                }
                Some(2) => {
                    v.cursor.is_some_and(|n| n > 1 && n < u64::MAX)
                        && v.text.as_ref().is_some_and(|s| s.len() <= 61440)
                        && v.reason == Some(0)
                        && v.usage_known.is_none()
                        && v.usage_tokens.is_none()
                }
                Some(3..=6) => {
                    v.cursor == Some(u64::MAX)
                        && v.text.as_ref().is_some_and(|s| {
                            s.len() <= 61440 && (v.kind != Some(3) || !s.is_empty())
                        })
                        && (v.kind != Some(3) || v.reason == Some(0))
                        && match v.usage_known {
                            Some(true) => v.usage_tokens.is_some_and(|n| n <= 2048),
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
            (v.after_project_id.is_none() || id(&v.after_project_id))
                && v.limit.is_some_and(|n| (1..=8).contains(&n))
        }
        Body::ProjectsReply(v) => {
            if v.error.is_some() {
                text(&v.error, 256) && v.projects.is_empty() && v.next_cursor.is_none()
            } else {
                v.projects.len() <= 8
                    && v.projects.iter().all(|p| p.error.is_none() && project(p))
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
    fn submit_presence_generation_bounds_and_direction() {
        let request = pb::ConversationSubmit {
            request_id: Some(vec![3; 16]),
            project_id: Some(vec![4; 16]),
            conversation_id: None,
            expected_generation: Some(0),
            prompt: Some("Hello".into()),
        };
        let value = envelope(Body::ConversationSubmit(request.clone()));
        assert!(validate_semantics(&value, Direction::ClientToServer).is_ok());
        assert!(validate_semantics(&value, Direction::ServerToClient).is_err());
        assert_eq!(
            decode_body(&encode_frame(&value).unwrap()[HEADER_BYTES..]).unwrap(),
            value
        );
        for changed in [
            pb::ConversationSubmit {
                conversation_id: Some(vec![]),
                ..request.clone()
            },
            pb::ConversationSubmit {
                expected_generation: None,
                ..request.clone()
            },
            pb::ConversationSubmit {
                prompt: Some("x".repeat(32769)),
                ..request.clone()
            },
            pb::ConversationSubmit {
                conversation_id: Some(vec![5; 16]),
                ..request
            },
        ] {
            assert!(!valid(&Body::ConversationSubmit(changed)));
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
            operation_id: Some(vec![1; 16]),
            cursor: Some(u64::MAX),
            generation: Some(1),
            kind: Some(3),
            text: Some("Complete".into()),
            reason: Some(0),
            usage_tokens: None,
            usage_known: Some(false),
            tools: Vec::new(),
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
                usage_tokens: Some(2049),
                ..event
            },
        ] {
            assert!(!valid(&Body::ConversationEvent(changed)));
        }
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
            ..project
        })));
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
            }),
            ..Default::default()
        };
        assert!(valid(&Body::ConversationEvent(event.clone())));
        event.model_context.as_mut().unwrap().basis = None;
        assert!(!valid(&Body::ConversationEvent(event.clone())));
        event.model_context.as_mut().unwrap().basis = Some(1);
        event.model_context.as_mut().unwrap().input_tokens = Some(4097);
        assert!(!valid(&Body::ConversationEvent(event)));
    }
}
