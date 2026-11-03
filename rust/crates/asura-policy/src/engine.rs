//! The deny-on-any-error wrapper and pattern A composition
//! (security-policy.md, wrapper rules W1 to W6 and scope composition).
use crate::request::{
    Action, Context, Decision, ErrorClass, Intent, JobPrincipal, MAX_ANCESTORS, MAX_ENTITIES,
    MAX_REQUEST_BYTES, MAX_SET_ELEMENTS, MAX_STRING_BYTES, PathRef, Primary, Request, Resource,
    ScopeResult,
};
use crate::text::{RevisionTexts, Scope, check_text};
use cedar_policy::{
    AuthorizationError, Authorizer, Context as CedarContext, Decision as CedarDecision, Effect,
    Entities, Entity, EntityId, EntityTypeName, EntityUid, Policy, PolicyId, PolicySet,
    Request as CedarRequest, RestrictedExpression, Schema,
};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::str::FromStr;

/// Rule IDs kept per scope result.
const MAX_RULES: usize = 8;

/// A scope text that failed to load. Any failure refuses the whole
/// revision: nothing activates in part.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoadError {
    pub scope: Option<Scope>,
    pub messages: Vec<String>,
}
impl std::fmt::Display for LoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.scope {
            Some(scope) => write!(f, "{} policy: ", scope.as_str())?,
            None => write!(f, "policy: ")?,
        }
        write!(f, "{}", self.messages.join("; "))
    }
}
impl std::error::Error for LoadError {}

pub(crate) fn parse_schema() -> Result<Schema, String> {
    Schema::from_cedarschema_str(crate::schema_text())
        .map(|(schema, _)| schema)
        .map_err(|e| format!("schema: {e}"))
}

struct RuleMeta {
    reason: String,
    explain: Option<String>,
}

struct ScopeSets {
    scope: Scope,
    /// One set per action, indexed by action code - 1. Each holds the
    /// policies of the action's group whose action scope includes it: an
    /// exact refinement of the per-group partition, because Cedar never
    /// applies a policy whose action scope does not match.
    groups: Vec<PolicySet>,
    /// Per action: the set holds only the lower scope's baseline permit.
    /// Such a set always allows with no error, so Cedar is not called.
    baseline_only: Vec<bool>,
    ids: Vec<String>,
}

struct Types {
    job: EntityTypeName,
    project: EntityTypeName,
    task: EntityTypeName,
    path: EntityTypeName,
    program: EntityTypeName,
    origin: EntityTypeName,
    profile: EntityTypeName,
    class: EntityTypeName,
}

/// One worker's engine: its own schema, its own copy of each scope set,
/// partitioned by action group. Never shared between threads.
pub struct Engine {
    schema: Schema,
    scopes: Vec<ScopeSets>,
    applicable: Vec<Scope>,
    meta: HashMap<String, RuleMeta>,
    actions: Vec<EntityUid>,
    types: Types,
    authorizer: Authorizer,
}

fn type_name(name: &str) -> EntityTypeName {
    EntityTypeName::from_str(name).expect("schema type name")
}
fn uid(kind: &EntityTypeName, id: &str) -> EntityUid {
    EntityUid::from_type_name_and_id(kind.clone(), EntityId::new(id))
}

/// The action IDs of a policy's scope, or `None` for any action.
fn scope_action_ids(policy: &Policy) -> Option<BTreeSet<String>> {
    let est = policy.to_json().ok()?;
    let action = &est["action"];
    let ids: Vec<String> = match action["op"].as_str() {
        Some("==") | Some("in") if action.get("entity").is_some() => {
            vec![action["entity"]["id"].as_str()?.to_owned()]
        }
        Some("in") => action["entities"]
            .as_array()?
            .iter()
            .filter_map(|e| e["id"].as_str().map(str::to_owned))
            .collect(),
        _ => return None,
    };
    Some(ids.into_iter().collect())
}
/// The actions a policy can apply to, from its action scope.
fn actions_of(policy: &Policy) -> Vec<Action> {
    let Some(ids) = scope_action_ids(policy) else {
        return Action::ALL.to_vec();
    };
    let mut actions = Vec::new();
    for id in &ids {
        match id.as_str() {
            "file.any" => actions.extend([
                Action::FileRead,
                Action::FileWrite,
                Action::FileCreate,
                Action::FileDelete,
            ]),
            "process.exec.any" => actions.extend([
                Action::ProcessExec,
                Action::ProcessExecAsk,
                Action::ProcessExecApproved,
            ]),
            other => match Action::ALL.into_iter().find(|a| a.as_str() == other) {
                Some(action) => actions.push(action),
                // An unknown action ID cannot pass strict validation; the
                // fallback keeps the policy for every action.
                None => return Action::ALL.to_vec(),
            },
        }
    }
    actions
}

