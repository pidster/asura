//! Local control framing and validation. This crate performs no runtime I/O.
use prost::Message;
use std::fmt;
mod audit_query;
mod sensor_query;

pub mod pb {
    include!(concat!(env!("OUT_DIR"), "/asura.control.v1.rs"));
}

pub const MAX_FRAME_BYTES: usize = 65_536;
pub const HEADER_BYTES: usize = 12;
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProtocolError {
    BadMagic,
    UnsupportedVersion { major: u16, minor: u16 },
    InvalidLength,
    BufferFull,
    MalformedWire,
    InvalidSemantics,
    WrongDirection,
    IdentityMismatch,
}
impl fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for ProtocolError {}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    ClientToServer,
    ServerToClient,
}
#[derive(Debug, Clone, PartialEq)]
pub enum Frame {
    Message(Box<pb::Envelope>),
    VersionRejected,
}

/// Buffers one frame. `push` returns consumed bytes; retain any remainder.
/// The caller owns deadlines, pipeline rejection and negotiation state.
#[derive(Default)]
pub struct ControlCodec {
    bytes: Vec<u8>,
    target: Option<usize>,
}
impl ControlCodec {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn buffered_len(&self) -> usize {
        self.bytes.len()
    }
    pub fn push(&mut self, input: &[u8]) -> Result<usize, ProtocolError> {
        let mut consumed = 0;
        if self.bytes.len() < HEADER_BYTES {
            consumed = input.len().min(HEADER_BYTES - self.bytes.len());
            self.bytes.extend_from_slice(&input[..consumed]);
            if self.bytes.len() < HEADER_BYTES {
                return Ok(consumed);
            }
            let target = HEADER_BYTES + header(&self.bytes)?;
            self.bytes.reserve_exact(target - self.bytes.len());
            self.target = Some(target);
        }
        let target = self.target.ok_or(ProtocolError::MalformedWire)?;
        let count = (target - self.bytes.len()).min(input.len() - consumed);
        self.bytes
            .extend_from_slice(&input[consumed..consumed + count]);
        Ok(consumed + count)
    }
    pub fn next_frame(&mut self) -> Result<Option<Frame>, ProtocolError> {
        let Some(target) = self.target else {
            return Ok(None);
        };
        if self.bytes.len() != target {
            return Ok(None);
        }
        let frame = if target == HEADER_BYTES {
            Frame::VersionRejected
        } else {
            Frame::Message(Box::new(decode_body(&self.bytes[HEADER_BYTES..])?))
        };
        self.bytes.clear();
        self.target = None;
        Ok(Some(frame))
    }
}
fn header(bytes: &[u8]) -> Result<usize, ProtocolError> {
    if &bytes[..4] != b"ASUR" {
        return Err(ProtocolError::BadMagic);
    }
    let major = u16::from_be_bytes([bytes[4], bytes[5]]);
    let minor = u16::from_be_bytes([bytes[6], bytes[7]]);
    let length = u32::from_be_bytes(bytes[8..12].try_into().unwrap()) as usize;
    // A server rejection advertises its supported version, which can differ.
    // The caller accepts zero length only during server negotiation.
    if length != 0 && (major, minor) != (0, 1) {
        return Err(ProtocolError::UnsupportedVersion { major, minor });
    }
    if length > MAX_FRAME_BYTES {
        return Err(ProtocolError::InvalidLength);
    }
    Ok(length)
}
pub fn version_rejection() -> [u8; HEADER_BYTES] {
    [b'A', b'S', b'U', b'R', 0, 0, 0, 1, 0, 0, 0, 0]
}
pub fn encode_frame(envelope: &pb::Envelope) -> Result<Vec<u8>, ProtocolError> {
    let length = envelope.encoded_len();
    if length == 0 || length > MAX_FRAME_BYTES {
        return Err(ProtocolError::InvalidLength);
    }
    let body = envelope.encode_to_vec();
    decode_body(&body)?;
    let mut frame = Vec::with_capacity(HEADER_BYTES + length);
    frame.extend_from_slice(&version_rejection());
    frame[8..12].copy_from_slice(&(length as u32).to_be_bytes());
    frame.extend_from_slice(&body);
    Ok(frame)
}
pub fn decode_body(bytes: &[u8]) -> Result<pb::Envelope, ProtocolError> {
    if bytes.is_empty() || bytes.len() > MAX_FRAME_BYTES {
        return Err(ProtocolError::InvalidLength);
    }
    scan(bytes, ROOT, 1, TABLES)?;
    let envelope = pb::Envelope::decode(bytes).map_err(|_| ProtocolError::MalformedWire)?;
    if envelope.body.is_none() {
        return Err(ProtocolError::MalformedWire);
    }
    Ok(envelope)
}

