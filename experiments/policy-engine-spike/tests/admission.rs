//! Integration with the real solver (AQ2 to AQ8, and AQ1 on a sample of
//! forbids; the `admission` binary runs AQ1 on every forbid). Fails when
//! `CVC5` is not set: a missing solver is a validation gap, never a pass.

use asura_policy_engine_spike::admission::{Analyser, run_all};

#[tokio::test(flavor = "current_thread")]
async fn admission_queries_give_the_designed_results() {
    assert!(
        std::env::var_os("CVC5").is_some(),
        "set CVC5 to the cvc5 1.3.1 binary; the admission layer cannot be verified without it"
    );
    let mut a = Analyser::new().expect("solver starts");
    let notes = run_all(&mut a, Some(10)).await;
    assert_eq!(notes["AQ2 corpus lint violations"], "0");
    assert_eq!(notes["AQ5 envelopes"], "26");
    assert!(notes["AQ8 seeded classifier permit lint"].contains("reads context.risk"));
    assert!(notes["AQ8 seeded repository permit lint"].contains("holds a permit"));
    let unexpected: Vec<String> = a
        .queries
        .iter()
        .filter(|q| !q.as_expected())
        .map(|q| {
            format!(
                "{} {} [{}]: {:?} {}",
                q.category, q.id, q.env, q.holds, q.note
            )
        })
        .collect();
    assert!(unexpected.is_empty(), "{}", unexpected.join("\n"));
    // The seeded violations must each produce a counterexample.
    assert!(
        a.queries
            .iter()
            .any(|q| q.category == "AQ8 seeded" && q.holds == Some(false))
    );
}
