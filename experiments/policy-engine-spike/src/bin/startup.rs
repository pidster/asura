//! PES-M10: cold load of the whole corpus and the first decision, in a
//! fresh process. Usage: `startup cedar|rego`. Peak memory comes from
//! `/usr/bin/time -l` around this process.

use asura_policy_engine_spike::cedar_engine::{CedarEngine, Pattern};
use asura_policy_engine_spike::corpus;
use asura_policy_engine_spike::rego_engine::RegoEngine;
use std::time::Instant;

fn main() {
    let which = std::env::args().nth(1).unwrap_or_else(|| "cedar".into());
    // The generator stands in for reading a stored bundle; it is not timed.
    let scale = corpus::scale();
    let first = corpus::conformance_requests().remove(0).1;
    let (cedar_src, rego_src) = (corpus::all_cedar(&scale), corpus::all_rego(&scale));
    let (schema, lib, decision) = (
        corpus::schema_text(),
        corpus::rego_lib(),
        corpus::rego_decision(),
    );

    let start = Instant::now();
    let allow = match which.as_str() {
        "cedar" => {
            let e = CedarEngine::load(&schema, &cedar_src, &scale.store).expect("Cedar loads");
            e.decide(&first, Pattern::B).allow
        }
        "cedar-phases" => {
            // Diagnostic: time each Cedar load step separately.
            use asura_policy_engine_spike::cedar_engine::{parse_schema, parse_sources, validate};
            let mut t = Instant::now();
            let mut lap = |name: &str| {
                eprintln!("{name}: {:.2} ms", t.elapsed().as_secs_f64() * 1000.0);
                t = Instant::now();
            };
            let schema = parse_schema(&schema).expect("schema");
            lap("schema parse");
            let ps = parse_sources(&cedar_src).expect("parse");
            lap("policy parse and re-id");
            let mut set = cedar_policy::PolicySet::new();
            for (_, p) in &ps {
                set.add(p.clone()).expect("add");
            }
            lap("policy set build");
            assert!(validate(&schema, &set).is_empty());
            lap("strict validation");
            let json = scale.store.to_cedar_json();
            lap("entity JSON encode");
            let entities =
                cedar_policy::Entities::from_json_value(json, Some(&schema)).expect("entities");
            lap("entities parse, schema check and closure");
            drop(entities);
            true
        }
        "rego" => {
            let mut e =
                RegoEngine::load(&lib, &decision, &rego_src, &scale.store).expect("Rego loads");
            e.decide(&first).allow
        }
        other => panic!("unknown engine {other}"),
    };
    let ms = start.elapsed().as_secs_f64() * 1000.0;
    println!(
        "{{\"engine\":\"{which}\",\"policies_sources\":{},\"entities\":{},\"load_and_first_decision_ms\":{ms:.2},\"first_allow\":{allow}}}",
        if which == "cedar" {
            cedar_src.len()
        } else {
            rego_src.len()
        },
        scale.store.entity_count()
    );
}
