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
/// The shared response and thinking text in one `ReadResponse` window: the
/// control event window bound (`MAX_RESPONSE_WINDOW_BYTES`, 60,928 bytes
/// since 2026-10-22), below `MAX_TEXT` so the terminal event that carries
/// the window fits one control frame (response-text-transfer.md, event
/// window bound). The service checks that the two bounds agree.
pub const MAX_WINDOW_TEXT: usize = 60_928;
const _: () = assert!(MAX_WINDOW_TEXT <= MAX_TEXT);

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
    /// Kind 29 (J3): the reference-only result of a `run` call with
    /// operation code 10 to 16. It holds no output, page or command text.
    JobResult(Box<JobResult>),
    /// Kind 30 (J4): one pin, unpin, delete or note of a job.
    JobRetention(Box<JobRetention>),
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
            Self::ResponseText(_) => 24,
            Self::ThinkingText(_) => 25,
            Self::JobIntent(_) => 26,
            Self::JobCancelRequested(_) => 27,
            Self::JobSettled(_) => 28,
            Self::JobResult(_) => 29,
            Self::JobRetention(_) => 30,
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

/// The five registered model-facing tool names (TCR-D24, J3-E2). Replay
/// looks a stored name up here; the service registry uses these constants.
pub const TOOL_NAMES: [&str; 5] = ["project", "memory", "service", "exec_cmd", "job"];
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
    /// The grouped job tool (J3-E2), registry position 4.
    Job,
}
impl RegisteredTool {
    /// The registry lookup. An empty (not retained) name is unregistered.
    pub fn lookup(name: &str) -> Option<Self> {
        match TOOL_NAMES.iter().position(|n| *n == name)? {
            0 => Some(Self::Project),
            1 => Some(Self::Memory),
            2 => Some(Self::Service),
            3 => Some(Self::ExecCmd),
            _ => Some(Self::Job),
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
    /// Codes 11 to 16: the `job` tool commands (J3-E2).
    JobList = 11,
    JobStatus = 12,
    JobRead = 13,
    JobWait = 14,
    JobCancel = 15,
    JobNote = 16,
}
impl ToolOperation {
    pub const ALL: [Self; 16] = [
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
        Self::JobList,
        Self::JobStatus,
        Self::JobRead,
        Self::JobWait,
        Self::JobCancel,
        Self::JobNote,
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
            Self::JobList
            | Self::JobStatus
            | Self::JobRead
            | Self::JobWait
            | Self::JobCancel
            | Self::JobNote => RegisteredTool::Job,
        }
    }
    /// Codes 10 to 16 resolve with kind 29, never kind 15; codes 1 to 9 with
    /// kind 15, never kind 29 (JR11).
    pub fn job_result(self) -> bool {
        self >= Self::ExecCmd
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
            Self::JobList => "list",
            Self::JobStatus => "status",
            Self::JobRead => "read",
            Self::JobWait => "wait",
            Self::JobCancel => "cancel",
            Self::JobNote => "note",
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
    /// A `job` field that is not 32 lowercase hexadecimal digits, or is all
    /// zero (J3).
    InvalidJobId = 16,
}
impl ToolFault {
    pub const ALL: [Self; 16] = [
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
        Self::InvalidJobId,
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
            Self::InvalidJobId => "invalid_job_id",
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
    /// Field 10, the decision reason (J3-E3): set with every `busy`, and
    /// with an exec_cmd `denied` for `execution_off`; `None` otherwise.
    pub reason: Option<ToolDecisionReason>,
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
/// Kind 14 field 10, the decision reason (J3-E3). The stored code depends
/// on the decision: `busy` 1 `queue_full`, 2 `jobs_full`, 3 `closed`;
/// `denied` 1 `execution_off` (EX11). Every other decision stores 0. The J1
/// busy codes are retired with the J1 path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolDecisionReason {
    /// `busy`: the agent's job queue is full.
    QueueFull,
    /// `busy`: the service has 64 unsettled jobs.
    JobsFull,
    /// `busy`: the controller is closed for shutdown.
    Closed,
    /// `denied`: the project's execution setting is off.
    ExecutionOff,
}
impl ToolDecisionReason {
    pub const ALL: [Self; 4] = [
        Self::QueueFull,
        Self::JobsFull,
        Self::Closed,
        Self::ExecutionOff,
    ];
    /// The decision this reason belongs to.
    pub fn decision(self) -> ToolDecision {
        match self {
            Self::QueueFull | Self::JobsFull | Self::Closed => ToolDecision::Busy,
            Self::ExecutionOff => ToolDecision::Denied,
        }
    }
    /// The stored field 10 code.
    pub fn code(self) -> u8 {
        match self {
            Self::QueueFull | Self::ExecutionOff => 1,
            Self::JobsFull => 2,
            Self::Closed => 3,
        }
    }
    /// The reason that `code` names under `decision`: `Some(None)` for code
    /// 0, `None` for a code that the decision does not allow.
    pub fn from_code(decision: ToolDecision, code: u8) -> Option<Option<Self>> {
        if code == 0 {
            return Some(None);
        }
        Self::ALL
            .into_iter()
            .find(|r| r.decision() == decision && r.code() == code)
            .map(Some)
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::QueueFull => "queue_full",
            Self::JobsFull => "jobs_full",
            Self::Closed => "closed",
            Self::ExecutionOff => "execution_off",
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
/// The capture states 1 to 4 of kind 28 field 10 (formerly kind 23 field 4,
/// retired by J3).
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
/// Kind 28 field 11 (the capture reason codes of the retired kind 23).
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
/// The capture stream block of kind 28: stored-stream facts, with no value
/// or value digest. `digest` is SHA-256 over exactly the stored (redacted)
/// bytes.
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
/// Kind 28 number entry. "Propagated" is not an origin (G17).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CaptureNumber {
    pub number: u16,
    pub kind: SecretKind,
    pub origin: NumberOrigin,
}
/// Replay facts of one `run` exec_cmd call: identities derived from the
/// installation and the call (TCR-D23) and the kind 14 sequence. Every
/// `run` exec_cmd call is a job (J3-E9); its settlement is the job's kind 28.
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
    /// `payload::payload_id` of `job`, in J1 and job mode alike.
    pub stdout_payload: Id,
    pub stderr_payload: Id,
    pub intent_sequence: u64,
    /// Always `None`: kind 23 is retired (J3-E9, JR17). Kept only until the
    /// payload recovery packet (J34-C) drops its last reader; then removed
    /// with [`ExecCmdSettlement`].
    pub settled: Option<ExecCmdSettlement>,
    /// The call's kind 26 frame, once committed.
    pub job_intent: Option<Locator>,
}
/// Retired with kind 23; see [`ExecCmdCall::settled`].
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
    /// Kind 14 field 10, the decision reason.
    pub reason: Option<ToolDecisionReason>,
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
    /// The call's result record: kind 15 for operation codes 1 to 9, kind
    /// 29 for codes 10 to 16 (JR11).
    pub result_frame: Option<Locator>,
    pub result_bytes: usize,
    pub preview: Option<String>,
    pub truncated: Option<bool>,
    /// Derived identity of a `run` create_note call.
    pub create: Option<CreateNoteCall>,
    /// Derived identities of a `run` exec_cmd call.
    pub exec_cmd: Option<Box<ExecCmdCall>>,
    /// The kind 29 of a call with operation code 10 to 16, once committed.
    /// It holds references and counts only; the result text is rendered
    /// from it and the job records (J3-E1).
    pub job_result: Option<Box<JobResult>>,
    /// The kind 30 model note of a `run` `job note` call, once committed:
    /// its job and frame (JR16).
    pub note: Option<(Id, Locator)>,
}
impl ToolRecord {
    /// A `run` call whose result record (kind 15 or kind 29) is not yet
    /// committed.
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
    /// The project's execution setting turned off (EX11): a service
    /// revocation, or the slot-grant check.
    ExecutionOff = 19,
    /// The person `!!` switch was off at slot grant (P2). Only a person
    /// job of profile `person-outside-1` may record it.
    OutsideSandboxOff = 20,
}
impl JobReason {
    pub const ALL: [Self; 21] = [
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
        Self::ExecutionOff,
        Self::OutsideSandboxOff,
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
            Self::ExecutionOff => "execution_off",
            Self::OutsideSandboxOff => "outside_sandbox_off",
        }
    }
    /// The kind 27 requester of a cancel reason; `None` for a reason that
    /// is never a cancel trigger (0, 11 to 18 and 20).
    pub fn requester(self) -> Option<JobRequester> {
        use JobReason::*;
        Some(match self {
            ModelCancel | TurnEnded => JobRequester::Model,
            ClientCancel => JobRequester::Client,
            PersonStop | PersonKill => JobRequester::Person,
            IdentityChanged | Shutdown | CaptureFault | Quota | Enospc | ExecutionOff => {
                JobRequester::Service
            }
            None | Restart | CwdUnavailable | ProjectChanged | SandboxUnavailable | SpawnFailed
            | CaptureUnavailable | ProtocolFault | SetupFailed | OutsideSandboxOff => {
                return Option::None;
            }
        })
    }
    /// The kind 28 outcomes that the reason table allows for this reason.
    pub fn allows(self, outcome: JobOutcome) -> bool {
        use JobOutcome as O;
        use JobReason::*;
        match self {
            None => matches!(outcome, O::Exited | O::Signalled),
            ModelCancel | TurnEnded | ClientCancel | PersonStop | PersonKill | IdentityChanged
            | Shutdown | ExecutionOff => matches!(
                outcome,
                O::Exited | O::Signalled | O::Cancelled | O::NotStarted
            ),
            CaptureFault => matches!(outcome, O::Exited | O::Signalled | O::Cancelled),
            Quota | Enospc => matches!(outcome, O::Exited | O::Signalled),
            Restart => outcome == O::Interrupted,
            CwdUnavailable | ProjectChanged | SandboxUnavailable | SpawnFailed
            | CaptureUnavailable | ProtocolFault | SetupFailed | OutsideSandboxOff => {
                outcome == O::NotStarted
            }
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

// ----- J3 kind 29 and J4 kind 30 (tool-job-controller.md). -----

/// Kind 29 fixed fields: operation (16), generation (8), ordinal (4),
/// status (1), outcome (1) and job ID (16).
pub const JOB_RESULT_FIXED: usize = 46;
/// List entries per kind 29 list block and per model `list` call.
pub const MAX_JOB_LIST_ENTRIES: usize = 8;
/// One list entry: job ID (16), state (1), reason (1), reason numbers (3),
/// stdout and stderr bytes (8 each).
pub const JOB_LIST_ENTRY: usize = 37;
/// The body sizes of kind 29. The body is never tagged: its form follows
/// from the operation code (JR12), and the sizes are distinct, so the codec
/// reads it by size.
pub const JOB_STATE_BLOCK: usize = 29;
pub const JOB_WAIT_BLOCK: usize = JOB_STATE_BLOCK + 1;
pub const JOB_PAGE_BLOCK: usize = 54;
pub const MAX_JOB_LIST_BLOCK: usize = 2 + MAX_JOB_LIST_ENTRIES * JOB_LIST_ENTRY;
/// Maximum kind 29 payload: the fixed fields and a full list block.
pub const MAX_JOB_RESULT_PAYLOAD: usize = JOB_RESULT_FIXED + MAX_JOB_LIST_BLOCK;
/// Source bytes per page, and the delivered text bound (R5).
pub const MAX_JOB_PAGE: u32 = 8192;
/// Note text bytes (person notes after display redaction; the model's
/// `text` argument).
pub const MAX_JOB_NOTE: usize = 2048;
/// Notes per job, model and person together; an append-time bound.
pub const MAX_JOB_NOTES: usize = 16;
/// Kind 30 records per job; an append-time bound.
pub const MAX_JOB_RETENTION_RECORDS: usize = 64;
/// Kind 30 payload of a model note: job ID (16), action (1), author (1),
/// time (8), operation, generation and ordinal (28), empty text (4).
pub const MODEL_NOTE_PAYLOAD: usize = 58;
/// Maximum kind 30 payload: a person note with a 16-byte request ID and
/// 2,048 bytes of text.
pub const MAX_JOB_RETENTION_PAYLOAD: usize = 16 + 1 + 1 + 8 + 16 + 4 + MAX_JOB_NOTE;

/// Kind 29 field 5.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum JobResultOutcome {
    Ok = 1,
    NotFound = 2,
    NotPermitted = 3,
    Busy = 4,
    Cancelled = 5,
    Unavailable = 6,
    Limit = 7,
}
impl JobResultOutcome {
    pub const ALL: [Self; 7] = [
        Self::Ok,
        Self::NotFound,
        Self::NotPermitted,
        Self::Busy,
        Self::Cancelled,
        Self::Unavailable,
        Self::Limit,
    ];
    pub fn from_code(code: u8) -> Option<Self> {
        Self::ALL.get(usize::from(code).checked_sub(1)?).copied()
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::NotFound => "not_found",
            Self::NotPermitted => "not_permitted",
            Self::Busy => "busy",
            Self::Cancelled => "cancelled",
            Self::Unavailable => "unavailable",
            Self::Limit => "limit",
        }
    }
    /// The wire `ToolStatus` that the outcome fixes (kind 29 field 4).
    pub fn status(self) -> u8 {
        match self {
            Self::Ok => 1,
            Self::NotFound => 3,
            Self::NotPermitted => 2,
            Self::Busy | Self::Unavailable => 4,
            Self::Cancelled => 6,
            Self::Limit => 7,
        }
    }
    /// The outcome table: whether operation `code` may record this outcome.
    pub fn allows(self, code: ToolOperation) -> bool {
        use ToolOperation as T;
        let job = code.job_result();
        match self {
            Self::Ok => job,
            Self::NotFound => job && !matches!(code, T::ExecCmd | T::JobList),
            Self::Unavailable => job && code != T::ExecCmd,
            Self::NotPermitted => code == T::JobCancel,
            Self::Busy => code == T::JobRead,
            Self::Cancelled => job && code != T::JobCancel,
            Self::Limit => code == T::JobNote,
        }
    }
}
/// J2 start-condition block reasons, in start-condition order (the state
/// block reason of a `queued` job).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum JobBlockReason {
    Recovering = 1,
    JournalUnavailable = 2,
    JournalHeld = 3,
    PayloadUnavailable = 4,
    AgentSlots = 5,
    ServiceSlots = 6,
    PendingInstalls = 7,
    Quota = 8,
    DiskSpace = 9,
}
impl JobBlockReason {
    pub const ALL: [Self; 9] = [
        Self::Recovering,
        Self::JournalUnavailable,
        Self::JournalHeld,
        Self::PayloadUnavailable,
        Self::AgentSlots,
        Self::ServiceSlots,
        Self::PendingInstalls,
        Self::Quota,
        Self::DiskSpace,
    ];
    pub fn from_code(code: u8) -> Option<Self> {
        Self::ALL.get(usize::from(code).checked_sub(1)?).copied()
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Recovering => "recovering",
            Self::JournalUnavailable => "journal_unavailable",
            Self::JournalHeld => "journal_held",
            Self::PayloadUnavailable => "payload_unavailable",
            Self::AgentSlots => "agent_slots",
            Self::ServiceSlots => "service_slots",
            Self::PendingInstalls => "pending_installs",
            Self::Quota => "quota",
            Self::DiskSpace => "disk_space",
        }
    }
}
/// Why a `capture_paused` job paused (state block reason).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum JobPauseReason {
    Quota = 1,
    DiskSpace = 2,
}
impl JobPauseReason {
    pub fn from_code(code: u8) -> Option<Self> {
        match code {
            1 => Some(Self::Quota),
            2 => Some(Self::DiskSpace),
            _ => None,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Quota => "quota",
            Self::DiskSpace => "disk_space",
        }
    }
}
/// A job's state in a kind 29 state block or list entry, with the reason
/// that the state carries. The stored state codes are 1 to 9 in order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JobRunState {
    Queued(JobBlockReason),
    Starting,
    Running,
    CapturePaused(JobPauseReason),
    Stopping,
    Draining,
    CleanupUnconfirmed,
    Finalizing,
    /// Reported only when the job's kind 28 committed before the record
    /// (J3-E7, JR14); the settled facts are then read from kind 28.
    Settled,
}
impl JobRunState {
    /// The stored state code (1 to 9).
    pub fn code(self) -> u8 {
        match self {
            Self::Queued(_) => 1,
            Self::Starting => 2,
            Self::Running => 3,
            Self::CapturePaused(_) => 4,
            Self::Stopping => 5,
            Self::Draining => 6,
            Self::CleanupUnconfirmed => 7,
            Self::Finalizing => 8,
            Self::Settled => 9,
        }
    }
    /// The stored reason code: the block or pause reason, otherwise 0.
    pub fn reason_code(self) -> u8 {
        match self {
            Self::Queued(reason) => reason as u8,
            Self::CapturePaused(reason) => reason as u8,
            _ => 0,
        }
    }
    /// The state and reason that two stored codes name, if valid.
    pub fn from_codes(state: u8, reason: u8) -> Option<Self> {
        let plain = |s: Self| (reason == 0).then_some(s);
        match state {
            1 => JobBlockReason::from_code(reason).map(Self::Queued),
            2 => plain(Self::Starting),
            3 => plain(Self::Running),
            4 => JobPauseReason::from_code(reason).map(Self::CapturePaused),
            5 => plain(Self::Stopping),
            6 => plain(Self::Draining),
            7 => plain(Self::CleanupUnconfirmed),
            8 => plain(Self::Finalizing),
            9 => plain(Self::Settled),
            _ => None,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued(_) => "queued",
            Self::Starting => "starting",
            Self::Running => "running",
            Self::CapturePaused(_) => "capture_paused",
            Self::Stopping => "stopping",
            Self::Draining => "draining",
            Self::CleanupUnconfirmed => "cleanup_unconfirmed",
            Self::Finalizing => "finalizing",
            Self::Settled => "settled",
        }
    }
    /// Before the spawned notice: the elapsed time is 0.
    pub fn before_spawn(self) -> bool {
        matches!(self, Self::Queued(_) | Self::Starting)
    }
    /// The reason numbers this state allows: running, limit and awaiting
    /// cleanup for `agent_slots` and `service_slots`; the count (first
    /// byte) for `pending_installs`; otherwise zero.
    pub fn allows_numbers(self, numbers: [u8; 3]) -> bool {
        match self {
            Self::Queued(JobBlockReason::AgentSlots | JobBlockReason::ServiceSlots) => true,
            Self::Queued(JobBlockReason::PendingInstalls) => numbers[1..] == [0, 0],
            _ => numbers == [0; 3],
        }
    }
}
/// Kind 29 state block (codes 10, 12, 14 and 15).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct JobSnapshot {
    pub state: JobRunState,
    pub numbers: [u8; 3],
    /// Milliseconds since the spawned notice; 0 before spawn and when
    /// settled.
    pub elapsed_ms: u64,
    /// Stored bytes (the readable watermark) at the snapshot; 0 when
    /// settled.
    pub stdout_bytes: u64,
    pub stderr_bytes: u64,
}
/// A job output stream (page block field 1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum OutputStream {
    Stdout = 1,
    Stderr = 2,
}
impl OutputStream {
    pub fn from_code(code: u8) -> Option<Self> {
        match code {
            1 => Some(Self::Stdout),
            2 => Some(Self::Stderr),
            _ => None,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Stdout => "stdout",
            Self::Stderr => "stderr",
        }
    }
}
/// Page block field 5.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum JobPageState {
    Live = 1,
    Complete = 2,
    Incomplete = 3,
    UnverifiedPrefix = 4,
    Expired = 5,
    Missing = 6,
    Corrupt = 7,
    Deleted = 8,
}
impl JobPageState {
    pub const ALL: [Self; 8] = [
        Self::Live,
        Self::Complete,
        Self::Incomplete,
        Self::UnverifiedPrefix,
        Self::Expired,
        Self::Missing,
        Self::Corrupt,
        Self::Deleted,
    ];
    pub fn from_code(code: u8) -> Option<Self> {
        Self::ALL.get(usize::from(code).checked_sub(1)?).copied()
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Live => "live",
            Self::Complete => "complete",
            Self::Incomplete => "incomplete",
            Self::UnverifiedPrefix => "unverified_prefix",
            Self::Expired => "expired",
            Self::Missing => "missing",
            Self::Corrupt => "corrupt",
            Self::Deleted => "deleted",
        }
    }
    /// States 1 to 4 may deliver bytes; states 5 to 8 deliver none.
    pub fn readable(self) -> bool {
        (self as u8) <= 4
    }
}
/// Kind 29 page block (code 13). The next offset is `start + source_len`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct JobPage {
    pub stream: OutputStream,
    /// The actual start after the UTF-8 snap (R5).
    pub start: u64,
    /// Source bytes read, 0 to 8,192.
    pub source_len: u32,
    /// The stream's stored bytes at the read.
    pub total: u64,
    pub state: JobPageState,
    /// SHA-256 of the delivered page text; zero when `source_len` is 0.
    pub digest: Hash,
}
impl JobPage {
    /// The turn cursor after this page (J3-E4).
    pub fn next_offset(&self) -> u64 {
        self.start.saturating_add(u64::from(self.source_len))
    }
}
/// One kind 29 list entry (code 11). A settled entry stores zero bytes;
/// its facts come from kind 28.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct JobListEntry {
    pub job: Id,
    pub state: JobRunState,
    pub numbers: [u8; 3],
    pub stdout_bytes: u64,
    pub stderr_bytes: u64,
}
/// Wait block field: why a `job wait` ended (code 14).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum WaitEnd {
    Settled = 1,
    NewOutput = 2,
    Time = 3,
    AlreadySettled = 4,
}
impl WaitEnd {
    pub fn from_code(code: u8) -> Option<Self> {
        Some(match code {
            1 => Self::Settled,
            2 => Self::NewOutput,
            3 => Self::Time,
            4 => Self::AlreadySettled,
            _ => return None,
        })
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Settled => "settled",
            Self::NewOutput => "new_output",
            Self::Time => "time",
            Self::AlreadySettled => "already_settled",
        }
    }
}
/// Kind 29 field 7. The body form follows from the operation code and the
/// outcome (JR12); it is never stored with a tag.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum JobResultBody {
    /// Code 16 with `ok`, and every outcome other than `ok` except code 10
    /// with `cancelled`.
    None,
    /// Codes 10, 12 and 15 with `ok`, and code 10 with `cancelled`.
    State(JobSnapshot),
    /// Code 13 with `ok`.
    Page(JobPage),
    /// Code 11 with `ok`: newest first by kind 26 sequence; `more` when
    /// further entries exist (then exactly 8 are shown).
    List {
        more: bool,
        entries: Vec<JobListEntry>,
    },
    /// Code 14 with `ok`.
    Wait(JobSnapshot, WaitEnd),
}
/// Kind 29, JobResult (R3, J3-E1): the reference-only result of a `run`
/// call with operation code 10 to 16. It holds no output, page or command
/// text; the rendered `ToolResult` comes from it, the job records and, for
/// a page, the retained bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JobResult {
    pub operation: Id,
    pub generation: u64,
    /// 1 to 200: the pending `run` kind 14.
    pub ordinal: u32,
    /// Wire `ToolStatus`, fixed by `outcome`.
    pub status: u8,
    pub outcome: JobResultOutcome,
    /// Zero for code 10 (derived from the call), for code 11 and with
    /// `not_found`; otherwise a job of the turn's conversation.
    pub job: Id,
    pub body: JobResultBody,
}
/// Kind 30 field 2.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum RetentionAction {
    Pin = 1,
    Unpin = 2,
    Delete = 3,
    Note = 4,
}
impl RetentionAction {
    pub fn from_code(code: u8) -> Option<Self> {
        Some(match code {
            1 => Self::Pin,
            2 => Self::Unpin,
            3 => Self::Delete,
            4 => Self::Note,
            _ => return None,
        })
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pin => "pin",
            Self::Unpin => "unpin",
            Self::Delete => "delete",
            Self::Note => "note",
        }
    }
}
/// Kind 30 fields 3 and 5: the author and the reference that names the
/// request (person) or the `run` `job note` call (model).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RetentionAuthor {
    /// Author 1: a control request ID.
    Person { request: Id },
    /// Author 2: the `run` kind 14 with operation code 16 for this job.
    Model {
        operation: Id,
        generation: u64,
        ordinal: u32,
    },
}
impl RetentionAuthor {
    pub fn code(&self) -> u8 {
        match self {
            Self::Person { .. } => 1,
            Self::Model { .. } => 2,
        }
    }
}
/// Kind 30, JobRetention (S1, J4-E2). A normal record, not a closure.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JobRetention {
    pub job: Id,
    pub action: RetentionAction,
    pub author: RetentionAuthor,
    /// Unix milliseconds, as kind 26 field 4; 0 only when the clock read
    /// failed. Replay never compares wall times.
    pub time: u64,
    /// A person note's text, 1 to 2,048 bytes after display redaction (each
    /// detected secret is `$ASURA_SECRET_0`); empty otherwise. A model
    /// note's text is the `text` argument of its kind 14 (TCR-O5).
    pub note: String,
}
/// The retention state of one job, from its `E` entries.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct JobRetentionSummary {
    pub pinned: bool,
    pub deleted: bool,
    /// Notes of either author.
    pub notes: u32,
    /// Kind 30 records of every action.
    pub records: u32,
}
impl JobRetentionSummary {
    /// The state after one more committed kind 30.
    pub fn apply(&mut self, action: RetentionAction) {
        self.records = self.records.saturating_add(1);
        match action {
            RetentionAction::Pin => self.pinned = true,
            RetentionAction::Unpin => self.pinned = false,
            RetentionAction::Delete => self.deleted = true,
            RetentionAction::Note => self.notes = self.notes.saturating_add(1),
        }
    }
    /// Whether a kind 30 with `action` changes anything (the kind 30 replay
    /// rules): pin only when unpinned, unpin only when pinned, delete once
    /// and only after the job's kind 28 (`settled`), a note always.
    pub fn allows(&self, action: RetentionAction, settled: bool) -> bool {
        match action {
            RetentionAction::Pin => !self.pinned,
            RetentionAction::Unpin => self.pinned,
            RetentionAction::Delete => settled && !self.deleted,
            RetentionAction::Note => true,
        }
    }
    /// The append-time bounds of the service: 16 notes and 64 records per
    /// job (`notes_full`, `retention_full`).
    pub fn has_room(&self, action: RetentionAction) -> bool {
        (self.records as usize) < MAX_JOB_RETENTION_RECORDS
            && (action != RetentionAction::Note || (self.notes as usize) < MAX_JOB_NOTES)
    }
}
/// The scope of a job listing read (J4): `L`, `M` or `Y` entries.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JobScope {
    Conversation(Id),
    Project(Id),
    AllProjects,
}
/// Entries per job listing read: 8 shown and one to detect more.
pub const MAX_JOB_LISTING_READ: usize = MAX_JOB_LIST_ENTRIES + 1;
/// One job as the listing reads return it: its kind 26, 27 and 28 records,
/// the command source locator and the retention summary. A settled job's
/// facts are read from its kind 28, never stored twice.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JobListing {
    pub job: Id,
    pub conversation: Id,
    pub project: Id,
    pub intent: JobIntent,
    /// The kind 26 frame; its sequence orders listings (J3-E5).
    pub intent_frame: Locator,
    pub cancel: Option<JobCancelRequested>,
    /// `Locator::NONE` without kind 27.
    pub cancel_frame: Locator,
    pub settled: Option<Box<JobSettled>>,
    /// `Locator::NONE` while unsettled.
    pub settled_frame: Locator,
    /// The command source: the model call's kind 14 (a person job's kind
    /// 31 after P2). Command, cwd and excerpt are read from it.
    pub command: Locator,
    pub retention: JobRetentionSummary,
}
/// One job's facts for `JobInspect` and the model's `status` (the
/// `JobFacts` read): its listing, the kind 14 that holds its command and
/// cwd, and its kind 30 records in order, at most 64.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JobRecords {
    pub listing: JobListing,
    pub command: Box<ToolCall>,
    pub retention: Vec<(Locator, JobRetention)>,
}
