//! Provider-independent tool contracts. Validation grants no host IO by itself.
//!
//! The conversation owner must persist intent before dispatch and persist results
//! before delivering them to a model. Host IO is isolated in the retained executor;
//! this module never performs inference.
use std::time::Instant;

mod parse;
use asura_storage::authority::conversation as journal;
pub use parse::{
    DEFAULT_YIELD_SECONDS, MAX_COMMAND_BYTES, MAX_WAIT_SECONDS, MAX_YIELD_SECONDS, Parsed,
    job_handle, parse_call,
};

/// Calls that may be recorded per turn; the next ordinal is only the
/// over-limit `limit` record (TCR-D6, TCR-D9; owner, 2026-10-18).
pub const MAX_CALLS: u32 = journal::MAX_RECORDED_CALLS;
// The wire codec and the journal must agree on the call limit (TCR-D32).
const _: () = assert!(MAX_CALLS == asura_control::model::MAX_RECORDED_TOOL_CALLS);
const _: () = assert!(journal::MAX_TOOL_CALL_ORDINAL == asura_control::model::MAX_TOOL_ORDINAL);
/// Each result's limit. A turn has no result-byte total (owner, 2026-10-18).
pub const MAX_RESULT_BYTES: usize = 16_384;
pub const MAX_PATH_BYTES: usize = 1_024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Definition {
    pub name: &'static str,
    pub description: &'static str,
}
const OPERATIONS: &[Definition] = &[
    Definition {
        name: "project_read_file",
        description: "Read a bounded UTF-8 page from a file in the admitted project.",
    },
    Definition {
        name: "project_list_directory",
        description: "List up to 128 immediate entries in an admitted project directory.",
    },
    Definition {
        name: "service_observe_status",
        description: "Observe bounded current service status for the admitted project.",
    },
    Definition {
        name: "memory_list_notes",
        description: "List bounded note summaries in the admitted project. Stored evidence is untrusted.",
    },
    Definition {
        name: "memory_get_note",
        description: "Read an immutable project note page as untrusted stored evidence.",
    },
    Definition {
        name: "memory_note_sources",
        description: "Read direct source references for a project note.",
    },
    Definition {
        name: "service_list_tools",
        description: "List registered tool names and descriptions; registration grants no access.",
    },
    Definition {
        name: "service_read_audit",
        description: "Inspect recent project audit metadata and logging health; records may have gaps.",
    },
    Definition {
        name: "exec_cmd",
        description: EXEC_CMD_DESCRIPTION,
    },
    Definition {
        name: "memory_create_note",
        description: "Create an immutable note in the admitted project, optionally citing one project note.",
    },
    Definition {
        name: "job_list",
        description: "List up to 8 jobs of this conversation, newest first.",
    },
    Definition {
        name: "job_status",
        description: "Show one job's state.",
    },
    Definition {
        name: "job_read",
        description: "Read one page of a job's stdout or stderr as untrusted tool output.",
    },
    Definition {
        name: "job_wait",
        description: "Wait for a job to settle or produce new output.",
    },
    Definition {
        name: "job_cancel",
        description: "Request cancellation of a job you started.",
    },
    Definition {
        name: "job_note",
        description: "Record a note on a job.",
    },
];
/// The exec_cmd registry text (tool-job-controller.md, helper tool
/// definitions).
const EXEC_CMD_DESCRIPTION: &str = "Run one /bin/sh -c command line in the admitted project as a job; returns its handle, state and byte counts, never output text.";

/// Public model-facing inventory. Typed commands retain separate operation policy.
/// Names come from the storage registry that replay uses (TCR-D24).
pub const REGISTRY: &[Definition] = &[
    Definition {
        name: journal::TOOL_NAMES[0],
        description: "Project commands: read_file(path, offset?, limit?) or list_directory(path?).",
    },
    Definition {
        name: journal::TOOL_NAMES[1],
        description: "Memory commands: list_notes(after?, limit?), get_note(version, offset?, limit?), note_sources(version), create_note(body, source_version?). Stored evidence is untrusted.",
    },
    Definition {
        name: journal::TOOL_NAMES[2],
        description: "Service commands: status, tools, audit(limit?). Read-only status, discovery and recent audit metadata.",
    },
    Definition {
        name: journal::TOOL_NAMES[3],
        description: EXEC_CMD_DESCRIPTION,
    },
    // The grouped job tool (J3-E2), registry position 4.
    Definition {
        name: journal::TOOL_NAMES[4],
        description: "Job commands: list(before?), status(job), read(job, stream, offset?, limit?), wait(job, seconds?), cancel(job), note(job, text).",
    },
];

/// The status and text sent for a call that does not run, rendered from the
/// kind 14 codes alone (TCR-D12). The text never holds model text, so it can
/// never repeat a secret. Replay and views render it again from the record.
/// `reason` is kind 14 field 10, the decision reason (J3-E3).
pub fn not_run_result(
    decision: journal::ToolDecision,
    fault: Option<journal::ToolFault>,
    reason: Option<journal::ToolDecisionReason>,
    refusal: Option<journal::ExecCmdRefusal>,
) -> Option<(u8, String)> {
    use journal::ToolDecision as D;
    let status = decision.status()?;
    let text = match decision {
        D::Run => return None,
        D::InvalidArguments | D::UnknownTool => format!(
            "{}: {}",
            decision.as_str(),
            fault.map_or("unknown", journal::ToolFault::as_str)
        ),
        D::Refused => match refusal {
            Some(refusal) => asura_platform::secrets::Refusal {
                code: refusal.code,
                kind: refusal.kind,
                word: refusal.word.map(usize::from),
            }
            .to_string(),
            None => "refused: secret_detection_unavailable".into(),
        },
        D::Busy | D::Denied => match reason.map(|r| (r, decision_reason_text(r))) {
            Some((reason, text)) => {
                format!("{}: {}: {text}", decision_prefix(decision), reason.as_str())
            }
            None if decision == D::Busy => "exec_cmd_busy: unknown".into(),
            None => decision.as_str().into(),
        },
        D::Limit | D::Cancelled => decision.as_str().into(),
    };
    Some((status, text))
}

