//! Admission proof times (security-policy.md, PL3 validation map,
//! "Budgets"; spike PES-M13 per query and PES-M18 per proof set), with the
//! real cvc5 of the development package:
//!
//! ```text
//! cargo run --release -p asura-policy-admission --example admission-bench [ROUNDS [PACKAGE]]
//! ```
//!
//! Each round runs `prove_builtin` and admits the default user text and
//! each seeded candidate in a new solver process. It prints the median and
//! largest time per query category over all rounds, and the median and
//! largest total per admission. The loops are bounded by counts; no time
//! bound ends any query.
#[path = "../tests/support/bridge.rs"]
mod bridge;

use asura_policy_admission::{Baseline, Candidate, NoLiteralCheck, Scope, admit, prove_builtin};
use bridge::{Bridge, Scratch, package, package_at};
use std::collections::BTreeMap;
use std::sync::atomic::AtomicBool;
use std::time::Instant;

const INSTALLATION: &str = include_str!("../../asura-policy/policies/installation.cedar");
const USER: &str = include_str!("../../asura-policy/policies/user-default.cedar");

fn ms(micros: u64) -> f64 {
    micros as f64 / 1000.0
}
fn median(values: &mut [u64]) -> u64 {
    values.sort_unstable();
    values[values.len() / 2]
}

fn main() {
    let rounds: usize = std::env::args()
        .nth(1)
        .and_then(|n| n.parse().ok())
        .unwrap_or(5);
    // The package directory, or the development package of the tests.
    let package = match std::env::args().nth(2) {
        Some(dir) => package_at(std::path::Path::new(&dir)),
        None => package(),
    };
    let seeded = [
        ("default", Scope::User, USER.to_owned()),
        (
            "narrowing",
            Scope::User,
            format!(
                "{USER}\n@id(\"no_git\") @reason(\"git_denied\") @intent(\"true\")\nforbid (principal, action == Asura::Action::\"process.exec\", resource) when {{ context has intent && context.intent has command && context.intent.command.first_tool == \"git\" }};\n"
            ),
        ),
        (
            "ad7-forbid",
            Scope::User,
            format!(
                "{USER}\n@id(\"no_addr\") @reason(\"r\")\nforbid (principal, action == Asura::Action::\"network.connect\", resource) when {{ principal.origin == Asura::Origin::\"person_typed\" && context.ip == ip(\"192.0.2.1\") && context.port == 443 }};\n"
            ),
        ),
        (
            "widening",
            Scope::User,
            format!(
                "{USER}\n@id(\"model_secret\") @reason(\"r\")\npermit (principal, action == Asura::Action::\"secret_use\", resource) when {{ principal.origin == Asura::Origin::\"model\" }};\n"
            ),
        ),
    ];
    let mut per_query: BTreeMap<&'static str, Vec<u64>> = BTreeMap::new();
    let mut totals: BTreeMap<&str, Vec<u64>> = BTreeMap::new();
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for _ in 0..rounds {
        let scratch = Scratch::new("bench");
        let mut pipe = Bridge::start(&scratch, &package);
        let at = Instant::now();
        let records = prove_builtin(INSTALLATION, USER, &mut pipe, &AtomicBool::new(false));
        totals
            .entry("prove_builtin")
            .or_default()
            .push(at.elapsed().as_micros() as u64);
        counts.insert("prove_builtin", records.len());
        for r in records {
            per_query.entry(r.id).or_default().push(r.micros);
        }
        pipe.finish();
        for (name, scope, text) in &seeded {
            let mut pipe = Bridge::start(&scratch, &package);
            let at = Instant::now();
            let a = admit(
                &Candidate {
                    scope: *scope,
                    text,
                },
                &Baseline {
                    installation: INSTALLATION,
                    user_default: USER,
                    previous: Some(USER),
                },
                Some(&mut pipe),
                &NoLiteralCheck,
                &AtomicBool::new(false),
            );
            totals
                .entry(name)
                .or_default()
                .push(at.elapsed().as_micros() as u64);
            counts.insert(name, a.queries.len());
            for r in a.queries {
                per_query.entry(r.id).or_default().push(r.micros);
            }
            pipe.finish();
        }
    }
    let mut all: Vec<u64> = per_query.values().flatten().copied().collect();
    println!("rounds {rounds}");
    for (id, values) in &mut per_query {
        let max = values.iter().copied().max().unwrap_or(0);
        println!(
            "query {id}: n={} median {:.1} ms max {:.1} ms",
            values.len(),
            ms(median(values)),
            ms(max)
        );
    }
    let max = all.iter().copied().max().unwrap_or(0);
    let p95 = {
        all.sort_unstable();
        all[(all.len() * 95 / 100).min(all.len() - 1)]
    };
    println!(
        "all queries: n={} median {:.1} ms p95 {:.1} ms max {:.1} ms (PES-M13: median at most 2 s, p95 at most 20 s)",
        all.len(),
        ms(median(&mut all.clone())),
        ms(p95),
        ms(max)
    );
    for (name, values) in &mut totals {
        let max = values.iter().copied().max().unwrap_or(0);
        println!(
            "admission {name}: {} queries, median {:.1} ms max {:.1} ms (PES-M18 guide: median at most 5 s, slowest at most 30 s)",
            counts[name],
            ms(median(values)),
            ms(max)
        );
    }
}
