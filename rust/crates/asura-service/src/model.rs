//! Model identifiers and declared provider capabilities owned by the service.
//!
//! Resolution performs no IO and grants no inference authority. Runtime availability
//! remains unknown until a verified helper reports it under an admitted operation.

use std::{error::Error, fmt, str::FromStr};

const MAX_IDENTIFIER_BYTES: usize = 1024;

/// A syntactically valid selection. Configuration storage has a separate grammar.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelIdentifier {
    provider: String,
    name: Option<String>,
}

/// A rejected identifier; parsing never resolves a provider or reads assets.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IdentifierError {
    Empty,
    TooLong,
    WhitespaceOrControl,
    MissingProviderSeparator,
    EmptyProvider,
    EmptyName,
}

impl fmt::Display for IdentifierError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Empty => "model identifier is empty",
            Self::TooLong => "model identifier exceeds 1024 bytes",
            Self::WhitespaceOrControl => {
                "model identifier contains whitespace or a control character"
            }
            Self::MissingProviderSeparator => "use system or provider:name",
            Self::EmptyProvider => "model provider is empty",
            Self::EmptyName => "model name is empty",
        })
    }
}

impl Error for IdentifierError {}

impl FromStr for ModelIdentifier {
    type Err = IdentifierError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.is_empty() {
            return Err(IdentifierError::Empty);
        }
        if value.len() > MAX_IDENTIFIER_BYTES {
            return Err(IdentifierError::TooLong);
        }
        if value.chars().any(|c| c.is_whitespace() || c.is_control()) {
            return Err(IdentifierError::WhitespaceOrControl);
        }
        if value == "system" {
            return Ok(Self {
                provider: value.into(),
                name: None,
            });
        }
        let (provider, name) = value
            .split_once(':')
            .ok_or(IdentifierError::MissingProviderSeparator)?;
        if provider.is_empty() {
            return Err(IdentifierError::EmptyProvider);
        }
        if name.is_empty() {
            return Err(IdentifierError::EmptyName);
        }
        let provider = provider.to_lowercase();
        // Unicode case conversion can expand the encoded identifier.
        if provider.len() + 1 + name.len() > MAX_IDENTIFIER_BYTES {
            return Err(IdentifierError::TooLong);
        }
        Ok(Self {
            provider,
            name: Some(name.into()),
        })
    }
}

impl ModelIdentifier {
    pub fn provider(&self) -> &str {
        &self.provider
    }

    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }
}

impl fmt::Display for ModelIdentifier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.provider)?;
        if let Some(name) = &self.name {
            write!(f, ":{name}")?;
        }
        Ok(())
    }
}

/// Required operations, checked against declarations before runtime admission.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Capability {
    Text,
    ToolCalling,
    StructuredOutput,
}

/// Runtime support is independent of the capabilities enabled for this turn.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct CapabilityProfile {
    pub supported: Option<u32>,
    pub source: Option<u32>,
    pub reasoning_disabled: bool,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Activation {
    Enabled,
    Disabled,
    ProviderControlled,
    Unknown,
}
impl CapabilityProfile {
    pub fn activation(self, bit: u32, tools_enabled: bool) -> Activation {
        if bit == 1 {
            return if tools_enabled {
                Activation::Enabled
            } else {
                Activation::Disabled
            };
        }
        if self.reasoning_disabled && bit == 4 {
            return Activation::Disabled;
        }
        if bit != 4 {
            return Activation::Disabled;
        }
        match self.supported {
            Some(mask) if mask & bit != 0 => Activation::ProviderControlled,
            Some(_) => Activation::Disabled,
            None => Activation::Unknown,
        }
    }
}

/// Static capability declarations are not live availability evidence.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Availability {
    Unknown,
}

#[derive(Debug, Eq, PartialEq)]
pub struct Provider {
    name: &'static str,
    capabilities: &'static [Capability],
    adapter: AdapterState,
    destination: DestinationKind,
    asset_directory: Option<&'static str>,
}

impl Provider {
    pub fn name(&self) -> &'static str {
        self.name
    }

    pub fn adapter_state(&self) -> AdapterState {
        self.adapter
    }

    pub fn destination_kind(&self) -> DestinationKind {
        self.destination
    }

    /// Managed-home relative path; inspecting this metadata performs no IO.
    pub fn asset_directory(&self) -> Option<&'static str> {
        self.asset_directory
    }

    pub fn capabilities(&self) -> &'static [Capability] {
        self.capabilities
    }
}

