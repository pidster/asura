//! Kinds 34 and 35 and the kind 14 policy refusal (security-policy.md,
//! PL1 implementation contract): the policy decisions that admit or refuse
//! work, and the policy revision events. Records hold digests, codes and
//! rule IDs only; never a command line, a path string or a secret value.
//! Replay rules R1 to R7 are applied by the replay state.
use super::codec::{Reader, Writer};
use super::*;
use crate::authority::ReplayError;
use std::collections::BTreeMap;

type Result<T> = std::result::Result<T, ReplayError>;

/// The kind 34 payload bound.
pub const MAX_POLICY_DECISION_PAYLOAD: usize = 4096;
/// The kind 35 payload bound.
pub const MAX_POLICY_REVISION_PAYLOAD: usize = 8192;
/// Rule IDs per scope result.
pub const MAX_POLICY_RULES: usize = 8;
/// Scope results or scope sets per record.
pub const MAX_POLICY_SCOPES: usize = 4;
/// A reason code: `[a-z0-9_]{1,48}`.
pub const MAX_POLICY_REASON: usize = 48;
/// A policy ID or source identity: `[a-z0-9_.-]{1,64}`.
pub const MAX_POLICY_ID: usize = 64;
/// A global rule ID `<scope>:<id>`: the longest scope name, a colon, an ID.
pub const MAX_POLICY_RULE: usize = 10 + 1 + MAX_POLICY_ID;
/// The `@explain` text: at most 200 bytes of printable ASCII.
pub const MAX_POLICY_EXPLAIN: usize = 200;
/// The Cedar language and SDK version strings.
pub const MAX_POLICY_VERSION: usize = 16;
/// Policy IDs per scope set; the payload bound applies first in practice.
pub const MAX_POLICY_IDS: usize = 512;
/// Pending kind 34 records: at most one per active call; a bound for the
/// checkpoint.
pub const MAX_PENDING_POLICY_DECISIONS: usize = 64;
/// The largest kind 14 policy refusal field: flag, scope, three strings.
pub const MAX_POLICY_REFUSAL_FIELD: usize =
    1 + 1 + 4 + MAX_POLICY_REASON + 4 + MAX_POLICY_RULE + 4 + MAX_POLICY_EXPLAIN;

/// The scope names of codes 1 to 4.
pub const POLICY_SCOPE_NAMES: [&str; 4] = ["installation", "user", "workspace", "repository"];

fn require(ok: bool) -> Result<()> {
    if ok {
        Ok(())
    } else {
        Err(ReplayError::Corrupt)
    }
}
/// `[a-z0-9_]{1,48}`.
pub fn valid_policy_reason(s: &str) -> bool {
    (1..=MAX_POLICY_REASON).contains(&s.len())
        && s.bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}
/// `[a-z0-9_.-]{1,64}`.
pub fn valid_policy_id(s: &str) -> bool {
    (1..=MAX_POLICY_ID).contains(&s.len())
        && s.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'.' | b'-')
        })
}
/// `<scope>:<id>` with a scope name of codes 1 to 4.
pub fn valid_policy_rule(s: &str) -> bool {
    match s.split_once(':') {
        Some((scope, id)) => POLICY_SCOPE_NAMES.contains(&scope) && valid_policy_id(id),
        None => false,
    }
}
/// At most 200 bytes of printable ASCII (0x20 to 0x7e).
pub fn valid_policy_explain(s: &str) -> bool {
    s.len() <= MAX_POLICY_EXPLAIN && s.bytes().all(|b| (0x20..=0x7e).contains(&b))
}
fn valid_version(s: &str) -> bool {
    (1..=MAX_POLICY_VERSION).contains(&s.len())
        && s.bytes().all(|b| b.is_ascii_digit() || b == b'.')
}

/// The kind 14 policy refusal field (security-policy.md, kind 14
/// additions).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PolicyRefusal {
    /// 0 none, 1 installation, 2 user, 3 workspace, 4 repository.
    pub scope: u8,
    /// `[a-z0-9_]{1,48}`.
    pub reason: String,
    /// Empty, or the global rule ID `<scope>:<id>`.
    pub rule: String,
    /// Empty, or the `@explain` text of an installation or user policy.
    pub explain: String,
}
impl PolicyRefusal {
    pub(crate) fn validate(&self) -> Result<()> {
        require(
            self.scope <= 4
                && valid_policy_reason(&self.reason)
                && (self.rule.is_empty() || valid_policy_rule(&self.rule))
                && valid_policy_explain(&self.explain)
                // Explanations of workspace and repository policies never
                // reach a model.
                && (self.explain.is_empty() || matches!(self.scope, 1 | 2)),
        )
    }
}
/// The last kind 14 field. Callers validate the record first; tests use
/// it to build invalid bytes.
pub(crate) fn policy_refusal_fields(w: &mut Writer, value: Option<&PolicyRefusal>) {
    match value {
        None => w.byte(0),
        Some(p) => {
            w.byte(1);
            w.byte(p.scope);
            for s in [&p.reason, &p.rule, &p.explain] {
                w.u32(s.len() as u32);
                w.bytes(s.as_bytes());
            }
        }
    }
}
pub(crate) fn read_policy_refusal(r: &mut Reader<'_>) -> Result<Option<PolicyRefusal>> {
    if !r.flag()? {
        return Ok(None);
    }
    let value = PolicyRefusal {
        scope: r.byte()?,
        reason: r.bounded_string(MAX_POLICY_REASON)?,
        rule: r.bounded_string(MAX_POLICY_RULE)?,
        explain: r.bounded_string(MAX_POLICY_EXPLAIN)?,
    };
    value.validate()?;
    Ok(Some(value))
}

