//! Policy admission (security-policy.md, PL3 implementation contract): the
//! admission steps of one candidate scope text, with the SymCC proofs run
//! over an injected [`SolverPipe`].
//!
//! The crate is synchronous and does no I/O of its own. The admission
//! worker of the service calls [`admit`] on its own thread; the platform's
//! solver process owner implements [`SolverPipe`]. No query has a time
//! limit: each ends on the solver's answer, the cancel flag or a pipe end.
#![forbid(unsafe_code)]

mod adapter;
mod lint;
mod prover;
mod specs;

#[cfg(test)]
mod tests;

pub use asura_policy::Scope;

use asura_policy::TextFault;
use cedar_policy::{Effect, Entities, Policy, PolicyId, PolicySet, RequestEnv, Schema};
use lint::FindingKind;
use prover::{Answer, Prover, Stop, env_action, env_name, generated, set_of};
use sha2::{Digest, Sha256};
use std::sync::atomic::AtomicBool;

/// Queries one admission may run; one more refuses `candidate_too_large`.
pub const MAX_QUERIES: usize = 4096;
/// Bytes written for one query (SA-T6).
pub const MAX_QUERY_BYTES: usize = 64 << 20;
/// Bytes of one answer line or model (SA-T4).
pub const MAX_MODEL_BYTES: usize = 1 << 20;
/// Standard error kept for diagnostics (SA-T3).
pub const MAX_STDERR_KEPT: usize = 64 << 10;
/// Counterexamples, widening examples and detail lines kept.
pub const MAX_EXAMPLES: usize = 8;
/// Bytes of one counterexample or detail line.
pub const MAX_EXAMPLE_BYTES: usize = 512;
/// Policies in one revision (admission step 1).
pub const MAX_REVISION_POLICIES: usize = 2048;

/// A solver output stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PipeStream {
    Stdout,
    Stderr,
}
/// The reader's answer to one chunk of output.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PipeFlow {
    /// The answer is not complete.
    More,
    /// The answer is complete; the exchange may end once the input is
    /// written.
    Answered,
    /// A bound was exceeded: end the exchange at once.
    Fault,
}
/// Why an exchange ended without an answer.
#[derive(Debug)]
pub enum PipeEnd {
    /// End of output or a broken pipe.
    Closed,
    Cancelled,
    MemoryBound,
    /// The output reader answered [`PipeFlow::Fault`].
    Fault,
    Io,
}
/// The byte pipe to one solver process (security-policy.md, PL3 solver
/// adapter). `exchange` writes all of `input` to the solver's standard
/// input while it passes every chunk of standard output and standard error
/// to `output`; it returns when the input is written and `output` has
/// answered [`PipeFlow::Answered`], at once on [`PipeFlow::Fault`], or with
/// a [`PipeEnd`].
pub trait SolverPipe {
    fn exchange(
        &mut self,
        input: &[u8],
        output: &mut dyn FnMut(PipeStream, &[u8]) -> PipeFlow,
    ) -> Result<(), PipeEnd>;
}

/// The result of checking one path literal against the file system.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LiteralResult {
    /// The literal exists and is spelled canonically.
    Canonical,
    /// The literal does not exist; the lexical check alone applies.
    Missing,
    /// The literal exists with another canonical spelling.
    Respell(String),
}
/// The literal check of admission step 4, injected by the service (the
/// platform canonicaliser).
pub trait LiteralCheck {
    fn check(&self, literal: &str) -> LiteralResult;
}
/// Treats every literal as missing: the lexical check only.
pub struct NoLiteralCheck;
impl LiteralCheck for NoLiteralCheck {
    fn check(&self, _: &str) -> LiteralResult {
        LiteralResult::Missing
    }
}

/// One candidate scope text.
pub struct Candidate<'a> {
    pub scope: Scope,
    pub text: &'a str,
}
/// The texts a candidate is admitted against.
pub struct Baseline<'a> {
    pub installation: &'a str,
    pub user_default: &'a str,
    /// The active text of the candidate's scope, for the change
    /// difference; `None` for a first admission of the scope.
    pub previous: Option<&'a str>,
}

