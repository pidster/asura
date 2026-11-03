//! Admission analysis with `cedar-policy-symcc` and cvc5 (design: "Admission
//! analysis", AQ1 to AQ9). Runs at admission only, never on the decision
//! path. A query has no time limit; it ends by the solver's reply or by the
//! operator's cancel. A solver failure is a refusal, never a pass.

use crate::cedar_engine::{
    lint_lower_scopes, parse_schema, parse_sources, scope_actions, validate,
};
use crate::corpus::{self, Scope, Source};
use crate::model::{EXPIRY_MS, rfc3339};
use cedar_policy::{Effect, Policy, PolicyId, PolicySet, RequestEnv, Schema};
use cedar_policy_symcc::solver::LocalSolver;
use cedar_policy_symcc::{CedarSymCompiler, CompiledPolicy, CompiledPolicySet};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::time::Instant;

/// One timed query result.
#[derive(Clone, Debug)]
pub struct Query {
    pub id: String,
    pub category: &'static str,
    pub env: String,
    /// Some(true) proven, Some(false) refuted (counterexample), None failed.
    pub holds: Option<bool>,
    pub expected: bool,
    pub ms: f64,
    pub note: String,
}

impl Query {
    pub fn as_expected(&self) -> bool {
        self.holds == Some(self.expected)
    }
}

pub struct Analyser {
    pub schema: Schema,
    pub envs: Vec<RequestEnv>,
    compiler: CedarSymCompiler<LocalSolver>,
    pub queries: Vec<Query>,
}

fn env_name(e: &RequestEnv) -> String {
    format!("{}/{}/{}", e.principal(), e.action(), e.resource())
}

pub fn set_of<'a>(ps: impl IntoIterator<Item = &'a Policy>) -> PolicySet {
    let mut s = PolicySet::new();
    for p in ps {
        s.add(p.clone()).expect("unique policy IDs");
    }
    s
}

pub fn policy(id: &str, text: &str) -> Policy {
    Policy::parse(Some(PolicyId::new(id)), text).unwrap_or_else(|e| panic!("{id}: {e}"))
}

fn env_action(e: &RequestEnv) -> String {
    e.action().id().unescaped().to_string()
}

/// AQ2: classifier attributes appear only in `@classifier` forbids.
pub fn lint_classifier(policies: &[(Scope, Policy)]) -> Vec<String> {
    fn reads_risk(v: &Value) -> bool {
        match v {
            Value::Object(o) => {
                let here = o.get("attr").and_then(Value::as_str) == Some("risk")
                    && o.get("left") == Some(&serde_json::json!({"Var": "context"}));
                here || o.values().any(reads_risk)
            }
            Value::Array(a) => a.iter().any(reads_risk),
            _ => false,
        }
    }
    policies
        .iter()
        .filter(|(_, p)| p.to_json().is_ok_and(|j| reads_risk(&j)))
        .filter(|(_, p)| p.effect() != Effect::Forbid || p.annotation("classifier").is_none())
        .map(|(_, p)| {
            format!(
                "{} reads context.risk but is not a @classifier forbid",
                p.id()
            )
        })
        .collect()
}

impl Analyser {
    pub fn new() -> Result<Self, String> {
        let schema = parse_schema(&corpus::schema_text())?;
        let envs: Vec<RequestEnv> = schema.request_envs().collect();
        let solver = LocalSolver::cvc5().map_err(|e| format!("cvc5: {e} (set CVC5)"))?;
        let compiler = CedarSymCompiler::new(solver).map_err(|e| e.to_string())?;
        Ok(Analyser {
            schema,
            envs,
            compiler,
            queries: Vec::new(),
        })
    }

    fn envs_for(&self, actions: Option<&BTreeSet<String>>) -> Vec<RequestEnv> {
        self.envs
            .iter()
            .filter(|e| actions.is_none_or(|a| a.contains(&env_action(e))))
            .cloned()
            .collect()
    }

    /// AQ1: one forbid never errors in one environment.
    pub async fn never_errors(&mut self, id: &str, p: &Policy, env: &RequestEnv, expected: bool) {
        let start = Instant::now();
        let (holds, note) = match CompiledPolicy::compile(p, env, &self.schema) {
            Ok(c) => match self.compiler.check_never_errors_opt(&c).await {
                Ok(b) => (Some(b), String::new()),
                Err(e) => (None, e.to_string()),
            },
            Err(e) => (None, format!("compile: {e}")),
        };
        self.queries.push(Query {
            id: id.into(),
            category: if id.starts_with("seed.") {
                "AQ1 seeded can-error"
            } else {
                "AQ1 never_errors"
            },
            env: env_name(env),
            holds,
            expected,
            ms: start.elapsed().as_secs_f64() * 1000.0,
            note,
        });
    }

