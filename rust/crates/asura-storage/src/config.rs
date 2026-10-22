//! YAML configuration adapter. Invoke only from the supervised service config worker.
use asura_platform::{PrivateFileError, PrivateFileSnapshot, RuntimeDirectory};
use serde::de::{self, DeserializeSeed, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Serialize};
use serde_yaml_ng::{Mapping, Value};
use std::collections::BTreeMap;
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};

type Result<T> = std::result::Result<T, &'static str>;

fn file_error(error: PrivateFileError) -> &'static str {
    match error {
        PrivateFileError::UnsafeOrIo => "config_io_or_unsafe_file",
        PrivateFileError::CancelledOrExpired => "config_cancelled",
        PrivateFileError::TooLarge => "config_too_large",
        PrivateFileError::Read => "config_read_failed",
        PrivateFileError::Changed => "config_changed",
        PrivateFileError::Write => "config_write_failed",
        PrivateFileError::Sync => "config_sync_failed",
        PrivateFileError::Replace => "config_replace_failed",
        PrivateFileError::OutcomeUnconfirmed => "config_outcome_unconfirmed",
    }
}

#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Config {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    model: Option<String>,
    #[serde(default)]
    audit: Audit,
    #[serde(default, skip_serializing_if = "Providers::is_empty")]
    providers: Providers,
    #[serde(default, skip_serializing_if = "Retention::is_default")]
    retention: Retention,
    #[serde(default, skip_serializing_if = "Journal::is_default")]
    journal: Journal,
    #[serde(default, skip_serializing_if = "Jobs::is_default")]
    jobs: Jobs,
    /// The model thinking level (thinking-level.md); absent is `default`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    thinking: Option<String>,
}
/// Thinking level spellings in code order: 0 default, 1 off, 2 low,
/// 3 medium, 4 high (thinking-level.md).
pub const THINKING_LEVELS: [&str; 5] = ["default", "off", "low", "medium", "high"];
/// The code of a thinking level spelling.
pub fn thinking_code(name: &str) -> Option<u8> {
    THINKING_LEVELS
        .iter()
        .position(|level| *level == name)
        .map(|code| code as u8)
}
/// Retained exec_cmd output (J1 quota ledger).
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Retention {
    #[serde(default = "quota_bytes", rename = "quotaBytes")]
    quota_bytes: u64,
}
fn quota_bytes() -> u64 {
    crate::payload::DEFAULT_QUOTA
}
impl Default for Retention {
    fn default() -> Self {
        Self {
            quota_bytes: quota_bytes(),
        }
    }
}
impl Retention {
    fn is_default(&self) -> bool {
        self.quota_bytes == quota_bytes()
    }
}
const MIB: u64 = 1024 * 1024;
/// Smallest accepted `journal.quotaBytes` (64 MiB; segmented journal).
pub const JOURNAL_MIN_QUOTA: u64 = 64 * MIB;
/// Largest accepted `journal.quotaBytes` (1 TiB).
pub const JOURNAL_MAX_QUOTA: u64 = 1024 * 1024 * MIB;
/// `journal.quotaBytes` when absent (4 GiB; owner, 2026-10-19, OD-SJ-1).
pub const JOURNAL_DEFAULT_QUOTA: u64 = 4096 * MIB;
/// Segmented authority journal quota, separate from `retention`
/// (owner, 2026-10-19, OD-SJ-4).
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Journal {
    #[serde(default = "journal_quota_bytes", rename = "quotaBytes")]
    quota_bytes: u64,
}
fn journal_quota_bytes() -> u64 {
    JOURNAL_DEFAULT_QUOTA
}
impl Default for Journal {
    fn default() -> Self {
        Self {
            quota_bytes: journal_quota_bytes(),
        }
    }
}
impl Journal {
    fn is_default(&self) -> bool {
        self.quota_bytes == journal_quota_bytes()
    }
}
/// Tool job counts (tool-job-controller.md, J2 configuration keys; owner
/// J2a-2 and AQ1). Each field is `(default, minimum, maximum)`.
pub const JOBS_LIVE_SERVICE: (u32, u32, u32) = (4, 1, 16);
pub const JOBS_LIVE_AGENT: (u32, u32, u32) = (2, 1, 8);
pub const JOBS_QUEUED_AGENT: (u32, u32, u32) = (8, 1, 16);
pub const JOBS_CLIENT_VIEWERS: (u32, u32, u32) = (8, 1, 32);
pub const JOBS_MODEL_RESERVED_PAGE_READS: (u32, u32, u32) = (1, 0, 1);
/// The `jobs` mapping, read at service start; written only away from its
/// defaults.
#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Jobs {
    #[serde(default = "live_service", rename = "liveService")]
    live_service: u64,
    #[serde(default = "live_agent", rename = "liveAgent")]
    live_agent: u64,
    #[serde(default = "queued_agent", rename = "queuedAgent")]
    queued_agent: u64,
    #[serde(default = "client_viewers", rename = "clientViewers")]
    client_viewers: u64,
    #[serde(
        default = "model_reserved_page_reads",
        rename = "modelReservedPageReads"
    )]
    model_reserved_page_reads: u64,
}
fn live_service() -> u64 {
    JOBS_LIVE_SERVICE.0.into()
}
fn live_agent() -> u64 {
    JOBS_LIVE_AGENT.0.into()
}
fn queued_agent() -> u64 {
    JOBS_QUEUED_AGENT.0.into()
}
fn client_viewers() -> u64 {
    JOBS_CLIENT_VIEWERS.0.into()
}
fn model_reserved_page_reads() -> u64 {
    JOBS_MODEL_RESERVED_PAGE_READS.0.into()
}
const JOBS_FIELDS: [&str; 5] = [
    "liveService",
    "liveAgent",
    "queuedAgent",
    "clientViewers",
    "modelReservedPageReads",
];
impl Default for Jobs {
    fn default() -> Self {
        Self {
            live_service: live_service(),
            live_agent: live_agent(),
            queued_agent: queued_agent(),
            client_viewers: client_viewers(),
            model_reserved_page_reads: model_reserved_page_reads(),
        }
    }
}
impl Jobs {
    fn is_default(&self) -> bool {
        *self == Self::default()
    }
    fn valid(&self) -> bool {
        let within = |value: u64, (_, min, max): (u32, u32, u32)| {
            (u64::from(min)..=u64::from(max)).contains(&value)
        };
        within(self.live_service, JOBS_LIVE_SERVICE)
            && within(self.live_agent, JOBS_LIVE_AGENT)
            && within(self.queued_agent, JOBS_QUEUED_AGENT)
            && within(self.client_viewers, JOBS_CLIENT_VIEWERS)
            && within(
                self.model_reserved_page_reads,
                JOBS_MODEL_RESERVED_PAGE_READS,
            )
    }
    /// The effective limits of a validated mapping.
    fn limits(&self) -> JobLimits {
        let narrow = |value: u64| u32::try_from(value).unwrap_or(u32::MAX);
        let live_service = narrow(self.live_service);
        JobLimits {
            live_service,
            live_agent: narrow(self.live_agent).min(live_service),
            queued_agent: narrow(self.queued_agent),
            client_viewers: narrow(self.client_viewers),
            model_reserved_page_reads: narrow(self.model_reserved_page_reads),
        }
    }
}
/// The effective job limits that a service applies from start to stop
/// (tool-job-controller.md, J2 configuration keys).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct JobLimits {
    /// Live jobs in the service (`jobs.liveService`).
    pub live_service: u32,
    /// Live jobs per agent: the smaller of `jobs.liveAgent` and
    /// `jobs.liveService`.
    pub live_agent: u32,
    /// Starts that may wait in one agent's job queue (`jobs.queuedAgent`).
    pub queued_agent: u32,
    /// Pending client job reads per service (`jobs.clientViewers`; J4).
    pub client_viewers: u32,
    /// Page-read workers reserved for model requests
    /// (`jobs.modelReservedPageReads`).
    pub model_reserved_page_reads: u32,
}
impl Default for JobLimits {
    fn default() -> Self {
        Jobs::default().limits()
    }
}
#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Providers {
    #[serde(default, skip_serializing_if = "Ollama::is_empty")]
    ollama: Ollama,
    #[serde(default, skip_serializing_if = "Mlx::is_empty")]
    mlx: Mlx,
}
impl Providers {
    fn is_empty(&self) -> bool {
        self.ollama.is_empty() && self.mlx.is_empty()
    }
}
#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Mlx {
    #[serde(default)]
    models: BTreeMap<String, ModelCapabilities>,
}
impl Mlx {
    fn is_empty(&self) -> bool {
        self.models.is_empty()
    }
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelCapabilities {
    capabilities: Vec<String>,
}
impl ModelCapabilities {
    fn mask(&self) -> Result<u32> {
        if self.capabilities.len() > 4 {
            return Err("config_invalid_value");
        }
        let mut mask = 0;
        for capability in &self.capabilities {
            let bit = match capability.as_str() {
                "toolCalling" => 1,
                "guidedGeneration" => 2,
                "reasoning" => 4,
                "vision" => 8,
                _ => return Err("config_invalid_value"),
            };
            if mask & bit != 0 {
                return Err("config_invalid_value");
            }
            mask |= bit;
        }
        Ok(mask)
    }
}
fn valid_asset_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 256
        && !name.chars().any(char::is_control)
        && !name.contains('\\')
        && name
            .split('/')
            .all(|part| !part.is_empty() && part != "." && part != "..")
}
#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Ollama {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    endpoint: Option<String>,
}
impl Ollama {
    fn is_empty(&self) -> bool {
        self.endpoint.is_none()
    }
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Audit {
    #[serde(default = "enabled")]
    enabled: bool,
    #[serde(default = "keep_files", rename = "keepFiles")]
    keep_files: u64,
    #[serde(default = "max_file_bytes", rename = "maxFileBytes")]
    max_file_bytes: u64,
}
fn enabled() -> bool {
    true
}
fn keep_files() -> u64 {
    5
}
fn max_file_bytes() -> u64 {
    10_485_760
}
impl Default for Audit {
    fn default() -> Self {
        Self {
            enabled: enabled(),
            keep_files: keep_files(),
            max_file_bytes: max_file_bytes(),
        }
    }
}

// Parse a small closed tree before deriving the typed schema. This preserves
// duplicate detection and rejects custom YAML tags/nulls. Typed schema validation
// permits bounded sequences only in capability declarations.
struct Strict(u8);
impl<'de> DeserializeSeed<'de> for Strict {
    type Value = Value;
    fn deserialize<D: de::Deserializer<'de>>(
        self,
        deserializer: D,
    ) -> std::result::Result<Value, D::Error> {
        deserializer.deserialize_any(self)
    }
}
impl<'de> Visitor<'de> for Strict {
    type Value = Value;
    fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
        formatter.write_str("a bounded configuration scalar or mapping")
    }
    fn visit_bool<E: de::Error>(self, value: bool) -> std::result::Result<Value, E> {
        Ok(Value::Bool(value))
    }
    fn visit_i64<E: de::Error>(self, value: i64) -> std::result::Result<Value, E> {
        Ok(Value::Number(value.into()))
    }
    fn visit_u64<E: de::Error>(self, value: u64) -> std::result::Result<Value, E> {
        Ok(Value::Number(value.into()))
    }
    fn visit_str<E: de::Error>(self, value: &str) -> std::result::Result<Value, E> {
        Ok(Value::String(value.to_owned()))
    }
    fn visit_string<E: de::Error>(self, value: String) -> std::result::Result<Value, E> {
        Ok(Value::String(value))
    }
    fn visit_seq<A: SeqAccess<'de>>(self, mut sequence: A) -> std::result::Result<Value, A::Error> {
        if self.0 >= 6 {
            return Err(de::Error::custom("nesting limit"));
        }
        let mut values = Vec::new();
        while let Some(value) = sequence.next_element_seed(Strict(6))? {
            if values.len() >= 4 || !value.is_string() {
                return Err(de::Error::custom("invalid capability sequence"));
            }
            values.push(value);
        }
        Ok(Value::Sequence(values))
    }
    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> std::result::Result<Value, A::Error> {
        if self.0 >= 6 {
            return Err(de::Error::custom("nesting limit"));
        }
        let mut values = Mapping::new();
        while let Some(key) = map.next_key_seed(Strict(6))? {
            if !matches!(key, Value::String(_)) || values.contains_key(&key) || values.len() >= 64 {
                return Err(de::Error::custom("invalid or duplicate key"));
            }
            let value = map.next_value_seed(Strict(self.0 + 1))?;
            values.insert(key, value);
        }
        Ok(Value::Mapping(values))
    }
}
fn parse(bytes: &[u8]) -> Result<Value> {
    let mut documents = serde_yaml_ng::Deserializer::from_slice(bytes);
    let document = documents.next().ok_or("config_invalid_yaml")?;
    let value = Strict(0)
        .deserialize(document)
        .map_err(|_| "config_invalid_yaml")?;
    if documents.next().is_some() {
        return Err("config_invalid_yaml");
    }
    Ok(value)
}
impl Config {
    fn read(bytes: &[u8]) -> Result<Self> {
        if bytes.iter().all(u8::is_ascii_whitespace) {
            return Ok(Self::default());
        }
        let value = parse(bytes)?;
        if !value.is_mapping() {
            return Err("config_invalid_schema");
        }
        let config: Self = serde_yaml_ng::from_value(value).map_err(|_| "config_invalid_schema")?;
        config.validate()?;
        Ok(config)
    }
    fn validate(&self) -> Result<()> {
        if self.model.as_ref().is_some_and(|model| {
            model.is_empty() || model.len() > 1024 || model.chars().any(char::is_control)
        }) || !(1..=10_000).contains(&self.audit.keep_files)
            || !(1..=1_099_511_627_776).contains(&self.audit.max_file_bytes)
            || !(crate::payload::MIN_QUOTA..=crate::payload::MAX_QUOTA)
                .contains(&self.retention.quota_bytes)
            || !(JOURNAL_MIN_QUOTA..=JOURNAL_MAX_QUOTA).contains(&self.journal.quota_bytes)
            || !self.jobs.valid()
            || self
                .thinking
                .as_deref()
                .is_some_and(|level| thinking_code(level).is_none())
        {
            return Err("config_invalid_value");
        }
        if self
            .providers
            .ollama
            .endpoint
            .as_ref()
            .is_some_and(|endpoint| !valid_endpoint(endpoint))
        {
            return Err("config_invalid_value");
        }
        if self.providers.mlx.models.len() > 64 {
            return Err("config_invalid_value");
        }
        for (asset, capabilities) in &self.providers.mlx.models {
            if !valid_asset_name(asset) {
                return Err("config_invalid_value");
            }
            capabilities.mask()?;
        }
        Ok(())
    }
    /// The effective thinking level code.
    fn thinking(&self) -> u8 {
        self.thinking
            .as_deref()
            .and_then(thinking_code)
            .unwrap_or(0)
    }
    fn model_capabilities(&self) -> Option<u32> {
        let asset = self.model.as_deref()?.strip_prefix("mlx:")?;
        self.providers.mlx.models.get(asset)?.mask().ok()
    }
    fn get(&self, key: &str) -> Result<String> {
        match key {
            "" => yaml(self),
            "model" => yaml(self.model.as_ref().ok_or("config_model_unset")?),
            "providers" => yaml(&self.providers),
            "providers.ollama" => yaml(&self.providers.ollama),
            "providers.mlx" => yaml(&self.providers.mlx),
            "providers.mlx.models" => yaml(&self.providers.mlx.models),
            "providers.ollama.endpoint" => yaml(
                self.providers
                    .ollama
                    .endpoint
                    .as_ref()
                    .ok_or("config_endpoint_unset")?,
            ),
            "audit" => yaml(&self.audit),
            "audit.enabled" => yaml(&self.audit.enabled),
            "audit.keepFiles" => yaml(&self.audit.keep_files),
            "audit.maxFileBytes" => yaml(&self.audit.max_file_bytes),
            "retention" => yaml(&RetentionView(&self.retention)),
            "retention.quotaBytes" => yaml(&self.retention.quota_bytes),
            "journal" => yaml(&self.journal),
            "journal.quotaBytes" => yaml(&self.journal.quota_bytes),
            "jobs" => yaml(&self.jobs),
            "jobs.liveService" => yaml(&self.jobs.live_service),
            "jobs.liveAgent" => yaml(&self.jobs.live_agent),
            "jobs.queuedAgent" => yaml(&self.jobs.queued_agent),
            "jobs.clientViewers" => yaml(&self.jobs.client_viewers),
            "jobs.modelReservedPageReads" => yaml(&self.jobs.model_reserved_page_reads),
            "thinking" => yaml(&THINKING_LEVELS[usize::from(self.thinking())]),
            _ => Err("config_unknown_key"),
        }
    }
    fn set(&mut self, key: &str, text: &str) -> Result<()> {
        if text.len() > 4096 {
            return Err("config_value_too_large");
        }
        let value = parse(text.as_bytes())?;
        match key {
            "model" => {
                let Value::String(model) = value else {
                    return Err("config_invalid_value");
                };
                self.model = Some(model);
            }
            "providers.mlx.models" => {
                self.providers.mlx.models =
                    serde_yaml_ng::from_value(value).map_err(|_| "config_invalid_value")?;
            }
            "providers.ollama.endpoint" => {
                let Value::String(endpoint) = value else {
                    return Err("config_invalid_value");
                };
                if valid_endpoint(&endpoint) && !loopback_endpoint(&endpoint) {
                    return Err("config_remote_endpoint_restricted");
                }
                self.providers.ollama.endpoint = Some(endpoint);
            }
            "audit" => {
                let Value::Mapping(ref fields) = value else {
                    return Err("config_invalid_value");
                };
                if fields.len() != 3
                    || ["enabled", "keepFiles", "maxFileBytes"]
                        .iter()
                        .any(|field| !fields.contains_key(Value::String((*field).into())))
                {
                    return Err("config_invalid_value");
                }
                self.audit =
                    serde_yaml_ng::from_value(value).map_err(|_| "config_invalid_value")?;
            }
            "audit.enabled" => {
                let Value::Bool(enabled) = value else {
                    return Err("config_invalid_value");
                };
                self.audit.enabled = enabled;
            }
            "audit.keepFiles" => {
                self.audit.keep_files = value.as_u64().ok_or("config_invalid_value")?
            }
            "audit.maxFileBytes" => {
                self.audit.max_file_bytes = value.as_u64().ok_or("config_invalid_value")?
            }
            "retention.quotaBytes" => {
                self.retention.quota_bytes = value.as_u64().ok_or("config_invalid_value")?
            }
            "journal" => {
                let Value::Mapping(ref fields) = value else {
                    return Err("config_invalid_value");
                };
                if fields.len() != 1 || !fields.contains_key(Value::String("quotaBytes".into())) {
                    return Err("config_invalid_value");
                }
                self.journal =
                    serde_yaml_ng::from_value(value).map_err(|_| "config_invalid_value")?;
            }
            "journal.quotaBytes" => {
                self.journal.quota_bytes = value.as_u64().ok_or("config_invalid_value")?
            }
            "jobs" => {
                // The whole mapping: every field present, no other field.
                let Value::Mapping(ref fields) = value else {
                    return Err("config_invalid_value");
                };
                if fields.len() != JOBS_FIELDS.len()
                    || JOBS_FIELDS
                        .iter()
                        .any(|field| !fields.contains_key(Value::String((*field).into())))
                {
                    return Err("config_invalid_value");
                }
                self.jobs = serde_yaml_ng::from_value(value).map_err(|_| "config_invalid_value")?;
            }
            "jobs.liveService" => {
                self.jobs.live_service = value.as_u64().ok_or("config_invalid_value")?
            }
            "jobs.liveAgent" => {
                self.jobs.live_agent = value.as_u64().ok_or("config_invalid_value")?
            }
            "jobs.queuedAgent" => {
                self.jobs.queued_agent = value.as_u64().ok_or("config_invalid_value")?
            }
            "jobs.clientViewers" => {
                self.jobs.client_viewers = value.as_u64().ok_or("config_invalid_value")?
            }
            "jobs.modelReservedPageReads" => {
                self.jobs.model_reserved_page_reads =
                    value.as_u64().ok_or("config_invalid_value")?
            }
            "thinking" => {
                let Value::String(level) = value else {
                    return Err("config_invalid_value");
                };
                // `default` removes the key: absent is the default.
                self.thinking = match thinking_code(&level) {
                    Some(0) => None,
                    Some(_) => Some(level),
                    None => return Err("config_invalid_value"),
                };
            }
            _ => return Err("config_unknown_key"),
        }
        self.validate()
    }
}