/// The first word of a `busy` or `denied` result with a reason.
fn decision_prefix(decision: journal::ToolDecision) -> &'static str {
    match decision {
        journal::ToolDecision::Busy => "exec_cmd_busy",
        _ => decision.as_str(),
    }
}
/// The fixed text after a decision reason's name (tool-job-controller.md,
/// J3 kind 14 changes).
fn decision_reason_text(reason: journal::ToolDecisionReason) -> &'static str {
    use journal::ToolDecisionReason as R;
    match reason {
        R::QueueFull => {
            "this agent's job queue is full; wait for a job to settle or cancel one with job cancel"
        }
        R::JobsFull => "the service has 64 unsettled jobs; wait for one to settle",
        R::Closed => "the service is stopping",
        R::ExecutionOff => "the person turned command execution off for this project; do not retry",
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Arguments {
    MemoryCreateNote {
        body: String,
        source_version: Option<String>,
    },
    /// An exec_cmd call; it runs as a job (J3).
    ExecCmd {
        command: String,
        cwd: String,
        /// The yield window, 0-30 seconds; 10 when absent.
        yield_seconds: u32,
    },
    /// A `job` tool command (codes 11-16).
    Job(JobArguments),
    ReadAudit {
        limit: u32,
    },
    ObserveStatus,
    ListTools,
    MemoryListNotes {
        after: Option<String>,
        limit: u32,
    },
    MemoryGetNote {
        version: String,
        offset: u64,
        limit: u32,
    },
    MemoryNoteSources {
        version: String,
    },
    ReadFile {
        path: String,
        offset: u64,
        limit: u32,
    },
    ListDirectory {
        path: String,
    },
}
/// The typed `job` tool commands (tool-job-controller.md, the job tool).
/// The parser has checked every bound; a handle is never all zero.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum JobArguments {
    /// Code 11. `before` is a kind 26 frame sequence, at least 1.
    List { before: Option<u64> },
    /// Code 12.
    Status { job: journal::Id },
    /// Code 13. `offset` absent means the turn cursor (J3-E4); `limit`
    /// 1-8,192 source bytes.
    Read {
        job: journal::Id,
        stream: journal::OutputStream,
        offset: Option<u64>,
        limit: u32,
    },
    /// Code 14. `seconds` 1-3,600; absent means only events end the wait.
    Wait {
        job: journal::Id,
        seconds: Option<u32>,
    },
    /// Code 15.
    Cancel { job: journal::Id },
    /// Code 16. `text` 1-2,048 UTF-8 bytes, no NUL.
    Note { job: journal::Id, text: String },
}
impl JobArguments {
    /// The kind 14 operation code of this command.
    pub fn operation(&self) -> journal::ToolOperation {
        use journal::ToolOperation as O;
        match self {
            Self::List { .. } => O::JobList,
            Self::Status { .. } => O::JobStatus,
            Self::Read { .. } => O::JobRead,
            Self::Wait { .. } => O::JobWait,
            Self::Cancel { .. } => O::JobCancel,
            Self::Note { .. } => O::JobNote,
        }
    }
    /// The job the command names; `None` for `list`.
    pub fn job(&self) -> Option<journal::Id> {
        match self {
            Self::List { .. } => None,
            Self::Status { job }
            | Self::Read { job, .. }
            | Self::Wait { job, .. }
            | Self::Cancel { job }
            | Self::Note { job, .. } => Some(*job),
        }
    }
    fn valid(&self) -> bool {
        match self {
            Self::List { before } => before.is_none_or(|b| b >= 1),
            Self::Read {
                job, offset, limit, ..
            } => {
                *job != [0; 16]
                    && offset.is_none_or(|o| o <= i64::MAX as u64)
                    && (1..=journal::MAX_JOB_PAGE).contains(limit)
            }
            Self::Wait { job, seconds } => {
                *job != [0; 16]
                    && seconds.is_none_or(|s| (1..=MAX_WAIT_SECONDS).contains(&u64::from(s)))
            }
            Self::Note { job, text } => {
                *job != [0; 16]
                    && !text.is_empty()
                    && text.len() <= journal::MAX_JOB_NOTE
                    && !text.contains('\0')
            }
            Self::Status { job } | Self::Cancel { job } => *job != [0; 16],
        }
    }
}
impl Arguments {
    pub fn is_memory_create(&self) -> bool {
        matches!(self, Self::MemoryCreateNote { .. })
    }
    pub fn is_memory(&self) -> bool {
        matches!(
            self,
            Self::MemoryCreateNote { .. }
                | Self::MemoryListNotes { .. }
                | Self::MemoryGetNote { .. }
                | Self::MemoryNoteSources { .. }
        )
    }
    pub fn memory_query(&self) -> Result<asura_storage::memory::ReadNotes, Rejection> {
        use asura_storage::memory::ReadNotes;
        self.validate()?;
        match self {
            Self::MemoryListNotes { after, limit } => Ok(ReadNotes::List {
                after: after.as_deref().map(memory_id).transpose()?,
                limit: *limit as u8,
            }),
            Self::MemoryGetNote { version, .. } => Ok(ReadNotes::Get(memory_id(version)?)),
            Self::MemoryNoteSources { version } => Ok(ReadNotes::Sources(memory_id(version)?)),
            _ => Err(Rejection::InvalidArguments),
        }
    }
    pub fn definition(&self) -> &'static Definition {
        match self {
            Self::MemoryCreateNote { .. } => &OPERATIONS[9],
            Self::ObserveStatus => &OPERATIONS[2],
            Self::ListTools => &OPERATIONS[6],
            Self::ReadAudit { .. } => &OPERATIONS[7],
            Self::ExecCmd { .. } => &OPERATIONS[8],
            Self::Job(job) => {
                &OPERATIONS
                    [10 + (job.operation() as usize - journal::ToolOperation::JobList as usize)]
            }
            Self::MemoryListNotes { .. } => &OPERATIONS[3],
            Self::MemoryGetNote { .. } => &OPERATIONS[4],
            Self::MemoryNoteSources { .. } => &OPERATIONS[5],
            Self::ReadFile { .. } => &OPERATIONS[0],
            Self::ListDirectory { .. } => &OPERATIONS[1],
        }
    }
    pub fn validate(&self) -> Result<(), Rejection> {
        let (path, allow_root) = match self {
            Self::MemoryCreateNote {
                body,
                source_version,
            } => {
                return if !body.is_empty()
                    && body.len() <= 16384
                    && source_version
                        .as_deref()
                        .is_none_or(|s| memory_id(s).is_ok())
                {
                    Ok(())
                } else {
                    Err(Rejection::InvalidArguments)
                };
            }
            Self::ObserveStatus | Self::ListTools => return Ok(()),
            Self::Job(job) => {
                return if job.valid() {
                    Ok(())
                } else {
                    Err(Rejection::InvalidArguments)
                };
            }
            Self::ExecCmd {
                command,
                cwd,
                yield_seconds,
            } => {
                return if !command.is_empty()
                    && command.len() <= MAX_COMMAND_BYTES
                    && !command
                        .chars()
                        .any(|c| c.is_control() && c != '\t' && c != '\n')
                    && u64::from(*yield_seconds) <= MAX_YIELD_SECONDS
                    && !cwd.is_empty()
                    && cwd.len() <= 1024
                    && !cwd.contains('\0')
                    && !cwd.starts_with('/')
                    && !cwd.split('/').any(|part| part == "..")
                {
                    Ok(())
                } else {
                    Err(Rejection::InvalidArguments)
                };
            }
            Self::ReadAudit { limit } => {
                return if (1..=16).contains(limit) {
                    Ok(())
                } else {
                    Err(Rejection::InvalidArguments)
                };
            }
            Self::MemoryListNotes { after, limit } => {
                return if (1..=8).contains(limit)
                    && after.as_ref().is_none_or(|id| memory_id(id).is_ok())
                {
                    Ok(())
                } else {
                    Err(Rejection::InvalidArguments)
                };
            }
            Self::MemoryGetNote {
                version,
                offset,
                limit,
            } => {
                return if memory_id(version).is_ok()
                    && (1..=16_384).contains(limit)
                    && offset
                        .checked_add(u64::from(*limit))
                        .is_some_and(|end| end <= i64::MAX as u64)
                {
                    Ok(())
                } else {
                    Err(Rejection::InvalidArguments)
                };
            }
            Self::MemoryNoteSources { version } => return memory_id(version).map(|_| ()),
            Self::ReadFile {
                path,
                offset,
                limit,
            } => {
                if *limit == 0
                    || *limit as usize > MAX_RESULT_BYTES
                    || offset
                        .checked_add(u64::from(*limit))
                        .is_none_or(|end| end > i64::MAX as u64)
                {
                    return Err(Rejection::InvalidArguments);
                }
                (path, false)
            }
            Self::ListDirectory { path } => (path, true),
        };
        if allow_root && path == "." {
            return Ok(());
        }
        if path.is_empty()
            || path.len() > MAX_PATH_BYTES
            || path.chars().any(|c| c.is_control() || c == '\\')
            || path
                .split('/')
                .any(|c| c.is_empty() || c == "." || c == "..")
        {
            return Err(Rejection::InvalidArguments);
        }
        Ok(())
    }
}

