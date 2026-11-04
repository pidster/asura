//! Probe SV-P1 on the product corpus (security-policy.md, PL4a
//! implementation contract, "SV-P1 product corpus"): queries PA1 to PA8 on
//! the PL1 built-in sets, against one solver command.
//!
//! ```text
//! admission-probe --schema F --installation F --user F --test-deny F -- SOLVER [ARG...]
//! ```
//!
//! Prints `result` lines (category, id, environment, result, verdict) in a
//! fixed order, one `decisions` line with the solver's answer counts, and
//! `time` and `note` lines that a comparison ignores. Exit 0: every result is
//! the expected one. Exit 1: a result is not. Exit 2: usage, unreadable input
//! or a solver that cannot start. No query has a time limit: each ends by the
//! solver's reply, and the operator's interrupt ends the run. The solver gets
//! exactly the given argument vector, so no `--tlimit` is added.

use cedar_policy::{
    ActionConstraint, Effect, Entities, EntityUid, Policy, PolicyId, PolicySet, RequestEnv, Schema,
};
use cedar_policy_symcc::solver::{Decision, DecisionWithModel, LocalSolver, Solver, SolverError};
use cedar_policy_symcc::{CedarSymCompiler, CompiledPolicy, CompiledPolicySet};
use std::collections::BTreeMap;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;
use tokio::process::Command;

/// Counts of the solver's answers, shared with the probe after the compiler
/// takes ownership of the solver.
#[derive(Default)]
struct Counts {
    sat: AtomicUsize,
    unsat: AtomicUsize,
    unknown: AtomicUsize,
}

impl Counts {
    fn record(&self, sat: bool, unsat: bool) {
        let slot = if sat {
            &self.sat
        } else if unsat {
            &self.unsat
        } else {
            &self.unknown
        };
        slot.fetch_add(1, Ordering::Relaxed);
    }
}

/// A `LocalSolver` that counts each answer it returns.
struct Counting {
    inner: LocalSolver,
    counts: Arc<Counts>,
}

impl Solver for Counting {
    fn smtlib_input(&mut self) -> &mut (dyn tokio::io::AsyncWrite + Unpin + Send) {
        self.inner.smtlib_input()
    }

    async fn enable_models(&mut self) -> Result<(), SolverError> {
        self.inner.enable_models().await
    }

    async fn check_sat(&mut self) -> Result<Decision, SolverError> {
        let d = self.inner.check_sat().await?;
        self.counts.record(d == Decision::Sat, d == Decision::Unsat);
        Ok(d)
    }

    async fn check_sat_with_model(&mut self) -> Result<DecisionWithModel, SolverError> {
        let d = self.inner.check_sat_with_model().await?;
        self.counts.record(
            matches!(d, DecisionWithModel::Sat { .. }),
            d == DecisionWithModel::Unsat,
        );
        Ok(d)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Holds,
    Refuted,
    Failed,
}

impl Outcome {
    fn name(self) -> &'static str {
        match self {
            Outcome::Holds => "holds",
            Outcome::Refuted => "refuted",
            Outcome::Failed => "failed",
        }
    }
}

struct Probe {
    schema: Schema,
    envs: Vec<RequestEnv>,
    actions: Entities,
    compiler: CedarSymCompiler<Counting>,
    unexpected: usize,
    total: usize,
}

fn env_name(e: &RequestEnv) -> String {
    format!("{}/{}/{}", e.principal(), e.action(), e.resource())
}

fn policy(id: &str, text: &str) -> Result<Policy, String> {
    Policy::parse(Some(PolicyId::new(id)), text).map_err(|e| format!("{id}: {e}"))
}

/// Parse a policy file; each policy's ID is its `@id` annotation, so that
/// two sets may hold the same policy text.
fn parse_set(name: &str, text: &str) -> Result<Vec<Policy>, String> {
    let set: PolicySet = text.parse().map_err(|e| format!("{name}: {e}"))?;
    let mut out: Vec<Policy> = Vec::new();
    for p in set.policies() {
        let id = p
            .annotation("id")
            .ok_or_else(|| format!("{name}: a policy has no @id"))?;
        out.push(p.new_id(PolicyId::new(id)));
    }
    out.sort_by_key(|a| a.id().to_string());
    Ok(out)
}