    /// `check_implies(a, b)` with a counterexample when it fails.
    pub async fn implies(
        &mut self,
        id: &str,
        category: &'static str,
        a: &PolicySet,
        b: &PolicySet,
        env: &RequestEnv,
        expected: bool,
    ) {
        let start = Instant::now();
        let (holds, note) = match (
            CompiledPolicySet::compile(a, env, &self.schema),
            CompiledPolicySet::compile(b, env, &self.schema),
        ) {
            (Ok(ca), Ok(cb)) => match self
                .compiler
                .check_implies_with_counterexample_opt(&ca, &cb)
                .await
            {
                Ok(None) => (Some(true), String::new()),
                Ok(Some(cex)) => (
                    Some(false),
                    format!(
                        "counterexample: {} {} {}",
                        cex.request
                            .principal()
                            .map(|u| u.to_string())
                            .unwrap_or_default(),
                        cex.request
                            .action()
                            .map(|u| u.to_string())
                            .unwrap_or_default(),
                        cex.request
                            .resource()
                            .map(|u| u.to_string())
                            .unwrap_or_default()
                    ),
                ),
                Err(e) => (None, e.to_string()),
            },
            (Err(e), _) | (_, Err(e)) => (None, format!("compile: {e}")),
        };
        self.queries.push(Query {
            id: id.into(),
            category,
            env: env_name(env),
            holds,
            expected,
            ms: start.elapsed().as_secs_f64() * 1000.0,
            note,
        });
    }
}

/// Safety specifications for AQ6: permit everything, then forbid one property.
pub fn safety_specs() -> Vec<(&'static str, &'static str, PolicySet)> {
    let all = "permit (principal, action, resource);";
    let specs = [
        (
            "S_raw_secret",
            "secret_use",
            "forbid (principal is Asura::Job, action == Asura::Action::\"secret_use\", resource is Asura::Secret) when { principal.origin == Asura::Origin::\"model\" && context.delivery == Asura::Delivery::\"raw\" };",
        ),
        (
            "S_implicit_exec",
            "write",
            "forbid (principal is Asura::Job, action == Asura::Action::\"write\", resource is Asura::Path) when { principal.origin == Asura::Origin::\"model\" && resource.class == Asura::PathClass::\"implicit_exec\" };",
        ),
        (
            "S_asura_state",
            "write",
            "forbid (principal is Asura::Job, action == Asura::Action::\"write\", resource is Asura::Path) when { resource.class == Asura::PathClass::\"asura_state\" && !(resource in principal.scratch) };",
        ),
        (
            "S_model_network",
            "connect",
            "forbid (principal is Asura::Job, action == Asura::Action::\"connect\", resource) when { principal.origin == Asura::Origin::\"model\" && (!context.via_proxy || context.ip.isLoopback() || context.ip.isInRange(ip(\"169.254.0.0/16\"))) };",
        ),
        (
            "S_tainted_egress",
            "egress",
            "forbid (principal is Asura::Task, action == Asura::Action::\"egress\", resource) when { context.tainted };",
        ),
        (
            "S_model_privilege",
            "exec",
            "forbid (principal is Asura::Job, action in Asura::Action::\"exec_any\", resource is Asura::Executable) when { principal.origin == Asura::Origin::\"model\" && [\"sudo\", \"su\", \"doas\"].contains(resource.tool) };",
        ),
    ];
    specs
        .into_iter()
        .map(|(id, action, forbid)| {
            let mut s = PolicySet::new();
            s.add(policy("spec.all", all)).expect("add");
            s.add(policy(id, forbid)).expect("add");
            (id, action, s)
        })
        .collect()
}

/// Envelope specification S_E: anything for the envelope's task, before expiry.
pub fn envelope_spec(task: &str) -> Policy {
    policy(
        "spec.envelope",
        &format!(
            "permit (principal is Asura::Job, action, resource) when {{ principal.task == Asura::Task::\"{task}\" && context.now < datetime(\"{}\") }};",
            rfc3339(EXPIRY_MS)
        ),
    )
}

