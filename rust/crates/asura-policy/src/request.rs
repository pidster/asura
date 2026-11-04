//! Typed requests and decisions. A request is typed Rust built by the
//! service owner from trusted sources, never Cedar JSON from a caller
//! (security-policy.md, asynchronous interface).
use crate::Scope;

/// W1 limits (security-policy.md, limits).
pub const MAX_REQUEST_BYTES: usize = 16 * 1024;
pub const MAX_STRING_BYTES: usize = 1024;
pub const MAX_SET_ELEMENTS: usize = 64;
pub const MAX_ENTITIES: usize = 128;
pub const MAX_ANCESTORS: usize = 64;

/// An effect action.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Action {
    FileRead = 1,
    FileWrite,
    FileCreate,
    FileDelete,
    ProcessExec,
    ProcessExecAsk,
    ProcessExecApproved,
    NetworkConnect,
    SecretUse,
    MachLookup,
    ContextEgress,
}
/// The action groups that partition each scope set.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ActionGroup {
    File,
    Process,
    Network,
    Secret,
    Mach,
    Egress,
}
impl ActionGroup {
    pub const ALL: [ActionGroup; 6] = [
        ActionGroup::File,
        ActionGroup::Process,
        ActionGroup::Network,
        ActionGroup::Secret,
        ActionGroup::Mach,
        ActionGroup::Egress,
    ];
    /// The group of a Cedar action ID, group actions included.
    pub fn of_id(id: &str) -> Option<Self> {
        Some(match id {
            "file.any" | "file.read" | "file.write" | "file.create" | "file.delete" => Self::File,
            "process.exec.any" | "process.exec" | "process.exec.ask" | "process.exec.approved" => {
                Self::Process
            }
            "network.connect" => Self::Network,
            "secret_use" => Self::Secret,
            "mach.lookup" => Self::Mach,
            "context.egress" => Self::Egress,
            _ => return None,
        })
    }
}
impl Action {
    pub const ALL: [Action; 11] = [
        Action::FileRead,
        Action::FileWrite,
        Action::FileCreate,
        Action::FileDelete,
        Action::ProcessExec,
        Action::ProcessExecAsk,
        Action::ProcessExecApproved,
        Action::NetworkConnect,
        Action::SecretUse,
        Action::MachLookup,
        Action::ContextEgress,
    ];
    /// The stored action code (kind 34).
    pub fn code(self) -> u8 {
        self as u8
    }
    pub fn from_code(code: u8) -> Option<Self> {
        Self::ALL.into_iter().find(|a| a.code() == code)
    }
    /// The Cedar action ID.
    pub fn as_str(self) -> &'static str {
        match self {
            Action::FileRead => "file.read",
            Action::FileWrite => "file.write",
            Action::FileCreate => "file.create",
            Action::FileDelete => "file.delete",
            Action::ProcessExec => "process.exec",
            Action::ProcessExecAsk => "process.exec.ask",
            Action::ProcessExecApproved => "process.exec.approved",
            Action::NetworkConnect => "network.connect",
            Action::SecretUse => "secret_use",
            Action::MachLookup => "mach.lookup",
            Action::ContextEgress => "context.egress",
        }
    }
    pub fn group(self) -> ActionGroup {
        ActionGroup::of_id(self.as_str()).expect("every action has a group")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Origin {
    Model,
    PersonTyped,
    System,
}
impl Origin {
    pub fn as_str(self) -> &'static str {
        match self {
            Origin::Model => "model",
            Origin::PersonTyped => "person_typed",
            Origin::System => "system",
        }
    }
    /// The stored code (kind 34).
    pub fn code(self) -> u8 {
        match self {
            Origin::Model => 1,
            Origin::PersonTyped => 2,
            Origin::System => 3,
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ProfileKind {
    Model1,
    Person1,
    PersonOutside1,
}
impl ProfileKind {
    pub fn as_str(self) -> &'static str {
        match self {
            ProfileKind::Model1 => "model-1",
            ProfileKind::Person1 => "person-1",
            ProfileKind::PersonOutside1 => "person-outside-1",
        }
    }
    /// The stored code (kind 34).
    pub fn code(self) -> u8 {
        match self {
            ProfileKind::Model1 => 1,
            ProfileKind::Person1 => 2,
            ProfileKind::PersonOutside1 => 3,
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PathClass {
    Ordinary,
    ImplicitExec,
    BuildTest,
    AsuraState,
}
impl PathClass {
    pub fn as_str(self) -> &'static str {
        match self {
            PathClass::Ordinary => "ordinary",
            PathClass::ImplicitExec => "implicit_exec",
            PathClass::BuildTest => "build_test",
            PathClass::AsuraState => "asura_state",
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum IntentSource {
    ModelTool,
    PersonCommand,
    McpTool,
    Skill,
}
impl IntentSource {
    pub fn as_str(self) -> &'static str {
        match self {
            IntentSource::ModelTool => "model_tool",
            IntentSource::PersonCommand => "person_command",
            IntentSource::McpTool => "mcp_tool",
            IntentSource::Skill => "skill",
        }
    }
}

/// How a named secret reaches a job.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Delivery {
    Raw,
    Brokered,
}
impl Delivery {
    pub fn as_str(self) -> &'static str {
        match self {
            Delivery::Raw => "raw",
            Delivery::Brokered => "brokered",
        }
    }
}
/// The parse of a command (security-policy.md, PL2 intent extractor).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Parse {
    Simple,
    Compound,
    Unparsed,
}
impl Parse {
    pub fn as_str(self) -> &'static str {
        match self {
            Parse::Simple => "simple",
            Parse::Compound => "compound",
            Parse::Unparsed => "unparsed",
        }
    }
}
/// The command features of an intent (PL2). Tighten-only: only `forbid`
/// policies read them.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Command {
    pub parse: Parse,
    pub tools: Vec<String>,
    pub first_tool: String,
    pub subcommand: String,
    pub flags: Vec<String>,
    pub shell_features: Vec<String>,
    pub bytes: u64,
    pub secret_vars: u32,
    pub secret_reference: bool,
    pub whole_command: bool,
}

/// A canonical path with its trusted attributes. `path` is absolute,
/// with no empty, `.` or `..` component and no trailing `/` (except `/`
/// itself). Its parents are the lexical parent directories, which PL1
/// builds from already-canonical inputs (PL1 contract, paths).
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct PathRef {
    pub path: String,
    pub class: PathClass,
    pub in_project: bool,
    pub rel: String,
}
impl PathRef {
    /// An ordinary path outside every project.
    pub fn ordinary(path: &str) -> Self {
        Self {
            path: path.to_owned(),
            class: PathClass::Ordinary,
            in_project: false,
            rel: String::new(),
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JobPrincipal {
    pub job: String,
    pub origin: Origin,
    pub profile: ProfileKind,
    pub project: String,
    pub project_root: PathRef,
    pub task: String,
    pub scratch: PathRef,
    pub cache_roots: Vec<PathRef>,
    pub developer_dir: Option<PathRef>,
    /// The project's declared variable names (person jobs; empty for
    /// model jobs).
    pub declared_env: Vec<String>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProgramRef {
    pub path: PathRef,
    pub identity_known: bool,
    pub platform: bool,
    pub team_id: String,
    pub signing_id: String,
    pub cdhash: String,
}
impl ProgramRef {
    /// A program the platform could not identify (PL1: every program).
    pub fn unidentified(path: PathRef) -> Self {
        Self {
            path,
            identity_known: false,
            platform: false,
            team_id: String::new(),
            signing_id: String::new(),
            cdhash: String::new(),
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Resource {
    Path(PathRef),
    Program(ProgramRef),
    /// A named secret, `Secret::"<NAME>"`.
    Secret(String),
    /// A canonical host name; its parents are its label suffixes.
    Domain(String),
    /// An IP address in its standard text form.
    Address(String),
}
/// Intent: tighten-only context (AD-27), with the command features of the
/// intent extractor when the proposer is a command (PL2).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Intent {
    pub source: IntentSource,
    pub name: String,
    pub digest: String,
    pub command: Option<Command>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Context {
    File {
        now_ms: i64,
        intent: Option<Intent>,
    },
    Exec {
        now_ms: i64,
        request_digest: String,
        env_terminal: Vec<String>,
        env_declared: Vec<String>,
        outside_switch: bool,
        intent: Option<Intent>,
    },
    Secret {
        now_ms: i64,
        delivery: Delivery,
        intent: Option<Intent>,
    },
    Connect {
        now_ms: i64,
        ip: String,
        port: u16,
        via_proxy: bool,
        intent: Option<Intent>,
    },
}
impl Context {
    /// The intent of the context, when it can hold one.
    pub fn intent(&self) -> Option<&Intent> {
        match self {
            Context::File { intent, .. }
            | Context::Exec { intent, .. }
            | Context::Secret { intent, .. }
            | Context::Connect { intent, .. } => intent.as_ref(),
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Request {
    pub principal: JobPrincipal,
    pub action: Action,
    pub resource: Resource,
    pub context: Context,
}

/// Why a scope or a request was refused for an error.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ErrorClass {
    PolicyError = 1,
    RequestInvalid = 2,
    RequestTooLarge = 3,
    ScopeUnavailable = 4,
}
impl ErrorClass {
    pub fn code(self) -> u8 {
        self as u8
    }
    pub fn reason(self) -> &'static str {
        match self {
            ErrorClass::PolicyError => "policy_error",
            ErrorClass::RequestInvalid => "request_invalid",
            ErrorClass::RequestTooLarge => "request_too_large",
            ErrorClass::ScopeUnavailable => "scope_unavailable",
        }
    }
}
/// One scope's result. `rules` are global rule IDs `<scope>:<id>`, sorted,
/// at most 8: the determining policies, or the erroring ones.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScopeResult {
    pub scope: Scope,
    pub allow: bool,
    pub rules: Vec<String>,
    pub error: Option<ErrorClass>,
}
/// The primary reason of a refusal: the first denying scope's first
/// reason by rule ID. `scope` is `None` for request errors and
/// `cannot_enforce`. `explain` is the `@explain` text of an installation or
/// user forbid only.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Primary {
    pub scope: Option<Scope>,
    pub reason: String,
    pub rule: Option<String>,
    pub explain: Option<String>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Decision {
    pub allow: bool,
    pub scopes: Vec<ScopeResult>,
    pub primary: Option<Primary>,
}
impl Decision {
    /// A refusal for a request error, recorded in every applicable scope.
    pub(crate) fn request_error(class: ErrorClass, scopes: &[Scope]) -> Self {
        Self {
            allow: false,
            scopes: scopes
                .iter()
                .map(|&scope| ScopeResult {
                    scope,
                    allow: false,
                    rules: Vec::new(),
                    error: Some(class),
                })
                .collect(),
            primary: Some(Primary {
                scope: None,
                reason: class.reason().to_owned(),
                rule: None,
                explain: None,
            }),
        }
    }
    /// The same scope results with the primary reason `cannot_enforce`.
    pub(crate) fn cannot_enforce(scopes: Vec<ScopeResult>) -> Self {
        Self {
            allow: false,
            scopes,
            primary: Some(Primary {
                scope: None,
                reason: "cannot_enforce".to_owned(),
                rule: None,
                explain: None,
            }),
        }
    }
}
