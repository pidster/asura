//! PES-M3: injected runtime errors FE1 to FE9 (design: "Injected error
//! cases"). Each case records the raw engine result beside the wrapper result.

use asura_policy_engine_spike::cedar_engine::{CedarEngine, Mode, Pattern, cedar_parts};
use asura_policy_engine_spike::corpus::{self, Scope, Source};
use asura_policy_engine_spike::decision::ErrorClass;
use asura_policy_engine_spike::model::{Command, Context, Request};
use asura_policy_engine_spike::rego_engine::RegoEngine;
use asura_policy_engine_spike::store::Store;

fn case(id: &str) -> Request {
    corpus::conformance_requests()
        .into_iter()
        .find(|(c, _)| c.id == id)
        .map(|(_, r)| r)
        .unwrap_or_else(|| panic!("no case {id}"))
}

fn cedar_with(extra: &str, store: &Store) -> CedarEngine {
    let mut sources = corpus::core_cedar();
    sources.push(Source {
        scope: Scope::Installation,
        name: "injected.cedar".into(),
        text: extra.into(),
    });
    CedarEngine::load(&corpus::schema_text(), &sources, store).expect("injected corpus validates")
}

/// Returns (raw allow, wrapper decision) for one Cedar injection.
fn run_cedar(
    extra: &str,
    store: &Store,
    req: &Request,
) -> (bool, asura_policy_engine_spike::decision::Decision) {
    let engine = cedar_with(extra, store);
    let (p, a, r, c) = cedar_parts(req);
    let raw = engine.decide_parts(p.clone(), a, r.clone(), c.clone(), Pattern::B, Mode::Raw);
    // Raw mode ignores errors, as Cedar's own decision does.
    let wrapped = engine.decide_parts(p, a, r, c, Pattern::B, Mode::Wrapper);
    (raw.allow, wrapped)
}

const FE: &str = "@id(\"fe\") @scope(\"installation\") @reason(\"injected\")\n";

#[test]
fn cedar_errors_skip_the_forbid_but_the_wrapper_denies() {
    let pc1 = case("PC1");
    let mut store = corpus::core_store();
    let base = cedar_with("", &store).decide(&pc1, Pattern::B);
    assert!(base.allow, "PC1 must be permitted without injection");

    // FE1: a forbid reads an attribute of a missing project entity.
    store.projects.remove("studio");
    let fe1 = format!(
        "{FE}forbid (principal is Asura::Job, action == Asura::Action::\"exec\", resource is Asura::Executable)\nwhen {{ principal.project.root == principal.scratch }};"
    );
    let (raw, d) = run_cedar(&fe1, &store, &pc1);
    println!("FE1 raw_allow={raw} wrapper={}", d.render());
    assert!(raw, "FE1: Cedar skips the erroring forbid");
    assert_eq!((d.allow, d.error), (false, Some(ErrorClass::PolicyError)));
    assert!(d.rules.contains("fe"));

    let store = corpus::core_store();
    // FE2: integer overflow in a forbid.
    let mut pc1_vars = pc1.clone();
    if let Context::Exec(c) = &mut pc1_vars.context {
        c.command = Command {
            secret_vars: 1,
            ..c.command.clone()
        };
    }
    let fe2 = format!(
        "{FE}forbid (principal is Asura::Job, action == Asura::Action::\"exec\", resource is Asura::Executable)\nwhen {{ context.command.secret_vars + 9223372036854775807 > 0 }};"
    );
    let (raw, d) = run_cedar(&fe2, &store, &pc1_vars);
    println!("FE2 raw_allow={raw} wrapper={}", d.render());
    assert!(raw);
    assert_eq!((d.allow, d.error), (false, Some(ErrorClass::PolicyError)));

    // FE3: a forbid reads an attribute of a missing executable.
    let mut pc12 = case("PC12");
    pc12.resource.id = "/usr/bin/not-in-store".into();
    let fe3 = format!(
        "{FE}forbid (principal is Asura::Job, action == Asura::Action::\"exec\", resource is Asura::Executable)\nwhen {{ resource.tool == \"rm\" }};"
    );
    let (raw, d) = run_cedar(&fe3, &store, &pc12);
    println!("FE3 raw_allow={raw} wrapper={}", d.render());
    assert!(raw);
    assert_eq!((d.allow, d.error), (false, Some(ErrorClass::PolicyError)));

    // FE4: an extension overflow in a forbid.
    let fe4 = format!(
        "{FE}forbid (principal is Asura::Job, action == Asura::Action::\"exec\", resource is Asura::Executable)\nwhen {{ context.now.offset(duration(\"9223372036854775807ms\")) > context.now }};"
    );
    let (raw, d) = run_cedar(&fe4, &store, &pc1);
    println!("FE4 raw_allow={raw} wrapper={}", d.render());
    assert!(raw);
    assert_eq!((d.allow, d.error), (false, Some(ErrorClass::PolicyError)));
}

