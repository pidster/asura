//! Job control messages of the tool job controller (tool-job-controller.md,
//! J4 control messages): presence, bounds, enum ranges and cross-field rules.
//! Domain errors are reply `error` tokens (J4-E4). Runtime-free.
use crate::{id, pb};
use pb::envelope::Body;
use prost::Message;

/// Entries in one `JobList` page; an absent `limit` means this many.
pub const MAX_JOB_LIST_ENTRIES: u32 = 8;
/// Encoded bytes of one `JobListReply`.
pub const MAX_JOB_LIST_REPLY_BYTES: usize = 40 * 1024;
/// Source bytes of one `JobRead` page, and delivered text bytes per stream page.
pub const MAX_JOB_PAGE_BYTES: u32 = 8_192;
/// Bytes of the command excerpt in a `JobEntry`: its first line.
pub const MAX_JOB_COMMAND_EXCERPT_BYTES: usize = 192;
/// Bytes of the whole stored command in a `JobDetail`.
pub const MAX_JOB_COMMAND_BYTES: usize = 8_192;
/// Bytes of a working directory in a `JobDetail`.
pub const MAX_JOB_CWD_BYTES: usize = 4_096;
/// Notes per job (kind 30).
pub const MAX_JOB_NOTES: u32 = 16;
/// UTF-8 bytes of one note.
pub const MAX_JOB_NOTE_BYTES: usize = 2_048;
/// Bytes of a brief block or pause reason text in a `JobEntry`.
pub const MAX_JOB_REASON_TEXT_BYTES: usize = 256;
/// IDs in `ConversationEvent.outlived_job_ids`; `outlived_jobs` may exceed it.
pub const MAX_OUTLIVED_JOB_IDS: usize = 8;
/// Unsettled jobs in one service (J2 fixed bound).
pub const MAX_LIVE_JOBS: u32 = 64;
/// The highest J2 block reason code (1 `recovering` to 9 `disk_space`).
pub const MAX_BLOCK_REASON: u32 = 9;
/// The highest job reason code: 20 `outside_sandbox_off` (person `!!`).
pub const MAX_JOB_REASON: u32 = 20;
/// Job reason 19 `execution_off` (EX11), a service cancel trigger.
pub const REASON_EXECUTION_OFF: u32 = 19;
/// Pause reasons of a paused capture: the kind 28 capture reason codes
/// 2 `quota` and 3 `enospc`.
pub const PAUSE_REASONS: [u32; 2] = [2, 3];

/// Reply `error` tokens (J4-E4).
pub mod error {
    /// No such job, or its project is unregistered.
    pub const NOT_FOUND: &str = "not_found";
    /// `expected_project_id` or `project_id` differs from the job's project.
    pub const PROJECT_MISMATCH: &str = "project_mismatch";
    /// `kill` for a model-origin job.
    pub const KILL_NOT_PERMITTED: &str = "kill_not_permitted";
    /// `delete` of a live job.
    pub const NOT_SETTLED: &str = "not_settled";
    /// 16 notes for the job.
    pub const NOTES_FULL: &str = "notes_full";
    /// 64 retention records for the job.
    pub const RETENTION_FULL: &str = "retention_full";
    /// Empty, too long, or a NUL or control byte other than tab and newline.
    pub const NOTE_INVALID: &str = "note_invalid";
    /// `jobs.clientViewers` pending requests, or the page pool's queue is full.
    pub const BUSY: &str = "busy";
    /// Journal or payload store unavailable.
    pub const UNAVAILABLE: &str = "unavailable";
    /// `before` names no kind 26 sequence in scope.
    pub const INVALID_CURSOR: &str = "invalid_cursor";
    /// Client-side only: a cancelled or lost mutation. Never on the wire.
    pub const OUTCOME_UNCONFIRMED: &str = "outcome_unconfirmed";
}

/// A person note: 1–2,048 UTF-8 bytes with no NUL and no control character
/// other than tab and newline. The service answers `note_invalid` otherwise;
/// the codec leaves that check to it so the token can be returned.
pub fn note_text_valid(text: &str) -> bool {
    !text.is_empty()
        && text.len() <= MAX_JOB_NOTE_BYTES
        && !text
            .chars()
            .any(|c| c.is_control() && c != '\t' && c != '\n')
}

