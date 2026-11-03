//! Fail-closed Cedar wrapper (design: "Fail-closed wrapper contract").

use crate::corpus::{Scope, Source};
use crate::decision::{Decision, ErrorClass, check_bounds};
use crate::model::{Context as Ctx, Request, rfc3339};
use crate::store::Store;
use cedar_policy::{
    AuthorizationError, Authorizer, Context, Decision as CedarDecision, Effect, Entities,
    EntityUid, Policy, PolicyId, PolicySet, Request as CedarRequest, Schema, ValidationMode,
    Validator,
};
use serde_json::{Value, json};
use std::collections::{BTreeSet, HashMap};
use std::str::FromStr;
use std::sync::Arc;

/// Scope composition (design: "Scope composition").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pattern {
    /// One merged set; workspace and repository are forbid-only.
    B,
    /// Authority set AND workspace set AND repository set.
    A,
    /// Diagnostic variant of B, not a designed pattern: the merged set
    /// pre-partitioned by action. Exact, because Cedar never evaluates the
    /// condition of a policy whose action scope does not match.
    BSliced,
}

/// Wrapper mode for the fuzz comparison.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// The full contract: W1 to W6.
    Wrapper,
    /// Cedar alone: no schema on the request, errors ignored.
    Raw,
}

pub struct CedarEngine {
    pub schema: Schema,
    pub merged: PolicySet,
    authority: PolicySet,
    workspace: PolicySet,
    repository: PolicySet,
    pub entities: Arc<Entities>,
    sliced: HashMap<&'static str, PolicySet>,
    reasons: HashMap<PolicyId, String>,
    actions: HashMap<&'static str, EntityUid>,
    authorizer: Authorizer,
}

const ACTIONS: [&str; 7] = [
    "exec",
    "exec_request_approval",
    "exec_approved",
    "write",
    "connect",
    "secret_use",
    "egress",
];

fn uid(t: &str, id: &str) -> Value {
    json!({"__entity": {"type": format!("Asura::{t}"), "id": id}})
}

fn extn(f: &str, arg: &str) -> Value {
    json!({"__extn": {"fn": f, "arg": arg}})
}

/// Encode the typed request's context as Cedar JSON.
pub fn cedar_context(req: &Request) -> Value {
    let now = extn("datetime", &rfc3339(req.context.now_ms()));
    match &req.context {
        Ctx::Exec(c) => {
            let mut v = json!({
                "now": now,
                "request_digest": c.request_digest,
                "outside_switch": c.outside_switch,
                "command": {
                    "parse": uid("Parse", c.command.parse.as_str()),
                    "subcommand": c.command.subcommand,
                    "flags": c.command.flags,
                    "shell_features": c.command.shell_features,
                    "env_terminal": c.command.env_terminal,
                    "env_declared": c.command.env_declared,
                    "secret_vars": c.command.secret_vars,
                    "secret_reference": c.command.secret_reference,
                },
            });
            if let Some(r) = c.risk {
                v["risk"] = uid("Risk", r.as_str());
            }
            v
        }
        Ctx::Write(_) => json!({"now": now}),
        Ctx::Connect(c) => json!({
            "now": now, "ip": extn("ip", &c.ip), "port": c.port, "via_proxy": c.via_proxy,
        }),
        Ctx::SecretUse(c) => json!({"now": now, "delivery": uid("Delivery", c.delivery.as_str())}),
        Ctx::Egress(c) => json!({
            "now": now, "data_class": uid("DataClass", c.data_class.as_str()), "tainted": c.tainted,
        }),
    }
}

/// Engine-facing Cedar parts of a typed request: principal UID JSON,
/// action name, resource UID JSON and context JSON.
pub fn cedar_parts(req: &Request) -> (Value, &'static str, Value, Value) {
    (
        json!({"type": format!("Asura::{}", req.principal.kind.as_str()), "id": req.principal.id}),
        req.action.as_str(),
        json!({"type": format!("Asura::{}", req.resource.kind.as_str()), "id": req.resource.id}),
        cedar_context(req),
    )
}