fn set_of<'a>(ps: impl IntoIterator<Item = &'a Policy>) -> Result<PolicySet, String> {
    let mut s = PolicySet::new();
    for p in ps {
        s.add(p.clone()).map_err(|e| e.to_string())?;
    }
    Ok(s)
}

impl Probe {
    /// The request environments whose action the policy's scope admits.
    fn envs_for(&self, p: &Policy) -> Vec<RequestEnv> {
        let admits = |action: &EntityUid, target: &EntityUid| {
            action == target
                || self
                    .actions
                    .ancestors(action)
                    .is_some_and(|mut a| a.any(|x| x == target))
        };
        self.envs
            .iter()
            .filter(|e| match p.action_constraint() {
                ActionConstraint::Any => true,
                ActionConstraint::Eq(t) => admits(e.action(), &t),
                ActionConstraint::In(ts) => ts.iter().any(|t| admits(e.action(), t)),
            })
            .cloned()
            .collect()
    }

    fn envs_named(&self, action: &str) -> Vec<RequestEnv> {
        self.envs
            .iter()
            .filter(|e| e.action().id().unescaped() == action)
            .cloned()
            .collect()
    }

    fn report(
        &mut self,
        category: &str,
        id: &str,
        env: &RequestEnv,
        (got, note): (Outcome, String),
        want: Outcome,
        start: Instant,
    ) {
        self.total += 1;
        let verdict = if got == want {
            "expected"
        } else {
            self.unexpected += 1;
            "UNEXPECTED"
        };
        let env = env_name(env);
        println!("result\t{category}\t{id}\t{env}\t{}\t{verdict}", got.name());
        println!(
            "time\t{category}\t{id}\t{env}\t{:.1} ms",
            start.elapsed().as_secs_f64() * 1000.0
        );
        if !note.is_empty() {
            println!("note\t{category}\t{id}\t{env}\t{note}");
        }
    }

    async fn never_errors(&mut self, category: &str, p: &Policy, want: Outcome) {
        for env in self.envs_for(p) {
            let start = Instant::now();
            let (got, note) = match CompiledPolicy::compile(p, &env, &self.schema) {
                Ok(c) => match self.compiler.check_never_errors_opt(&c).await {
                    Ok(true) => (Outcome::Holds, String::new()),
                    Ok(false) => (Outcome::Refuted, String::new()),
                    Err(e) => (Outcome::Failed, e.to_string()),
                },
                Err(e) => (Outcome::Failed, format!("compile: {e}")),
            };
            self.report(category, p.id().as_ref(), &env, (got, note), want, start);
        }
    }

    async fn implies(
        &mut self,
        category: &str,
        id: &str,
        a: &PolicySet,
        b: &PolicySet,
        envs: &[RequestEnv],
        want: Outcome,
    ) {
        for env in envs {
            let start = Instant::now();
            let (got, note) = match (
                CompiledPolicySet::compile(a, env, &self.schema),
                CompiledPolicySet::compile(b, env, &self.schema),
            ) {
                (Ok(ca), Ok(cb)) => {
                    match self
                        .compiler
                        .check_implies_with_counterexample_opt(&ca, &cb)
                        .await
                    {
                        Ok(None) => (Outcome::Holds, String::new()),
                        Ok(Some(_)) => (Outcome::Refuted, String::new()),
                        Err(e) => (Outcome::Failed, e.to_string()),
                    }
                }
                (Err(e), _) | (_, Err(e)) => (Outcome::Failed, format!("compile: {e}")),
            };
            self.report(category, id, env, (got, note), want, start);
        }
    }
}

