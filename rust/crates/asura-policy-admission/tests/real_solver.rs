//! PL-I6 and PL-I7 (security-policy.md, PL3 validation map) with the real
//! cvc5 of the development package, reached through the platform's solver
//! process owner under the solver profile. These tests fail when the
//! package is absent: an absent solver is a gap, never a pass.
#[path = "support/bridge.rs"]
mod bridge;

use asura_policy_admission::{
    Admission, Baseline, Candidate, NoLiteralCheck, Outcome, QueryOutcome, Reason, Scope, admit,
    prove_builtin,
};
use bridge::{Bridge, Scratch, package};
use std::sync::atomic::AtomicBool;

const INSTALLATION: &str = include_str!("../../asura-policy/policies/installation.cedar");
const USER: &str = include_str!("../../asura-policy/policies/user-default.cedar");

fn run(tag: &str, scope: Scope, text: &str) -> Admission {
    let scratch = Scratch::new(tag);
    let package = package();
    let mut pipe = Bridge::start(&scratch, &package);
    let copy = pipe.copy_directory().to_path_buf();
    let started = std::time::Instant::now();
    let admission = admit(
        &Candidate { scope, text },
        &Baseline {
            installation: INSTALLATION,
            user_default: USER,
            previous: Some(USER),
        },
        Some(&mut pipe),
        &NoLiteralCheck,
        &AtomicBool::new(false),
    );
    println!(
        "{tag}: {} queries in {:.1} ms",
        admission.queries.len(),
        started.elapsed().as_secs_f64() * 1000.0
    );
    let exit = pipe.finish();
    assert!(!exit.memory_bound);
    assert!(!copy.exists(), "the solver copy is removed after the reap");
    admission
}

fn refusal(a: &Admission) -> (Reason, Vec<String>, Vec<String>) {
    match &a.outcome {
        Outcome::Rejected {
            reason,
            details,
            counterexamples,
            ..
        } => (*reason, details.clone(), counterexamples.clone()),
        Outcome::Ready => panic!("expected a refusal, got Ready: {a:?}"),
    }
}

/// PL-I6: the release-gate proofs of the built-in sets all hold.
#[test]
fn pl_i6_builtin_sets_hold_with_the_real_solver() {
    let scratch = Scratch::new("builtin");
    let package = package();
    let mut pipe = Bridge::start(&scratch, &package);
    let records = prove_builtin(INSTALLATION, USER, &mut pipe, &AtomicBool::new(false));
    pipe.finish();
    for id in ["SP-AQ1", "SP-AQ4", "SP-AQ9", "compile-a"] {
        assert!(records.iter().any(|r| r.id == id), "{id} ran");
    }
    let bad: Vec<_> = records
        .iter()
        .filter(|r| r.outcome != QueryOutcome::Holds)
        .collect();
    assert!(bad.is_empty(), "{bad:?}");
    println!("prove_builtin: {} queries hold", records.len());
}

/// The default user set is admitted, keeps everything and adds nothing.
#[test]
fn the_default_user_set_is_admitted() {
    let a = run("default", Scope::User, USER);
    assert_eq!(a.outcome, Outcome::Ready, "{a:?}");
    assert!(!a.narrows);
    assert!(a.widening.is_empty());
}

fn with_user(extra: &str) -> String {
    format!("{USER}\n{extra}\n")
}

/// PL-I7: a CL-1 violation, an `@intent` permit, is refused by the lint
/// and shown by an SP-AQ3 counterexample.
#[test]
fn pl_i7_cl1_violation_is_refused_with_a_counterexample() {
    let text = with_user(
        r#"@id("intent_permit") @reason("r") @intent("true")
permit (principal, action == Asura::Action::"process.exec", resource) when { context has intent };"#,
    );
    let (reason, details, examples) = refusal(&run("cl1", Scope::User, &text));
    assert_eq!(reason, Reason::CandidateLint);
    assert!(
        details.iter().any(|d| d.contains("user:intent_permit")),
        "{details:?}"
    );
    assert!(!examples.is_empty(), "shown by a counterexample");
    assert!(examples[0].contains("process.exec"), "{examples:?}");
}

/// PL-I7: a scope widening, a repository `permit`, is refused by the lint
/// and shown by a counterexample.
#[test]
fn pl_i7_scope_widening_is_refused_with_a_counterexample() {
    let text = r#"@id("open") @reason("r") permit (principal, action, resource);"#;
    let (reason, details, examples) = refusal(&run("widen", Scope::Repository, text));
    assert_eq!(reason, Reason::CandidateLint);
    assert!(details[0].contains("repository:open"), "{details:?}");
    assert!(!examples.is_empty(), "shown by a counterexample");
}