mod wire;
use wire::{Field, Kind, scan};
include!(concat!(env!("OUT_DIR"), "/validation.rs"));
mod conversation;
pub mod model;

fn context_path(value: &Option<String>) -> bool {
    value.as_ref().is_some_and(|path| {
        path.starts_with('/')
            && path.len() <= 4096
            && !path.contains('\0')
            && path.split('/').all(|part| part != "." && part != "..")
    })
}

fn id(value: &Option<Vec<u8>>) -> bool {
    value
        .as_ref()
        .is_some_and(|v| v.len() == 16 && v.iter().any(|b| *b != 0))
}
fn text(value: &Option<String>, limit: usize) -> bool {
    value.as_ref().is_some_and(|s| {
        !s.is_empty() && s.len() <= limit && s.bytes().all(|b| (32..=126).contains(&b))
    })
}
fn installation_pair(state: Option<i32>, reason: Option<i32>) -> bool {
    use pb::{InstallationInspectionReason as R, InstallationState as S};
    let (Some(state), Some(reason)) = (state, reason) else {
        return false;
    };
    let (Ok(state), Ok(reason)) = (S::try_from(state), R::try_from(reason)) else {
        return false;
    };
    matches!(
        (state, reason),
        (S::Uninitialized, R::RuntimeOnly)
            | (
                S::Recovering,
                R::InspectionPending | R::InitializationPending
            )
            | (S::GraphUnavailable, R::GraphVerificationUnavailable)
            | (S::GraphReady, R::GraphVerified)
            | (
                S::RepairRequired,
                R::InstallationRemnants
                    | R::UnknownContent
                    | R::UnsupportedLayout
                    | R::UnsupportedFormat
                    | R::IncompleteTail
                    | R::CorruptAuthority
            )
            | (
                S::Unavailable,
                R::InspectionLimit
                    | R::InspectionTimeout
                    | R::UnsafeAuthority
                    | R::AuthorityChanged
                    | R::InspectionIo
            )
    )
}
fn installation_reply(value: &pb::InspectInstallationReply) -> bool {
    use pb::InstallationInspectionReason as R;
    if !installation_pair(value.installation, value.reason) {
        return false;
    }
    let replayed = matches!(
        value.reason.and_then(|r| R::try_from(r).ok()),
        Some(R::InitializationPending | R::GraphVerificationUnavailable | R::GraphVerified)
    );
    if replayed {
        id(&value.installation_id)
            && value.authority_revision.is_some_and(|v| v > 0)
            && value.recorded_owner_generation.is_some_and(|v| v > 0)
            && value.authority_format == Some(1)
            && if matches!(value.reason, Some(n) if n==R::GraphVerificationUnavailable as i32 || n==R::GraphVerified as i32)
            {
                value.binding_generation.is_some_and(|v| v > 0)
            } else {
                value.binding_generation.is_none()
            }
    } else {
        value.installation_id.is_none()
            && value.authority_revision.is_none()
            && value.recorded_owner_generation.is_none()
            && value.binding_generation.is_none()
            && value.authority_format.is_none()
    }
}
/// Validates domain shape and direction. Attachment state stays with its owner.
pub fn validate_semantics(
    envelope: &pb::Envelope,
    direction: Direction,
) -> Result<(), ProtocolError> {
    use pb::envelope::Body;
    let body = envelope
        .body
        .as_ref()
        .ok_or(ProtocolError::InvalidSemantics)?;
    let request = matches!(
        body,
        Body::Hello(_)
            | Body::Inspect(_)
            | Body::InspectInstallation(_)
            | Body::Stop(_)
            | Body::ModelsList(_)
            | Body::SensorsInspect(_)
            | Body::AuditRead(_)
            | Body::ConfigGet(_)
            | Body::ConfigSet(_)
            | Body::ConversationSubmit(_)
            | Body::ConversationHistory(_)
            | Body::ConversationReadPrompt(_)
            | Body::ConversationObserve(_)
            | Body::ConversationCancel(_)
            | Body::InitializeInstallation(_)
            | Body::ResolveInitialization(_)
            | Body::ProjectRegister(_)
            | Body::ProjectRename(_)
            | Body::ProjectList(_)
            | Body::ConversationEnqueue(_)
            | Body::ConversationQueueSubmit(_)
            | Body::ConversationQueueReorder(_)
            | Body::ConversationQueueList(_)
            | Body::ConversationQueueDecision(_)
            | Body::ObserveContext(_)
            | Body::ObserveService(_)
    );
    if request != (direction == Direction::ClientToServer) {
        return Err(ProtocolError::WrongDirection);
    }
    let identity_ok = match body {
        Body::Hello(_) => {
            envelope.service_epoch.is_none()
                && envelope.attachment_id.is_none()
                && envelope.request_counter.is_none()
        }
        Body::HelloReply(_) => {
            id(&envelope.service_epoch)
                && id(&envelope.attachment_id)
                && envelope.request_counter.is_none()
        }
        _ => {
            id(&envelope.service_epoch)
                && id(&envelope.attachment_id)
                && envelope.request_counter.is_some_and(|v| v != 0)
        }
    };
    let valid = match body {
        Body::Hello(v) => text(&v.client_build, 128),
        Body::HelloReply(v) => {
            text(&v.service_build, 128)
                && v.max_frame_bytes == Some(MAX_FRAME_BYTES as u32)
                && (4..=6).contains(&v.capabilities.len())
                && v.capabilities.iter().all(|c| (1..=6).contains(c))
                && v.capabilities
                    .iter()
                    .enumerate()
                    .all(|(i, c)| !v.capabilities[..i].contains(c))
                && v.capabilities.contains(&(pb::Capability::Config as i32))
                && v.capabilities.contains(&(pb::Capability::Inspect as i32))
                && v.capabilities.contains(&(pb::Capability::Stop as i32))
                && v.capabilities
                    .contains(&(pb::Capability::InspectInstallation as i32))
        }
        Body::Inspect(_) | Body::InspectInstallation(_) | Body::StopAccepted(_) => true,
        Body::InspectReply(v) => {
            v.lifecycle
                .is_some_and(|n| pb::Lifecycle::try_from(n).is_ok() && n != 0)
                && installation_pair(v.installation, v.reason)
                && v.unavailable_reason.is_none()
        }
        Body::InspectInstallationReply(v) => installation_reply(v),
        Body::ObserveService(_) => true,
        Body::ServiceObservation(v) => {
            v.uptime_ms.is_some()
                && v.stored_memory.as_ref().is_some_and(|memory| {
                    let available = v
                        .status
                        .as_ref()
                        .is_some_and(|s| s.installation == Some(6) && s.reason == Some(16));
                    memory.available == Some(available)
                        && memory.size_bytes.is_some() == memory.sampled_uptime_ms.is_some()
                        && memory
                            .sampled_uptime_ms
                            .is_none_or(|sample| sample <= v.uptime_ms.unwrap_or(0))
                        && match memory.size_reason {
                            Some(1) => memory.size_bytes.is_some() && memory.stale == Some(false),
                            Some(2..=7) => memory.stale == Some(true),
                            _ => false,
                        }
                })
                && v.revision.is_some_and(|revision| revision > 0)
                && v.pending.is_some()
                && v.status.as_ref().is_some_and(|status| {
                    status
                        .lifecycle
                        .is_some_and(|n| pb::Lifecycle::try_from(n).is_ok() && n != 0)
                        && installation_pair(status.installation, status.reason)
                        && status.unavailable_reason.is_none()
                })
                && (v.configured_model.is_none()
                    || v.configured_model.as_ref().is_some_and(|s| {
                        !s.is_empty() && s.len() <= 1024 && !s.chars().any(char::is_control)
                    }))
        }
        Body::ObserveContext(v) => {
            id(&v.project_id)
                && context_path(&v.working_directory)
                && match (&v.after_subscription, v.after_revision) {
                    (None, None) => true,
                    (Some(_), Some(_)) => id(&v.after_subscription),
                    _ => false,
                }
        }
        Body::ContextObservation(v) => {
            id(&v.project_id)
                && context_path(&v.working_directory)
                && id(&v.subscription_id)
                && v.revision.is_some()
                && v.pending.is_some()
                && if v.pending == Some(true) {
                    v.git_state.is_none()
                        && v.branch.is_none()
                        && v.detached.is_none()
                        && v.unborn.is_none()
                        && v.conflicts.is_none()
                        && v.reason.is_none()
                        && v.files_changed.is_none()
                        && v.added.is_none()
                        && v.deleted.is_none()
                } else {
                    v.revision.is_some_and(|revision| revision > 0)
                        && v.git_state.is_some_and(|state| state <= 3)
                        && v.detached.is_some()
                        && v.unborn.is_some()
                        && v.conflicts.is_some()
                        && (v.branch.is_none()
                            || v.branch.as_ref().is_some_and(|branch| {
                                !branch.is_empty()
                                    && branch.len() <= 1024
                                    && !branch.chars().any(char::is_control)
                            }))
                        && (v.reason.is_none() || text(&v.reason, 256))
                        && (v.git_state != Some(0) || text(&v.reason, 256))
                        && if matches!(v.git_state, Some(2 | 3)) {
                            v.files_changed.is_some() && v.added.is_some() == v.deleted.is_some()
                        } else {
                            v.files_changed.is_none() && v.added.is_none() && v.deleted.is_none()
                        }
                }
        }
        Body::AuditRead(v) => audit_query::request(v),
        Body::AuditReply(v) => audit_query::reply(v),
        Body::SensorsInspect(v) => sensor_query::request(v),
        Body::SensorsReply(v) => sensor_query::reply(v),
        Body::ModelsList(_) => true,
        Body::ModelsReply(v) => {
            if v.error.is_some() {
                inventory_reason(v.error.as_deref())
                    && v.configured_model.is_none()
                    && v.models.is_empty()
                    && v.issues.is_empty()
            } else {
                v.configured_model.as_ref().is_some_and(|s| {
                    !s.is_empty() && s.len() <= 1024 && !s.chars().any(char::is_control)
                }) && !v.models.is_empty()
                    && v.models.len() <= 65
                    && v.issues.len() <= 4
                    && v.models.iter().all(|row| {
                        inventory_entry(
                            row.selector.as_deref(),
                            row.provider.as_deref(),
                            row.status,
                            row.detail.as_deref(),
                            1024,
                        ) || (row.selector == v.configured_model
                            && row.provider.as_deref() == Some("unknown")
                            && row.status == Some(4)
                            && row.detail.as_deref() == Some("unsupported_selection"))
                    })
                    && v.issues.iter().all(|issue| {
                        inventory_provider(issue.provider.as_deref())
                            && inventory_reason(issue.reason.as_deref())
                    })
                    && v.models.iter().enumerate().all(|(index, row)| {
                        !v.models[..index]
                            .iter()
                            .any(|other| other.selector == row.selector)
                    })
                    && v.models
                        .iter()
                        .any(|row| row.selector == v.configured_model)
            }
        }
        Body::ConfigGet(v) => v.key.is_none() || text(&v.key, 128),
        Body::ConfigSet(v) => {
            text(&v.key, 128)
                && v.value_yaml
                    .as_ref()
                    .is_some_and(|s| !s.is_empty() && s.len() <= 4096)
        }
        Body::ConfigReply(v) => match (&v.value_yaml, &v.error) {
            (Some(value), None) => !value.is_empty() && value.len() <= 16384,
            (None, Some(_)) => text(&v.error, 256),
            _ => false,
        },
        body if conversation::handles(body) => conversation::valid(body),
        Body::Stop(v) => v.expected_epoch == envelope.service_epoch,
        Body::Error(v) => {
            v.code
                .is_some_and(|n| pb::ErrorCode::try_from(n).is_ok() && n != 0)
                && text(&v.message, 256)
        }
        _ => false,
    };
    if identity_ok && valid {
        Ok(())
    } else {
        Err(ProtocolError::InvalidSemantics)
    }
}
/// Checks exact correlation; does not advance the caller's sequence state.
pub fn validate_identity(
    envelope: &pb::Envelope,
    epoch: &[u8; 16],
    attachment: &[u8; 16],
    counter: u64,
) -> Result<(), ProtocolError> {
    if counter != 0
        && envelope.service_epoch.as_deref() == Some(epoch.as_slice())
        && envelope.attachment_id.as_deref() == Some(attachment.as_slice())
        && envelope.request_counter == Some(counter)
    {
        Ok(())
    } else {
        Err(ProtocolError::IdentityMismatch)
    }
}

