//! Provider-independent tool contracts. Validation grants no host IO by itself.
//!
//! The conversation owner must persist intent before dispatch and persist results
//! before delivering them to a model. Host IO is isolated in the retained executor;
//! this module never performs inference.
use std::time::Instant;

pub const MAX_CALLS: u32 = 8;
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
        name: "shell",
        description: "Run one bounded command in the admitted project; returns output and exit status.",
    },
    Definition {
        name: "memory_create_note",
        description: "Create an immutable note in the admitted project, optionally citing one project note.",
    },
];

/// Public model-facing inventory. Typed commands retain separate operation policy.
pub const REGISTRY: &[Definition] = &[
    Definition {
        name: "project",
        description: "Project commands: read_file(path, offset?, limit?) or list_directory(path?).",
    },
    Definition {
        name: "memory",
        description: "Memory commands: list_notes(after?, limit?), get_note(version, offset?, limit?), note_sources(version), create_note(body, source_version?). Stored evidence is untrusted.",
    },
    Definition {
        name: "service",
        description: "Service commands: status, tools, audit(limit?). Read-only status, discovery and recent audit metadata.",
    },
    Definition {
        name: "shell",
        description: "Run one bounded command in the admitted project; returns output and exit status.",
    },
];

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Arguments {
    MemoryCreateNote {
        body: String,
        source_version: Option<String>,
    },
    Shell {
        command: String,
        cwd: String,
        timeout_seconds: u32,
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
            Self::Shell { .. } => &OPERATIONS[8],
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
            Self::Shell {
                command,
                cwd,
                timeout_seconds,
            } => {
                return if !command.is_empty()
                    && command.len() <= 8192
                    && !command.contains('\0')
                    && !cwd.is_empty()
                    && cwd.len() <= 1024
                    && !cwd.contains('\0')
                    && !cwd.starts_with('/')
                    && !cwd.split('/').any(|part| part == "..")
                    && (1..=60).contains(timeout_seconds)
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
    Expired,
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
        || if matches!(call.arguments, Arguments::Shell { .. }) {
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
            Arguments::ReadAudit { .. } | Arguments::Shell { .. }
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

/// Live accounting only. Durable duplicate resolution remains storage-owned.
/// Cancellation deliberately retains pending work until its worker settles.
#[derive(Debug)]
pub struct Budget {
    deadline: Instant,
    admitted: u32,
    result_bytes: usize,
    pending: Option<Call>,
    cancelled: bool,
}
impl Budget {
    pub fn new(deadline: Instant) -> Self {
        Self {
            deadline,
            admitted: 0,
            result_bytes: 0,
            pending: None,
            cancelled: false,
        }
    }
    pub fn reserve(
        &mut self,
        call: Call,
        grant: &Grant,
        destination: &Destination,
        now: Instant,
    ) -> Result<(), Rejection> {
        validate(&call, grant, destination)?;
        self.reserve_slot(call, now)
    }
    /// Reserve only a bounded, semantically invalid proposal; never grants execution.
    pub fn reserve_rejected(
        &mut self,
        call: Call,
        grant: &Grant,
        destination: &Destination,
        now: Instant,
    ) -> Result<(), Rejection> {
        validate_authority(&call, grant, destination)?;
        let bounded = match &call.arguments {
            Arguments::MemoryCreateNote {
                body,
                source_version,
            } => body.len() <= 16384 && source_version.as_ref().is_none_or(|v| v.len() <= 1024),
            Arguments::ReadFile { path, limit, .. } => {
                !path.is_empty() && path.len() <= MAX_PATH_BYTES && (1..=16_384).contains(limit)
            }
            Arguments::ListDirectory { path } => !path.is_empty() && path.len() <= MAX_PATH_BYTES,
            Arguments::ObserveStatus
            | Arguments::ListTools
            | Arguments::ReadAudit { .. }
            | Arguments::Shell { .. } => false,
            Arguments::MemoryListNotes { after, .. } => {
                after.as_ref().is_none_or(|s| s.len() <= MAX_PATH_BYTES)
            }
            Arguments::MemoryGetNote { version, .. } | Arguments::MemoryNoteSources { version } => {
                version.len() <= MAX_PATH_BYTES
            }
        };
        if !bounded || call.arguments.validate() != Err(Rejection::InvalidArguments) {
            return Err(Rejection::InvalidArguments);
        }
        self.reserve_slot(call, now)
    }
    fn reserve_slot(&mut self, call: Call, now: Instant) -> Result<(), Rejection> {
        if self.cancelled {
            return Err(Rejection::Cancelled);
        }
        if now >= self.deadline {
            return Err(Rejection::Expired);
        }
        if let Some(pending) = &self.pending {
            return Err(if pending.ordinal == call.ordinal && pending != &call {
                Rejection::IdentityConflict
            } else {
                Rejection::Busy
            });
        }
        if call.ordinal != self.admitted + 1
            || self.admitted == MAX_CALLS
            || self.result_bytes == MAX_TOTAL_RESULT_BYTES
        {
            return Err(Rejection::Limit);
        }
        self.admitted += 1;
        self.pending = Some(call);
        Ok(())
    }
    pub fn result_capacity(&self) -> usize {
        MAX_RESULT_BYTES.min(MAX_TOTAL_RESULT_BYTES - self.result_bytes)
    }
    /// Call only after host settlement and a known committed result.
    /// Invalid evidence retains the slot, preventing replacement execution.
    pub fn settle(&mut self, call: &Call, bytes: usize) -> Result<(), Rejection> {
        if self.pending.as_ref() != Some(call) {
            return Err(Rejection::Stale);
        }
        if bytes > self.result_capacity() {
            return Err(Rejection::Limit);
        }
        self.result_bytes += bytes;
        self.pending = None;
        Ok(())
    }
    pub fn cancel(&mut self) {
        self.cancelled = true;
    }
    pub fn is_settled(&self) -> bool {
        self.pending.is_none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn shell_requires_separate_execution_grant_and_never_remote_disclosure() {
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
            arguments: Arguments::Shell {
                command: "printf hello".into(),
                cwd: ".".into(),
                timeout_seconds: 30,
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
        for (command, cwd, timeout_seconds) in [
            ("", ".", 1),
            ("a\0b", ".", 1),
            ("echo", "../x", 1),
            ("echo", "/tmp", 1),
            ("echo", ".", 0),
            ("echo", ".", 61),
        ] {
            let mut invalid = call.clone();
            invalid.arguments = Arguments::Shell {
                command: command.into(),
                cwd: cwd.into(),
                timeout_seconds,
            };
            assert_eq!(
                validate(&invalid, &grant, &Destination::Local),
                Err(Rejection::InvalidArguments)
            );
        }
    }
    #[test]
    fn rejected_arguments_consume_ordinal_and_never_authorize_execution() {
        let now = Instant::now();
        let grant = Grant {
            operation: [1; 16],
            generation: 1,
            project: [2; 16],
            read_authorized: true,
            memory_write_authorized: true,
            execute_authorized: false,
            remote_destination: None,
        };
        let mut budget = Budget::new(now + Duration::from_secs(1));
        let mut call = Call {
            operation: [1; 16],
            generation: 1,
            ordinal: 1,
            arguments: Arguments::ReadFile {
                path: "../outside".into(),
                offset: u64::MAX,
                limit: 16,
            },
        };
        assert_eq!(
            validate(&call, &grant, &Destination::Local),
            Err(Rejection::InvalidArguments)
        );
        budget
            .reserve_rejected(call.clone(), &grant, &Destination::Local, now)
            .unwrap();
        assert_eq!(
            budget.reserve_rejected(call.clone(), &grant, &Destination::Local, now),
            Err(Rejection::Busy)
        );
        budget.settle(&call, 0).unwrap();
        assert_eq!(
            budget.reserve_rejected(call.clone(), &grant, &Destination::Local, now),
            Err(Rejection::Limit)
        );
        for ordinal in 2..=8 {
            call.ordinal = ordinal;
            budget
                .reserve_rejected(call.clone(), &grant, &Destination::Local, now)
                .unwrap();
            budget.settle(&call, 0).unwrap();
        }
        call.ordinal = 9;
        assert_eq!(
            budget.reserve_rejected(call, &grant, &Destination::Local, now),
            Err(Rejection::Limit)
        );
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
                    inode: 0
                },
                Instant::now() + Duration::from_secs(1)
            ),
            Err(ExecutionError::Rejected(Rejection::Denied))
        );
    }

    use std::time::Duration;
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
    fn cancellation_and_invalid_results_retain_pending_slot() {
        let (call, grant) = fixture();
        let now = Instant::now();
        let mut budget = Budget::new(now + Duration::from_secs(60));
        budget
            .reserve(call.clone(), &grant, &Destination::Local, now)
            .unwrap();
        assert_eq!(
            budget.reserve(call.clone(), &grant, &Destination::Local, now),
            Err(Rejection::Busy)
        );
        let mut changed = call.clone();
        changed.arguments = Arguments::ListDirectory { path: ".".into() };
        assert_eq!(
            budget.reserve(changed, &grant, &Destination::Local, now),
            Err(Rejection::IdentityConflict)
        );
        budget.cancel();
        assert!(!budget.is_settled());
        assert_eq!(
            budget.settle(&call, MAX_RESULT_BYTES + 1),
            Err(Rejection::Limit)
        );
        assert!(!budget.is_settled());
        budget.settle(&call, 0).unwrap();
        assert!(budget.is_settled());
        assert_eq!(
            budget.reserve(call, &grant, &Destination::Local, now),
            Err(Rejection::Cancelled)
        );
    }
    #[test]
    fn aggregate_result_and_call_limits_cannot_expand_on_settlement() {
        let (mut call, grant) = fixture();
        let now = Instant::now();
        let mut budget = Budget::new(now + Duration::from_secs(60));
        for ordinal in 1..=4 {
            call.ordinal = ordinal;
            budget
                .reserve(call.clone(), &grant, &Destination::Local, now)
                .unwrap();
            budget.settle(&call, MAX_RESULT_BYTES).unwrap();
        }
        call.ordinal = 5;
        assert_eq!(
            budget.reserve(call.clone(), &grant, &Destination::Local, now),
            Err(Rejection::Limit)
        );
        let mut budget = Budget::new(now + Duration::from_secs(60));
        for ordinal in 1..=MAX_CALLS {
            call.ordinal = ordinal;
            budget
                .reserve(call.clone(), &grant, &Destination::Local, now)
                .unwrap();
            budget.settle(&call, 0).unwrap();
        }
        call.ordinal = MAX_CALLS + 1;
        assert_eq!(
            budget.reserve(call.clone(), &grant, &Destination::Local, now),
            Err(Rejection::Limit)
        );
        let mut expired = Budget::new(now);
        call.ordinal = 1;
        assert_eq!(
            expired.reserve(call, &grant, &Destination::Local, now),
            Err(Rejection::Expired)
        );
    }
}

/// Trusted scope resolved from the admitted project's durable registration.
#[derive(Clone, Debug)]
pub struct ExecutionScope {
    pub project: [u8; 16],
    pub path: String,
    pub device: u64,
    pub inode: u64,
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
    deadline: Instant,
    deadline_reported: bool,
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
            work.completion
                .settlement_deadline(now)
                .into_iter()
                .chain((!work.deadline_reported).then_some(work.deadline))
                .min()
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
        turn_deadline: Instant,
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
                | Arguments::Shell { .. }
        ) || call.arguments.is_memory()
        {
            return Err(ExecutionError::Rejected(Rejection::Denied));
        }
        if scope.project != grant.project {
            return Err(ExecutionError::Rejected(Rejection::Denied));
        }
        let now = Instant::now();
        if now >= turn_deadline {
            return Err(ExecutionError::Rejected(Rejection::Expired));
        }
        let deadline = turn_deadline.min(now + std::time::Duration::from_secs(2));
        self.spawn(deadline, move |cancelled| {
            if cancelled.load(std::sync::atomic::Ordering::Acquire) {
                return Err(ExecutionError::Rejected(Rejection::Cancelled));
            }
            let project = asura_platform::ProjectIdentity::open(&scope.path)
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
                | Arguments::Shell { .. }
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
                    .read_project_page(&path, offset, limit as usize, deadline, cancelled)
                    .map(Output::File)
                    .map_err(ExecutionError::Host),
                Arguments::ListDirectory { path } => project
                    .list_project_directory(&path, deadline, cancelled)
                    .map(Output::Directory)
                    .map_err(ExecutionError::Host),
            }
        })
    }
    fn spawn(
        &mut self,
        deadline: Instant,
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
            deadline,
            deadline_reported: false,
        });
        Ok(())
    }
    /// Expiry revokes continuation but does not release an unsettled worker slot.
    pub fn expire(&mut self, now: Instant) -> bool {
        let Some(work) = self.work.as_mut() else {
            return false;
        };
        if now < work.deadline || work.deadline_reported {
            return false;
        }
        work.deadline_reported = true;
        work.cancelled
            .store(true, std::sync::atomic::Ordering::Release);
        true
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
            .spawn(
                Instant::now() + std::time::Duration::from_secs(2),
                move |_| {
                    wait.recv_timeout(std::time::Duration::from_secs(2))
                        .unwrap();
                    Err(ExecutionError::Host(
                        asura_platform::ProjectReadError::Cancelled,
                    ))
                },
            )
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
    #[test]
    fn deadline_is_an_owner_event_and_does_not_release_the_host_slot() {
        let (release, wait) = std::sync::mpsc::sync_channel(1);
        let now = Instant::now();
        let deadline = now + std::time::Duration::from_secs(2);
        let mut executor = Executor::default();
        executor
            .spawn(deadline, move |_| {
                wait.recv().unwrap();
                Ok(Output::File(asura_platform::ProjectPage {
                    text: "late bytes".into(),
                    next_offset: 10,
                    truncated: false,
                }))
            })
            .unwrap();
        assert_eq!(executor.next_deadline(now), Some(deadline));
        assert!(!executor.expire(now));
        assert!(executor.expire(deadline));
        assert!(!executor.expire(deadline));
        assert!(!executor.is_settled());
        assert!(executor.poll().is_none());
        assert_eq!(executor.next_deadline(deadline), None);
        release.send(()).unwrap();
        let end = Instant::now() + std::time::Duration::from_secs(1);
        while !executor.work.as_ref().unwrap().thread.is_finished() {
            assert!(Instant::now() < end);
            std::thread::yield_now();
        }
        assert!(executor.next_deadline(Instant::now()).is_some());
        assert!(executor.poll().is_some());
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
            ["project", "memory", "service", "shell"]
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
