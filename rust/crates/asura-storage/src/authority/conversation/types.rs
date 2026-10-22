pub use asura_platform::secrets::{LossReason, LossSpan, NumberOrigin, RefusalCode, SecretKind};

pub type Id = [u8; 16];
pub type Hash = [u8; 32];
/// Projects kept in the live state by newest activity, plus pinned ones
/// (segmented journal). A memory bound, not a registration limit.
pub const RECENT_PROJECTS: usize = 64;
/// Conversations kept in the live state by newest activity, plus pinned ones.
pub const RECENT_CONVERSATIONS: usize = 256;
/// Settled inputs kept in the live state for queue projection.
pub const RECENT_SETTLED_INPUTS: usize = 64;
/// One response's piece list while its operation is active (SJ-D9).
pub const MAX_RESPONSE_PIECES: usize = 65_536;

/// The address of one journal frame: its sequence, the first sequence of the
/// segment file that holds it, its byte offset in that file and its length.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Locator {
    pub sequence: u64,
    pub segment: u64,
    pub offset: u32,
    pub length: u32,
}
impl Locator {
    pub const NONE: Self = Self {
        sequence: 0,
        segment: 0,
        offset: 0,
        length: 0,
    };
    pub fn is_none(&self) -> bool {
        self.sequence == 0
    }
    /// The frame's byte range within its segment file.
    pub fn range(&self) -> std::ops::Range<usize> {
        self.offset as usize..self.offset as usize + self.length as usize
    }
    pub fn encode(&self) -> [u8; 24] {
        let mut out = [0; 24];
        out[..8].copy_from_slice(&self.sequence.to_be_bytes());
        out[8..16].copy_from_slice(&self.segment.to_be_bytes());
        out[16..20].copy_from_slice(&self.offset.to_be_bytes());
        out[20..].copy_from_slice(&self.length.to_be_bytes());
        out
    }
    pub fn decode(bytes: &[u8]) -> Self {
        Self {
            sequence: u64::from_be_bytes(bytes[..8].try_into().expect("24-byte locator")),
            segment: u64::from_be_bytes(bytes[8..16].try_into().expect("24-byte locator")),
            offset: u32::from_be_bytes(bytes[16..20].try_into().expect("24-byte locator")),
            length: u32::from_be_bytes(bytes[20..24].try_into().expect("24-byte locator")),
        }
    }
}
pub const MAX_PROMPT: usize = 32 * 1024;
/// Response text in one kind 24 segment or one kind 8 tail: a per-frame bound,
/// not a response total (response-text-transfer.md).
pub const MAX_TEXT: usize = 60 * 1024;

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
    pub event_cursor: u64,
    /// The context composer's record; every accepted turn carries one (kind 21).
    pub composition: Composition,
    /// The thinking level the turn used: 0 default, 1 off, 2 low, 3 medium,
    /// 4 high (thinking-level.md).
    pub thinking_level: u8,
    /// The form sent: 0 none, 1 off, 2 on, 3 low, 4 medium, 5 high.
    pub thinking_request: u8,
}
/// Largest kind 21 thinking level code.
pub const MAX_THINKING_LEVEL: u8 = 4;
/// Largest kind 21 thinking request code.
pub const MAX_THINKING_REQUEST: u8 = 5;
/// Shown prior turns of a composed turn; `prior_operations` lists them.
pub const MAX_COMPOSED_PRIOR: usize = 1024;
/// The context composer's record of one turn-start composition (N1).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Composition {
    pub omitted_turns: u32,
    pub manifest_digest: Hash,
    pub composer_revision: u32,
    pub window_tokens: u32,
    pub output_reservation: u32,
    pub overhead_tokens: u32,
    pub bytes_per_token_milli: u32,
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
impl TerminalKind {
    pub fn from_code(code: u8) -> Option<Self> {
        Some(match code {
            1 => Self::Complete,
            2 => Self::Failed,
            3 => Self::Cancelled,
            4 => Self::Interrupted,
            _ => return None,
        })
    }
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
impl Cause {
    pub fn from_code(code: u8) -> Option<Self> {
        Some(match code {
            0 => Self::None,
            1 => Self::UserCancel,
            2 => Self::Deadline,
            3 => Self::OutputLimit,
            4 => Self::ServiceShutdown,
            5 => Self::ProviderFailure,
            6 => Self::ProtocolFailure,
            7 => Self::Restart,
            8 => Self::AuthorityFailure,
            9 => Self::InputLimit,
            10 => Self::Steering,
            _ => return None,
        })
    }
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
    /// Bytes of the journaled response (kind 24 segments) that `text` follows.
    pub text_offset: u64,
    /// The response tail, at most `MAX_TEXT` bytes.
    pub text: String,
}
/// Kind 24: one segment of an active response. Keep `offset` bytes of the
/// journaled response, then append `text` (response-text-transfer.md).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResponseText {
    pub operation: Id,
    pub generation: u64,
    pub offset: u64,
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
    InputReordered(InputReordered),
    InputDecision(InputDecision),
    InputPromoted(InputPromoted),
    /// Kind 14: one model tool call, the only copy of its arguments.
    ToolCall(Box<ToolCall>),
    ToolResult(ToolResult),
    /// Kind 23 (J1): capture facts of a `run` exec_cmd call, before kind 15.
    CaptureSettled(Box<CaptureSettled>),
    /// Kind 24: one response text segment.
    ResponseText(ResponseText),
    /// Kind 25: one thinking text segment, the same layout applied to the
    /// operation's thinking text (thinking-level.md).
    ThinkingText(ResponseText),
    /// Kind 26 (J2): a job's intent. Every other intent fact is derived.
    JobIntent(JobIntent),
    /// Kind 27 (J2): the first accepted cancel trigger of a job.
    JobCancelRequested(JobCancelRequested),
    /// Kind 28 (J2): a job's settlement, with the capture facts of kind 23.
    JobSettled(Box<JobSettled>),
}
impl Record {
    pub fn kind(&self) -> u16 {
        match self {
            Self::PendingInit(_) => 10,
            Self::ActiveBinding(_) => 11,
            Self::OwnerGeneration => 3,
            Self::ProjectRegistered(_) => 4,
            Self::TurnAccepted(_) => 21,
            Self::StartAuthorized(_) => 6,
            Self::CancelRequested(_) => 7,
            Self::TurnTerminal(_) => 8,
            Self::ProjectRequestAlias(_) => 9,
            Self::ProjectRenamed(_) => 20,
            Self::InputQueued(_) => 18,
            Self::InputReordered(_) => 19,
            Self::InputDecision(_) => 13,
            Self::ToolCall(_) => 14,
            Self::ToolResult(_) => 15,
            Self::InputPromoted(_) => 16,
            Self::CaptureSettled(_) => 23,
            Self::ResponseText(_) => 24,
            Self::ThinkingText(_) => 25,
            Self::JobIntent(_) => 26,
            Self::JobCancelRequested(_) => 27,
            Self::JobSettled(_) => 28,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RequestResult {
    pub digest: Hash,
    pub result: Outcome,
    /// Record kind of the frame that holds the result.
    pub kind: u16,
    /// The frame that holds the result.
    pub frame: Locator,
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
/// One registered project in the live state (recent or pinned).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectEntry {
    pub registration: ProjectRegistered,
    /// The kind 4 frame.
    pub registered: Locator,
    pub name: Option<ProjectName>,
    pub latest: Option<ProjectLatest>,
    /// Sequence of the newest committed record that names the project.
    pub activity: u64,
}
/// The project's newest accepted turn.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProjectLatest {
    pub conversation: Id,
    pub operation: Id,
    pub generation: u64,
    pub accepted: u64,
    /// Sequence of its kind 8, once terminal.
    pub terminal: Option<u64>,
}
/// The newest turn of a conversation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LatestTurn {
    pub operation: Id,
    pub terminal: Option<TerminalKind>,
    pub accepted: Locator,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Conversation {
    pub project: Id,
    pub generation: u64,
    /// The frame that created the conversation (kind 18 or 21).
    pub created: Locator,
    pub latest: Option<LatestTurn>,
    /// Completed turns so far (SJ-D12).
    pub completed: u64,
    /// Sequence of the newest committed record that names the conversation.
    pub activity: u64,
}
/// Bodies are deliberately not retained in replay indexes. Decode the validated
/// frame through its locator when a response is needed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Operation {
    pub conversation: Id,
    pub generation: u64,
    pub task: Id,
    pub owner_generation: u64,
    pub input_digest: Hash,
    pub accepted_frame: Locator,
    pub helper: Option<Id>,
    pub cancel: Option<Cause>,
    pub terminal: Option<Terminal>,
    pub tools: Vec<ToolRecord>,
    /// The journaled response: kind 24 segments, then the kind 8 tail.
    pub response: Response,
    /// The journaled thinking text: kind 25 segments (thinking-level.md).
    pub thinking: Response,
}
/// `kept` bytes of the text in the kind 24 or kind 8 frame at `frame`, at
/// byte `start` of the response. Replay clones no text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResponsePiece {
    pub start: u64,
    pub kept: u64,
    pub frame: Locator,
}
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Response {
    pub len: u64,
    pub pieces: Vec<ResponsePiece>,
}
impl Response {
    /// Keep `offset` bytes, then append `appended` bytes held in `frame`.
    /// Returns `None` when `offset` exceeds the length.
    pub fn apply(&mut self, offset: u64, appended: u64, frame: Locator) -> Option<()> {
        if offset > self.len {
            return None;
        }
        self.pieces.retain(|piece| piece.start < offset);
        if let Some(last) = self.pieces.last_mut() {
            last.kept = last.kept.min(offset - last.start);
        }
        if appended > 0 {
            self.pieces.push(ResponsePiece {
                start: offset,
                kept: appended,
                frame,
            });
        }
        self.len = offset.checked_add(appended)?;
        Some(())
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Terminal {
    pub kind: TerminalKind,
    pub cause: Cause,
    pub usage_known: bool,
    pub output_tokens: u32,
    pub frame: Locator,
    /// Completed turns of the conversation through this one (SJ-D12).
    pub completed: u64,
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
    /// Expected generation, or the promotion target generation for Steer.
    pub target_generation: u64,
    pub kind: InputKind,
    pub frame: Locator,
    pub sequence: u64,
    pub owner_generation: u64,
    pub held: bool,
    pub dropped: bool,
    pub resumed: bool,
    pub order_position: u64,
    /// The kind 16 promotion frame, for a Steer input.
    pub promoted: Option<Locator>,
    /// The turn its dispatch request started.
    pub dispatched: Option<(Id, u64)>,
    /// The terminal kind of that turn.
    pub dispatched_terminal: Option<TerminalKind>,
    /// The terminal kind and cause of a Steer input's target operation.
    pub target_terminal: Option<(TerminalKind, Cause)>,
    /// Sequence at which the input's status became final.
    pub settled: Option<u64>,
}

/// The four registered model-facing tool names (TCR-D24). Replay looks a
/// stored name up here; the service registry uses these constants.
pub const TOOL_NAMES: [&str; 4] = ["project", "memory", "service", "exec_cmd"];
/// Kind 14 bounds (tool-call-records.md, size limits).
pub const MAX_TOOL_ARGUMENTS: usize = 64_000;
pub const MAX_TOOL_NAME: usize = 256;
pub const MAX_STORED_TOOL_NAME: usize = 512;
pub const MAX_TOOL_NUMBERS: usize = 256;
/// Calls that may run or be rejected per turn; kind 15 ordinals are
/// 1..=MAX_RECORDED_CALLS (owner, 2026-10-18; tool-call-records.md TCR-D32).
pub const MAX_RECORDED_CALLS: u32 = 200;
/// The ordinal after the last recorded call exists only for the over-limit
/// `limit` record (or `cancelled`, TCR-D30).
pub const MAX_TOOL_CALL_ORDINAL: u32 = MAX_RECORDED_CALLS + 1;
/// A refused or whole-command exec_cmd command is cut at this size (field 20
/// bit 0).
pub const MAX_REFUSED_COMMAND_BYTES: usize = 8192;
/// The executable command replay derives for a whole-command call (field 20
/// bit 1, SL7); its secret variable count is 1.
pub use asura_platform::secrets::WHOLE_COMMAND;

/// A registered model-facing tool: the replay lookup of kind 14 field 5.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RegisteredTool {
    Project,
    Memory,
    Service,
    ExecCmd,
}
impl RegisteredTool {
    /// The registry lookup. An empty (not retained) name is unregistered.
    pub fn lookup(name: &str) -> Option<Self> {
        match TOOL_NAMES.iter().position(|n| *n == name)? {
            0 => Some(Self::Project),
            1 => Some(Self::Memory),
            2 => Some(Self::Service),
            _ => Some(Self::ExecCmd),
        }
    }
    pub fn name(self) -> &'static str {
        TOOL_NAMES[self as usize]
    }
}
/// Kind 14 field 4.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum ToolCallSource {
    GeneratedContent = 1,
    ProviderJson = 2,
}
/// Kind 14 field 7: the [operation table](tool-call-records.md#operation-codes).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum ToolOperation {
    ReadFile = 1,
    ListDirectory = 2,
    Status = 3,
    Tools = 4,
    Audit = 5,
    ListNotes = 6,
    GetNote = 7,
    NoteSources = 8,
    CreateNote = 9,
    ExecCmd = 10,
}
impl ToolOperation {
    pub const ALL: [Self; 10] = [
        Self::ReadFile,
        Self::ListDirectory,
        Self::Status,
        Self::Tools,
        Self::Audit,
        Self::ListNotes,
        Self::GetNote,
        Self::NoteSources,
        Self::CreateNote,
        Self::ExecCmd,
    ];
    pub fn from_code(code: u8) -> Option<Self> {
        Self::ALL.get(usize::from(code).checked_sub(1)?).copied()
    }
    pub fn tool(self) -> RegisteredTool {
        match self {
            Self::ReadFile | Self::ListDirectory => RegisteredTool::Project,
            Self::Status | Self::Tools | Self::Audit => RegisteredTool::Service,
            Self::ListNotes | Self::GetNote | Self::NoteSources | Self::CreateNote => {
                RegisteredTool::Memory
            }
            Self::ExecCmd => RegisteredTool::ExecCmd,
        }
    }
    /// The model-facing command name (exec_cmd has none of its own).
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ReadFile => "read_file",
            Self::ListDirectory => "list_directory",
            Self::Status => "status",
            Self::Tools => "tools",
            Self::Audit => "audit",
            Self::ListNotes => "list_notes",
            Self::GetNote => "get_note",
            Self::NoteSources => "note_sources",
            Self::CreateNote => "create_note",
            Self::ExecCmd => "exec_cmd",
        }
    }
}
/// Kind 14 field 8: the [decision table](tool-call-records.md#decisions).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum ToolDecision {
    Run = 1,
    InvalidArguments = 2,
    UnknownTool = 3,
    Refused = 4,
    Busy = 5,
    Denied = 6,
    Limit = 7,
    Cancelled = 8,
}
impl ToolDecision {
    pub const ALL: [Self; 8] = [
        Self::Run,
        Self::InvalidArguments,
        Self::UnknownTool,
        Self::Refused,
        Self::Busy,
        Self::Denied,
        Self::Limit,
        Self::Cancelled,
    ];
    pub fn from_code(code: u8) -> Option<Self> {
        Self::ALL.get(usize::from(code).checked_sub(1)?).copied()
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Run => "run",
            Self::InvalidArguments => "invalid_arguments",
            Self::UnknownTool => "unknown_tool",
            Self::Refused => "refused",
            Self::Busy => "busy",
            Self::Denied => "denied",
            Self::Limit => "limit",
            Self::Cancelled => "cancelled",
        }
    }
    /// The status sent to the helper for a call that does not run (TCR-D12).
    pub fn status(self) -> Option<u8> {
        match self {
            Self::Run => None,
            Self::InvalidArguments | Self::UnknownTool | Self::Refused => Some(3),
            Self::Busy => Some(4),
            Self::Denied => Some(2),
            Self::Limit => Some(7),
            Self::Cancelled => Some(6),
        }
    }
}
/// Kind 14 field 9: the [fault table](tool-call-records.md#faults). The lowest
/// code that applies wins.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum ToolFault {
    InvalidJson = 1,
    JsonLimit = 2,
    NotObject = 3,
    DuplicateKey = 4,
    UnknownCommand = 5,
    MissingField = 6,
    UnexpectedField = 7,
    WrongType = 8,
    OutOfRange = 9,
    UnsafePath = 10,
    InvalidVersionId = 11,
    ArgumentsOverLimit = 12,
    NameOverLimit = 13,
    InvalidText = 14,
    UnregisteredName = 15,
}
impl ToolFault {
    pub const ALL: [Self; 15] = [
        Self::InvalidJson,
        Self::JsonLimit,
        Self::NotObject,
        Self::DuplicateKey,
        Self::UnknownCommand,
        Self::MissingField,
        Self::UnexpectedField,
        Self::WrongType,
        Self::OutOfRange,
        Self::UnsafePath,
        Self::InvalidVersionId,
        Self::ArgumentsOverLimit,
        Self::NameOverLimit,
        Self::InvalidText,
        Self::UnregisteredName,
    ];
    pub fn from_code(code: u8) -> Option<Self> {
        Self::ALL.get(usize::from(code).checked_sub(1)?).copied()
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::InvalidJson => "invalid_json",
            Self::JsonLimit => "json_limit",
            Self::NotObject => "not_object",
            Self::DuplicateKey => "duplicate_key",
            Self::UnknownCommand => "unknown_command",
            Self::MissingField => "missing_field",
            Self::UnexpectedField => "unexpected_field",
            Self::WrongType => "wrong_type",
            Self::OutOfRange => "out_of_range",
            Self::UnsafePath => "unsafe_path",
            Self::InvalidVersionId => "invalid_version_id",
            Self::ArgumentsOverLimit => "arguments_over_limit",
            Self::NameOverLimit => "name_over_limit",
            Self::InvalidText => "invalid_text",
            Self::UnregisteredName => "unregistered_name",
        }
    }
}
/// Kind 14 field 12: why the argument text is not retained (form 4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum NotRetainedReason {
    Oversized = 1,
    DetectionUnavailable = 2,
    UnverifiableEscape = 3,
}
impl NotRetainedReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Oversized => "oversized",
            Self::DetectionUnavailable => "detection_unavailable",
            Self::UnverifiableEscape => "unverifiable_escape",
        }
    }
}
/// Kind 14 fields 11-12.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArgumentForm {
    Verbatim,
    Canonical,
    Unparsed,
    NotRetained(NotRetainedReason),
}
impl ArgumentForm {
    pub fn code(self) -> u8 {
        match self {
            Self::Verbatim => 1,
            Self::Canonical => 2,
            Self::Unparsed => 3,
            Self::NotRetained(_) => 4,
        }
    }
    pub fn not_retained(self) -> Option<NotRetainedReason> {
        match self {
            Self::NotRetained(reason) => Some(reason),
            _ => None,
        }
    }
}
/// Kind 14, ToolCall (tool-call-records.md). One record for every tool call
/// that reaches the service in an admitted, started turn. The redacted name
/// and argument text are the only copy of the arguments. The record never
/// authorizes execution; derived values are recomputed on replay.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolCall {
    pub operation: Id,
    pub generation: u64,
    /// 1..=MAX_TOOL_CALL_ORDINAL; the last only with decision `limit` or `cancelled`.
    pub ordinal: u32,
    pub source: ToolCallSource,
    /// Redacted name, 0-512 bytes; empty only when not retained.
    pub name: String,
    /// Received name size, at least 1.
    pub name_bytes: u32,
    pub tool_operation: Option<ToolOperation>,
    pub decision: ToolDecision,
    /// Set only with `invalid_arguments` or `unknown_tool`.
    pub fault: Option<ToolFault>,
    /// Set only with `busy`.
    pub busy: Option<ExecCmdBusyReason>,
    pub form: ArgumentForm,
    /// Received argument size.
    pub arguments_bytes: u32,
    /// Forms 1-3: 0-64,000 bytes; form 4: empty.
    pub arguments: String,
    /// Entry i is the secret kind of `$ASURA_SECRET_<i+1>` in fields 5 and 14.
    pub number_kinds: Vec<SecretKind>,
    /// Occurrences of the unnumbered placeholder in fields 5 and 14.
    pub unnumbered: u32,
    /// The rule set used; 0 and zero only with reason `detection_unavailable`
    /// from a failed rule-set self-check.
    pub rule_set_revision: u32,
    pub rule_set_digest: Hash,
    /// Field 20 bit 0: a refused or whole-command exec_cmd command was cut
    /// at 8,192 bytes.
    pub command_cut: bool,
    /// Field 20 bit 1 (SL7): the `run` exec_cmd command runs through the
    /// whole-command variable; field 14 holds its display text.
    pub whole_command: bool,
    /// Fields 21-23: an exec_cmd secret refusal, or with `whole_command` the
    /// fallback reason. Never holds a value.
    pub refusal: Option<ExecCmdRefusal>,
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
/// Kind 14 field 10, for a `busy` decision.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum ExecCmdBusyReason {
    Quota = 1,
    CapturePaused = 2,
    PendingFull = 3,
    PayloadUnavailable = 4,
}
impl ExecCmdBusyReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Quota => "quota",
            Self::CapturePaused => "capture_paused",
            Self::PendingFull => "pending_full",
            Self::PayloadUnavailable => "payload_unavailable",
        }
    }
}
/// Kind 14 fields 21-23 for a refused exec_cmd call. Never holds a value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExecCmdRefusal {
    pub code: RefusalCode,
    pub kind: Option<SecretKind>,
    /// The 1-based word index, 1-4096.
    pub word: Option<u16>,
}
/// Initial capture reservation of a `run` exec_cmd call: a constant, never
/// stored (TCR-O5).
pub const INITIAL_RESERVATION: u64 = 1024 * 1024;
/// Kind 23 field 4.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum CaptureState {
    Complete = 1,
    Incomplete = 2,
    Unconfirmed = 3,
    NotStarted = 4,
}
impl CaptureState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Complete => "complete",
            Self::Incomplete => "incomplete",
            Self::Unconfirmed => "unconfirmed",
            Self::NotStarted => "not_started",
        }
    }
}
/// Kind 23 field 5.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum CaptureReason {
    None = 0,
    StorageFault = 1,
    Quota = 2,
    Enospc = 3,
    DetectorFault = 4,
    DiscardedAfterCancel = 5,
    SettlementUnconfirmed = 6,
}
impl CaptureReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::StorageFault => "storage_fault",
            Self::Quota => "quota",
            Self::Enospc => "enospc",
            Self::DetectorFault => "detector_fault",
            Self::DiscardedAfterCancel => "discarded_after_cancel",
            Self::SettlementUnconfirmed => "settlement_unconfirmed",
        }
    }
}
/// Kind 23 stream block: stored-stream facts, with no value or value digest.
/// `digest` is SHA-256 over exactly the stored (redacted) bytes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CaptureStream {
    pub observed_bytes: u64,
    pub stored_bytes: u64,
    pub digest: Option<Hash>,
    pub replacements: u64,
    pub unnumbered_replacements: u64,
    pub literal_replacements: u64,
    /// Non-zero counts in increasing kind order.
    pub kinds: Vec<(SecretKind, u64)>,
    pub loss_span_count: u64,
    pub withheld_bytes: u64,
    /// The first `min(loss_span_count, 64)` spans.
    pub spans: Vec<LossSpan>,
    pub degraded: bool,
    pub discarded_bytes: u64,
    pub eof: bool,
}
/// Kind 23 field 15 entry. "Propagated" is not an origin (G17).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CaptureNumber {
    pub number: u16,
    pub kind: SecretKind,
    pub origin: NumberOrigin,
}
/// Kind 23, CaptureSettled (J1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CaptureSettled {
    pub operation: Id,
    pub generation: u64,
    pub ordinal: u32,
    pub state: CaptureState,
    pub reason: CaptureReason,
    /// 0 unknown, 1 exited, 2 signalled.
    pub exit_kind: u8,
    pub exit_value: u32,
    pub stdout: CaptureStream,
    pub stderr: CaptureStream,
    pub distinct_numbers: u32,
    pub numbers: Vec<CaptureNumber>,
    pub pause_count: u32,
    pub first_pause_ms: u64,
    pub paused_ms: u64,
    pub final_reservation: u64,
}
/// Replay facts of one `run` exec_cmd call: identities derived from the
/// installation and the call (TCR-D23), settlement and the journal sequence
/// used as "last use" by the interim expiry (J1-Q2).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExecCmdCall {
    /// Derived (SL7): [`WHOLE_COMMAND`] when field 20 bit 1 is set; `None`
    /// when the stored `command` is the executable text.
    pub fixed_command: Option<&'static str>,
    /// Derived: the secret variables passed to the guardian. 1 for the
    /// whole-command variable, else the distinct numbers in the stored
    /// command (field 15).
    pub secret_variables: u16,
    pub job: Id,
    /// J1 mode: `payload::payload_id` of the call. Job mode: the job's
    /// payload IDs ([`job_payload_ids`](super::job_payload_ids)).
    pub stdout_payload: Id,
    pub stderr_payload: Id,
    pub intent_sequence: u64,
    pub settled: Option<ExecCmdSettlement>,
    /// The kind 26 frame that made this a job-mode call (J2). A job-mode
    /// call never has kind 23; its facts are the job's (kinds 26 to 28).
    pub job_intent: Option<Locator>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExecCmdSettlement {
    pub state: CaptureState,
    pub reason: CaptureReason,
    pub sequence: u64,
    /// Stored bytes and digest per stream (stdout, stderr).
    pub streams: [(u64, Option<Hash>); 2],
    pub frame: Locator,
}

