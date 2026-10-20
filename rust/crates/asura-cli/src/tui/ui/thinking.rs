//! The thinking level item, `/think` and the folded thinking block
//! (thinking-level.md). The configuration worker writes the level; the
//! service maps it; this module only presents and requests.
use super::*;
use asura_control::thinking::{self, Level, Request};
use composer::Picker;

/// Picker rows in level code order.
pub(super) const ROWS: [(&str, Level); 5] = [
    ("model default", Level::Default),
    ("off", Level::Off),
    ("low", Level::Low),
    ("medium", Level::Medium),
    ("high", Level::High),
];
const USAGE: &str = "Use /think off|low|medium|high|default";

/// The status label for `level` and the observed model support
/// `(reasoning, controls)`, and whether it shows dimmed.
pub(super) fn label(level: Level, support: Option<(bool, u32)>) -> (String, bool) {
    let name = level.name();
    match support {
        None => (format!("think {name}"), false),
        Some((false, _)) => ("no thinking".into(), true),
        Some((true, controls)) => match (level, thinking::request(level, controls)) {
            (Level::Default, _) => ("think default".into(), false),
            (_, Request::None) => (format!("think {name} (n/a)"), true),
            (Level::Low | Level::Medium | Level::High, Request::On) => {
                (format!("think {name} (on)"), false)
            }
            _ => (format!("think {name}"), false),
        },
    }
}
/// A thinking level from a service observation code.
pub(super) fn from_code(code: Option<i32>) -> Option<Level> {
    code.and_then(|code| u8::try_from(code).ok())
        .and_then(Level::from_code)
}
/// The folded or open block above a response; `None` without thinking.
pub(super) fn block(text: &str, open: bool) -> Option<String> {
    if text.is_empty() {
        return None;
    }
    let count = text.chars().count();
    let unit = if count == 1 { "char" } else { "chars" };
    Some(if open {
        format!(
            "▾ Thinking · {count} {unit}\n{}",
            literal_multiline(text, usize::MAX)
        )
    } else {
        format!("▸ Thinking · {count} {unit}")
    })
}