/// What a kind 34 decides about.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PolicySubject {
    /// Code 1: one model tool call.
    ModelCall {
        operation: Id,
        generation: u64,
        ordinal: u32,
    },
    /// Code 2 (PL2): one person command start, named by its input.
    PersonInput { input: Id },
    /// Code 3 (PL3): a recheck of one queued, starting or running job
    /// after an activation (security-policy.md, kind 34 subject 3).
    JobRecheck { job: Id },
}
/// Pending person kind 34 records (R8 to R10): the service decision queue
/// bound.
pub const MAX_PENDING_PERSON_DECISIONS: usize = 512;
/// One scope's result in a kind 34.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PolicyScopeResult {
    /// 1 installation, 2 user, 3 workspace, 4 repository.
    pub scope: u8,
    pub allow: bool,
    /// 0 none, 1 `policy_error`, 2 `request_invalid`, 3
    /// `request_too_large`, 4 `scope_unavailable`.
    pub error: u8,
    /// At most 8 global rule IDs.
    pub rules: Vec<String>,
}
/// A kind 34 decision. `Ask` is refused by replay until packet PL7.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PolicyOutcome {
    Run = 1,
    Ask = 2,
    Deny = 3,
}
impl PolicyOutcome {
    pub fn from_code(code: u8) -> Option<Self> {
        Some(match code {
            1 => Self::Run,
            2 => Self::Ask,
            3 => Self::Deny,
            _ => return None,
        })
    }
}
/// Kind 34 `PolicyDecision`, at most 4 KiB.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PolicyDecision {
    pub subject: PolicySubject,
    /// The effect action code, 1 to 11 (`file.read` .. `context.egress`).
    pub action: u8,
    /// 1 model, 2 person_typed, 3 system.
    pub origin: u8,
    /// 1 `model-1`, 2 `person-1`, 3 `person-outside-1`.
    pub profile: u8,
    pub project: Id,
    pub task_conversation: Id,
    pub task_agent: u32,
    pub resource_digest: Hash,
    pub request_digest: Hash,
    pub intent_digest: Option<Hash>,
    pub revision_seq: u64,
    pub revision_digest: Hash,
    /// 1 to 4, in ascending scope order, distinct.
    pub scopes: Vec<PolicyScopeResult>,
    pub outcome: PolicyOutcome,
    /// 0 none, or the first denying scope.
    pub primary_scope: u8,
    /// `[a-z0-9_]{1,48}`.
    pub reason: String,
    /// Present exactly with `Run`.
    pub constraint_digest: Option<Hash>,
    /// The evaluation time, milliseconds since the Unix epoch.
    pub now_ms: u64,
}
impl PolicyDecision {
    pub(crate) fn validate(&self) -> Result<()> {
        // Subject 1 is a model `model-1` job with a constraint set; subject
        // 2 a person job with none until PL6.
        let (subject, model) = match self.subject {
            PolicySubject::ModelCall {
                operation,
                generation,
                ordinal,
            } => (
                operation != [0; 16]
                    && generation >= 1
                    && ordinal >= 1
                    && self.origin == 1
                    && self.profile == 1,
                true,
            ),
            PolicySubject::PersonInput { input } => (
                input != [0; 16]
                    && self.origin == 2
                    && matches!(self.profile, 2 | 3)
                    && self.task_agent == 0,
                false,
            ),
            // Subject 3: a model job of profile 1 carries a constraint set
            // exactly with `run`; a person job never does (until PL6).
            PolicySubject::JobRecheck { job } => (
                job != [0; 16]
                    && ((self.origin == 1 && self.profile == 1)
                        || (self.origin == 2 && matches!(self.profile, 2 | 3))),
                self.profile == 1,
            ),
        };
        let scopes = (1..=MAX_POLICY_SCOPES).contains(&self.scopes.len())
            && self.scopes.windows(2).all(|w| w[0].scope < w[1].scope)
            && self.scopes.iter().all(|s| {
                (1..=4).contains(&s.scope)
                    && s.error <= 4
                    && (s.error == 0 || !s.allow)
                    && s.rules.len() <= MAX_POLICY_RULES
                    && s.rules.iter().all(|r| valid_policy_rule(r))
            });
        let all_allow = self.scopes.iter().all(|s| s.allow);
        let outcome = match self.outcome {
            PolicyOutcome::Run => {
                all_allow && self.primary_scope == 0 && self.constraint_digest.is_some() == model
            }
            PolicyOutcome::Ask | PolicyOutcome::Deny => {
                self.constraint_digest.is_none()
                    && (self.primary_scope == 0
                        || self
                            .scopes
                            .iter()
                            .any(|s| s.scope == self.primary_scope && !s.allow))
            }
        };
        require(
            subject
                && (1..=11).contains(&self.action)
                && (1..=3).contains(&self.origin)
                && (1..=3).contains(&self.profile)
                && self.project != [0; 16]
                && self.task_conversation != [0; 16]
                && self.revision_seq >= 1
                && self.primary_scope <= 4
                && valid_policy_reason(&self.reason)
                && scopes
                && outcome,
        )
    }
    /// The subject's call key; `None` for a person subject.
    pub fn call(&self) -> Option<(Id, u64, u32)> {
        match self.subject {
            PolicySubject::ModelCall {
                operation,
                generation,
                ordinal,
            } => Some((operation, generation, ordinal)),
            PolicySubject::PersonInput { .. } | PolicySubject::JobRecheck { .. } => None,
        }
    }
}
fn option_hash(w: &mut Writer, value: Option<&Hash>) {
    w.byte(u8::from(value.is_some()));
    if let Some(value) = value {
        w.bytes(value);
    }
}
fn read_option_hash(r: &mut Reader<'_>) -> Result<Option<Hash>> {
    Ok(if r.flag()? { Some(r.array()?) } else { None })
}
pub(crate) fn write_policy_decision(w: &mut Writer, p: &PolicyDecision) -> Result<()> {
    p.validate()?;
    let start = w.0.len();
    match p.subject {
        PolicySubject::ModelCall {
            operation,
            generation,
            ordinal,
        } => {
            w.byte(1);
            w.bytes(&operation);
            w.u64(generation);
            w.u32(ordinal);
        }
        PolicySubject::PersonInput { input } => {
            w.byte(2);
            w.bytes(&input);
        }
        PolicySubject::JobRecheck { job } => {
            w.byte(3);
            w.bytes(&job);
        }
    }
    w.byte(p.action);
    w.byte(p.origin);
    w.byte(p.profile);
    w.bytes(&p.project);
    w.bytes(&p.task_conversation);
    w.u32(p.task_agent);
    w.bytes(&p.resource_digest);
    w.bytes(&p.request_digest);
    option_hash(w, p.intent_digest.as_ref());
    w.u64(p.revision_seq);
    w.bytes(&p.revision_digest);
    w.byte(p.scopes.len() as u8);
    for s in &p.scopes {
        w.byte(s.scope);
        w.byte(u8::from(s.allow));
        w.byte(s.error);
        w.byte(s.rules.len() as u8);
        for rule in &s.rules {
            w.string(rule, MAX_POLICY_RULE)?;
        }
    }
    w.byte(p.outcome as u8);
    w.byte(p.primary_scope);
    w.string(&p.reason, MAX_POLICY_REASON)?;
    option_hash(w, p.constraint_digest.as_ref());
    w.u64(p.now_ms);
    if w.0.len() - start > MAX_POLICY_DECISION_PAYLOAD {
        return Err(ReplayError::Limit);
    }
    Ok(())
}
pub(crate) fn read_policy_decision(r: &mut Reader<'_>) -> Result<PolicyDecision> {
    let subject = match r.byte()? {
        1 => PolicySubject::ModelCall {
            operation: r.array()?,
            generation: r.u64()?,
            ordinal: r.u32()?,
        },
        2 => PolicySubject::PersonInput { input: r.array()? },
        3 => PolicySubject::JobRecheck { job: r.array()? },
        _ => return Err(ReplayError::Corrupt),
    };
    let action = r.byte()?;
    let origin = r.byte()?;
    let profile = r.byte()?;
    let project = r.array()?;
    let task_conversation = r.array()?;
    let task_agent = r.u32()?;
    let resource_digest = r.array()?;
    let request_digest = r.array()?;
    let intent_digest = read_option_hash(r)?;
    let revision_seq = r.u64()?;
    let revision_digest = r.array()?;
    let count = r.byte()? as usize;
    require((1..=MAX_POLICY_SCOPES).contains(&count))?;
    let mut scopes = Vec::with_capacity(count);
    for _ in 0..count {
        let scope = r.byte()?;
        let allow = r.flag()?;
        let error = r.byte()?;
        let rules_count = r.byte()? as usize;
        require(rules_count <= MAX_POLICY_RULES)?;
        let mut rules = Vec::with_capacity(rules_count);
        for _ in 0..rules_count {
            rules.push(r.bounded_string(MAX_POLICY_RULE)?);
        }
        scopes.push(PolicyScopeResult {
            scope,
            allow,
            error,
            rules,
        });
    }
    let outcome = PolicyOutcome::from_code(r.byte()?).ok_or(ReplayError::Corrupt)?;
    let primary_scope = r.byte()?;
    let reason = r.bounded_string(MAX_POLICY_REASON)?;
    let constraint_digest = read_option_hash(r)?;
    let now_ms = r.u64()?;
    let value = PolicyDecision {
        subject,
        action,
        origin,
        profile,
        project,
        task_conversation,
        task_agent,
        resource_digest,
        request_digest,
        intent_digest,
        revision_seq,
        revision_digest,
        scopes,
        outcome,
        primary_scope,
        reason,
        constraint_digest,
        now_ms,
    };
    value.validate()?;
    Ok(value)
}

