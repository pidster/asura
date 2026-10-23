//! The owner's J5 step: the `Prepare` notice inputs (`Prepared.notice`)
//! become the turn's job notice before composition (tool-job-controller.md,
//! J5 binding point and TJ-Q8; J5-E10).
//!
//! Pure and bounded: the writer read every frame on the journal worker
//! (AE-01); this runs on the reactor over at most
//! [`journal::MAX_NOTICE_SETTLEMENTS`] settlements, each re-encoding one
//! kind 28 of at most a few KiB and parsing one kind 14 argument object.
use crate::context::{Notice, NoticeError, NoticeInputs, Settlement};
use asura_storage::authority::conversation::{self as journal, Hash, Id, JobNotice};

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
        // The kind 28 header's command digest, read with the frame.
        let digest = listing
            .settled
            .as_ref()
            .and(listing.settled_digest)
            .ok_or(NoticeError::Job)?;
        // The command source is the model call's kind 14 (a person job's
        // kind 31 after P2): its stored command and cwd (J5-E5).
        let command = journal::exec_cmd_command(&settlement.command).ok_or(NoticeError::Command)?;
        let directory =
            crate::job_control::exec_cmd_cwd(&settlement.command).ok_or(NoticeError::Command)?;
        sources.push(Source {
            command,
            directory,
            digest,
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

#[cfg(test)]
#[path = "notice_tests.rs"]
mod tests;