/// Restricted endpoint grammar shared with the private helper transport contract.
fn valid_endpoint(value: &str) -> bool {
    if value.len() > 2048
        || !value.is_ascii()
        || value.bytes().any(|b| {
            b.is_ascii_whitespace()
                || b.is_ascii_control()
                || matches!(b, b'%' | b'\\' | b'@' | b'?' | b'#')
        })
    {
        return false;
    }
    let Some((scheme, address)) = value.split_once("://") else {
        return false;
    };
    if !matches!(scheme, "http" | "https") {
        return false;
    }
    let address = address.strip_suffix('/').unwrap_or(address);
    if address.contains('/') {
        return false;
    }
    let (host, port) = if let Some(bracketed) = address.strip_prefix('[') {
        let Some((host, suffix)) = bracketed.split_once(']') else {
            return false;
        };
        if host.parse::<std::net::Ipv6Addr>().is_err() {
            return false;
        }
        let port = if suffix.is_empty() {
            None
        } else {
            let Some(port) = suffix.strip_prefix(':') else {
                return false;
            };
            Some(port)
        };
        (host, port)
    } else {
        let (host, port) = address
            .split_once(':')
            .map_or((address, None), |(host, port)| (host, Some(port)));
        if host.is_empty()
            || host.len() > 253
            || !host.split('.').all(|label| {
                !label.is_empty()
                    && label.len() <= 63
                    && !label.starts_with('-')
                    && !label.ends_with('-')
                    && label
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'-')
            })
        {
            return false;
        }
        (host, port)
    };
    if port.is_some_and(|port| {
        port.is_empty()
            || !port.bytes().all(|b| b.is_ascii_digit())
            || port.parse::<u16>().map_or(true, |value| value == 0)
    }) {
        return false;
    }
    scheme == "https" || matches!(host, "127.0.0.1" | "::1")
}