pub(crate) fn handles(body: &Body) -> bool {
    matches!(
        body,
        Body::JobList(_)
            | Body::JobListReply(_)
            | Body::JobInspect(_)
            | Body::JobDetail(_)
            | Body::JobRead(_)
            | Body::JobPage(_)
            | Body::JobObserve(_)
            | Body::JobCancel(_)
            | Body::JobCancelReply(_)
            | Body::JobRetain(_)
            | Body::JobRetainReply(_)
    )
}
pub(crate) fn is_request(body: &Body) -> bool {
    matches!(
        body,
        Body::JobList(_)
            | Body::JobInspect(_)
            | Body::JobRead(_)
            | Body::JobObserve(_)
            | Body::JobCancel(_)
            | Body::JobRetain(_)
    )
}

fn optional_id(value: &Option<Vec<u8>>) -> bool {
    value.is_none() || id(value)
}
fn token(value: &Option<String>, allowed: &[&str]) -> bool {
    value.as_deref().is_some_and(|v| allowed.contains(&v))
}

pub(crate) fn valid(body: &Body) -> bool {
    use error::*;
    match body {
        Body::JobList(v) => list(v),
        Body::JobListReply(v) => {
            if v.error.is_some() {
                token(&v.error, &[NOT_FOUND, BUSY, UNAVAILABLE, INVALID_CURSOR])
                    && v.entries.is_empty()
                    && v.next_before.is_none()
                    && v.revision.is_none()
                    && v.live_in_scope.is_none()
                    && v.live_in_service.is_none()
                    && v.execution_enabled.is_none()
            } else {
                list_reply(v)
            }
        }
        Body::JobInspect(v) => id(&v.job_id) && optional_id(&v.project_id),
        Body::JobDetail(v) => {
            if v.error.is_some() {
                token(&v.error, &[NOT_FOUND, PROJECT_MISMATCH, BUSY, UNAVAILABLE])
                    && v.entry.is_none()
                    && v.command.is_none()
                    && v.cwd.is_none()
                    && v.notes.is_empty()
                    && v.path_warnings.is_empty()
                    && v.path_overflow.is_none()
            } else {
                v.entry.as_ref().is_some_and(entry)
                    && v.command
                        .as_ref()
                        .is_some_and(|c| c.len() <= MAX_JOB_COMMAND_BYTES && !c.contains('\0'))
                    && v.cwd.as_ref().is_none_or(|c| {
                        c.starts_with('/') && c.len() <= MAX_JOB_CWD_BYTES && !c.contains('\0')
                    })
                    && v.notes.len() <= MAX_JOB_NOTES as usize
                    && v.notes.iter().all(|n| {
                        matches!(n.author, Some(1 | 2))
                            && n.unix_ms.is_some()
                            && n.text.as_deref().is_some_and(note_text_valid)
                    })
                    && crate::path_guard::warnings_valid(&v.path_warnings, v.path_overflow)
            }
        }
        Body::JobRead(v) => {
            id(&v.job_id)
                && optional_id(&v.project_id)
                && v.stream.is_some_and(|s| (1..=2).contains(&s))
                && v.limit.is_none_or(|n| (1..=MAX_JOB_PAGE_BYTES).contains(&n))
                // `at_end` reads the last page up to the watermark instead of
                // an offset; otherwise the offset is explicit.
                && if v.at_end == Some(true) {
                    v.offset.is_none()
                } else {
                    v.offset.is_some()
                }
        }
        Body::JobObserve(v) => {
            id(&v.job_id)
                && optional_id(&v.project_id)
                && v.after_state.is_none_or(|s| (1..=9).contains(&s))
                && match v.stream {
                    // One stream: the bytes the client holds of it.
                    Some(1 | 2) => v.offset.is_some(),
                    // Both streams: only at the end.
                    None => v.at_end == Some(true) && v.offset.is_none(),
                    _ => false,
                }
        }
        Body::JobPage(v) => {
            if v.error.is_some() {
                token(&v.error, &[NOT_FOUND, PROJECT_MISMATCH, BUSY, UNAVAILABLE])
                    && v.entry.is_none()
                    && v.pages.is_empty()
            } else {
                v.entry.as_ref().is_some_and(entry)
                    && (1..=2).contains(&v.pages.len())
                    && v.pages.iter().all(stream_page)
                    && (v.pages.len() == 1 || v.pages[0].stream != v.pages[1].stream)
            }
        }
        Body::JobCancel(v) => id(&v.request_id) && id(&v.job_id) && id(&v.expected_project_id),
        Body::JobCancelReply(v) => {
            id(&v.request_id)
                && if v.error.is_some() {
                    token(
                        &v.error,
                        &[NOT_FOUND, PROJECT_MISMATCH, KILL_NOT_PERMITTED, UNAVAILABLE],
                    ) && v.entry.is_none()
                        && v.requested.is_none()
                } else {
                    v.entry.as_ref().is_some_and(entry)
                        && match v.requested {
                            Some(true) => true,
                            // Not requested only because the job had settled.
                            Some(false) => v
                                .entry
                                .as_ref()
                                .is_some_and(|e| e.state == Some(pb::JobState::Settled as i32)),
                            None => false,
                        }
                }
        }
        Body::JobRetain(v) => {
            id(&v.request_id)
                && id(&v.job_id)
                && id(&v.expected_project_id)
                && match v.action {
                    // The note's text rule is the service's `note_invalid`.
                    Some(4) => v.note.is_some() && v.confirmed.is_none(),
                    // Delete needs the person's confirmation.
                    Some(3) => v.note.is_none() && v.confirmed == Some(true),
                    Some(1 | 2) => v.note.is_none() && v.confirmed.is_none(),
                    _ => false,
                }
        }
        Body::JobRetainReply(v) => {
            id(&v.request_id)
                && if v.error.is_some() {
                    token(
                        &v.error,
                        &[
                            NOT_FOUND,
                            PROJECT_MISMATCH,
                            NOT_SETTLED,
                            NOTES_FULL,
                            RETENTION_FULL,
                            NOTE_INVALID,
                            UNAVAILABLE,
                        ],
                    ) && v.entry.is_none()
                        && v.changed.is_none()
                } else {
                    v.entry.as_ref().is_some_and(entry) && v.changed.is_some()
                }
        }
        _ => false,
    }
}