/// Parse every source, rename each policy to its `@id`, and check that its
/// `@scope` matches the source. Returns (scope, policy) pairs.
pub fn parse_sources(sources: &[Source]) -> Result<Vec<(Scope, Policy)>, Vec<String>> {
    let mut out = Vec::new();
    let mut errors = Vec::new();
    for s in sources {
        match PolicySet::from_str(&s.text) {
            Ok(set) => {
                for p in set.policies() {
                    let Some(id) = p.annotation("id") else {
                        errors.push(format!("{}: policy without @id", s.name));
                        continue;
                    };
                    if p.annotation("scope") != Some(s.scope_annotation()) {
                        errors.push(format!("{}: {id} has the wrong @scope", s.name));
                    }
                    out.push((s.scope, p.new_id(PolicyId::new(id))));
                }
            }
            Err(e) => errors.push(format!("{}: {e}", s.name)),
        }
    }
    if errors.is_empty() {
        Ok(out)
    } else {
        Err(errors)
    }
}

impl Source {
    fn scope_annotation(&self) -> &'static str {
        match self.scope {
            Scope::Classifier => "installation",
            s => s.name(),
        }
    }
}

/// The action names a policy's scope constrains, or None for any action.
pub fn scope_actions(p: &Policy) -> Option<BTreeSet<String>> {
    let est = p.to_json().ok()?;
    let a = &est["action"];
    let ids: Vec<String> = match a["op"].as_str() {
        Some("==") | Some("in") if a.get("entity").is_some() => {
            vec![a["entity"]["id"].as_str()?.to_string()]
        }
        Some("in") => a["entities"]
            .as_array()?
            .iter()
            .filter_map(|e| e["id"].as_str().map(str::to_string))
            .collect(),
        _ => return None,
    };
    let mut out = BTreeSet::new();
    for id in ids {
        if id == "exec_any" {
            out.extend(["exec", "exec_request_approval", "exec_approved"].map(String::from));
        } else {
            out.insert(id);
        }
    }
    Some(out)
}

/// Admission lint for pattern B: lower scopes hold only forbids.
pub fn lint_lower_scopes(policies: &[(Scope, Policy)]) -> Vec<String> {
    policies
        .iter()
        .filter(|(s, p)| s.is_lower() && p.effect() != Effect::Forbid)
        .map(|(s, p)| format!("{} scope holds a permit: {}", s.name(), p.id()))
        .collect()
}

/// Strict validation of a policy set against the schema.
pub fn validate(schema: &Schema, set: &PolicySet) -> Vec<String> {
    let result = Validator::new(schema.clone()).validate(set, ValidationMode::Strict);
    result.validation_errors().map(|e| e.to_string()).collect()
}

/// Validator warnings (for example an impossible policy) for a policy set.
pub fn validation_warnings(schema: &Schema, set: &PolicySet) -> Vec<String> {
    let result = Validator::new(schema.clone()).validate(set, ValidationMode::Strict);
    result
        .validation_warnings()
        .map(|w| w.to_string())
        .collect()
}

pub fn parse_schema(text: &str) -> Result<Schema, String> {
    Schema::from_cedarschema_str(text)
        .map(|(s, _)| s)
        .map_err(|e| e.to_string())
}

fn set_of<'a>(policies: impl Iterator<Item = &'a Policy>) -> Result<PolicySet, String> {
    let mut set = PolicySet::new();
    for p in policies {
        set.add(p.clone()).map_err(|e| e.to_string())?;
    }
    Ok(set)
}

fn baseline(scope: &str) -> Policy {
    Policy::parse(
        Some(PolicyId::new(format!("baseline.{scope}"))),
        "permit (principal, action, resource);",
    )
    .expect("baseline policy parses")
}

