use std::ops::Range;

pub type Id = [u8; 16];
pub type Hash = [u8; 32];
pub const MAX_PROJECTS: usize = 64;
pub const MAX_CONVERSATIONS: usize = 256;
pub const OUTPUT_RESERVATION: u32 = 2048;
/// Historical format-1 admissions retain their recorded reservation.
pub const LEGACY_OUTPUT_RESERVATION: u32 = 512;
pub const MAX_PROMPT: usize = 32 * 1024;
pub const MAX_TEXT: usize = 60 * 1024;
pub const SHELL_TOOL_KIND: u8 = 16;
pub const MAX_SHELL_COMMAND_BYTES: usize = 8192;
pub const MAX_SHELL_CWD_BYTES: usize = 1024;
pub const MAX_SHELL_INTENT_BYTES: usize = MAX_SHELL_COMMAND_BYTES + 1 + MAX_SHELL_CWD_BYTES;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FrameContext {
    pub installation_id: Id,
    pub transition_id: Id,
    pub sequence: u64,
    pub prior_digest: Hash,
    pub expected_revision: u64,
    pub owner_generation: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Request<'a> {
    Initialize {
        mode: u8,
    },
    Register {
        location: &'a str,
    },
    RenameProject {
        project: Id,
        expected_name_revision: u64,
        name: &'a str,
    },
    Submit {
        project: Id,
        conversation: Option<Id>,
        expected_generation: u64,
        prompt: &'a str,
    },
    Queue {
        project: Id,
        conversation: Id,
        target_operation: Id,
        target_generation: u64,
        kind: InputKind,
        prompt: &'a str,
    },
    QueueInput {
        project: Id,
        conversation: Option<Id>,
        expected_generation: u64,
        new_conversation: bool,
        prompt: &'a str,
    },
    ReorderInput {
        input: Id,
        after: Option<Id>,
        expected_order_revision: u64,
    },
    InputDecision {
        input: Id,
        action: InputAction,
    },
    PromoteInput {
        input: Id,
        target_operation: Id,
        target_generation: u64,
    },
    Cancel {
        operation: Id,
        generation: u64,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingInit {
    pub request: Id,
    pub digest: Hash,
    pub mode: u8,
    pub configuration_revision: u64,
    pub configuration_digest: Hash,
    pub graph: Id,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActiveBinding {
    pub request: Id,
    pub generation: u64,
    pub graph: Id,
    pub configuration_digest: Hash,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectRegistered {
    pub request: Id,
    pub digest: Hash,
    pub project: Id,
    pub location: String,
    pub device: u64,
    pub inode: u64,
    pub registry_revision: u64,
    pub visibility: u8,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectRequestAlias {
    pub request: Id,
    pub digest: Hash,
    pub location: String,
    pub project: Id,
    pub registry_revision: u64,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectRenamed {
    pub request: Id,
    pub digest: Hash,
    pub project: Id,
    pub expected_name_revision: u64,
    pub name_revision: u64,
    pub requested_name: String,
    pub name: String,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectName {
    pub name: String,
    pub revision: u64,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TurnAccepted {
    pub request: Id,
    pub digest: Hash,
    pub project: Id,
    pub original_conversation: Option<Id>,
    pub expected_generation: u64,
    pub conversation: Id,
    pub generation: u64,
    pub task: Id,
    pub operation: Id,
    pub model: String,
    pub configuration_digest: Hash,
    pub instructions_digest: Hash,
    pub input_digest: Hash,
    pub prior_operations: Vec<Id>,
    pub prompt: String,
    pub reserved_output_tokens: u32,
    pub event_cursor: u64,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StartAuthorized {
    pub operation: Id,
    pub generation: u64,
    pub helper: Id,
    pub input_digest: Hash,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CancelRequested {
    pub operation: Id,
    pub generation: u64,
    pub cause: Cause,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum TerminalKind {
    Complete = 1,
    Failed = 2,
    Cancelled = 3,
    Interrupted = 4,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Cause {
    None = 0,
    UserCancel = 1,
    Deadline = 2,
    OutputLimit = 3,
    ServiceShutdown = 4,
    ProviderFailure = 5,
    ProtocolFailure = 6,
    Restart = 7,
    AuthorityFailure = 8,
    InputLimit = 9,
    Steering = 10,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TurnTerminal {
    pub operation: Id,
    pub generation: u64,
    pub kind: TerminalKind,
    pub cause: Cause,
    pub final_cursor: u64,
    pub usage_known: bool,
    pub output_tokens: u32,
    pub charged_tokens: u32,
    pub text: String,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Record {
    PendingInit(PendingInit),
    ActiveBinding(ActiveBinding),
    OwnerGeneration,
    ProjectRegistered(ProjectRegistered),
    ProjectRequestAlias(ProjectRequestAlias),
    ProjectRenamed(ProjectRenamed),
    TurnAccepted(Box<TurnAccepted>),
    StartAuthorized(StartAuthorized),
    CancelRequested(CancelRequested),
    TurnTerminal(TurnTerminal),
    InputQueued(InputQueued),
    InputQueuedV2(InputQueuedV2),
    InputReordered(InputReordered),
    InputDecision(InputDecision),
    InputPromoted(InputPromoted),
    MemoryCreateIntent(MemoryCreateIntent),
    ToolIntent(ToolIntent),
    ToolResult(ToolResult),
}
impl Record {
    pub fn kind(&self) -> u16 {
        match self {
            Self::PendingInit(_) => 10,
            Self::ActiveBinding(_) => 11,
            Self::OwnerGeneration => 3,
            Self::ProjectRegistered(_) => 4,
            Self::TurnAccepted(_) => 5,
            Self::StartAuthorized(_) => 6,
            Self::CancelRequested(_) => 7,
            Self::TurnTerminal(_) => 8,
            Self::ProjectRequestAlias(_) => 9,
            Self::ProjectRenamed(_) => 20,
            Self::InputQueued(_) => 12,
            Self::InputQueuedV2(_) => 18,
            Self::InputReordered(_) => 19,
            Self::InputDecision(_) => 13,
            Self::MemoryCreateIntent(_) => 17,
            Self::ToolIntent(_) => 14,
            Self::ToolResult(_) => 15,
            Self::InputPromoted(_) => 16,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RequestResult {
    pub digest: Hash,
    pub result: Outcome,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    Initialization {
        active: bool,
    },
    Project(Id),
    ProjectRename {
        project: Id,
        name: String,
        name_revision: u64,
        changed: bool,
    },
    Input(Id),
    QueueInput {
        input: Id,
        conversation: Id,
        order_revision: u64,
    },
    ReorderInput {
        input: Id,
        order_revision: u64,
    },
    InputDecision(Id),
    Turn {
        conversation: Id,
        generation: u64,
        task: Id,
        operation: Id,
    },
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Conversation {
    pub project: Id,
    pub generation: u64,
}
/// Bodies are deliberately not retained in replay indexes. Decode the validated
/// frame range from the same immutable journal bytes when a response is needed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Operation {
    pub reserved_output_tokens: u32,
    pub conversation: Id,
    pub generation: u64,
    pub task: Id,
    pub owner_generation: u64,
    pub input_digest: Hash,
    pub accepted_frame: Range<usize>,
    pub helper: Option<Id>,
    pub cancel: Option<Cause>,
    pub terminal: Option<Terminal>,
    pub tools: Vec<ToolRecord>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Terminal {
    pub kind: TerminalKind,
    pub cause: Cause,
    pub usage_known: bool,
    pub output_tokens: u32,
    pub charged_tokens: u32,
    pub frame: Range<usize>,
}

pub const MAX_INPUTS: usize = 16;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum InputKind {
    Queue = 1,
    Steer = 2,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum InputAction {
    Resume = 1,
    Drop = 2,
    Hold = 3,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum InputStatus {
    Queued = 1,
    Held = 2,
    Running = 3,
    Complete = 4,
    Failed = 5,
    Dropped = 6,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InputQueued {
    pub request: Id,
    pub digest: Hash,
    pub dispatch_request: Id,
    pub project: Id,
    pub conversation: Id,
    pub target_operation: Id,
    pub target_generation: u64,
    pub previous: Option<Id>,
    pub kind: InputKind,
    pub prompt: String,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InputQueuedV2 {
    pub request: Id,
    pub digest: Hash,
    pub input: Id,
    pub dispatch_request: Id,
    pub project: Id,
    pub conversation: Id,
    pub expected_generation: u64,
    pub new_conversation: bool,
    pub prompt: String,
    pub order_revision: u64,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InputReordered {
    pub request: Id,
    pub digest: Hash,
    pub input: Id,
    pub after: Option<Id>,
    pub expected_order_revision: u64,
    pub order_revision: u64,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InputDecision {
    pub request: Id,
    pub digest: Hash,
    pub input: Id,
    pub action: InputAction,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueuedInput {
    pub request: Id,
    pub dispatch_request: Id,
    pub project: Id,
    pub conversation: Id,
    pub target_operation: Id,
    pub target_generation: u64,
    pub previous: Option<Id>,
    pub kind: InputKind,
    pub frame: Range<usize>,
    pub sequence: u64,
    pub owner_generation: u64,
    pub held: bool,
    pub dropped: bool,
    pub resumed: bool,
    pub v2: bool,
    pub order_position: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
/// Format-1 kinds: 1 file, 2 directory, 3 status, 4–5 rejected file/directory;
/// 6 memory list, 7 memory get, 8 memory sources, 9–11 their rejected proposals.
/// Kind 15 is bounded recent audit inspection.
/// Kind 16 stores exact command + NUL + cwd in path, effective milliseconds in
/// offset, and requested timeout seconds in limit. It never authorizes replay.
/// Kind 14 is static tool inventory; kinds 12–13 are reserved for memory creation.
/// For memory kinds `path` stores a version/cursor ID, never a filesystem path.
pub struct ToolIntent {
    pub operation: Id,
    pub generation: u64,
    pub ordinal: u32,
    pub kind: u8,
    pub path: String,
    pub offset: u64,
    pub limit: u32,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolResult {
    pub operation: Id,
    pub generation: u64,
    pub ordinal: u32,
    pub status: u8,
    pub text: String,
    pub next_offset: Option<u64>,
    pub truncated: bool,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolRecord {
    pub create: Option<Box<MemoryCreateIntent>>,
    pub kind: u8,
    pub offset: u64,
    pub limit: u32,
    pub result_status: Option<u8>,
    pub intent_frame: Range<usize>,
    pub result_frame: Option<Range<usize>>,
    pub result_bytes: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InputPromoted {
    pub request: Id,
    pub digest: Hash,
    pub input: Id,
    pub target_operation: Id,
    pub target_generation: u64,
}

/// Durable authorization evidence for one immutable note creation, not replay permission.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemoryCreateIntent {
    pub operation: Id,
    pub generation: u64,
    pub ordinal: u32,
    pub project: Id,
    pub memory_operation: Id,
    pub object: Id,
    pub version: Id,
    pub body_bytes: u32,
    pub body_sha256: Hash,
    pub source: Option<(Id, Id)>,
    pub command_sha256: Hash,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CreateNoteIdentity {
    pub operation: Id,
    pub object: Id,
    pub version: Id,
    pub edge: Id,
}