/// Replay projection of one kind 14 call and its later records. Bounded
/// display projections of verified records, never authority.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ToolRecord {
    /// The stored redacted name (at most 512 bytes; empty when not retained).
    pub name: String,
    /// Derived: the registry lookup of the name.
    pub tool: Option<RegisteredTool>,
    pub tool_operation: Option<ToolOperation>,
    pub decision: ToolDecision,
    pub fault: Option<ToolFault>,
    pub busy: Option<ExecCmdBusyReason>,
    pub refusal: Option<ExecCmdRefusal>,
    /// Field 20 bit 1 (SL7): the command ran from a secret variable; the
    /// arguments show its display text.
    pub whole_command: bool,
    pub form: ArgumentForm,
    pub arguments_bytes: u32,
    /// A display prefix of at most 192 bytes of the stored argument text.
    pub arguments: String,
    pub result_status: Option<u8>,
    pub intent_frame: Locator,
    pub result_frame: Option<Locator>,
    pub result_bytes: usize,
    pub preview: Option<String>,
    pub truncated: Option<bool>,
    pub exit_code: Option<i32>,
    /// Derived identity of a `run` create_note call.
    pub create: Option<CreateNoteCall>,
    /// Derived identities and settlement of a `run` exec_cmd call.
    pub exec_cmd: Option<Box<ExecCmdCall>>,
}
impl ToolRecord {
    /// A `run` call whose kind 15 is not yet committed.
    pub fn unresolved(&self) -> bool {
        self.decision == ToolDecision::Run && self.result_frame.is_none()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InputPromoted {
    pub request: Id,
    pub digest: Hash,
    pub input: Id,
    pub target_operation: Id,
    pub target_generation: u64,
}

/// A `run` create_note call: the turn identity, its project and the derived
/// memory identities. Recomputed on replay from the binding and the call,
/// never stored (TCR-O5, HM3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CreateNoteCall {
    pub operation: Id,
    pub generation: u64,
    pub ordinal: u32,
    pub project: Id,
    pub identity: CreateNoteIdentity,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CreateNoteIdentity {
    pub operation: Id,
    pub object: Id,
    pub version: Id,
    pub edge: Id,
}

// ----- J2 job records: kinds 26, 27 and 28 (tool-job-controller.md). -----

/// Unsettled jobs per service: the checkpoint, ledger and replay bound (JR9).
pub const MAX_UNSETTLED_JOBS: usize = 64;
/// Kind 26 payload: origin (1), agent (4), the model reference (28) or the
/// person reference (16), and the intent time (8).
pub const MAX_JOB_INTENT_PAYLOAD: usize = 41;
/// Kind 27 payload.
pub const JOB_CANCEL_PAYLOAD: usize = 19;
/// Kind 28 maximum payload: 37 bytes of job facts, 3 of capture header,
/// 1,494 per stream, 1,030 for numbers, 20 for pause facts, 8 for the
/// reservation and 8 for the settled time.
pub const MAX_JOB_SETTLED_PAYLOAD: usize = 4_094;

/// Kind 26 fields 1 and 3: who started the job and the reference that
/// names it. The job ID derives from it ([`job_id`](super::job_id)).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JobOrigin {
    /// Origin 1: one `run` exec_cmd kind 14.
    Model {
        operation: Id,
        generation: u64,
        ordinal: u32,
    },
    /// Origin 2: one person input (kind PCQ, P2). Replay rejects it in J2.
    Person { input: Id },
}
impl JobOrigin {
    pub fn code(&self) -> u8 {
        match self {
            Self::Model { .. } => 1,
            Self::Person { .. } => 2,
        }
    }
}
/// Kind 26, JobIntent. Replay derives the job ID, payload IDs, conversation,
/// project, profile, command, cwd and grant from the origin reference
/// (TCR-O5).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct JobIntent {
    pub origin: JobOrigin,
    /// 0, the conversation's main agent.
    pub agent: u32,
    /// Field 4 (J4-E1): Unix milliseconds of the service clock when the
    /// record was built; 0 only when the clock read failed. Replay never
    /// compares wall times.
    pub time: u64,
}
/// Kind 27 field 2.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum JobRequester {
    Model = 1,
    Client = 2,
    Person = 3,
    Service = 4,
}
impl JobRequester {
    pub fn from_code(code: u8) -> Option<Self> {
        Some(match code {
            1 => Self::Model,
            2 => Self::Client,
            3 => Self::Person,
            4 => Self::Service,
            _ => return None,
        })
    }
}
/// Kind 28 field 2.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum JobOutcome {
    Exited = 1,
    Signalled = 2,
    Cancelled = 3,
    NotStarted = 4,
    Interrupted = 5,
}
impl JobOutcome {
    pub const ALL: [Self; 5] = [
        Self::Exited,
        Self::Signalled,
        Self::Cancelled,
        Self::NotStarted,
        Self::Interrupted,
    ];
    pub fn from_code(code: u8) -> Option<Self> {
        Self::ALL.get(usize::from(code).checked_sub(1)?).copied()
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Exited => "exited",
            Self::Signalled => "signalled",
            Self::Cancelled => "cancelled",
            Self::NotStarted => "not_started",
            Self::Interrupted => "interrupted",
        }
    }
}
/// The one job reason table (kind 27 field 3, kind 28 field 3). The codes
/// are stored; later reasons take the next free codes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum JobReason {
    None = 0,
    ModelCancel = 1,
    /// The turn was cancelled, steered or invalidated before handle delivery.
    TurnEnded = 2,
    ClientCancel = 3,
    PersonStop = 4,
    PersonKill = 5,
    IdentityChanged = 6,
    Shutdown = 7,
    CaptureFault = 8,
    /// Quota in `Draining`.
    Quota = 9,
    /// ENOSPC in `Draining`.
    Enospc = 10,
    Restart = 11,
    CwdUnavailable = 12,
    ProjectChanged = 13,
    SandboxUnavailable = 14,
    SpawnFailed = 15,
    CaptureUnavailable = 16,
    ProtocolFault = 17,
    SetupFailed = 18,
}
impl JobReason {
    pub const ALL: [Self; 19] = [
        Self::None,
        Self::ModelCancel,
        Self::TurnEnded,
        Self::ClientCancel,
        Self::PersonStop,
        Self::PersonKill,
        Self::IdentityChanged,
        Self::Shutdown,
        Self::CaptureFault,
        Self::Quota,
        Self::Enospc,
        Self::Restart,
        Self::CwdUnavailable,
        Self::ProjectChanged,
        Self::SandboxUnavailable,
        Self::SpawnFailed,
        Self::CaptureUnavailable,
        Self::ProtocolFault,
        Self::SetupFailed,
    ];
    pub fn from_code(code: u8) -> Option<Self> {
        Self::ALL.get(usize::from(code)).copied()
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::ModelCancel => "model_cancel",
            Self::TurnEnded => "turn_ended",
            Self::ClientCancel => "client_cancel",
            Self::PersonStop => "person_stop",
            Self::PersonKill => "person_kill",
            Self::IdentityChanged => "identity_changed",
            Self::Shutdown => "shutdown",
            Self::CaptureFault => "capture_fault",
            Self::Quota => "quota",
            Self::Enospc => "enospc",
            Self::Restart => "restart",
            Self::CwdUnavailable => "cwd_unavailable",
            Self::ProjectChanged => "project_changed",
            Self::SandboxUnavailable => "sandbox_unavailable",
            Self::SpawnFailed => "spawn_failed",
            Self::CaptureUnavailable => "capture_unavailable",
            Self::ProtocolFault => "protocol_fault",
            Self::SetupFailed => "setup_failed",
        }
    }
    /// The kind 27 requester of a cancel reason; `None` for a reason that
    /// is never a cancel trigger (0 and 11 to 18).
    pub fn requester(self) -> Option<JobRequester> {
        use JobReason::*;
        Some(match self {
            ModelCancel | TurnEnded => JobRequester::Model,
            ClientCancel => JobRequester::Client,
            PersonStop | PersonKill => JobRequester::Person,
            IdentityChanged | Shutdown | CaptureFault | Quota | Enospc => JobRequester::Service,
            None | Restart | CwdUnavailable | ProjectChanged | SandboxUnavailable | SpawnFailed
            | CaptureUnavailable | ProtocolFault | SetupFailed => return Option::None,
        })
    }
    /// The kind 28 outcomes that the reason table allows for this reason.
    pub fn allows(self, outcome: JobOutcome) -> bool {
        use JobOutcome as O;
        use JobReason::*;
        match self {
            None => matches!(outcome, O::Exited | O::Signalled),
            ModelCancel | TurnEnded | ClientCancel | PersonStop | PersonKill | IdentityChanged
            | Shutdown => matches!(
                outcome,
                O::Exited | O::Signalled | O::Cancelled | O::NotStarted
            ),
            CaptureFault => matches!(outcome, O::Exited | O::Signalled | O::Cancelled),
            Quota | Enospc => matches!(outcome, O::Exited | O::Signalled),
            Restart => outcome == O::Interrupted,
            CwdUnavailable | ProjectChanged | SandboxUnavailable | SpawnFailed
            | CaptureUnavailable | ProtocolFault | SetupFailed => outcome == O::NotStarted,
        }
    }
}
/// Kind 27, JobCancelRequested: at most one per job.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct JobCancelRequested {
    pub job: Id,
    pub requester: JobRequester,
    pub reason: JobReason,
    /// Set exactly with requester `person` and reason `person_kill`.
    pub kill: bool,
}
/// Kind 28 field 6.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum JobCleanup {
    None = 0,
    Confirmed = 1,
    /// A settled notice after a cleanup-unconfirmed notice.
    Late = 2,
}
/// Kind 28 field 10: the kind 23 capture states plus `interrupted`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum JobCaptureState {
    Complete = 1,
    Incomplete = 2,
    Unconfirmed = 3,
    NotStarted = 4,
    Interrupted = 5,
}
impl JobCaptureState {
    /// The kind 23 state for codes 1 to 4; `None` for `interrupted`.
    pub fn capture(self) -> Option<CaptureState> {
        Some(match self {
            Self::Complete => CaptureState::Complete,
            Self::Incomplete => CaptureState::Incomplete,
            Self::Unconfirmed => CaptureState::Unconfirmed,
            Self::NotStarted => CaptureState::NotStarted,
            Self::Interrupted => return None,
        })
    }
    pub fn as_str(self) -> &'static str {
        self.capture().map_or("interrupted", CaptureState::as_str)
    }
}
/// Kind 28 stream block: the kind 23 stream block followed by the number of
/// newline bytes (0x0A) in the stored, redacted bytes.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct JobStream {
    pub capture: CaptureStream,
    pub lines: u64,
}
/// Kind 28, JobSettled: once per job. It holds no output, command or path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JobSettled {
    pub job: Id,
    pub outcome: JobOutcome,
    pub reason: JobReason,
    /// 0 unknown, 1 exited, 2 signalled.
    pub exit_kind: u8,
    pub exit_value: u32,
    pub cleanup: JobCleanup,
    /// 0, or the kind 4 reason (9 or 13) when cleanup is `late`.
    pub unconfirmed_reason: u8,
    /// The kind 6 value; 0 when cleanup is `none`.
    pub escapees: u32,
    /// Milliseconds from the spawned notice to the settled notice; 0 with
    /// no process.
    pub elapsed_ms: u64,
    pub capture_state: JobCaptureState,
    /// Kind 23 field 5 codes.
    pub capture_reason: CaptureReason,
    pub stdout: JobStream,
    pub stderr: JobStream,
    pub distinct_numbers: u32,
    pub numbers: Vec<CaptureNumber>,
    pub pause_count: u32,
    pub first_pause_ms: u64,
    pub paused_ms: u64,
    pub final_reservation: u64,
    /// Field 22 (J4-E1): Unix milliseconds when the record was built; 0
    /// only when the clock read failed.
    pub settled_time: u64,
}
impl JobSettled {
    /// The kind 28 of a job that a restart interrupted (J2-E5): reason
    /// `restart`, every capture field zero, built at `settled_time`.
    pub fn interrupted(job: Id, settled_time: u64) -> Self {
        Self {
            job,
            outcome: JobOutcome::Interrupted,
            reason: JobReason::Restart,
            exit_kind: 0,
            exit_value: 0,
            cleanup: JobCleanup::None,
            unconfirmed_reason: 0,
            escapees: 0,
            elapsed_ms: 0,
            capture_state: JobCaptureState::Interrupted,
            capture_reason: CaptureReason::None,
            stdout: JobStream::default(),
            stderr: JobStream::default(),
            distinct_numbers: 0,
            numbers: Vec::new(),
            pause_count: 0,
            first_pause_ms: 0,
            paused_ms: 0,
            final_reservation: 0,
            settled_time,
        }
    }
}
/// The kind 27 facts of an unsettled job.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct JobCancel {
    pub requester: JobRequester,
    pub reason: JobReason,
    pub kill: bool,
    pub frame: Locator,
}
/// One unsettled job in the replay state: a kind 26 without kind 28. The
/// controller receives these at start and settles each one `interrupted`.
/// Every field except the frames is derived from kind 26 (TCR-O5).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UnsettledJob {
    pub job: Id,
    pub origin: JobOrigin,
    pub agent: u32,
    /// The conversation of the origin operation's kind 21.
    pub conversation: Id,
    pub project: Id,
    /// The owner generation in the kind 26 frame header.
    pub owner_generation: u64,
    /// Kind 26 field 4: the intent wall time (Unix milliseconds).
    pub time: u64,
    /// The kind 26 frame; its sequence orders the unsettled jobs.
    pub intent: Locator,
    /// Kind 27, once committed.
    pub cancel: Option<JobCancel>,
    /// Derived payload IDs: stdout, stderr.
    pub payloads: [Id; 2],
}
/// The settled facts of a job: its `W` index entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SettledJob {
    pub job: Id,
    pub conversation: Id,
    /// Kind 26 field 1.
    pub origin: u8,
    pub intent: Locator,
    /// Kind 27, or `Locator::NONE`.
    pub cancel: Locator,
    pub settled: Locator,
}
/// What replay knows of one job ID.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum JobState {
    Unsettled(UnsettledJob),
    Settled(SettledJob),
}
