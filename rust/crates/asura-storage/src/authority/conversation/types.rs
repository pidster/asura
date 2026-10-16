pub use asura_platform::secrets::{LossReason, LossSpan, NumberOrigin, RefusalCode, SecretKind};
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
    /// Present for turns the context composer built; encoded as record kind 21.
    pub composition: Option<Composition>,
}
/// Shown prior turns of a composed turn; `prior_operations` lists them.
pub const MAX_COMPOSED_PRIOR: usize = 1024;
/// The context composer's record of one turn-start composition (N1).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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
    /// Kind 22 (J1): every new shell call. Kind 14 with tool kind 16 stays
    /// readable for older journals and is never written by a J1 binary.
    ShellIntent(Box<ShellIntent>),
    /// Kind 23 (J1): capture facts of an admitted shell call, before kind 15.
    CaptureSettled(Box<CaptureSettled>),
}
impl Record {
    pub fn kind(&self) -> u16 {
        match self {
            Self::PendingInit(_) => 10,
            Self::ActiveBinding(_) => 11,
            Self::OwnerGeneration => 3,
            Self::ProjectRegistered(_) => 4,
            Self::TurnAccepted(p) if p.composition.is_some() => 21,
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
            Self::ShellIntent(_) => 22,
            Self::CaptureSettled(_) => 23,
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
/// Kind 22 field 4.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum ShellDisposition {
    Admitted = 1,
    Refused = 2,
    Busy = 3,
}
/// Kind 22 field 11, for a `busy` disposition.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum ShellBusyReason {
    Quota = 1,
    CapturePaused = 2,
    PendingFull = 3,
    PayloadUnavailable = 4,
}
impl ShellBusyReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Quota => "quota",
            Self::CapturePaused => "capture_paused",
            Self::PendingFull => "pending_full",
            Self::PayloadUnavailable => "payload_unavailable",
        }
    }
}
/// Kind 22 fields 8–10 for a `refused` disposition. Never holds a value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ShellRefusal {
    pub code: RefusalCode,
    pub kind: Option<SecretKind>,
    /// The 1-based word index, 1–4096.
    pub word: Option<u16>,
}
/// Initial capture reservation recorded by an admitted kind 22 (R1).
pub const INITIAL_RESERVATION: u64 = 1024 * 1024;
/// Kind 22, ShellIntent (J1). An admitted or busy record holds the rewritten
/// command; a refused one holds the display-redacted command, cut at 8,192
/// bytes with `command_truncated`. No field holds a secret value or a digest
/// of one: `variables` lists the kind of variable `ASURA_SECRET_<i+1>`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShellIntent {
    pub operation: Id,
    pub generation: u64,
    pub ordinal: u32,
    pub disposition: ShellDisposition,
    pub command_truncated: bool,
    pub command: String,
    pub cwd: String,
    pub refusal: Option<ShellRefusal>,
    pub busy: Option<ShellBusyReason>,
    pub variables: Vec<SecretKind>,
    /// Admitted: the `asura.shell.job` derivation; otherwise zero.
    pub job: Id,
    /// Admitted: payload IDs for stdout and stderr; otherwise zero.
    pub stdout_payload: Id,
    pub stderr_payload: Id,
    /// Admitted: [`INITIAL_RESERVATION`]; otherwise 0.
    pub initial_reservation: u64,
    /// The rule set used; 0 and zero only with `secret_detection_unavailable`.
    pub rule_set_revision: u32,
    pub rule_set_digest: Hash,
}
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
    pub rule_set_revision: u32,
    pub rule_set_digest: Hash,
    pub stdout: CaptureStream,
    pub stderr: CaptureStream,
    pub distinct_numbers: u32,
    pub numbers: Vec<CaptureNumber>,
    pub pause_count: u32,
    pub first_pause_ms: u64,
    pub paused_ms: u64,
    pub final_reservation: u64,
}
/// Replay facts of one kind 22 call: identities, settlement and journal
/// sequences used as "last use" by the interim expiry (J1-Q2).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShellCall {
    pub disposition: ShellDisposition,
    pub job: Id,
    pub stdout_payload: Id,
    pub stderr_payload: Id,
    pub intent_sequence: u64,
    /// Kind 22 fields 18–19; kind 23 must name the same rule set.
    pub rule_set_revision: u32,
    pub rule_set_digest: Hash,
    pub settled: Option<ShellSettlement>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShellSettlement {
    pub state: CaptureState,
    pub reason: CaptureReason,
    pub sequence: u64,
    /// Stored bytes and digest per stream (stdout, stderr).
    pub streams: [(u64, Option<Hash>); 2],
    pub frame: Range<usize>,
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
    /// Bounded display projections of verified format-1 records, not authority.
    pub arguments: String,
    pub preview: Option<String>,
    pub truncated: Option<bool>,
    pub exit_code: Option<i32>,
    /// Present for a kind 22 shell call (J1).
    pub shell: Option<Box<ShellCall>>,
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