/// The full Cedar corpus as (scope, policy) pairs, validated.
pub fn corpus_policies() -> Vec<(Scope, Policy)> {
    let scale = corpus::scale();
    let sources: Vec<Source> = corpus::all_cedar(&scale);
    let ps = parse_sources(&sources).expect("corpus parses");
    assert!(lint_lower_scopes(&ps).is_empty());
    let schema = parse_schema(&corpus::schema_text()).expect("schema");
    assert!(validate(&schema, &set_of(ps.iter().map(|(_, p)| p))).is_empty());
    ps
}

/// Summary statistics over query times in milliseconds.
pub fn summary(times: &mut [f64]) -> (f64, f64, f64) {
    times.sort_by(f64::total_cmp);
    let pick = |p: f64| {
        if times.is_empty() {
            return 0.0;
        }
        let rank = ((p / 100.0) * times.len() as f64).ceil() as usize;
        times[rank.clamp(1, times.len()) - 1]
    };
    (pick(50.0), pick(95.0), times.last().copied().unwrap_or(0.0))
}

fn envs_named(a: &Analyser, action: &str) -> Vec<RequestEnv> {
    a.envs
        .iter()
        .filter(|e| env_action(e) == action)
        .cloned()
        .collect()
}

/// Run AQ1 to AQ8 over the corpus. AQ9 runs separately (`coverage_probes`).
pub async fn run_all(
    a: &mut Analyser,
    never_errors_limit: Option<usize>,
) -> BTreeMap<String, String> {
    let mut notes = BTreeMap::new();
    let ps = corpus_policies();
    let all = set_of(ps.iter().map(|(_, p)| p));
    let p0_pairs: Vec<&(Scope, Policy)> = ps
        .iter()
        .filter(|(_, p)| p.annotation("classifier").is_none())
        .collect();
    let p0 = set_of(p0_pairs.iter().map(|(_, p)| p));
    let upper = set_of(ps.iter().filter(|(s, _)| !s.is_lower()).map(|(_, p)| p));

    // AQ2: classifier lint over the corpus.
    let lint = lint_classifier(&ps);
    notes.insert("AQ2 corpus lint violations".into(), lint.len().to_string());

    // AQ1: every forbid, every applicable environment.
    let forbids: Vec<&Policy> = ps
        .iter()
        .map(|(_, p)| p)
        .filter(|p| p.effect() == Effect::Forbid)
        .collect();
    notes.insert("AQ1 forbid policies".into(), forbids.len().to_string());
    for p in forbids
        .iter()
        .take(never_errors_limit.unwrap_or(usize::MAX))
    {
        for env in a.envs_for(scope_actions(p).as_ref()) {
            a.never_errors(p.id().as_ref(), p, &env, true).await;
        }
    }
    // AQ1 seeded: forbids that can error must be refuted, or AQ1 proves
    // nothing. FE2 and FE4 shapes (overflow) and the FE1 shape (an attribute
    // of a referenced entity that may be missing at run time).
    let exec_env: Vec<RequestEnv> = envs_named(a, "exec");
    for (id, cond, expected) in [
        (
            "seed.fe2.overflow",
            "context.command.secret_vars + 9223372036854775807 > 0",
            false,
        ),
        (
            "seed.fe4.offset",
            "context.now.offset(duration(\"9223372036854775807ms\")) > context.now",
            false,
        ),
        (
            "seed.fe1.missing_entity",
            "principal.project.root == principal.scratch",
            true,
        ),
    ] {
        let p = policy(
            id,
            &format!(
                "forbid (principal is Asura::Job, action == Asura::Action::\"exec\", resource is Asura::Executable) when {{ {cond} }};"
            ),
        );
        for env in &exec_env {
            a.never_errors(id, &p, env, expected).await;
        }
    }

    let envs = a.envs.clone();
    // AQ3: CL-1, P implies P0. AQ4: P implies P without lower scopes.
    for env in &envs {
        a.implies("CL-1", "AQ3 cl1 implies(P,P0)", &all, &p0, env, true)
            .await;
        a.implies(
            "narrowing",
            "AQ4 implies(P,P_upper)",
            &all,
            &upper,
            env,
            true,
        )
        .await;
    }

    // AQ6: safety specifications on P and on P0.
    for (id, action, spec) in safety_specs() {
        for env in envs.iter().filter(|e| env_action(e) == action) {
            a.implies(id, "AQ6 safety on P", &all, &spec, env, true)
                .await;
            a.implies(id, "AQ6 safety on P0", &p0, &spec, env, true)
                .await;
        }
    }

    // AQ5: envelope containment, one envelope at a time, against P0 without
    // any envelope.
    let mut envelopes: BTreeMap<String, Vec<Policy>> = BTreeMap::new();
    for (_, p) in &p0_pairs {
        if let Some(e) = p.annotation("envelope") {
            envelopes.entry(e.to_string()).or_default().push(p.clone());
        }
    }
    let base: Vec<&Policy> = p0_pairs
        .iter()
        .map(|(_, p)| p)
        .filter(|p| p.annotation("envelope").is_none())
        .collect();
    notes.insert("AQ5 envelopes".into(), envelopes.len().to_string());
    for (task, grants) in &envelopes {
        let with = set_of(base.iter().copied().chain(grants.iter()));
        let spec_e = envelope_spec(task);
        let bound = set_of(base.iter().copied().chain(std::iter::once(&spec_e)));
        let actions: BTreeSet<String> = grants.iter().filter_map(scope_actions).flatten().collect();
        for env in envs.iter().filter(|e| actions.contains(&env_action(e))) {
            a.implies(task, "AQ5 envelope containment", &with, &bound, env, true)
                .await;
        }
        for (id, action, spec) in safety_specs() {
            for env in envs
                .iter()
                .filter(|e| env_action(e) == action && actions.contains(action))
            {
                a.implies(
                    &format!("{task}:{id}"),
                    "AQ5 envelope safety",
                    &with,
                    &spec,
                    env,
                    true,
                )
                .await;
            }
        }
    }

    // AQ7: change difference. Narrowing holds; widening yields a counterexample.
    let narrow = policy(
        "change.narrow",
        "@id(\"change.narrow\") forbid (principal is Asura::Job, action == Asura::Action::\"connect\", resource) when { context.port == 80 };",
    );
    let wide = policy(
        "change.wide",
        "@id(\"change.wide\") permit (principal is Asura::Job, action == Asura::Action::\"write\", resource is Asura::Path) when { resource in Asura::Path::\"/Users/person/notes\" };",
    );
    let p_narrow = set_of(ps.iter().map(|(_, p)| p).chain(std::iter::once(&narrow)));
    let p_wide = set_of(ps.iter().map(|(_, p)| p).chain(std::iter::once(&wide)));
    for env in envs.iter().filter(|e| env_action(e) == "connect") {
        a.implies(
            "narrowing change",
            "AQ7 change difference",
            &p_narrow,
            &all,
            env,
            true,
        )
        .await;
    }
    for env in envs.iter().filter(|e| env_action(e) == "write") {
        a.implies(
            "widening change",
            "AQ7 change difference",
            &p_wide,
            &all,
            env,
            false,
        )
        .await;
    }

    // AQ8: seeded violations.
    let research_pitfall = "@id(\"seed.clf.pitfall\") @classifier(\"risk\") permit (principal is Asura::Job, action == Asura::Action::\"exec\", resource) unless { context.risk == Asura::Risk::\"dangerous\" };";
    let pitfall_valid = parse_sources(&[Source {
        scope: Scope::Installation,
        name: "seed".into(),
        text: research_pitfall.replace(
            "@classifier",
            "@scope(\"installation\") @reason(\"x\") @classifier",
        ),
    }])
    .map(|v| validate(&a.schema, &set_of(v.iter().map(|(_, p)| p))));
    notes.insert(
        "AQ8 research pitfall (permit unless context.risk) strict validation errors".into(),
        pitfall_valid.map_or_else(|e| format!("parse: {e:?}"), |v| v.len().to_string()),
    );
    let clf_permit = policy(
        "seed.clf.permit",
        "@classifier(\"risk\") permit (principal is Asura::Job, action == Asura::Action::\"exec\", resource is Asura::Executable) when { context has risk && context.risk == Asura::Risk::\"safe\" };",
    );
    let lint = lint_classifier(&[(Scope::Installation, clf_permit.clone())]);
    notes.insert(
        "AQ8 seeded classifier permit lint".into(),
        format!("{lint:?}"),
    );
    let p_bad = set_of(
        ps.iter()
            .map(|(_, p)| p)
            .chain(std::iter::once(&clf_permit)),
    );
    for env in envs.iter().filter(|e| env_action(e) == "exec") {
        a.implies(
            "seeded classifier permit",
            "AQ8 seeded",
            &p_bad,
            &p0,
            env,
            false,
        )
        .await;
    }
    let repo_permit = policy(
        "seed.repo.permit",
        "@scope(\"repository\") permit (principal is Asura::Job, action == Asura::Action::\"write\", resource is Asura::Path) when { resource in Asura::Path::\"/etc\" };",
    );
    let lint = lint_lower_scopes(&[(Scope::Repository, repo_permit.clone())]);
    notes.insert(
        "AQ8 seeded repository permit lint".into(),
        format!("{lint:?}"),
    );
    let p_repo = set_of(
        ps.iter()
            .map(|(_, p)| p)
            .chain(std::iter::once(&repo_permit)),
    );
    for env in envs.iter().filter(|e| env_action(e) == "write") {
        a.implies(
            "seeded repository permit",
            "AQ8 seeded",
            &p_repo,
            &upper,
            env,
            false,
        )
        .await;
    }
    let bad_envelope = policy(
        "seed.envelope",
        &format!(
            "@envelope(\"t-studio-2\") permit (principal is Asura::Job, action == Asura::Action::\"write\", resource is Asura::Path) when {{ resource in Asura::Path::\"/Users/person/src/shared/out\" && context.now < datetime(\"{}\") }};",
            rfc3339(EXPIRY_MS)
        ),
    );
    let with = set_of(base.iter().copied().chain(std::iter::once(&bad_envelope)));
    let spec_e = envelope_spec("t-studio-2");
    let bound = set_of(base.iter().copied().chain(std::iter::once(&spec_e)));
    for env in envs.iter().filter(|e| env_action(e) == "write") {
        a.implies(
            "seeded envelope without task",
            "AQ8 seeded",
            &with,
            &bound,
            env,
            false,
        )
        .await;
    }
    notes
}