fn list(v: &pb::JobList) -> bool {
    let ids = match v.scope {
        Some(1) => id(&v.project_id) && v.conversation_id.is_none(),
        Some(2) => v.project_id.is_none() && v.conversation_id.is_none(),
        Some(3) => id(&v.project_id) && id(&v.conversation_id),
        _ => false,
    };
    ids && v.before.is_none_or(|n| n > 0)
        && v.limit
            .is_none_or(|n| (1..=MAX_JOB_LIST_ENTRIES).contains(&n))
}

fn list_reply(v: &pb::JobListReply) -> bool {
    v.entries.len() <= MAX_JOB_LIST_ENTRIES as usize
        && v.encoded_len() <= MAX_JOB_LIST_REPLY_BYTES
        && v.revision.is_some_and(|n| n > 0)
        && v.live_in_service.is_some_and(|n| n <= MAX_LIVE_JOBS)
        && v.live_in_scope.is_some_and(|n| Some(n) <= v.live_in_service)
        && v.entries.iter().all(entry)
        // Newest first by kind 26 sequence; one entry per job.
        && v.entries.windows(2).all(|w| w[0].sequence > w[1].sequence)
        && v.entries.iter().enumerate().all(|(i, e)| {
            v.entries[..i].iter().all(|p| p.job_id != e.job_id)
        })
        && v.next_before.is_none_or(|n| {
            v.entries.last().is_some_and(|last| last.sequence == Some(n))
        })
}

