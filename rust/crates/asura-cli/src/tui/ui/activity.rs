//! Bounded response activity projection. No I/O or execution ownership.
use super::*;
use asura_control::pb;
use crossterm::event::KeyEvent;

#[derive(Default)]
pub(super) struct Activity {
    operation: Option<Vec<u8>>,
    generation: u64,
    cursor: u64,
    kind: i32,
    tools: Vec<pb::ToolProgress>,
    statistics: Option<pb::TurnStatistics>,
    /// Live step a pending turn waits on; display only (owner, 2026-10-16).
    waiting_step: Option<i32>,
    expanded: bool,
    unavailable: bool,
    /// The turn's assembled thinking text and whether its block is open
    /// (thinking-level.md). Blocks start folded.
    thinking: String,
    thinking_open: bool,
    /// Jobs that outlived the turn, from its terminal event (EX12, J4-E6).
    outlived: Option<u32>,
}
impl Activity {
    /// The EX12 line kept with the response; drawn again from the terminal
    /// event after a restore.
    pub(super) fn outlived_notice(&self) -> Option<String> {
        crate::tui::jobs::outlived_notice(self.outlived?)
    }
    /// The jobs the turn's calls name (`ToolProgress.job_id`), in call
    /// order, at most the chooser's 9.
    pub(super) fn job_ids(&self) -> Vec<[u8; 16]> {
        let mut ids: Vec<[u8; 16]> = Vec::new();
        for id in self
            .tools
            .iter()
            .filter_map(|tool| <[u8; 16]>::try_from(tool.job_id.as_deref()?).ok())
        {
            if !ids.contains(&id) && ids.len() < crate::tui::jobs::MAX_CHOOSER {
                ids.push(id);
            }
        }
        ids
    }
    pub(super) fn thinking(&self) -> &str {
        &self.thinking
    }
    pub(super) fn thinking_open(&self) -> bool {
        self.thinking_open
    }
    /// The thinking text of a whole event, as the worker assembled it.
    pub(super) fn set_thinking(&mut self, event: &pb::ConversationEvent) {
        if let Some(text) = &event.thinking_text {
            self.thinking = text.clone();
        }
    }
    #[cfg(test)]
    pub(super) fn expanded(&self) -> bool {
        self.expanded
    }
    pub(super) fn clear_statistics(&mut self) {
        self.statistics = None;
    }
    pub(super) fn active(&self) -> bool {
        self.kind < 3 && !self.unavailable
    }
    pub(super) fn display(&self) -> String {
        let state: String = if self.unavailable {
            "Observation unavailable".into()
        } else {
            match (self.kind, self.waiting_step.and_then(wait_label)) {
                // Segmented journal OD-SJ-2: the turn pauses at the journal
                // quota or disk floor, before or during output.
                (0..=2, _) if self.waiting_step == Some(JOURNAL_PAUSED) => {
                    format!("{PAUSED_TEXT} · Esc stops")
                }
                (0 | 1, Some(step)) => format!("Waiting: {step} · Esc stops"),
                (0 | 1, None) => "Waiting".into(),
                (2, _) => "Generating".into(),
                (3, _) => "Complete".into(),
                (4, _) => "Failed".into(),
                (5, _) => "Cancelled".into(),
                (6, _) => "Interrupted".into(),
                _ => "Unknown".into(),
            }
        };
        let expanded = self.active() || self.expanded;
        // The event carries the most recent calls only (TCR-D31); the last
        // ordinal is the turn's call count.
        let mut text = format!(
            "{} Activity · {state} · {} tools",
            if expanded { "▾" } else { "▸" },
            self.tools.last().and_then(|tool| tool.ordinal).unwrap_or(0)
        );
        if let Some(statistics) = &self.statistics {
            text.push_str(&format!(
                "\n  {}",
                statistics_text(statistics, self.kind >= 3)
            ));
        }
        if expanded {
            for tool in &self.tools {
                let state = match tool.state {
                    Some(1) => "running",
                    Some(2) => "complete",
                    Some(3) => "failed",
                    Some(4) => "interrupted",
                    _ => "unknown",
                };
                // The name as the model sent it (redacted), or a marker when the
                // service did not retain it (TCR-D13).
                let name = match tool.name.as_deref() {
                    Some("") | None => "[name not retained]".to_owned(),
                    Some(name) => literal(name, 64),
                };
                text.push_str(&format!(
                    "\n  ⚙ {}. {name} · {state}",
                    tool.ordinal.unwrap_or(0),
                ));
                if let Some(arguments) = &tool.arguments {
                    text.push_str(&format!("\n    {}", literal(arguments, 192)));
                }
                if let Some(policy) = tool.policy_decision {
                    // Kind 14 decision codes; a rejected call shows its fault.
                    let decision = match policy {
                        1 => "service admitted",
                        2 => "invalid_arguments",
                        3 => "unknown_tool",
                        4 => "refused",
                        5 => "busy",
                        6 => "access denied",
                        7 => "call limit",
                        8 => "cancelled before decision",
                        _ => "admission unknown",
                    };
                    match tool.fault.and_then(fault_label) {
                        Some(fault) => text.push_str(&format!("\n    · {decision}: {fault}")),
                        None => text.push_str(&format!("\n    · {decision}")),
                    }
                    // SL7: the arguments above are the filtered display text.
                    if tool.command_filtered == Some(true) {
                        text.push_str(" · command filtered: run from a secret variable");
                    }
                }
                if tool.state == Some(3) {
                    text.push_str(&format!(
                        " · {}",
                        match tool.status {
                            Some(2) => "denied",
                            Some(3) => "invalid",
                            Some(4) => "unavailable",
                            Some(5) => "timeout",
                            Some(6) => "cancelled",
                            Some(7) => "resource limit",
                            _ => "unknown",
                        }
                    ));
                }
                if tool.state.is_some_and(|state| state >= 2) {
                    text.push_str(&format!("\n    ↳ {state}"));
                    if let Some(exit) = tool.exit_code {
                        text.push_str(&format!(" · exit {exit}"));
                    }
                    if let Some(bytes) = tool.result_bytes {
                        text.push_str(&format!(" · {} bytes", grouped_count(bytes)));
                    }
                    if let Some(elapsed) = tool.elapsed_ms {
                        if elapsed < 1000 {
                            text.push_str(&format!(" · {elapsed} ms"));
                        } else {
                            text.push_str(&format!(" · {:.1}s", elapsed as f64 / 1000.0));
                        }
                    }
                    if tool.truncated == Some(true) {
                        text.push_str(" · captured result truncated");
                    }
                    if let Some(preview) = &tool.preview {
                        text.push_str(&format!("\n      preview: {}", literal(preview, 128)));
                    }
                }
            }
        }
        text
    }
}