/// A refusal reason code (security-policy.md, admission steps).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reason {
    CandidateTooLarge,
    CandidateParse,
    CandidateInvalid,
    CandidateLint,
    ProofFailed,
    SolverFailed,
    SolverUnknown,
    CannotEnforce,
    Cancelled,
}
impl Reason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::CandidateTooLarge => "candidate_too_large",
            Self::CandidateParse => "candidate_parse",
            Self::CandidateInvalid => "candidate_invalid",
            Self::CandidateLint => "candidate_lint",
            Self::ProofFailed => "proof_failed",
            Self::SolverFailed => "solver_failed",
            Self::SolverUnknown => "solver_unknown",
            Self::CannotEnforce => "cannot_enforce",
            Self::Cancelled => "cancelled",
        }
    }
}
/// The cause of a `solver_failed` refusal (audit `policy.solver_failed`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SolverCause {
    Unavailable,
    Closed,
    Protocol,
    InputBound,
    OutputBound,
    MemoryBound,
    Cancelled,
    Io,
}
impl SolverCause {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unavailable => "unavailable",
            Self::Closed => "closed",
            Self::Protocol => "protocol",
            Self::InputBound => "input_bound",
            Self::OutputBound => "output_bound",
            Self::MemoryBound => "memory_bound",
            Self::Cancelled => "cancelled",
            Self::Io => "io",
        }
    }
}

/// The outcome of one query.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueryOutcome {
    Holds,
    Refuted,
    Unknown,
    Failed,
}
impl QueryOutcome {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Holds => "holds",
            Self::Refuted => "refuted",
            Self::Unknown => "unknown",
            Self::Failed => "failed",
        }
    }
}
/// One query that ran, in order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QueryRecord {
    /// `SP-AQ1` to `SP-AQ9`, `compile-a` or `compile-b`.
    pub id: &'static str,
    pub environment: String,
    pub outcome: QueryOutcome,
    pub micros: u64,
}

/// The result of admission steps 1 to 7.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Steps 1 to 7 passed; the service pins the text (step 8).
    Ready,
    Rejected {
        reason: Reason,
        /// At most 8 lines of at most 512 bytes, naming `<scope>:<id>`.
        details: Vec<String>,
        /// At most 8, for the person only.
        counterexamples: Vec<String>,
        cause: Option<SolverCause>,
    },
}
/// One admission.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Admission {
    pub outcome: Outcome,
    pub queries: Vec<QueryRecord>,
    /// What the candidate newly permits: at most one counterexample per
    /// environment and 8 in all (SP-AQ8).
    pub widening: Vec<String>,
    /// The candidate does not keep everything the active text permits, so
    /// activation rechecks live decisions.
    pub narrows: bool,
    pub digest: [u8; 32],
    /// The candidate's policy IDs (scope-local `@id`), sorted.
    pub policy_ids: Vec<String>,
    /// The first 64 KiB of the solver's standard error.
    pub stderr_tail: Vec<u8>,
}

fn clip(text: String) -> String {
    let mut text = lint::printable(&text);
    if text.len() > MAX_EXAMPLE_BYTES {
        let mut end = MAX_EXAMPLE_BYTES - 3;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
        text.push_str("...");
    }
    text
}

/// The product schema, its sorted request environments and its action
/// entities.
fn schema_parts() -> Result<(Schema, Vec<RequestEnv>, Entities), String> {
    let schema = asura_policy::parse_schema()?;
    let mut envs: Vec<RequestEnv> = schema.request_envs().collect();
    envs.sort_by_key(env_name);
    let actions = schema.action_entities().map_err(|e| e.to_string())?;
    Ok((schema, envs, actions))
}

/// The failing admission step of a text and its problems.
type TextProblems = (TextFault, Vec<String>);

/// Parsed policies of one scope text, renamed `<scope>:<id>`.
fn scope_policies(
    schema: &Schema,
    scope: Scope,
    text: &str,
) -> Result<Vec<(String, Policy)>, TextProblems> {
    let set = asura_policy::check_scope_text(schema, scope, text)?;
    let mut out: Vec<(String, Policy)> = set
        .policies()
        .map(|p| {
            let local = p.annotation("id").unwrap_or("?");
            let global = format!("{}:{local}", scope.as_str());
            (global.clone(), p.new_id(PolicyId::new(&global)))
        })
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}