impl Engine {
    /// Build this worker's engine from the revision texts: parse, check
    /// annotations, strictly validate (warnings refuse), rename each policy
    /// to its global ID and partition each scope by action group. A
    /// workspace or repository text must be forbid-only; its baseline
    /// permit is added here.
    pub fn build(texts: &RevisionTexts) -> Result<Self, LoadError> {
        let schema = parse_schema().map_err(|e| LoadError {
            scope: None,
            messages: vec![e],
        })?;
        let mut seen = BTreeSet::new();
        let mut scopes = Vec::new();
        let mut meta = HashMap::new();
        let mut ordered: Vec<_> = texts.scopes.iter().collect();
        ordered.sort_by_key(|s| s.scope);
        for text in ordered {
            let fail = |messages: Vec<String>| LoadError {
                scope: Some(text.scope),
                messages,
            };
            if !seen.insert(text.scope) {
                return Err(fail(vec!["scope listed twice".into()]));
            }
            let set = check_text(&schema, text.scope, &text.text).map_err(fail)?;
            let lower = matches!(text.scope, Scope::Workspace | Scope::Repository);
            let mut policies = Vec::new();
            for policy in set.policies() {
                if lower && policy.effect() != Effect::Forbid {
                    return Err(fail(vec![format!(
                        "{} holds a permit",
                        policy.annotation("id").unwrap_or("?")
                    )]));
                }
                let local = policy.annotation("id").expect("checked @id");
                let global = format!("{}:{local}", text.scope.as_str());
                meta.insert(
                    global.clone(),
                    RuleMeta {
                        reason: policy.annotation("reason").expect("checked").to_owned(),
                        explain: policy.annotation("explain").map(str::to_owned),
                    },
                );
                policies.push((global.clone(), policy.new_id(PolicyId::new(&global))));
            }
            if lower {
                let global = format!("{}:baseline", text.scope.as_str());
                let baseline = Policy::parse(
                    Some(PolicyId::new(&global)),
                    "permit (principal, action, resource);",
                )
                .map_err(|e| fail(vec![e.to_string()]))?;
                policies.push((global, baseline));
            }
            let mut groups: Vec<PolicySet> = Action::ALL.iter().map(|_| PolicySet::new()).collect();
            for (_, policy) in &policies {
                for action in actions_of(policy) {
                    groups[usize::from(action.code()) - 1]
                        .add(policy.clone())
                        .map_err(|e| fail(vec![e.to_string()]))?;
                }
            }
            let baseline_only = groups
                .iter()
                .map(|set| {
                    lower
                        && set.policies().count() == 1
                        && set
                            .policies()
                            .all(|p| p.id().to_string().ends_with(":baseline"))
                })
                .collect();
            let mut ids: Vec<String> = policies.into_iter().map(|(id, _)| id).collect();
            ids.sort();
            scopes.push(ScopeSets {
                scope: text.scope,
                groups,
                baseline_only,
                ids,
            });
        }
        let actions = Action::ALL
            .iter()
            .map(|a| uid(&type_name("Asura::Action"), a.as_str()))
            .collect();
        Ok(Self {
            applicable: scopes.iter().map(|s| s.scope).collect(),
            schema,
            scopes,
            meta,
            actions,
            types: Types {
                job: type_name("Asura::Job"),
                project: type_name("Asura::Project"),
                task: type_name("Asura::Task"),
                path: type_name("Asura::Path"),
                program: type_name("Asura::Program"),
                origin: type_name("Asura::Origin"),
                profile: type_name("Asura::Profile"),
                class: type_name("Asura::PathClass"),
            },
            authorizer: Authorizer::new(),
        })
    }