/// Shared inventory field validation for the public and private generated contracts.
pub(crate) fn inventory_selector(value: &str, max: usize) -> bool {
    !value.is_empty()
        && value.len() <= max
        && !value.chars().any(|c| c.is_whitespace() || c.is_control())
}
pub(crate) fn inventory_provider(value: Option<&str>) -> bool {
    matches!(value, Some("system" | "ollama" | "coreai" | "mlx"))
}
pub(crate) fn inventory_reason(value: Option<&str>) -> bool {
    value.is_some_and(|v| {
        !v.is_empty() && v.len() <= 64 && v.bytes().all(|b| b.is_ascii_lowercase() || b == b'_')
    })
}
pub(crate) fn inventory_entry(
    selector: Option<&str>,
    provider: Option<&str>,
    status: Option<u32>,
    detail: Option<&str>,
    max: usize,
) -> bool {
    inventory_provider(provider)
        && match status {
            Some(1) => provider == Some("system"),
            Some(2) => matches!(provider, Some("coreai" | "mlx")),
            Some(3) => provider == Some("ollama"),
            Some(4 | 5) => true,
            _ => false,
        }
        && selector.is_some_and(|s| {
            inventory_selector(s, max)
                && if provider == Some("system") {
                    s == "system"
                } else {
                    s.split_once(':')
                        .is_some_and(|(p, name)| Some(p) == provider && !name.is_empty())
                }
        })
        && detail
            .is_none_or(|s| !s.is_empty() && s.len() <= 256 && !s.chars().any(char::is_control))
}