fn rego_with(
    extra: Option<&str>,
    decision: Option<&str>,
    data: serde_json::Value,
) -> Result<RegoEngine, Vec<String>> {
    let mut sources = corpus::core_rego();
    if let Some(text) = extra {
        sources.push(Source {
            scope: Scope::Installation,
            name: "injected.rego".into(),
            text: format!(
                "package asura.installation\n\nimport rego.v1\n\nimport data.asura.lib\n\n{text}"
            ),
        });
    }
    let decision_text = decision.map_or_else(corpus::rego_decision, str::to_string);
    RegoEngine::load_data(&corpus::rego_lib(), &decision_text, &sources, data)
}

#[test]
fn rego_errors_deny_but_undefined_data_silently_allows() {
    let pc12 = case("PC12");
    let data = corpus::core_store().to_rego_data();
    let mut base = rego_with(None, None, data.clone()).expect("core loads");
    assert!(
        base.decide(&pc12).allow,
        "PC12 must be permitted without injection"
    );

    // FE5: a built-in error with strict built-in errors on.
    let fe5 = "deny contains {\"id\": \"fe\", \"reason\": \"injected\"} if {\n\tinput.action == \"exec\"\n\tnet.cidr_contains(\"not-a-cidr\", \"1.2.3.4\")\n}";
    let d = rego_with(Some(fe5), None, data.clone())
        .expect("FE5 loads")
        .decide(&pc12);
    println!("FE5 wrapper={}", d.render());
    assert_eq!((d.allow, d.error), (false, Some(ErrorClass::EngineError)));

    // FE6: a second module gives `result` a conflicting value.
    let fe6_decision = format!(
        "{}\n\nresult := {{\"allow\": true, \"rules\": {{{{\"id\": \"fe\", \"reason\": \"injected\"}}}}}} if input.action == \"exec\"\n",
        corpus::rego_decision()
    );
    match rego_with(None, Some(&fe6_decision), data.clone()) {
        Ok(mut e) => {
            let d = e.decide(&case("PC13"));
            println!("FE6 wrapper={}", d.render());
            assert!(!d.allow, "FE6 must not allow");
            assert_eq!(d.error, Some(ErrorClass::EngineError));
        }
        Err(errs) => println!("FE6 refused at load: {errs:?}"),
    }

    // FE7: a deny reads data that is missing; undefined is not an error.
    let fe7 = "deny contains {\"id\": \"fe\", \"reason\": \"injected\"} if {\n\tinput.action == \"exec\"\n\tstartswith(data.entities.projects[lib.job.project].root, \"/\")\n}";
    let present = rego_with(Some(fe7), None, data.clone())
        .expect("FE7 loads")
        .decide(&pc12);
    assert!(!present.allow, "FE7 denies while the data exists");
    let mut missing = data.clone();
    missing["entities"]["projects"]
        .as_object_mut()
        .expect("projects")
        .remove("studio");
    let d = rego_with(Some(fe7), None, missing)
        .expect("FE7 loads")
        .decide(&pc12);
    println!("FE7 present={} missing={}", present.render(), d.render());
    assert!(
        d.allow,
        "FE7: the documented Rego hazard — the deny silently does not fire"
    );

    // FE8: division by zero in a deny.
    let fe8 = "deny contains {\"id\": \"fe\", \"reason\": \"injected\"} if {\n\tinput.action == \"exec\"\n\tx := 1 / 0\n\tx > 0\n}";
    let d = rego_with(Some(fe8), None, data.clone())
        .expect("FE8 loads")
        .decide(&pc12);
    println!("FE8 wrapper={}", d.render());
    assert!(!d.allow, "FE8 must not allow");

    // FE9: the combiner has no `result`.
    let fe9 = "package asura.decision\n\nimport rego.v1\n\nother := 1\n";
    match rego_with(None, Some(fe9), data) {
        // regorus refuses an undefined rule path when the bundle is prepared.
        Err(errs) => println!("FE9 refused at load: {errs:?}"),
        Ok(mut e) => {
            let d = e.decide(&pc12);
            println!("FE9 wrapper={}", d.render());
            assert_eq!((d.allow, d.error), (false, Some(ErrorClass::ResultInvalid)));
        }
    }
}