    /// The global policy IDs of one scope, sorted; baseline permits included.
    pub fn policy_ids(&self, scope: Scope) -> Vec<String> {
        self.scopes
            .iter()
            .find(|s| s.scope == scope)
            .map(|s| s.ids.clone())
            .unwrap_or_default()
    }
    /// The scopes this engine evaluates, in order.
    pub fn scopes(&self) -> &[Scope] {
        &self.applicable
    }
    /// The `@explain` text and reason of a global rule ID.
    pub fn rule_reason(&self, rule: &str) -> Option<(&str, Option<&str>)> {
        self.meta
            .get(rule)
            .map(|m| (m.reason.as_str(), m.explain.as_deref()))
    }

    /// W1 and W2 only, for the bench breakdown: whether the request builds.
    #[doc(hidden)]
    pub fn build_only(&self, request: &Request) -> bool {
        self.build_request(request, None).is_ok()
    }
    /// Decide one request in every applicable scope (pattern A). Every
    /// failure denies with a reason; no failure path reaches Allow.
    pub fn decide(&self, request: &Request) -> Decision {
        self.decide_inner(request, None)
    }
    fn decide_inner(&self, request: &Request, context: Option<Value>) -> Decision {
        let built = match self.build_request(request, context) {
            Ok(built) => built,
            Err(class) => return Decision::request_error(class, &self.applicable),
        };
        let (cedar, entities) = built;
        let index = usize::from(request.action.code()) - 1;
        let results: Vec<ScopeResult> = self
            .scopes
            .iter()
            .map(|s| {
                if s.baseline_only[index] {
                    ScopeResult {
                        scope: s.scope,
                        allow: true,
                        rules: Vec::new(),
                        error: None,
                    }
                } else {
                    self.evaluate(s.scope, &cedar, &s.groups[index], &entities)
                }
            })
            .collect();
        let allow = !results.is_empty() && results.iter().all(|r| r.allow);
        let primary = if allow {
            None
        } else {
            results.iter().find(|r| !r.allow).map(|r| self.primary(r))
        };
        Decision {
            allow,
            scopes: results,
            primary,
        }
    }

    fn primary(&self, result: &ScopeResult) -> Primary {
        let scope = Some(result.scope);
        if let Some(error) = result.error {
            return Primary {
                scope,
                reason: error.reason().to_owned(),
                rule: result.rules.first().cloned(),
                explain: None,
            };
        }
        match result.rules.first() {
            None => Primary {
                scope,
                reason: "no_permit".to_owned(),
                rule: None,
                explain: None,
            },
            Some(rule) => {
                let meta = self.meta.get(rule);
                Primary {
                    scope,
                    reason: meta.map_or_else(|| "policy_error".to_owned(), |m| m.reason.clone()),
                    rule: Some(rule.clone()),
                    explain: match result.scope {
                        Scope::Installation | Scope::User => meta.and_then(|m| m.explain.clone()),
                        Scope::Workspace | Scope::Repository => None,
                    },
                }
            }
        }
    }

    /// W3, W5 and W6 over one scope's set for the action group.
    fn evaluate(
        &self,
        scope: Scope,
        request: &CedarRequest,
        set: &PolicySet,
        entities: &Entities,
    ) -> ScopeResult {
        let response = self.authorizer.is_authorized(request, set, entities);
        let mut errors: Vec<String> = response
            .diagnostics()
            .errors()
            .map(|e| match e {
                AuthorizationError::PolicyEvaluationError(p) => p.policy_id().to_string(),
            })
            .collect();
        if !errors.is_empty() {
            errors.sort();
            errors.dedup();
            errors.truncate(MAX_RULES);
            return ScopeResult {
                scope,
                allow: false,
                rules: errors,
                error: Some(ErrorClass::PolicyError),
            };
        }
        let mut rules: Vec<String> = response
            .diagnostics()
            .reason()
            .map(|id| id.to_string())
            .filter(|id| !id.ends_with(":baseline"))
            .collect();
        rules.sort();
        rules.truncate(MAX_RULES);
        ScopeResult {
            scope,
            allow: response.decision() == CedarDecision::Allow,
            rules,
            error: None,
        }
    }

