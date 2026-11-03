//! Typed request model shared by both engines (design: "one typed request
//! model with two encoders"). Its JSON form is also the Rego input.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::net::IpAddr;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Kind {
    Job,
    Task,
    Path,
    Executable,
    Domain,
    Address,
    Secret,
    Destination,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Job => "Job",
            Kind::Task => "Task",
            Kind::Path => "Path",
            Kind::Executable => "Executable",
            Kind::Domain => "Domain",
            Kind::Address => "Address",
            Kind::Secret => "Secret",
            Kind::Destination => "Destination",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Uid {
    #[serde(rename = "type")]
    pub kind: Kind,
    pub id: String,
}

impl Uid {
    pub fn new(kind: Kind, id: impl Into<String>) -> Self {
        Uid {
            kind,
            id: id.into(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    Exec,
    ExecRequestApproval,
    ExecApproved,
    Write,
    Connect,
    SecretUse,
    Egress,
}

impl Action {
    pub fn as_str(self) -> &'static str {
        match self {
            Action::Exec => "exec",
            Action::ExecRequestApproval => "exec_request_approval",
            Action::ExecApproved => "exec_approved",
            Action::Write => "write",
            Action::Connect => "connect",
            Action::SecretUse => "secret_use",
            Action::Egress => "egress",
        }
    }

    pub fn parse(s: &str) -> Option<Action> {
        serde_json::from_value(Value::String(s.to_string())).ok()
    }

    /// Principal and resource kinds that the schema allows for this action.
    fn kinds(self) -> (Kind, &'static [Kind]) {
        match self {
            Action::Exec | Action::ExecRequestApproval | Action::ExecApproved => {
                (Kind::Job, &[Kind::Executable])
            }
            Action::Write => (Kind::Job, &[Kind::Path]),
            Action::Connect => (Kind::Job, &[Kind::Domain, Kind::Address]),
            Action::SecretUse => (Kind::Job, &[Kind::Secret]),
            Action::Egress => (Kind::Task, &[Kind::Destination]),
        }
    }
}

macro_rules! closed_enum {
    ($name:ident { $($variant:ident => $text:literal),* $(,)? }) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
        pub enum $name { $( #[serde(rename = $text)] $variant ),* }
        impl $name {
            pub fn as_str(self) -> &'static str { match self { $( $name::$variant => $text ),* } }
        }
    };
}

closed_enum!(Parse { Simple => "simple", Compound => "compound", Unparsed => "unparsed" });
closed_enum!(Risk { Safe => "safe", Moderate => "moderate", Dangerous => "dangerous", Unknown => "unknown" });
closed_enum!(Delivery { Raw => "raw", Brokered => "brokered" });
closed_enum!(DataClass {
    Prompt => "prompt", History => "history", Notice => "notice",
    FilePage => "file_page", ToolResult => "tool_result",
});

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Command {
    pub parse: Parse,
    pub subcommand: String,
    pub flags: BTreeSet<String>,
    pub shell_features: BTreeSet<String>,
    pub env_terminal: BTreeSet<String>,
    pub env_declared: BTreeSet<String>,
    pub secret_vars: i64,
    pub secret_reference: bool,
}

impl Command {
    pub fn simple(subcommand: &str) -> Self {
        Command {
            parse: Parse::Simple,
            subcommand: subcommand.to_string(),
            flags: BTreeSet::new(),
            shell_features: BTreeSet::new(),
            env_terminal: BTreeSet::new(),
            env_declared: BTreeSet::new(),
            secret_vars: 0,
            secret_reference: false,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecCtx {
    pub now_ms: i64,
    pub request_digest: String,
    pub command: Command,
    pub outside_switch: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub risk: Option<Risk>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WriteCtx {
    pub now_ms: i64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConnectCtx {
    pub now_ms: i64,
    pub ip: String,
    pub port: i64,
    pub via_proxy: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SecretCtx {
    pub now_ms: i64,
    pub delivery: Delivery,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EgressCtx {
    pub now_ms: i64,
    pub data_class: DataClass,
    pub tainted: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Context {
    Exec(ExecCtx),
    Write(WriteCtx),
    Connect(ConnectCtx),
    SecretUse(SecretCtx),
    Egress(EgressCtx),
}

impl Context {
    pub fn now_ms(&self) -> i64 {
        match self {
            Context::Exec(c) => c.now_ms,
            Context::Write(c) => c.now_ms,
            Context::Connect(c) => c.now_ms,
            Context::SecretUse(c) => c.now_ms,
            Context::Egress(c) => c.now_ms,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Request {
    pub principal: Uid,
    pub action: Action,
    pub resource: Uid,
    pub context: Context,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRequest {
    principal: Uid,
    action: Action,
    resource: Uid,
    context: Value,
}

impl Request {
    /// The engine-neutral JSON form, which is also the Rego `input`.
    pub fn to_json(&self) -> Value {
        let context = match &self.context {
            Context::Exec(c) => serde_json::to_value(c),
            Context::Write(c) => serde_json::to_value(c),
            Context::Connect(c) => serde_json::to_value(c),
            Context::SecretUse(c) => serde_json::to_value(c),
            Context::Egress(c) => serde_json::to_value(c),
        }
        .unwrap_or(Value::Null);
        json!({
            "principal": self.principal,
            "action": self.action,
            "resource": self.resource,
            "context": context,
        })
    }

    /// Typed validation of an engine-neutral JSON request (wrapper rule W2
    /// for the Rego wrapper). Every field is required unless the schema marks
    /// it optional; unknown fields, wrong types and wrong kinds are refused.
    pub fn from_json(v: &Value) -> Result<Request, String> {
        let raw: RawRequest = RawRequest::deserialize(v).map_err(|e| e.to_string())?;
        let (principal_kind, resource_kinds) = raw.action.kinds();
        if raw.principal.kind != principal_kind {
            return Err("principal kind does not fit the action".into());
        }
        if !resource_kinds.contains(&raw.resource.kind) {
            return Err("resource kind does not fit the action".into());
        }
        fn ctx<T: for<'de> Deserialize<'de>>(v: Value) -> Result<T, String> {
            serde_json::from_value(v).map_err(|e| e.to_string())
        }
        let context = match raw.action {
            Action::Exec | Action::ExecRequestApproval | Action::ExecApproved => {
                Context::Exec(ctx(raw.context)?)
            }
            Action::Write => Context::Write(ctx(raw.context)?),
            Action::Connect => {
                let c: ConnectCtx = ctx(raw.context)?;
                c.ip.parse::<IpAddr>().map_err(|e| e.to_string())?;
                if !(0..=65_535).contains(&c.port) {
                    return Err("port out of range".into());
                }
                Context::Connect(c)
            }
            Action::SecretUse => Context::SecretUse(ctx(raw.context)?),
            Action::Egress => Context::Egress(ctx(raw.context)?),
        };
        if context.now_ms() < 0 {
            return Err("time before the epoch".into());
        }
        Ok(Request {
            principal: raw.principal,
            action: raw.action,
            resource: raw.resource,
            context,
        })
    }
}

/// RFC 3339 UTC text for milliseconds since the epoch (proleptic Gregorian).
pub fn rfc3339(ms: i64) -> String {
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

/// 2026-11-03T12:00:00Z, the default `now` of the corpus.
pub const NOW_MS: i64 = 1_793_707_200_000;
/// 2026-11-04T00:00:00Z, after every envelope expiry.
pub const LATE_MS: i64 = 1_793_750_400_000;
/// 2026-11-03T18:00:00Z, the envelope and approval expiry.
pub const EXPIRY_MS: i64 = 1_793_728_800_000;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc3339_matches_known_instants() {
        assert_eq!(rfc3339(NOW_MS), "2026-11-03T12:00:00Z");
        assert_eq!(rfc3339(EXPIRY_MS), "2026-11-03T18:00:00Z");
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(951_782_400_123), "2000-02-29T00:00:00.123Z");
    }

    #[test]
    fn typed_validation_refuses_unknown_and_mistyped_fields() {
        let ok = json!({"principal": {"type": "Job", "id": "j"}, "action": "write",
            "resource": {"type": "Path", "id": "/x"}, "context": {"now_ms": 1}});
        assert!(Request::from_json(&ok).is_ok());
        let mut extra = ok.clone();
        extra["context"]["x"] = json!(1);
        assert!(Request::from_json(&extra).is_err());
        let mut wrong = ok.clone();
        wrong["context"]["now_ms"] = json!("1");
        assert!(Request::from_json(&wrong).is_err());
        let mut kind = ok;
        kind["resource"]["type"] = json!("Secret");
        assert!(Request::from_json(&kind).is_err());
    }
}
