//! The model exec_cmd path at `ToolCall` (SL3, SL7): the rewrite happens
//! before any record. The kind 14 record holds the rewritten command as the
//! only copy (tool-call-records.md). J3 retired the J1 call path (J3-E9);
//! until J34-E submits exec_cmd calls to the job controller, a model exec_cmd
//! call that passes the rewrite is recorded `denied` and runs nothing
//! (tool-job-controller.md, TJ-P3 step 4). The secrets kept here are what
//! J34-E hands to the controller's `StartRequest`.
use asura_platform::secrets::{Plan, Refusal, SecretCommand, SecretVariable};
use asura_storage::authority::conversation as journal;

/// The live secrets of a rewritten plan: the capture literals, the
/// whole-command value (SL7) and its fallback reason.
#[cfg_attr(not(test), allow(dead_code))] // J34-E passes them to `StartRequest`
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
#[cfg_attr(not(test), allow(dead_code))] // J34-E passes them to `StartRequest`
impl LiveSecrets {
    /// The SL7 line of a whole-command run; `None` for a rewrite.
    pub(super) fn filtered(&self) -> Option<String> {
        self.display.as_deref().map(filtered_line)
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

/// Display bytes the model's result shows for a whole-command run (SL7).
const FILTERED_DISPLAY_BYTES: usize = 2048;
/// The SL7 line beside the display text: `command filtered: run from a
/// secret variable: <display>`. The display is cut at 2,048 bytes, never
/// inside a placeholder, with `…` when cut; control characters become
/// spaces so the line stays one line. It holds placeholders, never a value.
#[cfg_attr(not(test), allow(dead_code))] // the J3 result text shows it
pub(super) fn filtered_line(display: &str) -> String {
    let mut cut = display.len().min(FILTERED_DISPLAY_BYTES);
    while !display.is_char_boundary(cut) {
        cut -= 1;
    }
    // Never inside `$ASURA_SECRET_<n>`: a cut inside one backs up to its
    // `$`, the last `$` before the cut.
    if cut < display.len()
        && let Some(at) = display[..cut].rfind('$')
        && let Some(digits) = display[at..].strip_prefix("$ASURA_SECRET_")
    {
        let end =
            at + "$ASURA_SECRET_".len() + digits.bytes().take_while(u8::is_ascii_digit).count();
        if end > cut {
            cut = at;
        }
    }
    let mut line = format!("{}: ", asura_platform::secrets::WHOLE_COMMAND_NOTICE);
    line.extend(
        display[..cut]
            .chars()
            .map(|c| if c.is_control() { ' ' } else { c }),
    );
    if cut < display.len() {
        line.push('…');
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(
            live.filtered().unwrap(),
            "command filtered: run from a secret variable: echo '$ASURA_SECRET_1'"
        );
        let (numbered, command) = live.request();
        assert!(numbered.is_empty() && live.variables.is_empty());
        assert_eq!(
            command.unwrap().value(),
            format!("echo '{token}'").as_bytes()
        );
        // A rewrite passes its numbered variables and no command.
        let plan =
            asura_platform::secrets::rewrite_exec_cmd_call(&format!("printf %s \"{token}\""), ".")
                .unwrap();
        let mut live = LiveSecrets::from(plan);
        assert!(live.reason.is_none() && live.command.is_none());
        assert!(live.filtered().is_none());
        let (numbered, command) = live.request();
        assert_eq!(numbered.len(), 1);
        assert!(command.is_none());
    }
    /// SL7: the filtered line shows the display text, cut at 2,048 bytes
    /// before a placeholder, on one line.
    #[test]
    fn sl7_filtered_line_cuts_before_a_placeholder_and_stays_one_line() {
        assert_eq!(
            filtered_line("echo '$ASURA_SECRET_1'\nls"),
            "command filtered: run from a secret variable: echo '$ASURA_SECRET_1' ls"
        );
        let long = format!("{}$ASURA_SECRET_12 tail", "x".repeat(2040));
        assert_eq!(
            filtered_line(&long),
            format!(
                "command filtered: run from a secret variable: {}…",
                "x".repeat(2040)
            )
        );
        let fits = format!("{}$ASURA_SECRET_1 tail", "x".repeat(2048 - 15));
        assert!(filtered_line(&fits).ends_with("$ASURA_SECRET_1…"));
        let wide = "é".repeat(1100);
        assert!(filtered_line(&wide).ends_with('…'));
    }
}