impl CedarEngine {
    /// Load, lint and strictly validate a corpus. Any failure refuses the
    /// whole bundle: nothing activates in part.
    pub fn load(schema_text: &str, sources: &[Source], store: &Store) -> Result<Self, Vec<String>> {
        let schema = parse_schema(schema_text).map_err(|e| vec![e])?;
        let policies = parse_sources(sources)?;
        let lint = lint_lower_scopes(&policies);
        if !lint.is_empty() {
            return Err(lint);
        }
        let merged = set_of(policies.iter().map(|(_, p)| p)).map_err(|e| vec![e])?;
        let errors = validate(&schema, &merged);
        if !errors.is_empty() {
            return Err(errors);
        }
        let authority = set_of(
            policies
                .iter()
                .filter(|(s, _)| !s.is_lower())
                .map(|(_, p)| p),
        )
        .map_err(|e| vec![e])?;
        let lower = |scope: Scope| -> Result<PolicySet, Vec<String>> {
            let mut set = set_of(policies.iter().filter(|(s, _)| *s == scope).map(|(_, p)| p))
                .map_err(|e| vec![e])?;
            set.add(baseline(scope.name()))
                .map_err(|e| vec![e.to_string()])?;
            Ok(set)
        };
        let workspace = lower(Scope::Workspace)?;
        let repository = lower(Scope::Repository)?;
        let entities = Entities::from_json_value(store.to_cedar_json(), Some(&schema))
            .map_err(|e| vec![e.to_string()])?;
        let scopes: Vec<(&Policy, Option<BTreeSet<String>>)> = policies
            .iter()
            .map(|(_, p)| (p, scope_actions(p)))
            .collect();
        let mut sliced = HashMap::new();
        for action in ACTIONS {
            let set = set_of(
                scopes
                    .iter()
                    .filter(|(_, s)| s.as_ref().is_none_or(|a| a.contains(action)))
                    .map(|(p, _)| *p),
            )
            .map_err(|e| vec![e])?;
            sliced.insert(action, set);
        }
        let reasons = policies
            .iter()
            .filter_map(|(_, p)| {
                p.annotation("reason")
                    .map(|r| (p.id().clone(), r.to_string()))
            })
            .collect();
        let actions = ACTIONS
            .iter()
            .map(|a| {
                let u = EntityUid::from_json(json!({"type": "Asura::Action", "id": a}))
                    .expect("action UID");
                (*a, u)
            })
            .collect();
        Ok(CedarEngine {
            schema,
            merged,
            authority,
            workspace,
            repository,
            entities: Arc::new(entities),
            sliced,
            reasons,
            actions,
            authorizer: Authorizer::new(),
        })
    }

    pub fn decide(&self, req: &Request, pattern: Pattern) -> Decision {
        let (p, a, r, c) = cedar_parts(req);
        self.decide_parts(p, a, r, c, pattern, Mode::Wrapper)
    }

