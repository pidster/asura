//! PES-M9 Cedar probe: the baseline plus the Cedar wrapper.

use asura_policy_engine_spike::cedar_engine::{CedarEngine, Pattern};
use asura_policy_engine_spike::{all_requests, corpus};

fn main() {
    let scale = corpus::scale();
    let data = scale.store.to_rego_data();
    let mut bytes = data.to_string().len();
    let engine = CedarEngine::load(
        &corpus::schema_text(),
        &corpus::all_cedar(&scale),
        &scale.store,
    )
    .expect("Cedar loads");
    for r in all_requests(&scale) {
        bytes += r.to_json().to_string().len();
        bytes += usize::from(engine.decide(&r, Pattern::B).allow);
    }
    println!("{bytes}");
}
