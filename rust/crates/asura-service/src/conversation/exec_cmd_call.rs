//! The model exec_cmd path at `ToolCall` (SL3, SL7): the rewrite happens
//! before any record. The kind 14 record holds the rewritten command as the
//! only copy (tool-call-records.md). Every model exec_cmd call runs as a
//! job (J3): the secrets kept here move into the controller's
//! `StartRequest` when the owner submits the start, and the job's result
//! text, with the SL7 filtered line, is rendered by `job_text`.
use asura_platform::secrets::{
    JobRedaction, Plan, Refusal, RuleSet, SecretCommand, SecretVariable,
};
use asura_storage::authority::conversation as journal;

/// The live secrets of a rewritten plan: the capture literals, the
/// whole-command value (SL7) and its fallback reason.
pub(super) struct LiveSecrets {
    pub variables: Vec<SecretVariable>,
    pub command: Option<SecretCommand>,
    pub reason: Option<Refusal>,
}
impl From<Plan> for LiveSecrets {
    fn from(plan: Plan) -> Self {
        match plan {
            Plan::Rewritten(rewrite) => Self {
                variables: rewrite.variables,
                command: None,
                reason: None,
            },
            Plan::WholeCommand(whole) => Self {
                variables: whole.variables,
                command: Some(whole.command),
                reason: Some(whole.reason),
            },
        }
    }
}
impl LiveSecrets {
    /// The capture worker's redaction: every variable of the plan, before
    /// [`Self::request`] moves them.
    pub(super) fn redaction(&self, rules: &'static RuleSet) -> JobRedaction {
        JobRedaction::new(rules, &self.variables)
    }
    /// The request secrets: the whole-command value alone, or the numbered
    /// variables. Literals stay with the capture worker's redaction.
    pub(super) fn request(&mut self) -> (Vec<SecretVariable>, Option<SecretCommand>) {
        match self.command.take() {
            Some(command) => {
                self.variables.clear();
                (Vec::new(), Some(command))
            }
            None => (std::mem::take(&mut self.variables), None),
        }
    }
}

/// One model exec_cmd start waiting for its submit, which the owner makes
/// when it appends the call's kind 14 (J3 call flow, steps 3 and 4).
pub(super) struct ExecStart {
    /// The SL3 rewrite, or the whole-command form (SL7).
    pub command: String,
    pub cwd: String,
    pub yield_seconds: u32,
    pub secrets: LiveSecrets,
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A refusal or fallback reason keeps the code, kind and a bounded word
    /// index only (SL7: a single-quoted value runs from a secret variable).
    #[test]
    fn refusal_fields_hold_no_value() {
        let token = ["ghp_", &"aB3".repeat(12)].concat();
        let rules = asura_platform::secrets::detection().unwrap();
        let plan = asura_platform::secrets::rewrite_exec_cmd_call_with(
            rules,
            &format!("echo '{token}'"),
            ".",
        )
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
        let _redaction = live.redaction(rules);
        let (numbered, command) = live.request();
        assert!(numbered.is_empty() && live.variables.is_empty());
        assert_eq!(
            command.unwrap().value(),
            format!("echo '{token}'").as_bytes()
        );
        // A rewrite passes its numbered variables and no command.
        let plan = asura_platform::secrets::rewrite_exec_cmd_call_with(
            rules,
            &format!("printf %s \"{token}\""),
            ".",
        )
        .unwrap();
        let mut live = LiveSecrets::from(plan);
        assert!(live.reason.is_none() && live.command.is_none());
        let (numbered, command) = live.request();
        assert_eq!(numbered.len(), 1);
        assert!(command.is_none());
    }
}
