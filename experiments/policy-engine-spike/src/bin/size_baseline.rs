//! PES-M9 baseline: the same corpus and request handling as the engine
//! probes, with no engine linked.

use asura_policy_engine_spike::{all_requests, corpus};

fn main() {
    let scale = corpus::scale();
    let data = scale.store.to_rego_data();
    let mut bytes = data.to_string().len();
    for r in all_requests(&scale) {
        bytes += r.to_json().to_string().len();
    }
    println!("{bytes}");
}
