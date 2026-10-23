//! The owner's J5 step: the `Prepare` notice inputs (`Prepared.notice`)
//! become the turn's job notice before composition (tool-job-controller.md,
//! J5 binding point and TJ-Q8; J5-E10).
//!
//! Pure and bounded: the writer read every frame on the journal worker
//! (AE-01); this runs on the reactor over at most
//! [`journal::MAX_NOTICE_SETTLEMENTS`] settlements, each re-encoding one
//! kind 28 of at most a few KiB and parsing one kind 14 argument object.
use crate::context::{Notice, NoticeError, NoticeInputs, Settlement};
use asura_storage::authority::conversation::{self as journal, Hash, Id, JobNotice, JobSettled};

/// The stored command, cwd and kind 28 command digest of one settlement.
struct Source {
    command: String,
    directory: String,
    digest: Hash,
}

/// The notice of a turn of `conversation` from its `Prepare` notice
/// inputs. A `NoticeError` means the inputs disagree with rules that replay
/// already enforced (J5-E10): the caller fails the turn as a journal repair
/// case and never drops the notice.
pub(super) fn build(conversation: Id, inputs: &JobNotice) -> Result<Notice, NoticeError> {
    let mut sources = Vec::with_capacity(inputs.settlements.len());
    for settlement in &inputs.settlements {
        let listing = &settlement.listing;
        if listing.conversation != conversation {
            return Err(NoticeError::Job);
        }
        let settled = listing.settled.as_deref().ok_or(NoticeError::Job)?;
        // The command source is the model call's kind 14 (a person job's
        // kind 31 after P2): its stored command and cwd (J5-E5).
        let command = journal::exec_cmd_command(&settlement.command).ok_or(NoticeError::Command)?;
        let directory =
            crate::job_control::exec_cmd_cwd(&settlement.command).ok_or(NoticeError::Command)?;
        sources.push(Source {
            command,
            directory,
            digest: settled_digest(settled)?,
        });
    }
    let settlements: Vec<Settlement<'_>> = inputs
        .settlements
        .iter()
        .zip(&sources)
        .filter_map(|(settlement, source)| {
            Some(Settlement {
                sequence: settlement.listing.settled_frame.sequence,
                digest: source.digest,
                intent: &settlement.listing.intent,
                settled: settlement.listing.settled.as_deref()?,
                // Kind 14 sources are model jobs; kind 31 field 7 bit 1
                // marks a `!!` person job once P2 records it.
                outside_sandbox: false,
                command: &source.command,
                directory: &source.directory,
            })
        })
        .collect();
    Notice::build(&NoticeInputs {
        conversation,
        previous: inputs.after,
        through: inputs.through,
        settlements: &settlements,
        live: inputs.live,
    })
}

/// The command digest of a kind 28 frame header: SHA-256 of the two-byte
/// kind and the exact payload bytes (persistence-recovery.md, format 1
/// byte contract). The canonical codec writes it at header bytes 88..120.
///
/// Interim (J5-E design gap): `Prepare` does not return the frame header
/// digest, so the owner re-encodes the decoded kind 28 with the one codec.
/// Format 1 decoding is strict, so the payload bytes are the stored ones.
fn settled_digest(settled: &JobSettled) -> Result<Hash, NoticeError> {
    let frame = journal::encode_frame(
        &journal::FrameContext {
            installation_id: [1; 16],
            transition_id: [1; 16],
            sequence: 1,
            prior_digest: [0; 32],
            expected_revision: 0,
            owner_generation: 1,
        },
        &journal::Record::JobSettled(Box::new(settled.clone())),
    )
    .map_err(|_| NoticeError::Job)?;
    frame
        .get(COMMAND_DIGEST)
        .and_then(|digest| digest.try_into().ok())
        .ok_or(NoticeError::Job)
}

/// Header bytes of the command digest: after magic (8), version and kind
/// (2 each), length (4), sequence (8), previous digest (32), installation
/// (16) and operation (16).
const COMMAND_DIGEST: std::ops::Range<usize> = 88..120;

#[cfg(test)]
#[path = "notice_tests.rs"]
mod tests;
