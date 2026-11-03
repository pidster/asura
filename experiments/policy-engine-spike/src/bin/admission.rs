//! PES-M13 to PES-M18: admission analysis with SymCC and cvc5. Needs the
//! `CVC5` environment variable. No query has a time limit.

use asura_policy_engine_spike::admission::{Analyser, coverage_probes, run_all, summary};
use std::collections::BTreeMap;

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let mut a = match Analyser::new() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("admission analysis unavailable: {e}");
            std::process::exit(2);
        }
    };
    println!("request environments: {}", a.envs.len());
    let notes = run_all(&mut a, None).await;
    for (k, v) in &notes {
        println!("note {k}: {v}");
    }

    let mut by_cat: BTreeMap<&str, Vec<&_>> = BTreeMap::new();
    for q in &a.queries {
        by_cat.entry(q.category).or_default().push(q);
    }
    for (cat, qs) in &by_cat {
        let ok = qs.iter().filter(|q| q.as_expected()).count();
        let failed = qs.iter().filter(|q| q.holds.is_none()).count();
        let mut t: Vec<f64> = qs.iter().map(|q| q.ms).collect();
        let (p50, p95, max) = summary(&mut t);
        println!(
            "category {cat}: queries={} as_expected={ok} solver_or_compile_failures={failed} median_ms={p50:.1} p95_ms={p95:.1} max_ms={max:.1}",
            qs.len()
        );
    }
    for q in a
        .queries
        .iter()
        .filter(|q| !q.as_expected() || !q.note.is_empty())
    {
        println!(
            "detail {} [{}] {}: holds={:?} expected={} {:.1} ms {}",
            q.category, q.env, q.id, q.holds, q.expected, q.ms, q.note
        );
    }
    let mut all: Vec<f64> = a.queries.iter().map(|q| q.ms).collect();
    let (p50, p95, max) = summary(&mut all);
    let slowest = a
        .queries
        .iter()
        .max_by(|x, y| x.ms.total_cmp(&y.ms))
        .map(|q| format!("{} {} [{}]", q.category, q.id, q.env))
        .unwrap_or_default();
    println!(
        "all queries={} median_ms={p50:.1} p95_ms={p95:.1} max_ms={max:.1} slowest={slowest}",
        a.queries.len()
    );

    // Per-envelope proof set: containment plus safety with that envelope.
    let mut per_env: BTreeMap<String, f64> = BTreeMap::new();
    for q in a.queries.iter().filter(|q| q.category.starts_with("AQ5")) {
        let task = q.id.split(':').next().unwrap_or_default().to_string();
        *per_env.entry(task).or_default() += q.ms;
    }
    let mut env_times: Vec<f64> = per_env.values().copied().collect();
    let (e50, _, emax) = summary(&mut env_times);
    let contained = per_env.len();
    println!("envelope proof sets={contained} median_ms={e50:.1} max_ms={emax:.1}");

    for (name, result, ms) in coverage_probes(&mut a).await {
        println!("coverage {name}: {result} ({ms:.1} ms)");
    }
    let unexpected = a.queries.iter().filter(|q| !q.as_expected()).count();
    println!("unexpected results: {unexpected}");
    if unexpected > 0 {
        std::process::exit(1);
    }
}
