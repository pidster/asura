//! Pure private model transport. Operation state and credit ledgers belong to the service.
use crate::{
    ProtocolError,
    wire::{Field, Kind, scan},
};
use prost::Message;
pub mod pb {
    include!(concat!(env!("OUT_DIR"), "/asura.model.v1.rs"));
}
include!(concat!(env!("OUT_DIR"), "/model-validation.rs"));
pub const MAX_FRAME_BYTES: usize = 65_536;
pub const MAX_CHUNK_BYTES: usize = 16_384;
pub const MAX_INPUT_BYTES: usize = 65_536;
const MAX_OUTPUT_BYTES: u64 = 4 * 1024 * 1024;
const MAX_SNAPSHOT_BYTES: u64 = 60 * 1024;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    ServiceToHelper,
    HelperToService,
}
/// Buffers one length-prefixed frame. The caller retains any unconsumed bytes.
#[derive(Default)]
pub struct ModelCodec {
    bytes: Vec<u8>,
    target: Option<usize>,
}
impl ModelCodec {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn buffered_len(&self) -> usize {
        self.bytes.len()
    }
    pub fn push(&mut self, input: &[u8]) -> Result<usize, ProtocolError> {
        let mut consumed = 0;
        if self.bytes.len() < 4 {
            consumed = input.len().min(4 - self.bytes.len());
            self.bytes.extend_from_slice(&input[..consumed]);
            if self.bytes.len() < 4 {
                return Ok(consumed);
            }
            let length = u32::from_be_bytes(self.bytes[..4].try_into().unwrap()) as usize;
            if length == 0 || length > MAX_FRAME_BYTES {
                return Err(ProtocolError::InvalidLength);
            }
            self.target = Some(length + 4);
            self.bytes.reserve_exact(length);
        }
        let target = self.target.ok_or(ProtocolError::MalformedWire)?;
        let count = (target - self.bytes.len()).min(input.len() - consumed);
        self.bytes
            .extend_from_slice(&input[consumed..consumed + count]);
        Ok(consumed + count)
    }
    pub fn next_frame(
        &mut self,
        direction: Direction,
    ) -> Result<Option<pb::Envelope>, ProtocolError> {
        if self.target != Some(self.bytes.len()) {
            return Ok(None);
        }
        let envelope = decode_body(&self.bytes[4..], direction)?;
        self.bytes.clear();
        self.target = None;
        Ok(Some(envelope))
    }
}
pub fn encode_frame(
    envelope: &pb::Envelope,
    direction: Direction,
) -> Result<Vec<u8>, ProtocolError> {
    let length = envelope.encoded_len();
    if length == 0 || length > MAX_FRAME_BYTES {
        return Err(ProtocolError::InvalidLength);
    }
    let bytes = envelope.encode_to_vec();
    decode_body(&bytes, direction)?;
    let mut frame = Vec::with_capacity(bytes.len() + 4);
    frame.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
    frame.extend_from_slice(&bytes);
    Ok(frame)
}
pub fn decode_body(bytes: &[u8], direction: Direction) -> Result<pb::Envelope, ProtocolError> {
    if bytes.is_empty() || bytes.len() > MAX_FRAME_BYTES {
        return Err(ProtocolError::InvalidLength);
    }
    scan(bytes, ROOT, 1, TABLES)?;
    let message = pb::Envelope::decode(bytes).map_err(|_| ProtocolError::MalformedWire)?;
    if message.encode_to_vec() != bytes {
        return Err(ProtocolError::MalformedWire);
    }
    validate_semantics(&message, direction)?;
    Ok(message)
}
pub fn validate_identity(
    envelope: &pb::Envelope,
    operation: &[u8; 16],
    generation: u64,
) -> Result<(), ProtocolError> {
    if generation > 0
        && operation.iter().any(|b| *b != 0)
        && envelope.operation_id.as_deref() == Some(operation.as_slice())
        && envelope.generation == Some(generation)
    {
        Ok(())
    } else {
        Err(ProtocolError::IdentityMismatch)
    }
}
fn selector(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 1024
        && !value.chars().any(|c| c.is_whitespace() || c.is_control())
}
fn positive(value: Option<u64>, max: u64) -> bool {
    value.is_some_and(|n| n > 0 && n <= max)
}
fn count_bytes(count: Option<u64>, bytes: Option<u64>, max: u64) -> bool {
    matches!((count, bytes), (Some(count), Some(bytes)) if bytes <= max && count <= bytes && (count == 0) == (bytes == 0) && bytes <= count.saturating_mul(MAX_CHUNK_BYTES as u64))
}
fn transfer(id: Option<u64>, direction: Option<i32>) -> bool {
    match direction {
        Some(1) => id == Some(1),
        Some(2) => id.is_some_and(|v| (2..=1025).contains(&v)),
        _ => false,
    }
}
/// Validates individual messages only; ordering, credits and cumulative reconciliation remain stateful.
pub fn validate_semantics(
    envelope: &pb::Envelope,
    direction: Direction,
) -> Result<(), ProtocolError> {
    use pb::envelope::Body;
    let body = envelope
        .body
        .as_ref()
        .ok_or(ProtocolError::InvalidSemantics)?;
    let service = direction == Direction::ServiceToHelper;
    let allowed = match body {
        Body::Hello(_) | Body::Credit(_) => true,
        Body::Begin(_)
        | Body::InputEnd(_)
        | Body::Start(_)
        | Body::Cancel(_)
        | Body::ToolResult(_) => service,
        Body::Ready(_)
        | Body::SnapshotEnd(_)
        | Body::Terminal(_)
        | Body::ToolCall(_)
        | Body::ContextMeasured(_) => !service,
        Body::Chunk(v) => v.direction == Some(if service { 1 } else { 2 }),
    };
    if !allowed {
        return Err(ProtocolError::WrongDirection);
    }
    let identity = if matches!(body, Body::Hello(_)) {
        envelope.operation_id.is_none() && envelope.generation.is_none()
    } else {
        crate::id(&envelope.operation_id) && positive(envelope.generation, u64::MAX)
    };
    let valid = match body {
        Body::Hello(v) => {
            let inventory = if v.inventory_only == Some(true) {
                v.selected_model.as_deref() == Some("system")
                    && if service {
                        v.models.is_empty() && v.issues.is_empty()
                    } else {
                        !v.models.is_empty()
                            && v.models.len() <= 64
                            && ["system", "ollama", "coreai", "mlx"]
                                .iter()
                                .all(|provider| {
                                    v.models
                                        .iter()
                                        .filter(|row| row.provider.as_deref() == Some(*provider))
                                        .count()
                                        <= if *provider == "system" { 1 } else { 21 }
                                })
                            && v.issues.len() <= 4
                            && v.models.iter().all(|row| {
                                crate::inventory_entry(
                                    row.selector.as_deref(),
                                    row.provider.as_deref(),
                                    row.status,
                                    row.detail.as_deref(),
                                    256,
                                )
                            })
                            && v.issues.iter().all(|issue| {
                                crate::inventory_provider(issue.provider.as_deref())
                                    && crate::inventory_reason(issue.reason.as_deref())
                            })
                            && v.models.iter().enumerate().all(|(index, row)| {
                                !v.models[..index]
                                    .iter()
                                    .any(|other| other.selector == row.selector)
                            })
                            && v.models
                                .iter()
                                .any(|row| row.selector.as_deref() == Some("system"))
                    }
            } else {
                v.models.is_empty() && v.issues.is_empty()
            };
            let profile = match (v.supported_capabilities, v.capability_source) {
                (None, None) => true,
                (None, Some(4)) => {
                    !service
                        && v.inventory_only != Some(true)
                        && v.availability == Some(1)
                        && v.capabilities == Some(1)
                }
                (Some(mask), Some(1..=3)) => {
                    !service
                        && v.inventory_only != Some(true)
                        && v.availability == Some(1)
                        && mask <= 15
                        && ((mask & 1 != 0) == (v.capabilities == Some(3)))
                }
                _ => false,
            };
            let reasoning = v.reasoning_disabled.is_none_or(|disabled| {
                !service
                    && v.inventory_only != Some(true)
                    && v.availability == Some(1)
                    && (!disabled
                        || (v.supported_capabilities.is_some_and(|mask| mask & 4 != 0)
                            && v.selected_model
                                .as_deref()
                                .is_some_and(|name| name.starts_with("mlx:"))))
            });
            let shape = reasoning
                && profile
                && v.model_capabilities.is_none_or(|mask| {
                    service
                        && v.inventory_only != Some(true)
                        && mask <= 15
                        && v.selected_model
                            .as_deref()
                            .is_some_and(|model| model.starts_with("mlx:"))
                })
                && v.local_tool_destination.is_none_or(|local| {
                    !service && v.inventory_only != Some(true) && v.availability == Some(1) && local
                })
                && match (
                    v.context_tokens,
                    v.reported_context_tokens,
                    v.context_source,
                ) {
                    (Some(effective), Some(reported), Some(source @ 1..=4)) => {
                        !service
                            && v.inventory_only != Some(true)
                            && effective > 512
                            && reported >= effective
                            && match v
                                .selected_model
                                .as_deref()
                                .unwrap_or("system")
                                .to_lowercase()
                                .split_once(':')
                            {
                                None => {
                                    source == 1
                                        && v.selected_model.as_deref().unwrap_or("system")
                                            == "system"
                                }
                                Some(("coreai", _)) => source == 2,
                                Some(("mlx", _)) => source == 3,
                                Some(("ollama", _)) => source == 4,
                                _ => false,
                            }
                    }
                    (None, None, None) => true,
                    _ => false,
                }
                && v.inventory_only != Some(false)
                && inventory
                && v.selected_model.as_deref().is_none_or(selector)
                && v.asset_root.as_ref().is_none_or(|p| {
                    service
                        && p.starts_with('/')
                        && p.len() <= 4096
                        && !p.chars().any(char::is_control)
                })
                && v.endpoint.as_ref().is_none_or(|p| {
                    service && !p.is_empty() && p.len() <= 2048 && !p.chars().any(char::is_control)
                })
                && v.model_name.as_ref().is_none_or(|n| {
                    !n.is_empty() && n.len() <= 256 && !n.chars().any(char::is_control)
                })
                && (!service || v.model_name.is_none())
                && v.build_id.as_ref().is_some_and(|b| b.len() == 32)
                && v.schema_digest.as_ref().is_some_and(|b| b.len() == 32)
                && v.max_frame_bytes == Some(MAX_FRAME_BYTES as u32);
            shape
                && if service {
                    v.availability == Some(3)
                        && v.capabilities == Some(0)
                        && v.context_tokens.is_none()
                        && v.reported_context_tokens.is_none()
                        && v.context_source.is_none()
                        && v.reason == Some(0)
                } else if v.inventory_only == Some(true) {
                    v.availability == Some(3)
                        && v.capabilities == Some(0)
                        && v.context_tokens.is_none()
                        && v.reported_context_tokens.is_none()
                        && v.context_source.is_none()
                        && v.model_name.is_none()
                        && v.reason == Some(0)
                } else {
                    match v.availability {
                        Some(1) => {
                            matches!(v.capabilities, Some(1 | 3))
                                && v.context_tokens.is_some_and(|n| n > 0)
                                && v.reported_context_tokens.is_some()
                                && v.context_source.is_some()
                                && v.reason == Some(0)
                        }
                        Some(2 | 3) => {
                            v.capabilities == Some(0)
                                && v.context_tokens.is_none()
                                && v.reported_context_tokens.is_none()
                                && v.context_source.is_none()
                                && v.reason == Some(1)
                        }
                        _ => false,
                    }
                }
        }
        Body::ContextMeasured(v) => {
            matches!((v.input_tokens, v.capacity_tokens), (Some(n), Some(c)) if c > 0 && n <= c)
        }
        Body::Begin(v) => {
            v.model.as_deref().is_some_and(selector)
                && positive(v.input_bytes, MAX_INPUT_BYTES as u64)
                && v.deadline_remaining_ms
                    .is_none_or(|n| (1..=60_000).contains(&n))
                && v.max_response_tokens
                    .is_some_and(|n| (1..=2048).contains(&n))
        }
        Body::Chunk(v) => {
            transfer(v.transfer_id, v.direction)
                && v.data
                    .as_ref()
                    .is_some_and(|b| !b.is_empty() && b.len() <= MAX_CHUNK_BYTES)
                && if v.direction == Some(1) {
                    v.revision == Some(0) && v.ordinal.is_some_and(|n| n < MAX_INPUT_BYTES as u64)
                } else {
                    positive(v.revision, 1024) && v.ordinal.is_some_and(|n| n < MAX_SNAPSHOT_BYTES)
                }
        }
        Body::Credit(v) => {
            transfer(v.transfer_id, v.direction)
                && v.direction == Some(if service { 2 } else { 1 })
                && matches!((v.accepted_bytes,v.granted_bytes), (Some(a),Some(g)) if a <= if service { MAX_SNAPSHOT_BYTES } else { MAX_INPUT_BYTES as u64 } && g >= a && g - a <= 65_536)
        }
        Body::InputEnd(v) => {
            positive(v.count, MAX_INPUT_BYTES as u64)
                && count_bytes(v.count, v.total_bytes, MAX_INPUT_BYTES as u64)
        }
        Body::Ready(_) | Body::Start(_) => true,
        Body::Cancel(v) => v.reason.is_some_and(|n| (1..=4).contains(&n)),
        Body::SnapshotEnd(v) => {
            positive(v.revision, 1024) && count_bytes(v.count, v.total_bytes, MAX_SNAPSHOT_BYTES)
        }
        Body::ToolCall(v) => {
            v.ordinal.is_some_and(|n| (1..=8).contains(&n))
                && match &v.arguments {
                    Some(pb::tool_call::Arguments::ObserveStatus(_)) => true,
                    Some(pb::tool_call::Arguments::ListTools(_)) => true,
                    Some(pb::tool_call::Arguments::Shell(args)) => {
                        args.command
                            .as_ref()
                            .is_some_and(|c| !c.is_empty() && c.len() <= 8192 && !c.contains('\0'))
                            && args.cwd.as_ref().is_none_or(|p| {
                                !p.is_empty()
                                    && p.len() <= 1024
                                    && !p.starts_with('/')
                                    && !p.contains('\0')
                                    && !p.split('/').any(|c| c == "..")
                            })
                            && args.timeout_seconds.is_none_or(|n| (1..=60).contains(&n))
                    }

                    Some(pb::tool_call::Arguments::ReadAudit(args)) => {
                        args.limit.is_some_and(|n| (1..=16).contains(&n))
                    }
                    Some(pb::tool_call::Arguments::MemoryListNotes(args)) => {
                        args.after.as_ref().is_none_or(|id| id.len() <= 1024)
                            && args.limit.is_some()
                    }
                    Some(pb::tool_call::Arguments::MemoryGetNote(args)) => {
                        args.version.as_ref().is_some_and(|id| id.len() <= 1024)
                            && args.offset.is_some()
                            && args.limit.is_some()
                    }
                    Some(pb::tool_call::Arguments::MemoryCreateNote(args)) => {
                        args.body.as_ref().is_some_and(|body| body.len() <= 16_384)
                            && args
                                .source_version
                                .as_ref()
                                .is_none_or(|id| id.len() <= 1024)
                    }
                    Some(pb::tool_call::Arguments::MemoryNoteSources(args)) => {
                        args.version.as_ref().is_some_and(|id| id.len() <= 1024)
                    }
                    Some(pb::tool_call::Arguments::ReadFile(args)) => {
                        args.path
                            .as_ref()
                            .is_some_and(|p| !p.is_empty() && p.len() <= 1024)
                            && args.offset.is_some()
                            && args.limit.is_some_and(|n| (1..=16_384).contains(&n))
                    }
                    Some(pb::tool_call::Arguments::ListDirectory(args)) => args
                        .path
                        .as_ref()
                        .is_some_and(|p| !p.is_empty() && p.len() <= 1024),
                    None => false,
                }
        }
        Body::ToolResult(v) => {
            v.ordinal.is_some_and(|n| (1..=8).contains(&n))
                && v.status.is_some_and(|n| (1..=7).contains(&n))
                && v.text.as_ref().is_some_and(|text| text.len() <= 16_384)
                && v.truncated.is_some()
                && (v.status == Some(1) || v.next_offset.is_none())
        }
        Body::Terminal(v) => {
            let usage = match v.usage_known {
                Some(false) => v.usage_tokens.is_none(),
                Some(true) => v.usage_tokens.is_some_and(|tokens| tokens <= 2048),
                None => false,
            };
            usage
                && v.last_revision.is_some_and(|n| n <= 1024)
                && count_bytes(v.count, v.total_bytes, MAX_OUTPUT_BYTES)
                && match v.outcome {
                    Some(1) => positive(v.last_revision, 1024) && v.reason == Some(0),
                    Some(2 | 3) => v.reason.is_some_and(|n| (1..=12).contains(&n)),
                    _ => false,
                }
        }
    };
    if identity && valid {
        Ok(())
    } else {
        Err(ProtocolError::InvalidSemantics)
    }
}
pub fn encode_input(input: &pb::ModelInput) -> Result<Vec<u8>, ProtocolError> {
    validate_input(input)?;
    let bytes = input.encode_to_vec();
    if bytes.len() > MAX_INPUT_BYTES {
        return Err(ProtocolError::InvalidLength);
    }
    Ok(bytes)
}
pub fn decode_input(bytes: &[u8]) -> Result<pb::ModelInput, ProtocolError> {
    if bytes.is_empty() || bytes.len() > MAX_INPUT_BYTES {
        return Err(ProtocolError::InvalidLength);
    }
    scan(bytes, INPUT, 1, TABLES)?;
    let input = pb::ModelInput::decode(bytes).map_err(|_| ProtocolError::MalformedWire)?;
    if input.encode_to_vec() != bytes {
        return Err(ProtocolError::MalformedWire);
    }
    validate_input(&input)?;
    Ok(input)
}
pub fn validate_input(input: &pb::ModelInput) -> Result<(), ProtocolError> {
    let instructions = input
        .instructions
        .as_ref()
        .ok_or(ProtocolError::InvalidSemantics)?;
    let prompt = input
        .prompt
        .as_ref()
        .ok_or(ProtocolError::InvalidSemantics)?;
    if prompt.is_empty() || prompt.len() > 32 * 1024 || input.history.len() > 32 {
        return Err(ProtocolError::InvalidSemantics);
    }
    let mut bytes = instructions
        .len()
        .checked_add(prompt.len())
        .ok_or(ProtocolError::InvalidLength)?;
    for turn in &input.history {
        if !matches!(turn.role, Some(1 | 2)) {
            return Err(ProtocolError::InvalidSemantics);
        }
        let text = turn.text.as_ref().ok_or(ProtocolError::InvalidSemantics)?;
        bytes = bytes
            .checked_add(text.len())
            .ok_or(ProtocolError::InvalidLength)?;
    }
    if bytes > MAX_INPUT_BYTES || input.encoded_len() > MAX_INPUT_BYTES {
        Err(ProtocolError::InvalidLength)
    } else {
        Ok(())
    }
}
#[cfg(test)]
mod tests;
