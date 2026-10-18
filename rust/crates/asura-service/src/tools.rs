//! Provider-independent tool contracts. Validation grants no host IO by itself.
//!
//! The conversation owner must persist intent before dispatch and persist results
//! before delivering them to a model. Host IO is isolated in the retained executor;
//! this module never performs inference.
use std::time::Instant;

mod parse;
use asura_storage::authority::conversation as journal;
pub use parse::{Parsed, parse_call};

/// Calls that may be recorded per turn (ordinals 1-8); ordinal 9 is only the
/// over-limit `limit` record (TCR-D6, TCR-D9).
pub const MAX_CALLS: u32 = journal::MAX_RECORDED_CALLS;
pub const MAX_RESULT_BYTES: usize = 16_384;
pub const MAX_TOTAL_RESULT_BYTES: usize = 65_536;
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
        description: "Run one bounded /bin/sh -c command line in the admitted project; returns output and exit status.",
    },
    Definition {
        name: "memory_create_note",
        description: "Create an immutable note in the admitted project, optionally citing one project note.",
    },
];

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
        description: "Run one bounded /bin/sh -c command line in the admitted project; returns output and exit status.",
    },
];

/// The status and text sent for a call that does not run, rendered from the
/// kind 14 codes alone (TCR-D12). The text never holds model text, so it can
/// never repeat a secret. Replay and views render it again from the record.
pub fn not_run_result(
    decision: journal::ToolDecision,
    fault: Option<journal::ToolFault>,
    busy: Option<journal::ExecCmdBusyReason>,
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
        D::Busy => format!(
            "exec_cmd_busy: {}",
            busy.map_or("unknown", journal::ExecCmdBusyReason::as_str)
        ),
        D::Denied | D::Limit | D::Cancelled => decision.as_str().into(),
    };
    Some((status, text))
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Arguments {
    MemoryCreateNote {
        body: String,
        source_version: Option<String>,
    },
    ExecCmd {
        command: String,
        cwd: String,
    },
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
            Self::ExecCmd { command, cwd } => {
                return if !command.is_empty()
                    && command.len() <= 8192
                    && !command.contains('\0')
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
    InvalidArguments,
    Limit,
    Busy,
    Cancelled,
    IdentityConflict,
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
    if grant.project == [0; 16]
        || if matches!(call.arguments, Arguments::ExecCmd { .. }) {
            !grant.execute_authorized
        } else if call.arguments.is_memory_create() {
            !grant.memory_write_authorized
        } else {
            !grant.read_authorized
        }
    {
        return Err(Rejection::Denied);
    }
    if (call.arguments.is_memory()
        || matches!(
            call.arguments,
            Arguments::ReadAudit { .. } | Arguments::ExecCmd { .. }
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
/// recorded call (run or not) consumes one ordinal; a call that does not run
/// consumes no result bytes (TCR-D9). The pending call itself is owned by the
/// conversation owner, one at a time.
#[derive(Debug)]
pub struct Budget {
    recorded: u32,
    result_bytes: usize,
    cancelled: bool,
}
impl Budget {
    /// The only budget: no time bound; cancellation ends admission.
    pub fn cancellation_driven() -> Self {
        Self {
            recorded: 0,
            result_bytes: 0,
            cancelled: false,
        }
    }
    /// The ordinal the helper must send next (1-9).
    pub fn next_ordinal(&self) -> u32 {
        self.recorded + 1
    }
    /// Counts one recorded call at `ordinal` 1-8. Ordinal 9 is never counted.
    pub fn record(&mut self, ordinal: u32) -> Result<(), Rejection> {
        if ordinal != self.recorded + 1 || self.recorded == MAX_CALLS {
            return Err(Rejection::Limit);
        }
        self.recorded += 1;
        Ok(())
    }
    pub fn result_capacity(&self) -> usize {
        MAX_RESULT_BYTES.min(MAX_TOTAL_RESULT_BYTES - self.result_bytes)
    }
    /// Call only after a known committed result of a `run` call.
    pub fn settle(&mut self, bytes: usize) -> Result<(), Rejection> {
        if bytes > self.result_capacity() {
            return Err(Rejection::Limit);
        }
        self.result_bytes += bytes;
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
            },
        };
        assert_eq!(
            validate(&call, &grant, &Destination::Local),
            Err(Rejection::Denied)
        );
        grant.execute_authorized = true;
        grant.read_authorized = false;
        assert_eq!(validate(&call, &grant, &Destination::Local), Ok(()));
        assert_eq!(
            validate(&call, &grant, &Destination::Remote("approved".into())),
            Err(Rejection::Denied)
        );
        for (command, cwd) in [("", "."), ("a\0b", "."), ("echo", "../x"), ("echo", "/tmp")] {
            let mut invalid = call.clone();
            invalid.arguments = Arguments::ExecCmd {
                command: command.into(),
                cwd: cwd.into(),
            };
            assert_eq!(
                validate(&invalid, &grant, &Destination::Local),
                Err(Rejection::InvalidArguments)
            );
        }
    }
    /// TCR-D9: every recorded call consumes one ordinal 1-8 whether or not
    /// it runs; ordinal 9 is never counted and a gap is refused.
    #[test]
    fn every_recorded_call_consumes_one_ordinal_and_nine_is_never_counted() {
        let mut budget = Budget::cancellation_driven();
        assert_eq!(budget.next_ordinal(), 1);
        assert_eq!(budget.record(2), Err(Rejection::Limit));
        for ordinal in 1..=MAX_CALLS {
            budget.record(ordinal).unwrap();
            // A call that does not run consumes no result bytes.
            budget.settle(0).unwrap();
        }
        assert_eq!(budget.next_ordinal(), 9);
        assert_eq!(budget.record(9), Err(Rejection::Limit));
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
    #[test]
    fn aggregate_result_bytes_cannot_expand_on_settlement() {
        let mut budget = Budget::cancellation_driven();
        for ordinal in 1..=4 {
            budget.record(ordinal).unwrap();
            budget.settle(MAX_RESULT_BYTES).unwrap();
        }
        assert_eq!(budget.result_capacity(), 0);
        budget.record(5).unwrap();
        assert_eq!(budget.settle(1), Err(Rejection::Limit));
    }
    /// TCR-D12: rendered text of calls that do not run holds only codes.
    #[test]
    fn not_run_text_is_rendered_from_codes_only() {
        use journal::{ExecCmdBusyReason as B, ToolDecision as D, ToolFault as F};
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
        assert_eq!(
            not_run_result(D::Busy, None, Some(B::Quota), None),
            Some((4, "exec_cmd_busy: quota".into()))
        );
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
            ["project", "memory", "service", "exec_cmd"]
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
