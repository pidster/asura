//! The decision record shared by both wrappers, and the input bounds of
//! wrapper rule W1.

use serde_json::Value;
use std::collections::BTreeSet;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ErrorClass {
    RequestTooLarge,
    RequestInvalid,
    PolicyError,
    EngineError,
    ResultInvalid,
}

impl ErrorClass {
    pub fn reason(self) -> &'static str {
        match self {
            ErrorClass::RequestTooLarge => "request_too_large",
            ErrorClass::RequestInvalid => "request_invalid",
            ErrorClass::PolicyError => "policy_error",
            ErrorClass::EngineError => "engine_error",
            ErrorClass::ResultInvalid => "result_invalid",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Decision {
    pub allow: bool,
    /// Determining rule IDs (permits for Allow, forbids for Deny, erroring
    /// policies for `policy_error`).
    pub rules: BTreeSet<String>,
    /// Stable reason codes.
    pub reasons: BTreeSet<String>,
    pub error: Option<ErrorClass>,
}

impl Decision {
    pub fn deny_error(class: ErrorClass, rules: BTreeSet<String>) -> Self {
        Decision {
            allow: false,
            rules,
            reasons: BTreeSet::from([class.reason().to_string()]),
            error: Some(class),
        }
    }

    pub fn no_permit() -> Self {
        Decision {
            allow: false,
            rules: BTreeSet::new(),
            reasons: BTreeSet::from(["no_permit".to_string()]),
            error: None,
        }
    }

    /// A one-line stable rendering for digests and logs.
    pub fn render(&self) -> String {
        format!(
            "{}|{}|{}|{:?}",
            if self.allow { "allow" } else { "deny" },
            self.rules.iter().cloned().collect::<Vec<_>>().join(","),
            self.reasons.iter().cloned().collect::<Vec<_>>().join(","),
            self.error
        )
    }
}

pub const MAX_REQUEST_BYTES: usize = 16 * 1024;
pub const MAX_STRING_BYTES: usize = 1024;
pub const MAX_SET_ELEMENTS: usize = 64;
pub const MAX_DEPTH: usize = 8;

/// W1: refuse oversized input before any engine sees it. `bytes` is the
/// serialised size when the caller already has it.
pub fn check_bounds(v: &Value, bytes: usize) -> Result<(), String> {
    if bytes > MAX_REQUEST_BYTES {
        return Err(format!("request of {bytes} bytes"));
    }
    fn walk(v: &Value, depth: usize) -> Result<(), String> {
        if depth > MAX_DEPTH {
            return Err("nesting too deep".into());
        }
        match v {
            Value::String(s) if s.len() > MAX_STRING_BYTES => Err("string too long".into()),
            Value::Array(a) => {
                if a.len() > MAX_SET_ELEMENTS {
                    return Err("set too large".into());
                }
                a.iter().try_for_each(|x| walk(x, depth + 1))
            }
            Value::Object(o) => {
                if o.len() > MAX_SET_ELEMENTS {
                    return Err("record too large".into());
                }
                o.iter().try_for_each(|(k, x)| {
                    if k.len() > MAX_STRING_BYTES {
                        return Err("key too long".into());
                    }
                    walk(x, depth + 1)
                })
            }
            _ => Ok(()),
        }
    }
    walk(v, 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn bounds_refuse_each_limit() {
        assert!(check_bounds(&json!({"a": "x"}), 10).is_ok());
        assert!(check_bounds(&json!({}), MAX_REQUEST_BYTES + 1).is_err());
        assert!(check_bounds(&json!({"a": "x".repeat(1025)}), 10).is_err());
        assert!(check_bounds(&json!({"a": vec![1; 65]}), 10).is_err());
        let mut deep = json!(1);
        for _ in 0..10 {
            deep = json!({ "a": deep });
        }
        assert!(check_bounds(&deep, 10).is_err());
    }

    #[test]
    fn error_decisions_deny_with_their_class() {
        let d = Decision::deny_error(ErrorClass::PolicyError, BTreeSet::from(["p".into()]));
        assert!(!d.allow);
        assert!(d.reasons.contains("policy_error"));
        assert!(Decision::no_permit().reasons.contains("no_permit"));
    }
}
