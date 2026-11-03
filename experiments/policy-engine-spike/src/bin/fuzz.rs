//! PES-M4: malformed variants of permitted requests. Every variant breaks
//! the schema or the W1 bounds, so any Allow through the wrapper is a
//! wrapper defect. Raw-engine allows are recorded beside it.
//! Usage: `fuzz cedar|rego [count]` (default 1,000,000). The run ends by
//! iteration count.

use asura_policy_engine_spike::cedar_engine::{CedarEngine, Mode, Pattern, cedar_parts};
use asura_policy_engine_spike::corpus::{self, Rng};
use asura_policy_engine_spike::decision::ErrorClass;
use asura_policy_engine_spike::engines;
use asura_policy_engine_spike::model::Request;
use asura_policy_engine_spike::rego_engine::RegoEngine;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::thread;

const THREADS: usize = 8;
const KINDS: u64 = 12;

/// Collect paths of leaves in `v`. Cedar entity and extension objects are
/// leaves. `skip` names optional keys that must not be removed.
fn leaves(v: &Value, path: &mut Vec<String>, out: &mut Vec<Vec<String>>) {
    match v {
        Value::Object(o) if !(o.contains_key("__entity") || o.contains_key("__extn")) => {
            for (k, x) in o {
                path.push(k.clone());
                leaves(x, path, out);
                path.pop();
            }
        }
        _ => out.push(path.clone()),
    }
}

fn at<'a>(v: &'a mut Value, path: &[String]) -> &'a mut Value {
    path.iter().fold(v, |acc, k| &mut acc[k.as_str()])
}

fn retype(v: &Value) -> Value {
    match v {
        Value::Bool(_) => json!("true"),
        Value::Number(_) => json!("443"),
        Value::String(_) => json!(7),
        Value::Array(_) => json!("x"),
        Value::Object(_) => json!(1),
        Value::Null => json!(true),
    }
}

fn nested(depth: usize) -> Value {
    (0..depth).fold(json!(1), |acc, _| json!({ "n": acc }))
}

/// Mutate the context (and, for some kinds, the principal, resource or
/// action) so that the result is invalid. `cedar` selects Cedar JSON forms.
fn mutate(
    rng: &mut Rng,
    principal: &mut Value,
    action: &mut String,
    resource: &mut Value,
    context: &mut Value,
    cedar: bool,
) -> u64 {
    let kind = rng.below(KINDS);
    let mut ls = Vec::new();
    leaves(context, &mut Vec::new(), &mut ls);
    let pick = |rng: &mut Rng, ls: &[Vec<String>]| ls[rng.below(ls.len() as u64) as usize].clone();
    match kind {
        0 => {
            // Remove a required field (never the optional `risk`).
            let required: Vec<Vec<String>> = ls
                .iter()
                .filter(|p| p.last().is_some_and(|k| k != "risk"))
                .cloned()
                .collect();
            let p = pick(rng, &required);
            let (last, parent) = p.split_last().expect("path");
            if let Value::Object(o) = at(context, parent) {
                o.remove(last);
            }
        }
        1 => {
            let p = pick(rng, &ls);
            let slot = at(context, &p);
            *slot = retype(slot);
        }
        2 => context["zz_unknown"] = json!(1),
        3 => {
            if context.get("command").is_some() {
                context["command"]["zz_unknown"] = json!("x");
            } else {
                context["zz_unknown"] = json!([1]);
            }
        }
        4 => {
            // Invalid enumeration value.
            let enums: Vec<Vec<String>> = ls
                .iter()
                .filter(|p| {
                    let v = at(&mut context.clone(), p).clone();
                    if cedar {
                        v.get("__entity").is_some()
                    } else {
                        matches!(
                            p.last().map(String::as_str),
                            Some("risk" | "parse" | "delivery" | "data_class")
                        )
                    }
                })
                .cloned()
                .collect();
            if enums.is_empty() {
                context["zz_unknown"] = json!("enum");
            } else {
                let p = pick(rng, &enums);
                let slot = at(context, &p);
                if cedar {
                    slot["__entity"]["id"] = json!("bogus");
                } else {
                    *slot = json!("bogus");
                }
            }
        }
        5 => {
            // Invalid extension literal or time.
            if cedar {
                let ext: Vec<Vec<String>> = ls
                    .iter()
                    .filter(|p| at(&mut context.clone(), p).get("__extn").is_some())
                    .cloned()
                    .collect();
                let p = pick(rng, &ext);
                at(context, &p)["__extn"]["arg"] = json!("2026-13-45T99:00:00Z");
            } else if context.get("ip").is_some() {
                context["ip"] = json!("not-an-ip");
            } else {
                context["now_ms"] = json!(-5);
            }
        }
        6 => principal["type"] = json!(if cedar { "Asura::Secret" } else { "Secret" }),
        7 => resource["type"] = json!(if cedar { "Asura::Project" } else { "Project" }),
        8 => *action = "deploy".into(),
        9 => principal["id"] = json!("p".repeat(2_000)),
        10 => context["zz_list"] = Value::Array((0..100).map(|i| json!(i)).collect()),
        _ => context["zz_deep"] = nested(12),
    }
    kind
}