/// S1 to S6 (security-policy.md, safety specifications), each written as
/// "permit all, then forbid the property": (id, action, forbid text).
const SAFETY: [(&str, &str, &str); 6] = [
    (
        "S1",
        "file.write",
        r#"forbid (principal is Asura::Job, action in [Asura::Action::"file.write", Asura::Action::"file.create", Asura::Action::"file.delete"], resource is Asura::Path) when { principal.origin == Asura::Origin::"model" && resource.class == Asura::PathClass::"implicit_exec" };"#,
    ),
    (
        "S2",
        "file.write",
        r#"forbid (principal is Asura::Job, action in [Asura::Action::"file.write", Asura::Action::"file.create", Asura::Action::"file.delete"], resource is Asura::Path) when { resource.class == Asura::PathClass::"asura_state" && !(resource in principal.scratch) };"#,
    ),
    (
        "S3",
        "network.connect",
        r#"forbid (principal is Asura::Job, action == Asura::Action::"network.connect", resource) when { principal.origin == Asura::Origin::"model" && !context.via_proxy };"#,
    ),
    (
        "S4",
        "secret_use",
        r#"forbid (principal is Asura::Job, action == Asura::Action::"secret_use", resource) when { principal.origin == Asura::Origin::"model" };"#,
    ),
    (
        "S5",
        "context.egress",
        r#"forbid (principal is Asura::Task, action == Asura::Action::"context.egress", resource) when { context.tainted };"#,
    ),
    (
        "S6",
        "process.exec.approved",
        r#"forbid (principal, action == Asura::Action::"process.exec.approved", resource);"#,
    ),
];

fn spec(id: &str, forbid: &str) -> Result<PolicySet, String> {
    set_of(&[
        policy("spec.all", "permit (principal, action, resource);")?,
        policy(id, forbid)?,
    ])
}

