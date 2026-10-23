//! The command text of a model job and the model note text, read from their
//! stored kind 14 arguments (J4 listings). Pure: the journal worker calls it
//! on frames it has already read.
use super::{Id, ToolCall, ToolDecision, ToolOperation};
use asura_platform::secrets::json::{self, Value};

/// The `JobEntry.command` excerpt bound, including the `…` mark. Equals
/// the control contract's `MAX_JOB_COMMAND_EXCERPT_BYTES`.
pub const MAX_JOB_COMMAND_EXCERPT_BYTES: usize = 192;

/// The stored, redacted `command` of an exec_cmd kind 14 (field 14). For a
/// whole-command call it is the display text (SL7). `None` when the record
/// is not an exec_cmd call or holds no `command` string.
pub fn exec_cmd_command(call: &ToolCall) -> Option<String> {
    if call.tool_operation != Some(ToolOperation::ExecCmd) {
        return None;
    }
    string_argument(call, "command")
}

/// The `JobEntry.command` excerpt: the first line of the stored command, at
/// most 192 bytes including the `…` mark, cut at a UTF-8 boundary. `…` marks
/// a cut or a dropped later line. Control characters become spaces so the
/// excerpt stays on one line.
pub fn command_excerpt(command: &str) -> String {
    const MAX: usize = MAX_JOB_COMMAND_EXCERPT_BYTES;
    let first = command.split('\n').next().unwrap_or("");
    let cut = first.len() < command.len() || first.len() > MAX;
    let budget = if cut { MAX - '…'.len_utf8() } else { MAX };
    let mut end = first.len().min(budget);
    while !first.is_char_boundary(end) {
        end -= 1;
    }
    let mut excerpt: String = first[..end]
        .chars()
        .map(|c| if c.is_control() { ' ' } else { c })
        .collect();
    if cut {
        excerpt.push('…');
    }
    excerpt
}

/// The `text` argument of a `run` job `note` kind 14 (code 16) whose `job`
/// argument is this job's handle; `None` for any other call.
pub(crate) fn model_note_text(call: &ToolCall, job: Id) -> Option<String> {
    if call.decision != ToolDecision::Run || call.tool_operation != Some(ToolOperation::JobNote) {
        return None;
    }
    let Value::Object(members) = json::parse(&call.arguments).ok()? else {
        return None;
    };
    let handle: String = job.iter().map(|b| format!("{b:02x}")).collect();
    let mut named = false;
    let mut text = None;
    for (key, value) in members {
        match (key.as_str(), value) {
            ("job", Value::String(h)) => named = h == handle,
            ("text", Value::String(t)) => text = Some(t),
            _ => {}
        }
    }
    text.filter(|_| named)
}

