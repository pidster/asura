pub mod conversation;
#[cfg(feature = "test-support")]
pub mod fixture;
pub mod writer;

use sha2::{Digest, Sha256};
use std::sync::atomic::{AtomicBool, Ordering};

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
    Cancelled,
}
impl std::fmt::Display for ReplayError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for ReplayError {}
/// Replay has no time bound (J1-0); only the cancel flag stops it.
fn active(cancelled: &AtomicBool) -> Result<(), ReplayError> {
    if cancelled.load(Ordering::Acquire) {
        Err(ReplayError::Cancelled)
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
pub fn replay(bytes: &[u8], cancelled: &AtomicBool) -> Result<Replay, ReplayError> {
    active(cancelled)?;
    if bytes.len() > MAX_JOURNAL {
        return Err(ReplayError::Limit);
    }
    if bytes.is_empty() {
        return Err(ReplayError::Empty);
    }
    // Every journal starts with kind 10. Kind 3 owner generations and all
    // later records replay through the conversation authority.
    if envelope(bytes)?.kind != 10 {
        return Err(ReplayError::UnsupportedFormat);
    }
    let state = conversation::replay(bytes, cancelled)?;
    Ok(Replay {
        installation_id: state.installation_id,
        revision: state.revision,
        owner_generation: state.owner_generation,
        binding_generation: state.binding.as_ref().map(|binding| binding.generation),
        state: if state.binding.is_some() {
            ReplayState::ActiveBinding
        } else {
            ReplayState::PendingInit
        },
    })
}

/// One strict format-1 envelope for conversation records.
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