/// A kind 35 event. Replay accepts `Activated` and (PL3) `Rejected`;
/// `Superseded` and `Revoked` wait for the packet that writes them (R2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PolicyRevisionEvent {
    Activated = 1,
    Superseded = 2,
    Revoked = 3,
    Rejected = 4,
}
impl PolicyRevisionEvent {
    pub fn from_code(code: u8) -> Option<Self> {
        Some(match code {
            1 => Self::Activated,
            2 => Self::Superseded,
            3 => Self::Revoked,
            4 => Self::Rejected,
            _ => return None,
        })
    }
}
/// One scope set of a revision manifest.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PolicyScopeSet {
    /// 1 installation, 2 user, 3 workspace, 4 repository.
    pub scope: u8,
    /// Present exactly for the workspace and repository scopes.
    pub project: Option<Id>,
    pub text_digest: Hash,
    /// The source identity, such as `builtin`: `[a-z0-9_.-]{1,64}`.
    pub source: String,
    /// The policy IDs of the scope set, each `[a-z0-9_.-]{1,64}`.
    pub policy_ids: Vec<String>,
}
/// Kind 35 `PolicyRevision`, at most 8 KiB.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PolicyRevision {
    pub seq: u64,
    pub digest: Hash,
    /// The previous active sequence; 0 for the first.
    pub previous_seq: u64,
    pub event: PolicyRevisionEvent,
    pub schema_digest: Hash,
    pub cedar_lang: String,
    pub cedar_sdk: String,
    pub solver_digest: Option<Hash>,
    pub admission_digest: Option<Hash>,
    /// 1 to 4 scope sets, ordered by scope and project, distinct.
    pub scopes: Vec<PolicyScopeSet>,
}
impl PolicyRevision {
    pub(crate) fn validate(&self) -> Result<()> {
        let sets = (1..=MAX_POLICY_SCOPES).contains(&self.scopes.len())
            && self
                .scopes
                .windows(2)
                .all(|w| (w[0].scope, w[0].project) < (w[1].scope, w[1].project))
            && self.scopes.iter().all(|s| {
                (1..=4).contains(&s.scope)
                    && s.project.is_some() == matches!(s.scope, 3 | 4)
                    && s.project != Some([0; 16])
                    && valid_policy_id(&s.source)
                    && s.policy_ids.len() <= MAX_POLICY_IDS
                    && s.policy_ids.iter().all(|id| valid_policy_id(id))
            });
        require(
            self.seq >= 1
                && self.previous_seq < self.seq
                && valid_version(&self.cedar_lang)
                && valid_version(&self.cedar_sdk)
                && sets,
        )
    }
}
pub(crate) fn write_policy_revision(w: &mut Writer, p: &PolicyRevision) -> Result<()> {
    p.validate()?;
    let start = w.0.len();
    w.u64(p.seq);
    w.bytes(&p.digest);
    w.u64(p.previous_seq);
    w.byte(p.event as u8);
    w.bytes(&p.schema_digest);
    w.string(&p.cedar_lang, MAX_POLICY_VERSION)?;
    w.string(&p.cedar_sdk, MAX_POLICY_VERSION)?;
    option_hash(w, p.solver_digest.as_ref());
    option_hash(w, p.admission_digest.as_ref());
    w.byte(p.scopes.len() as u8);
    for s in &p.scopes {
        w.byte(s.scope);
        w.optional(s.project);
        w.bytes(&s.text_digest);
        w.string(&s.source, MAX_POLICY_ID)?;
        w.u16(s.policy_ids.len() as u16);
        for id in &s.policy_ids {
            w.string(id, MAX_POLICY_ID)?;
        }
        if w.0.len() - start > MAX_POLICY_REVISION_PAYLOAD {
            return Err(ReplayError::Limit);
        }
    }
    if w.0.len() - start > MAX_POLICY_REVISION_PAYLOAD {
        return Err(ReplayError::Limit);
    }
    Ok(())
}
pub(crate) fn read_policy_revision(r: &mut Reader<'_>) -> Result<PolicyRevision> {
    let seq = r.u64()?;
    let digest = r.array()?;
    let previous_seq = r.u64()?;
    let event = PolicyRevisionEvent::from_code(r.byte()?).ok_or(ReplayError::Corrupt)?;
    let schema_digest = r.array()?;
    let cedar_lang = r.bounded_string(MAX_POLICY_VERSION)?;
    let cedar_sdk = r.bounded_string(MAX_POLICY_VERSION)?;
    let solver_digest = read_option_hash(r)?;
    let admission_digest = read_option_hash(r)?;
    let count = r.byte()? as usize;
    require((1..=MAX_POLICY_SCOPES).contains(&count))?;
    let mut scopes = Vec::with_capacity(count);
    for _ in 0..count {
        let scope = r.byte()?;
        let project = r.optional()?;
        let text_digest = r.array()?;
        let source = r.bounded_string(MAX_POLICY_ID)?;
        let ids = r.u16()? as usize;
        require(ids <= MAX_POLICY_IDS)?;
        let mut policy_ids = Vec::with_capacity(ids);
        for _ in 0..ids {
            policy_ids.push(r.bounded_string(MAX_POLICY_ID)?);
        }
        scopes.push(PolicyScopeSet {
            scope,
            project,
            text_digest,
            source,
            policy_ids,
        });
    }
    let value = PolicyRevision {
        seq,
        digest,
        previous_seq,
        event,
        schema_digest,
        cedar_lang,
        cedar_sdk,
        solver_digest,
        admission_digest,
        scopes,
    };
    value.validate()?;
    Ok(value)
}