fn tighten_only(policy: &Policy) -> bool {
    policy.annotation("intent").is_some() || policy.annotation("classifier").is_some()
}

struct Run {
    widening: Vec<String>,
    narrows: bool,
}

/// Admits one candidate (security-policy.md, PL3 admission, steps 1 to 7).
pub fn admit(
    candidate: &Candidate<'_>,
    baseline: &Baseline<'_>,
    solver: Option<&mut dyn SolverPipe>,
    literals: &dyn LiteralCheck,
    cancel: &AtomicBool,
) -> Admission {
    let text_digest: [u8; 32] = Sha256::digest(candidate.text.as_bytes()).into();
    let mut admission = Admission {
        outcome: Outcome::Ready,
        queries: Vec::new(),
        widening: Vec::new(),
        narrows: true,
        digest: [0; 32],
        policy_ids: Vec::new(),
        stderr_tail: Vec::new(),
    };
    let reject = |reason, details: Vec<String>, examples: Vec<String>, cause| Outcome::Rejected {
        reason,
        details: details.into_iter().take(MAX_EXAMPLES).map(clip).collect(),
        counterexamples: examples.into_iter().take(MAX_EXAMPLES).map(clip).collect(),
        cause,
    };
    let (schema, envs, actions) = match schema_parts() {
        Ok(parts) => parts,
        Err(e) => {
            admission.outcome = reject(Reason::CandidateInvalid, vec![e], Vec::new(), None);
            return finish(admission, &text_digest);
        }
    };
    // Steps 1 to 3.
    if candidate.text.len() > asura_policy::MAX_SCOPE_TEXT_BYTES {
        admission.outcome = reject(
            Reason::CandidateTooLarge,
            vec![format!("{}: candidate_too_large", candidate.scope.as_str())],
            Vec::new(),
            None,
        );
        return finish(admission, &text_digest);
    }
    let policies = match scope_policies(&schema, candidate.scope, candidate.text) {
        Ok(p) => p,
        Err((fault, messages)) => {
            let reason = match fault {
                TextFault::TooLarge => Reason::CandidateTooLarge,
                TextFault::Parse => Reason::CandidateParse,
                TextFault::Invalid => Reason::CandidateInvalid,
            };
            admission.outcome = reject(reason, messages, Vec::new(), None);
            return finish(admission, &text_digest);
        }
    };
    admission.policy_ids = policies
        .iter()
        .map(|(id, _)| id.rsplit(':').next().unwrap_or(id).to_owned())
        .collect();
    let lower = matches!(candidate.scope, Scope::Workspace | Scope::Repository);
    if candidate.scope != Scope::Installation {
        let installation = baseline
            .installation
            .parse::<PolicySet>()
            .map_or(0, |s| s.policies().count());
        if installation + policies.len() + usize::from(lower) > MAX_REVISION_POLICIES {
            admission.outcome = reject(
                Reason::CandidateTooLarge,
                vec![format!(
                    "the revision would hold more than {MAX_REVISION_POLICIES} policies"
                )],
                Vec::new(),
                None,
            );
            return finish(admission, &text_digest);
        }
    }
    // Step 4.
    let findings = lint::lint(candidate.scope, &policies, literals);
    let author: Vec<Policy> = policies.iter().map(|(_, p)| p.clone()).collect();
    let mut p_all = author.clone();
    if lower {
        p_all.push(generated(
            &format!("{}:baseline", candidate.scope.as_str()),
            specs::BASELINE,
        ));
    }
    let Some(solver) = solver else {
        admission.outcome = if findings.is_empty() {
            reject(
                Reason::SolverFailed,
                vec!["no solver is available".into()],
                Vec::new(),
                Some(SolverCause::Unavailable),
            )
        } else {
            lint_rejection(&findings, Vec::new())
        };
        return finish(admission, &text_digest);
    };
    let cancel_ref = cancel;
    let mut prover = Prover::new(solver, &schema, &envs, &actions, cancel_ref);
    if !findings.is_empty() {
        // Shown by a counterexample when a proof can find one (PES-M16,
        // PES-M17); the candidate is refused either way.
        let mut examples = Vec::new();
        let p = set_of(&p_all);
        if findings
            .iter()
            .any(|f| f.kind == FindingKind::TightenPermit)
        {
            let p0 = set_of(author.iter().filter(|x| !tighten_only(x)));
            examples.extend(first_refutation(&mut prover, "SP-AQ3", &p, &p0));
        }
        if findings.iter().any(|f| f.kind == FindingKind::LowerPermit) {
            let own = set_of(&author);
            for env in &envs {
                if let Ok(Answer::Refuted(Some(e))) = prover.always_denies("SP-AQ5", &own, env) {
                    examples.push(e);
                    break;
                }
            }
        }
        admission.outcome = lint_rejection(&findings, examples);
        return finish_with(admission, &text_digest, prover);
    }
    let reference = if lower {
        vec![generated("baseline", specs::BASELINE)]
    } else {
        match scope_policies(&schema, Scope::User, baseline.user_default) {
            Ok(p) => p.into_iter().map(|(_, p)| p).collect(),
            Err((_, messages)) => {
                admission.outcome = reject(Reason::CandidateInvalid, messages, Vec::new(), None);
                return finish_with(admission, &text_digest, prover);
            }
        }
    };
    let previous = baseline.previous.and_then(|text| {
        scope_policies(&schema, candidate.scope, text)
            .ok()
            .map(|mut p| {
                if lower {
                    p.push((
                        format!("{}:baseline", candidate.scope.as_str()),
                        generated(
                            &format!("{}:baseline", candidate.scope.as_str()),
                            specs::BASELINE,
                        ),
                    ));
                }
                p.into_iter().map(|(_, p)| p).collect::<Vec<_>>()
            })
    });
    match prove_candidate(
        &mut prover,
        candidate.scope,
        &author,
        &p_all,
        &reference,
        previous.as_deref(),
    ) {
        Ok(run) => {
            admission.widening = run.widening;
            admission.narrows = run.narrows;
        }
        Err((stop, examples)) => {
            admission.outcome = reject(stop.reason, vec![stop.detail], examples, stop.cause);
        }
    }
    finish_with(admission, &text_digest, prover)
}