#[derive(Default)]
struct Tally {
    total: u64,
    wrapper_allows: u64,
    raw_allows: u64,
    by_class: BTreeMap<String, u64>,
    by_kind: BTreeMap<u64, (u64, u64)>, // kind -> (count, raw allows)
    example: Option<String>,
}

impl Tally {
    fn merge(&mut self, o: Tally) {
        self.total += o.total;
        self.wrapper_allows += o.wrapper_allows;
        self.raw_allows += o.raw_allows;
        for (k, v) in o.by_class {
            *self.by_class.entry(k).or_default() += v;
        }
        for (k, (a, b)) in o.by_kind {
            let e = self.by_kind.entry(k).or_default();
            e.0 += a;
            e.1 += b;
        }
        if self.example.is_none() {
            self.example = o.example;
        }
    }
}

fn run_cedar(engine: &CedarEngine, bases: &[Request], count: u64, seed: u64) -> Tally {
    let mut rng = Rng::new(seed);
    let mut t = Tally::default();
    for _ in 0..count {
        let base = &bases[rng.below(bases.len() as u64) as usize];
        let (mut p, a, mut r, mut c) = cedar_parts(base);
        let mut a = a.to_string();
        let kind = mutate(&mut rng, &mut p, &mut a, &mut r, &mut c, true);
        let w = engine.decide_parts(
            p.clone(),
            &a,
            r.clone(),
            c.clone(),
            Pattern::B,
            Mode::Wrapper,
        );
        let raw = engine.decide_parts(p, &a, r, c, Pattern::B, Mode::Raw);
        tally(&mut t, kind, w.allow, raw.allow, w.error, &a);
    }
    t
}

fn run_rego(mut engine: RegoEngine, bases: &[Request], count: u64, seed: u64) -> Tally {
    let mut rng = Rng::new(seed);
    let mut t = Tally::default();
    for _ in 0..count {
        let base = &bases[rng.below(bases.len() as u64) as usize];
        let mut v = base.to_json();
        let (mut p, mut a, mut r, mut c) = (
            v["principal"].take(),
            v["action"].as_str().unwrap_or_default().to_string(),
            v["resource"].take(),
            v["context"].take(),
        );
        let kind = mutate(&mut rng, &mut p, &mut a, &mut r, &mut c, false);
        let input = json!({"principal": p, "action": a, "resource": r, "context": c});
        let w = engine.decide_json(&input, Mode::Wrapper);
        let raw = engine.decide_json(&input, Mode::Raw);
        tally(&mut t, kind, w.allow, raw.allow, w.error, &a);
    }
    t
}

fn tally(
    t: &mut Tally,
    kind: u64,
    wrapper: bool,
    raw: bool,
    class: Option<ErrorClass>,
    action: &str,
) {
    t.total += 1;
    t.wrapper_allows += u64::from(wrapper);
    t.raw_allows += u64::from(raw);
    let class = class.map_or("decided", ErrorClass::reason);
    *t.by_class.entry(class.to_string()).or_default() += 1;
    let e = t.by_kind.entry(kind).or_default();
    e.0 += 1;
    e.1 += u64::from(raw);
    if wrapper && t.example.is_none() {
        t.example = Some(format!("kind {kind} action {action}"));
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let which = args.next().unwrap_or_else(|| "cedar".into());
    let count: u64 = args
        .next()
        .and_then(|s| s.parse().ok())
        .unwrap_or(1_000_000);
    let scale = corpus::scale();
    let (cedar, rego) = engines(&scale);
    // Bases: every permitted request of the corpus.
    let mut probe = rego.clone();
    let bases: Vec<Request> = asura_policy_engine_spike::all_requests(&scale)
        .into_iter()
        .filter(|r| cedar.decide(r, Pattern::B).allow && probe.decide(r).allow)
        .collect();
    let bases = Arc::new(bases);
    let cedar = Arc::new(cedar);
    let per = count / THREADS as u64;
    let handles: Vec<_> = (0..THREADS)
        .map(|i| {
            let (bases, cedar, rego, which) =
                (bases.clone(), cedar.clone(), rego.clone(), which.clone());
            thread::spawn(move || match which.as_str() {
                "cedar" => run_cedar(&cedar, &bases, per, 0xF00D + i as u64),
                "rego" => run_rego(rego, &bases, per, 0xF00D + i as u64),
                other => panic!("unknown engine {other}"),
            })
        })
        .collect();
    let mut t = Tally::default();
    for h in handles {
        t.merge(h.join().expect("fuzz thread"));
    }
    let kinds: Vec<String> = t
        .by_kind
        .iter()
        .map(|(k, (n, raw))| format!("\"{k}\":[{n},{raw}]"))
        .collect();
    let classes: Vec<String> = t
        .by_class
        .iter()
        .map(|(k, n)| format!("\"{k}\":{n}"))
        .collect();
    println!(
        "{{\"engine\":\"{which}\",\"bases\":{},\"variants\":{},\"wrapper_allows\":{},\"raw_allows\":{},\
         \"wrapper_outcomes\":{{{}}},\"kind_count_raw_allows\":{{{}}},\"first_wrapper_allow\":{:?}}}",
        bases.len(),
        t.total,
        t.wrapper_allows,
        t.raw_allows,
        classes.join(","),
        kinds.join(","),
        t.example
    );
    if t.wrapper_allows > 0 {
        std::process::exit(1);
    }
}