/// PL-I7: an AD-7 forbid of one address and port for person jobs fails
/// SP-AQ9, with a counterexample.
#[test]
fn pl_i7_ad7_forbid_of_one_address_and_port_fails() {
    let text = with_user(
        r#"@id("no_addr") @reason("r")
forbid (principal, action == Asura::Action::"network.connect", resource)
when { principal.origin == Asura::Origin::"person_typed" && context.ip == ip("192.0.2.1") && context.port == 443 };"#,
    );
    let (reason, details, examples) = refusal(&run("ad7", Scope::User, &text));
    assert_eq!(reason, Reason::ProofFailed);
    assert!(
        details[0].contains("person network forbidden (AD-7)"),
        "{details:?}"
    );
    assert!(examples[0].contains("network.connect"), "{examples:?}");
}

/// PL-I7: a forbid that can error fails SP-AQ1, with a counterexample.
#[test]
fn pl_i7_a_forbid_that_can_error_fails() {
    let text = with_user(
        r#"@id("overflow") @reason("r")
forbid (principal, action == Asura::Action::"network.connect", resource)
when { principal.origin == Asura::Origin::"model" && context.port + 9223372036854775807 > 0 };"#,
    );
    let (reason, details, examples) = refusal(&run("errors", Scope::User, &text));
    assert_eq!(reason, Reason::ProofFailed);
    assert!(
        details[0].contains("user:overflow: this forbid can error"),
        "{details:?}"
    );
    assert!(!examples.is_empty());
}

/// PL-I7: a model write forbid on `vendor/` under the project root cannot
/// be enforced by the root-granular `model-1` compiler.
#[test]
fn pl_i7_a_subtree_forbid_cannot_be_enforced() {
    let text = with_user(
        r#"@id("vendor") @reason("r")
forbid (principal, action == Asura::Action::"file.write", resource is Asura::Path)
when { principal.profile == Asura::Profile::"model-1" && resource.rel like "vendor/*" };"#,
    );
    let (reason, details, examples) = refusal(&run("vendor", Scope::User, &text));
    assert_eq!(reason, Reason::CannotEnforce);
    assert!(details[0].contains("write project region"), "{details:?}");
    assert!(!examples.is_empty());
}

/// PL-I7: a policy that changes person file effects, which PL3 does not
/// enforce, cannot be enforced. (A person file forbid changes nothing,
/// because the default user set permits no person file effect; a permit
/// changes them.)
#[test]
fn pl_i7_a_person_file_change_cannot_be_enforced() {
    let text = with_user(
        r#"@id("person_write") @reason("r")
permit (principal, action == Asura::Action::"file.write", resource)
when { principal.profile == Asura::Profile::"person-1" };"#,
    );
    let (reason, details, examples) = refusal(&run("person", Scope::User, &text));
    assert_eq!(reason, Reason::CannotEnforce);
    assert!(details[0].contains("file.write"), "{details:?}");
    assert!(!examples.is_empty());
}

/// A tighten-only entry forbid is admitted and narrows, so activation
/// rechecks live decisions.
#[test]
fn a_narrowing_entry_forbid_is_admitted_and_narrows() {
    let text = with_user(
        r#"@id("no_git") @reason("git_denied") @intent("true")
forbid (principal, action == Asura::Action::"process.exec", resource)
when { context has intent && context.intent has command && context.intent.command.first_tool == "git" };"#,
    );
    let a = run("narrow", Scope::User, &text);
    assert_eq!(a.outcome, Outcome::Ready, "{a:?}");
    assert!(a.narrows);
    assert!(a.widening.is_empty());
}

/// A widening change is admitted and reports what it newly permits.
#[test]
fn a_widening_change_reports_examples() {
    let text = with_user(
        r#"@id("model_secret") @reason("r")
permit (principal, action == Asura::Action::"secret_use", resource)
when { principal.origin == Asura::Origin::"model" };"#,
    );
    let a = run("wide", Scope::User, &text);
    assert_eq!(a.outcome, Outcome::Ready, "{a:?}");
    assert!(!a.narrows);
    assert!(!a.widening.is_empty());
    assert!(a.widening[0].contains("secret_use"), "{:?}", a.widening);
}
