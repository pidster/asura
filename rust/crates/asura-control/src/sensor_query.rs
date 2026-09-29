//! Closed, bounded inspection messages; observations confer no authority.
use crate::pb;
fn id(value: &Option<Vec<u8>>) -> bool {
    value
        .as_ref()
        .is_some_and(|v| v.len() == 16 && v.iter().any(|b| *b != 0))
}
pub(crate) fn request(v: &pb::SensorsInspect) -> bool {
    id(&v.project_id)
        && v.offset.is_some_and(|n| n <= 64)
        && v.limit.is_some_and(|n| (1..=16).contains(&n))
        && (v.offset == Some(0) || v.revision.is_some())
}
fn observation(v: &pb::SensorObservation) -> bool {
    let timestamps = matches!((v.observed_ms, v.received_ms, v.expires_ms), (Some(a), Some(b), Some(c)) if a <= b && b < c);
    let correlation = id(&v.operation) && v.generation.is_some_and(|n| n > 0);
    let activity = v.phase.is_some_and(|n| (1..=2).contains(&n)) && v.active_foreground.is_some();
    let status = v.lifecycle.is_some_and(|n| n <= 32)
        && v.installation.is_some_and(|n| n <= 32)
        && v.status_reason.is_some_and(|n| n <= 128)
        && v.ordinal.is_some_and(|n| (1..=8).contains(&n));
    id(&v.id)
        && id(&v.source_epoch)
        && v.sequence.is_some_and(|n| n > 0)
        && timestamps
        && (v.causal_root.is_none() || id(&v.causal_root))
        && v.background.is_some()
        && v.causal_hops.is_some_and(|n| n <= 255)
        && match v.source {
            Some(1) => {
                correlation
                    && activity
                    && v.since_ms.is_none()
                    && v.lifecycle.is_none()
                    && v.installation.is_none()
                    && v.status_reason.is_none()
                    && v.ordinal.is_none()
            }
            Some(2) => {
                v.since_ms.zip(v.observed_ms).is_some_and(|(a, b)| a <= b)
                    && v.operation.is_none()
                    && v.generation.is_none()
                    && v.phase.is_none()
                    && v.active_foreground.is_none()
                    && v.lifecycle.is_none()
                    && v.installation.is_none()
                    && v.status_reason.is_none()
                    && v.ordinal.is_none()
            }
            Some(3) => {
                correlation
                    && status
                    && v.phase.is_none()
                    && v.active_foreground.is_none()
                    && v.since_ms.is_none()
            }
            _ => false,
        }
}
fn proposal(v: &pb::SensorProposal) -> bool {
    id(&v.id)
        && matches!(v.purpose, Some(1 | 2))
        && matches!(
            (v.state, v.reason),
            (Some(1), Some(1 | 2)) | (Some(2), Some(4)) | (Some(3), Some(3))
        )
        && !v.observations.is_empty()
        && v.observations.len() <= 32
        && v.observations.iter().all(|v| id(&v.id))
        && v.observations
            .iter()
            .enumerate()
            .all(|(i, value)| !v.observations[..i].contains(value))
        && v.omitted.is_some()
        && v.created_ms.zip(v.expires_ms).is_some_and(|(a, b)| a < b)
}
pub(crate) fn reply(v: &pb::SensorsReply) -> bool {
    if let Some(error) = &v.error {
        return matches!(
            error.as_str(),
            "sensor_loading"
                | "sensor_unavailable"
                | "project_unknown"
                | "revision_conflict"
                | "invalid_offset"
                | "service_draining"
                | "conversation_busy"
                | "installation_not_initialized"
                | "authority_repair_required"
                | "storage_unavailable"
                | "outcome_unconfirmed"
                | "initialization_incomplete"
                | "installation_already_initialized"
                | "request_conflict"
                | "project_stale"
                | "model_unavailable"
                | "capacity_exhausted"
                | "storage_timeout"
                | "cancelled"
                | "invalid_request"
        ) && v.project_id.is_none()
            && v.revision.is_none()
            && v.total_observations.is_none()
            && v.next_offset.is_none()
            && v.pending_persistence.is_none()
            && v.intake_unavailable.is_none()
            && v.clock_uncertain.is_none()
            && v.observations.is_empty()
            && v.proposals.is_empty()
            && v.offset.is_none();
    }
    let Some((offset, total)) = v.offset.zip(v.total_observations) else {
        return false;
    };
    let end = offset.saturating_add(v.observations.len() as u32);
    id(&v.project_id)
        && v.revision.is_some()
        && total <= 64
        && offset <= total
        && end <= total
        && (end == total || !v.observations.is_empty())
        && v.next_offset == (end < total).then_some(end)
        && v.pending_persistence.is_some()
        && v.intake_unavailable.is_some()
        && v.clock_uncertain.is_some()
        && v.observations.len() <= 16
        && v.proposals.len() <= 8
        && v.observations.iter().all(observation)
        && v.proposals.iter().all(proposal)
        && v.observations
            .iter()
            .enumerate()
            .all(|(i, row)| !v.observations[..i].iter().any(|old| old.id == row.id))
        && v.proposals
            .iter()
            .enumerate()
            .all(|(i, row)| !v.proposals[..i].iter().any(|old| old.id == row.id))
}