/// The kind 36 payload bound.
pub const MAX_POLICY_GRANT_PAYLOAD: usize = 8192;
/// Open kind 36 grants (security-policy.md, limits: 256 generated grants).
pub const MAX_OPEN_POLICY_GRANTS: usize = 256;
/// Activated revisions replay keeps for rollback (PL3 rollback).
pub const MAX_RECENT_POLICY_REVISIONS: usize = 64;

/// A kind 36 grant kind. `Envelope` is refused by replay until PL8.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PolicyGrantKind {
    Approval = 1,
    Envelope = 2,
}
impl PolicyGrantKind {
    pub fn from_code(code: u8) -> Option<Self> {
        Some(match code {
            1 => Self::Approval,
            2 => Self::Envelope,
            _ => return None,
        })
    }
}
/// A kind 36 event (security-policy.md, kind 36 in PL3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PolicyGrantEvent {
    Requested = 1,
    Granted = 2,
    Denied = 3,
    Cancelled = 4,
    Ended = 5,
}
impl PolicyGrantEvent {
    pub fn from_code(code: u8) -> Option<Self> {
        Some(match code {
            1 => Self::Requested,
            2 => Self::Granted,
            3 => Self::Denied,
            4 => Self::Cancelled,
            5 => Self::Ended,
            _ => return None,
        })
    }
}
/// The cause codes of a kind 36 `cancelled` or `ended`.
pub mod grant_cause {
    pub const NONE: u8 = 0;
    pub const RESTART: u8 = 1;
    pub const PERSON: u8 = 2;
    pub const JOB_SETTLED: u8 = 3;
    pub const NARROWING: u8 = 4;
    pub const EXPIRY: u8 = 5;
}
/// Kind 36 `PolicyGrant`, at most 8 KiB. No command, path or policy text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PolicyGrant {
    pub grant: Id,
    pub kind: PolicyGrantKind,
    pub event: PolicyGrantEvent,
    pub task_conversation: Id,
    pub task_agent: u32,
    pub job: Id,
    /// Present exactly with `requested`.
    pub request_digest: Option<Hash>,
    /// Optional with `requested`, absent otherwise.
    pub constraint_digest: Option<Hash>,
    /// Optional with `requested`, absent otherwise.
    pub intent_digest: Option<Hash>,
    /// Optional with `granted`, absent otherwise.
    pub expiry_ms: Option<u64>,
    /// 1 to 5 with `cancelled` and `ended`, 0 otherwise.
    pub cause: u8,
    /// The active revision sequence when written.
    pub revision_seq: u64,
}
impl PolicyGrant {
    pub(crate) fn validate(&self) -> Result<()> {
        use PolicyGrantEvent as E;
        let fields = match self.event {
            E::Requested => {
                self.request_digest.is_some() && self.expiry_ms.is_none() && self.cause == 0
            }
            E::Granted => {
                self.request_digest.is_none()
                    && self.constraint_digest.is_none()
                    && self.intent_digest.is_none()
                    && self.cause == 0
            }
            E::Denied => self.no_facts() && self.cause == 0,
            E::Cancelled | E::Ended => self.no_facts() && (1..=5).contains(&self.cause),
        };
        require(
            self.grant != [0; 16]
                && self.task_conversation != [0; 16]
                && self.job != [0; 16]
                && self.revision_seq >= 1
                && fields,
        )
    }
    fn no_facts(&self) -> bool {
        self.request_digest.is_none()
            && self.constraint_digest.is_none()
            && self.intent_digest.is_none()
            && self.expiry_ms.is_none()
    }
}
pub(crate) fn write_policy_grant(w: &mut Writer, p: &PolicyGrant) -> Result<()> {
    p.validate()?;
    w.bytes(&p.grant);
    w.byte(p.kind as u8);
    w.byte(p.event as u8);
    w.bytes(&p.task_conversation);
    w.u32(p.task_agent);
    w.bytes(&p.job);
    option_hash(w, p.request_digest.as_ref());
    option_hash(w, p.constraint_digest.as_ref());
    option_hash(w, p.intent_digest.as_ref());
    w.byte(u8::from(p.expiry_ms.is_some()));
    if let Some(expiry) = p.expiry_ms {
        w.u64(expiry);
    }
    w.byte(p.cause);
    w.u64(p.revision_seq);
    Ok(())
}
pub(crate) fn read_policy_grant(r: &mut Reader<'_>) -> Result<PolicyGrant> {
    let grant = r.array()?;
    let kind = PolicyGrantKind::from_code(r.byte()?).ok_or(ReplayError::Corrupt)?;
    let event = PolicyGrantEvent::from_code(r.byte()?).ok_or(ReplayError::Corrupt)?;
    let value = PolicyGrant {
        grant,
        kind,
        event,
        task_conversation: r.array()?,
        task_agent: r.u32()?,
        job: r.array()?,
        request_digest: read_option_hash(r)?,
        constraint_digest: read_option_hash(r)?,
        intent_digest: read_option_hash(r)?,
        expiry_ms: if r.flag()? { Some(r.u64()?) } else { None },
        cause: r.byte()?,
        revision_seq: r.u64()?,
    };
    value.validate()?;
    Ok(value)
}