fn first_refutation(
    prover: &mut Prover<'_, '_>,
    id: &'static str,
    a: &PolicySet,
    b: &PolicySet,
) -> Option<String> {
    for env in prover.envs.iter() {
        match prover.implies(id, a, b, env) {
            Ok(Answer::Refuted(e)) => return e,
            Ok(Answer::Holds) => {}
            Err(_) => return None,
        }
    }
    None
}

fn lint_rejection(findings: &[lint::Finding], examples: Vec<String>) -> Outcome {
    Outcome::Rejected {
        reason: Reason::CandidateLint,
        details: findings
            .iter()
            .take(MAX_EXAMPLES)
            .map(|f| clip(format!("{}: {}", f.rule, f.message)))
            .collect(),
        counterexamples: examples.into_iter().take(MAX_EXAMPLES).map(clip).collect(),
        cause: None,
    }
}

type Refusal = (Stop, Vec<String>);

fn refused(reason: Reason, detail: String, example: Option<String>) -> Refusal {
    (
        Stop {
            reason,
            cause: None,
            detail,
        },
        example.into_iter().collect(),
    )
}

/// Steps 5 to 7 for a candidate scope set.
fn prove_candidate(
    prover: &mut Prover<'_, '_>,
    scope: Scope,
    author: &[Policy],
    p_all: &[Policy],
    reference: &[Policy],
    previous: Option<&[Policy]>,
) -> Result<Run, Refusal> {
    let p = set_of(p_all);
    // SP-AQ1: every forbid never errors.
    for policy in author.iter().filter(|x| x.effect() == Effect::Forbid) {
        for env in prover.envs_for(policy) {
            if let Answer::Refuted(e) = prover.never_errors(policy, &env).map_err(no_example)? {
                return Err(refused(
                    Reason::ProofFailed,
                    format!(
                        "{}: this forbid can error ({})",
                        policy.id(),
                        env_name(&env)
                    ),
                    e,
                ));
            }
        }
    }
    // SP-AQ3: CL-1, when the scope has tighten-only policies.
    if author.iter().any(tighten_only) {
        let p0 = set_of(p_all.iter().filter(|x| !tighten_only(x)));
        for env in prover.envs.iter() {
            if let Answer::Refuted(e) =
                prover.implies("SP-AQ3", &p, &p0, env).map_err(no_example)?
            {
                return Err(refused(
                    Reason::ProofFailed,
                    format!(
                        "{}: tighten-only policies permit more (CL-1)",
                        scope.as_str()
                    ),
                    e,
                ));
            }
        }
    }
    // SP-AQ9: person network for every host, address and port (AD-7).
    ad7(prover, &p, scope)?;
    // Step 6: the PL3 compile checks.
    compile_regions(prover, &p, scope)?;
    let reference_set = |extra: Option<&Policy>| {
        let mut r: Vec<Policy> = reference.to_vec();
        r.extend(extra.cloned());
        set_of(&r)
    };
    for env in prover.envs.iter() {
        let Some(restriction) = specs::restriction(&env_action(env)) else {
            continue;
        };
        let x = restriction.map(|text| generated("compile.restriction", &text));
        let mut a: Vec<Policy> = p_all.to_vec();
        a.extend(x.clone());
        let (a, b) = (set_of(&a), reference_set(x.as_ref()));
        if let Answer::Refuted(e) = prover
            .equivalent("compile-b", &a, &b, env)
            .map_err(no_example)?
        {
            return Err(refused(
                Reason::CannotEnforce,
                format!(
                    "{}: PL3 does not enforce this effect ({}); keep it as the default user set has it",
                    scope.as_str(),
                    env_action(env)
                ),
                e,
            ));
        }
    }
    // Step 7: the change difference, both ways.
    let mut run = Run {
        widening: Vec::new(),
        narrows: previous.is_none(),
    };
    if let Some(previous) = previous {
        let old = set_of(previous);
        for env in prover.envs.iter() {
            if let Answer::Refuted(e) = prover
                .implies("SP-AQ8", &p, &old, env)
                .map_err(no_example)?
                && run.widening.len() < MAX_EXAMPLES
            {
                run.widening.extend(e.map(clip));
            }
            if let Answer::Refuted(_) = prover
                .implies("SP-AQ8", &old, &p, env)
                .map_err(no_example)?
            {
                run.narrows = true;
            }
        }
    }
    Ok(run)
}