/// Valid endpoint whose host is a numeric loopback address. Remote endpoints
/// stay restricted until the execution threat model is reviewed.
pub fn loopback_endpoint(value: &str) -> bool {
    if !valid_endpoint(value) {
        return false;
    }
    let address = value.split_once("://").map_or("", |(_, address)| address);
    let address = address.strip_suffix('/').unwrap_or(address);
    let host = match address.strip_prefix('[') {
        Some(bracketed) => bracketed.split_once(']').map_or("", |(host, _)| host),
        None => address.split_once(':').map_or(address, |(host, _)| host),
    };
    matches!(host, "127.0.0.1" | "::1")
}

/// The retention mapping with its default, for `get retention`.
struct RetentionView<'a>(&'a Retention);
impl Serialize for RetentionView<'_> {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut map = serializer.serialize_map(Some(1))?;
        map.serialize_entry("quotaBytes", &self.0.quota_bytes)?;
        map.end()
    }
}

fn yaml(value: &impl Serialize) -> Result<String> {
    serde_yaml_ng::to_string(value).map_err(|_| "config_encode_failed")
}

/// A completed config operation.
#[derive(Debug, PartialEq, Eq)]
pub struct Reply {
    /// The effective YAML value of the key after the operation.
    pub value_yaml: String,
    /// The new effective `journal.quotaBytes` after a persisted set of
    /// `journal` or `journal.quotaBytes`, which applies at once; else `None`.
    pub journal_quota: Option<u64>,
}

