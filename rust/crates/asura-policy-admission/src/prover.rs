//! The proof queries of one admission over SymCC and the adapter. Each
//! query runs in one request environment with no time limit; it ends on the
//! solver's answer, the cancel flag, a pipe end or the query bound.
use crate::adapter::{Adapter, block_on};
use crate::lint::printable;
use crate::{
    MAX_EXAMPLE_BYTES, MAX_QUERIES, QueryOutcome, QueryRecord, Reason, SolverCause, SolverPipe,
};
use cedar_policy::{
    ActionConstraint, Entities, EntityUid, Policy, PolicyId, PolicySet, RequestEnv, Schema,
};
use cedar_policy_symcc::err::Error as SymError;
use cedar_policy_symcc::{CedarSymCompiler, CompiledPolicy, CompiledPolicySet, Env};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

/// Why a run of queries stopped before its end.
#[derive(Debug)]
pub(crate) struct Stop {
    pub reason: Reason,
    pub cause: Option<SolverCause>,
    pub detail: String,
}

/// A query's answer: holds, or refuted with a counterexample when the
/// query asks for one.
pub(crate) enum Answer {
    Holds,
    Refuted(Option<String>),
}

pub(crate) struct Prover<'a, 'p> {
    compiler: CedarSymCompiler<Adapter<'p>>,
    pub schema: &'a Schema,
    pub envs: &'a [RequestEnv],
    actions: &'a Entities,
    cancel: &'a AtomicBool,
    pub records: Vec<QueryRecord>,
}

/// The display name of a request environment.
pub(crate) fn env_name(env: &RequestEnv) -> String {
    format!("{}/{}/{}", env.principal(), env.action(), env.resource())
}

/// The action name of an environment, without its type.
pub(crate) fn env_action(env: &RequestEnv) -> String {
    env.action().id().unescaped().to_owned()
}

/// A counterexample as printable ASCII, at most 512 bytes.
pub(crate) fn render(env: &Env) -> String {
    let text = format!("{env}");
    let mut flat = String::new();
    for word in text.split_whitespace() {
        if !flat.is_empty() {
            flat.push(' ');
        }
        flat.push_str(word);
    }
    let mut out = printable(&flat);
    if out.len() > MAX_EXAMPLE_BYTES {
        let mut end = MAX_EXAMPLE_BYTES - 3;
        while !out.is_char_boundary(end) {
            end -= 1;
        }
        out.truncate(end);
        out.push_str("...");
    }
    out
}