    /// W1 and W2: the typed bounds, then the entity slice, the context and
    /// the request, each checked against the schema.
    fn build_request(
        &self,
        request: &Request,
        context: Option<Value>,
    ) -> Result<(CedarRequest, Entities), ErrorClass> {
        let slice = Slice::of(request)?;
        let context = context.unwrap_or_else(|| context_json(&request.context));
        let bytes = slice.bytes + serde_json::to_vec(&context).map_or(usize::MAX, |v| v.len());
        if bytes > MAX_REQUEST_BYTES {
            return Err(ErrorClass::RequestTooLarge);
        }
        let entities = self.entities(request, &slice)?;
        let action = &self.actions[usize::from(request.action.code()) - 1];
        let context = CedarContext::from_json_value(context, Some((&self.schema, action)))
            .map_err(|_| ErrorClass::RequestInvalid)?;
        let principal = uid(&self.types.job, &request.principal.job);
        let resource = match &request.resource {
            Resource::Path(path) => uid(&self.types.path, &path.path),
            Resource::Program(program) => uid(&self.types.program, &program.path.path),
        };
        let cedar = CedarRequest::new(
            principal,
            action.clone(),
            resource,
            context,
            Some(&self.schema),
        )
        .map_err(|_| ErrorClass::RequestInvalid)?;
        Ok((cedar, entities))
    }

    fn entities(&self, request: &Request, slice: &Slice) -> Result<Entities, ErrorClass> {
        let t = &self.types;
        let p = &request.principal;
        let s = RestrictedExpression::new_string;
        let mut list = Vec::with_capacity(slice.count);
        for (path, attrs) in &slice.paths {
            let parents = lexical_parent(path)
                .map(|parent| HashSet::from([uid(&t.path, parent)]))
                .unwrap_or_default();
            let entity = Entity::new(
                uid(&t.path, path),
                HashMap::from([
                    (
                        "class".to_owned(),
                        RestrictedExpression::new_entity_uid(uid(&t.class, attrs.class.as_str())),
                    ),
                    (
                        "in_project".to_owned(),
                        RestrictedExpression::new_bool(attrs.in_project),
                    ),
                    ("rel".to_owned(), s(attrs.rel.clone())),
                ]),
                parents,
            )
            .map_err(|_| ErrorClass::RequestInvalid)?;
            list.push(entity);
        }
        let mut job = HashMap::from([
            (
                "origin".to_owned(),
                RestrictedExpression::new_entity_uid(uid(&t.origin, p.origin.as_str())),
            ),
            (
                "profile".to_owned(),
                RestrictedExpression::new_entity_uid(uid(&t.profile, p.profile.as_str())),
            ),
            (
                "project".to_owned(),
                RestrictedExpression::new_entity_uid(uid(&t.project, &p.project)),
            ),
            (
                "task".to_owned(),
                RestrictedExpression::new_entity_uid(uid(&t.task, &p.task)),
            ),
            (
                "scratch".to_owned(),
                RestrictedExpression::new_entity_uid(uid(&t.path, &p.scratch.path)),
            ),
            (
                "cache_roots".to_owned(),
                RestrictedExpression::new_set(
                    p.cache_roots
                        .iter()
                        .map(|c| RestrictedExpression::new_entity_uid(uid(&t.path, &c.path))),
                ),
            ),
        ]);
        if let Some(dir) = &p.developer_dir {
            job.insert(
                "developer_dir".to_owned(),
                RestrictedExpression::new_entity_uid(uid(&t.path, &dir.path)),
            );
        }
        let wrap = |r: Result<Entity, _>| r.map_err(|_| ErrorClass::RequestInvalid);
        list.push(wrap(Entity::new(uid(&t.job, &p.job), job, HashSet::new()))?);
        list.push(wrap(Entity::new(
            uid(&t.project, &p.project),
            HashMap::from([
                (
                    "root".to_owned(),
                    RestrictedExpression::new_entity_uid(uid(&t.path, &p.project_root.path)),
                ),
                (
                    "declared_env".to_owned(),
                    RestrictedExpression::new_set(std::iter::empty()),
                ),
            ]),
            HashSet::new(),
        ))?);
        list.push(wrap(Entity::new(
            uid(&t.task, &p.task),
            HashMap::from([(
                "project".to_owned(),
                RestrictedExpression::new_entity_uid(uid(&t.project, &p.project)),
            )]),
            HashSet::new(),
        ))?);
        if let Resource::Program(program) = &request.resource {
            list.push(wrap(Entity::new(
                uid(&t.program, &program.path.path),
                HashMap::from([
                    (
                        "path".to_owned(),
                        RestrictedExpression::new_entity_uid(uid(&t.path, &program.path.path)),
                    ),
                    (
                        "identity_known".to_owned(),
                        RestrictedExpression::new_bool(program.identity_known),
                    ),
                    (
                        "platform".to_owned(),
                        RestrictedExpression::new_bool(program.platform),
                    ),
                    ("team_id".to_owned(), s(program.team_id.clone())),
                    ("signing_id".to_owned(), s(program.signing_id.clone())),
                    ("cdhash".to_owned(), s(program.cdhash.clone())),
                ]),
                HashSet::new(),
            ))?);
        }
        Entities::from_entities(list, Some(&self.schema)).map_err(|_| ErrorClass::RequestInvalid)
    }
}

