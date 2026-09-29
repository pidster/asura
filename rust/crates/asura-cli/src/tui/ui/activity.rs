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
    expanded: bool,
    unavailable: bool,
}
impl Activity {
    pub(super) fn active(&self) -> bool {
        self.kind < 3 && !self.unavailable
    }
    pub(super) fn display(&self) -> String {
        let state = if self.unavailable {
            "Observation unavailable"
        } else {
            match self.kind {
                0 | 1 => "Waiting",
                2 => "Generating",
                3 => "Complete",
                4 => "Failed",
                5 => "Cancelled",
                6 => "Interrupted",
                _ => "Unknown",
            }
        };
        let expanded = self.active() || self.expanded;
        let mut text = format!(
            "{} Activity · {state} · {} tools",
            if expanded { "▾" } else { "▸" },
            self.tools.len()
        );
        if expanded {
            for tool in &self.tools {
                let state = match tool.state {
                    Some(1) => "running",
                    Some(2) => "complete",
                    Some(3) => "failed",
                    Some(4) => "interrupted",
                    _ => "unknown",
                };
                text.push_str(&format!(
                    "\n  {}. {} · {state}",
                    tool.ordinal.unwrap_or(0),
                    literal(tool.name.as_deref().unwrap_or("unknown"), 64)
                ));
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
            }
        }
        text
    }
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
            return None;
        }
        entry.operation = Some(operation.clone());
        entry.generation = generation;
        entry.cursor = cursor;
        if entry.kind >= 3 && event.kind.is_some_and(|kind| kind < 3) {
            return None;
        }
        entry.kind = event.kind.unwrap_or(1);
        entry.unavailable = false;
        if !event.tools.is_empty() || entry.kind != 1 {
            entry.tools = event.tools.iter().take(8).cloned().collect();
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
    pub(super) fn history_key(&mut self, key: KeyEvent) -> bool {
        if !self.history_focus {
            return false;
        }
        if key.modifiers == KeyModifiers::CONTROL && matches!(key.code, KeyCode::Char('p' | 'n')) {
            self.history_focus = false;
            return false;
        }
        if !key.modifiers.is_empty() {
            return true;
        }
        match key.code {
            KeyCode::Esc => self.history_focus = false,
            KeyCode::Up => self.history_scroll = self.history_scroll.saturating_sub(1),
            KeyCode::Down if self.history_scroll >= self.history_scroll_max => {
                self.history_focus = false;
            }
            KeyCode::Down => {
                self.history_scroll = (self.history_scroll + 1).min(self.history_scroll_max)
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
                name: Some("shell".into()),
                state: Some(state),
                status: (state == 2).then_some(1),
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
    fn live_pending_activity_survives_heartbeats_and_collapses_on_completion() {
        let mut app = App::new();
        app.append_response("Inspect the project".into(), Some(vec![1; 16]));
        app.active_operation = Some(vec![1; 16]);
        app.conversation_busy = true;
        apply(&mut app, event(1, 2, 1, 1));
        assert!(draw(&mut app, 90, 25, "running").contains("shell · running"));
        let mut heartbeat = event(1, 2, 1, 1);
        heartbeat.tools.clear();
        apply(&mut app, heartbeat);
        assert_eq!(app.activity[0].tools.len(), 1);
        let mut complete = event(1, 3, 3, 2);
        complete.text = Some("Project inspection finished.".into());
        apply(&mut app, complete);
        assert!(!draw(&mut app, 90, 25, "collapsed").contains("shell · complete"));
        app.insert("keep this draft");
        key(&mut app, KeyCode::F(6));
        key(&mut app, KeyCode::Enter);
        assert!(draw(&mut app, 90, 25, "expanded").contains("shell · complete"));
        key(&mut app, KeyCode::Esc);
        assert_eq!(app.draft(), "keep this draft");
        app.conversation_busy = true;
        let mut duplicate = event(1, 3, 3, 2);
        duplicate.text = Some("Project inspection finished.".into());
        apply(&mut app, duplicate);
        assert!(!app.conversation_busy);
        assert_eq!(app.transcript.len(), 1);
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
            })
            .collect();
        apply(&mut app, complete);
        assert_eq!(app.activity.len(), 1);
        assert_eq!(app.activity[0].tools.len(), 8);
        app.activity[0].expanded = true;
        let text = app.activity[0].display();
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
}