impl<'a, 'p> Prover<'a, 'p> {
    pub(crate) fn new(
        pipe: &'p mut dyn SolverPipe,
        schema: &'a Schema,
        envs: &'a [RequestEnv],
        actions: &'a Entities,
        cancel: &'a AtomicBool,
    ) -> Self {
        Self {
            compiler: CedarSymCompiler::new(Adapter::new(pipe))
                .expect("CedarSymCompiler::new never fails"),
            schema,
            envs,
            actions,
            cancel,
            records: Vec::new(),
        }
    }
    pub(crate) fn adapter(&self) -> &Adapter<'p> {
        self.compiler.solver()
    }
    /// The environments `policy`'s action scope admits.
    pub(crate) fn envs_for(&self, policy: &Policy) -> Vec<RequestEnv> {
        let admits = |action: &EntityUid, target: &EntityUid| {
            action == target
                || self
                    .actions
                    .ancestors(action)
                    .is_some_and(|mut a| a.any(|x| x == target))
        };
        self.envs
            .iter()
            .filter(|e| match policy.action_constraint() {
                ActionConstraint::Any => true,
                ActionConstraint::Eq(t) => admits(e.action(), &t),
                ActionConstraint::In(ts) => ts.iter().any(|t| admits(e.action(), t)),
            })
            .cloned()
            .collect()
    }
    pub(crate) fn envs_named(&self, action: &str) -> Vec<RequestEnv> {
        self.envs
            .iter()
            .filter(|e| env_action(e) == action)
            .cloned()
            .collect()
    }
    fn begin(&self) -> Result<Instant, Stop> {
        if self.cancel.load(Ordering::Acquire) {
            return Err(Stop {
                reason: Reason::Cancelled,
                cause: Some(SolverCause::Cancelled),
                detail: "cancelled".into(),
            });
        }
        if self.records.len() >= MAX_QUERIES {
            return Err(Stop {
                reason: Reason::CandidateTooLarge,
                cause: None,
                detail: format!("more than {MAX_QUERIES} proof queries"),
            });
        }
        Ok(Instant::now())
    }
    /// Maps a SymCC failure: a solver fault keeps its cause; `unknown` is
    /// `solver_unknown`; anything else is an analysis failure.
    fn failure(&self, error: Option<SymError>) -> Stop {
        let adapter = self.compiler.solver();
        let cause = adapter.cause.or_else(|| {
            if adapter.overflowed() {
                Some(SolverCause::InputBound)
            } else {
                matches!(&error, Some(SymError::SolverError(_)) | None)
                    .then_some(SolverCause::Protocol)
            }
        });
        match (error, cause) {
            (Some(SymError::SolverUnknown), None) => Stop {
                reason: Reason::SolverUnknown,
                cause: None,
                detail: "the solver answered unknown".into(),
            },
            (_, Some(SolverCause::Cancelled)) => Stop {
                reason: Reason::Cancelled,
                cause: Some(SolverCause::Cancelled),
                detail: "cancelled".into(),
            },
            (_, Some(cause)) => Stop {
                reason: Reason::SolverFailed,
                cause: Some(cause),
                detail: format!("solver failed: {}", cause.as_str()),
            },
            (Some(error), None) => Stop {
                reason: Reason::ProofFailed,
                cause: None,
                detail: printable(&format!("analysis failed: {error}")),
            },
            (None, None) => Stop {
                reason: Reason::SolverFailed,
                cause: Some(SolverCause::Protocol),
                detail: "solver failed: protocol".into(),
            },
        }
    }
    fn record(&mut self, id: &'static str, env: &RequestEnv, outcome: QueryOutcome, at: Instant) {
        self.records.push(QueryRecord {
            id,
            environment: env_name(env),
            outcome,
            micros: u64::try_from(at.elapsed().as_micros()).unwrap_or(u64::MAX),
        });
    }
    fn finish(
        &mut self,
        id: &'static str,
        env: &RequestEnv,
        at: Instant,
        result: Option<Result<Option<Option<String>>, Box<SymError>>>,
    ) -> Result<Answer, Stop> {
        // Outer `None`: the future was not ready (a protocol fault).
        // `Ok(None)`: holds. `Ok(Some(example))`: refuted.
        match result {
            Some(Ok(None)) => {
                self.record(id, env, QueryOutcome::Holds, at);
                Ok(Answer::Holds)
            }
            Some(Ok(Some(example))) => {
                self.record(id, env, QueryOutcome::Refuted, at);
                Ok(Answer::Refuted(example))
            }
            Some(Err(error)) => {
                let unknown = matches!(*error, SymError::SolverUnknown);
                self.record(
                    id,
                    env,
                    if unknown {
                        QueryOutcome::Unknown
                    } else {
                        QueryOutcome::Failed
                    },
                    at,
                );
                Err(self.failure(Some(*error)))
            }
            None => {
                self.record(id, env, QueryOutcome::Failed, at);
                Err(self.failure(None))
            }
        }
    }
    fn compile_set(&self, set: &PolicySet, env: &RequestEnv) -> Result<CompiledPolicySet, Stop> {
        CompiledPolicySet::compile(set, env, self.schema).map_err(|e| Stop {
            reason: Reason::ProofFailed,
            cause: None,
            detail: printable(&format!("analysis failed in {}: {e}", env_name(env))),
        })
    }
    /// SP-AQ1 for one forbid in one environment.
    pub(crate) fn never_errors(
        &mut self,
        policy: &Policy,
        env: &RequestEnv,
    ) -> Result<Answer, Stop> {
        let at = self.begin()?;
        let compiled = CompiledPolicy::compile(policy, env, self.schema).map_err(|e| Stop {
            reason: Reason::ProofFailed,
            cause: None,
            detail: printable(&format!("analysis failed in {}: {e}", env_name(env))),
        })?;
        let result = block_on(
            self.compiler
                .check_never_errors_with_counterexample_opt(&compiled),
        )
        .map(|r| r.map_err(Box::new).map(|c| c.map(|e| Some(render(&e)))));
        self.finish("SP-AQ1", env, at, result)
    }
    /// `check_implies(a, b)` in one environment, with a counterexample.
    pub(crate) fn implies(
        &mut self,
        id: &'static str,
        a: &PolicySet,
        b: &PolicySet,
        env: &RequestEnv,
    ) -> Result<Answer, Stop> {
        let at = self.begin()?;
        let (ca, cb) = (self.compile_set(a, env)?, self.compile_set(b, env)?);
        let result = block_on(
            self.compiler
                .check_implies_with_counterexample_opt(&ca, &cb),
        )
        .map(|r| r.map_err(Box::new).map(|c| c.map(|e| Some(render(&e)))));
        self.finish(id, env, at, result)
    }
    /// `check_disjoint(a, b)` in one environment, no counterexample.
    pub(crate) fn disjoint(
        &mut self,
        id: &'static str,
        a: &PolicySet,
        b: &PolicySet,
        env: &RequestEnv,
    ) -> Result<Answer, Stop> {
        let at = self.begin()?;
        let (ca, cb) = (self.compile_set(a, env)?, self.compile_set(b, env)?);
        let result = block_on(self.compiler.check_disjoint_opt(&ca, &cb))
            .map(|r| r.map_err(Box::new).map(|holds| (!holds).then_some(None)));
        self.finish(id, env, at, result)
    }
    /// `check_equivalent(a, b)` in one environment, with a counterexample.
    pub(crate) fn equivalent(
        &mut self,
        id: &'static str,
        a: &PolicySet,
        b: &PolicySet,
        env: &RequestEnv,
    ) -> Result<Answer, Stop> {
        let at = self.begin()?;
        let (ca, cb) = (self.compile_set(a, env)?, self.compile_set(b, env)?);
        let result = block_on(
            self.compiler
                .check_equivalent_with_counterexample_opt(&ca, &cb),
        )
        .map(|r| r.map_err(Box::new).map(|c| c.map(|e| Some(render(&e)))));
        self.finish(id, env, at, result)
    }
    /// `check_always_denies(a)` in one environment, with a counterexample.
    pub(crate) fn always_denies(
        &mut self,
        id: &'static str,
        a: &PolicySet,
        env: &RequestEnv,
    ) -> Result<Answer, Stop> {
        let at = self.begin()?;
        let ca = self.compile_set(a, env)?;
        let result = block_on(
            self.compiler
                .check_always_denies_with_counterexample_opt(&ca),
        )
        .map(|r| r.map_err(Box::new).map(|c| c.map(|e| Some(render(&e)))));
        self.finish(id, env, at, result)
    }
}

/// A policy parsed from generated text with the given ID.
pub(crate) fn generated(id: &str, text: &str) -> Policy {
    Policy::parse(Some(PolicyId::new(id)), text).expect("generated policy text parses")
}

/// A policy set from policies; IDs are unique by construction.
pub(crate) fn set_of<'x>(policies: impl IntoIterator<Item = &'x Policy>) -> PolicySet {
    let mut set = PolicySet::new();
    for p in policies {
        set.add(p.clone()).expect("unique policy IDs");
    }
    set
}
