pub mod checkpoint;
pub mod conversation;
#[cfg(feature = "test-support")]
pub mod fixture;
pub mod index;
pub mod journal;
pub mod writer;

use sha2::{Digest, Sha256};
use std::sync::atomic::{AtomicBool, Ordering};

/// One segment file: at most 8 MiB and 32,768 frames (segmented journal).
pub const SEGMENT_BYTES: usize = 8 * 1024 * 1024;
pub const SEGMENT_FRAMES: usize = 32_768;
pub const MAX_FRAME: usize = 64 * 1024;
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
pub(crate) fn active(cancelled: &AtomicBool) -> Result<(), ReplayError> {
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
/// The inspection summary of a replayed state.
pub fn summary(state: &conversation::Replay) -> Replay {
    Replay {
        installation_id: state.installation_id,
        revision: state.revision,
        owner_generation: state.owner_generation,
        binding_generation: state.binding.as_ref().map(|binding| binding.generation),
        state: if state.binding.is_some() {
            ReplayState::ActiveBinding
        } else {
            ReplayState::PendingInit
        },
    }
}
/// Replays one contiguous journal held in memory (segment file 1). Any error
/// discards the entire public result.
pub fn replay(bytes: &[u8], cancelled: &AtomicBool) -> Result<Replay, ReplayError> {
    active(cancelled)?;
    if bytes.is_empty() {
        return Err(ReplayError::Empty);
    }
    // Every journal starts with kind 10.
    if envelope(bytes)?.kind != 10 {
        return Err(ReplayError::UnsupportedFormat);
    }
    let state = conversation::replay(bytes, cancelled)?;
    Ok(summary(&state))
}

/// One strict format-1 envelope for conversation records.
pub(crate) struct Envelope<'a> {
    pub(crate) context: conversation::FrameContext,
    pub(crate) kind: u16,
    pub(crate) payload: &'a [u8],
    pub(crate) digest: [u8; 32],
    pub(crate) length: usize,
}
/// Decodes one frame read through a locator: the envelope must fill exactly
/// the located bytes and carry the located sequence.
pub(crate) fn verified_frame(
    bytes: &[u8],
    locator: &conversation::Locator,
) -> Result<(conversation::FrameContext, conversation::Record, [u8; 32]), ReplayError> {
    let envelope = envelope(bytes).map_err(|e| match e {
        ReplayError::UnsupportedFormat => e,
        _ => ReplayError::Corrupt,
    })?;
    if envelope.length != bytes.len()
        || envelope.length != locator.length as usize
        || envelope.context.sequence != locator.sequence
    {
        return Err(ReplayError::Corrupt);
    }
    let record = conversation::decode_record(envelope.kind, envelope.payload)?;
    Ok((envelope.context, record, envelope.digest))
}
/// [`verified_frame`] returning the record with its header command digest
/// at bytes 88..120, which [`envelope`] checked against SHA-256 of the kind
/// and payload (J5 notice manifest: the kind 28 source digest).
pub(crate) fn verified_command(
    bytes: &[u8],
    locator: &conversation::Locator,
) -> Result<([u8; 32], conversation::Record), ReplayError> {
    let (_, record, _) = verified_frame(bytes, locator)?;
    Ok((bytes[88..120].try_into().unwrap(), record))
}
pub(crate) fn verified_record(
    bytes: &[u8],
    locator: &conversation::Locator,
) -> Result<conversation::Record, ReplayError> {
    verified_frame(bytes, locator).map(|(_, record, _)| record)
}
pub(crate) fn envelope(bytes: &[u8]) -> Result<Envelope<'_>, ReplayError> {
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
