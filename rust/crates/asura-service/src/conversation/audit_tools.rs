//! Shared bounded projection for control and model audit reads. No file access.
use asura_control::pb;
use asura_storage::audit::{Event, Health, HealthState, Record};

pub(super) fn unavailable() -> Health {
    Health {
        state: HealthState::Unavailable,
        enabled: false,
        keep_files: None,
        max_file_bytes: None,
        dropped: 0,
        window_capacity: 256,
        hydrated: false,
        older_omitted: false,
        reason: Some(asura_storage::audit::HealthReason::Io),
    }
}
pub(super) fn read(
    records: &[Record],
    health: &Health,
    project: [u8; 16],
    limit: u32,
    error: Option<&str>,
) -> (pb::AuditReply, String) {
    let mut reply = pb::AuditReply {
        project_id: Some(project.to_vec()),
        health: Some(pb::AuditHealth {
            state: Some(health.state as u32),
            enabled: Some(health.enabled),
            keep_files: health.keep_files,
            max_file_bytes: health.max_file_bytes,
            dropped: Some(health.dropped),
            window_capacity: Some(256),
            hydrated: Some(health.hydrated),
            older_omitted: Some(health.older_omitted),
            reason: health.reason.map(|r| r as u32),
        }),
        entries: Vec::new(),
        error: error.map(str::to_owned),
    };
    let mut lines = String::new();
    let mut omitted = health.older_omitted;
    if error.is_none()
        && matches!(
            health.state,
            HealthState::Active | HealthState::Disabled | HealthState::Stale
        )
    {
        for record in records
            .iter()
            .take(256)
            .filter(|record| record.event.project() == Some(project))
        {
            let Ok(bytes) = record.encode() else {
                let mut invalid = health.clone();
                invalid.state = HealthState::Unavailable;
                invalid.reason = Some(asura_storage::audit::HealthReason::MalformedRecord);
                return read(&[], &invalid, project, limit, error);
            };
            if reply.entries.len() >= limit.min(16) as usize || lines.len() + bytes.len() > 15_000 {
                omitted = true;
                break;
            }
            let mut entry = pb::AuditEntry {
                service_epoch: Some(record.service_epoch.to_vec()),
                sequence: Some(record.sequence),
                unix_time_ms: record.unix_time_ms,
                project: Some(project.to_vec()),
                reason: Some(0),
                ..Default::default()
            };
            match &record.event {
                Event::ConversationAdmission {
                    request,
                    conversation,
                    operation,
                    requested_generation,
                    current_generation,
                    outcome,
                    reason,
                    ..
                } => {
                    entry.kind = Some(1);
                    entry.request = Some(request.to_vec());
                    entry.conversation = conversation.map(|v| v.to_vec());
                    entry.operation = operation.map(|v| v.to_vec());
                    entry.requested_generation = Some(*requested_generation);
                    entry.current_generation = *current_generation;
                    entry.outcome = Some(*outcome as u32);
                    entry.reason = Some(*reason as u32);
                }
                Event::ConversationFinished {
                    conversation,
                    operation,
                    generation,
                    outcome,
                    reason,
                    ..
                } => {
                    entry.kind = Some(2);
                    entry.conversation = Some(conversation.to_vec());
                    entry.operation = Some(operation.to_vec());
                    entry.generation = Some(*generation);
                    entry.outcome = Some(*outcome as u32);
                    entry.reason = Some(*reason as u32);
                }
                Event::ToolFinished {
                    operation,
                    generation,
                    ordinal,
                    tool,
                    outcome,
                    ..
                } => {
                    entry.kind = Some(3);
                    entry.operation = Some(operation.to_vec());
                    entry.generation = Some(*generation);
                    entry.ordinal = Some(*ordinal);
                    entry.tool = Some(*tool as u32);
                    entry.outcome = Some(*outcome as u32);
                }
                _ => continue,
            }
            let Ok(line) = std::str::from_utf8(&bytes) else {
                continue;
            };
            lines.push_str(line);
            reply.entries.push(entry);
        }
    }
    reply.health.as_mut().unwrap().older_omitted = Some(omitted);
    let text = format!(
        "Audit diagnostic evidence; window=recent capacity=256 state={:?} enabled={} keep_files={:?} max_file_bytes={:?} reason={:?} hydrated={} dropped={} older_omitted={}\n{}",
        health.state,
        health.enabled,
        health.keep_files,
        health.max_file_bytes,
        health.reason,
        health.hydrated,
        health.dropped,
        omitted,
        lines
    );
    (reply, text)
}

