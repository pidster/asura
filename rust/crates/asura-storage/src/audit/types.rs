//! Closed metadata schema shared by storage and service; no user-content fields.
use serde::{Deserialize, Serialize};
pub const MAX_RECORD_BYTES: usize = 4096;
pub const WINDOW_CAPACITY: usize = 256;
pub type Id = [u8; 16];
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Settings {
    pub enabled: bool,
    pub keep_files: u32,
    pub max_file_bytes: u64,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: true,
            keep_files: 5,
            max_file_bytes: 10_485_760,
        }
    }
}
impl Settings {
    pub fn validate(self) -> Result<(), Error> {
        if (1..=10_000).contains(&self.keep_files)
            && (1..=1_099_511_627_776).contains(&self.max_file_bytes)
        {
            Ok(())
        } else {
            Err(Error::Invalid)
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum HealthState {
    Starting = 1,
    Active = 2,
    Disabled = 3,
    Stale = 4,
    Unavailable = 5,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[repr(u32)]
pub enum HealthReason {
    ConfigInvalid = 1,
    UnsafeStorage = 2,
    MalformedRecord = 3,
    Io = 4,
    Deadline = 5,
    Cancelled = 6,
    Limit = 7,
    SequenceExhausted = 8,
    QueueFull = 9,
    Closed = 10,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Health {
    pub state: HealthState,
    pub enabled: bool,
    pub keep_files: Option<u32>,
    pub max_file_bytes: Option<u64>,
    pub dropped: u64,
    pub window_capacity: u32,
    pub hydrated: bool,
    pub older_omitted: bool,
    pub reason: Option<HealthReason>,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Invalid,
    Limit,
    Disabled,
    Cancelled,
    Deadline,
    UnsafeStorage,
    MalformedRecord,
    Io,
    Unconfirmed,
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "audit_{self:?}")
    }
}
impl std::error::Error for Error {}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[repr(u32)]
pub enum DecisionReason {
    None = 0,
    RequestConflict = 1,
    GenerationConflict = 2,
    Unavailable = 3,
    Denied = 4,
    Invalid = 5,
    Busy = 6,
    Limit = 7,
    Timeout = 8,
    Cancelled = 9,
    Internal = 10,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[repr(u32)]
pub enum AdmissionOutcome {
    Accepted = 1,
    Rejected = 2,
    Unconfirmed = 3,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[repr(u32)]
pub enum FinishedOutcome {
    Completed = 1,
    Failed = 2,
    Cancelled = 3,
    Interrupted = 4,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[repr(u32)]
pub enum ToolOutcome {
    Success = 1,
    Denied = 2,
    InvalidArguments = 3,
    Unavailable = 4,
    Timeout = 5,
    Cancelled = 6,
    Limit = 7,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[repr(u32)]
pub enum Tool {
    ProjectReadFile = 1,
    ProjectListDirectory = 2,
    ServiceObserveStatus = 3,
    MemoryListNotes = 4,
    MemoryGetNote = 5,
    MemoryNoteSources = 6,
    ServiceListTools = 7,
    ServiceReadAudit = 8,
    ExecCmd = 9,
    MemoryCreateNote = 10,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigLoad {
    Loaded,
    Defaults,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Setting {
    Model,
    Audit,
    #[serde(rename = "audit.enabled")]
    AuditEnabled,
    #[serde(rename = "audit.keepFiles")]
    AuditKeepFiles,
    #[serde(rename = "audit.maxFileBytes")]
    AuditMaxFileBytes,
    #[serde(rename = "providers.mlx.models")]
    ProvidersMlxModels,
    #[serde(rename = "retention.quotaBytes")]
    RetentionQuotaBytes,
    #[serde(rename = "journal.quotaBytes")]
    JournalQuotaBytes,
    Jobs,
    #[serde(rename = "jobs.liveService")]
    JobsLiveService,
    #[serde(rename = "jobs.liveAgent")]
    JobsLiveAgent,
    #[serde(rename = "jobs.queuedAgent")]
    JobsQueuedAgent,
    #[serde(rename = "jobs.clientViewers")]
    JobsClientViewers,
    #[serde(rename = "jobs.modelReservedPageReads")]
    JobsModelReservedPageReads,
}
impl Setting {
    /// The setting that a successful `ConfigSet` of `key` reports in
    /// `config.changed`; `None` for a key that emits no audit event. A mapping
    /// set reports the mapping, except `journal`, which reports its one leaf.
    pub fn for_config_key(key: &str) -> Option<Self> {
        Some(match key {
            "model" => Self::Model,
            "audit" => Self::Audit,
            "audit.enabled" => Self::AuditEnabled,
            "audit.keepFiles" => Self::AuditKeepFiles,
            "audit.maxFileBytes" => Self::AuditMaxFileBytes,
            "providers.mlx.models" => Self::ProvidersMlxModels,
            "retention.quotaBytes" => Self::RetentionQuotaBytes,
            "journal" | "journal.quotaBytes" => Self::JournalQuotaBytes,
            "jobs" => Self::Jobs,
            "jobs.liveService" => Self::JobsLiveService,
            "jobs.liveAgent" => Self::JobsLiveAgent,
            "jobs.queuedAgent" => Self::JobsQueuedAgent,
            "jobs.clientViewers" => Self::JobsClientViewers,
            "jobs.modelReservedPageReads" => Self::JobsModelReservedPageReads,
            _ => return None,
        })
    }
}
/// The origin of a job (tool-job-controller.md, kind 26 field 1).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[repr(u8)]
pub enum JobOrigin {
    Model = 1,
    Person = 2,
}
/// The first failing start condition of a queued job (J2 start conditions).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobBlockReason {
    Recovering,
    JournalUnavailable,
    JournalHeld,
    PayloadUnavailable,
    AgentSlots,
    ServiceSlots,
    PendingInstalls,
    Quota,
    DiskSpace,
}
/// The guardian profile token of a job. P1 adds the person profiles.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum JobProfile {
    #[serde(rename = "model-1")]
    Model1,
}
/// The requester of a job cancel (kind 27 field 2).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[repr(u8)]
pub enum JobRequester {
    Model = 1,
    Client = 2,
    Person = 3,
    Service = 4,
}
/// Job reason codes (tool-job-controller.md, job reason codes).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[repr(u8)]
pub enum JobReason {
    None = 0,
    ModelCancel = 1,
    TurnEnded = 2,
    ClientCancel = 3,
    PersonStop = 4,
    PersonKill = 5,
    IdentityChanged = 6,
    Shutdown = 7,
    CaptureFault = 8,
    Quota = 9,
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
    /// Whether kind 27 allows this reason for `requester` (reason table).
    pub fn allowed_for(self, requester: JobRequester) -> bool {
        use JobReason as R;
        match requester {
            JobRequester::Model => matches!(self, R::ModelCancel | R::TurnEnded),
            JobRequester::Client => self == R::ClientCancel,
            JobRequester::Person => matches!(self, R::PersonStop | R::PersonKill),
            JobRequester::Service => matches!(
                self,
                R::IdentityChanged | R::Shutdown | R::CaptureFault | R::Quota | R::Enospc
            ),
        }
    }
}
/// The settled outcome of a job (kind 28 field 2).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[repr(u8)]
pub enum JobOutcome {
    Exited = 1,
    Signalled = 2,
    Cancelled = 3,
    NotStarted = 4,
    Interrupted = 5,
}
/// The leader's exit kind (kind 28 field 4).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[repr(u8)]
pub enum JobExitKind {
    Unknown = 0,
    Exited = 1,
    Signalled = 2,
}
/// The cleanup fact of a job (kind 28 field 6).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[repr(u8)]
pub enum JobCleanup {
    None = 0,
    Confirmed = 1,
    Late = 2,
}
/// The guardian kind 4 reason behind a late cleanup (kind 28 field 7).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[repr(u8)]
pub enum JobUnconfirmedReason {
    None = 0,
    CleanupFailed = 9,
    EscapeeUnterminated = 13,
}
/// The capture state of a settled job (kind 28 field 10).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[repr(u8)]
pub enum JobCaptureState {
    Complete = 1,
    Incomplete = 2,
    Unconfirmed = 3,
    NotStarted = 4,
    Interrupted = 5,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigOutcome {
    Persisted,
    Unconfirmed,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    Stop,
    Signal,
    OwnerLifetimeEnded,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "details", deny_unknown_fields)]
pub enum Event {
    #[serde(rename = "service.started")]
    ServiceStarted {
        configuration: ConfigLoad,
        keep_files: u32,
        max_file_bytes: u64,
    },
    #[serde(rename = "config.changed")]
    ConfigChanged {
        setting: Setting,
        outcome: ConfigOutcome,
    },
    #[serde(rename = "service.stopping")]
    ServiceStopping { reason: StopReason },
    #[serde(rename = "journal.gap")]
    JournalGap {
        count: u64,
        first_sequence: u64,
        last_sequence: u64,
        reason: HealthReason,
    },
    #[serde(rename = "conversation.admission")]
    ConversationAdmission {
        #[serde(with = "id")]
        project: Id,
        #[serde(with = "id")]
        request: Id,
        #[serde(default, skip_serializing_if = "Option::is_none", with = "optional_id")]
        conversation: Option<Id>,
        #[serde(default, skip_serializing_if = "Option::is_none", with = "optional_id")]
        operation: Option<Id>,
        requested_generation: u64,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "present_u64"
        )]
        current_generation: Option<u64>,
        outcome: AdmissionOutcome,
        reason: DecisionReason,
    },
    #[serde(rename = "conversation.finished")]
    ConversationFinished {
        #[serde(with = "id")]
        project: Id,
        #[serde(with = "id")]
        conversation: Id,
        #[serde(with = "id")]
        operation: Id,
        generation: u64,
        outcome: FinishedOutcome,
        reason: DecisionReason,
    },
    #[serde(rename = "tool.finished")]
    ToolFinished {
        #[serde(with = "id")]
        project: Id,
        #[serde(with = "id")]
        operation: Id,
        generation: u64,
        ordinal: u32,
        tool: Tool,
        outcome: ToolOutcome,
    },
    /// A job first waits for a start condition; once per job.
    #[serde(rename = "job.queued")]
    JobQueued {
        #[serde(with = "id")]
        project: Id,
        #[serde(with = "id")]
        conversation: Id,
        #[serde(with = "id")]
        job: Id,
        origin: JobOrigin,
        agent: u32,
        reason: JobBlockReason,
    },
    /// The guardian's spawned notice arrived. A model job carries
    /// `operation`, `generation` and `ordinal`; a person job carries `input`.
    #[serde(rename = "job.started")]
    JobStarted {
        #[serde(with = "id")]
        project: Id,
        #[serde(with = "id")]
        conversation: Id,
        #[serde(with = "id")]
        job: Id,
        origin: JobOrigin,
        agent: u32,
        profile: JobProfile,
        #[serde(default, skip_serializing_if = "Option::is_none", with = "optional_id")]
        operation: Option<Id>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "present_u64"
        )]
        generation: Option<u64>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "present_u32"
        )]
        ordinal: Option<u32>,
        #[serde(default, skip_serializing_if = "Option::is_none", with = "optional_id")]
        input: Option<Id>,
    },
    /// Kind 27 committed.
    #[serde(rename = "job.cancel_requested")]
    JobCancelRequested {
        #[serde(with = "id")]
        project: Id,
        #[serde(with = "id")]
        conversation: Id,
        #[serde(with = "id")]
        job: Id,
        requester: JobRequester,
        reason: JobReason,
        kill: bool,
    },
    /// Kind 28 committed. `escapees` is a count only (escapee control).
    #[serde(rename = "job.settled")]
    JobSettled {
        #[serde(with = "id")]
        project: Id,
        #[serde(with = "id")]
        conversation: Id,
        #[serde(with = "id")]
        job: Id,
        outcome: JobOutcome,
        reason: JobReason,
        exit_kind: JobExitKind,
        exit_value: u32,
        cleanup: JobCleanup,
        unconfirmed_reason: JobUnconfirmedReason,
        escapees: u32,
        capture_state: JobCaptureState,
        stdout_bytes: u64,
        stderr_bytes: u64,
        elapsed_ms: u64,
        pause_count: u32,
    },
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Record {
    pub schema: u32,
    pub service_build: String,
    #[serde(with = "id")]
    pub service_epoch: Id,
    pub sequence: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unix_time_ms: Option<u64>,
    #[serde(flatten)]
    pub event: Event,
}
impl Event {
    pub fn project(&self) -> Option<Id> {
        match self {
            Self::ConversationAdmission { project, .. }
            | Self::ConversationFinished { project, .. }
            | Self::ToolFinished { project, .. }
            | Self::JobQueued { project, .. }
            | Self::JobStarted { project, .. }
            | Self::JobCancelRequested { project, .. }
            | Self::JobSettled { project, .. } => Some(*project),
            _ => None,
        }
    }
    pub fn validate(&self) -> Result<(), Error> {
        let nonzero = |id: &Id| *id != [0; 16];
        let valid = match self {
            Self::ServiceStarted {
                keep_files,
                max_file_bytes,
                ..
            } => Settings {
                enabled: true,
                keep_files: *keep_files,
                max_file_bytes: *max_file_bytes,
            }
            .validate()
            .is_ok(),
            Self::ConfigChanged { .. } | Self::ServiceStopping { .. } => true,
            Self::JournalGap {
                count,
                first_sequence,
                last_sequence,
                ..
            } => *count > 0 && *first_sequence > 0 && first_sequence <= last_sequence,
            Self::ConversationAdmission {
                project,
                request,
                conversation,
                operation,
                ..
            } => {
                nonzero(project)
                    && nonzero(request)
                    && conversation.as_ref().is_none_or(nonzero)
                    && operation.as_ref().is_none_or(nonzero)
            }
            Self::ConversationFinished {
                project,
                conversation,
                operation,
                generation,
                ..
            } => nonzero(project) && nonzero(conversation) && nonzero(operation) && *generation > 0,
            Self::ToolFinished {
                project,
                operation,
                generation,
                ordinal,
                ..
            } => {
                nonzero(project)
                    && nonzero(operation)
                    && *generation > 0
                    // Every recorded call, including the over-limit record.
                    && (1..=crate::authority::conversation::MAX_TOOL_CALL_ORDINAL).contains(ordinal)
            }
            Self::JobQueued {
                project,
                conversation,
                job,
                ..
            } => nonzero(project) && nonzero(conversation) && nonzero(job),
            Self::JobStarted {
                project,
                conversation,
                job,
                origin,
                operation,
                generation,
                ordinal,
                input,
                ..
            } => {
                nonzero(project)
                    && nonzero(conversation)
                    && nonzero(job)
                    && match origin {
                        JobOrigin::Model => {
                            operation.as_ref().is_some_and(nonzero)
                                && generation.is_some_and(|g| g > 0)
                                && ordinal.is_some_and(|o| {
                                    (1..=crate::authority::conversation::MAX_TOOL_CALL_ORDINAL)
                                        .contains(&o)
                                })
                                && input.is_none()
                        }
                        JobOrigin::Person => {
                            input.as_ref().is_some_and(nonzero)
                                && operation.is_none()
                                && generation.is_none()
                                && ordinal.is_none()
                        }
                    }
            }
            Self::JobCancelRequested {
                project,
                conversation,
                job,
                requester,
                reason,
                kill,
            } => {
                nonzero(project)
                    && nonzero(conversation)
                    && nonzero(job)
                    && reason.allowed_for(*requester)
                    // The kill flag goes with `person_kill` alone (J2d).
                    && *kill == (*reason == JobReason::PersonKill)
            }
            Self::JobSettled {
                project,
                conversation,
                job,
                ..
            } => nonzero(project) && nonzero(conversation) && nonzero(job) && self.settled_facts(),
        };
        if valid { Ok(()) } else { Err(Error::Invalid) }
    }
}
impl Event {
    /// The kind 28 outcome rules, applied to the facts `job.settled` carries.
    fn settled_facts(&self) -> bool {
        let Self::JobSettled {
            outcome,
            reason,
            exit_kind,
            exit_value,
            cleanup,
            unconfirmed_reason,
            escapees,
            capture_state,
            stdout_bytes,
            stderr_bytes,
            elapsed_ms,
            pause_count,
            ..
        } = self
        else {
            return false;
        };
        let code = *reason as u8;
        let exit = match exit_kind {
            JobExitKind::Unknown => *exit_value == 0,
            JobExitKind::Exited => *exit_value <= 255,
            JobExitKind::Signalled => (1..=127).contains(exit_value),
        };
        // An unconfirmed reason exists exactly when cleanup was late.
        let unconfirmed =
            (*cleanup == JobCleanup::Late) == (*unconfirmed_reason != JobUnconfirmedReason::None);
        let ran = matches!(cleanup, JobCleanup::Confirmed | JobCleanup::Late)
            && matches!(
                capture_state,
                JobCaptureState::Complete
                    | JobCaptureState::Incomplete
                    | JobCaptureState::Unconfirmed
            );
        let no_process = *exit_kind == JobExitKind::Unknown
            && *cleanup == JobCleanup::None
            && *escapees == 0
            && *elapsed_ms == 0;
        let outcome_rules = match outcome {
            JobOutcome::Exited => code <= 10 && *exit_kind == JobExitKind::Exited && ran,
            JobOutcome::Signalled => code <= 10 && *exit_kind == JobExitKind::Signalled && ran,
            JobOutcome::Cancelled => {
                (1..=8).contains(&code) && *exit_kind != JobExitKind::Unknown && ran
            }
            JobOutcome::NotStarted => {
                ((1..=7).contains(&code) || (12..=18).contains(&code))
                    && no_process
                    && *capture_state == JobCaptureState::NotStarted
            }
            JobOutcome::Interrupted => {
                *reason == JobReason::Restart
                    && no_process
                    && *capture_state == JobCaptureState::Interrupted
                    && *stdout_bytes == 0
                    && *stderr_bytes == 0
                    && *pause_count == 0
            }
        };
        exit && unconfirmed && outcome_rules
    }
}
impl Record {
    pub fn validate(&self) -> Result<(), Error> {
        if self.schema != 1
            || self.service_build.is_empty()
            || self.service_build.len() > 128
            || self.service_build.chars().any(char::is_control)
            || self.service_epoch == [0; 16]
            || self.sequence == 0
        {
            return Err(Error::Invalid);
        }
        self.event.validate()?;
        if serde_json::to_vec(self).map_err(|_| Error::Invalid)?.len() + 1 > MAX_RECORD_BYTES {
            return Err(Error::Limit);
        }
        Ok(())
    }
    pub fn encode(&self) -> Result<Vec<u8>, Error> {
        self.validate()?;
        let mut data = serde_json::to_vec(self).map_err(|_| Error::Invalid)?;
        data.push(b'\n');
        Ok(data)
    }
    pub fn decode(line: &[u8]) -> Result<Self, Error> {
        if line.len() > MAX_RECORD_BYTES || line.last() != Some(&b'\n') {
            return Err(Error::MalformedRecord);
        }
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Raw {
            schema: u32,
            service_build: String,
            #[serde(with = "id")]
            service_epoch: Id,
            sequence: u64,
            #[serde(default, deserialize_with = "present_u64")]
            unix_time_ms: Option<u64>,
            kind: String,
            details: Box<serde_json::value::RawValue>,
        }
        let raw: Raw = serde_json::from_slice(line).map_err(|_| Error::MalformedRecord)?;
        let event: Event = serde_json::from_str(&format!(
            "{{\"kind\":{},\"details\":{}}}",
            serde_json::to_string(&raw.kind).map_err(|_| Error::MalformedRecord)?,
            raw.details.get()
        ))
        .map_err(|_| Error::MalformedRecord)?;
        let result = Self {
            schema: raw.schema,
            service_build: raw.service_build,
            service_epoch: raw.service_epoch,
            sequence: raw.sequence,
            unix_time_ms: raw.unix_time_ms,
            event,
        };
        result.validate().map_err(|_| Error::MalformedRecord)?;
        Ok(result)
    }
}
mod id {
    use super::*;
    pub fn serialize<S: serde::Serializer>(value: &Id, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&value.iter().map(|b| format!("{b:02x}")).collect::<String>())
    }
    pub fn parse(text: &str) -> Result<Id, Error> {
        if text.len() != 32
            || !text
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(Error::Invalid);
        }
        let mut out = [0; 16];
        for (i, byte) in out.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&text[i * 2..i * 2 + 2], 16).map_err(|_| Error::Invalid)?;
        }
        if out == [0; 16] {
            Err(Error::Invalid)
        } else {
            Ok(out)
        }
    }
    pub fn deserialize<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Id, D::Error> {
        let text = String::deserialize(d)?;
        parse(&text).map_err(serde::de::Error::custom)
    }
}
mod optional_id {
    use super::*;
    pub fn serialize<S: serde::Serializer>(value: &Option<Id>, s: S) -> Result<S::Ok, S::Error> {
        match value {
            Some(v) => id::serialize(v, s),
            None => s.serialize_none(),
        }
    }
    pub fn deserialize<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<Id>, D::Error> {
        let text = String::deserialize(d)?;
        id::parse(&text).map(Some).map_err(serde::de::Error::custom)
    }
}

fn present_u64<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<u64>, D::Error> {
    u64::deserialize(d).map(Some)
}
fn present_u32<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<u32>, D::Error> {
    u32::deserialize(d).map(Some)
}

#[cfg(test)]
mod setting_tests {
    use super::*;
    #[test]
    fn journal_quota_setting_uses_its_config_key_name() {
        assert_eq!(
            serde_json::to_string(&Setting::JournalQuotaBytes).unwrap(),
            "\"journal.quotaBytes\""
        );
        assert_eq!(
            serde_json::from_str::<Setting>("\"journal.quotaBytes\"").unwrap(),
            Setting::JournalQuotaBytes
        );
        assert_ne!(Setting::JournalQuotaBytes, Setting::RetentionQuotaBytes);
    }
}
