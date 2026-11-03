//! Asura's effect policy (security-policy.md): the Cedar schema
//! `asura-policy/1`, the built-in installation and default user sets, the
//! deny-on-any-error wrapper with per-scope evaluation (pattern A), and the
//! `model-1` region compiler.
//!
//! The crate is pure and synchronous: no I/O, no clock, no threads. Each
//! policy worker builds its own [`Engine`]; an engine is never shared
//! between threads. The service owner (`asura-service::policy`) supplies
//! every trusted attribute and the time.
#![forbid(unsafe_code)]

mod compile;
mod engine;
mod request;
mod text;

#[cfg(test)]
mod tests;

pub use compile::{
    JobOutcome, ROOTS, Regions, Root, RootSet, compile_model1, decide_model1_job, model1_principal,
};
pub use engine::{
    Engine, LoadError, PRINCIPAL_CACHE_BYTES, PRINCIPAL_CACHE_ENTRIES, PrincipalCacheStats,
};
pub use request::{
    Action, ActionGroup, Context, Decision, ErrorClass, Intent, IntentSource, JobPrincipal,
    MAX_ANCESTORS, MAX_ENTITIES, MAX_REQUEST_BYTES, MAX_SET_ELEMENTS, MAX_STRING_BYTES, Origin,
    PathClass, PathRef, Primary, ProfileKind, ProgramRef, Request, Resource, ScopeResult,
};
pub use text::{
    MAX_POLICIES_PER_SCOPE, MAX_SCOPE_TEXT_BYTES, RevisionTexts, Scope, ScopeText, validate_text,
};

use sha2::{Digest, Sha256};

/// The schema identifier; it stays `1` before release 1.0.0.
pub const SCHEMA_ID: &str = "asura-policy/1";
/// The pinned Cedar SDK version (`cedar-policy =4.14.1`).
pub const CEDAR_SDK: &str = "4.14.1";

const SCHEMA_TEXT: &str = include_str!("../schema/asura.cedarschema");

/// The schema text, a complete schema with no `external` declarations.
pub fn schema_text() -> &'static str {
    SCHEMA_TEXT
}
/// SHA-256 of the schema text.
pub fn schema_digest() -> [u8; 32] {
    sha256(SCHEMA_TEXT.as_bytes())
}
/// The Cedar language version of the linked SDK.
pub fn cedar_lang() -> String {
    cedar_policy::get_lang_version().to_string()
}

pub(crate) fn sha256(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}
/// Lowercase hex of a digest.
pub fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push_str(&format!("{b:02x}"));
    }
    out
}