/// AQ9: one probe policy per feature, compiled and checked for never-errors
/// against a probe schema.
pub async fn coverage_probes(a: &mut Analyser) -> Vec<(String, String, f64)> {
    let schema = parse_schema(
        "namespace Probe {\n\
         entity Level enum [\"low\", \"high\"];\n\
         entity User = { level: Level, age?: Long };\n\
         entity Doc = { name: String, score: decimal, at: datetime, ip: ipaddr, labels: Set<String> } tags String;\n\
         action view appliesTo { principal: [User], resource: [Doc], context: { d: duration } };\n}",
    )
    .expect("probe schema");
    let env = schema.request_envs().next().expect("one environment");
    let head = "forbid (principal, action == Probe::Action::\"view\", resource) when ";
    let probes = [
        ("like", "{ resource.name like \"*.pem\" }"),
        (
            "ipaddr",
            "{ resource.ip.isInRange(ip(\"10.0.0.0/8\")) || resource.ip.isLoopback() }",
        ),
        (
            "datetime",
            "{ resource.at < datetime(\"2026-11-03T00:00:00Z\") }",
        ),
        (
            "duration and offset",
            "{ resource.at.offset(context.d) > datetime(\"2026-01-01\") }",
        ),
        ("decimal", "{ resource.score.lessThan(decimal(\"0.6\")) }"),
        (
            "enum entity",
            "{ principal.level == Probe::Level::\"high\" }",
        ),
        (
            "entity tags",
            "{ resource.hasTag(\"owner\") && resource.getTag(\"owner\") == \"x\" }",
        ),
        (
            "set operators",
            "{ resource.labels.containsAny([\"a\"]) && resource.labels.containsAll([\"b\"]) && !resource.labels.isEmpty() }",
        ),
        (
            "has on optional",
            "{ principal has age && principal.age > 18 }",
        ),
        (
            "in entity set",
            "{ resource in [Probe::Doc::\"a\", Probe::Doc::\"b\"] }",
        ),
    ];
    let mut out = Vec::new();
    for (name, cond) in probes {
        let p = policy(name, &format!("{head}{cond};"));
        let start = Instant::now();
        let result = match CompiledPolicy::compile(&p, &env, &schema) {
            Ok(c) => match a.compiler.check_never_errors_opt(&c).await {
                Ok(b) => format!("analysable; never_errors={b}"),
                Err(e) => format!("solver error: {e}"),
            },
            Err(e) => format!("unsupported: {e}"),
        };
        out.push((
            name.to_string(),
            result,
            start.elapsed().as_secs_f64() * 1000.0,
        ));
    }
    out
}
