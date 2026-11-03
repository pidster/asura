//! Policy engine spike: Cedar and Rego on one Asura corpus.
//! Governing design: docs/designs/policy-engine-spike.md.

#[cfg(feature = "admission")]
pub mod admission;
pub mod cedar_engine;
pub mod corpus;
pub mod decision;
pub mod model;
pub mod rego_engine;
pub mod stats;
pub mod store;

use cedar_engine::CedarEngine;
use corpus::Scale;
use rego_engine::RegoEngine;

/// Build both engines over the whole corpus (core plus scale).
pub fn engines(scale: &Scale) -> (CedarEngine, RegoEngine) {
    let cedar = CedarEngine::load(
        &corpus::schema_text(),
        &corpus::all_cedar(scale),
        &scale.store,
    )
    .unwrap_or_else(|e| panic!("Cedar corpus refused: {e:#?}"));
    let rego = RegoEngine::load(
        &corpus::rego_lib(),
        &corpus::rego_decision(),
        &corpus::all_rego(scale),
        &scale.store,
    )
    .unwrap_or_else(|e| panic!("Rego corpus refused: {e:#?}"));
    (cedar, rego)
}

/// Every request of the corpus: conformance first, then scale.
pub fn all_requests(scale: &Scale) -> Vec<model::Request> {
    let mut v: Vec<model::Request> = corpus::conformance_requests()
        .into_iter()
        .map(|(_, r)| r)
        .collect();
    v.extend(scale.requests.iter().cloned());
    v
}
