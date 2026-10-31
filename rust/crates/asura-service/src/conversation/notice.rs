//! The owner's J5 step: the `Prepare` notice inputs (`Prepared.notice`)
//! become the turn's job notice before composition (tool-job-controller.md,
//! J5 binding point and TJ-Q8; J5-E10).
//!
//! Pure and bounded: the writer read every frame on the journal worker
//! (AE-01); this runs on the reactor over at most
//! [`journal::MAX_NOTICE_SETTLEMENTS`] settlements, each re-encoding one
//! kind 28 of at most a few KiB and parsing one kind 14 argument object.
use crate::context::taint::Taint;
use crate::context::{Candidate, Notice, NoticeError, NoticeInputs, Settlement};
use asura_storage::authority::conversation::{
    self as journal, Hash, Id, JobNotice, PriorNotice, TurnAccepted, TurnTerminal,
};

/// The stored command, cwd, kind 28 command digest and `!!` flag of one
/// settlement.
struct Source {
    command: String,
    directory: String,
    digest: Hash,
    outside_sandbox: bool,
    /// The taint flag of the command source (T1).
    received_secret: bool,
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
            // The deterministic taint check of the command source (T1).
            received_secret: settlement.command.received_secret_values(),
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
                taint: Taint::from_flag(source.received_secret),
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

/// The composer's candidates from the `Prepare` history (newest first),
/// each with the notice it had when it ran (tool-job-controller.md,
/// notices in later turns; CCR15). `notices` holds the notice inputs of
/// the first `history` turns in the same order (`Prepared.history_notices`);
/// a turn past them is beyond the shown-turn bound and gets no notice.
/// Each notice is built from the writer's reference facts by the same
/// selection and checks as the turn's own notice, so a `NoticeError` is
/// the same journal repair case (J5-E10).
pub(super) fn candidates(
    conversation: Id,
    history: Vec<(TurnAccepted, TurnTerminal)>,
    notices: &[PriorNotice],
) -> Result<Vec<Candidate>, NoticeError> {
    let mut out = Vec::with_capacity(history.len());
    for (index, (prior, terminal)) in history.into_iter().enumerate() {
        let notice = match notices.get(index) {
            Some(inputs) => {
                // The inputs are the turn's own kind 21 facts.
                if inputs.through != prior.composition.notice_through
                    || inputs.live != prior.composition.notice_live
                {
                    return Err(NoticeError::Range);
                }
                prior_notice(conversation, inputs)?
            }
            None => Notice::none(conversation, prior.composition.notice_through),
        };
        out.push(Candidate {
            operation: prior.operation,
            generation: prior.generation,
            prompt: prior.prompt,
            answer: terminal.text,
            notice,
        });
    }
    Ok(out)
}

/// A prior turn's reference-only notice from its fixed-size facts.
fn prior_notice(conversation: Id, inputs: &PriorNotice) -> Result<Notice, NoticeError> {
    let mut settlements = Vec::with_capacity(inputs.settlements.len());
    for settlement in &inputs.settlements {
        if settlement.settled.job != settlement.job {
            return Err(NoticeError::Job);
        }
        settlements.push(Settlement {
            sequence: settlement.sequence,
            digest: settlement.digest,
            intent: &settlement.intent,
            settled: &settlement.settled,
            outside_sandbox: settlement.outside_sandbox,
            command: "",
            directory: "",
            taint: Taint::from_flag(settlement.received_secret),
        });
    }
    Notice::reference_only(&NoticeInputs {
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
