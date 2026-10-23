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

/// The stored command, cwd, kind 28 command digest and `!!` flag of one
/// settlement.
struct Source {
    command: String,
    directory: String,
    digest: Hash,
    outside_sandbox: bool,
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
        // The command source is the model call's kind 14 or the person
        // input's kind 31: its stored command and cwd (J5-E5). A person
        // command's text is the rewrite, or the display-only copy of a
        // whole-command input; never a secret value.
        let (command, directory) = match &settlement.command {
            journal::JobCommand::Model(call) => (
                journal::exec_cmd_command(call).ok_or(NoticeError::Command)?,
                crate::job_control::exec_cmd_cwd(call).ok_or(NoticeError::Command)?,
            ),
            journal::JobCommand::Person(person) => {
                (person.command_text().to_owned(), person.cwd.clone())
            }
        };
        // The origin and the source agree: replay keeps a person intent on
        // its kind 31 (PR2), so a mismatch is inconsistent input.
        if matches!(listing.intent.origin, journal::JobOrigin::Person { .. })
            != settlement.command.person().is_some()
        {
            return Err(NoticeError::Command);
        }
        sources.push(Source {
            command,
            directory,
            digest,
            // Kind 31 field 7 bit 1 (PCR24); always false for kind 14.
            outside_sandbox: settlement.command.outside_sandbox(),
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
                outside_sandbox: source.outside_sandbox,
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