fn no_example(stop: Stop) -> Refusal {
    (stop, Vec::new())
}

fn ad7(prover: &mut Prover<'_, '_>, p: &PolicySet, scope: Scope) -> Result<(), Refusal> {
    for profile in specs::PERSON_PROFILES {
        let spec = set_of([&generated("spec.ad7", &specs::person_network(profile))]);
        for env in prover.envs_named("network.connect") {
            if let Answer::Refuted(e) = prover
                .implies("SP-AQ9", &spec, p, &env)
                .map_err(no_example)?
            {
                return Err(refused(
                    Reason::ProofFailed,
                    format!(
                        "{}: person network forbidden (AD-7) for {profile}",
                        scope.as_str()
                    ),
                    e,
                ));
            }
        }
    }
    Ok(())
}

fn compile_regions(
    prover: &mut Prover<'_, '_>,
    p: &PolicySet,
    scope: Scope,
) -> Result<(), Refusal> {
    for region in specs::regions() {
        let Some(env) = prover.envs_named(region.action).into_iter().next() else {
            continue;
        };
        let full = set_of([&generated("spec.region", &region.full)]);
        match prover
            .implies("compile-a", &full, p, &env)
            .map_err(no_example)?
        {
            Answer::Holds => {}
            Answer::Refuted(example) => {
                let root = set_of([&generated("spec.root", &region.root)]);
                if let Answer::Refuted(_) = prover
                    .disjoint("compile-a", &root, p, &env)
                    .map_err(no_example)?
                {
                    return Err(refused(
                        Reason::CannotEnforce,
                        format!(
                            "{}: the model-1 profile cannot enforce part of the {} region",
                            scope.as_str(),
                            region.name
                        ),
                        example,
                    ));
                }
            }
        }
    }
    Ok(())
}

