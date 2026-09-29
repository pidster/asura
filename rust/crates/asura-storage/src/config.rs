//! YAML configuration adapter. Invoke only from the supervised service config worker.
use asura_platform::{PrivateFileError, PrivateFileSnapshot, RuntimeDirectory};
use serde::de::{self, DeserializeSeed, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Serialize};
use serde_yaml_ng::{Mapping, Value};
use std::collections::BTreeMap;
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

type Result<T> = std::result::Result<T, &'static str>;

fn file_error(error: PrivateFileError) -> &'static str {
    match error {
        PrivateFileError::UnsafeOrIo => "config_io_or_unsafe_file",
        PrivateFileError::CancelledOrExpired => "config_cancelled_or_expired",
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

fn yaml(value: &impl Serialize) -> Result<String> {
    serde_yaml_ng::to_string(value).map_err(|_| "config_encode_failed")
}

pub fn execute(
    runtime: RuntimeDirectory,
    key: &str,
    value: Option<&str>,
    deadline: Instant,
    cancel: &AtomicBool,
) -> Result<String> {
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
        )
    {
        return Err("config_unknown_key");
    }
    let snapshot =
        PrivateFileSnapshot::read(runtime, "config.yaml", deadline, cancel).map_err(file_error)?;
    let mut config = Config::read(&snapshot.bytes)?;
    if let Some(value) = value {
        config.set(key, value)?;
        let serialized = yaml(&config)?;
        snapshot
            .replace(serialized.as_bytes(), deadline, cancel)
            .map_err(file_error)?;
    } else if cancel.load(Ordering::Acquire) || Instant::now() >= deadline {
        return Err("config_cancelled_or_expired");
    }
    config.get(key)
}

/// Startup-only typed audit settings from the canonical validated YAML snapshot.
pub fn audit_settings(
    runtime: RuntimeDirectory,
    deadline: Instant,
    cancel: &AtomicBool,
) -> Result<(crate::audit::Settings, crate::audit::ConfigLoad)> {
    let snapshot =
        PrivateFileSnapshot::read(runtime, "config.yaml", deadline, cancel).map_err(file_error)?;
    let config = Config::read(&snapshot.bytes)?;
    if cancel.load(Ordering::Acquire) || Instant::now() >= deadline {
        return Err("config_cancelled_or_expired");
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
    pub digest: [u8; 32],
}

/// Validated canonical configuration identity for durable admission.
pub fn conversation_snapshot(
    runtime: RuntimeDirectory,
    deadline: Instant,
    cancel: &AtomicBool,
) -> Result<ConversationSnapshot> {
    use sha2::{Digest, Sha256};
    let asset_root = runtime
        .model_assets_path()
        .into_os_string()
        .into_string()
        .map_err(|_| "config_io_or_unsafe_file")?;
    let snapshot =
        PrivateFileSnapshot::read(runtime, "config.yaml", deadline, cancel).map_err(file_error)?;
    let config = Config::read(&snapshot.bytes)?;
    let digest = Sha256::digest(yaml(&config)?.as_bytes()).into();
    let model_capabilities = config.model_capabilities();
    Ok(ConversationSnapshot {
        asset_root,
        model_capabilities,
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
        let deadline = Instant::now() + std::time::Duration::from_secs(10);
        let call = |key, value| execute(runtime.clone(), key, value, deadline, &cancel);
        assert_eq!(call("audit.enabled", None).unwrap(), "true\n");
        call("model", Some("provider:model")).unwrap();
        call("audit.keepFiles", Some("8")).unwrap();
        assert_eq!(call("audit.keepFiles", None).unwrap(), "8\n");
        assert_eq!(call("model", None).unwrap(), "provider:model\n");
        let original = conversation_snapshot(runtime.clone(), deadline, &cancel).unwrap();
        call(
            "providers.ollama.endpoint",
            Some("https://models.example:8443"),
        )
        .unwrap();
        let changed = conversation_snapshot(runtime.clone(), deadline, &cancel).unwrap();
        assert_eq!(changed.model, original.model);
        assert_eq!(
            changed.asset_root,
            root.0.join(".asura/data/models").to_str().unwrap()
        );
        assert_eq!(
            changed.ollama_endpoint.as_deref(),
            Some("https://models.example:8443")
        );
        assert_ne!(changed.digest, original.digest);
        assert_eq!(
            call("providers.ollama.endpoint", None).unwrap(),
            "https://models.example:8443\n"
        );
        assert!(
            call("", None)
                .unwrap()
                .contains("providers:\n  ollama:\n    endpoint:")
        );
        assert_eq!(changed.model_capabilities, None);
        call("providers.mlx.models", Some("{'family/model.v1': {capabilities: [toolCalling, reasoning]}, disabled: {capabilities: []}}")).unwrap();
        call("model", Some("mlx:family/model.v1")).unwrap();
        let enabled = conversation_snapshot(runtime.clone(), deadline, &cancel).unwrap();
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
            conversation_snapshot(runtime.clone(), deadline, &cancel)
                .unwrap()
                .model_capabilities,
            Some(0)
        );
        call("model", Some("coreai:family/model.v1")).unwrap();
        assert_eq!(
            conversation_snapshot(runtime.clone(), deadline, &cancel)
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
}
