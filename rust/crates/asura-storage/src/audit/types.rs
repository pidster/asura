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
    Shell = 9,
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
            | Self::ToolFinished { project, .. } => Some(*project),
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
                    && (1..=8).contains(ordinal)
            }
        };
        if valid { Ok(()) } else { Err(Error::Invalid) }
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