/// Installed adapter state is separate from model/runtime availability.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AdapterState {
    Connected,
    PendingIntegration,
    NotBuilt,
}

/// Provider identity alone cannot establish the destination's locality.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DestinationKind {
    OnDevice,
    EndpointDependent,
}

static PROVIDERS: [Provider; 4] = [
    Provider {
        name: "system",
        capabilities: &[Capability::Text],
        adapter: AdapterState::Connected,
        destination: DestinationKind::OnDevice,
        asset_directory: None,
    },
    Provider {
        name: "ollama",
        capabilities: &[Capability::Text],
        adapter: AdapterState::Connected,
        destination: DestinationKind::EndpointDependent,
        asset_directory: None,
    },
    Provider {
        name: "coreai",
        capabilities: &[Capability::Text],
        adapter: AdapterState::Connected,
        destination: DestinationKind::OnDevice,
        asset_directory: Some("data/models/coreai"),
    },
    Provider {
        name: "mlx",
        capabilities: &[Capability::Text],
        adapter: AdapterState::Connected,
        destination: DestinationKind::OnDevice,
        asset_directory: Some("data/models/mlx"),
    },
];

/// Offer tools to adapters with a local route; Hello must establish permission.
/// Native tool capability is negotiated separately with the verified helper.
/// Endpoint-dependent adapters need a destination grant, even on loopback.
pub(crate) fn project_tools_allowed(selector: &str) -> bool {
    selector
        .parse::<ModelIdentifier>()
        .ok()
        .and_then(|identifier| resolve(identifier, &[Capability::Text]).ok())
        .is_some_and(|selection| {
            selection.provider().destination_kind() == DestinationKind::OnDevice
                || selection.provider().name() == "ollama"
        })
}

/// The canonical registry. No dynamic registration or implicit fallback exists.
pub fn providers() -> &'static [Provider] {
    &PROVIDERS
}

/// Selection succeeded, but a verified helper and service admission are still required.
#[derive(Debug, Eq, PartialEq)]
pub struct Selection {
    identifier: ModelIdentifier,
    provider: &'static Provider,
}

impl Selection {
    pub fn identifier(&self) -> &ModelIdentifier {
        &self.identifier
    }

    pub fn provider(&self) -> &'static Provider {
        self.provider
    }

    pub fn availability(&self) -> Availability {
        Availability::Unknown
    }
}

/// Expected resolution failures; none triggers another provider automatically.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Unavailable {
    UnknownProvider(String),
    AdapterNotReady(String, AdapterState),
    UnsupportedModel(ModelIdentifier),
    UnsupportedCapability(Capability),
}

impl fmt::Display for Unavailable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownProvider(provider) => {
                write!(f, "model provider '{provider}' is unavailable")
            }
            Self::AdapterNotReady(provider, state) => {
                write!(f, "model provider '{provider}' adapter is {state:?}")
            }
            Self::UnsupportedModel(model) => write!(f, "model '{model}' is unavailable"),
            Self::UnsupportedCapability(capability) => {
                write!(f, "model capability {capability:?} is unavailable")
            }
        }
    }
}

impl Error for Unavailable {}