/// Kind 14 fault codes (tool-call-records.md, faults).
fn fault_label(code: u32) -> Option<&'static str> {
    Some(match code {
        1 => "invalid_json",
        2 => "json_limit",
        3 => "not_object",
        4 => "duplicate_key",
        5 => "unknown_command",
        6 => "missing_field",
        7 => "unexpected_field",
        8 => "wrong_type",
        9 => "out_of_range",
        10 => "unsafe_path",
        11 => "invalid_version_id",
        12 => "arguments_over_limit",
        13 => "name_over_limit",
        14 => "invalid_text",
        15 => "unregistered_name",
        _ => return None,
    })
}
/// `CONVERSATION_WAIT_STEP_JOURNAL_PAUSED`: the journal is held at its quota
/// or disk floor and the turn is paused until it resumes or is cancelled.
const JOURNAL_PAUSED: i32 = 5;
pub(super) const PAUSED_TEXT: &str = "Paused: journal space is full.";
/// Waiting step labels for an accepted turn (lowercase, after `Waiting:`).
fn wait_label(step: i32) -> Option<&'static str> {
    match step {
        3 => Some("sending input"),
        4 => Some("model output"),
        _ => None,
    }
}
fn grouped_count(count: u64) -> String {
    let digits = count.to_string();
    let mut text = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            text.push(',');
        }
        text.push(digit);
    }
    text
}