#[cfg(test)]
impl Engine {
    /// The decision with the context JSON replaced (PL-U2).
    pub(crate) fn decide_with_context(&self, request: &Request, context: Value) -> Decision {
        self.decide_inner(request, Some(context))
    }
    /// Cedar alone over one scope's group set: its decision, ignoring
    /// errors (PL-U3 comparison).
    pub(crate) fn raw_allows(&self, scope: Scope, request: &Request) -> bool {
        let (cedar, entities) = self.build_request(request, None).expect("valid request");
        let index = usize::from(request.action.code()) - 1;
        let set = &self
            .scopes
            .iter()
            .find(|s| s.scope == scope)
            .expect("scope")
            .groups[index];
        self.authorizer
            .is_authorized(&cedar, set, &entities)
            .decision()
            == CedarDecision::Allow
    }
}
/// The entity count of a request's slice (PL-U1).
#[cfg(test)]
pub(crate) fn slice_count(request: &Request) -> Result<usize, ErrorClass> {
    Slice::of(request).map(|s| s.count)
}

/// The lexical parent of a canonical absolute path; `None` for `/`.
pub(crate) fn lexical_parent(path: &str) -> Option<&str> {
    if path == "/" {
        return None;
    }
    match path.rfind('/') {
        Some(0) => Some("/"),
        Some(i) => Some(&path[..i]),
        None => None,
    }
}

/// A canonical absolute path (PL1: lexical form only).
fn canonical(path: &str) -> bool {
    if path == "/" {
        return true;
    }
    path.starts_with('/')
        && !path.ends_with('/')
        && !path.chars().any(char::is_control)
        && path[1..]
            .split('/')
            .all(|c| !c.is_empty() && c != "." && c != "..")
}
fn components(path: &str) -> usize {
    if path == "/" {
        0
    } else {
        path.matches('/').count()
    }
}

