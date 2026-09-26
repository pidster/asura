//! Inert, bounded inspection of the same decoded events used by ordinary chat.

use std::collections::VecDeque;

use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::{
    Frame,
    layout::Rect,
    style::Style,
    widgets::{Block, Paragraph, Wrap},
};

use crate::ui::Palette;

const HISTORY_LIMIT: usize = 8;

#[derive(Default)]
pub struct KeyProbe {
    counter: u64,
    observations: VecDeque<String>,
}

impl KeyProbe {
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns an exit request only for explicit exit presses. All other keys
    /// remain diagnostic data, and paste content is never retained.
    pub fn handle(&mut self, event: Event) -> bool {
        let observation = match event {
            Event::Key(key) => {
                let exit = (key.code == KeyCode::Esc && key.modifiers.is_empty())
                    || (matches!(key.code, KeyCode::Char('q' | 'c'))
                        && key.modifiers == KeyModifiers::CONTROL);
                if exit && key.kind == KeyEventKind::Press {
                    return true;
                }
                format!(
                    "code={:?} kind={:?}\nmodifiers={}",
                    key.code,
                    key.kind,
                    modifiers(key.modifiers)
                )
            }
            Event::Paste(_) => "Paste ignored".into(),
            _ => return false,
        };
        self.counter = self.counter.saturating_add(1);
        self.observations
            .push_front(format!("#{} {observation}", self.counter));
        self.observations.truncate(HISTORY_LIMIT);
        false
    }

    pub fn draw(&mut self, frame: &mut Frame<'_>, light: bool) {
        let area = frame.area();
        let palette = Palette::new(light);
        let style = Style::default().fg(palette.text).bg(palette.base);
        frame.render_widget(Block::default().style(style), area);
        if area.width < 30 || area.height < 8 {
            frame.render_widget(
                Paragraph::new("Resize to 30 x 8.\nEsc / Ctrl+Q / Ctrl+C exits.")
                    .wrap(Wrap { trim: false })
                    .style(style),
                area,
            );
            return;
        }
        for (index, text) in [
            "KEYS · no submission",
            "Return Ctrl+Return Cmd+Return",
            "Shift+Return Option+Return",
            "Esc / Ctrl+Q / Ctrl+C exits",
        ]
        .into_iter()
        .enumerate()
        {
            frame.render_widget(
                Paragraph::new(text).style(style.fg(if index == 0 {
                    palette.accent
                } else {
                    palette.muted
                })),
                Rect::new(area.x, area.y + index as u16, area.width, 1),
            );
        }
        let history = if self.observations.is_empty() {
            "Press a candidate key.".into()
        } else {
            self.observations
                .iter()
                .cloned()
                .collect::<Vec<_>>()
                .join("\n")
        };
        frame.render_widget(
            Paragraph::new(history)
                .wrap(Wrap { trim: false })
                .style(style),
            Rect::new(area.x, area.y + 4, area.width, area.height - 4),
        );
    }
}