pub(super) fn reason(code: &str) -> asura_storage::audit::DecisionReason {
    use asura_storage::audit::DecisionReason as R;
    match code {
        "request_conflict" => R::RequestConflict,
        "generation_conflict" => R::GenerationConflict,
        "invalid_request" | "model_input_invalid" => R::Invalid,
        "conversation_busy" | "model_busy" => R::Busy,
        "capacity_exhausted" | "context_limit" | "output_limit" => R::Limit,
        "storage_timeout" | "model_timeout" => R::Timeout,
        "cancelled" | "service_draining" => R::Cancelled,
        "project_stale" | "project_unknown" => R::Denied,
        "model_unavailable"
        | "storage_unavailable"
        | "authority_repair_required"
        | "installation_not_initialized" => R::Unavailable,
        _ => R::Internal,
    }
}
pub(super) fn rejected(
    request: &pb::Envelope,
    state: Option<&asura_storage::authority::conversation::Replay>,
    code: &str,
) -> Option<Event> {
    let pb::envelope::Body::ConversationSubmit(query) = request.body.as_ref()? else {
        return None;
    };
    let project: [u8; 16] = query.project_id.as_deref()?.try_into().ok()?;
    let state = state?;
    if !state.projects.contains_key(&project) {
        return None;
    }
    let conversation = query
        .conversation_id
        .as_deref()
        .and_then(|v| v.try_into().ok());
    let current_generation = conversation
        .and_then(|id| state.conversations.get(&id))
        .filter(|value| value.project == project)
        .map(|value| value.generation);
    Some(Event::ConversationAdmission {
        project,
        request: query.request_id.as_deref()?.try_into().ok()?,
        conversation,
        operation: None,
        requested_generation: query.expected_generation?,
        current_generation,
        outcome: if code == "outcome_unconfirmed" {
            asura_storage::audit::AdmissionOutcome::Unconfirmed
        } else {
            asura_storage::audit::AdmissionOutcome::Rejected
        },
        reason: reason(code),
    })
}