fn statistics_text(statistics: &pb::TurnStatistics, terminal: bool) -> String {
    let elapsed = statistics
        .elapsed_ms
        .map(|ms| format!("{:.1}s", ms as f64 / 1000.0))
        .unwrap_or_else(|| "?".into());
    let input = statistics
        .input_tokens
        .map(grouped_count)
        .unwrap_or_else(|| "?".into());
    let output = statistics
        .output_tokens
        .map(grouped_count)
        .unwrap_or_else(|| "?".into());
    let mut text = format!(
        "{}:{elapsed} · ↓{input} ↑{output}",
        if terminal { "last" } else { "now" }
    );
    if let Some(cached) = statistics.cached_input_tokens {
        text.push_str(&format!(" · cached {}", grouped_count(cached)));
    }
    if let Some(requests) = statistics.request_count {
        text.push_str(&format!(" · {requests} requests"));
    }
    if statistics.complete != Some(true) {
        text.push_str(" · partial");
    }
    text
}
impl App {
    pub(super) fn append_response(&mut self, prompt: String, operation: Option<Vec<u8>>) {
        // Tests and old local display entries may have no activity yet.
        self.activity
            .resize_with(self.transcript.len(), Activity::default);
        self.rendered_responses
            .resize_with(self.transcript.len(), Default::default);
        if self.transcript.len() == 8 {
            self.transcript.remove(0);
            self.rendered_responses.remove(0);
            self.activity.remove(0);
            self.response_selection = self.response_selection.map(|n| n.saturating_sub(1));
            self.person_transcript_shifted();
        }
        self.transcript.push((prompt, String::new()));
        self.rendered_responses.push(Default::default());
        self.activity.push(Activity {
            operation,
            ..Default::default()
        });
    }
    pub(super) fn observe_activity(&mut self, event: &pb::ConversationEvent) -> Option<usize> {
        let operation = event.operation_id.as_ref()?;
        let generation = event.generation?;
        let cursor = event.cursor?;
        self.activity
            .resize_with(self.transcript.len(), Activity::default);
        let explicit = matches!(&self.retained_request, Some(super::super::conversation::Request::Setup(crate::conversation::Command::Observe(id))) if id.as_slice() == operation);
        if self
            .active_operation
            .as_ref()
            .is_some_and(|id| id != operation)
            && !explicit
        {
            return None;
        }
        let index = if let Some(index) = self
            .activity
            .iter()
            .position(|a| a.operation.as_ref() == Some(operation))
        {
            index
        } else {
            let explicit = matches!(&self.retained_request, Some(super::super::conversation::Request::Setup(crate::conversation::Command::Observe(id))) if id.as_slice() == operation);
            if self
                .active_operation
                .as_ref()
                .is_some_and(|id| id != operation)
                && !explicit
            {
                return None;
            }
            if self.activity.last().is_some_and(|a| a.operation.is_none()) {
                self.activity.len() - 1
            } else {
                self.append_response("Recovered operation".into(), Some(operation.clone()));
                self.activity.len() - 1
            }
        };
        let entry = &mut self.activity[index];
        if entry.generation != 0 && (entry.generation != generation || cursor < entry.cursor) {
            return None;
        }
        entry.unavailable = false;
        // A busy journal read can send an equal-cursor, empty Pending heartbeat.
        if cursor == entry.cursor && entry.cursor != 0 {
            // Service-owned elapsed time can change without a journal revision.
            // An empty Pending heartbeat cannot replace tools, response or kind.
            if let Some(statistics) = &event.statistics {
                entry.statistics = Some(*statistics);
            }
            // The step may change without a cursor change; it is the only
            // other value a heartbeat can replace.
            if entry.kind <= 1 && event.kind == Some(1) {
                entry.waiting_step = event.waiting_step;
            } else if entry.kind == 2 && event.kind == Some(1) {
                // A generating turn can only pause for the journal, or resume.
                entry.waiting_step = event.waiting_step.filter(|step| *step == JOURNAL_PAUSED);
            }
            return None;
        }
        entry.operation = Some(operation.clone());
        entry.generation = generation;
        entry.cursor = cursor;
        if entry.kind >= 3 && event.kind.is_some_and(|kind| kind < 3) {
            return None;
        }
        entry.kind = event.kind.unwrap_or(1);
        // Only a terminal event carries the outlived count.
        entry.outlived = event.outlived_jobs.filter(|_| entry.kind >= 3);
        entry.waiting_step = event.waiting_step.filter(|_| entry.kind == 1);
        entry.statistics = event.statistics;
        entry.unavailable = false;
        if !event.tools.is_empty() || entry.kind != 1 {
            entry.tools = event
                .tools
                .iter()
                .take(asura_control::MAX_TOOL_PROGRESS_ENTRIES)
                .cloned()
                .collect();
        }
        Some(index)
    }
    pub(super) fn activity_unavailable(&mut self) {
        if let Some(entry) = self
            .activity
            .iter_mut()
            .find(|a| a.operation == self.active_operation && a.active())
        {
            entry.unavailable = true;
        }
    }
    pub(super) fn activity_key(&mut self, key: KeyEvent) -> bool {
        if self.picker.is_some() {
            return false;
        }
        if key.code == KeyCode::F(6) && key.modifiers.is_empty() {
            self.history_focus = false;
            self.response_selection = if self.response_selection.is_some() {
                None
            } else {
                self.transcript.len().checked_sub(1)
            };
            self.response_scroll = 0;
            self.focus = Focus::Editor;
            return true;
        }
        let Some(selected) = self.response_selection else {
            return false;
        };
        if !key.modifiers.is_empty() {
            return true;
        }
        match key.code {
            KeyCode::Esc => {
                self.response_selection = None;
                self.response_scroll = 0;
            }
            KeyCode::Up | KeyCode::Down | KeyCode::Home | KeyCode::End => {
                self.response_selection = Some(match key.code {
                    KeyCode::Up => selected.saturating_sub(1),
                    KeyCode::Down => (selected + 1).min(self.transcript.len().saturating_sub(1)),
                    KeyCode::Home => 0,
                    _ => self.transcript.len().saturating_sub(1),
                });
                self.response_scroll = 0;
            }
            KeyCode::Enter | KeyCode::Char(' ') => {
                if let Some(entry) = self.activity.get_mut(selected) {
                    entry.expanded = !entry.expanded;
                }
                self.response_scroll = 0;
            }
            // The viewer for the response's job, or a chooser (J4 TUI).
            KeyCode::Char('j') => {
                let ids = self
                    .activity
                    .get(selected)
                    .map(Activity::job_ids)
                    .unwrap_or_default();
                self.open_response_jobs(ids);
            }
            // Open or fold the thinking block (thinking-level.md).
            KeyCode::Char('t') => {
                if let Some(entry) = self
                    .activity
                    .get_mut(selected)
                    .filter(|entry| !entry.thinking.is_empty())
                {
                    entry.thinking_open = !entry.thinking_open;
                }
                self.response_scroll = 0;
            }
            KeyCode::PageUp => {
                self.response_scroll = self.response_scroll.saturating_sub(self.response_page)
            }
            KeyCode::PageDown => {
                self.response_scroll = self
                    .response_scroll
                    .saturating_add(self.response_page)
                    .min(self.response_scroll_max)
            }
            _ => {}
        }
        true
    }
    /// Log focus: scroll the conversation and step down the focus chain.
    /// Entering the log is owned by the composer's focus chain.
    pub(super) fn history_key(&mut self, key: KeyEvent) -> bool {
        if !self.history_focus {
            return false;
        }
        if (key.modifiers == KeyModifiers::CONTROL
            && matches!(
                key.code,
                KeyCode::Char('p' | 'n') | KeyCode::Up | KeyCode::Down
            ))
            || (key.modifiers == KeyModifiers::SHIFT
                && matches!(key.code, KeyCode::Left | KeyCode::Right))
        {
            self.history_focus = false;
            return false;
        }
        if key.modifiers == KeyModifiers::SHIFT && key.code == KeyCode::Down {
            self.history_focus = false;
            let ids = self.queue_focus_ids();
            if let Some(id) = ids.first() {
                self.queue_focused_id = Some(id.clone());
                self.focus = Focus::Queue(0);
            }
            return true;
        }
        if !key.modifiers.is_empty() {
            return true;
        }
        match key.code {
            // Esc drops a held or checking person entry in view (EPG-R5);
            // otherwise it returns to the input.
            KeyCode::Esc => {
                if !self.person_log_escape() {
                    self.history_focus = false;
                }
            }
            // Enter runs a held `!!` entry in view (EPG-R5). Otherwise
            // Enter or Space opens a person entry's job at the end of
            // stdout; the fold never expands inline (PC11).
            KeyCode::Enter if self.person_log_enter() => {}
            KeyCode::Enter | KeyCode::Char(' ') => {
                self.person_open_from_log();
            }
            KeyCode::Up => self.history_scroll = self.history_scroll.saturating_sub(1),
            KeyCode::Down => {
                self.history_scroll = self
                    .history_scroll
                    .saturating_add(1)
                    .min(self.history_scroll_max)
            }
            KeyCode::PageUp => {
                self.history_scroll = self.history_scroll.saturating_sub(self.history_page)
            }
            KeyCode::PageDown => {
                self.history_scroll = self
                    .history_scroll
                    .saturating_add(self.history_page)
                    .min(self.history_scroll_max)
            }
            KeyCode::Home => self.history_scroll = 0,
            KeyCode::End => self.history_scroll = self.history_scroll_max,
            _ => {}
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};
    fn event(id: u8, cursor: u64, kind: i32, state: i32) -> pb::ConversationEvent {
        pb::ConversationEvent {
            operation_id: Some(vec![id; 16]),
            generation: Some(1),
            cursor: Some(cursor),
            kind: Some(kind),
            tools: vec![pb::ToolProgress {
                ordinal: Some(1),
                name: Some("exec_cmd".into()),
                state: Some(state),
                status: (state == 2).then_some(1),
                ..Default::default()
            }],
            ..Default::default()
        }
    }
    fn apply(app: &mut App, event: pb::ConversationEvent) {
        let done = event.kind.is_some_and(|n| n >= 3);
        app.conversation_finished(super::super::super::conversation::Update {
            event: Some(event),
            done,
            ..Default::default()
        });
    }
    fn key(app: &mut App, code: KeyCode) {
        app.handle(Event::Key(KeyEvent::new(code, KeyModifiers::NONE)));
    }
    fn draw(app: &mut App, width: u16, height: u16, name: &str) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let buffer = terminal.backend().buffer();
        if let Some(dir) = std::env::var_os("ASURA_TUI_PREVIEW_DIR") {
            std::fs::create_dir_all(&dir).unwrap();
            let cells: Vec<_> = buffer.content.iter().map(|cell| serde_json::json!({"symbol":cell.symbol(), "fg":format!("{:?}",cell.fg), "bg":format!("{:?}",cell.bg), "modifier":format!("{:?}",cell.modifier)})).collect();
            std::fs::write(
                std::path::PathBuf::from(dir).join(format!("activity-{name}-{width}.json")),
                serde_json::to_vec(
                    &serde_json::json!({"width":width,"height":height,"cells":cells}),
                )
                .unwrap(),
            )
            .unwrap();
        }
        buffer.content.iter().map(|cell| cell.symbol()).collect()
    }
    #[test]
    fn rich_tool_activity_and_turn_statistics_keep_verified_values_visible() {
        let mut app = App::new();
        app.append_response("Find matching files".into(), Some(vec![1; 16]));
        app.active_operation = Some(vec![1; 16]);
        app.insert("draft stays");
        let mut running = event(1, 2, 2, 1);
        running.tools[0].arguments = Some("find . -name *.wisp".into());
        running.tools[0].policy_decision = Some(1);
        running.statistics = Some(pb::TurnStatistics {
            elapsed_ms: Some(200),
            request_count: Some(1),
            complete: Some(false),
            ..Default::default()
        });
        apply(&mut app, running);
        let live = draw(&mut app, 100, 32, "rich-running");
        assert!(live.contains("find . -name *.wisp"));
        assert!(live.contains("service admitted"));
        assert!(live.contains("now:0.2s · ↓? ↑?"));
        assert!(!live.contains("safe by rules"));
        let mut complete = event(1, 3, 3, 2);
        complete.text = Some("The complete response is retained.".into());
        complete.tools[0].arguments = Some("find . -name *.wisp".into());
        complete.tools[0].policy_decision = Some(1);
        complete.tools[0].elapsed_ms = Some(2);
        complete.tools[0].result_bytes = Some(989);
        complete.tools[0].exit_code = Some(0);
        complete.tools[0].preview = Some("./functionality-self-test.wisp".into());
        complete.tools[0].truncated = Some(false);
        complete.statistics = Some(pb::TurnStatistics {
            elapsed_ms: Some(42_100),
            input_tokens: Some(17_524),
            output_tokens: Some(739),
            cached_input_tokens: Some(0),
            complete: Some(true),
            request_count: Some(2),
        });
        apply(&mut app, complete);
        let collapsed = draw(&mut app, 100, 32, "rich-collapsed");
        assert!(collapsed.contains("last:42.1s · ↓17,524 ↑739"));
        assert!(!collapsed.contains("find . -name *.wisp"));
        key(&mut app, KeyCode::F(6));
        key(&mut app, KeyCode::Enter);
        let expanded = draw(&mut app, 100, 32, "rich-expanded");
        assert!(expanded.contains("↳ complete · exit 0 · 989 bytes · 2 ms"));
        assert!(expanded.contains("preview: ./functionality-self-test.wisp"));
        assert!(expanded.contains("cached 0"));
        assert!(!expanded.contains("captured result truncated"));
        assert_eq!(app.transcript[0].1, "The complete response is retained.");
        assert_eq!(app.draft(), "draft stays");
    }
    #[test]
    fn equal_cursor_statistics_refresh_preserves_response_and_tool_state() {
        let mut app = App::new();
        app.active_operation = Some(vec![1; 16]);
        let mut running = event(1, 4, 2, 1);
        running.text = Some("Current answer".into());
        running.statistics = Some(pb::TurnStatistics {
            elapsed_ms: Some(1000),
            ..Default::default()
        });
        apply(&mut app, running);
        let mut heartbeat = event(1, 4, 1, 1);
        heartbeat.tools.clear();
        heartbeat.statistics = Some(pb::TurnStatistics {
            elapsed_ms: Some(2000),
            input_tokens: Some(0),
            output_tokens: Some(0),
            complete: Some(false),
            ..Default::default()
        });
        apply(&mut app, heartbeat.clone());
        assert_eq!(app.activity[0].kind, 2);
        assert_eq!(app.activity[0].tools.len(), 1);
        assert_eq!(app.transcript[0].1, "Current answer");
        assert!(app.activity[0].display().contains("now:2.0s · ↓0 ↑0"));
        heartbeat.generation = Some(2);
        heartbeat.statistics.as_mut().unwrap().elapsed_ms = Some(9000);
        apply(&mut app, heartbeat);
        assert!(app.activity[0].display().contains("now:2.0s"));
        // A replay projection explicitly lacking statistics does not inherit
        // the prior owner's volatile counts or monotonic timing.
        apply(&mut app, event(1, 5, 2, 1));
        assert!(app.activity[0].statistics.is_none());
    }
    #[test]
    fn service_epoch_change_removes_volatile_statistics_without_losing_draft() {
        let mut app = App::new();
        app.view.epoch = Some("old-owner".into());
        app.active_operation = Some(vec![1; 16]);
        let mut complete = event(1, 4, 3, 2);
        complete.text = Some("Retained answer".into());
        complete.statistics = Some(pb::TurnStatistics {
            elapsed_ms: Some(2000),
            input_tokens: Some(32),
            output_tokens: Some(7),
            complete: Some(true),
            ..Default::default()
        });
        apply(&mut app, complete);
        app.insert("preserved draft");
        let mut view = app.view.clone();
        view.epoch = Some("new-owner".into());
        app.set_view(view);
        assert!(app.activity[0].statistics.is_none());
        assert_eq!(app.transcript[0].1, "Retained answer");
        assert_eq!(app.draft(), "preserved draft");
    }
    #[test]
    fn partial_unknown_counts_and_literal_previews_cannot_claim_completeness() {
        let statistics = pb::TurnStatistics {
            input_tokens: Some(1_000_000),
            output_tokens: None,
            complete: Some(false),
            ..Default::default()
        };
        assert_eq!(
            statistics_text(&statistics, true),
            "last:? · ↓1,000,000 ↑? · partial"
        );
        let mut record = event(1, 2, 4, 3).tools.remove(0);
        record.arguments = Some("arg\n\u{1b}[31m".repeat(100));
        record.preview = Some("preview\n\u{1b}[31m".repeat(100));
        record.policy_decision = Some(6);
        record.truncated = Some(true);
        record.exit_code = Some(-1);
        let activity = Activity {
            tools: vec![record],
            expanded: true,
            kind: 4,
            ..Default::default()
        };
        let display = activity.display();
        assert!(display.contains("access denied"));
        assert!(display.contains("captured result truncated"));
        assert!(display.contains("exit -1"));
        assert!(!display.contains('\u{1b}'));
        assert!(display.len() < 700);
        assert_eq!(grouped_count(u64::MAX), "18,446,744,073,709,551,615");
    }
    /// SL7 view: a call that ran from the whole-command variable says
    /// `command filtered: run from a secret variable` beside its display
    /// text; an ordinary admitted call does not.
    #[test]
    fn whole_command_call_says_filtered_beside_display_text() {
        let filtered = pb::ToolProgress {
            ordinal: Some(1),
            name: Some("exec_cmd".into()),
            state: Some(2),
            status: Some(1),
            arguments: Some(
                r#"{"command":"printf %s '$ASURA_SECRET_1' | shasum -a 256","cwd":"."}"#.into(),
            ),
            policy_decision: Some(1),
            command_filtered: Some(true),
            ..Default::default()
        };
        let plain = pb::ToolProgress {
            ordinal: Some(2),
            command_filtered: None,
            arguments: Some(r#"{"command":"ls","cwd":"."}"#.into()),
            ..filtered.clone()
        };
        let activity = Activity {
            tools: vec![filtered, plain],
            expanded: true,
            kind: 3,
            ..Default::default()
        };
        let display = activity.display();
        assert!(
            display.contains(
                "printf %s '$ASURA_SECRET_1' | shasum -a 256\",\"cwd\":\".\"}\n    · service admitted · command filtered: run from a secret variable\n"
            ),
            "{display}"
        );
        assert_eq!(display.matches("command filtered").count(), 1, "{display}");
    }
    /// TCR-D13 / TCR-E01 view: a rejected call shows its decision and fault;
    /// a text that was not retained shows only its size and reason; a name
    /// that was not retained shows a marker, never a guess.
    #[test]
    fn rejected_calls_show_decision_fault_and_never_guess_content() {
        let rejected = pb::ToolProgress {
            ordinal: Some(1),
            name: Some("project".into()),
            state: Some(3),
            status: Some(3),
            arguments: Some(r#"{"command":"read_file"}"#.into()),
            policy_decision: Some(2),
            fault: Some(6),
            preview: Some("invalid_arguments: missing_field".into()),
            ..Default::default()
        };
        let oversized = pb::ToolProgress {
            ordinal: Some(2),
            name: Some(String::new()),
            state: Some(3),
            status: Some(3),
            arguments: Some("[not retained: oversized, 70000 bytes]".into()),
            policy_decision: Some(2),
            fault: Some(12),
            ..Default::default()
        };
        let valid = pb::ToolProgress {
            ordinal: Some(3),
            name: Some("project".into()),
            state: Some(2),
            status: Some(1),
            arguments: Some(r#"{"command":"read_file","path":"README.md"}"#.into()),
            policy_decision: Some(1),
            ..Default::default()
        };
        let activity = Activity {
            tools: vec![rejected, oversized, valid],
            expanded: true,
            kind: 3,
            ..Default::default()
        };
        let display = activity.display();
        assert!(display.contains("1. project · failed"), "{display}");
        assert!(
            display.contains("· invalid_arguments: missing_field"),
            "{display}"
        );
        assert!(
            display.contains("2. [name not retained] · failed"),
            "{display}"
        );
        assert!(display.contains("[not retained: oversized, 70000 bytes]"));
        assert!(display.contains("· invalid_arguments: arguments_over_limit"));
        assert!(display.contains("3. project · complete"));
        assert!(display.contains("· service admitted"));
        for (code, label) in [(1, "invalid_json"), (15, "unregistered_name")] {
            assert_eq!(fault_label(code), Some(label));
        }
        assert_eq!(fault_label(16), None);
    }
    #[test]
    fn live_pending_activity_survives_heartbeats_and_collapses_on_completion() {
        let mut app = App::new();
        app.append_response("Inspect the project".into(), Some(vec![1; 16]));
        app.active_operation = Some(vec![1; 16]);
        app.conversation_busy = true;
        apply(&mut app, event(1, 2, 1, 1));
        assert!(draw(&mut app, 90, 25, "running").contains("exec_cmd · running"));
        let mut heartbeat = event(1, 2, 1, 1);
        heartbeat.tools.clear();
        apply(&mut app, heartbeat);
        assert_eq!(app.activity[0].tools.len(), 1);
        let mut complete = event(1, 3, 3, 2);
        complete.text = Some("Project inspection finished.".into());
        apply(&mut app, complete);
        assert!(!draw(&mut app, 90, 25, "collapsed").contains("exec_cmd · complete"));
        app.insert("keep this draft");
        key(&mut app, KeyCode::F(6));
        key(&mut app, KeyCode::Enter);
        assert!(draw(&mut app, 90, 25, "expanded").contains("exec_cmd · complete"));
        key(&mut app, KeyCode::Esc);
        assert_eq!(app.draft(), "keep this draft");
        app.conversation_busy = true;
        let mut duplicate = event(1, 3, 3, 2);
        duplicate.text = Some("Project inspection finished.".into());
        apply(&mut app, duplicate);
        assert!(!app.conversation_busy);
        assert_eq!(app.transcript.len(), 1);
    }
    /// WS-02/WS-04 (TUI): the activity row names the waiting step; an
    /// equal-cursor heartbeat may change only the step and statistics.
    #[test]
    fn waiting_step_label_follows_heartbeats_and_clears_on_text() {
        let mut app = App::new();
        app.append_response("Slow model".into(), Some(vec![1; 16]));
        app.active_operation = Some(vec![1; 16]);
        app.conversation_busy = true;
        let mut pending = event(1, 2, 1, 1);
        pending.tools.clear();
        pending.waiting_step = Some(3);
        apply(&mut app, pending.clone());
        assert!(
            draw(&mut app, 100, 25, "sending-input")
                .contains("Activity · Waiting: sending input · Esc stops · 0 tools")
        );
        pending.waiting_step = Some(4);
        apply(&mut app, pending.clone());
        assert!(
            app.activity[0]
                .display()
                .contains("Waiting: model output · Esc stops")
        );
        // No step keeps the generic label.
        pending.waiting_step = None;
        apply(&mut app, pending.clone());
        assert!(
            app.activity[0]
                .display()
                .contains("Activity · Waiting · 0 tools")
        );
        // Unknown or pre-admission values are not shown on an accepted turn.
        pending.waiting_step = Some(2);
        apply(&mut app, pending.clone());
        assert!(
            app.activity[0]
                .display()
                .contains("Activity · Waiting · 0 tools")
        );
        let mut text = event(1, 3, 2, 1);
        text.tools.clear();
        text.text = Some("partial".into());
        text.waiting_step = Some(4);
        apply(&mut app, text);
        assert!(app.activity[0].display().contains("Generating"));
        // A heartbeat cannot put a step back on a generating turn.
        pending.cursor = Some(3);
        pending.waiting_step = Some(4);
        apply(&mut app, pending);
        assert!(app.activity[0].display().contains("Generating"));
        assert!(!app.activity[0].display().contains("model output"));
        assert_eq!(app.transcript[0].1, "partial");
    }
    /// SJ-16 (TUI): wait step 5 shows the journal pause while present, then
    /// the normal running or terminal display.
    #[test]
    fn journal_pause_step_shows_paused_then_running_or_cancelled() {
        let mut app = App::new();
        app.append_response("Big answer".into(), Some(vec![1; 16]));
        app.active_operation = Some(vec![1; 16]);
        app.conversation_busy = true;
        let mut pending = event(1, 2, 1, 1);
        pending.tools.clear();
        pending.waiting_step = Some(5);
        apply(&mut app, pending.clone());
        assert!(
            draw(&mut app, 100, 25, "journal-paused")
                .contains("Activity · Paused: journal space is full. · Esc stops")
        );
        // Output resumes: the next text event clears the pause.
        let mut text = event(1, 3, 2, 1);
        text.tools.clear();
        text.text = Some("partial".into());
        apply(&mut app, text);
        assert!(app.activity[0].display().contains("Generating"));
        // A pause during output: an equal-cursor heartbeat sets it, and a
        // heartbeat without the step resumes the generating display.
        pending.cursor = Some(3);
        pending.waiting_step = Some(5);
        apply(&mut app, pending.clone());
        assert!(app.activity[0].display().contains(PAUSED_TEXT));
        pending.waiting_step = None;
        apply(&mut app, pending.clone());
        assert!(app.activity[0].display().contains("Generating"));
        pending.waiting_step = Some(5);
        apply(&mut app, pending);
        // Cancel during the pause: the terminal event shows Cancelled.
        let mut cancelled = event(1, 4, 5, 1);
        cancelled.tools.clear();
        cancelled.text = Some("partial".into());
        apply(&mut app, cancelled);
        let display = app.activity[0].display();
        assert!(display.contains("Cancelled"), "{display}");
        assert!(!display.contains("Paused"));
    }
    #[test]
    fn stale_operation_cursor_generation_and_observation_failure_are_safe() {
        let mut app = App::new();
        app.active_operation = Some(vec![1; 16]);
        apply(&mut app, event(1, 4, 2, 1));
        assert!(app.observe_activity(&event(1, 3, 1, 1)).is_none());
        let mut stale = event(1, 5, 2, 1);
        stale.generation = Some(2);
        assert!(app.observe_activity(&stale).is_none());
        app.append_response("Next".into(), Some(vec![2; 16]));
        app.active_operation = Some(vec![2; 16]);
        apply(&mut app, event(1, 6, 3, 2));
        assert_eq!(app.active_operation, Some(vec![2; 16]));
        assert_eq!(app.activity[0].cursor, 4);
        apply(&mut app, event(2, 2, 1, 1));
        app.activity_unavailable();
        assert!(
            app.activity[1]
                .display()
                .contains("Observation unavailable")
        );
        assert_eq!(app.activity[1].tools[0].state, Some(1));
    }
    #[test]
    fn navigation_retains_full_response_and_draft_through_resize_and_eviction() {
        let mut app = App::new();
        app.append_response("Long answer".into(), Some(vec![1; 16]));
        app.active_operation = Some(vec![1; 16]);
        let mut complete = event(1, 2, 3, 2);
        let text = format!(
            "START\n{}\nTHE FINAL LINE",
            "Long response content.\n".repeat(1000)
        );
        complete.text = Some(text.clone());
        apply(&mut app, complete);
        assert_eq!(app.transcript[0].1, text);
        app.insert("draft preserved");
        key(&mut app, KeyCode::F(6));
        assert!(draw(&mut app, 80, 24, "long-start").contains("START"));
        for _ in 0..200 {
            key(&mut app, KeyCode::PageDown);
        }
        assert!(draw(&mut app, 80, 24, "long-end").contains("THE FINAL LINE"));
        for (w, h) in [(8, 3), (1, 1), (2, 2), (0, 0), (40, 12), (80, 24)] {
            draw(&mut app, w, h, "resize");
        }
        app.handle(Event::Paste("must not insert".into()));
        assert_eq!(app.draft(), "draft preserved");
        for id in 2..=10 {
            app.append_response(format!("Input {id}"), Some(vec![id; 16]));
        }
        assert_eq!(app.transcript.len(), 8);
        assert_eq!(app.rendered_responses.len(), 8);
        assert!(app.rendered_responses[0].lines.is_empty());
        assert_eq!(app.activity.len(), 8);
        assert_eq!(app.response_selection, Some(0));
        key(&mut app, KeyCode::End);
        assert_eq!(app.response_selection, Some(7));
        key(&mut app, KeyCode::Up);
        assert_eq!(app.response_selection, Some(6));
        key(&mut app, KeyCode::Home);
        assert_eq!(app.response_selection, Some(0));
        key(&mut app, KeyCode::F(6));
        assert_eq!(app.draft(), "draft preserved");
    }
    #[test]
    fn all_tool_outcomes_and_explicit_recovery_are_visible() {
        let mut app = App::new();
        app.active_operation = Some(vec![1; 16]);
        app.retained_request = Some(super::super::super::conversation::Request::Setup(
            crate::conversation::Command::Observe([2; 16]),
        ));
        let mut complete = event(2, 3, 4, 3);
        complete.tools = (1..=8)
            .map(|ordinal| pb::ToolProgress {
                ordinal: Some(ordinal),
                name: Some("service_read_audit".into()),
                state: Some(if ordinal == 8 { 4 } else { 3 }),
                status: (ordinal < 8).then_some(ordinal.max(2)),
                ..Default::default()
            })
            .collect();
        apply(&mut app, complete);
        assert_eq!(app.activity.len(), 1);
        assert_eq!(app.activity[0].tools.len(), 8);
        app.activity[0].expanded = true;
        let text = app.activity[0].display();
        assert!(text.contains("· 8 tools"), "{text}");
        for state in [
            "denied",
            "invalid",
            "unavailable",
            "timeout",
            "cancelled",
            "resource limit",
            "interrupted",
        ] {
            assert!(text.contains(state), "{state}: {text}");
        }
    }
    /// TCR-D31 (owner change, 2026-10-18): a long turn's event carries the most
    /// recent window; the header counts every call from the last ordinal.
    #[test]
    fn long_turn_window_shows_recent_calls_and_the_full_count() {
        let mut app = App::new();
        app.active_operation = Some(vec![1; 16]);
        app.retained_request = Some(super::super::super::conversation::Request::Setup(
            crate::conversation::Command::Observe([2; 16]),
        ));
        let last = asura_control::model::MAX_TOOL_ORDINAL;
        let first = last + 1 - asura_control::MAX_TOOL_PROGRESS_ENTRIES as u32;
        let mut complete = event(2, 3, 4, 3);
        complete.tools = (first..=last)
            .map(|ordinal| pb::ToolProgress {
                ordinal: Some(ordinal),
                name: Some("service".into()),
                state: Some(if ordinal == last { 3 } else { 2 }),
                status: Some(if ordinal == last { 7 } else { 1 }),
                ..Default::default()
            })
            .collect();
        apply(&mut app, complete);
        app.activity[0].expanded = true;
        let text = app.activity[0].display();
        assert!(text.contains(&format!("· {last} tools")), "{text}");
        assert!(text.contains(&format!("⚙ {first}. service")), "{text}");
        assert!(text.contains(&format!("⚙ {last}. service")), "{text}");
        assert!(
            !text.contains(&format!("⚙ {}. service", first - 1)),
            "{text}"
        );
    }
}
