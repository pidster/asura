//! The model shell path at `ToolCall` (J1, SL3): rewrite before any record,
//! refusal records that never hold a value, admission and kind 22.
use crate::capture_worker;
use crate::tools::{self, Arguments};
use asura_platform::secrets::{
    self, MAX_REWRITTEN_COMMAND_BYTES, Refusal, RefusalCode, SecretVariable, display_redacted,
};
use asura_storage::authority::conversation as journal;
use asura_storage::payload::{ObjectKind, PayloadStore, Victim, payload_id};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use zeroize::{Zeroize, Zeroizing};

/// The text recorded for a refused command or cwd when the rule set failed
/// its self-check: nothing can be redacted, so nothing is kept (G9).
pub(super) const WITHHELD: &str = "[withheld]";
const MAX_CWD_BYTES: usize = 1024;

/// A shell call after the rewrite step. The raw command is gone.
pub(super) enum Prepared {
    Rewritten(Vec<SecretVariable>),
    Refused { refusal: Refusal, truncated: bool },
}

/// The live state of one shell call in the conversation owner.
pub(super) struct ShellLive {
    pub intent: journal::ShellIntent,
    /// Secret variables, moved into the request at dispatch.
    pub secrets: Vec<SecretVariable>,
    pub victims: Vec<Victim>,
    pub store: Option<Arc<PayloadStore>>,
    /// Kind 23 waiting to append, then whether it committed.
    pub settled: Option<journal::CaptureSettled>,
    pub settled_committed: bool,
    /// The capture worker after settlement, until it moves to the
    /// pending-install list.
    pub capture: Option<capture_worker::Handle>,
}
impl ShellLive {
    pub(super) fn admitted(&self) -> bool {
        self.intent.disposition == journal::ShellDisposition::Admitted
    }
    /// An admitted call that never started: its reservation and victims return.
    pub(super) fn release_admission(&mut self) {
        if let Some(store) = self.store.take() {
            store.restore_victims(&self.victims);
            store.release(self.intent.job);
        }
        self.victims.clear();
        self.secrets.clear();
    }
}

/// Keyed SHA-256 for duplicate detection: never leaves memory (SO2).
fn keyed(key: &[u8; 32], tag: u8, parts: &[&[u8]]) -> [u8; 32] {
    let mut hash = Sha256::new();
    hash.update(key);
    hash.update([tag]);
    for part in parts {
        hash.update((part.len() as u64).to_be_bytes());
        hash.update(part);
    }
    hash.finalize().into()
}

fn cut(text: &str, limit: usize) -> (String, bool) {
    if text.len() <= limit {
        return (text.to_owned(), false);
    }
    let mut end = limit;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    (text[..end].to_owned(), true)
}
fn valid_cwd(cwd: &str) -> bool {
    !cwd.is_empty()
        && cwd.len() <= MAX_CWD_BYTES
        && !cwd.starts_with('/')
        && !cwd.contains('\0')
        && !cwd.split('/').any(|part| part == "..")
}

/// Step 2: rewrites the command in place, scans the cwd and checks the
/// 8,192-byte limit (the planner owns it). The raw command is zeroized; on
/// refusal the call holds the display-redacted command and cwd. Returns the
/// keyed digest of the secret values (or of the raw command when refused).
pub(super) fn prepare(call: &mut tools::Call, key: &[u8; 32]) -> (Prepared, [u8; 32]) {
    let Arguments::Shell { command, cwd, .. } = &mut call.arguments else {
        unreachable!("shell call");
    };
    let raw = Zeroizing::new(std::mem::take(command));
    match secrets::rewrite_shell_call(&raw, cwd) {
        Ok(rewrite) => {
            let values: Vec<&[u8]> = rewrite.variables.iter().map(|v| v.value()).collect();
            let digest = keyed(key, 1, &values);
            *command = rewrite.command;
            (Prepared::Rewritten(rewrite.variables), digest)
        }
        Err(refusal) => {
            let digest = keyed(key, 2, &[raw.as_bytes()]);
            let (display, truncated, display_cwd) = match secrets::detection() {
                Ok(rules) => {
                    let (display, truncated) =
                        cut(&display_redacted(rules, &raw), MAX_REWRITTEN_COMMAND_BYTES);
                    let (redacted_cwd, _) = cut(&display_redacted(rules, cwd), MAX_CWD_BYTES);
                    let redacted_cwd = if valid_cwd(&redacted_cwd) {
                        redacted_cwd
                    } else {
                        WITHHELD.into()
                    };
                    (display, truncated, redacted_cwd)
                }
                Err(_) => (WITHHELD.into(), false, WITHHELD.into()),
            };
            *command = if display.is_empty() || display.contains('\0') {
                WITHHELD.into()
            } else {
                display
            };
            let mut old = std::mem::replace(cwd, display_cwd);
            old.zeroize();
            (Prepared::Refused { refusal, truncated }, digest)
        }
    }
}