/// One job's summary: live facts while unsettled, kind 28 facts once settled.
pub(crate) fn entry(v: &pb::JobEntry) -> bool {
    let Some(state) = v.state else {
        return false;
    };
    let settled = state == pb::JobState::Settled as i32;
    let person = v.origin == Some(pb::JobOrigin::Person as i32);
    let reason_block = match (state, v.block_reason) {
        // A queued job always shows why it waits (AQ1).
        (1, Some(n)) => (1..=MAX_BLOCK_REASON).contains(&n),
        (1, None) => false,
        // A paused capture shows its pause reason: 2 quota or 3 enospc.
        (4, Some(n)) => PAUSE_REASONS.contains(&n),
        (4, None) => true,
        (_, reason) => reason.is_none(),
    };
    // The shell's kind 5 exit of a released `!!` job (DRAINING with
    // `shell_exited`); every other unsettled job has no exit.
    let draining_exit = state == pb::JobState::Draining as i32
        && person
        && v.outside_sandbox == Some(true)
        && v.shell_exited == Some(true)
        && matches!(v.exit_kind, Some(1 | 2));
    let exit = match (v.exit_kind, v.exit_value) {
        (None, None) => true,
        (Some(0), Some(0)) => true,
        (Some(1), Some(n)) => n <= 255,
        (Some(2), Some(n)) => (1..=127).contains(&n),
        _ => false,
    };
    id(&v.job_id)
        && id(&v.project_id)
        && id(&v.conversation_id)
        && matches!(v.origin, Some(1 | 2))
        // Only a person `!!` job runs outside the sandbox.
        && v.outside_sandbox.is_none_or(|outside| !outside || person)
        && v.command.as_ref().is_some_and(|c| {
            c.len() <= MAX_JOB_COMMAND_EXCERPT_BYTES && !c.contains(['\n', '\0'])
        })
        && (1..=9).contains(&state)
        && reason_block
        && match &v.reason_text {
            None => true,
            Some(text) => {
                v.block_reason.is_some()
                    && !text.is_empty()
                    && text.len() <= MAX_JOB_REASON_TEXT_BYTES
                    && !text.chars().any(char::is_control)
            }
        }
        && v.reason.is_none_or(|n| n <= MAX_JOB_REASON)
        && exit
        && if settled {
            v.outcome.is_some_and(|n| (1..=5).contains(&n))
                && v.reason.is_some()
                && v.exit_kind.is_some()
                && v.capture_state.is_some_and(|n| (1..=5).contains(&n))
        } else {
            v.outcome.is_none()
                && (v.exit_kind.is_none() || draining_exit)
                && v.capture_state.is_none()
                && v.stdout_lines.is_none()
                && v.stderr_lines.is_none()
                && v.settled_unix_ms.is_none()
                // Output is deleted only after settlement.
                && v.deleted != Some(true)
        }
        && v.notes.is_none_or(|n| n <= MAX_JOB_NOTES)
        && v.sequence.is_some_and(|n| n > 0)
        && v.input_id.as_ref().is_none_or(|_| person && id(&v.input_id))
        // OD2 fields: a person `!!` job only. Survivors left running are
        // known after kind 6 (DRAINING) or at settlement; `shell_exited` is
        // set, only true, while the released job drains.
        && ((v.left_running.is_none() && v.shell_exited.is_none())
            || (person && v.outside_sandbox == Some(true)))
        && v.left_running.is_none_or(|_| state == 6 || settled)
        && v.shell_exited.is_none_or(|exited| exited && state == 6)
}

