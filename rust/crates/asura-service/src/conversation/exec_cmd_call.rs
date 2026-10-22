//! The model exec_cmd path at `ToolCall` (J1, SL3): the rewrite happens before
//! any record; a `run` call keeps its admission, variables and derived run
//! identity live. The kind 14 record holds the rewritten command as the only
//! copy (tool-call-records.md); job and payload IDs are derived (TCR-D23).
use crate::capture_worker;
use crate::exec_cmd_worker::ExecCmdRun;
use asura_platform::secrets::{Plan, Refusal, SecretCommand, SecretVariable};
use asura_storage::authority::conversation as journal;
use asura_storage::payload::{PayloadStore, Victim};
use std::sync::Arc;

/// The live state of one `run` exec_cmd call in the conversation owner.
pub(super) struct ExecCmdLive {
    pub run: ExecCmdRun,
    /// Secret variables: the capture literals, and unless `secret_command`
    /// is set, the numbered variables moved into the request at dispatch.
    pub secrets: Vec<SecretVariable>,
    /// SL7: the whole raw command, moved into the request at dispatch as
    /// `ASURA_SECRET_COMMAND`. Held in memory only (SO6).
    pub secret_command: Option<SecretCommand>,
    pub victims: Vec<Victim>,
    pub store: Option<Arc<PayloadStore>>,
    /// Kind 23 waiting to append, then whether it committed.
    pub settled: Option<journal::CaptureSettled>,
    pub settled_committed: bool,
    /// The capture worker after settlement, until it moves to the
    /// pending-install list.
    pub capture: Option<capture_worker::Handle>,
}
impl ExecCmdLive {
    /// An admitted call that never started: its reservation and victims return.
    pub(super) fn release_admission(&mut self) {
        if let Some(store) = self.store.take() {
            store.restore_victims(&self.victims);
            store.release(self.run.job);
        }
        self.victims.clear();
        self.secrets.clear();
        self.secret_command = None;
    }
}

/// The live secrets of an admitted plan: the capture literals, the
/// whole-command value (SL7) and its fallback reason.
pub(super) struct LiveSecrets {
    pub variables: Vec<SecretVariable>,
    pub command: Option<SecretCommand>,
    pub reason: Option<Refusal>,
    /// The display text the model's result shows (SL7).
    display: Option<String>,
}
impl From<Plan> for LiveSecrets {
    fn from(plan: Plan) -> Self {
        match plan {
            Plan::Rewritten(rewrite) => Self {
                variables: rewrite.variables,
                command: None,
                reason: None,
                display: None,
            },
            Plan::WholeCommand(whole) => Self {
                variables: whole.variables,
                command: Some(whole.command),
                reason: Some(whole.reason),
                display: Some(whole.display),
            },
        }
    }
}
impl LiveSecrets {
    /// Marks a whole-command run so its result says it was filtered.
    pub(super) fn filter(&self, mut run: ExecCmdRun) -> ExecCmdRun {
        run.filtered = self
            .display
            .as_deref()
            .map(crate::exec_cmd_worker::filtered_line);
        run
    }
}

/// The request secrets at dispatch: the whole-command value alone, or the
/// numbered variables. Literals stay with the capture worker.
pub(super) fn request_secrets(
    live: &mut ExecCmdLive,
) -> (Vec<SecretVariable>, Option<SecretCommand>) {
    match live.secret_command.take() {
        Some(command) => {
            live.secrets.clear();
            (Vec::new(), Some(command))
        }
        None => (std::mem::take(&mut live.secrets), None),
    }
}

/// Kind 14 fields 21-23 for an SL3 refusal or an SL7 fallback reason. Never
/// holds a value.
pub(super) fn refusal(refusal: &Refusal) -> journal::ExecCmdRefusal {
    journal::ExecCmdRefusal {
        code: refusal.code,
        kind: refusal.kind,
        word: refusal
            .word
            .and_then(|w| u16::try_from(w).ok())
            .filter(|w| (1..=4096).contains(w)),
    }
}

