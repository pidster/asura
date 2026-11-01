//! `/secrets` in the TUI (named-secrets.md, the offer): list, store, replace
//! and remove named secrets. A store reads the value from the captured
//! terminal environment only; the value never reaches a notice, a panel or
//! the editor. Rendering and key handling perform no I/O: the secrets
//! worker sends the request.
use super::person::Launch;
use super::*;
use crate::tui::secrets::{Kind, Outcome, Request, Update};
use asura_control::pb;
use asura_control::secrets::Zeroizing;
use asura_platform::secrets::named;

const USAGE: &str =
    "Use /secrets, /secrets store NAME, /secrets replace NAME or /secrets remove NAME";
/// The name rule's text (NS-U12).
const INVALID: &str =
    "is not a valid secret name: 1 to 48 letters, digits or _, not starting with a digit";

/// `/secrets` state beside the person log.
#[derive(Default)]
pub(super) struct Secrets {
    /// The parsed request and, for a store or replace, its value.
    pub request: Option<(Request, Option<Zeroizing<Vec<u8>>>)>,
    pub busy: bool,
    pub cancel: bool,
}

/// `/secrets` arguments.
pub(super) fn parse(draft: &str) -> Result<Kind, String> {
    let rest = draft
        .trim()
        .strip_prefix("/secrets")
        .ok_or_else(|| USAGE.to_owned())?;
    if rest.is_empty() {
        return Ok(Kind::List);
    }
    let rest = rest
        .strip_prefix(char::is_whitespace)
        .ok_or_else(|| USAGE.to_owned())?;
    let mut words = rest.split_whitespace();
    let (Some(verb), Some(name), None) = (words.next(), words.next(), words.next()) else {
        return Err(USAGE.into());
    };
    if !named::name_valid(name) {
        return Err(format!("{} {INVALID}", literal(name, 64)));
    }
    let name = name.to_owned();
    match verb {
        "store" => Ok(Kind::Store(name)),
        "replace" => Ok(Kind::Replace(name)),
        "remove" => Ok(Kind::Remove(name)),
        _ => Err(USAGE.into()),
    }
}

/// The notice of a reply (named-secrets.md, `/secrets` command).
pub(super) fn reply_notice(kind: &Kind, reply: &pb::SecretReply) -> String {
    use pb::SecretResult as R;
    let name = match kind {
        Kind::Store(n) | Kind::Replace(n) | Kind::Remove(n) => literal(n, 48),
        Kind::List => String::new(),
    };
    let status = reply.status.unwrap_or(0);
    match R::try_from(reply.result.unwrap_or(0)).unwrap_or(R::Unspecified) {
        R::Stored => format!(
            "Stored {name} as a named secret. Use $ASURA_SECRET_{name} in a ! or !! command."
        ),
        R::Replaced => format!("Replaced the named secret {name}."),
        R::Exists => format!(
            "A named secret {name} exists. Replace it with /secrets replace {name}, or keep it."
        ),
        R::Removed => format!(
            "Removed the named secret {name}. Commands already queued keep their copy."
        ),
        R::Absent => format!("No named secret {name} exists."),
        R::Listed => "Secrets".into(),
        R::ReplaceFailed => format!(
            "{name} could not be replaced and is no longer stored: the Keychain failed (OSStatus -{status}). Store it again with /secrets store {name}."
        ),
        R::Locked => "The Keychain is locked until this Mac is unlocked after start-up. Unlock it, then try again.".into(),
        R::NotSigned => "This Asura build is not signed for the Keychain, so it cannot store secrets.".into(),
        R::Unavailable => format!("The Keychain is unavailable (OSStatus -{status})."),
        R::Denied => "macOS denied Asura access to the Keychain item.".into(),
        R::Full => "The secret store is full: 64 named secrets or 256 items. Remove one with /secrets remove NAME.".into(),
        R::Busy => "Keychain requests are queued; try again.".into(),
        R::Invalid => format!("{name} {INVALID}"),
        R::Unspecified => "Secrets request failed.".into(),
    }
}

/// The "Secrets" panel: the stored names, then each flagged name of this
/// terminal with its kind and its action. Names and kinds only.
pub(super) fn panel_text(stored: &[String], launch: &Launch) -> String {
    let mut lines = vec!["Stored named secrets".to_owned()];
    if stored.is_empty() {
        lines.push("None".into());
    }
    lines.extend(stored.iter().map(|n| format!("  {}", literal(n, 48))));
    lines.push(String::new());
    lines.push("Flagged in this terminal (not passed to commands)".into());
    match launch.scan.get() {
        None => lines.push("Checking this terminal's environment".into()),
        Some(scan) if scan.flagged.is_empty() => lines.push("None".into()),
        Some(scan) => {
            for (name, finding) in &scan.flagged {
                let action = if stored.iter().any(|s| s == name) {
                    format!("stored; use $ASURA_SECRET_{name}")
                } else if named::name_valid(name) {
                    format!("/secrets store {name}")
                } else {
                    "this name cannot be stored".into()
                };
                lines.push(format!(
                    "  {} ({}): {action}",
                    literal(name, 64),
                    finding.label()
                ));
            }
        }
    }
    lines.push(String::new());
    lines.push(
        "A command uses a stored secret by naming $ASURA_SECRET_<NAME>; its output is then private to you."
            .into(),
    );
    lines.join("\n")
}

