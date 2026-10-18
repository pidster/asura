//! The model exec_cmd path at `ToolCall` (J1, SL3): the rewrite happens before
//! any record; a `run` call keeps its admission, variables and derived run
//! identity live. The kind 14 record holds the rewritten command as the only
//! copy (tool-call-records.md); job and payload IDs are derived (TCR-D23).
use crate::capture_worker;
use crate::exec_cmd_worker::ExecCmdRun;
use asura_platform::secrets::{Refusal, SecretVariable};
use asura_storage::authority::conversation as journal;
use asura_storage::payload::{PayloadStore, Victim};
use std::sync::Arc;

/// The live state of one `run` exec_cmd call in the conversation owner.
pub(super) struct ExecCmdLive {
    pub run: ExecCmdRun,
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
impl ExecCmdLive {
    /// An admitted call that never started: its reservation and victims return.
    pub(super) fn release_admission(&mut self) {
        if let Some(store) = self.store.take() {
            store.restore_victims(&self.victims);
            store.release(self.run.job);
        }
        self.victims.clear();
        self.secrets.clear();
    }
}

/// Kind 14 fields 21-23 for an SL3 refusal. Never holds a value.
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
                payload_id([9; 16], [1; 16], 3, ordinal, ObjectKind::Stdout)
            );
            assert_eq!(
                run.stderr_payload,
                payload_id([9; 16], [1; 16], 3, ordinal, ObjectKind::Stderr)
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
    /// A refusal record keeps the code, kind and a bounded word index only.
    #[test]
    fn refusal_fields_hold_no_value() {
        let token = ["ghp_", &"aB3".repeat(12)].concat();
        let refused =
            asura_platform::secrets::rewrite_exec_cmd_call(&format!("echo '{token}'"), ".")
                .unwrap_err();
        let fields = refusal(&refused);
        assert_eq!(
            fields.code,
            asura_platform::secrets::RefusalCode::SecretSingleQuoted
        );
        assert!(fields.word.is_some_and(|w| (1..=4096).contains(&w)));
        assert!(!format!("{fields:?}").contains(&token));
    }
}
