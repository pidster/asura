//! Diagnostic for PES-M11: single-threaded time of each step of one decision,
//! over every corpus request, to separate engine cost from harness
//! contention. Iteration-bounded: 20 passes over the corpus.

use asura_policy_engine_spike::cedar_engine::{CedarEngine, Pattern, cedar_parts};
use asura_policy_engine_spike::corpus;
use asura_policy_engine_spike::decision::check_bounds;
use asura_policy_engine_spike::model::Request;
use asura_policy_engine_spike::{all_requests, engines};
use cedar_policy::{Authorizer, Context, EntityUid, Request as CedarRequest};
use std::time::Instant;

const PASSES: usize = 20;

fn us(total_ns: u128, n: usize) -> f64 {
    total_ns as f64 / n as f64 / 1000.0
}

fn main() {
    let scale = corpus::scale();
    let requests = all_requests(&scale);
    let (cedar, mut rego) = engines(&scale);
    let core = CedarEngine::load(&corpus::schema_text(), &corpus::core_cedar(), &scale.store)
        .expect("core loads");
    let n = requests.len() * PASSES;
    let auth = Authorizer::new();

    let mut t = [0u128; 6];
    for _ in 0..PASSES {
        for r in &requests {
            let s = Instant::now();
            let (p, a, res, c) = cedar_parts(r);
            t[0] += s.elapsed().as_nanos();
            let s = Instant::now();
            let checked = serde_json::json!([p, res, c]);
            let bytes = serde_json::to_vec(&checked).map_or(0, |b| b.len());
            let _ = check_bounds(&checked, bytes);
            t[1] += s.elapsed().as_nanos();
            let action =
                EntityUid::from_json(serde_json::json!({"type": "Asura::Action", "id": a}))
                    .expect("action");
            let s = Instant::now();
            let ctx = Context::from_json_value(c, Some((&cedar.schema, &action))).expect("context");
            t[2] += s.elapsed().as_nanos();
            let s = Instant::now();
            let req = CedarRequest::new(
                EntityUid::from_json(p).expect("p"),
                action,
                EntityUid::from_json(res).expect("r"),
                ctx,
                Some(&cedar.schema),
            )
            .expect("request");
            t[3] += s.elapsed().as_nanos();
            let s = Instant::now();
            let _ = auth.is_authorized(&req, &cedar.merged, &cedar.entities);
            t[4] += s.elapsed().as_nanos();
            let s = Instant::now();
            let _ = auth.is_authorized(&req, &core.merged, &cedar.entities);
            t[5] += s.elapsed().as_nanos();
        }
    }
    println!(
        "{{\"cedar_us\":{{\"encode\":{:.2},\"bounds\":{:.2},\"context_validate\":{:.2},\"request_validate\":{:.2},\
         \"is_authorized_258_policies\":{:.2},\"is_authorized_38_core_policies\":{:.2}}}}}",
        us(t[0], n),
        us(t[1], n),
        us(t[2], n),
        us(t[3], n),
        us(t[4], n),
        us(t[5], n)
    );

    let mut full = [0u128; 2];
    for _ in 0..PASSES {
        for r in &requests {
            let s = Instant::now();
            let _ = cedar.decide(r, Pattern::B);
            full[0] += s.elapsed().as_nanos();
            let s = Instant::now();
            let _ = rego.decide(r);
            full[1] += s.elapsed().as_nanos();
        }
    }
    let mut rt = [0u128; 3];
    for _ in 0..PASSES {
        for r in &requests {
            let s = Instant::now();
            let v = r.to_json();
            rt[0] += s.elapsed().as_nanos();
            let s = Instant::now();
            let _ = Request::from_json(&v);
            rt[1] += s.elapsed().as_nanos();
            let s = Instant::now();
            let _ = rego.decide_json(&v, asura_policy_engine_spike::cedar_engine::Mode::Raw);
            rt[2] += s.elapsed().as_nanos();
        }
    }
    println!(
        "{{\"single_thread_decide_us\":{{\"cedar_b\":{:.2},\"rego\":{:.2}}},\"rego_us\":{{\"encode\":{:.2},\"typed_validate\":{:.2},\"evaluate\":{:.2}}}}}",
        us(full[0], n),
        us(full[1], n),
        us(rt[0], n),
        us(rt[1], n),
        us(rt[2], n)
    );
}