/// Match declarations only. This does not load, contact or authorize a model.
pub fn resolve(
    identifier: ModelIdentifier,
    required: &[Capability],
) -> Result<Selection, Unavailable> {
    let provider = providers()
        .iter()
        .find(|provider| provider.name == identifier.provider)
        .ok_or_else(|| Unavailable::UnknownProvider(identifier.provider.clone()))?;
    if provider.adapter != AdapterState::Connected {
        return Err(Unavailable::AdapterNotReady(
            identifier.provider.clone(),
            provider.adapter,
        ));
    }
    // The first adapter selects the system default, not arbitrary system aliases.
    if identifier.provider == "system" && identifier.name.is_some() {
        return Err(Unavailable::UnsupportedModel(identifier));
    }
    for capability in required {
        if !provider.capabilities.contains(capability) {
            return Err(Unavailable::UnsupportedCapability(*capability));
        }
    }
    Ok(Selection {
        identifier,
        provider,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn project_tool_offer_is_limited_to_registered_adapters() {
        for selector in [
            "system",
            "coreai:fixture",
            "mlx:fixture",
            "MLX:fixture",
            "ollama:fixture",
        ] {
            assert!(project_tools_allowed(selector), "{selector}");
        }
        for selector in ["unknown:fixture", "system:other", "", "mlx:"] {
            assert!(!project_tools_allowed(selector), "{selector}");
        }
    }

    #[test]
    fn tagged_names_preserve_case_and_all_remaining_colons() {
        let id: ModelIdentifier = "OLLAMA:Granite4.1:8B".parse().unwrap();
        assert_eq!(id.provider(), "ollama");
        assert_eq!(id.name(), Some("Granite4.1:8B"));
        assert_eq!(id.to_string(), "ollama:Granite4.1:8B");
        assert_eq!(id.to_string().parse::<ModelIdentifier>().unwrap(), id);
    }

    #[test]
    fn invalid_components_do_not_become_defaults() {
        for (input, expected) in [
            ("", IdentifierError::Empty),
            ("system ", IdentifierError::WhitespaceOrControl),
            ("system\n", IdentifierError::WhitespaceOrControl),
            (
                "provider:na\u{00a0}me",
                IdentifierError::WhitespaceOrControl,
            ),
            ("provider:na\0me", IdentifierError::WhitespaceOrControl),
            ("provider", IdentifierError::MissingProviderSeparator),
            ("SYSTEM", IdentifierError::MissingProviderSeparator),
            (":model", IdentifierError::EmptyProvider),
            ("provider:", IdentifierError::EmptyName),
        ] {
            assert_eq!(input.parse::<ModelIdentifier>(), Err(expected), "{input:?}");
        }
    }

    #[test]
    fn identifier_bytes_are_bounded() {
        let largest = format!("p:{}", "a".repeat(1022));
        assert!(largest.parse::<ModelIdentifier>().is_ok());
        assert_eq!(
            format!("{largest}a").parse::<ModelIdentifier>(),
            Err(IdentifierError::TooLong)
        );
        assert_eq!(
            format!("p:{}", "é".repeat(512)).parse::<ModelIdentifier>(),
            Err(IdentifierError::TooLong)
        );
    }

    #[test]
    fn unknown_provider_never_falls_back_to_system() {
        let id = "unknown:granite4.1:8b".parse().unwrap();
        assert_eq!(
            resolve(id, &[Capability::Text]),
            Err(Unavailable::UnknownProvider("unknown".into()))
        );
        let id = "system:other".parse().unwrap();
        assert_eq!(
            resolve(id, &[Capability::Text]),
            Err(Unavailable::UnsupportedModel(
                "system:other".parse().unwrap()
            ))
        );
    }

    #[test]
    fn system_declares_only_text_and_never_claims_runtime_readiness() {
        assert_eq!(providers().len(), 4);
        let selection = resolve("system".parse().unwrap(), &[Capability::Text]).unwrap();
        assert_eq!(selection.identifier().to_string(), "system");
        assert_eq!(selection.provider().name(), "system");
        assert_eq!(selection.provider().capabilities(), &[Capability::Text]);
        assert_eq!(selection.availability(), Availability::Unknown);
        for capability in [Capability::ToolCalling, Capability::StructuredOutput] {
            assert_eq!(
                resolve("system".parse().unwrap(), &[Capability::Text, capability]),
                Err(Unavailable::UnsupportedCapability(capability))
            );
        }
    }
    #[test]
    fn inventory_is_not_admission_and_does_not_assume_ollama_is_local() {
        let inventory = providers();
        assert_eq!(inventory.len(), 4);
        let ollama = inventory.iter().find(|p| p.name == "ollama").unwrap();
        assert_eq!(ollama.destination, DestinationKind::EndpointDependent);
        assert_eq!(ollama.adapter, AdapterState::Connected);
        for name in ["ollama", "coreai", "mlx"] {
            let selection = resolve(
                format!("{name}:example").parse().unwrap(),
                &[Capability::Text],
            )
            .unwrap();
            assert_eq!(selection.availability(), Availability::Unknown);
            assert_eq!(selection.provider().capabilities(), &[Capability::Text]);
        }
        assert_eq!(
            inventory
                .iter()
                .find(|p| p.name == "coreai")
                .unwrap()
                .asset_directory,
            Some("data/models/coreai")
        );
        assert_eq!(
            inventory
                .iter()
                .find(|p| p.name == "mlx")
                .unwrap()
                .asset_directory,
            Some("data/models/mlx")
        );
    }
}