fn string_argument(call: &ToolCall, name: &str) -> Option<String> {
    let Ok(Value::Object(members)) = json::parse(&call.arguments) else {
        return None;
    };
    members.into_iter().find_map(|(key, value)| match value {
        Value::String(text) if key == name => Some(text),
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    //! J4T-03 (unit): the excerpt of each listing entry and the stored
    //! command and note texts it is read from.
    use super::*;
    use crate::authority::conversation::{ArgumentForm, ToolCallSource};

    fn call(op: ToolOperation, arguments: &str) -> ToolCall {
        ToolCall {
            operation: [7; 16],
            generation: 1,
            ordinal: 1,
            source: ToolCallSource::GeneratedContent,
            name: "tool".into(),
            name_bytes: 4,
            tool_operation: Some(op),
            decision: ToolDecision::Run,
            fault: None,
            reason: None,
            form: ArgumentForm::Verbatim,
            arguments_bytes: arguments.len() as u32,
            arguments: arguments.into(),
            number_kinds: Vec::new(),
            unnumbered: 0,
            rule_set_revision: 1,
            rule_set_digest: [3; 32],
            command_cut: false,
            whole_command: false,
            refusal: None,
        }
    }

    #[test]
    fn j4t_03_excerpt_first_line_bound_mark_controls_and_utf8_cut() {
        assert_eq!(command_excerpt("ls -la"), "ls -la");
        assert_eq!(command_excerpt(""), "");
        // A dropped later line is marked, also when the first line is empty.
        assert_eq!(command_excerpt("make\nmake install"), "make…");
        assert_eq!(command_excerpt("\nsecond"), "…");
        // Control characters become spaces; the line stays one line.
        assert_eq!(command_excerpt("a\tb\rc\u{7f}d"), "a b c d");
        // Exactly 192 bytes is kept whole; 193 is cut to 189 bytes and `…`.
        let exact = "x".repeat(MAX_JOB_COMMAND_EXCERPT_BYTES);
        assert_eq!(command_excerpt(&exact), exact);
        let over = "x".repeat(MAX_JOB_COMMAND_EXCERPT_BYTES + 1);
        let cut = command_excerpt(&over);
        assert_eq!(cut.len(), MAX_JOB_COMMAND_EXCERPT_BYTES);
        assert_eq!(cut, format!("{}…", "x".repeat(189)));
        // A cut inside a character moves back to its boundary.
        let wide = "é".repeat(200);
        let cut = command_excerpt(&wide);
        assert_eq!(cut, format!("{}…", "é".repeat(94)));
        assert!(cut.len() <= MAX_JOB_COMMAND_EXCERPT_BYTES);
        let four = format!("a{}", "😀".repeat(60));
        let cut = command_excerpt(&four);
        assert_eq!(cut, format!("a{}…", "😀".repeat(47)));
        assert!(cut.len() <= MAX_JOB_COMMAND_EXCERPT_BYTES);
    }

    #[test]
    fn j4t_03_exec_cmd_command_reads_the_stored_command_and_display_text() {
        let plain = call(
            ToolOperation::ExecCmd,
            r#"{"command":"printf \"$ASURA_SECRET_1\"","cwd":"."}"#,
        );
        assert_eq!(
            exec_cmd_command(&plain).as_deref(),
            Some("printf \"$ASURA_SECRET_1\"")
        );
        // A whole-command call stores its display text in field 14 (SL7).
        let whole = ToolCall {
            whole_command: true,
            ..call(
                ToolOperation::ExecCmd,
                r#"{"command":"curl -H \"$ASURA_SECRET_1\" https://example.test","cwd":"."}"#,
            )
        };
        let text = exec_cmd_command(&whole).unwrap();
        assert_eq!(text, "curl -H \"$ASURA_SECRET_1\" https://example.test");
        assert_eq!(command_excerpt(&text), text);
        // Not an exec_cmd call, no command string, or unparsable arguments.
        assert_eq!(
            exec_cmd_command(&call(ToolOperation::JobList, r#"{"command":"list"}"#)),
            None
        );
        assert_eq!(
            exec_cmd_command(&call(ToolOperation::ExecCmd, r#"{"command":7}"#)),
            None
        );
        assert_eq!(exec_cmd_command(&call(ToolOperation::ExecCmd, "")), None);
    }

    #[test]
    fn j4t_03_model_note_text_needs_a_run_note_call_for_the_job() {
        let job = [0xab; 16];
        let handle = "ab".repeat(16);
        let args =
            format!(r#"{{"command":"note","job":"{handle}","text":"keep $ASURA_SECRET_1"}}"#);
        let note = call(ToolOperation::JobNote, &args);
        assert_eq!(
            model_note_text(&note, job).as_deref(),
            Some("keep $ASURA_SECRET_1")
        );
        // Another job, another code, a refused call, or no text.
        assert_eq!(model_note_text(&note, [0xac; 16]), None);
        assert_eq!(
            model_note_text(&call(ToolOperation::JobStatus, &args), job),
            None
        );
        let refused = ToolCall {
            decision: ToolDecision::Refused,
            ..note.clone()
        };
        assert_eq!(model_note_text(&refused, job), None);
        let bare = format!(r#"{{"command":"note","job":"{handle}"}}"#);
        assert_eq!(
            model_note_text(&call(ToolOperation::JobNote, &bare), job),
            None
        );
    }
}
