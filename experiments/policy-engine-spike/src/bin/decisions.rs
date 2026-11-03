//! PES-M5 cross-process part: print a digest of every decision of the
//! corpus. Two runs in separate processes must print the same digest.

use asura_policy_engine_spike::cedar_engine::Pattern;
use asura_policy_engine_spike::stats::fnv1a;
use asura_policy_engine_spike::{all_requests, corpus, engines};

fn main() {
    let scale = corpus::scale();
    let (cedar, mut rego) = engines(&scale);
    let mut text = String::new();
    for r in all_requests(&scale) {
        text.push_str(&cedar.decide(&r, Pattern::B).render());
        text.push('\n');
        text.push_str(&cedar.decide(&r, Pattern::A).render());
        text.push('\n');
        text.push_str(&rego.decide(&r).render());
        text.push('\n');
    }
    println!(
        "{{\"decisions\":{},\"digest\":\"{:016x}\"}}",
        text.lines().count(),
        fnv1a(text.as_bytes())
    );
}