async fn run(args: Args) -> Result<bool, String> {
    let read = |p: &str| std::fs::read_to_string(p).map_err(|e| format!("{p}: {e}"));
    let (schema, _) =
        Schema::from_cedarschema_str(&read(&args.schema)?).map_err(|e| format!("schema: {e}"))?;
    let installation = parse_set("installation", &read(&args.installation)?)?;
    let user = parse_set("user", &read(&args.user)?)?;
    let test_deny = parse_set("test-deny", &read(&args.test_deny)?)?;
    // `request_envs` has no fixed order; sort so that two runs print the
    // same lines in the same order.
    let mut envs: Vec<RequestEnv> = schema.request_envs().collect();
    envs.sort_by_key(env_name);
    let actions = schema.action_entities().map_err(|e| e.to_string())?;
    let mut cmd = Command::new(&args.solver[0]);
    cmd.args(&args.solver[1..]);
    let counts = Arc::new(Counts::default());
    let inner = LocalSolver::from_command(&mut cmd).map_err(|e| format!("solver: {e}"))?;
    let compiler = CedarSymCompiler::new(Counting {
        inner,
        counts: Arc::clone(&counts),
    })
    .map_err(|e| e.to_string())?;
    let mut p = Probe {
        schema,
        envs,
        actions,
        compiler,
        unexpected: 0,
        total: 0,
    };
    println!("note\tenvironments\t{}", p.envs.len());

    // PA1: every forbid of I and T never errors.
    for f in installation
        .iter()
        .chain(test_deny.iter())
        .filter(|f| f.effect() == Effect::Forbid)
    {
        p.never_errors("PA1 never_errors", f, Outcome::Holds).await;
    }
    // PA1s: seeded forbids that can error must be refuted.
    let seeds = [
        (
            "seed.overflow",
            r#"forbid (principal is Asura::Job, action == Asura::Action::"network.connect", resource) when { context.port + 9223372036854775807 > 0 };"#,
        ),
        (
            "seed.offset",
            r#"forbid (principal is Asura::Job, action == Asura::Action::"network.connect", resource) when { context.now.offset(duration("9223372036854775807ms")) > context.now };"#,
        ),
    ];
    for (id, text) in seeds {
        p.never_errors(
            "PA1s seeded can-error",
            &policy(id, text)?,
            Outcome::Refuted,
        )
        .await;
    }

    let all = p.envs.clone();
    let i = set_of(&installation)?;
    let i0 = set_of(
        installation
            .iter()
            .filter(|x| x.annotation("intent").is_none()),
    )?;
    let it = set_of(installation.iter().chain(test_deny.iter()))?;
    let u = set_of(&user)?;
    // PA3: tighten-only (SP-AQ3).
    p.implies(
        "PA3 tighten-only",
        "I implies I0",
        &i,
        &i0,
        &all,
        Outcome::Holds,
    )
    .await;
    p.implies(
        "PA3 tighten-only",
        "I+T implies I",
        &it,
        &i,
        &all,
        Outcome::Holds,
    )
    .await;
    // PA4: safety specifications on I.
    let mut specs: BTreeMap<&str, PolicySet> = BTreeMap::new();
    for (id, action, forbid) in SAFETY {
        let s = spec(id, forbid)?;
        let envs = p.envs_named(action);
        p.implies("PA4 safety on I", id, &i, &s, &envs, Outcome::Holds)
            .await;
        specs.insert(id, s);
    }
    // PA4s: a seeded permit of every connection. With the S3 forbid the
    // installation set still meets S3; without it, S3 is refuted.
    let connect = policy(
        "seed.connect",
        r#"permit (principal is Asura::Job, action == Asura::Action::"network.connect", resource);"#,
    )?;
    let i_seed = set_of(installation.iter().chain(std::iter::once(&connect)))?;
    let i_seed_no_s3 = set_of(
        installation
            .iter()
            .filter(|x| x.id().to_string() != "s3.model_network")
            .chain(std::iter::once(&connect)),
    )?;
    let envs = p.envs_named("network.connect");
    p.implies(
        "PA4s seeded",
        "I+connect implies S3",
        &i_seed,
        &specs["S3"],
        &envs,
        Outcome::Holds,
    )
    .await;
    p.implies(
        "PA4s seeded",
        "I-s3+connect implies S3",
        &i_seed_no_s3,
        &specs["S3"],
        &envs,
        Outcome::Refuted,
    )
    .await;
    // PA8: change difference on the user set (SP-AQ8).
    let narrow = policy(
        "change.narrow",
        r#"forbid (principal is Asura::Job, action == Asura::Action::"file.write", resource is Asura::Path) when { resource.class == Asura::PathClass::"build_test" };"#,
    )?;
    let wide = policy(
        "change.wide",
        r#"permit (principal is Asura::Job, action == Asura::Action::"file.write", resource is Asura::Path) when { resource in Asura::Path::"/etc" };"#,
    )?;
    let writes = p.envs_named("file.write");
    let u_narrow = set_of(user.iter().chain(std::iter::once(&narrow)))?;
    let u_wide = set_of(user.iter().chain(std::iter::once(&wide)))?;
    p.implies(
        "PA8 change difference",
        "narrowing",
        &u_narrow,
        &u,
        &writes,
        Outcome::Holds,
    )
    .await;
    p.implies(
        "PA8 change difference",
        "widening",
        &u_wide,
        &u,
        &writes,
        Outcome::Refuted,
    )
    .await;

    println!(
        "decisions\tsat={}\tunsat={}\tunknown={}",
        counts.sat.load(Ordering::Relaxed),
        counts.unsat.load(Ordering::Relaxed),
        counts.unknown.load(Ordering::Relaxed)
    );
    println!("note\tqueries\t{}\tunexpected\t{}", p.total, p.unexpected);
    Ok(p.unexpected == 0)
}

struct Args {
    schema: String,
    installation: String,
    user: String,
    test_deny: String,
    solver: Vec<String>,
}

fn parse_args() -> Result<Args, String> {
    let mut it = std::env::args().skip(1);
    let mut named: BTreeMap<String, String> = BTreeMap::new();
    let mut solver = Vec::new();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--schema" | "--installation" | "--user" | "--test-deny" => {
                let v = it.next().ok_or_else(|| format!("{a} needs a file"))?;
                named.insert(a, v);
            }
            "--" => {
                solver = it.by_ref().collect();
            }
            _ => return Err(format!("unknown argument: {a}")),
        }
    }
    let mut take = |k: &str| named.remove(k).ok_or_else(|| format!("{k} is required"));
    let args = Args {
        schema: take("--schema")?,
        installation: take("--installation")?,
        user: take("--user")?,
        test_deny: take("--test-deny")?,
        solver,
    };
    if args.solver.is_empty() {
        return Err("give the solver command after --".into());
    }
    Ok(args)
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("admission-probe: {e}");
            return ExitCode::from(2);
        }
    };
    match run(args).await {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(1),
        Err(e) => {
            eprintln!("admission-probe: {e}");
            ExitCode::from(2)
        }
    }
}