pub fn execute(
    runtime: RuntimeDirectory,
    key: &str,
    value: Option<&str>,
    cancel: &AtomicBool,
) -> Result<Reply> {
    if !(key.is_empty() && value.is_none())
        && !matches!(
            key,
            "model"
                | "audit"
                | "audit.enabled"
                | "audit.keepFiles"
                | "audit.maxFileBytes"
                | "providers"
                | "providers.ollama"
                | "providers.ollama.endpoint"
                | "providers.mlx"
                | "providers.mlx.models"
                | "retention"
                | "retention.quotaBytes"
                | "journal"
                | "journal.quotaBytes"
                | "jobs"
                | "jobs.liveService"
                | "jobs.liveAgent"
                | "jobs.queuedAgent"
                | "jobs.clientViewers"
                | "jobs.modelReservedPageReads"
                | "thinking"
        )
    {
        return Err("config_unknown_key");
    }
    // No time bound (owner, 2026-10-16): the job ends on completion, an error
    // or the owner's cancel.
    let snapshot = PrivateFileSnapshot::read(runtime, "config.yaml", cancel).map_err(file_error)?;
    let mut config = Config::read(&snapshot.bytes)?;
    let mut journal_quota = None;
    if let Some(value) = value {
        config.set(key, value)?;
        let serialized = yaml(&config)?;
        snapshot
            .replace(serialized.as_bytes(), cancel)
            .map_err(file_error)?;
        if matches!(key, "journal" | "journal.quotaBytes") {
            journal_quota = Some(config.journal.quota_bytes);
        }
    } else if cancel.load(Ordering::Acquire) {
        return Err("config_cancelled");
    }
    Ok(Reply {
        value_yaml: config.get(key)?,
        journal_quota,
    })
}

/// The effective `retention.quotaBytes` (J1). Read once at service start on
/// the payload recovery worker; the work has no time bound.
pub fn retention_quota(runtime: RuntimeDirectory, cancel: &AtomicBool) -> Result<u64> {
    let snapshot = PrivateFileSnapshot::read(runtime, "config.yaml", cancel).map_err(file_error)?;
    Ok(Config::read(&snapshot.bytes)?.retention.quota_bytes)
}

/// The effective `journal.quotaBytes` (segmented journal), read with the same
/// bounded read pipeline on the caller's worker. The work has no time bound;
/// it ends on completion, an error or cancel.
pub fn journal_quota(runtime: RuntimeDirectory, cancel: &AtomicBool) -> Result<u64> {
    let snapshot = PrivateFileSnapshot::read(runtime, "config.yaml", cancel).map_err(file_error)?;
    Ok(Config::read(&snapshot.bytes)?.journal.quota_bytes)
}

/// The effective job limits (J2), read once at service start like
/// `retention.quotaBytes`; a running service keeps them until it stops. The
/// read runs on the caller's worker with the bounded read pipeline and has no
/// time bound; it ends on completion, an error or cancel.
pub fn job_limits(runtime: RuntimeDirectory, cancel: &AtomicBool) -> Result<JobLimits> {
    let snapshot = PrivateFileSnapshot::read(runtime, "config.yaml", cancel).map_err(file_error)?;
    Ok(Config::read(&snapshot.bytes)?.jobs.limits())
}

/// Startup-only typed audit settings from the canonical validated YAML snapshot.
pub fn audit_settings(
    runtime: RuntimeDirectory,
    cancel: &AtomicBool,
) -> Result<(crate::audit::Settings, crate::audit::ConfigLoad)> {
    let snapshot = PrivateFileSnapshot::read(runtime, "config.yaml", cancel).map_err(file_error)?;
    let config = Config::read(&snapshot.bytes)?;
    if cancel.load(Ordering::Acquire) {
        return Err("config_cancelled");
    }
    let settings = crate::audit::Settings {
        enabled: config.audit.enabled,
        keep_files: config.audit.keep_files as u32,
        max_file_bytes: config.audit.max_file_bytes,
    };
    let source = if snapshot.existed() {
        crate::audit::ConfigLoad::Loaded
    } else {
        crate::audit::ConfigLoad::Defaults
    };
    Ok((settings, source))
}

/// One validated configuration and destination snapshot for durable admission.
#[derive(Debug)]
pub struct ConversationSnapshot {
    pub model: String,
    pub asset_root: String,
    pub ollama_endpoint: Option<String>,
    pub model_capabilities: Option<u32>,
    /// The thinking level code, 0 default through 4 high (thinking-level.md).
    pub thinking: u8,
    pub digest: [u8; 32],
}