/// One activated revision kept for rollback: its sequence and digest and
/// the user scope's text digest and source identity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RecentRevision {
    pub seq: u64,
    pub digest: Hash,
    pub user_text: Hash,
    pub user_source: String,
}

/// The source identities a kind 35 scope set may name (R12). The
/// test-only denying installation set (`builtin-test-deny`) exists only
/// in test-support builds, but replay accepts its records in every build.
fn valid_source(scope: u8, source: &str) -> bool {
    match source {
        "person" => true,
        "builtin" => matches!(scope, 1 | 2),
        "builtin-test-deny" => scope == 1,
        _ => false,
    }
}

/// The policy facts replay keeps (R1 to R14): the last kind 35 sequence,
/// the active revision with its manifest, the recent activated revisions,
/// the kind 34 records that wait for their kind 14 or kind 26, and the
/// open kind 36 grants.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PolicyReplay {
    /// The last kind 35 sequence; 0 before the first.
    pub last_seq: u64,
    /// The active revision's sequence and digest.
    pub active: Option<(u64, Hash)>,
    pub(crate) pending: BTreeMap<(Id, u64, u32), PolicyOutcome>,
    /// Person kind 34 records that wait for their kind 26, hold or drop
    /// (R8 to R10).
    pub(crate) person: BTreeMap<Id, PolicyOutcome>,
    /// The last `activated` kind 35 (PL3 restart).
    pub(crate) manifest: Option<PolicyRevision>,
    /// The last 64 activated revisions, oldest first (PL3 rollback).
    pub(crate) recent: Vec<RecentRevision>,
    /// Open kind 36 grants: the latest record of each (R14).
    pub(crate) grants: BTreeMap<Id, PolicyGrant>,
}
impl PolicyReplay {
    /// The active revision, if any.
    pub fn active(&self) -> Option<(u64, Hash)> {
        self.active
    }
    /// The last `activated` kind 35: the active revision's manifest.
    pub fn active_manifest(&self) -> Option<&PolicyRevision> {
        self.manifest.as_ref()
    }
    /// The last 64 activated revisions, oldest first.
    pub fn recent(&self) -> &[RecentRevision] {
        &self.recent
    }
    /// The open kind 36 grants, by grant ID: the latest record of each.
    pub fn open_grants(&self) -> impl Iterator<Item = &PolicyGrant> {
        self.grants.values()
    }
    /// The kind 34 outcome that waits for the call's kind 14.
    pub fn pending(&self, operation: Id, generation: u64, ordinal: u32) -> Option<PolicyOutcome> {
        self.pending.get(&(operation, generation, ordinal)).copied()
    }
    /// R1, R2, R11 and R12 for one kind 35.
    pub(crate) fn revision(&mut self, p: &PolicyRevision) -> Result<()> {
        require(
            matches!(
                p.event,
                PolicyRevisionEvent::Activated | PolicyRevisionEvent::Rejected
            ) && Some(p.seq) == self.last_seq.checked_add(1)
                && p.previous_seq == self.active.map_or(0, |(seq, _)| seq)
                && p.scopes.iter().all(|s| valid_source(s.scope, &s.source)),
        )?;
        self.last_seq = p.seq;
        if p.event == PolicyRevisionEvent::Activated {
            self.active = Some((p.seq, p.digest));
            if let Some(user) = p.scopes.iter().find(|s| s.scope == 2) {
                if self.recent.len() >= MAX_RECENT_POLICY_REVISIONS {
                    self.recent.remove(0);
                }
                self.recent.push(RecentRevision {
                    seq: p.seq,
                    digest: p.digest,
                    user_text: user.text_digest,
                    user_source: user.source.clone(),
                });
            }
            self.manifest = Some(p.clone());
        }
        Ok(())
    }
    /// R3, R13 and the pending half of R4 for one kind 34; the caller
    /// checks the call's turn and ordinal, or (R8) the person input's
    /// state.
    pub(crate) fn decision(&mut self, p: &PolicyDecision) -> Result<()> {
        require(
            p.outcome != PolicyOutcome::Ask
                && self.active == Some((p.revision_seq, p.revision_digest)),
        )?;
        match (p.call(), p.subject) {
            (Some(call), _) => {
                require(!self.pending.contains_key(&call))?;
                if self.pending.len() >= MAX_PENDING_POLICY_DECISIONS {
                    return Err(ReplayError::Limit);
                }
                self.pending.insert(call, p.outcome);
            }
            (None, PolicySubject::PersonInput { input }) => {
                // R8: a new decision replaces the input's pending one.
                if !self.person.contains_key(&input)
                    && self.person.len() >= MAX_PENDING_PERSON_DECISIONS
                {
                    return Err(ReplayError::Limit);
                }
                self.person.insert(input, p.outcome);
            }
            // R13: a recheck adds no pending entry.
            (None, PolicySubject::JobRecheck { .. }) => {}
            (None, PolicySubject::ModelCall { .. }) => return Err(ReplayError::Corrupt),
        }
        Ok(())
    }
    /// R14 for one kind 36.
    pub(crate) fn grant(&mut self, p: &PolicyGrant) -> Result<()> {
        use PolicyGrantEvent as E;
        require(
            p.kind == PolicyGrantKind::Approval
                && self.active.map(|(seq, _)| seq) == Some(p.revision_seq),
        )?;
        let open = self.grants.get(&p.grant);
        match p.event {
            E::Requested => {
                require(open.is_none())?;
                if self.grants.len() >= MAX_OPEN_POLICY_GRANTS {
                    return Err(ReplayError::Limit);
                }
            }
            event => {
                let open = open.ok_or(ReplayError::Corrupt)?;
                let state = matches!(
                    (open.event, event),
                    (E::Requested, E::Granted | E::Denied | E::Cancelled)
                        | (E::Granted, E::Ended | E::Cancelled)
                );
                require(
                    state
                        && open.kind == p.kind
                        && open.job == p.job
                        && open.task_conversation == p.task_conversation
                        && open.task_agent == p.task_agent,
                )?;
            }
        }
        if matches!(p.event, E::Denied | E::Cancelled | E::Ended) {
            self.grants.remove(&p.grant);
        } else {
            self.grants.insert(p.grant, p.clone());
        }
        Ok(())
    }
    /// The person kind 34 outcome that waits for the input's kind 26.
    pub fn person_pending(&self, input: Id) -> Option<PolicyOutcome> {
        self.person.get(&input).copied()
    }
    /// R9 for a person-origin kind 26: a pending decision must be `run`.
    pub(crate) fn person_job(&mut self, input: Id) -> Result<()> {
        match self.person.remove(&input) {
            Some(PolicyOutcome::Deny | PolicyOutcome::Ask) => Err(ReplayError::Corrupt),
            Some(PolicyOutcome::Run) | None => Ok(()),
        }
    }
    /// R10 for a hold or drop decision of the input.
    pub(crate) fn person_settled(&mut self, input: Id) {
        self.person.remove(&input);
    }
    /// R5 to R7 for one kind 14: its pending kind 34 is cleared.
    pub(crate) fn tool_call(&mut self, p: &ToolCall) -> Result<()> {
        let pending = self.pending.remove(&(p.operation, p.generation, p.ordinal));
        if p.reason == Some(ToolDecisionReason::PolicyDenied) {
            require(pending == Some(PolicyOutcome::Deny))?;
        }
        if pending == Some(PolicyOutcome::Deny) {
            require(p.decision != ToolDecision::Run)?;
        }
        Ok(())
    }
    /// R7 for one kind 8: a pending kind 34 of the turn is allowed only
    /// with an `interrupted` terminal, which clears it.
    pub(crate) fn terminal(
        &mut self,
        operation: Id,
        generation: u64,
        interrupted: bool,
    ) -> Result<()> {
        let any = self
            .pending
            .keys()
            .any(|(op, g, _)| *op == operation && *g == generation);
        if any {
            require(interrupted)?;
            self.pending
                .retain(|(op, g, _), _| !(*op == operation && *g == generation));
        }
        Ok(())
    }
    /// The checkpoint encoding of every policy fact.
    pub(crate) fn write_checkpoint(&self, w: &mut Writer) -> Result<()> {
        w.u64(self.last_seq);
        w.byte(u8::from(self.active.is_some()));
        if let Some((seq, digest)) = self.active {
            w.u64(seq);
            w.bytes(&digest);
        }
        require(self.pending.len() <= MAX_PENDING_POLICY_DECISIONS)?;
        w.u32(self.pending.len() as u32);
        for ((operation, generation, ordinal), outcome) in &self.pending {
            w.bytes(operation);
            w.u64(*generation);
            w.u32(*ordinal);
            w.byte(*outcome as u8);
        }
        // PL2: pending person kind 34 records (R8 to R10).
        require(self.person.len() <= MAX_PENDING_PERSON_DECISIONS)?;
        w.u32(self.person.len() as u32);
        for (input, outcome) in &self.person {
            w.bytes(input);
            w.byte(*outcome as u8);
        }
        // PL3: the active manifest, the recent revisions and the open
        // grants.
        w.byte(u8::from(self.manifest.is_some()));
        if let Some(manifest) = &self.manifest {
            write_policy_revision(w, manifest)?;
        }
        require(self.recent.len() <= MAX_RECENT_POLICY_REVISIONS)?;
        w.u32(self.recent.len() as u32);
        for r in &self.recent {
            w.u64(r.seq);
            w.bytes(&r.digest);
            w.bytes(&r.user_text);
            w.string(&r.user_source, MAX_POLICY_ID)?;
        }
        require(self.grants.len() <= MAX_OPEN_POLICY_GRANTS)?;
        w.u32(self.grants.len() as u32);
        for grant in self.grants.values() {
            write_policy_grant(w, grant)?;
        }
        Ok(())
    }
    /// Restores checkpointed facts with their consistency checks.
    pub(crate) fn read_checkpoint(r: &mut Reader<'_>) -> Result<Self> {
        let last_seq = r.u64()?;
        let active = if r.flag()? {
            Some((r.u64()?, r.array()?))
        } else {
            None
        };
        let count = r.u32()? as usize;
        require(count <= MAX_PENDING_POLICY_DECISIONS)?;
        let mut pending = BTreeMap::new();
        for _ in 0..count {
            let key: (Id, u64, u32) = (r.array()?, r.u64()?, r.u32()?);
            let outcome = PolicyOutcome::from_code(r.byte()?).ok_or(ReplayError::Corrupt)?;
            require(pending.last_key_value().is_none_or(|(k, _)| *k < key))?;
            pending.insert(key, outcome);
        }
        let count = r.u32()? as usize;
        require(count <= MAX_PENDING_PERSON_DECISIONS)?;
        let mut person = BTreeMap::new();
        for _ in 0..count {
            let input: Id = r.array()?;
            let outcome = PolicyOutcome::from_code(r.byte()?).ok_or(ReplayError::Corrupt)?;
            require(person.last_key_value().is_none_or(|(k, _)| *k < input))?;
            person.insert(input, outcome);
        }
        let manifest = if r.flag()? {
            Some(read_policy_revision(r)?)
        } else {
            None
        };
        let count = r.u32()? as usize;
        require(count <= MAX_RECENT_POLICY_REVISIONS)?;
        let mut recent: Vec<RecentRevision> = Vec::with_capacity(count);
        for _ in 0..count {
            let entry = RecentRevision {
                seq: r.u64()?,
                digest: r.array()?,
                user_text: r.array()?,
                user_source: r.bounded_string(MAX_POLICY_ID)?,
            };
            require(
                recent.last().is_none_or(|last| last.seq < entry.seq)
                    && valid_source(2, &entry.user_source),
            )?;
            recent.push(entry);
        }
        let count = r.u32()? as usize;
        require(count <= MAX_OPEN_POLICY_GRANTS)?;
        let mut grants: BTreeMap<Id, PolicyGrant> = BTreeMap::new();
        for _ in 0..count {
            let grant = read_policy_grant(r)?;
            require(
                grants
                    .last_key_value()
                    .is_none_or(|(k, _)| *k < grant.grant)
                    && grant.kind == PolicyGrantKind::Approval
                    && matches!(
                        grant.event,
                        PolicyGrantEvent::Requested | PolicyGrantEvent::Granted
                    ),
            )?;
            grants.insert(grant.grant, grant);
        }
        Self::restore(Restored {
            last_seq,
            active,
            pending,
            person,
            manifest,
            recent,
            grants,
        })
    }
    /// Restores checkpointed facts; `seq`, `active` and the manifest are
    /// checked against each other.
    pub(crate) fn restore(f: Restored) -> Result<Self> {
        let manifest_ok = match (&f.manifest, f.active) {
            (None, None) => true,
            (Some(m), Some((seq, digest))) => {
                m.event == PolicyRevisionEvent::Activated && m.seq == seq && m.digest == digest
            }
            _ => false,
        };
        require(
            f.active
                .is_none_or(|(seq, _)| seq >= 1 && seq <= f.last_seq)
                && (f.last_seq > 0 || f.active.is_none())
                && manifest_ok
                && f.recent.len() <= MAX_RECENT_POLICY_REVISIONS
                && f.recent.last().is_none_or(|r| r.seq <= f.last_seq)
                && f.pending.len() <= MAX_PENDING_POLICY_DECISIONS
                && f.pending.values().all(|o| *o != PolicyOutcome::Ask)
                && f.person.len() <= MAX_PENDING_PERSON_DECISIONS
                && f.person.values().all(|o| *o != PolicyOutcome::Ask)
                && (f.person.is_empty() || f.active.is_some())
                && f.grants.len() <= MAX_OPEN_POLICY_GRANTS
                && (f.grants.is_empty() || f.active.is_some()),
        )?;
        Ok(Self {
            last_seq: f.last_seq,
            active: f.active,
            pending: f.pending,
            person: f.person,
            manifest: f.manifest,
            recent: f.recent,
            grants: f.grants,
        })
    }
}
/// The checkpointed policy facts before their checks.
pub(crate) struct Restored {
    pub last_seq: u64,
    pub active: Option<(u64, Hash)>,
    pub pending: BTreeMap<(Id, u64, u32), PolicyOutcome>,
    pub person: BTreeMap<Id, PolicyOutcome>,
    pub manifest: Option<PolicyRevision>,
    pub recent: Vec<RecentRevision>,
    pub grants: BTreeMap<Id, PolicyGrant>,
}
