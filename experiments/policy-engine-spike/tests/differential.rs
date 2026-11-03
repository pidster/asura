//! PES-M19 (Cedar and Rego agree on the 1,000 scale requests) and the
//! pattern part of PES-M17 (Cedar patterns A and B agree on every request).

use asura_policy_engine_spike::cedar_engine::Pattern;
use asura_policy_engine_spike::{all_requests, corpus, engines};

#[test]
fn engines_and_patterns_agree_on_the_scale_corpus() {
    let scale = corpus::scale();
    let (cedar, mut rego) = engines(&scale);
    let mut engine_diffs = Vec::new();
    let mut pattern_diffs = Vec::new();
    let (mut allows, mut errors) = (0, 0);
    for (i, req) in scale.requests.iter().enumerate() {
        let b = cedar.decide(req, Pattern::B);
        let r = rego.decide(req);
        if (b.allow, &b.rules) != (r.allow, &r.rules) {
            engine_diffs.push(format!(
                "#{i} {:?}: cedar {} / rego {}",
                req.action,
                b.render(),
                r.render()
            ));
        }
        if b.allow {
            allows += 1;
        }
        if b.error.is_some() || r.error.is_some() {
            errors += 1;
        }
    }
    for req in all_requests(&scale) {
        let b = cedar.decide(&req, Pattern::B);
        let a = cedar.decide(&req, Pattern::A);
        if (b.allow, &b.rules, b.error) != (a.allow, &a.rules, a.error) {
            pattern_diffs.push(format!(
                "{:?} {}: B {} / A {}",
                req.action,
                req.resource.id,
                b.render(),
                a.render()
            ));
        }
        // The diagnostic action-sliced variant must be exact.
        let s = cedar.decide(&req, Pattern::BSliced);
        if b != s {
            pattern_diffs.push(format!(
                "{:?} {}: B {} / sliced {}",
                req.action,
                req.resource.id,
                b.render(),
                s.render()
            ));
        }
    }
    println!(
        "scale requests: {}, allowed: {allows}, with errors: {errors}, engine differences: {}, pattern differences: {}",
        scale.requests.len(),
        engine_diffs.len(),
        pattern_diffs.len()
    );
    // The scale corpus must exercise both outcomes to be a meaningful comparison.
    assert!(allows > 100 && allows < 900, "allowed {allows}");
    assert!(engine_diffs.is_empty(), "{}", engine_diffs.join("\n"));
    assert!(pattern_diffs.is_empty(), "{}", pattern_diffs.join("\n"));
}