/// Validated canonical configuration identity for durable admission. The
/// read has no time bound (J1-0; owner, 2026-10-16); cancel or an error ends it.
pub fn conversation_snapshot(
    runtime: RuntimeDirectory,
    cancel: &AtomicBool,
) -> Result<ConversationSnapshot> {
    use sha2::{Digest, Sha256};
    let asset_root = runtime
        .model_assets_path()
        .into_os_string()
        .into_string()
        .map_err(|_| "config_io_or_unsafe_file")?;
    let snapshot = PrivateFileSnapshot::read(runtime, "config.yaml", cancel).map_err(file_error)?;
    let config = Config::read(&snapshot.bytes)?;
    let digest = Sha256::digest(yaml(&config)?.as_bytes()).into();
    let model_capabilities = config.model_capabilities();
    Ok(ConversationSnapshot {
        asset_root,
        model_capabilities,
        thinking: config.thinking(),
        model: config.model.unwrap_or_else(|| "system".into()),
        ollama_endpoint: config.providers.ollama.endpoint,
        digest,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn storage_execution_preserves_invalid_files_and_persists_settings() {
        use std::os::unix::fs::DirBuilderExt;
        struct Scratch(std::path::PathBuf);
        impl Drop for Scratch {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let root = Scratch(
            format!(
                "/private/tmp/asura-cfg-{:x}",
                u128::from_ne_bytes(asura_platform::random_id())
            )
            .into(),
        );
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&root.0)
            .unwrap();
        let runtime = RuntimeDirectory::scratch(&root.0, true).unwrap();
        let cancel = AtomicBool::new(false);
        let call = |key, value| {
            execute(runtime.clone(), key, value, &cancel).map(|reply| reply.value_yaml)
        };
        assert_eq!(call("audit.enabled", None).unwrap(), "true\n");
        call("model", Some("provider:model")).unwrap();
        call("audit.keepFiles", Some("8")).unwrap();
        assert_eq!(call("audit.keepFiles", None).unwrap(), "8\n");
        assert_eq!(call("model", None).unwrap(), "provider:model\n");
        let original = conversation_snapshot(runtime.clone(), &cancel).unwrap();
        let file = root.0.join(".asura/config.yaml");
        let before = std::fs::read(&file).unwrap();
        assert_eq!(
            call(
                "providers.ollama.endpoint",
                Some("https://models.example:8443")
            ),
            Err("config_remote_endpoint_restricted")
        );
        assert_eq!(std::fs::read(&file).unwrap(), before);
        call("providers.ollama.endpoint", Some("https://127.0.0.1:8443")).unwrap();
        let changed = conversation_snapshot(runtime.clone(), &cancel).unwrap();
        assert_eq!(changed.model, original.model);
        assert_eq!(
            changed.asset_root,
            root.0.join(".asura/data/models").to_str().unwrap()
        );
        assert_eq!(
            changed.ollama_endpoint.as_deref(),
            Some("https://127.0.0.1:8443")
        );
        assert_ne!(changed.digest, original.digest);
        assert_eq!(
            call("providers.ollama.endpoint", None).unwrap(),
            "https://127.0.0.1:8443\n"
        );
        assert!(
            call("", None)
                .unwrap()
                .contains("providers:\n  ollama:\n    endpoint:")
        );
        assert_eq!(changed.model_capabilities, None);
        call("providers.mlx.models", Some("{'family/model.v1': {capabilities: [toolCalling, reasoning]}, disabled: {capabilities: []}}")).unwrap();
        call("model", Some("mlx:family/model.v1")).unwrap();
        let enabled = conversation_snapshot(runtime.clone(), &cancel).unwrap();
        assert_eq!(enabled.model_capabilities, Some(5));
        assert_ne!(enabled.digest, changed.digest);
        assert!(
            call("providers.mlx.models", None)
                .unwrap()
                .contains("family/model.v1")
        );
        assert!(
            call("providers.mlx", None)
                .unwrap()
                .contains("capabilities:")
        );
        assert!(call("", None).unwrap().contains("mlx:"));
        call("model", Some("mlx:disabled")).unwrap();
        assert_eq!(
            conversation_snapshot(runtime.clone(), &cancel)
                .unwrap()
                .model_capabilities,
            Some(0)
        );
        call("model", Some("coreai:family/model.v1")).unwrap();
        assert_eq!(
            conversation_snapshot(runtime.clone(), &cancel)
                .unwrap()
                .model_capabilities,
            None
        );
        let path = root.0.join(".asura/config.yaml");
        let before = std::fs::read(&path).unwrap();
        assert!(call("audit.enabled", Some("not_a_boolean")).is_err());
        assert!(
            call(
                "providers.mlx.models",
                Some("{bad: {capabilities: [toolCalling, toolCalling]}}")
            )
            .is_err()
        );
        assert!(call("providers.ollama.endpoint", Some("http://remote.example")).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), before);
        std::fs::write(&path, b"model: invalid\nmodel: duplicate\n").unwrap();
        let invalid = std::fs::read(&path).unwrap();
        assert!(call("model", Some("replacement")).is_err());
        assert!(call("", None).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), invalid);
    }
    #[test]
    fn remote_endpoints_are_restricted_to_numeric_loopback() {
        for endpoint in [
            "http://127.0.0.1:11434",
            "http://[::1]:11434/",
            "https://127.0.0.1:8443",
            "https://[::1]",
        ] {
            assert!(loopback_endpoint(endpoint), "{endpoint}");
        }
        for endpoint in [
            "https://models.example",
            "https://localhost:11434",
            "https://127.0.0.2",
            "https://10.0.0.1:11434",
            "https://[::2]",
            "http://remote.example",
            "",
        ] {
            assert!(!loopback_endpoint(endpoint), "{endpoint}");
        }
        let mut config = Config::default();
        assert_eq!(
            config.set("providers.ollama.endpoint", "https://models.example"),
            Err("config_remote_endpoint_restricted")
        );
        assert_eq!(config.providers.ollama.endpoint, None);
    }
    #[test]
    fn endpoint_schema_is_closed_and_transport_grammar_is_explicit() {
        for endpoint in [
            "http://127.0.0.1:11434",
            "http://[::1]:11434/",
            "https://models.example",
            "https://127.0.0.1:8443",
        ] {
            assert!(valid_endpoint(endpoint), "{endpoint}");
        }
        for endpoint in [
            "",
            "http://localhost",
            "http://remote.example",
            "https://host/path",
            "https://host//",
            "https://user:secret@host",
            "https://host?key=secret",
            "https://host#fragment",
            "https://host:0",
            "https://host:65536",
            "https://host:",
            "https://%65xample.com",
            "https://host\\path",
            "https://host space",
            "file:///tmp/model",
            "https://[invalid]",
            "https://.host",
        ] {
            assert!(!valid_endpoint(endpoint), "{endpoint}");
        }
        let config =
            Config::read(b"providers: {ollama: {endpoint: 'https://models.example'}}").unwrap();
        assert_eq!(
            config.providers.ollama.endpoint.as_deref(),
            Some("https://models.example")
        );
        for document in [
            "providers: {ollama: {endpoint: true}}",
            "providers: {ollama: {endpoint: null}}",
            "providers: {ollama: {token: secret}}",
            "providers: {remote_classifier: {endpoint: 'https://host'}}",
            "providers: {ollama: {endpoint: 'https://a', endpoint: 'https://b'}}",
        ] {
            assert!(Config::read(document.as_bytes()).is_err(), "{document}");
        }
    }
    /// TL-02, TL-03 and TL-14 (thinking-level.md): the `thinking` key is
    /// absent by default, takes the five spellings, `default` removes it and
    /// other values reject without change.
    #[test]
    fn thinking_level_key_defaults_sets_removes_and_rejects() {
        let mut config = Config::default();
        assert_eq!(config.get("thinking").unwrap(), "default\n");
        assert_eq!(config.thinking(), 0);
        for (name, code) in [("off", 1), ("low", 2), ("medium", 3), ("high", 4)] {
            config.set("thinking", name).unwrap();
            assert_eq!(config.get("thinking").unwrap(), format!("{name}\n"));
            assert_eq!(config.thinking(), code);
            assert!(
                yaml(&config)
                    .unwrap()
                    .contains(&format!("thinking: {name}"))
            );
        }
        config.set("thinking", "default").unwrap();
        assert_eq!(config.thinking(), 0);
        assert!(!yaml(&config).unwrap().contains("thinking"));
        config.set("thinking", "high").unwrap();
        for invalid in [
            "max",
            "High",
            "true",
            "4",
            "null",
            "[high]",
            "!tag high",
            "''",
        ] {
            assert!(config.set("thinking", invalid).is_err(), "{invalid}");
            assert_eq!(config.thinking(), 4);
        }
        assert!(Config::read(b"thinking: maximum").is_err());
        assert_eq!(Config::read(b"thinking: low").unwrap().thinking(), 2);
        assert_eq!(Config::read(b"thinking: default").unwrap().thinking(), 0);
    }
    #[test]
    fn defaults_types_and_independent_keys() {
        let mut config = Config::read(b"").unwrap();
        assert_eq!(config.get("model"), Err("config_model_unset"));
        assert_eq!(config.get("audit.keepFiles").unwrap(), "5\n");
        assert_eq!(
            config.get("").unwrap(),
            "audit:\n  enabled: true\n  keepFiles: 5\n  maxFileBytes: 10485760\n"
        );
        config.set("model", "'provider:model'").unwrap();
        config.set("audit.enabled", "false").unwrap();
        assert_eq!(config.model.as_deref(), Some("provider:model"));
        assert!(!config.audit.enabled);
        assert_eq!(config.audit.max_file_bytes, 10_485_760);
        config
            .set("audit", "{enabled: true, keepFiles: 8, maxFileBytes: 1234}")
            .unwrap();
        assert_eq!(
            Config::read(yaml(&config).unwrap().as_bytes())
                .unwrap()
                .audit
                .keep_files,
            8
        );
    }
    #[test]
    fn standard_tags_and_aliases_still_obey_closed_schema_and_depth() {
        let config =
            Config::read(b"model: !!str true\naudit: {enabled: !!bool false, keepFiles: !!int 7}")
                .unwrap();
        assert_eq!(config.model.as_deref(), Some("true"));
        assert!(!config.audit.enabled);
        assert_eq!(config.audit.keep_files, 7);
        assert!(Config::read(b"model: !!bool true").is_err());
        assert!(Config::read(b"audit: &recursive {enabled: *recursive}").is_err());
        assert!(Config::read(b"audit: &a {enabled: &b {enabled: *a}}").is_err());
        assert!(Config::read(b"model: &name example").is_ok());
    }
    #[test]
    fn rejects_invalid_documents_without_permissive_yaml_coercion() {
        for document in [
            "model: null",
            "model: true",
            "model: ''",
            "unknown: true",
            "model: first\nmodel: second",
            "audit: {enabled: true, enabled: false}",
            "audit: {unknown: false}",
            "audit: null",
            "audit: {keepFiles: 0}",
            "audit: {maxFileBytes: 1099511627777}",
            "model: !tag hello",
            "!tag {model: hello}",
            "model: hello\n---\nmodel: next",
            "audit: {enabled: {nested: {bad: true}}}",
            "model: [a,b]",
        ] {
            assert!(
                Config::read(document.as_bytes()).is_err(),
                "accepted {document}"
            );
        }
        assert!(Config::read(b"audit: {enabled: false}").is_ok());
        for (key, value) in [
            ("model", "true"),
            ("model", "null"),
            ("audit.enabled", "yes"),
            ("audit.keepFiles", "-1"),
            ("audit.keepFiles", "10001"),
            ("audit.maxFileBytes", "0"),
            ("audit", "{enabled: true}"),
            ("model", "!tag test"),
            ("model", "\"line\\nfeed\""),
        ] {
            assert!(
                Config::default().set(key, value).is_err(),
                "accepted {key}: {value}"
            );
        }
    }
    #[test]
    fn mlx_capabilities_are_exact_explicit_bounded_and_typed() {
        let mut config = Config::default();
        config.set("model", "mlx:org/model.v1").unwrap();
        assert_eq!(config.model_capabilities(), None);
        config
            .set(
                "providers.mlx.models",
                "{'org/model.v1': {capabilities: []}}",
            )
            .unwrap();
        assert_eq!(config.model_capabilities(), Some(0));
        config.set("providers.mlx.models", "{'org/model.v1': {capabilities: [toolCalling, guidedGeneration, reasoning, vision]}}").unwrap();
        assert_eq!(config.model_capabilities(), Some(15));
        let roundtrip = Config::read(yaml(&config).unwrap().as_bytes()).unwrap();
        assert_eq!(roundtrip.model_capabilities(), Some(15));
        for selector in [
            "mlx:org/model",
            "mlx:ORG/model.v1",
            "coreai:org/model.v1",
            "ollama:org/model.v1",
            "system",
        ] {
            config.set("model", selector).unwrap();
            assert_eq!(config.model_capabilities(), None);
        }
        for invalid in [
            "{x: {}}",
            "{x: {capabilities: null}}",
            "{x: {capabilities: toolCalling}}",
            "{x: {capabilities: [toolCalling, toolCalling]}}",
            "{x: {capabilities: [unknown]}}",
            "{x: {capabilities: [true]}}",
            "{x: {capabilities: [[toolCalling]]}}",
            "{x: {capabilities: [{x: 1}]}}",
            "{x: {capabilities: [], enabled: true}}",
            "{x: {capabilities: []}, x: {capabilities: []}}",
            "{x: {capabilities: [toolCalling, guidedGeneration, reasoning, vision, unknown]}}",
        ] {
            assert!(
                Config::default()
                    .set("providers.mlx.models", invalid)
                    .is_err(),
                "accepted {invalid}"
            );
        }
        for asset in [
            "",
            "/absolute",
            "../outside",
            "a/../b",
            "a/./b",
            "a//b",
            "a/",
            "a\\b",
            "a\nb",
        ] {
            let value = format!(
                "{{{}: {{capabilities: []}}}}",
                serde_yaml_ng::to_string(asset).unwrap().trim()
            );
            assert!(!valid_asset_name(asset));
            assert!(
                Config::default()
                    .set("providers.mlx.models", &value)
                    .is_err(),
                "accepted {asset:?}"
            );
        }
        assert!(valid_asset_name(&"x".repeat(256)));
        assert!(!valid_asset_name(&"x".repeat(257)));
        let values = |count| {
            (0..count)
                .map(|n| format!("m{n}: {{capabilities: []}}\n"))
                .collect::<String>()
        };
        assert!(
            Config::default()
                .set("providers.mlx.models", &values(64))
                .is_ok()
        );
        assert!(
            Config::default()
                .set("providers.mlx.models", &values(65))
                .is_err()
        );
        assert!(
            Config::read(b"providers: {mlx: {models: {x: {capabilities: [toolCalling]}}}}").is_ok()
        );
        assert!(
            Config::read(b"providers: {mlx: {models: {x: {capabilities: {a: {b: {c: 1}}}}}}}")
                .is_err()
        );
        assert!(
            Config::default()
                .set("providers.mlx.models.org.model", "[]")
                .is_err()
        );
    }
    /// `retention.quotaBytes` (J1, G13): default 1 GiB, bounds 64 MiB to
    /// 1 TiB, persisted only when set away from the default.
    #[test]
    fn retention_quota_bytes_default_bounds_and_persistence() {
        let mut config = Config::read(b"").unwrap();
        assert_eq!(config.get("retention.quotaBytes").unwrap(), "1073741824\n");
        assert_eq!(config.get("retention").unwrap(), "quotaBytes: 1073741824\n");
        assert!(!yaml(&config).unwrap().contains("retention"));
        for bad in ["67108863", "1099511627777", "-1", "true", "'1'"] {
            assert_eq!(
                config.set("retention.quotaBytes", bad),
                Err("config_invalid_value"),
                "{bad}"
            );
        }
        config.set("retention.quotaBytes", "67108864").unwrap();
        config.set("retention.quotaBytes", "1099511627776").unwrap();
        let text = yaml(&config).unwrap();
        assert!(
            text.contains("retention:\n  quotaBytes: 1099511627776\n"),
            "{text}"
        );
        let again = Config::read(text.as_bytes()).unwrap();
        assert_eq!(again.retention.quota_bytes, 1_099_511_627_776);
        assert!(Config::read(b"retention: {quotaBytes: 1}").is_err());
        assert!(Config::read(b"retention: {other: 1}").is_err());
    }
    /// SJ-14 unit layer: `journal.quotaBytes` defaults to 4 GiB, accepts
    /// 64 MiB through 1 TiB, and is persisted only away from the default.
    #[test]
    fn journal_quota_bytes_default_bounds_and_persistence() {
        assert_eq!(JOURNAL_MIN_QUOTA, 67_108_864);
        assert_eq!(JOURNAL_MAX_QUOTA, 1_099_511_627_776);
        assert_eq!(JOURNAL_DEFAULT_QUOTA, 4_294_967_296);
        let mut config = Config::read(b"").unwrap();
        assert_eq!(config.get("journal.quotaBytes").unwrap(), "4294967296\n");
        assert_eq!(config.get("journal").unwrap(), "quotaBytes: 4294967296\n");
        assert!(!yaml(&config).unwrap().contains("journal"));
        for bad in [
            "67108863",
            "1099511627777",
            "0",
            "-1",
            "true",
            "'67108864'",
            "null",
            "[67108864]",
            "{quotaBytes: 67108864}",
            "!tag 67108864",
        ] {
            assert!(
                Config::default().set("journal.quotaBytes", bad).is_err(),
                "accepted {bad}"
            );
        }
        for bad in ["67108863", "1099511627777", "-1", "true", "'1'"] {
            assert_eq!(
                Config::default().set("journal.quotaBytes", bad),
                Err("config_invalid_value"),
                "{bad}"
            );
        }
        config.set("journal.quotaBytes", "67108864").unwrap();
        assert_eq!(config.get("journal.quotaBytes").unwrap(), "67108864\n");
        config.set("journal.quotaBytes", "1099511627776").unwrap();
        let text = yaml(&config).unwrap();
        assert!(
            text.contains("journal:\n  quotaBytes: 1099511627776\n"),
            "{text}"
        );
        assert!(!text.contains("retention"), "{text}");
        let again = Config::read(text.as_bytes()).unwrap();
        assert_eq!(again.journal.quota_bytes, 1_099_511_627_776);
        assert_eq!(again.retention.quota_bytes, crate::payload::DEFAULT_QUOTA);
        assert!(
            config
                .get("")
                .unwrap()
                .contains("journal:\n  quotaBytes: 1099511627776\n")
        );
        // Back at the default, the mapping is omitted again.
        config.set("journal.quotaBytes", "4294967296").unwrap();
        assert!(!yaml(&config).unwrap().contains("journal"));
        // The mapping set requires exactly its one field.
        config.set("journal", "{quotaBytes: 134217728}").unwrap();
        assert_eq!(config.get("journal").unwrap(), "quotaBytes: 134217728\n");
        for bad in [
            "{}",
            "{quotaBytes: 67108863}",
            "{quotaBytes: 1099511627777}",
            "{quotaBytes: 134217728, other: 1}",
            "{other: 1}",
            "{quotaBytes: true}",
            "{quotaBytes: null}",
            "134217728",
            "null",
        ] {
            assert!(
                Config::default().set("journal", bad).is_err(),
                "accepted {bad}"
            );
        }
        assert_eq!(
            Config::default().set("journal.other", "1"),
            Err("config_unknown_key")
        );
        // Stored documents obey the same bounds and closed schema.
        for document in [
            "journal: {quotaBytes: 67108863}",
            "journal: {quotaBytes: 1099511627777}",
            "journal: {quotaBytes: '67108864'}",
            "journal: {quotaBytes: null}",
            "journal: {quotaBytes: 67108864, quotaBytes: 67108864}",
            "journal: {other: 1}",
            "journal: null",
            "journal: 67108864",
        ] {
            assert!(
                Config::read(document.as_bytes()).is_err(),
                "accepted {document}"
            );
        }
        assert_eq!(
            Config::read(b"journal: {}").unwrap().journal.quota_bytes,
            JOURNAL_DEFAULT_QUOTA
        );
        assert_eq!(
            Config::read(b"journal: {quotaBytes: !!int 67108864}")
                .unwrap()
                .journal
                .quota_bytes,
            JOURNAL_MIN_QUOTA
        );
    }
    /// SJ-14 unit layer against a scratch runtime: `journal_quota` reads the
    /// default and set values, `execute` reports the applied quota only for a
    /// persisted journal set, and invalid stored values are never overwritten.
    #[test]
    fn journal_quota_reads_scratch_runtime_and_reports_applied_sets() {
        use std::os::unix::fs::DirBuilderExt;
        struct Scratch(std::path::PathBuf);
        impl Drop for Scratch {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let root = Scratch(
            format!(
                "/private/tmp/asura-jq-{:x}",
                u128::from_ne_bytes(asura_platform::random_id())
            )
            .into(),
        );
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&root.0)
            .unwrap();
        let runtime = RuntimeDirectory::scratch(&root.0, true).unwrap();
        let cancel = AtomicBool::new(false);
        let call = |key, value| execute(runtime.clone(), key, value, &cancel);
        assert_eq!(
            journal_quota(runtime.clone(), &cancel),
            Ok(JOURNAL_DEFAULT_QUOTA)
        );
        assert_eq!(
            call("journal.quotaBytes", None),
            Ok(Reply {
                value_yaml: "4294967296\n".into(),
                journal_quota: None
            })
        );
        let file = root.0.join(".asura/config.yaml");
        // An unrelated set persists without the default journal mapping and
        // reports no journal change.
        assert_eq!(
            call("audit.keepFiles", Some("7")).unwrap().journal_quota,
            None
        );
        assert!(!std::fs::read_to_string(&file).unwrap().contains("journal"));
        assert_eq!(
            call("journal.quotaBytes", Some("67108864")),
            Ok(Reply {
                value_yaml: "67108864\n".into(),
                journal_quota: Some(JOURNAL_MIN_QUOTA)
            })
        );
        assert!(
            std::fs::read_to_string(&file)
                .unwrap()
                .contains("journal:\n  quotaBytes: 67108864\n")
        );
        assert_eq!(
            journal_quota(runtime.clone(), &cancel),
            Ok(JOURNAL_MIN_QUOTA)
        );
        assert_eq!(
            call("journal", Some("{quotaBytes: 1099511627776}")),
            Ok(Reply {
                value_yaml: "quotaBytes: 1099511627776\n".into(),
                journal_quota: Some(JOURNAL_MAX_QUOTA)
            })
        );
        assert_eq!(
            journal_quota(runtime.clone(), &cancel),
            Ok(JOURNAL_MAX_QUOTA)
        );
        assert!(
            call("", None)
                .unwrap()
                .value_yaml
                .contains("journal:\n  quotaBytes: 1099511627776\n")
        );
        // Rejected sets leave the file and the reported quota unchanged.
        let before = std::fs::read(&file).unwrap();
        for (key, value) in [
            ("journal.quotaBytes", "67108863"),
            ("journal.quotaBytes", "1099511627777"),
            ("journal", "{other: 1}"),
            ("journal.other", "1"),
        ] {
            assert!(call(key, Some(value)).is_err(), "accepted {key}: {value}");
        }
        assert_eq!(call("journal.other", None), Err("config_unknown_key"));
        assert_eq!(std::fs::read(&file).unwrap(), before);
        // Setting the default removes the mapping from the file.
        assert_eq!(
            call("journal.quotaBytes", Some("4294967296"))
                .unwrap()
                .journal_quota,
            Some(JOURNAL_DEFAULT_QUOTA)
        );
        assert!(!std::fs::read_to_string(&file).unwrap().contains("journal"));
        // An invalid stored value is an error for every reader and is never
        // overwritten by a set.
        std::fs::write(&file, b"journal: {quotaBytes: 67108863}\n").unwrap();
        let invalid = std::fs::read(&file).unwrap();
        assert_eq!(
            journal_quota(runtime.clone(), &cancel),
            Err("config_invalid_value")
        );
        assert_eq!(
            call("journal.quotaBytes", None),
            Err("config_invalid_value")
        );
        assert_eq!(
            call("journal.quotaBytes", Some("67108864")),
            Err("config_invalid_value")
        );
        std::fs::write(&file, b"journal: {quotaBytes: 67108864, other: 1}\n").unwrap();
        let unknown = std::fs::read(&file).unwrap();
        assert_eq!(
            journal_quota(runtime.clone(), &cancel),
            Err("config_invalid_schema")
        );
        assert!(call("model", Some("replacement")).is_err());
        assert_eq!(std::fs::read(&file).unwrap(), unknown);
        assert_ne!(unknown, invalid);
        // Cancellation ends the read.
        let cancelled = AtomicBool::new(true);
        assert!(journal_quota(runtime.clone(), &cancelled).is_err());
    }
    const JOBS_KEYS: [(&str, (u32, u32, u32)); 5] = [
        ("jobs.liveService", JOBS_LIVE_SERVICE),
        ("jobs.liveAgent", JOBS_LIVE_AGENT),
        ("jobs.queuedAgent", JOBS_QUEUED_AGENT),
        ("jobs.clientViewers", JOBS_CLIENT_VIEWERS),
        (
            "jobs.modelReservedPageReads",
            JOBS_MODEL_RESERVED_PAGE_READS,
        ),
    ];
    /// J2T-21 unit layer: the `jobs` keys have their J2a-2 defaults and
    /// bounds, reject out-of-range and mistyped values with
    /// `config_invalid_value`, and are written only away from the defaults.
    #[test]
    fn j2t_21_jobs_keys_defaults_bounds_and_persistence() {
        assert_eq!(JOBS_LIVE_SERVICE, (4, 1, 16));
        assert_eq!(JOBS_LIVE_AGENT, (2, 1, 8));
        assert_eq!(JOBS_QUEUED_AGENT, (8, 1, 16));
        assert_eq!(JOBS_CLIENT_VIEWERS, (8, 1, 32));
        assert_eq!(JOBS_MODEL_RESERVED_PAGE_READS, (1, 0, 1));
        let config = Config::read(b"").unwrap();
        for (key, (default, _, _)) in JOBS_KEYS {
            assert_eq!(config.get(key).unwrap(), format!("{default}\n"), "{key}");
        }
        assert_eq!(
            config.get("jobs").unwrap(),
            "liveService: 4\nliveAgent: 2\nqueuedAgent: 8\nclientViewers: 8\nmodelReservedPageReads: 1\n"
        );
        assert!(!yaml(&config).unwrap().contains("jobs"));
        assert!(!config.get("").unwrap().contains("jobs"));
        assert_eq!(
            JobLimits::default(),
            JobLimits {
                live_service: 4,
                live_agent: 2,
                queued_agent: 8,
                client_viewers: 8,
                model_reserved_page_reads: 1,
            }
        );
        for (key, (_, min, max)) in JOBS_KEYS {
            let mut config = Config::default();
            for good in [min, max] {
                config.set(key, &good.to_string()).unwrap();
                assert_eq!(config.get(key).unwrap(), format!("{good}\n"), "{key}");
            }
            let before = yaml(&config).unwrap();
            let below = if min == 0 {
                "-1".to_owned()
            } else {
                (min - 1).to_string()
            };
            for bad in [
                below.as_str(),
                &(max + 1).to_string(),
                "-1",
                "true",
                "'1'",
                "{x: 1}",
            ] {
                assert_eq!(
                    config.set(key, bad),
                    Err("config_invalid_value"),
                    "accepted {key}: {bad}"
                );
            }
            // The strict YAML reader rejects these before typing.
            for bad in ["1.5", "null", "[1]", "!tag 1"] {
                assert!(config.set(key, bad).is_err(), "accepted {key}: {bad}");
            }
            // A rejected set is not observable through a fresh config; the
            // caller persists only after a successful set.
            assert_eq!(
                Config::read(before.as_bytes()).unwrap().get(key).unwrap(),
                format!("{max}\n")
            );
        }
        let mut config = Config::default();
        config.set("jobs.liveService", "16").unwrap();
        let text = yaml(&config).unwrap();
        assert!(text.contains("jobs:\n  liveService: 16\n"), "{text}");
        let again = Config::read(text.as_bytes()).unwrap();
        assert_eq!(again.jobs.live_service, 16);
        assert_eq!(again.jobs.live_agent, 2);
        // Setting the default again omits the mapping: there is no deletion
        // syntax, so this is the reset.
        config.set("jobs.liveService", "4").unwrap();
        assert!(!yaml(&config).unwrap().contains("jobs"));
        assert_eq!(
            Config::default().set("jobs.other", "1"),
            Err("config_unknown_key")
        );
        assert_eq!(
            Config::default().get("jobs.other"),
            Err("config_unknown_key")
        );
    }
    /// J2T-21 unit layer: `set jobs` takes the whole mapping, and stored
    /// documents obey the same bounds and closed schema.
    #[test]
    fn j2t_21_jobs_mapping_set_requires_every_field() {
        let mut config = Config::default();
        config
            .set(
                "jobs",
                "{liveService: 16, liveAgent: 8, queuedAgent: 16, clientViewers: 32, modelReservedPageReads: 0}",
            )
            .unwrap();
        assert_eq!(
            config.get("jobs").unwrap(),
            "liveService: 16\nliveAgent: 8\nqueuedAgent: 16\nclientViewers: 32\nmodelReservedPageReads: 0\n"
        );
        assert_eq!(config.get("jobs.modelReservedPageReads").unwrap(), "0\n");
        for bad in [
            "{}",
            "{liveService: 4}",
            "{liveService: 4, liveAgent: 2, queuedAgent: 8, clientViewers: 8}",
            "{liveService: 4, liveAgent: 2, queuedAgent: 8, clientViewers: 8, modelReservedPageReads: 1, other: 1}",
            "{liveService: 0, liveAgent: 2, queuedAgent: 8, clientViewers: 8, modelReservedPageReads: 1}",
            "{liveService: 4, liveAgent: 9, queuedAgent: 8, clientViewers: 8, modelReservedPageReads: 1}",
            "{liveService: 4, liveAgent: 2, queuedAgent: 17, clientViewers: 8, modelReservedPageReads: 1}",
            "{liveService: 4, liveAgent: 2, queuedAgent: 8, clientViewers: 33, modelReservedPageReads: 1}",
            "{liveService: 4, liveAgent: 2, queuedAgent: 8, clientViewers: 8, modelReservedPageReads: 2}",
            "{liveService: true, liveAgent: 2, queuedAgent: 8, clientViewers: 8, modelReservedPageReads: 1}",
            "{liveService: '4', liveAgent: 2, queuedAgent: 8, clientViewers: 8, modelReservedPageReads: 1}",
            "4",
        ] {
            let mut copy = Config::default();
            assert_eq!(
                copy.set("jobs", bad),
                Err("config_invalid_value"),
                "accepted {bad}"
            );
        }
        for bad in [
            "{liveService: null, liveAgent: 2, queuedAgent: 8, clientViewers: 8, modelReservedPageReads: 1}",
            "null",
        ] {
            assert!(
                Config::default().set("jobs", bad).is_err(),
                "accepted {bad}"
            );
        }
        for document in [
            "jobs: {liveService: 0}",
            "jobs: {liveService: 17}",
            "jobs: {liveAgent: 0}",
            "jobs: {liveAgent: 9}",
            "jobs: {queuedAgent: 17}",
            "jobs: {clientViewers: 0}",
            "jobs: {modelReservedPageReads: 2}",
            "jobs: {liveService: '4'}",
            "jobs: {liveService: null}",
            "jobs: {liveService: 4, liveService: 4}",
            "jobs: {other: 1}",
            "jobs: null",
            "jobs: 4",
        ] {
            assert!(
                Config::read(document.as_bytes()).is_err(),
                "accepted {document}"
            );
        }
        // Absent fields inherit their defaults.
        let partial = Config::read(b"jobs: {liveAgent: 1}").unwrap();
        assert_eq!(partial.jobs.live_service, 4);
        assert_eq!(partial.jobs.live_agent, 1);
        assert!(Config::read(b"jobs: {}").unwrap().jobs.is_default());
    }
    /// J2T-21 unit layer: the effective per-agent limit is the smaller of
    /// `jobs.liveAgent` and `jobs.liveService`; `get` still reports the
    /// configured value.
    #[test]
    fn j2t_21_effective_live_agent_is_the_smaller_value() {
        let mut config = Config::default();
        config.set("jobs.liveService", "1").unwrap();
        config.set("jobs.liveAgent", "8").unwrap();
        assert_eq!(config.get("jobs.liveAgent").unwrap(), "8\n");
        let limits = config.jobs.limits();
        assert_eq!(limits.live_service, 1);
        assert_eq!(limits.live_agent, 1);
        config.set("jobs.liveService", "16").unwrap();
        assert_eq!(config.jobs.limits().live_agent, 8);
        config.set("jobs.liveAgent", "3").unwrap();
        config.set("jobs.liveService", "3").unwrap();
        assert_eq!(config.jobs.limits().live_agent, 3);
    }
    /// J2T-21 unit layer against a scratch runtime: `execute` gets and sets
    /// the keys and reports no applied value (they apply at service start);
    /// `job_limits` reads the persisted effective limits; invalid stored
    /// values fail every reader and are never overwritten.
    #[test]
    fn j2t_21_job_limits_read_scratch_runtime() {
        use std::os::unix::fs::DirBuilderExt;
        struct Scratch(std::path::PathBuf);
        impl Drop for Scratch {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let root = Scratch(
            format!(
                "/private/tmp/asura-jobs-{:x}",
                u128::from_ne_bytes(asura_platform::random_id())
            )
            .into(),
        );
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&root.0)
            .unwrap();
        let runtime = RuntimeDirectory::scratch(&root.0, true).unwrap();
        let cancel = AtomicBool::new(false);
        let call = |key, value| execute(runtime.clone(), key, value, &cancel);
        assert_eq!(
            job_limits(runtime.clone(), &cancel),
            Ok(JobLimits::default())
        );
        let file = root.0.join(".asura/config.yaml");
        assert_eq!(
            call("jobs.liveService", Some("2")),
            Ok(Reply {
                value_yaml: "2\n".into(),
                journal_quota: None
            })
        );
        assert!(
            std::fs::read_to_string(&file)
                .unwrap()
                .contains("jobs:\n  liveService: 2\n")
        );
        assert_eq!(
            job_limits(runtime.clone(), &cancel).unwrap(),
            JobLimits {
                live_service: 2,
                live_agent: 2,
                ..JobLimits::default()
            }
        );
        call(
            "jobs",
            Some("{liveService: 3, liveAgent: 5, queuedAgent: 1, clientViewers: 32, modelReservedPageReads: 0}"),
        )
        .unwrap();
        assert_eq!(
            job_limits(runtime.clone(), &cancel).unwrap(),
            JobLimits {
                live_service: 3,
                live_agent: 3,
                queued_agent: 1,
                client_viewers: 32,
                model_reserved_page_reads: 0,
            }
        );
        assert_eq!(call("jobs.liveAgent", None).unwrap().value_yaml, "5\n");
        assert!(call("", None).unwrap().value_yaml.contains("jobs:\n"));
        let before = std::fs::read(&file).unwrap();
        for (key, value) in [
            ("jobs.liveService", "17"),
            ("jobs.modelReservedPageReads", "2"),
            ("jobs", "{liveService: 4}"),
            ("jobs.other", "1"),
        ] {
            assert!(call(key, Some(value)).is_err(), "accepted {key}: {value}");
        }
        assert_eq!(std::fs::read(&file).unwrap(), before);
        // Setting every default removes the mapping from the file.
        call(
            "jobs",
            Some("{liveService: 4, liveAgent: 2, queuedAgent: 8, clientViewers: 8, modelReservedPageReads: 1}"),
        )
        .unwrap();
        assert!(!std::fs::read_to_string(&file).unwrap().contains("jobs"));
        std::fs::write(&file, b"jobs: {liveService: 0}\n").unwrap();
        let invalid = std::fs::read(&file).unwrap();
        assert_eq!(
            job_limits(runtime.clone(), &cancel),
            Err("config_invalid_value")
        );
        assert_eq!(call("jobs", None), Err("config_invalid_value"));
        assert!(call("jobs.liveService", Some("4")).is_err());
        assert_eq!(std::fs::read(&file).unwrap(), invalid);
        let cancelled = AtomicBool::new(true);
        assert!(job_limits(runtime.clone(), &cancelled).is_err());
    }
}
