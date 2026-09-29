//! Closed project-scoped audit metadata. No free-form record fields.
use crate::pb;
use prost::Message;
fn id(v: &Option<Vec<u8>>) -> bool {
    v.as_ref()
        .is_some_and(|v| v.len() == 16 && v.iter().any(|b| *b != 0))
}
fn optional_id(v: &Option<Vec<u8>>) -> bool {
    v.is_none() || id(v)
}
pub(crate) fn request(v: &pb::AuditRead) -> bool {
    id(&v.project_id) && v.limit.is_some_and(|n| (1..=16).contains(&n))
}
fn health(v: &pb::AuditHealth) -> bool {
    v.state.is_some_and(|n| (1..=5).contains(&n))
        && v.enabled.is_some()
        && (matches!((v.keep_files,v.max_file_bytes),(Some(k),Some(b)) if (1..=10000).contains(&k) && (1..=1_099_511_627_776).contains(&b))
            || (matches!(v.state, Some(1 | 5))
                && v.keep_files.is_none()
                && v.max_file_bytes.is_none()))
        && v.dropped.is_some()
        && v.window_capacity == Some(256)
        && v.hydrated.is_some()
        && v.older_omitted.is_some()
        && v.reason.is_none_or(|n| (1..=10).contains(&n))
}
fn entry(v: &pb::AuditEntry) -> bool {
    id(&v.service_epoch)
        && v.sequence.is_some_and(|n| n > 0)
        && id(&v.project)
        && optional_id(&v.request)
        && optional_id(&v.conversation)
        && optional_id(&v.operation)
        && v.reason.is_some_and(|n| n <= 10)
        && match v.kind {
            Some(1) => {
                id(&v.request)
                    && v.requested_generation.is_some()
                    && v.generation.is_none()
                    && v.ordinal.is_none()
                    && v.tool.is_none()
                    && v.outcome.is_some_and(|n| (1..=3).contains(&n))
            }
            Some(2) => {
                v.request.is_none()
                    && id(&v.conversation)
                    && id(&v.operation)
                    && v.requested_generation.is_none()
                    && v.current_generation.is_none()
                    && v.generation.is_some_and(|n| n > 0)
                    && v.ordinal.is_none()
                    && v.tool.is_none()
                    && v.outcome.is_some_and(|n| (1..=4).contains(&n))
            }
            Some(3) => {
                v.request.is_none()
                    && v.conversation.is_none()
                    && id(&v.operation)
                    && v.requested_generation.is_none()
                    && v.current_generation.is_none()
                    && v.generation.is_some_and(|n| n > 0)
                    && v.ordinal.is_some_and(|n| (1..=8).contains(&n))
                    && v.tool.is_some_and(|n| (1..=10).contains(&n))
                    && v.outcome.is_some_and(|n| (1..=7).contains(&n))
                    && v.reason == Some(0)
            }
            _ => false,
        }
}
pub(crate) fn reply(v: &pb::AuditReply) -> bool {
    id(&v.project_id)
        && v.health.as_ref().is_some_and(health)
        && v.entries.len() <= 16
        && v.encoded_len() <= 16384
        && v.error.as_deref().is_none_or(|s| {
            matches!(
                s,
                "audit_unavailable"
                    | "project_stale"
                    | "invalid_request"
                    | "capacity_exhausted"
                    | "storage_timeout"
                    | "authority_unavailable"
                    | "authority_repair_required"
                    | "installation_required"
                    | "project_unknown"
                    | "conversation_busy"
                    | "installation_not_initialized"
                    | "service_draining"
                    | "storage_unavailable"
                    | "outcome_unconfirmed"
            )
        })
        && (v.error.is_none() || v.entries.is_empty())
        && (v
            .health
            .as_ref()
            .is_some_and(|h| !matches!(h.state, Some(1 | 5)))
            || v.entries.is_empty())
        && v.entries
            .iter()
            .all(|e| entry(e) && e.project == v.project_id)
        && v.entries.iter().enumerate().all(|(i, e)| {
            v.entries[..i]
                .iter()
                .all(|p| p.service_epoch != e.service_epoch || p.sequence > e.sequence)
        })
}
#[cfg(test)]
mod tests {
    use super::*;
    fn valid() -> pb::AuditReply {
        pb::AuditReply {
            project_id: Some(vec![1; 16]),
            health: Some(pb::AuditHealth {
                state: Some(2),
                enabled: Some(true),
                keep_files: Some(5),
                max_file_bytes: Some(10485760),
                dropped: Some(0),
                window_capacity: Some(256),
                hydrated: Some(true),
                older_omitted: Some(false),
                reason: None,
            }),
            entries: vec![pb::AuditEntry {
                service_epoch: Some(vec![2; 16]),
                sequence: Some(1),
                kind: Some(1),
                project: Some(vec![1; 16]),
                request: Some(vec![3; 16]),
                requested_generation: Some(3),
                current_generation: Some(4),
                outcome: Some(2),
                reason: Some(1),
                ..Default::default()
            }],
            error: None,
        }
    }
    #[test]
    fn closed_shapes_bounds_and_project_fence() {
        assert!(reply(&valid()));
        for limit in [0, 17, u32::MAX] {
            assert!(!request(&pb::AuditRead {
                project_id: Some(vec![1; 16]),
                limit: Some(limit)
            }));
        }
        let mut v = valid();
        v.entries[0].project = Some(vec![9; 16]);
        assert!(!reply(&v));
        let mut v = valid();
        v.entries[0].tool = Some(1);
        assert!(!reply(&v));
        let mut v = valid();
        v.health.as_mut().unwrap().reason = Some(11);
        assert!(!reply(&v));
        let mut v = valid();
        v.entries[0].reason = Some(11);
        assert!(!reply(&v));
        let mut v = valid();
        v.entries = vec![v.entries[0].clone(); 17];
        assert!(!reply(&v));
        let mut v = valid();
        v.health.as_mut().unwrap().state = Some(5);
        assert!(!reply(&v));
        v.entries.clear();
        v.health.as_mut().unwrap().keep_files = None;
        v.health.as_mut().unwrap().max_file_bytes = None;
        assert!(reply(&v));
    }
    #[test]
    fn memory_create_audit_code_is_closed() {
        let mut v = valid();
        v.entries[0] = pb::AuditEntry {
            service_epoch: Some(vec![1; 16]),
            sequence: Some(1),
            kind: Some(3),
            project: Some(vec![1; 16]),
            operation: Some(vec![2; 16]),
            generation: Some(1),
            ordinal: Some(1),
            tool: Some(10),
            outcome: Some(1),
            reason: Some(0),
            ..Default::default()
        };
        assert!(reply(&v));
        v.entries[0].tool = Some(11);
        assert!(!reply(&v));
    }
    #[test]
    fn audit_direction_and_unknown_nested_fields_are_strict() {
        let env = pb::Envelope {
            service_epoch: Some(vec![1; 16]),
            attachment_id: Some(vec![2; 16]),
            request_counter: Some(1),
            body: Some(pb::envelope::Body::AuditRead(pb::AuditRead {
                project_id: Some(vec![1; 16]),
                limit: Some(16),
            })),
        };
        assert!(crate::validate_semantics(&env, crate::Direction::ClientToServer).is_ok());
        assert!(crate::validate_semantics(&env, crate::Direction::ServerToClient).is_err());
        let mut header = env.clone();
        header.body = None;
        let mut bytes = header.encode_to_vec();
        bytes.extend_from_slice(&[0xfa, 2, 22, 10, 16]);
        bytes.extend_from_slice(&[1; 16]);
        bytes.extend_from_slice(&[16, 16, 24, 1]);
        assert!(crate::decode_body(&bytes).is_err());
        let reply_env = pb::Envelope {
            body: Some(pb::envelope::Body::AuditReply(valid())),
            ..env
        };
        assert!(crate::validate_semantics(&reply_env, crate::Direction::ServerToClient).is_ok());
        assert!(crate::validate_semantics(&reply_env, crate::Direction::ClientToServer).is_err());
    }
}
