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

/// What a kind 34 decides about. Codes 2 (person input) and 3 (job
/// recheck) are reserved for later packets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PolicySubject {
    /// Code 1: one model tool call.
    ModelCall {
        operation: Id,
        generation: u64,
        ordinal: u32,
    },
}
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
        let PolicySubject::ModelCall {
            operation,
            generation,
            ordinal,
        } = self.subject;
        let subject = operation != [0; 16] && generation >= 1 && ordinal >= 1;
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
                all_allow && self.primary_scope == 0 && self.constraint_digest.is_some()
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
    /// The subject's call key.
    pub fn call(&self) -> (Id, u64, u32) {
        let PolicySubject::ModelCall {
            operation,
            generation,
            ordinal,
        } = self.subject;
        (operation, generation, ordinal)
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
    let PolicySubject::ModelCall {
        operation,
        generation,
        ordinal,
    } = p.subject;
    w.byte(1);
    w.bytes(&operation);
    w.u64(generation);
    w.u32(ordinal);
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

/// A kind 35 event. Replay accepts only `Activated` until the packet that
/// writes the others (R2).
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

/// The policy facts replay keeps (R1 to R7): the last kind 35 sequence,
/// the active revision and the kind 34 records that wait for their kind 14.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct PolicyReplay {
    /// The last kind 35 sequence; 0 before the first.
    pub last_seq: u64,
    /// The active revision's sequence and digest.
    pub active: Option<(u64, Hash)>,
    pub(crate) pending: BTreeMap<(Id, u64, u32), PolicyOutcome>,
}
impl PolicyReplay {
    /// The active revision, if any.
    pub fn active(&self) -> Option<(u64, Hash)> {
        self.active
    }
    /// The kind 34 outcome that waits for the call's kind 14.
    pub fn pending(&self, operation: Id, generation: u64, ordinal: u32) -> Option<PolicyOutcome> {
        self.pending.get(&(operation, generation, ordinal)).copied()
    }
    /// The pending entries in key order (checkpoint encoding).
    pub(crate) fn pending_entries(
        &self,
    ) -> impl Iterator<Item = (&(Id, u64, u32), &PolicyOutcome)> {
        self.pending.iter()
    }
    /// R1 and R2 for one kind 35.
    pub(crate) fn revision(&mut self, p: &PolicyRevision) -> Result<()> {
        require(
            p.event == PolicyRevisionEvent::Activated
                && Some(p.seq) == self.last_seq.checked_add(1)
                && p.previous_seq == self.active.map_or(0, |(seq, _)| seq),
        )?;
        self.last_seq = p.seq;
        self.active = Some((p.seq, p.digest));
        Ok(())
    }
    /// R3 and the pending half of R4 for one kind 34; the caller checks the
    /// call's turn and ordinal.
    pub(crate) fn decision(&mut self, p: &PolicyDecision) -> Result<()> {
        require(
            p.outcome != PolicyOutcome::Ask
                && self.active == Some((p.revision_seq, p.revision_digest))
                && !self.pending.contains_key(&p.call()),
        )?;
        if self.pending.len() >= MAX_PENDING_POLICY_DECISIONS {
            return Err(ReplayError::Limit);
        }
        self.pending.insert(p.call(), p.outcome);
        Ok(())
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
    /// Restores checkpointed facts; `seq` and `active` are checked.
    pub(crate) fn restore(
        last_seq: u64,
        active: Option<(u64, Hash)>,
        pending: BTreeMap<(Id, u64, u32), PolicyOutcome>,
    ) -> Result<Self> {
        require(
            active.is_none_or(|(seq, _)| seq >= 1 && seq <= last_seq)
                && (last_seq == 0) == active.is_none()
                && pending.len() <= MAX_PENDING_POLICY_DECISIONS
                && pending.values().all(|o| *o != PolicyOutcome::Ask),
        )?;
        Ok(Self {
            last_seq,
            active,
            pending,
        })
    }
}