/// Immutable metadata only; registration is not authority or runtime availability.
pub fn inventory_text() -> Result<String, Rejection> {
    format_inventory(REGISTRY)
}
fn format_inventory(entries: &[Definition]) -> Result<String, Rejection> {
    if entries.len() > 32 {
        return Err(Rejection::Limit);
    }
    let mut result = String::new();
    for entry in entries {
        let size = entry
            .name
            .len()
            .checked_add(entry.description.len())
            .and_then(|n| n.checked_add(2))
            .ok_or(Rejection::Limit)?;
        if size > MAX_RESULT_BYTES.saturating_sub(result.len()) {
            return Err(Rejection::Limit);
        }
        result.push_str(entry.name);
        result.push('\t');
        result.push_str(entry.description);
        result.push('\n');
    }
    Ok(result)
}

pub(crate) fn memory_id(value: &str) -> Result<asura_storage::memory::Id, Rejection> {
    if value.len() != 32
        || !value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(Rejection::InvalidArguments);
    }
    let mut bytes = [0; 16];
    for (i, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&value[i * 2..i * 2 + 2], 16)
            .map_err(|_| Rejection::InvalidArguments)?;
    }
    asura_storage::memory::Id::new(bytes).map_err(|_| Rejection::InvalidArguments)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Call {
    pub operation: [u8; 16],
    pub generation: u64,
    pub ordinal: u32,
    pub arguments: Arguments,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Destination {
    Local,
    Remote(String),
}
/// Constructed by admission from explicit authority, never from model arguments.
#[derive(Clone, Debug)]
pub struct Grant {
    pub operation: [u8; 16],
    pub generation: u64,
    pub project: [u8; 16],
    pub read_authorized: bool,
    pub memory_write_authorized: bool,
    pub execute_authorized: bool,
    pub remote_destination: Option<String>,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Rejection {
    Stale,
    Denied,
    /// An exec_cmd call while the project's effective execution setting is
    /// off (`execute_authorized` false, EX11): decision `denied` with
    /// reason 1 `execution_off`.
    ExecutionOff,
    InvalidArguments,
    Limit,
    Busy,
    Cancelled,
    IdentityConflict,
}

impl Rejection {
    /// The kind 14 decision and field 10 reason (J3-E3) of a call that
    /// authority refuses: `denied`, with `execution_off` only for an
    /// exec_cmd call refused by the execution setting; the over-limit
    /// ordinal is `limit`.
    pub fn decision(self) -> (journal::ToolDecision, Option<journal::ToolDecisionReason>) {
        use journal::{ToolDecision as D, ToolDecisionReason as R};
        match self {
            Self::ExecutionOff => (D::Denied, Some(R::ExecutionOff)),
            Self::Limit => (D::Limit, None),
            Self::Cancelled => (D::Cancelled, None),
            Self::Stale
            | Self::Denied
            | Self::InvalidArguments
            | Self::Busy
            | Self::IdentityConflict => (D::Denied, None),
        }
    }
}

pub fn validate(call: &Call, grant: &Grant, destination: &Destination) -> Result<(), Rejection> {
    validate_authority(call, grant, destination)?;
    call.arguments.validate()
}
pub fn validate_authority(
    call: &Call,
    grant: &Grant,
    destination: &Destination,
) -> Result<(), Rejection> {
    if call.operation == [0; 16]
        || call.operation != grant.operation
        || call.generation == 0
        || call.generation != grant.generation
    {
        return Err(Rejection::Stale);
    }
    if grant.project == [0; 16] {
        return Err(Rejection::Denied);
    }
    // `execute_authorized` is the project's effective execution setting
    // (EX11). Job commands need only `read_authorized`: `cancel` and `note`
    // stay allowed while execution is off (ES-E3).
    if matches!(call.arguments, Arguments::ExecCmd { .. }) {
        if !grant.execute_authorized {
            return Err(Rejection::ExecutionOff);
        }
    } else if if call.arguments.is_memory_create() {
        !grant.memory_write_authorized
    } else {
        !grant.read_authorized
    } {
        return Err(Rejection::Denied);
    }
    // Job output and states never go to a remote destination, like the
    // exec_cmd result itself.
    if (call.arguments.is_memory()
        || matches!(
            call.arguments,
            Arguments::ReadAudit { .. } | Arguments::ExecCmd { .. } | Arguments::Job(_)
        ))
        && matches!(destination, Destination::Remote(_))
    {
        return Err(Rejection::Denied);
    }
    if let Destination::Remote(destination) = destination
        && (destination.is_empty() || grant.remote_destination.as_ref() != Some(destination))
    {
        return Err(Rejection::Denied);
    }
    if !(1..=MAX_CALLS).contains(&call.ordinal) {
        return Err(Rejection::Limit);
    }
    Ok(())
}

/// Live accounting only. Durable records remain storage-owned. Every
/// recorded call (run or not) consumes one ordinal (TCR-D9). A turn has no
/// result-byte total; each result keeps `MAX_RESULT_BYTES` (owner,
/// 2026-10-18). The pending call itself is owned by the conversation owner,
/// one at a time.
#[derive(Debug)]
pub struct Budget {
    recorded: u32,
    cancelled: bool,
}
impl Budget {
    /// The only budget: no time bound; cancellation ends admission.
    pub fn cancellation_driven() -> Self {
        Self {
            recorded: 0,
            cancelled: false,
        }
    }
    /// The ordinal the helper must send next (1 to the over-limit ordinal).
    pub fn next_ordinal(&self) -> u32 {
        self.recorded + 1
    }
    /// Counts one recorded call at `ordinal` 1..=MAX_CALLS. The over-limit
    /// ordinal is never counted.
    pub fn record(&mut self, ordinal: u32) -> Result<(), Rejection> {
        if ordinal != self.recorded + 1 || self.recorded == MAX_CALLS {
            return Err(Rejection::Limit);
        }
        self.recorded += 1;
        Ok(())
    }
    /// The bytes one result may hold. No turn total reduces it.
    pub fn result_capacity(&self) -> usize {
        MAX_RESULT_BYTES
    }
    /// Call only after a known committed result of a `run` call. Refuses a
    /// result over the per-result limit.
    pub fn settle(&mut self, bytes: usize) -> Result<(), Rejection> {
        if bytes > self.result_capacity() {
            return Err(Rejection::Limit);
        }
        Ok(())
    }
    pub fn cancel(&mut self) {
        self.cancelled = true;
    }
    pub fn is_cancelled(&self) -> bool {
        self.cancelled
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn exec_cmd_requires_separate_execution_grant_and_never_remote_disclosure() {
        let mut grant = Grant {
            operation: [1; 16],
            generation: 1,
            project: [2; 16],
            read_authorized: true,
            memory_write_authorized: true,
            execute_authorized: false,
            remote_destination: Some("approved".into()),
        };
        let call = Call {
            operation: [1; 16],
            generation: 1,
            ordinal: 1,
            arguments: Arguments::ExecCmd {
                command: "printf hello".into(),
                cwd: ".".into(),
                yield_seconds: 10,
            },
        };
        // J3T-14 (unit): execution off is `denied` with reason 1
        // `execution_off` and its fixed text (EX11).
        let refused = validate(&call, &grant, &Destination::Local);
        assert_eq!(refused, Err(Rejection::ExecutionOff));
        let (decision, reason) = refused.unwrap_err().decision();
        assert_eq!(
            not_run_result(decision, None, reason, None),
            Some((
                2,
                "denied: execution_off: the person turned command execution off for this project; do not retry"
                    .into()
            ))
        );
        grant.execute_authorized = true;
        grant.read_authorized = false;
        assert_eq!(validate(&call, &grant, &Destination::Local), Ok(()));
        assert_eq!(
            validate(&call, &grant, &Destination::Remote("approved".into())),
            Err(Rejection::Denied)
        );
        for (command, cwd, yield_seconds) in [
            ("", ".", 10),
            ("a\0b", ".", 10),
            ("a\rb", ".", 10),
            ("echo", "../x", 10),
            ("echo", "/tmp", 10),
            ("echo", ".", 31),
        ] {
            let mut invalid = call.clone();
            invalid.arguments = Arguments::ExecCmd {
                command: command.into(),
                cwd: cwd.into(),
                yield_seconds,
            };
            assert_eq!(
                validate(&invalid, &grant, &Destination::Local),
                Err(Rejection::InvalidArguments)
            );
        }
    }
    /// The job tool: every command needs `read_authorized` only, so
    /// `cancel` and `note` stay allowed while execution is off (ES-E3); job
    /// state and output never go to a remote destination; the project
    /// executor never runs a job command.
    #[test]
    fn job_commands_need_read_authority_only_and_never_go_remote() {
        let (mut call, mut grant) = fixture();
        grant.execute_authorized = false;
        grant.remote_destination = Some("approved".into());
        let job = [7; 16];
        let commands = [
            JobArguments::List { before: Some(1) },
            JobArguments::Status { job },
            JobArguments::Read {
                job,
                stream: journal::OutputStream::Stdout,
                offset: None,
                limit: 8192,
            },
            JobArguments::Wait {
                job,
                seconds: Some(3600),
            },
            JobArguments::Cancel { job },
            JobArguments::Note {
                job,
                text: "n".into(),
            },
        ];
        for (index, command) in commands.into_iter().enumerate() {
            assert_eq!(
                Arguments::Job(command.clone()).definition().name,
                [
                    "job_list",
                    "job_status",
                    "job_read",
                    "job_wait",
                    "job_cancel",
                    "job_note"
                ][index]
            );
            call.arguments = Arguments::Job(command);
            grant.read_authorized = true;
            assert_eq!(validate(&call, &grant, &Destination::Local), Ok(()));
            assert_eq!(
                validate(&call, &grant, &Destination::Remote("approved".into())),
                Err(Rejection::Denied)
            );
            grant.read_authorized = false;
            assert_eq!(
                validate(&call, &grant, &Destination::Local),
                Err(Rejection::Denied)
            );
        }
        for invalid in [
            JobArguments::Status { job: [0; 16] },
            JobArguments::List { before: Some(0) },
            JobArguments::Read {
                job,
                stream: journal::OutputStream::Stderr,
                offset: Some(u64::MAX),
                limit: 1,
            },
            JobArguments::Read {
                job,
                stream: journal::OutputStream::Stderr,
                offset: None,
                limit: 8193,
            },
            JobArguments::Wait {
                job,
                seconds: Some(0),
            },
            JobArguments::Note {
                job,
                text: String::new(),
            },
            JobArguments::Note {
                job,
                text: "a\0".into(),
            },
        ] {
            assert_eq!(
                Arguments::Job(invalid).validate(),
                Err(Rejection::InvalidArguments)
            );
        }
        grant.read_authorized = true;
        call.arguments = Arguments::Job(JobArguments::Status { job });
        assert_eq!(
            Executor::default().start(
                call,
                grant,
                Destination::Local,
                ExecutionScope {
                    project: [2; 16],
                    path: "/does-not-exist".into(),
                    device: 0,
                    inode: 0,
                    boundary: asura_platform::StateBoundary::detached_for_test(),
                },
            ),
            Err(ExecutionError::Rejected(Rejection::Denied))
        );
        // The decision of each authority refusal (J3-E3).
        use journal::{ToolDecision as D, ToolDecisionReason as R};
        assert_eq!(
            Rejection::ExecutionOff.decision(),
            (D::Denied, Some(R::ExecutionOff))
        );
        assert_eq!(Rejection::Denied.decision(), (D::Denied, None));
        assert_eq!(Rejection::Stale.decision(), (D::Denied, None));
        assert_eq!(Rejection::Limit.decision(), (D::Limit, None));
    }
    /// TCR-D9, TCR-U18: every recorded call consumes one ordinal 1-200
    /// whether or not it runs; the over-limit ordinal 201 is never counted and
    /// a gap is refused (owner, 2026-10-18).
    #[test]
    fn every_recorded_call_consumes_one_ordinal_and_the_over_limit_ordinal_is_never_counted() {
        assert_eq!(MAX_CALLS, 200);
        let mut budget = Budget::cancellation_driven();
        assert_eq!(budget.next_ordinal(), 1);
        assert_eq!(budget.record(2), Err(Rejection::Limit));
        for ordinal in 1..=MAX_CALLS {
            budget.record(ordinal).unwrap();
        }
        assert_eq!(budget.next_ordinal(), journal::MAX_TOOL_CALL_ORDINAL);
        assert_eq!(
            budget.record(journal::MAX_TOOL_CALL_ORDINAL),
            Err(Rejection::Limit)
        );
        assert_eq!(budget.result_capacity(), MAX_RESULT_BYTES);
    }

    #[test]
    fn memory_arguments_are_scoped_and_never_remote() {
        let (mut call, mut grant) = fixture();
        grant.remote_destination = Some("https://approved.invalid".into());
        for args in [
            Arguments::MemoryListNotes {
                after: None,
                limit: 8,
            },
            Arguments::MemoryGetNote {
                version: "ab".repeat(16),
                offset: 0,
                limit: 16384,
            },
            Arguments::MemoryNoteSources {
                version: "ab".repeat(16),
            },
        ] {
            call.arguments = args;
            assert!(validate(&call, &grant, &Destination::Local).is_ok());
            assert_eq!(
                validate(
                    &call,
                    &grant,
                    &Destination::Remote("https://approved.invalid".into())
                ),
                Err(Rejection::Denied)
            );
            assert!(call.arguments.memory_query().is_ok());
        }
        for value in [
            "",
            "ABABABABABABABABABABABABABABABAB",
            "00000000000000000000000000000000",
            "../../private",
            "abababababababababababababababa",
        ] {
            assert_eq!(
                Arguments::MemoryNoteSources {
                    version: value.into()
                }
                .validate(),
                Err(Rejection::InvalidArguments)
            );
        }
        for args in [
            Arguments::MemoryListNotes {
                after: Some(String::new()),
                limit: 1,
            },
            Arguments::MemoryListNotes {
                after: None,
                limit: 0,
            },
            Arguments::MemoryListNotes {
                after: None,
                limit: 9,
            },
            Arguments::MemoryGetNote {
                version: "ab".repeat(16),
                offset: u64::MAX,
                limit: 1,
            },
        ] {
            assert_eq!(args.validate(), Err(Rejection::InvalidArguments));
        }
    }
    #[test]
    fn status_is_typed_and_rejected_by_filesystem_executor() {
        let call = Call {
            operation: [1; 16],
            generation: 1,
            ordinal: 1,
            arguments: Arguments::ObserveStatus,
        };
        let grant = Grant {
            operation: call.operation,
            generation: 1,
            project: [2; 16],
            read_authorized: true,
            memory_write_authorized: true,
            execute_authorized: false,
            remote_destination: None,
        };
        assert_eq!(call.arguments.definition().name, "service_observe_status");
        assert!(validate(&call, &grant, &Destination::Local).is_ok());
        assert_eq!(
            validate(
                &call,
                &grant,
                &Destination::Remote("https://example.invalid".into())
            ),
            Err(Rejection::Denied)
        );
        let mut executor = Executor::default();
        assert_eq!(
            executor.start(
                call,
                grant,
                Destination::Local,
                ExecutionScope {
                    project: [2; 16],
                    path: "/does-not-exist".into(),
                    device: 0,
                    inode: 0,
                    boundary: asura_platform::StateBoundary::detached_for_test(),
                },
            ),
            Err(ExecutionError::Rejected(Rejection::Denied))
        );
    }

    fn fixture() -> (Call, Grant) {
        (
            Call {
                operation: [1; 16],
                generation: 3,
                ordinal: 1,
                arguments: Arguments::ReadFile {
                    path: "src/main.rs".into(),
                    offset: 0,
                    limit: 1024,
                },
            },
            Grant {
                operation: [1; 16],
                generation: 3,
                project: [2; 16],
                read_authorized: true,
                memory_write_authorized: true,
                execute_authorized: false,
                remote_destination: None,
            },
        )
    }
    #[test]
    fn path_validation_cannot_be_used_as_traversal_authority() {
        for path in [
            "",
            "/etc/passwd",
            "../key",
            "a/../key",
            "a//b",
            "a/./b",
            "a/",
            "a\\b",
            "a\0b",
            ".",
            "a\nb",
        ] {
            let args = Arguments::ReadFile {
                path: path.into(),
                offset: 0,
                limit: 1,
            };
            assert_eq!(
                args.validate(),
                Err(Rejection::InvalidArguments),
                "{path:?}"
            );
        }
        assert!(
            Arguments::ListDirectory { path: ".".into() }
                .validate()
                .is_ok()
        );
        assert!(
            Arguments::ReadFile {
                path: "資料/source.rs".into(),
                offset: 0,
                limit: 16_384
            }
            .validate()
            .is_ok()
        );
        for (offset, limit) in [(0, 0), (0, 16_385), (u64::MAX, 1)] {
            assert_eq!(
                Arguments::ReadFile {
                    path: "a".into(),
                    offset,
                    limit
                }
                .validate(),
                Err(Rejection::InvalidArguments)
            );
        }
    }
    #[test]
    fn audit_never_discloses_to_an_approved_remote_destination() {
        let (mut call, mut grant) = fixture();
        call.arguments = Arguments::ReadAudit { limit: 16 };
        grant.remote_destination = Some("https://approved.invalid".into());
        assert_eq!(validate(&call, &grant, &Destination::Local), Ok(()));
        assert_eq!(
            validate(
                &call,
                &grant,
                &Destination::Remote("https://approved.invalid".into())
            ),
            Err(Rejection::Denied)
        );
        for limit in [0, 17, u32::MAX] {
            assert_eq!(
                Arguments::ReadAudit { limit }.validate(),
                Err(Rejection::InvalidArguments)
            );
        }
    }
    #[test]
    fn grants_fence_operation_project_and_remote_disclosure() {
        let (mut call, mut grant) = fixture();
        assert_eq!(validate(&call, &grant, &Destination::Local), Ok(()));
        assert_eq!(
            validate(&call, &grant, &Destination::Remote("provider-a".into())),
            Err(Rejection::Denied)
        );
        grant.remote_destination = Some("provider-a".into());
        assert_eq!(
            validate(&call, &grant, &Destination::Remote("provider-a".into())),
            Ok(())
        );
        assert_eq!(
            validate(&call, &grant, &Destination::Remote("provider-b".into())),
            Err(Rejection::Denied)
        );
        call.generation += 1;
        assert_eq!(
            validate(&call, &grant, &Destination::Local),
            Err(Rejection::Stale)
        );
        call.generation = grant.generation;
        grant.read_authorized = false;
        assert_eq!(
            validate(&call, &grant, &Destination::Local),
            Err(Rejection::Denied)
        );
    }
    #[test]
    fn cancellation_is_visible_and_never_restores_capacity() {
        let mut budget = Budget::cancellation_driven();
        budget.record(1).unwrap();
        budget.settle(MAX_RESULT_BYTES).unwrap();
        budget.cancel();
        assert!(budget.is_cancelled());
        assert_eq!(budget.result_capacity(), MAX_RESULT_BYTES);
    }

    #[test]
    fn oversized_result_is_refused_without_consuming_capacity() {
        let mut budget = Budget::cancellation_driven();
        budget.record(1).unwrap();
        assert_eq!(budget.settle(MAX_RESULT_BYTES + 1), Err(Rejection::Limit));
        assert_eq!(budget.result_capacity(), MAX_RESULT_BYTES);
    }
    /// TCR-U18: a turn has no result-byte total (owner, 2026-10-18). Every
    /// recorded call can settle a full 16 KiB result, far beyond 64 KiB.
    #[test]
    fn full_results_for_every_call_have_no_turn_total() {
        let mut budget = Budget::cancellation_driven();
        for ordinal in 1..=MAX_CALLS {
            budget.record(ordinal).unwrap();
            assert_eq!(budget.result_capacity(), MAX_RESULT_BYTES);
            budget.settle(MAX_RESULT_BYTES).unwrap();
        }
        assert_eq!(budget.settle(MAX_RESULT_BYTES + 1), Err(Rejection::Limit));
    }
    /// TCR-D12: rendered text of calls that do not run holds only codes.
    #[test]
    fn not_run_text_is_rendered_from_codes_only() {
        use journal::{ToolDecision as D, ToolDecisionReason as R, ToolFault as F};
        assert_eq!(
            not_run_result(D::InvalidArguments, Some(F::MissingField), None, None),
            Some((3, "invalid_arguments: missing_field".into()))
        );
        assert_eq!(
            not_run_result(D::UnknownTool, Some(F::UnregisteredName), None, None),
            Some((3, "unknown_tool: unregistered_name".into()))
        );
        assert_eq!(
            not_run_result(D::Refused, None, None, None),
            Some((3, "refused: secret_detection_unavailable".into()))
        );
        let refusal = journal::ExecCmdRefusal {
            code: asura_platform::secrets::RefusalCode::SecretSingleQuoted,
            kind: Some(asura_platform::secrets::SecretKind::GithubToken),
            word: Some(2),
        };
        let (status, text) = not_run_result(D::Refused, None, None, Some(refusal)).unwrap();
        assert_eq!(status, 3);
        assert!(text.starts_with("Command refused: a `github-token` secret in word 2"));
        // J3T-14 (unit, rendered texts): busy codes 1-3 and `denied`
        // reason 1; the J1 busy codes are retired.
        for (decision, reason, status, text) in [
            (
                D::Busy,
                R::QueueFull,
                4,
                "exec_cmd_busy: queue_full: this agent's job queue is full; wait for a job to settle or cancel one with job cancel",
            ),
            (
                D::Busy,
                R::JobsFull,
                4,
                "exec_cmd_busy: jobs_full: the service has 64 unsettled jobs; wait for one to settle",
            ),
            (
                D::Busy,
                R::Closed,
                4,
                "exec_cmd_busy: closed: the service is stopping",
            ),
            (
                D::Denied,
                R::ExecutionOff,
                2,
                "denied: execution_off: the person turned command execution off for this project; do not retry",
            ),
        ] {
            assert_eq!(reason.decision(), decision);
            assert_eq!(
                not_run_result(decision, None, Some(reason), None),
                Some((status, text.into()))
            );
        }
        assert_eq!(
            not_run_result(D::Denied, None, None, None),
            Some((2, "denied".into()))
        );
        assert_eq!(
            not_run_result(D::Limit, None, None, None),
            Some((7, "limit".into()))
        );
        assert_eq!(
            not_run_result(D::Cancelled, None, None, None),
            Some((6, "cancelled".into()))
        );
        assert_eq!(not_run_result(D::Run, None, None, None), None);
    }
}

/// Trusted scope resolved from the admitted project's durable registration.
#[derive(Clone, Debug)]
pub struct ExecutionScope {
    pub project: [u8; 16],
    pub path: String,
    pub device: u64,
    pub inode: u64,
    /// The runtime's Asura home boundary; every project open applies it.
    pub boundary: asura_platform::StateBoundary,
}
#[derive(Debug, Eq, PartialEq)]
pub enum Output {
    File(asura_platform::ProjectPage),
    Directory(asura_platform::ProjectListing),
    /// The project identity recheck alone (a `job` command): the project
    /// is the one the turn was admitted for.
    Checked,
}
#[derive(Debug, Eq, PartialEq)]
pub enum ExecutionError {
    Rejected(Rejection),
    Host(asura_platform::ProjectReadError),
    WorkerFailed,
}
struct Work {
    cancelled: std::sync::Arc<std::sync::atomic::AtomicBool>,
    thread: std::thread::JoinHandle<Result<Output, ExecutionError>>,
    completion: crate::completion::Completion,
}
/// One service-owned execution slot. Its owner must retain this value until settled.
#[derive(Default)]
pub struct Executor {
    work: Option<Work>,
    wake: Option<asura_platform::events::WakeSender>,
}
impl Executor {
    pub(crate) fn register(&mut self, wake: asura_platform::events::WakeSender) {
        self.wake = Some(wake);
    }
    pub(crate) fn next_deadline(&self, now: Instant) -> Option<Instant> {
        self.work.as_ref().and_then(|work| {
            if work.thread.is_finished() {
                return Some(now);
            }
            work.completion.settlement_deadline(now)
        })
    }

    /// The conversation owner must commit ToolIntent before calling this method.
    /// All potentially blocking path and file operations occur on the worker.
    pub fn start(
        &mut self,
        call: Call,
        grant: Grant,
        destination: Destination,
        scope: ExecutionScope,
    ) -> Result<(), ExecutionError> {
        if self.work.is_some() {
            return Err(ExecutionError::Rejected(Rejection::Busy));
        }
        validate(&call, &grant, &destination).map_err(ExecutionError::Rejected)?;
        if matches!(
            call.arguments,
            Arguments::ObserveStatus
                | Arguments::ListTools
                | Arguments::ReadAudit { .. }
                | Arguments::ExecCmd { .. }
                | Arguments::Job(_)
        ) || call.arguments.is_memory()
        {
            return Err(ExecutionError::Rejected(Rejection::Denied));
        }
        if scope.project != grant.project {
            return Err(ExecutionError::Rejected(Rejection::Denied));
        }
        // No time bound: the read ends on completion or the turn's cancel.
        self.spawn(move |cancelled| {
            if cancelled.load(std::sync::atomic::Ordering::Acquire) {
                return Err(ExecutionError::Rejected(Rejection::Cancelled));
            }
            let project = asura_platform::ProjectIdentity::open(&scope.path, &scope.boundary)
                .map_err(|_| ExecutionError::Host(asura_platform::ProjectReadError::UnsafePath))?;
            if project.device != scope.device || project.inode != scope.inode {
                return Err(ExecutionError::Host(
                    asura_platform::ProjectReadError::Changed,
                ));
            }
            match call.arguments {
                Arguments::ObserveStatus
                | Arguments::ListTools
                | Arguments::ReadAudit { .. }
                | Arguments::ExecCmd { .. }
                | Arguments::Job(_)
                | Arguments::MemoryListNotes { .. }
                | Arguments::MemoryGetNote { .. }
                | Arguments::MemoryNoteSources { .. }
                | Arguments::MemoryCreateNote { .. } => {
                    Err(ExecutionError::Rejected(Rejection::Denied))
                }
                Arguments::ReadFile {
                    path,
                    offset,
                    limit,
                } => project
                    .read_project_page(&path, offset, limit as usize, cancelled)
                    .map(Output::File)
                    .map_err(ExecutionError::Host),
                Arguments::ListDirectory { path } => project
                    .list_project_directory(&path, cancelled)
                    .map(Output::Directory)
                    .map_err(ExecutionError::Host),
            }
        })
    }
    /// The project identity recheck that every `job` command runs first
    /// (tool-job-controller.md, the job tool): the same open and compare as
    /// a project read, on the same worker, with no read. `Changed` means
    /// the project was replaced. The conversation owner must commit the
    /// call's kind 14 first.
    pub fn recheck(
        &mut self,
        call: Call,
        grant: Grant,
        destination: Destination,
        scope: ExecutionScope,
    ) -> Result<(), ExecutionError> {
        if self.work.is_some() {
            return Err(ExecutionError::Rejected(Rejection::Busy));
        }
        validate(&call, &grant, &destination).map_err(ExecutionError::Rejected)?;
        if !matches!(call.arguments, Arguments::Job(_)) || scope.project != grant.project {
            return Err(ExecutionError::Rejected(Rejection::Denied));
        }
        self.spawn(move |cancelled| {
            if cancelled.load(std::sync::atomic::Ordering::Acquire) {
                return Err(ExecutionError::Rejected(Rejection::Cancelled));
            }
            let project = asura_platform::ProjectIdentity::open(&scope.path, &scope.boundary)
                .map_err(|_| ExecutionError::Host(asura_platform::ProjectReadError::UnsafePath))?;
            if project.device != scope.device || project.inode != scope.inode {
                return Err(ExecutionError::Host(
                    asura_platform::ProjectReadError::Changed,
                ));
            }
            Ok(Output::Checked)
        })
    }
    fn spawn(
        &mut self,
        run: impl FnOnce(&std::sync::atomic::AtomicBool) -> Result<Output, ExecutionError>
        + Send
        + 'static,
    ) -> Result<(), ExecutionError> {
        let cancelled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let token = cancelled.clone();
        let completion = crate::completion::Completion::default();
        if let Some(wake) = &self.wake {
            completion.register(wake.clone());
        }
        let signal = completion.clone();
        let thread = std::thread::Builder::new()
            .name("asura-project-tool".into())
            .spawn(move || {
                let _completion = signal.guard();
                run(&token)
            })
            .map_err(|_| ExecutionError::WorkerFailed)?;
        self.work = Some(Work {
            cancelled,
            thread,
            completion,
        });
        Ok(())
    }
    pub fn cancel(&self) {
        if let Some(work) = &self.work {
            work.cancelled
                .store(true, std::sync::atomic::Ordering::Release);
        }
    }
    pub fn is_settled(&self) -> bool {
        self.work.is_none()
    }
    /// Never joins an unfinished worker or waits for a producer lock.
    pub fn poll(&mut self) -> Option<Result<Output, ExecutionError>> {
        if !self.work.as_ref()?.thread.is_finished() {
            return None;
        }
        let work = self.work.take().expect("finished worker remains owned");
        Some(
            work.thread
                .join()
                .unwrap_or(Err(ExecutionError::WorkerFailed)),
        )
    }
}

#[cfg(test)]
mod executor_tests {
    use super::*;
    #[test]
    fn cancellation_does_not_release_a_stalled_worker_slot() {
        let (release, wait) = std::sync::mpsc::sync_channel(1);
        let mut executor = Executor::default();
        executor
            .spawn(move |_| {
                wait.recv_timeout(std::time::Duration::from_secs(2))
                    .unwrap();
                Err(ExecutionError::Host(
                    asura_platform::ProjectReadError::Cancelled,
                ))
            })
            .unwrap();
        executor.cancel();
        let pending = executor.poll();
        let held = !executor.is_settled();
        // Always release the fixture before asserting to avoid leaving test work behind.
        release.send(()).unwrap();
        let deadline = Instant::now() + std::time::Duration::from_secs(2);
        let outcome = loop {
            if let Some(result) = executor.poll() {
                break Some(result);
            }
            if Instant::now() >= deadline {
                break None;
            }
            std::thread::yield_now();
        };
        assert!(pending.is_none());
        assert!(held);
        assert_eq!(
            outcome,
            Some(Err(ExecutionError::Host(
                asura_platform::ProjectReadError::Cancelled
            )))
        );
        assert!(executor.is_settled());
    }
    /// MT-D1: a stalled project read has no timer. Its slot stays held and
    /// schedules no deadline; only cancel (or completion) ends it.
    #[test]
    fn stalled_worker_schedules_no_deadline_and_ends_on_cancel() {
        let (release, wait) = std::sync::mpsc::sync_channel::<()>(1);
        let mut executor = Executor::default();
        executor
            .spawn(move |cancelled| {
                // Stands in for a blocked read that checks its cancel flag.
                while !cancelled.load(std::sync::atomic::Ordering::Acquire) {
                    if wait.try_recv().is_ok() {
                        break;
                    }
                    std::thread::yield_now();
                }
                Err(ExecutionError::Host(
                    asura_platform::ProjectReadError::Cancelled,
                ))
            })
            .unwrap();
        let later = Instant::now() + std::time::Duration::from_secs(3600);
        assert_eq!(executor.next_deadline(later), None);
        assert!(executor.poll().is_none());
        assert!(!executor.is_settled());
        executor.cancel();
        let end = Instant::now() + std::time::Duration::from_secs(5);
        let outcome = loop {
            if let Some(result) = executor.poll() {
                break result;
            }
            assert!(Instant::now() < end, "cancel did not end the stalled read");
            std::thread::yield_now();
        };
        drop(release);
        assert_eq!(
            outcome,
            Err(ExecutionError::Host(
                asura_platform::ProjectReadError::Cancelled
            ))
        );
        assert!(executor.is_settled());
    }
}

#[cfg(test)]
mod inventory_tests {
    use super::*;
    #[test]
    fn inventory_uses_exact_registry_metadata_and_rejects_overflow() {
        assert_eq!(
            REGISTRY.iter().map(|tool| tool.name).collect::<Vec<_>>(),
            ["project", "memory", "service", "exec_cmd", "job"]
        );
        // The J3 registry texts (helper tool definitions).
        assert_eq!(
            REGISTRY[3].description,
            "Run one /bin/sh -c command line in the admitted project as a job; returns its handle, state and byte counts, never output text."
        );
        assert_eq!(
            REGISTRY[4].description,
            "Job commands: list(before?), status(job), read(job, stream, offset?, limit?), wait(job, seconds?), cancel(job), note(job, text)."
        );
        let text = inventory_text().unwrap();
        assert_eq!(text.lines().count(), REGISTRY.len());
        for definition in REGISTRY {
            assert!(
                text.lines()
                    .any(|line| line == format!("{}\t{}", definition.name, definition.description))
            );
        }
        assert_eq!(Arguments::ListTools.definition().name, "service_list_tools");
        let entry = Definition {
            name: "tool",
            description: "description",
        };
        assert_eq!(format_inventory(&[entry; 33]), Err(Rejection::Limit));
        static LONG: [u8; MAX_RESULT_BYTES] = [b'x'; MAX_RESULT_BYTES];
        let long = std::str::from_utf8(&LONG).unwrap();
        assert_eq!(
            format_inventory(&[Definition {
                name: "x",
                description: long
            }]),
            Err(Rejection::Limit)
        );
    }
}