fn finish_with(
    mut admission: Admission,
    text_digest: &[u8; 32],
    prover: Prover<'_, '_>,
) -> Admission {
    admission.stderr_tail = prover.adapter().stderr.clone();
    admission.queries = prover.records;
    finish(admission, text_digest)
}

/// The admission digest: SHA-256 of a tag, the text digest and every
/// query's ID, environment and outcome in the order run.
fn finish(mut admission: Admission, text_digest: &[u8; 32]) -> Admission {
    let mut hash = Sha256::new();
    hash.update(b"asura-policy-admission/1\0");
    hash.update(text_digest);
    for q in &admission.queries {
        hash.update(q.id.as_bytes());
        hash.update([0]);
        hash.update(q.environment.as_bytes());
        hash.update([0]);
        hash.update(q.outcome.as_str().as_bytes());
        hash.update([0]);
    }
    admission.digest = hash.finalize().into();
    admission
}

/// Release-gate proofs of the built-in sets (PL-I6): SP-AQ1, SP-AQ3,
/// SP-AQ4 (S1 to S6), SP-AQ9 and compile check (a) on the installation
/// set, and SP-AQ1, SP-AQ9 and compile check (a) on the default user set.
/// The run stops at the first refuted or failed query; a stop that is not
/// a query outcome adds one `Failed` record named `stop`.
pub fn prove_builtin(
    installation: &str,
    user_default: &str,
    solver: &mut dyn SolverPipe,
    cancel: &AtomicBool,
) -> Vec<QueryRecord> {
    let Ok((schema, envs, actions)) = schema_parts() else {
        return Vec::new();
    };
    let (Ok(i), Ok(u)) = (
        scope_policies(&schema, Scope::Installation, installation),
        scope_policies(&schema, Scope::User, user_default),
    ) else {
        return Vec::new();
    };
    let i: Vec<Policy> = i.into_iter().map(|(_, p)| p).collect();
    let u: Vec<Policy> = u.into_iter().map(|(_, p)| p).collect();
    let mut prover = Prover::new(solver, &schema, &envs, &actions, cancel);
    if let Err(stop) = builtin_run(&mut prover, &i, &u) {
        let last_failed = prover
            .records
            .last()
            .is_some_and(|r| r.outcome != QueryOutcome::Holds);
        if !last_failed {
            prover.records.push(QueryRecord {
                id: "stop",
                environment: stop.detail,
                outcome: QueryOutcome::Failed,
                micros: 0,
            });
        }
    }
    prover.records
}

fn builtin_run(prover: &mut Prover<'_, '_>, i: &[Policy], u: &[Policy]) -> Result<(), Stop> {
    for (scope, set) in [(Scope::Installation, i), (Scope::User, u)] {
        let p = set_of(set);
        for policy in set.iter().filter(|x| x.effect() == Effect::Forbid) {
            for env in prover.envs_for(policy) {
                prover.never_errors(policy, &env)?;
            }
        }
        if set.iter().any(tighten_only) {
            let p0 = set_of(set.iter().filter(|x| !tighten_only(x)));
            for env in prover.envs.iter() {
                prover.implies("SP-AQ3", &p, &p0, env)?;
            }
        }
        if scope == Scope::Installation {
            let all = generated("spec.all", "permit (principal, action, resource);");
            for (id, _, forbid) in specs::SAFETY {
                let spec = set_of([&all, &generated(&format!("spec.{id}"), forbid)]);
                for env in prover.envs.iter() {
                    prover.implies("SP-AQ4", &p, &spec, env)?;
                }
            }
        }
        ad7(prover, &p, scope).map_err(|(stop, _)| stop)?;
        compile_regions(prover, &p, scope).map_err(|(stop, _)| stop)?;
    }
    Ok(())
}
