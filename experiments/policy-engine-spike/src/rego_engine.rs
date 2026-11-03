//! Fail-closed Rego wrapper over `regorus` (design: "Fail-closed wrapper
//! contract"). Each worker owns a clone of one prepared engine.

use crate::cedar_engine::Mode;
use crate::corpus::{Scope, Source};
use crate::decision::{Decision, ErrorClass, check_bounds};
use crate::model::Request;
use crate::store::Store;
use regorus::{Engine, Value as RegoValue};
use serde_json::Value;
use std::collections::BTreeSet;

pub const DECISION_RULE: &str = "data.asura.decision.result";

#[derive(Clone)]
pub struct RegoEngine {
    engine: Engine,
}

/// Admission check: a module from a scope must declare exactly that scope's
/// package. Without it a repository module could declare
/// `package asura.installation` and add a permit.
fn expected_package(scope: Scope) -> String {
    format!("data.asura.{}", scope.name())
}

impl RegoEngine {
    /// Build an engine from the helper, the combiner, the scope modules and
    /// the data document. Any failure refuses the whole bundle.
    pub fn load(
        lib: &str,
        decision: &str,
        sources: &[Source],
        store: &Store,
    ) -> Result<Self, Vec<String>> {
        Self::load_data(lib, decision, sources, store.to_rego_data())
    }

    pub fn load_data(
        lib: &str,
        decision: &str,
        sources: &[Source],
        data: Value,
    ) -> Result<Self, Vec<String>> {
        let mut engine = Engine::new();
        engine.set_strict_builtin_errors(true);
        let mut errors = Vec::new();
        for (name, text, want) in [
            ("rego/lib.rego", lib, "data.asura.lib".to_string()),
            (
                "rego/decision.rego",
                decision,
                "data.asura.decision".to_string(),
            ),
        ] {
            match engine.add_policy(name.into(), text.into()) {
                Ok(pkg) if pkg == want => {}
                Ok(pkg) => errors.push(format!("{name}: package {pkg}, expected {want}")),
                Err(e) => errors.push(format!("{name}: {e}")),
            }
        }
        for s in sources {
            match engine.add_policy(s.name.clone(), s.text.clone()) {
                Ok(pkg) if pkg == expected_package(s.scope) => {}
                Ok(pkg) => errors.push(format!(
                    "{}: package {pkg}, expected {}",
                    s.name,
                    expected_package(s.scope)
                )),
                Err(e) => errors.push(format!("{}: {e}", s.name)),
            }
        }
        if let Err(e) = engine.add_data(RegoValue::from(data)) {
            errors.push(format!("data: {e}"));
        }
        if !errors.is_empty() {
            return Err(errors);
        }
        let mut me = RegoEngine { engine };
        // Prepare the engine once, so worker clones start prepared.
        me.engine.set_input(RegoValue::new_object());
        if let Err(e) = me.engine.eval_rule(DECISION_RULE.into()) {
            return Err(vec![format!("prepare: {e}")]);
        }
        Ok(me)
    }

    pub fn decide(&mut self, req: &Request) -> Decision {
        self.decide_json(&req.to_json(), Mode::Wrapper)
    }

    /// Decide from the engine-facing JSON input (the fuzzer mutates it).
    pub fn decide_json(&mut self, input: &Value, mode: Mode) -> Decision {
        if mode == Mode::Wrapper {
            let bytes = serde_json::to_vec(input).map_or(usize::MAX, |b| b.len());
            if check_bounds(input, bytes).is_err() {
                return Decision::deny_error(ErrorClass::RequestTooLarge, BTreeSet::new());
            }
            if Request::from_json(input).is_err() {
                return Decision::deny_error(ErrorClass::RequestInvalid, BTreeSet::new());
            }
        }
        self.engine.set_input(RegoValue::from(input.clone()));
        let result = match self.engine.eval_rule(DECISION_RULE.into()) {
            Ok(v) => v,
            Err(_) => return Decision::deny_error(ErrorClass::EngineError, BTreeSet::new()),
        };
        if mode == Mode::Raw {
            return Decision {
                allow: result["allow"] == RegoValue::Bool(true),
                rules: BTreeSet::new(),
                reasons: BTreeSet::new(),
                error: None,
            };
        }
        parse_result(&result)
    }

    /// Evaluate an arbitrary query, for the built-in probe.
    pub fn eval_query(&mut self, query: &str) -> Result<String, String> {
        self.engine
            .eval_query(query.into(), false)
            .map(|r| format!("{r:?}"))
            .map_err(|e| e.to_string())
    }

    pub fn add_policy(&mut self, name: &str, text: &str) -> Result<String, String> {
        self.engine
            .add_policy(name.into(), text.into())
            .map_err(|e| e.to_string())
    }
}

/// W4 to W6: a well-formed result or a deny.
fn parse_result(v: &RegoValue) -> Decision {
    let invalid = || Decision::deny_error(ErrorClass::ResultInvalid, BTreeSet::new());
    let allow = match &v["allow"] {
        RegoValue::Bool(b) => *b,
        _ => return invalid(),
    };
    let Ok(set) = v["rules"].as_set() else {
        return invalid();
    };
    let mut rules = BTreeSet::new();
    let mut reasons = BTreeSet::new();
    for r in set.iter() {
        let (Ok(id), Ok(reason)) = (r["id"].as_string(), r["reason"].as_string()) else {
            return invalid();
        };
        rules.insert(id.to_string());
        reasons.insert(reason.to_string());
    }
    if allow && rules.is_empty() {
        return invalid();
    }
    if !allow && rules.is_empty() {
        return Decision::no_permit();
    }
    Decision {
        allow,
        rules,
        reasons,
        error: None,
    }
}