fn modifiers(value: KeyModifiers) -> String {
    let mut names: Vec<String> = [
        (KeyModifiers::SHIFT, "SHIFT"),
        (KeyModifiers::CONTROL, "CONTROL"),
        (KeyModifiers::ALT, "ALT"),
        (KeyModifiers::SUPER, "SUPER"),
        (KeyModifiers::HYPER, "HYPER"),
        (KeyModifiers::META, "META"),
    ]
    .into_iter()
    .filter(|(flag, _)| value.contains(*flag))
    .map(|(_, name)| name.into())
    .collect();
    let unknown = value.bits() & !KeyModifiers::all().bits();
    if unknown != 0 {
        names.push(format!("0x{unknown:02x}"));
    }
    if names.is_empty() {
        "NONE".into()
    } else {
        names.join("|")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyEvent, MouseButton, MouseEvent, MouseEventKind};
    use ratatui::{Terminal, backend::TestBackend};

    fn key(code: KeyCode, modifiers: KeyModifiers, kind: KeyEventKind) -> Event {
        Event::Key(KeyEvent::new_with_kind(code, modifiers, kind))
    }

    fn visible(probe: &mut KeyProbe, width: u16, height: u16, light: bool) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| probe.draw(frame, light)).unwrap();
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(0, 0)].bg, Palette::new(light).base);
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn ki1_return_classification_preserves_every_modifier_and_event_kind() {
        let mut probe = KeyProbe::new();
        for (modifier, label) in [
            (KeyModifiers::NONE, "NONE"),
            (KeyModifiers::CONTROL, "CONTROL"),
            (KeyModifiers::SUPER, "SUPER"),
            (KeyModifiers::SHIFT, "SHIFT"),
            (KeyModifiers::ALT, "ALT"),
            (KeyModifiers::HYPER, "HYPER"),
            (KeyModifiers::META, "META"),
            (KeyModifiers::CONTROL | KeyModifiers::SHIFT, "SHIFT|CONTROL"),
            (KeyModifiers::from_bits_retain(0x80), "0x80"),
        ] {
            for kind in [
                KeyEventKind::Press,
                KeyEventKind::Repeat,
                KeyEventKind::Release,
            ] {
                assert!(!probe.handle(key(KeyCode::Enter, modifier, kind)));
                let latest = &probe.observations[0];
                assert!(latest.contains("code=Enter"));
                assert!(latest.contains(&format!("kind={kind:?}")));
                assert!(latest.contains(&format!("modifiers={label}")));
            }
        }
    }

    #[test]
    fn ki1_history_is_bounded_counter_saturates_and_paste_is_redacted() {
        let mut probe = KeyProbe::new();
        for value in 'a'..='z' {
            assert!(!probe.handle(key(
                KeyCode::Char(value),
                KeyModifiers::NONE,
                KeyEventKind::Press
            )));
        }
        assert_eq!(probe.observations.len(), 8);
        assert!(probe.observations[0].contains("#26 code=Char('z')"));
        assert!(probe.observations[7].contains("#19 code=Char('s')"));
        probe.counter = u64::MAX;
        assert!(!probe.handle(Event::Paste("/send SECRET\u{1b}[2J".into())));
        assert_eq!(probe.counter, u64::MAX);
        assert_eq!(
            probe.observations[0],
            format!("#{} Paste ignored", u64::MAX)
        );
        assert!(
            !probe
                .observations
                .iter()
                .any(|text| text.contains("SECRET"))
        );
        let before = probe.observations.clone();
        for event in [
            Event::Resize(30, 8),
            Event::FocusGained,
            Event::FocusLost,
            Event::Mouse(MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: 1,
                row: 1,
                modifiers: KeyModifiers::NONE,
            }),
        ] {
            assert!(!probe.handle(event));
        }
        assert_eq!(probe.observations, before);
    }

    #[test]
    fn ki1_control_characters_are_escaped_in_diagnostic_text() {
        let mut probe = KeyProbe::new();
        for value in ['\u{1b}', '\r', '\n', '\t', '\u{85}', '\u{7f}'] {
            probe.handle(key(
                KeyCode::Char(value),
                KeyModifiers::NONE,
                KeyEventKind::Press,
            ));
            let first_line = probe.observations[0].lines().next().unwrap();
            assert!(first_line.chars().all(|character| !character.is_control()));
            assert!(first_line.contains('\\'));
        }
    }

    #[test]
    fn ki2_only_exact_exit_presses_exit() {
        let mut probe = KeyProbe::new();
        for (code, modifiers) in [
            (KeyCode::Esc, KeyModifiers::NONE),
            (KeyCode::Char('q'), KeyModifiers::CONTROL),
            (KeyCode::Char('c'), KeyModifiers::CONTROL),
        ] {
            for kind in [KeyEventKind::Repeat, KeyEventKind::Release] {
                assert!(!probe.handle(key(code, modifiers, kind)));
            }
            assert!(probe.handle(key(code, modifiers, KeyEventKind::Press)));
            assert!(!probe.handle(key(
                code,
                modifiers | KeyModifiers::ALT,
                KeyEventKind::Press
            )));
        }
        for code in [
            KeyCode::Enter,
            KeyCode::F(5),
            KeyCode::Char('s'),
            KeyCode::Char('t'),
        ] {
            assert!(!probe.handle(key(code, KeyModifiers::CONTROL, KeyEventKind::Press)));
        }
    }

    #[test]
    fn ki1_actual_buffers_keep_candidates_and_newest_event_readable() {
        for (width, height) in [(80, 24), (40, 12), (30, 8)] {
            for light in [false, true] {
                for modifier in [
                    KeyModifiers::NONE,
                    KeyModifiers::CONTROL,
                    KeyModifiers::SUPER,
                    KeyModifiers::SHIFT,
                    KeyModifiers::ALT,
                    KeyModifiers::all(),
                ] {
                    let mut probe = KeyProbe::new();
                    for _ in 0..10 {
                        probe.handle(Event::Paste("PRIVATE".into()));
                    }
                    probe.handle(key(KeyCode::Enter, modifier, KeyEventKind::Press));
                    let screen = visible(&mut probe, width, height, light);
                    for expected in [
                        "no submission",
                        "Return",
                        "Ctrl+Return",
                        "Cmd+Return",
                        "Shift+Return",
                        "Option+Return",
                        "Ctrl+Q",
                        "#11 code=Enter",
                        "kind=Press",
                    ] {
                        assert!(screen.contains(expected), "{expected}: {screen}");
                    }
                    let unwrapped: String = screen.lines().map(str::trim_end).collect();
                    assert!(
                        unwrapped.contains(&format!("modifiers={}", modifiers(modifier))),
                        "{screen}"
                    );
                    assert!(!screen.contains("PRIVATE"));
                }
                let mut probe = KeyProbe::new();
                probe.handle(Event::Paste("PRIVATE".into()));
                assert!(visible(&mut probe, width, height, light).contains("#1 Paste ignored"));
            }
        }
    }
}