/// The per-request entity slice: every path entity with its attributes,
/// and the counts that W1 bounds.
struct Slice {
    paths: BTreeMap<String, Attrs>,
    count: usize,
    bytes: usize,
}
#[derive(Clone, PartialEq, Eq)]
struct Attrs {
    class: crate::request::PathClass,
    in_project: bool,
    rel: String,
}
impl Slice {
    fn of(request: &Request) -> Result<Self, ErrorClass> {
        let p: &JobPrincipal = &request.principal;
        let too_long = |s: &str| s.len() > MAX_STRING_BYTES;
        let mut strings: Vec<&str> = vec![&p.job, &p.project, &p.task];
        let mut explicit: Vec<&PathRef> = vec![&p.project_root, &p.scratch];
        let mut program_strings = 0;
        if p.cache_roots.len() > MAX_SET_ELEMENTS {
            return Err(ErrorClass::RequestTooLarge);
        }
        explicit.extend(p.cache_roots.iter());
        explicit.extend(p.developer_dir.iter());
        match &request.resource {
            Resource::Path(path) => explicit.push(path),
            Resource::Program(program) => {
                program_strings = 3;
                explicit.push(&program.path);
                strings.extend([
                    program.team_id.as_str(),
                    program.signing_id.as_str(),
                    program.cdhash.as_str(),
                ]);
            }
        }
        let (intent, sets): (Option<&Intent>, Vec<&Vec<String>>) = match &request.context {
            Context::File { intent, .. } => (intent.as_ref(), Vec::new()),
            Context::Exec {
                request_digest,
                env_terminal,
                env_declared,
                intent,
                ..
            } => {
                strings.push(request_digest);
                (intent.as_ref(), vec![env_terminal, env_declared])
            }
        };
        for set in sets {
            if set.len() > MAX_SET_ELEMENTS {
                return Err(ErrorClass::RequestTooLarge);
            }
            strings.extend(set.iter().map(String::as_str));
        }
        if let Some(intent) = intent {
            strings.extend([intent.name.as_str(), intent.digest.as_str()]);
        }
        // Context strings are counted in the context JSON; the rest belong
        // to entities.
        let context_strings = strings.len() - 3 - program_strings;
        for path in &explicit {
            strings.extend([path.path.as_str(), path.rel.as_str()]);
        }
        if strings.iter().any(|s| too_long(s)) {
            return Err(ErrorClass::RequestTooLarge);
        }
        if p.job.is_empty() || p.project.is_empty() || p.task.is_empty() {
            return Err(ErrorClass::RequestInvalid);
        }
        let mut paths: BTreeMap<String, Attrs> = BTreeMap::new();
        for path in &explicit {
            if !canonical(&path.path) {
                return Err(ErrorClass::RequestInvalid);
            }
            if components(&path.path) > MAX_ANCESTORS {
                return Err(ErrorClass::RequestTooLarge);
            }
            let attrs = Attrs {
                class: path.class,
                in_project: path.in_project,
                rel: path.rel.clone(),
            };
            match paths.get(&path.path) {
                Some(existing) if *existing != attrs => return Err(ErrorClass::RequestInvalid),
                Some(_) => {}
                None => {
                    paths.insert(path.path.clone(), attrs);
                }
            }
        }
        for path in &explicit {
            let mut cursor = lexical_parent(&path.path);
            while let Some(parent) = cursor {
                if paths.contains_key(parent) {
                    // Its own ancestors were added with it or will be.
                } else {
                    paths.insert(
                        parent.to_owned(),
                        Attrs {
                            class: crate::request::PathClass::Ordinary,
                            in_project: false,
                            rel: String::new(),
                        },
                    );
                }
                cursor = lexical_parent(parent);
            }
        }
        let count = paths.len() + 3 + usize::from(matches!(request.resource, Resource::Program(_)));
        if count > MAX_ENTITIES {
            return Err(ErrorClass::RequestTooLarge);
        }
        // Encoded size: every entity ID and attribute string with a fixed
        // per-entity overhead of 64 bytes.
        let bytes = paths
            .iter()
            .map(|(path, attrs)| 2 * path.len() + attrs.rel.len() + 64)
            .sum::<usize>()
            + strings
                .iter()
                .enumerate()
                .filter(|(i, _)| {
                    *i < 3 + program_strings || *i >= 3 + program_strings + context_strings
                })
                .map(|(_, s)| s.len())
                .sum::<usize>()
            + 64 * 4;
        Ok(Self {
            paths,
            count,
            bytes,
        })
    }
}

/// RFC 3339 UTC text of milliseconds since the Unix epoch.
pub(crate) fn rfc3339(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let millis = ms.rem_euclid(1000);
    let days = secs.div_euclid(86_400);
    let sod = secs.rem_euclid(86_400);
    // Civil-from-days (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    let (h, mi, s) = (sod / 3600, (sod % 3600) / 60, sod % 60);
    if millis == 0 {
        format!("{y:04}-{m:02}-{d:02}T{h:02}:{mi:02}:{s:02}Z")
    } else {
        format!("{y:04}-{m:02}-{d:02}T{h:02}:{mi:02}:{s:02}.{millis:03}Z")
    }
}

fn intent_json(intent: &Intent) -> Value {
    json!({
        "source": {"__entity": {"type": "Asura::IntentSource", "id": intent.source.as_str()}},
        "name": intent.name,
        "digest": intent.digest,
    })
}

pub(crate) fn context_json(context: &Context) -> Value {
    let now = |ms: i64| json!({"__extn": {"fn": "datetime", "arg": rfc3339(ms)}});
    match context {
        Context::File { now_ms, intent } => {
            let mut v = json!({ "now": now(*now_ms) });
            if let Some(intent) = intent {
                v["intent"] = intent_json(intent);
            }
            v
        }
        Context::Exec {
            now_ms,
            request_digest,
            env_terminal,
            env_declared,
            outside_switch,
            intent,
        } => {
            let mut v = json!({
                "now": now(*now_ms),
                "request_digest": request_digest,
                "env_terminal": env_terminal,
                "env_declared": env_declared,
                "outside_switch": outside_switch,
            });
            if let Some(intent) = intent {
                v["intent"] = intent_json(intent);
            }
            v
        }
    }
}