/// Kind 23 `not_started` for a `run` call that never dispatched.
pub(super) fn not_started(run: &ExecCmdRun) -> journal::CaptureSettled {
    journal::CaptureSettled {
        operation: run.operation,
        generation: run.generation,
        ordinal: run.ordinal,
        state: journal::CaptureState::NotStarted,
        reason: journal::CaptureReason::None,
        exit_kind: 0,
        exit_value: 0,
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
    use crate::tools::{self, Arguments};
    use asura_storage::payload::{ObjectKind, payload_id};

    /// TCR-U13 (live part): the live run identity uses the same storage
    /// derivations that replay recomputes, for every ordinal.
    #[test]
    fn tcr_u13_live_run_identity_equals_storage_derivation() {
        for ordinal in 1..=tools::MAX_CALLS {
            let call = tools::Call {
                operation: [1; 16],
                generation: 3,
                ordinal,
                arguments: Arguments::ExecCmd {
                    command: "printf \"$ASURA_SECRET_1\"".into(),
                    cwd: "src".into(),
                },
            };
            let run = ExecCmdRun::derive([9; 16], &call).unwrap();
            assert_eq!(run.job, journal::exec_cmd_job_id([1; 16], 3, ordinal));
            assert_eq!(
                run.stdout_payload,
                payload_id([9; 16], run.job, ObjectKind::Stdout)
            );
            assert_eq!(
                run.stderr_payload,
                payload_id([9; 16], run.job, ObjectKind::Stderr)
            );
            assert_eq!(
                (run.command.as_str(), run.cwd.as_str()),
                ("printf \"$ASURA_SECRET_1\"", "src")
            );
            let settled = not_started(&run);
            assert_eq!(
                (settled.ordinal, settled.state),
                (ordinal, journal::CaptureState::NotStarted)
            );
        }
        let other = tools::Call {
            operation: [1; 16],
            generation: 3,
            ordinal: 1,
            arguments: Arguments::ListTools,
        };
        assert!(ExecCmdRun::derive([9; 16], &other).is_none());
    }
    /// A refusal or fallback reason keeps the code, kind and a bounded word
    /// index only (SL7: a single-quoted value runs from a secret variable).
    #[test]
    fn refusal_fields_hold_no_value() {
        let token = ["ghp_", &"aB3".repeat(12)].concat();
        let plan = asura_platform::secrets::rewrite_exec_cmd_call(&format!("echo '{token}'"), ".")
            .unwrap();
        assert_eq!(plan.command(), asura_platform::secrets::WHOLE_COMMAND);
        let mut live = LiveSecrets::from(plan);
        let fields = refusal(live.reason.as_ref().unwrap());
        assert_eq!(
            fields.code,
            asura_platform::secrets::RefusalCode::SecretSingleQuoted
        );
        assert!(fields.word.is_some_and(|w| (1..=4096).contains(&w)));
        assert!(!format!("{fields:?}").contains(&token));
        // The literal keeps number 1; the request gets the command alone.
        assert_eq!(live.variables.len(), 1);
        assert_eq!(live.variables[0].number(), 1);
        let call = tools::Call {
            operation: [1; 16],
            generation: 3,
            ordinal: 1,
            arguments: Arguments::ExecCmd {
                command: asura_platform::secrets::WHOLE_COMMAND.into(),
                cwd: ".".into(),
            },
        };
        let run = live.filter(ExecCmdRun::derive([9; 16], &call).unwrap());
        let line = run.filtered.clone().unwrap();
        assert_eq!(
            line,
            "command filtered: run from a secret variable: echo '$ASURA_SECRET_1'"
        );
        let mut exec = ExecCmdLive {
            run,
            secrets: std::mem::take(&mut live.variables),
            secret_command: live.command.take(),
            victims: Vec::new(),
            store: None,
            settled: None,
            settled_committed: false,
            capture: None,
        };
        let (numbered, command) = request_secrets(&mut exec);
        assert!(numbered.is_empty() && exec.secrets.is_empty());
        assert_eq!(
            command.unwrap().value(),
            format!("echo '{token}'").as_bytes()
        );
        // A rewrite passes its numbered variables and no command.
        let plan =
            asura_platform::secrets::rewrite_exec_cmd_call(&format!("printf %s \"{token}\""), ".")
                .unwrap();
        let live = LiveSecrets::from(plan);
        assert!(live.reason.is_none() && live.command.is_none());
        exec.secrets = live.variables;
        let (numbered, command) = request_secrets(&mut exec);
        assert_eq!(numbered.len(), 1);
        assert!(command.is_none());
    }
}
