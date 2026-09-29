pub mod conversation;
pub mod writer;

use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

pub const MAX_JOURNAL: usize = 8 * 1024 * 1024;
pub const MAX_FRAME: usize = 64 * 1024;
pub const MAX_FRAMES: usize = 32_768;
const HEADER: usize = 144;
const TRAILER: usize = 40;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReplayState {
    PendingInit,
    ActiveBinding,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Replay {
    pub installation_id: [u8; 16],
    pub revision: u64,
    pub owner_generation: u64,
    pub binding_generation: Option<u64>,
    pub state: ReplayState,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReplayError {
    Empty,
    IncompleteTail,
    UnsupportedFormat,
    Corrupt,
    Limit,
    Timeout,
    Cancelled,
}
impl std::fmt::Display for ReplayError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for ReplayError {}
fn active(deadline: Instant, cancelled: &AtomicBool) -> Result<(), ReplayError> {
    if cancelled.load(Ordering::Acquire) {
        Err(ReplayError::Cancelled)
    } else if Instant::now() >= deadline {
        Err(ReplayError::Timeout)
    } else {
        Ok(())
    }
}
fn u64_at(bytes: &[u8], offset: usize) -> u64 {
    u64::from_be_bytes(
        bytes[offset..offset + 8]
            .try_into()
            .expect("validated fixed-width field"),
    )
}
fn id_at(bytes: &[u8], offset: usize) -> [u8; 16] {
    bytes[offset..offset + 16]
        .try_into()
        .expect("validated fixed-width field")
}
/// Replays complete format-1 frames. Any error discards the entire public result.
/// The input plus the bounded operation-ID set stay below the 16 MiB replay budget.
pub fn replay(
    bytes: &[u8],
    deadline: Instant,
    cancelled: &AtomicBool,
) -> Result<Replay, ReplayError> {
    active(deadline, cancelled)?;
    if bytes.len() > MAX_JOURNAL {
        return Err(ReplayError::Limit);
    }
    if bytes.is_empty() {
        return Err(ReplayError::Empty);
    }
    // Distinct initialization kinds share format 1 without reinterpreting payloads.
    if envelope(bytes)?.kind == 10 {
        let state = conversation::replay(bytes, deadline, cancelled)?;
        return Ok(Replay {
            installation_id: state.installation_id,
            revision: state.revision,
            owner_generation: state.owner_generation,
            binding_generation: state.binding.as_ref().map(|binding| binding.generation),
            state: if state.binding.is_some() {
                ReplayState::ActiveBinding
            } else {
                ReplayState::PendingInit
            },
        });
    }
    let mut ids = HashSet::new();
    ids.try_reserve(MAX_FRAMES)
        .map_err(|_| ReplayError::Limit)?;
    let mut offset = 0;
    let mut count = 0u64;
    let mut prior_digest = [0; 32];
    let mut installation = [0; 16];
    let mut revision = 0u64;
    let mut owner = 0u64;
    let mut graph = [0; 16];
    let mut configuration = [0; 32];
    let mut binding = None;
    while offset < bytes.len() {
        active(deadline, cancelled)?;
        if count >= MAX_FRAMES as u64 {
            return Err(ReplayError::Limit);
        }
        let remaining = &bytes[offset..];
        let envelope = envelope(remaining)?;
        let length = envelope.length;
        let frame = &remaining[..length];
        let sequence = envelope.context.sequence;
        let expected = envelope.context.expected_revision;
        let resulting = u64_at(frame, 128);
        let generation = envelope.context.owner_generation;
        let iid = envelope.context.installation_id;
        let operation = envelope.context.transition_id;
        if sequence != count.checked_add(1).ok_or(ReplayError::Corrupt)?
            || envelope.context.prior_digest != prior_digest
            || !ids.insert(operation)
            || expected != revision
            || (count != 0 && iid != installation)
        {
            return Err(ReplayError::Corrupt);
        }
        let payload = envelope.payload;
        let digest = envelope.digest;
        let kind = envelope.kind;
        match kind {
            1 => {
                if count != 0
                    || generation != 1
                    || payload.len() != 57
                    || ![1, 2].contains(&payload[0])
                    || u64_at(payload, 1) == 0
                    || id_at(payload, 41) == [0; 16]
                {
                    return Err(ReplayError::Corrupt);
                }
                graph = id_at(payload, 41);
                configuration.copy_from_slice(&payload[9..41]);
            }
            2 => {
                if count == 0
                    || binding.is_some()
                    || generation != owner
                    || payload.len() != 56
                    || u64_at(payload, 0) != 1
                    || id_at(payload, 8) != graph
                    || payload[24..56] != configuration
                {
                    return Err(ReplayError::Corrupt);
                }
                binding = Some(1);
            }
            3 => {
                if count == 0
                    || !payload.is_empty()
                    || generation != owner.checked_add(1).ok_or(ReplayError::Corrupt)?
                {
                    return Err(ReplayError::Corrupt);
                }
            }
            _ => return Err(ReplayError::UnsupportedFormat),
        }
        installation = iid;
        revision = resulting;
        owner = generation;
        prior_digest = digest;
        count = sequence;
        offset += length;
    }
    active(deadline, cancelled)?;
    Ok(Replay {
        installation_id: installation,
        revision,
        owner_generation: owner,
        binding_generation: binding,
        state: if binding.is_some() {
            ReplayState::ActiveBinding
        } else {
            ReplayState::PendingInit
        },
    })
}

/// One strict format-1 envelope for diagnostic and conversation records.
struct Envelope<'a> {
    context: conversation::FrameContext,
    kind: u16,
    payload: &'a [u8],
    digest: [u8; 32],
    length: usize,
}
fn envelope(bytes: &[u8]) -> Result<Envelope<'_>, ReplayError> {
    if bytes.len() < HEADER {
        return Err(ReplayError::IncompleteTail);
    }
    if &bytes[..8] != b"ASURAJ01" {
        return Err(ReplayError::Corrupt);
    }
    if bytes[8..10] != 1u16.to_be_bytes() {
        return Err(ReplayError::UnsupportedFormat);
    }
    let length = u32::from_be_bytes(bytes[12..16].try_into().unwrap()) as usize;
    if length > MAX_FRAME {
        return Err(ReplayError::Limit);
    }
    if length < HEADER + TRAILER {
        return Err(ReplayError::Corrupt);
    }
    if bytes.len() < length {
        return Err(ReplayError::IncompleteTail);
    }
    let frame = &bytes[..length];
    let hash_end = length - TRAILER;
    let digest: [u8; 32] = Sha256::digest(&frame[..hash_end]).into();
    if frame[hash_end..hash_end + 32] != digest || &frame[length - 8..] != b"ASURAC01" {
        return Err(ReplayError::Corrupt);
    }
    let kind = u16::from_be_bytes(frame[10..12].try_into().unwrap());
    let payload = &frame[HEADER..hash_end];
    let mut command = Sha256::new();
    command.update(&frame[10..12]);
    command.update(payload);
    if frame[88..120] != command.finalize()[..] {
        return Err(ReplayError::Corrupt);
    }
    let expected = u64_at(frame, 120);
    let installation_id = id_at(frame, 56);
    let transition_id = id_at(frame, 72);
    let owner_generation = u64_at(frame, 136);
    if installation_id == [0; 16]
        || transition_id == [0; 16]
        || owner_generation == 0
        || u64_at(frame, 128) != expected.checked_add(1).ok_or(ReplayError::Corrupt)?
    {
        return Err(ReplayError::Corrupt);
    }
    Ok(Envelope {
        context: conversation::FrameContext {
            installation_id,
            transition_id,
            sequence: u64_at(frame, 16),
            prior_digest: frame[24..56].try_into().unwrap(),
            expected_revision: expected,
            owner_generation,
        },
        kind,
        payload,
        digest,
        length,
    })
}