pub(super) fn finished(
    turn: &asura_storage::authority::conversation::TurnAccepted,
    terminal: &asura_storage::authority::conversation::TurnTerminal,
) -> Event {
    use asura_storage::{
        audit::{DecisionReason as R, FinishedOutcome as O},
        authority::conversation::{Cause, TerminalKind},
    };
    let outcome = match terminal.kind {
        TerminalKind::Complete => O::Completed,
        TerminalKind::Failed => O::Failed,
        TerminalKind::Cancelled => O::Cancelled,
        TerminalKind::Interrupted => O::Interrupted,
    };
    let reason = match terminal.cause {
        Cause::None => R::None,
        Cause::UserCancel | Cause::Steering | Cause::ServiceShutdown => R::Cancelled,
        Cause::Deadline => R::Timeout,
        Cause::OutputLimit | Cause::InputLimit => R::Limit,
        Cause::ProviderFailure | Cause::Restart => R::Unavailable,
        Cause::AuthorityFailure | Cause::ProtocolFailure => R::Internal,
    };
    Event::ConversationFinished {
        project: turn.project,
        conversation: turn.conversation,
        operation: turn.operation,
        generation: turn.generation,
        outcome,
        reason,
    }
}
pub(super) fn tool_finished(
    turn: &asura_storage::authority::conversation::TurnAccepted,
    call: &crate::tools::Call,
    result: &asura_storage::authority::conversation::ToolResult,
) -> Event {
    use crate::tools::Arguments as A;
    use asura_storage::audit::{Tool, ToolOutcome as O};
    let tool = match call.arguments {
        A::Shell { .. } => Tool::Shell,
        A::ReadFile { .. } => Tool::ProjectReadFile,
        A::ListDirectory { .. } => Tool::ProjectListDirectory,
        A::ObserveStatus => Tool::ServiceObserveStatus,
        A::ListTools => Tool::ServiceListTools,
        A::ReadAudit { .. } => Tool::ServiceReadAudit,
        A::MemoryListNotes { .. } => Tool::MemoryListNotes,
        A::MemoryGetNote { .. } => Tool::MemoryGetNote,
        A::MemoryNoteSources { .. } => Tool::MemoryNoteSources,
        A::MemoryCreateNote { .. } => Tool::MemoryCreateNote,
    };
    let outcome = match result.status {
        1 => O::Success,
        2 => O::Denied,
        3 => O::InvalidArguments,
        5 => O::Timeout,
        6 => O::Cancelled,
        7 => O::Limit,
        _ => O::Unavailable,
    };
    Event::ToolFinished {
        project: turn.project,
        operation: turn.operation,
        generation: turn.generation,
        ordinal: call.ordinal,
        tool,
        outcome,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use asura_storage::audit::{AdmissionOutcome, DecisionReason};
    fn record(project: [u8; 16], sequence: u64) -> Record {
        Record {
            schema: 1,
            service_build: "asura/test".into(),
            service_epoch: [2; 16],
            sequence,
            unix_time_ms: None,
            event: Event::ConversationAdmission {
                project,
                request: [3; 16],
                conversation: Some([4; 16]),
                operation: None,
                requested_generation: 3,
                current_generation: Some(4),
                outcome: AdmissionOutcome::Rejected,
                reason: DecisionReason::RequestConflict,
            },
        }
    }
    fn health() -> Health {
        Health {
            state: HealthState::Active,
            enabled: true,
            keep_files: Some(5),
            max_file_bytes: Some(10485760),
            dropped: 0,
            window_capacity: 256,
            hydrated: true,
            older_omitted: false,
            reason: None,
        }
    }
    #[test]
    fn inspection_excludes_other_projects_and_enforces_closed_control_contract() {
        let records = [record([9; 16], 4), record([1; 16], 3), record([1; 16], 2)];
        let (reply, text) = read(&records, &health(), [1; 16], 1, None);
        assert_eq!(reply.entries.len(), 1);
        assert_eq!(reply.entries[0].sequence, Some(3));
        assert_eq!(reply.health.as_ref().unwrap().older_omitted, Some(true));
        assert!(!text.contains(&"09".repeat(16)));
        assert!(text.contains("request_conflict"));
        let env = pb::Envelope {
            service_epoch: Some(vec![2; 16]),
            attachment_id: Some(vec![3; 16]),
            request_counter: Some(1),
            body: Some(pb::envelope::Body::AuditReply(reply)),
        };
        assert!(
            asura_control::validate_semantics(&env, asura_control::Direction::ServerToClient)
                .is_ok()
        );
        let (reply, _) = read(&records, &health(), [1; 16], 16, Some("project_unknown"));
        assert!(reply.entries.is_empty());
    }
    #[test]
    fn invalid_cached_record_hides_the_whole_response() {
        let mut invalid = record([1; 16], 2);
        invalid.schema = 2;
        let (reply, text) = read(&[record([1; 16], 3), invalid], &health(), [1; 16], 16, None);
        assert!(reply.entries.is_empty());
        assert_eq!(
            reply.health.unwrap().state,
            Some(HealthState::Unavailable as u32)
        );
        assert!(text.contains("MalformedRecord"));
        assert!(!text.contains("request_conflict"));
    }
    #[test]
    fn unavailable_hides_rows_and_stale_retains_committed_evidence_with_health() {
        let rows = [record([1; 16], 1)];
        assert!(
            read(&rows, &unavailable(), [1; 16], 16, None)
                .0
                .entries
                .is_empty()
        );
        let mut stale = health();
        stale.state = HealthState::Stale;
        stale.dropped = 3;
        let (reply, text) = read(&rows, &stale, [1; 16], 16, None);
        assert_eq!(reply.entries.len(), 1);
        assert!(text.contains("dropped=3"));
        assert!(text.len() <= 16384);
    }
}
