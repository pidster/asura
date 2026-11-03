//! Scope texts, the revision digest and the text checks that PL1 applies
//! when an engine is built (security-policy.md, bundle format and wrapper
//! rules A1 and A2).
use crate::{CEDAR_SDK, cedar_lang, schema_digest, sha256};
use cedar_policy::{PolicySet, Schema, ValidationMode, Validator};
use std::collections::BTreeSet;
use std::str::FromStr;

/// One source of policy. The order of the variants is the evaluation and
/// explanation order of pattern A.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Scope {
    Installation = 1,
    User = 2,
    Workspace = 3,
    Repository = 4,
}
impl Scope {
    pub const ALL: [Scope; 4] = [
        Scope::Installation,
        Scope::User,
        Scope::Workspace,
        Scope::Repository,
    ];
    pub fn code(self) -> u8 {
        self as u8
    }
    pub fn from_code(code: u8) -> Option<Self> {
        Self::ALL.into_iter().find(|s| s.code() == code)
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Scope::Installation => "installation",
            Scope::User => "user",
            Scope::Workspace => "workspace",
            Scope::Repository => "repository",
        }
    }
}

/// One scope's admitted text. `source` is the source identity (`builtin`
/// in PL1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScopeText {
    pub scope: Scope,
    pub source: &'static str,
    pub text: String,
}

/// The texts of one policy revision, at most one per scope.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RevisionTexts {
    pub scopes: Vec<ScopeText>,
}

const INSTALLATION: &str = include_str!("../policies/installation.cedar");
const USER_DEFAULT: &str = include_str!("../policies/user-default.cedar");
#[cfg(feature = "test-support")]
const TEST_DENY: &str = include_str!("../policies/installation-test-deny.cedar");

impl RevisionTexts {
    /// The shipped installation set and the default user set.
    pub fn builtin() -> Self {
        Self {
            scopes: vec![
                ScopeText {
                    scope: Scope::Installation,
                    source: "builtin",
                    text: INSTALLATION.to_owned(),
                },
                ScopeText {
                    scope: Scope::User,
                    source: "builtin",
                    text: USER_DEFAULT.to_owned(),
                },
            ],
        }
    }
    /// The built-in sets with the test-only forbid of model `process.exec`
    /// added to the installation scope (PL-I3, PL-E2).
    #[cfg(feature = "test-support")]
    pub fn builtin_test_deny() -> Self {
        let mut texts = Self::builtin();
        texts.scopes[0].text = format!("{INSTALLATION}\n{TEST_DENY}");
        texts.scopes[0].source = "builtin-test-deny";
        texts
    }
    /// SHA-256 of one scope's text.
    pub fn text_digest(&self, scope: Scope) -> Option<[u8; 32]> {
        self.scopes
            .iter()
            .find(|s| s.scope == scope)
            .map(|s| sha256(s.text.as_bytes()))
    }
    /// The revision digest: SHA-256 over a fixed tag, the schema digest, the
    /// Cedar language and SDK versions and each scope's code and text
    /// digest, in scope order.
    pub fn digest(&self) -> [u8; 32] {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"asura-policy-revision/1\0");
        bytes.extend_from_slice(&schema_digest());
        bytes.extend_from_slice(cedar_lang().as_bytes());
        bytes.push(0);
        bytes.extend_from_slice(CEDAR_SDK.as_bytes());
        bytes.push(0);
        let mut scopes: Vec<&ScopeText> = self.scopes.iter().collect();
        scopes.sort_by_key(|s| s.scope);
        for s in scopes {
            bytes.push(s.scope.code());
            bytes.extend_from_slice(&sha256(s.text.as_bytes()));
        }
        sha256(&bytes)
    }
}

/// Bounds of admission step 1.
pub const MAX_SCOPE_TEXT_BYTES: usize = 256 * 1024;
pub const MAX_POLICIES_PER_SCOPE: usize = 512;
const RESERVED: [&str; 5] = ["scope", "hard", "grant", "approval", "envelope"];

fn id_ok(id: &str) -> bool {
    (1..=64).contains(&id.len())
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"_.-".contains(&b))
}
pub(crate) fn reason_ok(reason: &str) -> bool {
    (1..=48).contains(&reason.len())
        && reason
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}
fn explain_ok(text: &str) -> bool {
    text.len() <= 200 && text.bytes().all(|b| (0x20..0x7f).contains(&b))
}

/// Parse, annotate and strictly validate one scope text (bounds,
/// annotations, A1 strict validation, A2 warnings refuse). Returns every
/// problem found.
pub fn validate_text(scope: Scope, text: &str) -> Result<(), Vec<String>> {
    let schema = crate::engine::parse_schema().map_err(|e| vec![e])?;
    check_text(&schema, scope, text).map(|_| ())
}

pub(crate) fn check_text(
    schema: &Schema,
    scope: Scope,
    text: &str,
) -> Result<PolicySet, Vec<String>> {
    let name = scope.as_str();
    if text.len() > MAX_SCOPE_TEXT_BYTES {
        return Err(vec![format!("{name}: candidate_too_large")]);
    }
    let set = PolicySet::from_str(text).map_err(|e| vec![format!("{name}: {e}")])?;
    let mut errors = Vec::new();
    if set.templates().next().is_some() {
        errors.push(format!("{name}: templates are not allowed"));
    }
    if set.policies().count() > MAX_POLICIES_PER_SCOPE {
        errors.push(format!("{name}: candidate_too_large"));
    }
    let mut seen = BTreeSet::new();
    for policy in set.policies() {
        let label = policy.annotation("id").unwrap_or("<no id>").to_owned();
        match policy.annotation("id") {
            Some(id) if id_ok(id) => {
                if !seen.insert(id.to_owned()) {
                    errors.push(format!("{name}: duplicate @id {id}"));
                }
            }
            _ => errors.push(format!("{name}: {label}: @id missing or malformed")),
        }
        match policy.annotation("reason") {
            Some(reason) if reason_ok(reason) => {}
            _ => errors.push(format!("{name}: {label}: @reason missing or malformed")),
        }
        if let Some(explain) = policy.annotation("explain")
            && !explain_ok(explain)
        {
            errors.push(format!("{name}: {label}: @explain malformed"));
        }
        for (key, value) in policy.annotations() {
            let key = key.to_string();
            match key.as_str() {
                "id" | "reason" | "explain" => {}
                "intent" | "classifier" if value == "true" => {}
                "hard" if scope == Scope::Installation && value == "true" => {}
                k if RESERVED.contains(&k) => {
                    errors.push(format!("{name}: {label}: reserved annotation @{k}"))
                }
                k => errors.push(format!("{name}: {label}: unknown annotation @{k}")),
            }
        }
    }
    let result = Validator::new(schema.clone()).validate(&set, ValidationMode::Strict);
    errors.extend(
        result
            .validation_errors()
            .map(|e| format!("{name}: invalid: {e}")),
    );
    errors.extend(
        result
            .validation_warnings()
            .map(|w| format!("{name}: warning: {w}")),
    );
    if errors.is_empty() {
        Ok(set)
    } else {
        Err(errors)
    }
}
