//! PES-M9 Rego probe: the baseline plus the Rego wrapper.

use asura_policy_engine_spike::rego_engine::RegoEngine;
use asura_policy_engine_spike::{all_requests, corpus};

fn main() {
    let scale = corpus::scale();
    let data = scale.store.to_rego_data();
    let mut bytes = data.to_string().len();
    let mut engine = RegoEngine::load(
        &corpus::rego_lib(),
        &corpus::rego_decision(),
        &corpus::all_rego(&scale),
        &scale.store,
    )
    .expect("Rego loads");
    for r in all_requests(&scale) {
        bytes += r.to_json().to_string().len();
        bytes += usize::from(engine.decide(&r).allow);
    }
    println!("{bytes}");
}
