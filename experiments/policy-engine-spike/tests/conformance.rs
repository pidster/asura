//! PES-M1 and PES-M7: every conformance case gives the expected decision,
//! rule IDs and reason codes through each wrapper and both Cedar patterns.

use asura_policy_engine_spike::cedar_engine::Pattern;
use asura_policy_engine_spike::corpus::{self, Case};
use asura_policy_engine_spike::decision::{Decision, ErrorClass};
use asura_policy_engine_spike::engines;
use std::collections::BTreeSet;

fn expect(case: &Case, engine: &str) -> (BTreeSet<String>, Option<String>) {
    if engine == "cedar"
        && let Some(r) = &case.cedar_rules
    {
        return (r.iter().cloned().collect(), case.cedar_error.clone());
    }
    (case.rules.iter().cloned().collect(), None)
}

fn check(case: &Case, engine: &str, got: &Decision, failures: &mut Vec<String>) {
    let (rules, error) = expect(case, engine);
    let error_ok = match (&error, got.error) {
        (None, None) => true,
        (Some(e), Some(g)) => e == g.reason(),
        _ => false,
    };
    let reasons_ok = if got.error.is_some() {
        got.reasons
            .contains(got.error.map(ErrorClass::reason).unwrap_or_default())
    } else if rules.is_empty() {
        got.reasons == BTreeSet::from(["no_permit".to_string()])
    } else {
        !got.reasons.is_empty() && !got.reasons.contains("no_permit")
    };
    if got.allow != case.allow || got.rules != rules || !error_ok || !reasons_ok {
        failures.push(format!(
            "{} [{engine}]: expected allow={} rules={rules:?} error={error:?}; got {}",
            case.id,
            case.allow,
            got.render()
        ));
    }
}

#[test]
fn conformance_cases_pass_in_both_engines_and_patterns() {
    let scale = corpus::scale();
    let (cedar, mut rego) = engines(&scale);
    let mut failures = Vec::new();
    let cases = corpus::conformance_requests();
    assert_eq!(cases.len(), 64);
    let (mut allows, mut denies) = (0, 0);
    for (case, req) in &cases {
        let b = cedar.decide(req, Pattern::B);
        let r = rego.decide(req);
        check(case, "cedar", &b, &mut failures);
        check(case, "cedar", &cedar.decide(req, Pattern::A), &mut failures);
        check(
            case,
            "cedar",
            &cedar.decide(req, Pattern::BSliced),
            &mut failures,
        );
        check(case, "rego", &r, &mut failures);
        // Reason codes come from Cedar annotations and from Rego rule
        // objects; for the same rules they must be the same codes.
        if b.error.is_none() && r.error.is_none() && b.rules == r.rules && b.reasons != r.reasons {
            failures.push(format!(
                "{}: reasons differ: {} / {}",
                case.id,
                b.render(),
                r.render()
            ));
        }
        if b.allow { allows += 1 } else { denies += 1 }
    }
    assert_eq!((allows, denies), (27, 37), "allow and deny counts");
    assert!(
        failures.is_empty(),
        "{} failures:\n{}",
        failures.len(),
        failures.join("\n")
    );
}