impl App {
    /// The configured level: the latest saved or observed value.
    pub(super) fn thinking_level(&self) -> Level {
        self.thinking_level.unwrap_or_default()
    }
    /// `/think [LEVEL]`. Returns whether the draft is consumed.
    pub(super) fn think_command(&mut self, draft: &str) -> bool {
        let words: Vec<_> = draft.split_whitespace().collect();
        let level = match words.as_slice() {
            ["/think"] => {
                self.editor = Editor::new();
                self.notice = format!("Thinking: {} · {USAGE}", self.thinking_level().name());
                return false;
            }
            ["/think", value] => Level::parse(value),
            _ => None,
        };
        let Some(level) = level else {
            self.notice = format!("{USAGE}; draft retained");
            return false;
        };
        self.save_thinking(level, Some(draft.to_owned()));
        false
    }
    /// Sends `ConfigSet thinking`. `draft` is the command text to clear on
    /// success; `None` saves from the status picker.
    pub(super) fn save_thinking(&mut self, level: Level, draft: Option<String>) {
        if self.config_draft.is_some() || self.thinking_save.is_some() {
            self.notice = "Configuration request busy; draft retained".into();
            return;
        }
        self.config_request = Some(super::super::config::Request {
            draft: draft.clone().unwrap_or_default(),
            key: "thinking".into(),
            value: Some(level.name().into()),
        });
        self.config_draft = Some(draft.clone().unwrap_or_default());
        self.thinking_save = Some((level, draft.is_none()));
        self.notice = "Saving thinking level".into();
    }
    /// The config reply for a thinking save. Returns false when no thinking
    /// save is pending.
    pub(super) fn thinking_saved(&mut self, outcome: &super::super::config::Outcome) -> bool {
        let Some((level, picker)) = self.thinking_save.take() else {
            return false;
        };
        let submitted = self.config_draft.take();
        match outcome {
            Ok(_) => {
                self.thinking_level = Some(level);
                if picker {
                    if matches!(self.picker, Some(Picker::Thinking { .. })) {
                        self.picker = None;
                        self.focus = Focus::Thinking;
                    }
                } else if submitted.as_deref() == Some(self.editor.text().as_str()) {
                    self.editor = Editor::new();
                }
                self.notice = format!("Thinking: {} for next turns", level.name());
            }
            Err(error) => self.notice = format!("{}; draft retained", literal(error, 1024)),
        }
        true
    }
    /// The picker's current row and the note for a level the observed model
    /// does not take.
    pub(super) fn thinking_rows(&self) -> Vec<String> {
        ROWS.iter()
            .map(|(name, level)| {
                let current = if *level == self.thinking_level() {
                    " · current"
                } else {
                    ""
                };
                let applied = match self.thinking_support {
                    Some((true, controls))
                        if *level != Level::Default
                            && thinking::request(*level, controls) == Request::None =>
                    {
                        " · not applied to this model"
                    }
                    Some((false, _)) if *level != Level::Default => " · model cannot think",
                    _ => "",
                };
                format!("{name}{current}{applied}")
            })
            .collect()
    }
    /// Opens the thinking list at the current level.
    pub(super) fn open_thinking_picker(&mut self) {
        let selected = ROWS
            .iter()
            .position(|(_, level)| *level == self.thinking_level())
            .unwrap_or(0);
        self.picker = Some(Picker::Thinking { selected });
        self.notice.clear();
    }
    /// Records the model's reported thinking support from an event.
    pub(super) fn observe_thinking_support(&mut self, context: &asura_control::pb::ModelContext) {
        if let Some(reasoning) = context.reasoning {
            self.thinking_support = Some((reasoning, context.thinking_controls.unwrap_or(0)));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyEvent;
    use ratatui::{Terminal, backend::TestBackend};

    fn key(app: &mut App, code: KeyCode) {
        app.handle(Event::Key(KeyEvent::new(code, KeyModifiers::NONE)));
    }
    fn render(app: &mut App, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let buffer = terminal.backend().buffer();
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// TL-04 and TL-07 (display): labels for unknown, unsupported, uncontrolled,
    /// on/off-only and graded models.
    #[test]
    fn tl04_tl07_labels_follow_the_shared_mapping() {
        let both = thinking::OFF | thinking::ON;
        assert_eq!(label(Level::High, None), ("think high".into(), false));
        assert_eq!(
            label(Level::High, Some((false, 0))),
            ("no thinking".into(), true)
        );
        assert_eq!(
            label(Level::Default, Some((true, 0))),
            ("think default".into(), false)
        );
        assert_eq!(
            label(Level::Off, Some((true, 0))),
            ("think off (n/a)".into(), true)
        );
        assert_eq!(
            label(Level::Medium, Some((true, both))),
            ("think medium (on)".into(), false)
        );
        assert_eq!(
            label(Level::Off, Some((true, both))),
            ("think off".into(), false)
        );
        assert_eq!(
            label(Level::Low, Some((true, thinking::ALL))),
            ("think low".into(), false)
        );
        assert_eq!(from_code(Some(3)), Some(Level::Medium));
        assert_eq!(from_code(Some(9)), None);
        assert_eq!(from_code(Some(-1)), None);
    }

    /// TL-02, TL-03 and TL-14 (TUI): `/think` sends one config set, a bad
    /// value keeps the draft, a busy worker refuses, and a reply updates the
    /// level and clears the unchanged draft.
    #[test]
    fn tl02_tl14_think_command_saves_through_the_config_worker() {
        let mut app = App::new();
        app.insert("/think maximum");
        key(&mut app, KeyCode::Enter);
        assert!(app.take_config_request().is_none());
        assert_eq!(app.draft(), "/think maximum");
        assert!(app.notice.contains("draft retained"));
        app.editor = Editor::new();
        app.insert("/think");
        key(&mut app, KeyCode::Enter);
        assert!(app.take_config_request().is_none());
        assert!(app.notice.starts_with("Thinking: default"));
        app.insert("/think high");
        key(&mut app, KeyCode::Enter);
        let request = app.take_config_request().unwrap();
        assert_eq!(
            (request.key.as_str(), request.value.as_deref()),
            ("thinking", Some("high"))
        );
        // A second save waits for the first.
        app.save_thinking(Level::Low, None);
        assert!(app.take_config_request().is_none());
        app.config_finished(Ok("high\n".into()));
        assert_eq!(app.thinking_level(), Level::High);
        assert_eq!(app.draft(), "");
        assert_eq!(app.notice, "Thinking: high for next turns");
        app.insert("/think default");
        key(&mut app, KeyCode::Enter);
        assert!(app.take_config_request().is_some());
        app.config_finished(Err("config_io_or_unsafe_file".into()));
        assert_eq!(app.thinking_level(), Level::High);
        assert_eq!(app.draft(), "/think default");
        assert!(app.notice.contains("draft retained"));
    }

    /// TL-08: the status bar's thinking item opens a list at the current
    /// level; Enter saves the selected level; Esc closes without a change.
    #[test]
    fn tl08_status_item_list_selects_and_saves_a_level() {
        let mut app = App::new();
        app.thinking_support = Some((true, thinking::OFF | thinking::ON));
        key(&mut app, KeyCode::Down);
        assert_eq!(app.focus, Focus::Project);
        key(&mut app, KeyCode::Right);
        key(&mut app, KeyCode::Right);
        assert_eq!(app.focus, Focus::Thinking);
        key(&mut app, KeyCode::Right);
        assert_eq!(app.focus, Focus::Project);
        key(&mut app, KeyCode::Left);
        assert_eq!(app.focus, Focus::Thinking);
        key(&mut app, KeyCode::Enter);
        assert!(matches!(app.picker, Some(Picker::Thinking { selected: 0 })));
        let screen = render(&mut app, 100, 24);
        assert!(screen.contains("model default · current"), "{screen}");
        assert!(screen.contains("low"));
        key(&mut app, KeyCode::Esc);
        assert!(app.picker.is_none() && app.take_config_request().is_none());
        key(&mut app, KeyCode::Enter);
        key(&mut app, KeyCode::End);
        key(&mut app, KeyCode::Enter);
        let request = app.take_config_request().unwrap();
        assert_eq!(request.value.as_deref(), Some("high"));
        // Keys other than Esc wait while the save is pending.
        key(&mut app, KeyCode::Up);
        assert!(matches!(app.picker, Some(Picker::Thinking { selected: 4 })));
        app.config_finished(Ok("high\n".into()));
        assert!(app.picker.is_none());
        assert_eq!(app.focus, Focus::Thinking);
        let screen = render(&mut app, 100, 24);
        assert!(screen.contains("think high (on)"), "{screen}");
        key(&mut app, KeyCode::Esc);
        assert_eq!(app.focus, Focus::Editor);
    }

    /// TL-07 (display): a model that cannot reason shows `no thinking`.
    #[test]
    fn tl07_status_shows_a_model_that_cannot_think() {
        let mut app = App::new();
        app.thinking_level = Some(Level::High);
        app.observe_thinking_support(&asura_control::pb::ModelContext {
            model_name: Some("system".into()),
            reasoning: Some(false),
            thinking_controls: Some(0),
            ..Default::default()
        });
        let screen = render(&mut app, 100, 24);
        assert!(screen.contains("no thinking"), "{screen}");
        assert!(app.thinking_rows()[4].ends_with("model cannot think"));
    }

    /// TL-09 and TL-11: a turn's thinking shows folded above its answer; in
    /// the response browser `t` opens and folds it; the model support from
    /// the event reaches the status item.
    #[test]
    fn tl11_folded_block_opens_with_t_in_the_response_browser() {
        let mut app = App::new();
        app.append_response("Why?".into(), Some(vec![4; 16]));
        app.active_operation = Some(vec![4; 16]);
        app.conversation_finished(crate::tui::conversation::Update {
            event: Some(asura_control::pb::ConversationEvent {
                operation_id: Some(vec![4; 16]),
                cursor: Some(u64::MAX),
                generation: Some(1),
                kind: Some(3),
                reason: Some(0),
                text: Some("Because.".into()),
                text_offset: Some(0),
                text_bytes: Some(8),
                thinking_text: Some("First, \u{1b}[31mcheck the premise.".into()),
                thinking_offset: Some(0),
                thinking_bytes: Some(35),
                thinking_level: Some(4),
                thinking_request: Some(2),
                usage_known: Some(false),
                model_context: Some(asura_control::pb::ModelContext {
                    model_name: Some("ollama:qwen3".into()),
                    reasoning: Some(true),
                    thinking_controls: Some(thinking::OFF | thinking::ON),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            done: true,
            ..Default::default()
        });
        app.thinking_level = Some(Level::High);
        let screen = render(&mut app, 100, 24);
        assert!(screen.contains("▸ Thinking · "), "{screen}");
        assert!(!screen.contains("check the premise"));
        assert!(screen.contains("Because."));
        assert!(screen.contains("think high (on)"), "{screen}");
        key(&mut app, KeyCode::F(6));
        key(&mut app, KeyCode::Char('t'));
        let screen = render(&mut app, 100, 24);
        assert!(screen.contains("▾ Thinking · "), "{screen}");
        // Sanitized: the escape sequence never reaches the terminal.
        assert!(screen.contains("check the premise"), "{screen}");
        assert!(!screen.contains('\u{1b}'));
        key(&mut app, KeyCode::Char('t'));
        let screen = render(&mut app, 100, 24);
        assert!(!screen.contains("check the premise"));
    }

    #[test]
    fn folded_and_open_blocks_count_characters() {
        assert_eq!(block("", true), None);
        assert_eq!(block("é", false).unwrap(), "▸ Thinking · 1 char");
        assert_eq!(
            block("ab\ncd", true).unwrap(),
            "▾ Thinking · 5 chars\nab\ncd"
        );
    }
}
