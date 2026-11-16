//! Step 4 lints (security-policy.md, PL3 admission): tighten-only
//! (SP-AQ2), forbid-only (SP-AQ5) and path literals. Each walks the
//! policy's JSON form, so a read is found however it is spelled
//! (`context.intent`, `context["intent"]`, `context has intent`).
use crate::{LiteralCheck, LiteralResult};
use asura_policy::Scope;
use cedar_policy::{Effect, Policy};
use serde_json::Value;

/// What one policy reads of the tighten-only context attributes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Reads {
    pub intent: bool,
    pub risk: bool,
}

fn is_context(value: &Value) -> bool {
    value.get("Var").and_then(Value::as_str) == Some("context")
}

fn walk(value: &Value, visit: &mut dyn FnMut(&serde_json::Map<String, Value>)) {
    match value {
        Value::Object(map) => {
            visit(map);
            for child in map.values() {
                walk(child, visit);
            }
        }
        Value::Array(items) => {
            for child in items {
                walk(child, visit);
            }
        }
        _ => {}
    }
}

/// The tighten-only attributes `policy` reads.
pub(crate) fn reads(policy: &Policy) -> Reads {
    let mut reads = Reads::default();
    let Ok(json) = policy.to_json() else {
        // A policy with no JSON form reads everything, so the lint refuses
        // it unless it is an annotated forbid.
        return Reads {
            intent: true,
            risk: true,
        };
    };
    walk(&json, &mut |map| {
        for key in [".", "has"] {
            if let Some(inner) = map.get(key)
                && inner.get("left").is_some_and(is_context)
            {
                match inner.get("attr").and_then(Value::as_str) {
                    Some("intent") => reads.intent = true,
                    Some("risk") => reads.risk = true,
                    _ => {}
                }
            }
        }
    });
    reads
}

/// Every `Asura::Path` entity literal in `policy`, in order of appearance.
pub(crate) fn path_literals(policy: &Policy) -> Vec<String> {
    let mut found = Vec::new();
    if let Ok(json) = policy.to_json() {
        walk(&json, &mut |map| {
            if map.get("type").and_then(Value::as_str) == Some("Asura::Path")
                && let Some(id) = map.get("id").and_then(Value::as_str)
            {
                found.push(id.to_owned());
            }
        });
    }
    found
}

/// One lint finding: the policy and the problem.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Finding {
    pub rule: String,
    pub message: String,
    pub kind: FindingKind,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FindingKind {
    /// A `permit` that reads intent or risk: SP-AQ3 shows it.
    TightenPermit,
    /// An annotation mismatch on a policy.
    TightenAnnotation,
    /// A `permit` in a workspace or repository text: SP-AQ5 shows it.
    LowerPermit,
    Literal,
}

/// Runs the step 4 lints over `policies` (global ID, policy) of `scope`.
pub(crate) fn lint(
    scope: Scope,
    policies: &[(String, Policy)],
    literals: &dyn LiteralCheck,
) -> Vec<Finding> {
    let mut findings = Vec::new();
    let lower = matches!(scope, Scope::Workspace | Scope::Repository);
    for (rule, policy) in policies {
        let mut add = |kind, message: String| {
            findings.push(Finding {
                rule: rule.clone(),
                message,
                kind,
            })
        };
        let read = reads(policy);
        let marked = |name| policy.annotation(name) == Some("true");
        for (attr, read, annotation) in [
            ("context.intent", read.intent, "intent"),
            ("context.risk", read.risk, "classifier"),
        ] {
            if read && !marked(annotation) {
                add(
                    FindingKind::TightenAnnotation,
                    format!("reads {attr} without @{annotation}(\"true\")"),
                );
            }
            if !read && marked(annotation) {
                add(
                    FindingKind::TightenAnnotation,
                    format!("@{annotation}(\"true\") on a policy that does not read {attr}"),
                );
            }
        }
        if (read.intent || read.risk) && policy.effect() == Effect::Permit {
            add(
                FindingKind::TightenPermit,
                "a policy that reads intent or risk must be a forbid".into(),
            );
        }
        if lower && policy.effect() == Effect::Permit {
            add(
                FindingKind::LowerPermit,
                format!("a {} policy may only forbid", scope.as_str()),
            );
        }
        for literal in path_literals(policy) {
            if !asura_policy::lexically_canonical_literal(&literal) {
                add(
                    FindingKind::Literal,
                    format!("path literal is not canonical: {}", printable(&literal)),
                );
                continue;
            }
            if let LiteralResult::Respell(spelling) = literals.check(&literal) {
                add(
                    FindingKind::Literal,
                    format!(
                        "path literal {} is spelled {} on this Mac",
                        printable(&literal),
                        printable(&spelling)
                    ),
                );
            }
        }
    }
    findings
}

/// Printable ASCII with other bytes escaped, for detail lines.
pub(crate) fn printable(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if (' '..='~').contains(&c) {
            out.push(c);
        } else {
            out.extend(c.escape_default());
        }
    }
    out
}