impl App {
    pub(super) fn secrets_command(&mut self, draft: String) {
        if self.person.secrets.busy {
            self.notice = "Secrets request pending; draft retained".into();
            return;
        }
        if self.person.supported != Some(true) {
            self.notice = crate::tui::queue::COMMANDS_NOT_ENABLED.into();
            return;
        }
        let kind = match parse(&draft) {
            Ok(kind) => kind,
            Err(text) => {
                self.notice = text;
                return;
            }
        };
        let value = match &kind {
            Kind::Store(name) | Kind::Replace(name) => {
                let Some(value) = self.person.launch.value(name) else {
                    self.notice = format!(
                        "{} is not set in this terminal; draft retained",
                        literal(name, 48)
                    );
                    return;
                };
                if value.is_empty() || value.len() > asura_control::secrets::MAX_VALUE_BYTES {
                    self.notice = format!(
                        "{} cannot be stored: its value is empty or over 4,096 bytes",
                        literal(name, 48)
                    );
                    return;
                }
                Some(value)
            }
            _ => None,
        };
        self.person.secrets.request = Some((Request { kind, draft }, value));
        self.person.secrets.busy = true;
        self.notice = "Secrets request pending · Esc stops waiting".into();
    }
    pub(in super::super) fn take_secrets_request(
        &mut self,
    ) -> Option<(Request, Option<Zeroizing<Vec<u8>>>)> {
        self.person.secrets.request.take()
    }
    pub(in super::super) fn take_secrets_cancel(&mut self) -> bool {
        std::mem::take(&mut self.person.secrets.cancel)
    }
    /// `/cancel` or Escape while a secrets request waits.
    pub(super) fn secrets_stop(&mut self) -> bool {
        if !self.person.secrets.busy {
            return false;
        }
        self.person.secrets.cancel = true;
        self.notice = "Secrets request wait stopped; the store or remove may still apply; /secrets lists the names".into();
        true
    }
    pub(in super::super) fn secrets_finished(&mut self, update: Update) {
        self.person.secrets.busy = false;
        let request = update.request;
        match update.outcome {
            Outcome::Reply(reply) => {
                if self.editor.text() == request.draft {
                    self.editor = Editor::new();
                }
                self.notice = reply_notice(&request.kind, &reply);
                if request.kind == Kind::List
                    && reply.result == Some(pb::SecretResult::Listed as i32)
                {
                    self.result_title = "Secrets";
                    self.config_result = Some(panel_text(&reply.names, &self.person.launch));
                }
            }
            Outcome::Definite(error) => {
                self.notice = format!("{}; draft retained", literal(&error, 256));
            }
            Outcome::Unconfirmed(error) => {
                self.notice = format!(
                    "{}; the outcome is unknown; /secrets lists the names",
                    literal(&error, 256)
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use asura_platform::secrets::environment::Finding;

    fn reply(result: pb::SecretResult, status: Option<u32>) -> pb::SecretReply {
        pb::SecretReply {
            result: Some(result as i32),
            names: Vec::new(),
            status,
        }
    }

    /// NS-U12 (`/secrets`): parse, notices and the panel.
    #[test]
    fn ns_u12_secrets_command() {
        assert_eq!(parse("/secrets"), Ok(Kind::List));
        assert_eq!(
            parse("/secrets store GITHUB_TOKEN"),
            Ok(Kind::Store("GITHUB_TOKEN".into()))
        );
        assert_eq!(parse("/secrets replace A"), Ok(Kind::Replace("A".into())));
        assert_eq!(parse("/secrets remove A"), Ok(Kind::Remove("A".into())));
        for bad in [
            "/secrets store",
            "/secrets drop A",
            "/secrets store A B",
            "/secretsx",
        ] {
            assert_eq!(parse(bad), Err(USAGE.into()), "{bad}");
        }
        assert!(parse("/secrets store 1A").unwrap_err().contains(INVALID));
        assert!(parse("/secrets store COMMAND").is_err());
        let store = Kind::Store("GITHUB_TOKEN".into());
        use pb::SecretResult as R;
        assert_eq!(
            reply_notice(&store, &reply(R::Stored, None)),
            "Stored GITHUB_TOKEN as a named secret. Use $ASURA_SECRET_GITHUB_TOKEN in a ! or !! command."
        );
        assert_eq!(
            reply_notice(&store, &reply(R::Exists, None)),
            "A named secret GITHUB_TOKEN exists. Replace it with /secrets replace GITHUB_TOKEN, or keep it."
        );
        assert!(reply_notice(&store, &reply(R::ReplaceFailed, Some(25_291))).contains("-25291"));
        assert!(reply_notice(&store, &reply(R::Locked, Some(25_308))).contains("locked"));
        assert!(reply_notice(&store, &reply(R::NotSigned, None)).contains("not signed"));
        assert!(reply_notice(&store, &reply(R::Full, None)).contains("64 named secrets"));
        let launch = Launch::scanned(
            None,
            vec![(b"GITHUB_TOKEN".to_vec(), b"fixture".to_vec())],
            vec![
                (
                    "GITHUB_TOKEN".into(),
                    Finding::Rule(asura_platform::secrets::SecretKind::GithubToken),
                ),
                ("OPENAI".into(), Finding::HighEntropy),
                ("A".repeat(49), Finding::HighEntropy),
            ],
        );
        let text = panel_text(&["GITHUB_TOKEN".into()], &launch);
        assert!(
            text.contains("GITHUB_TOKEN (github-token): stored"),
            "{text}"
        );
        assert!(
            text.contains("OPENAI (high-entropy): /secrets store OPENAI"),
            "{text}"
        );
        assert!(text.contains("this name cannot be stored"), "{text}");
        assert!(!text.contains("fixture"), "no value: {text}");
    }
}