fn stream_page(v: &pb::JobStreamPage) -> bool {
    let (Some(start), Some(next), Some(total), Some(state), Some(text)) = (
        v.start,
        v.next_offset,
        v.total_bytes,
        v.state,
        v.text.as_ref(),
    ) else {
        return false;
    };
    let consumed = next.checked_sub(start);
    matches!(v.stream, Some(1 | 2))
        && (1..=8).contains(&state)
        && text.len() <= MAX_JOB_PAGE_BYTES as usize
        && consumed.is_some_and(|n| n <= u64::from(MAX_JOB_PAGE_BYTES))
        && next <= total
        // Expired, missing, corrupt and deleted pages carry no text.
        && (state < 5 || (consumed == Some(0) && text.is_empty()))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn note_text_rule_matches_the_note_invalid_token() {
        assert!(note_text_valid("a"));
        assert!(note_text_valid("line\n\tindented"));
        assert!(note_text_valid(&"é".repeat(1024)));
        assert!(!note_text_valid(""));
        assert!(!note_text_valid(&"a".repeat(MAX_JOB_NOTE_BYTES + 1)));
        assert!(!note_text_valid("a\0b"));
        assert!(!note_text_valid("a\rb"));
        assert!(!note_text_valid("a\u{1b}[31m"));
        assert!(!note_text_valid("a\u{85}b"));
    }
    fn outside_entry(state: pb::JobState) -> pb::JobEntry {
        let settled = state == pb::JobState::Settled;
        pb::JobEntry {
            job_id: Some(vec![1; 16]),
            project_id: Some(vec![2; 16]),
            conversation_id: Some(vec![3; 16]),
            origin: Some(pb::JobOrigin::Person as i32),
            outside_sandbox: Some(true),
            command: Some("make serve &".into()),
            state: Some(state as i32),
            sequence: Some(4),
            input_id: Some(vec![5; 16]),
            outcome: settled.then_some(1),
            reason: settled.then_some(0),
            exit_kind: settled.then_some(1),
            exit_value: settled.then_some(0),
            capture_state: settled.then_some(1),
            ..Default::default()
        }
    }
    /// `JobEntry` fields 28 and 29 (OD2): only a person `!!` job; survivors
    /// in DRAINING or SETTLED; `shell_exited` only true and only DRAINING;
    /// the shell's exit (fields 12 and 13) in DRAINING only with
    /// `shell_exited`.
    #[test]
    fn released_outside_job_fields() {
        use pb::JobState::{Draining, Running, Settled};
        let draining = pb::JobEntry {
            left_running: Some(2),
            shell_exited: Some(true),
            ..outside_entry(Draining)
        };
        assert!(entry(&draining));
        let settled = pb::JobEntry {
            left_running: Some(2),
            ..outside_entry(Settled)
        };
        assert!(entry(&settled));
        assert!(entry(&pb::JobEntry {
            shell_exited: None,
            ..draining.clone()
        }));
        // The shell's exit while the released job drains.
        let exited = pb::JobEntry {
            exit_kind: Some(1),
            exit_value: Some(0),
            ..draining.clone()
        };
        assert!(entry(&exited));
        assert!(entry(&pb::JobEntry {
            exit_kind: Some(2),
            exit_value: Some(15),
            ..draining.clone()
        }));
        for (name, changed) in [
            (
                "exit without shell_exited",
                pb::JobEntry {
                    shell_exited: None,
                    ..exited.clone()
                },
            ),
            (
                "exit while running",
                pb::JobEntry {
                    exit_kind: Some(1),
                    exit_value: Some(0),
                    ..outside_entry(Running)
                },
            ),
            (
                "exit in a sandboxed person job",
                pb::JobEntry {
                    outside_sandbox: Some(false),
                    ..exited.clone()
                },
            ),
            (
                "exit kind 0 while draining",
                pb::JobEntry {
                    exit_kind: Some(0),
                    exit_value: Some(0),
                    ..draining.clone()
                },
            ),
            (
                "exit kind without value",
                pb::JobEntry {
                    exit_value: None,
                    ..exited.clone()
                },
            ),
            (
                "exit value out of range",
                pb::JobEntry {
                    exit_value: Some(256),
                    ..exited.clone()
                },
            ),
            (
                "model origin",
                pb::JobEntry {
                    origin: Some(pb::JobOrigin::Model as i32),
                    input_id: None,
                    outside_sandbox: None,
                    ..draining.clone()
                },
            ),
            (
                "sandboxed person",
                pb::JobEntry {
                    outside_sandbox: Some(false),
                    ..draining.clone()
                },
            ),
            (
                "survivors while running",
                pb::JobEntry {
                    left_running: Some(1),
                    ..outside_entry(Running)
                },
            ),
            (
                "shell exited while running",
                pb::JobEntry {
                    shell_exited: Some(true),
                    ..outside_entry(Running)
                },
            ),
            (
                "shell exited when settled",
                pb::JobEntry {
                    shell_exited: Some(true),
                    ..settled.clone()
                },
            ),
            (
                "shell exited false",
                pb::JobEntry {
                    shell_exited: Some(false),
                    ..draining.clone()
                },
            ),
        ] {
            assert!(!entry(&changed), "{name}");
        }
    }
    #[test]
    fn id_slice_rejects_zero_and_wrong_length() {
        use crate::id_slice;
        assert!(id_slice(&[1; 16]));
        assert!(!id_slice(&[0; 16]));
        assert!(!id_slice(&[1; 15]));
    }
}