    /// Decide from engine-facing JSON parts (the fuzzer mutates these).
    pub fn decide_parts(
        &self,
        principal: Value,
        action: &str,
        resource: Value,
        context: Value,
        pattern: Pattern,
        mode: Mode,
    ) -> Decision {
        let checked = json!([principal, resource, context]);
        if mode == Mode::Wrapper
            && let Err(_) = check_bounds(
                &checked,
                serde_json::to_vec(&checked).map_or(usize::MAX, |b| b.len()),
            )
        {
            return Decision::deny_error(ErrorClass::RequestTooLarge, BTreeSet::new());
        }
        let [principal, resource, context]: [Value; 3] = match checked {
            Value::Array(v) => match v.try_into() {
                Ok(a) => a,
                Err(_) => return Decision::deny_error(ErrorClass::RequestInvalid, BTreeSet::new()),
            },
            _ => return Decision::deny_error(ErrorClass::RequestInvalid, BTreeSet::new()),
        };
        let invalid = || Decision::deny_error(ErrorClass::RequestInvalid, BTreeSet::new());
        let action_name = action.to_string();
        let Some(action) = self.actions.get(action) else {
            return invalid();
        };
        let (Ok(principal), Ok(resource)) = (
            EntityUid::from_json(principal),
            EntityUid::from_json(resource),
        ) else {
            return invalid();
        };
        let schema = (mode == Mode::Wrapper).then_some(&self.schema);
        let Ok(context) = Context::from_json_value(context, schema.map(|s| (s, action))) else {
            return invalid();
        };
        let Ok(request) = CedarRequest::new(principal, action.clone(), resource, context, schema)
        else {
            return invalid();
        };
        match (mode, pattern) {
            (Mode::Raw, _) => self.raw(&request),
            (Mode::Wrapper, Pattern::B) => self.evaluate(&request, &self.merged),
            (Mode::Wrapper, Pattern::A) => self.pattern_a(&request),
            (Mode::Wrapper, Pattern::BSliced) => match self.sliced.get(action_name.as_str()) {
                Some(set) => self.evaluate(&request, set),
                None => invalid(),
            },
        }
    }

    fn raw(&self, request: &CedarRequest) -> Decision {
        let response = self
            .authorizer
            .is_authorized(request, &self.merged, &self.entities);
        let rules: BTreeSet<String> = response
            .diagnostics()
            .reason()
            .map(|id| id.to_string())
            .collect();
        Decision {
            allow: response.decision() == CedarDecision::Allow,
            reasons: rules.iter().filter_map(|r| self.reason_of(r)).collect(),
            rules,
            error: None,
        }
    }

    fn reason_of(&self, id: &str) -> Option<String> {
        self.reasons.get(&PolicyId::new(id)).cloned()
    }

    /// W3, W5 and W6 over one policy set.
    fn evaluate(&self, request: &CedarRequest, set: &PolicySet) -> Decision {
        let response = self.authorizer.is_authorized(request, set, &self.entities);
        let errors: BTreeSet<String> = response
            .diagnostics()
            .errors()
            .map(|e| match e {
                AuthorizationError::PolicyEvaluationError(p) => p.policy_id().to_string(),
            })
            .collect();
        if !errors.is_empty() {
            return Decision::deny_error(ErrorClass::PolicyError, errors);
        }
        let rules: BTreeSet<String> = response
            .diagnostics()
            .reason()
            .map(|id| id.to_string())
            .filter(|id| !id.starts_with("baseline."))
            .collect();
        if rules.is_empty() && response.decision() == CedarDecision::Deny {
            return Decision::no_permit();
        }
        let reasons = rules.iter().filter_map(|r| self.reason_of(r)).collect();
        Decision {
            allow: response.decision() == CedarDecision::Allow,
            rules,
            reasons,
            error: None,
        }
    }

    fn pattern_a(&self, request: &CedarRequest) -> Decision {
        let parts = [
            self.evaluate(request, &self.authority),
            self.evaluate(request, &self.workspace),
            self.evaluate(request, &self.repository),
        ];
        let errors: BTreeSet<String> = parts
            .iter()
            .filter(|d| d.error.is_some())
            .flat_map(|d| d.rules.iter().cloned())
            .collect();
        if parts.iter().any(|d| d.error.is_some()) {
            return Decision::deny_error(ErrorClass::PolicyError, errors);
        }
        if parts.iter().all(|d| d.allow) {
            return parts[0].clone();
        }
        let denying: Vec<&Decision> = parts
            .iter()
            .filter(|d| !d.allow && !d.rules.is_empty())
            .collect();
        if denying.is_empty() {
            return Decision::no_permit();
        }
        Decision {
            allow: false,
            rules: denying
                .iter()
                .flat_map(|d| d.rules.iter().cloned())
                .collect(),
            reasons: denying
                .iter()
                .flat_map(|d| d.reasons.iter().cloned())
                .collect(),
            error: None,
        }
    }
}