fn rule_set(refused_unavailable: bool) -> (u32, journal::Hash) {
    match secrets::detection() {
        Ok(rules) if !refused_unavailable => (rules.revision(), rules.digest()),
        _ => (0, [0; 32]),
    }
}

/// Kind 22 for one call. `call` holds the rewritten (or display-redacted)
/// command; no field holds a value or a value digest.
pub(super) fn intent(
    call: &tools::Call,
    installation: journal::Id,
    prepared: &Prepared,
    busy: Option<journal::ShellBusyReason>,
) -> journal::ShellIntent {
    let Arguments::Shell { command, cwd, .. } = &call.arguments else {
        unreachable!("shell call");
    };
    let (disposition, refusal, truncated) = match prepared {
        Prepared::Refused { refusal, truncated } => (
            journal::ShellDisposition::Refused,
            Some(journal::ShellRefusal {
                code: refusal.code,
                kind: refusal.kind,
                word: refusal
                    .word
                    .and_then(|w| u16::try_from(w).ok())
                    .filter(|w| (1..=4096).contains(w)),
            }),
            *truncated,
        ),
        Prepared::Rewritten(_) if busy.is_some() => (journal::ShellDisposition::Busy, None, false),
        Prepared::Rewritten(_) => (journal::ShellDisposition::Admitted, None, false),
    };
    let admitted = disposition == journal::ShellDisposition::Admitted;
    let variables = match prepared {
        Prepared::Rewritten(variables) if admitted => variables.iter().map(|v| v.kind()).collect(),
        _ => Vec::new(),
    };
    let unavailable = refusal.is_some_and(|r| r.code == RefusalCode::DetectionUnavailable);
    let (rule_set_revision, rule_set_digest) = rule_set(unavailable);
    let (job, stdout_payload, stderr_payload) = if admitted {
        (
            journal::shell_job_id(call.operation, call.generation, call.ordinal),
            payload_id(
                installation,
                call.operation,
                call.generation,
                call.ordinal,
                ObjectKind::Stdout,
            ),
            payload_id(
                installation,
                call.operation,
                call.generation,
                call.ordinal,
                ObjectKind::Stderr,
            ),
        )
    } else {
        ([0; 16], [0; 16], [0; 16])
    };
    journal::ShellIntent {
        operation: call.operation,
        generation: call.generation,
        ordinal: call.ordinal,
        disposition,
        command_truncated: truncated,
        command: command.clone(),
        cwd: cwd.clone(),
        refusal,
        busy: if admitted { None } else { busy },
        variables,
        job,
        stdout_payload,
        stderr_payload,
        initial_reservation: if admitted {
            journal::INITIAL_RESERVATION
        } else {
            0
        },
        rule_set_revision,
        rule_set_digest,
    }
}

/// Kind 15 for a refused or busy call: status 3 with the refusal's display
/// text (never a value), or status 4 with `shell_busy: <reason>`.
pub(super) fn precomputed_result(
    call: &tools::Call,
    prepared: &Prepared,
    busy: Option<journal::ShellBusyReason>,
) -> Option<journal::ToolResult> {
    let (status, text) = match (prepared, busy) {
        (Prepared::Refused { refusal, .. }, _) => (3, refusal.to_string()),
        (Prepared::Rewritten(_), Some(reason)) => (4, format!("shell_busy: {}", reason.as_str())),
        (Prepared::Rewritten(_), None) => return None,
    };
    Some(journal::ToolResult {
        operation: call.operation,
        generation: call.generation,
        ordinal: call.ordinal,
        status,
        text,
        next_offset: None,
        truncated: false,
    })
}

