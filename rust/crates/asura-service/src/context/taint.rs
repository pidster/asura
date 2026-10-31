//! The taint exclusion hook (context-composer.md, taint exclusion hook;
//! owner decision T1, 2026-10-31): "Asura sees" is not "a model sees".
//!
//! A job that received a raw secret value is tainted, and the composer
//! never places its output, outcome, counts or durations in model input.
//! The decision is a deterministic Asura check over the job's command
//! source record: storage owns the predicate
//! (`PersonCommandQueued::received_secret_values`), every read that hands
//! job facts to the composer carries its flag, and this module maps the
//! flag to a [`Taint`]. Nothing here reads a model output, a classifier or a
//! summary. The value is derived at each read and never stored.
//!
//! The wider taint design is pending; it extends [`TaintSource`].
use asura_storage::authority::conversation::JobCommand;

/// Why a job is tainted. The code is recorded in the manifest's excluded
/// segment (composer revision 4).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum TaintSource {
    /// A person command with stored secret variables or the whole-command
    /// variable (kind 31 fields 14-15 or field 17 bit 1).
    PersonSecretValues = 1,
}

/// The taint decision for one job.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Taint {
    #[default]
    Clean,
    Tainted(TaintSource),
}
impl Taint {
    /// From the flag a storage read derived from the command source.
    pub(crate) fn from_flag(received_secret: bool) -> Self {
        if received_secret {
            Self::Tainted(TaintSource::PersonSecretValues)
        } else {
            Self::Clean
        }
    }
    /// From the job's command source record (kind 14 or kind 31).
    pub(crate) fn of(command: &JobCommand) -> Self {
        Self::from_flag(command.received_secret_values())
    }
    pub(crate) fn is_tainted(self) -> bool {
        matches!(self, Self::Tainted(_))
    }
    /// The manifest code: 0 clean, otherwise the source code.
    #[allow(dead_code)] // The notice manifest uses it from CC3.
    pub(crate) fn code(self) -> u8 {
        match self {
            Self::Clean => 0,
            Self::Tainted(source) => source as u8,
        }
    }
}

/// The `result:` value of a tainted notice entry.
#[allow(dead_code)] // The notice entry uses it from CC3.
pub(crate) const PRIVATE_RESULT: &str = "private (the command received a secret value; its output, result and counts are not shown to you)";
/// The text of a kind 29 `private` result (codes 12 to 14).
pub(crate) const PRIVATE_TEXT: &str = "private: the person ran this job with a secret value; its output, result and counts are not shown to you";

#[cfg(test)]
mod tests {
    //! CC-U20: the taint rule over the command source records.
    use super::*;
    use asura_storage::authority::conversation::{
        self as journal, ArgumentForm, PersonCommandQueued, SecretKind, ToolCall, ToolCallSource,
        ToolDecision, ToolOperation,
    };

    fn person(kinds: usize, whole: bool, outside: bool) -> JobCommand {
        JobCommand::Person(Box::new(PersonCommandQueued {
            request: [1; 16],
            digest: [2; 32],
            input: [3; 16],
            project: [4; 16],
            conversation: [5; 16],
            expected_generation: 1,
            new_conversation: false,
            outside_sandbox: outside,
            command: if whole {
                journal::WHOLE_COMMAND.into()
            } else {
                "printf %s \"$ASURA_SECRET_1\"".into()
            },
            display_command: String::new(),
            cwd: ".".into(),
            search_path: String::new(),
            environment: Vec::new(),
            order_revision: 1,
            number_kinds: vec![SecretKind::GithubToken; kinds],
            unnumbered: 0,
            command_cut: false,
            whole_command: whole,
            rule_set_revision: 1,
            rule_set_digest: [6; 32],
            fallback: None,
        }))
    }

    #[test]
    fn cc_u20_taint_comes_from_stored_secret_variables_only() {
        assert_eq!(Taint::of(&person(0, false, false)), Taint::Clean);
        let tainted = Taint::Tainted(TaintSource::PersonSecretValues);
        assert_eq!(Taint::of(&person(1, false, false)), tainted);
        assert_eq!(Taint::of(&person(2, false, true)), tainted);
        assert_eq!(Taint::of(&person(0, true, false)), tainted);
        // The `!!` launch environment alone is not a taint source (O3).
        assert_eq!(Taint::of(&person(0, false, true)), Taint::Clean);
        assert_eq!(tainted.code(), 1);
        assert_eq!(Taint::Clean.code(), 0);
        // A model call with SL3 secret variables is untainted today (O2).
        let model = JobCommand::Model(Box::new(ToolCall {
            operation: [7; 16],
            generation: 1,
            ordinal: 1,
            source: ToolCallSource::GeneratedContent,
            name: "exec_cmd".into(),
            name_bytes: 8,
            tool_operation: Some(ToolOperation::ExecCmd),
            decision: ToolDecision::Run,
            fault: None,
            reason: None,
            form: ArgumentForm::Verbatim,
            arguments_bytes: 2,
            arguments: "{}".into(),
            number_kinds: vec![SecretKind::GithubToken],
            unnumbered: 0,
            rule_set_revision: 1,
            rule_set_digest: [3; 32],
            command_cut: false,
            whole_command: true,
            refusal: None,
        }));
        assert_eq!(Taint::of(&model), Taint::Clean);
    }
}