#[cfg(test)]
mod config_root_tests {
    use super::*;
    #[test]
    fn config_get_absent_key_selects_root_but_empty_key_is_invalid() {
        for (key, accepted) in [
            (None, true),
            (Some(String::new()), false),
            (Some("model".into()), true),
        ] {
            let envelope = pb::Envelope {
                service_epoch: Some(vec![1; 16]),
                attachment_id: Some(vec![2; 16]),
                request_counter: Some(1),
                body: Some(pb::envelope::Body::ConfigGet(pb::ConfigGet { key })),
            };
            assert_eq!(
                validate_semantics(&envelope, Direction::ClientToServer).is_ok(),
                accepted
            );
            let encoded = encode_frame(&envelope).unwrap();
            let decoded = decode_body(&encoded[HEADER_BYTES..]).unwrap();
            assert_eq!(decoded, envelope);
        }
        let envelope = pb::Envelope {
            service_epoch: Some(vec![1; 16]),
            attachment_id: Some(vec![2; 16]),
            request_counter: Some(1),
            body: Some(pb::envelope::Body::ConfigSet(pb::ConfigSet {
                key: None,
                value_yaml: Some("true".into()),
            })),
        };
        assert!(validate_semantics(&envelope, Direction::ClientToServer).is_err());
    }
}