/// Kind 23 `not_started` for an admitted call that never dispatched.
pub(super) fn not_started(intent: &journal::ShellIntent) -> journal::CaptureSettled {
    journal::CaptureSettled {
        operation: intent.operation,
        generation: intent.generation,
        ordinal: intent.ordinal,
        state: journal::CaptureState::NotStarted,
        reason: journal::CaptureReason::None,
        exit_kind: 0,
        exit_value: 0,
        rule_set_revision: intent.rule_set_revision,
        rule_set_digest: intent.rule_set_digest,
        stdout: journal::CaptureStream::default(),
        stderr: journal::CaptureStream::default(),
        distinct_numbers: 0,
        numbers: Vec::new(),
        pause_count: 0,
        first_pause_ms: 0,
        paused_ms: 0,
        final_reservation: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn call(command: &str, cwd: &str) -> tools::Call {
        tools::Call {
            operation: [1; 16],
            generation: 1,
            ordinal: 1,
            arguments: Arguments::Shell {
                command: command.into(),
                cwd: cwd.into(),
                timeout_seconds: 30,
            },
        }
    }
    fn token() -> String {
        ["ghp_", &"aB3".repeat(12)].concat()
    }
    fn shell_text(call: &tools::Call) -> (&str, &str) {
        match &call.arguments {
            Arguments::Shell { command, cwd, .. } => (command, cwd),
            _ => unreachable!(),
        }
    }
    /// J1T-04 unit (G8): the call keeps the rewrite; a repeated ordinal with
    /// a different secret value has a different keyed digest; equal values
    /// match. The digest depends on the per-turn key.
    #[test]
    fn j1t_04_rewrite_replaces_the_raw_command_and_keys_the_values() {
        let key = [7; 32];
        let other = ["ghp_", &"zY9".repeat(12)].concat();
        let mut a = call(
            &format!("curl -H \"Authorization: Bearer {}\" x", token()),
            ".",
        );
        let mut b = call(
            &format!("curl -H \"Authorization: Bearer {}\" x", token()),
            ".",
        );
        let mut c = call(&format!("curl -H \"Authorization: Bearer {other}\" x"), ".");
        let (pa, da) = prepare(&mut a, &key);
        let (_, db) = prepare(&mut b, &key);
        let (_, dc) = prepare(&mut c, &key);
        assert!(matches!(pa, Prepared::Rewritten(ref v) if v.len() == 1));
        assert_eq!(a, b);
        assert_eq!(a, c, "the rewrite alone cannot tell the values apart");
        assert_eq!(da, db);
        assert_ne!(da, dc);
        assert!(!format!("{a:?}").contains(&token()));
        let mut d = call(
            &format!("curl -H \"Authorization: Bearer {}\" x", token()),
            ".",
        );
        assert_ne!(prepare(&mut d, &[8; 32]).1, da);
    }
    /// J1T-03 unit: a display-redacted refused command over 8,192 bytes is
    /// cut at a UTF-8 boundary with the flag set; the record holds no value.
    #[test]
    fn j1t_03_refused_display_is_cut_with_the_flag() {
        let mut long = call(&format!("echo '{}' {}", token(), "é".repeat(4200)), ".");
        let (prepared, _) = prepare(&mut long, &[1; 32]);
        let Prepared::Refused { refusal, truncated } = &prepared else {
            panic!("refused");
        };
        assert_eq!(refusal.code, RefusalCode::SecretSingleQuoted);
        assert!(*truncated);
        let (command, _) = shell_text(&long);
        assert!(command.len() <= 8192 && command.len() >= 8190);
        assert!(!command.contains(&token()));
        let intent = intent(&long, [9; 16], &prepared, None);
        assert_eq!(intent.disposition, journal::ShellDisposition::Refused);
        assert!(intent.command_truncated);
        assert_eq!(intent.job, [0; 16]);
        assert!(intent.variables.is_empty());
        let result = precomputed_result(&long, &prepared, None).unwrap();
        assert_eq!(result.status, 3);
        assert!(!result.text.contains(&token()));
        let encoded = format!("{intent:?}{result:?}");
        assert!(!encoded.contains(&token()));
    }
    /// The cwd is scanned: `secret_in_cwd` records a display-redacted cwd.
    #[test]
    fn secret_in_cwd_records_a_redacted_cwd() {
        let mut c = call("ls", &format!("dir/{}", token()));
        let (prepared, _) = prepare(&mut c, &[1; 32]);
        let Prepared::Refused { refusal, .. } = &prepared else {
            panic!("refused");
        };
        assert_eq!(refusal.code, RefusalCode::SecretInCwd);
        let (command, cwd) = shell_text(&c);
        assert_eq!(command, "ls");
        assert!(!cwd.contains(&token()) && cwd.starts_with("dir/$ASURA_SECRET_"));
    }
    /// Admitted kind 22 carries the job and payload identities, the variable
    /// kinds and the 1 MiB reservation; busy carries none of them.
    #[test]
    fn admitted_and_busy_intents() {
        let mut c = call(&format!("printf '%s' \"{}\"", token()), "src");
        let (prepared, _) = prepare(&mut c, &[1; 32]);
        let admitted = intent(&c, [9; 16], &prepared, None);
        assert_eq!(admitted.disposition, journal::ShellDisposition::Admitted);
        assert_eq!(admitted.variables, vec![secrets::SecretKind::GithubToken]);
        assert_eq!(admitted.job, journal::shell_job_id([1; 16], 1, 1));
        assert_eq!(
            admitted.stdout_payload,
            payload_id([9; 16], [1; 16], 1, 1, ObjectKind::Stdout)
        );
        assert_eq!(admitted.initial_reservation, 1024 * 1024);
        assert_eq!(admitted.rule_set_revision, 1);
        assert!(precomputed_result(&c, &prepared, None).is_none());
        let busy = intent(
            &c,
            [9; 16],
            &prepared,
            Some(journal::ShellBusyReason::Quota),
        );
        assert_eq!(busy.disposition, journal::ShellDisposition::Busy);
        assert!(busy.variables.is_empty() && busy.job == [0; 16]);
        assert_eq!(busy.command, admitted.command);
        let result =
            precomputed_result(&c, &prepared, Some(journal::ShellBusyReason::Quota)).unwrap();
        assert_eq!(
            (result.status, result.text.as_str()),
            (4, "shell_busy: quota")
        );
    }
}
