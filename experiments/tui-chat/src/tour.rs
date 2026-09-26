//! Bounded, checked demonstrations through the ordinary application input path.

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::{
    Frame, Terminal,
    backend::TestBackend,
    layout::Rect,
    style::Style,
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
};

use crate::{
    app::{App, Overlay},
    model::{Action, MessageState, Project, RequestId},
    ui::{self, Palette},
};

pub const SCENE_COUNT: usize = 10;
const MAX_SCENE_EVENTS: usize = 99;
const TITLES: [&str; SCENE_COUNT] = [
    "Multiline draft",
    "Steer or Queue",
    "Expired decision",
    "Project switch",
    "Recovery",
    "Unknown outcome",
    "Reconnected",
    "Explicit reset",
    "Message tray",
    "Inspect full text",
];
const OBSERVATIONS: [&str; SCENE_COUNT] = [
    "Inspect the hard lines, combining accent, joined emoji and wide characters. Newline and paste undo/redo checks have passed; nothing was submitted.",
    "The choice opens without selecting Steer or Queue. Repeated Enter cannot submit. Preparation also checked Steer with a newer draft and Queue starting after success.",
    "The captured decision expired while focused. Its unavailable placeholder keeps the draft intact; Enter cannot answer it or send the draft.",
    "Observatory has its own draft. The header still shows Studio working. Round-trip navigation preserved both drafts and their separate undo/redo history.",
    "Rejected text was explicitly appended to a newer draft. Undo/redo checked that the append is one transaction and recovery did not submit anything.",
    "The fixture accepted a request, then lost its acknowledgement. Its displayed outcome remains unknown. The independent draft is retained; hidden acceptance is not presented as acknowledged.",
    "Reconnection resolved the original request once. A repeated disconnect/reconnect did not create another task or repeat the accepted message. The newer draft remains intact.",
    "Reset has no default choice and retains text in both projects. Preparation also checked that explicit confirmation clears both.",
    "The message tray retains acknowledged Steer and two queued follow-ups. The status bar combines synthetic project/Git and model/context data with the new draft count and running spinner. Enter still chooses Steer or Queue. Use the ordinary session for physical shortcut checks.",
    "Inspection retained its captured full text and focus while the task completed. Enter stayed read-only. Automated scenario checks are complete; native appearance and usability still need your report.",
];

pub struct Tour {
    app: App,
    index: usize,
    palette_inverted: bool,
    help: bool,
    help_scroll: usize,
}

impl Tour {
    pub fn new() -> Result<Self, String> {
        Ok(Self {
            app: prepare(0)?,
            index: 0,
            palette_inverted: false,
            help: false,
            help_scroll: 0,
        })
    }

    pub fn index(&self) -> usize {
        self.index
    }

    pub fn title(&self) -> &'static str {
        TITLES[self.index]
    }

    pub fn app(&self) -> &App {
        &self.app
    }

    /// Only tour controls and resize reach state; scene input is synthetic.
    pub fn handle(&mut self, event: Event) -> Result<bool, String> {
        let key = match event {
            Event::Resize(width, height) => {
                self.app
                    .handle(Event::Resize(width, height.saturating_sub(1)));
                self.clamp_help_scroll();
                return Ok(false);
            }
            Event::Key(key) if key.kind == KeyEventKind::Press => key,
            _ => return Ok(false),
        };
        if key.modifiers == KeyModifiers::CONTROL && matches!(key.code, KeyCode::Char('q' | 'c')) {
            return Ok(true);
        }
        if !key.modifiers.is_empty() {
            return Ok(false);
        }
        match key.code {
            KeyCode::Char('q') => return Ok(true),
            KeyCode::Esc if !self.help => return Ok(true),
            KeyCode::Esc => self.help = false,
            KeyCode::Char('l') => self.palette_inverted = !self.palette_inverted,
            KeyCode::Char('h') | KeyCode::F(1) => {
                // A tiny viewport can dismiss help, but cannot open another view.
                if self.help || self.usable() {
                    self.help = !self.help;
                    self.help_scroll = 0;
                }
            }
            _ if !self.usable() => {}
            KeyCode::Up | KeyCode::PageUp if self.help => {
                self.help_scroll = self.help_scroll.saturating_sub(self.scroll_step(key.code));
            }
            KeyCode::Down | KeyCode::PageDown if self.help => {
                self.help_scroll = self.help_scroll.saturating_add(self.scroll_step(key.code));
                self.clamp_help_scroll();
            }
            _ if self.help => {}
            KeyCode::Char('n') | KeyCode::Right => {
                self.select((self.index + 1).min(SCENE_COUNT - 1))?;
            }
            KeyCode::Char('p') | KeyCode::Left => {
                self.select(self.index.saturating_sub(1))?;
            }
            KeyCode::Char('r') => self.select(0)?,
            _ => {}
        }
        Ok(false)
    }

    pub fn draw(&mut self, frame: &mut Frame<'_>, light: bool) {
        let area = frame.area();
        let light = light ^ self.palette_inverted;
        let palette = Palette::new(light);
        let base = Style::default().fg(palette.text).bg(palette.base);
        let content = Rect::new(
            area.x,
            area.y.saturating_add(1),
            area.width,
            area.height.saturating_sub(1),
        )
        .intersection(area);
        ui::draw_in_area(
            frame,
            &mut self.app,
            light,
            content,
            "Locked · H help Q quit",
        );
        if !self.usable() {
            frame.render_widget(Clear, area);
            frame.render_widget(
                Paragraph::new("Resize to 30 × 9 or larger.\nQ quits · H closes help")
                    .style(base)
                    .wrap(Wrap { trim: false }),
                area,
            );
            return;
        }
        let instructions = Rect::new(area.x, area.y, area.width, 1);
        frame.render_widget(Clear, instructions);
        frame.render_widget(Block::default().style(base), instructions);
        let mut label = format!(
            "Tour {}/{} N/P L H help Q quit",
            self.index + 1,
            SCENE_COUNT
        );
        if area.width < 40 {
            label = format!("Tour {}/{} N/P L H Q quit", self.index + 1, SCENE_COUNT);
        }
        if area.width >= 60 {
            label.push_str(" · editing locked · ");
            label.push_str(self.title());
        }
        frame.render_widget(
            Paragraph::new(label).style(base.fg(palette.accent)),
            Rect::new(
                instructions.x.saturating_add(1),
                instructions.y,
                instructions.width.saturating_sub(2),
                1,
            ),
        );
        if self.help {
            let popup = help_rect(area);
            let block = Block::default()
                .borders(Borders::ALL)
                .title(" Tour help ")
                .title_bottom(" H/Esc close · Q quit ")
                .border_style(base.fg(palette.accent))
                .style(base);
            let inner = block.inner(popup);
            self.clamp_help_scroll();
            frame.render_widget(Clear, popup);
            frame.render_widget(block, popup);
            frame.render_widget(
                Paragraph::new(self.help_text())
                    .style(base)
                    .wrap(Wrap { trim: false })
                    .scroll((u16::try_from(self.help_scroll).unwrap_or(u16::MAX), 0)),
                inner,
            );
            // The application focus is retained underneath this presentation.
            frame.set_cursor_position((inner.x, inner.y));
        }
    }

    fn usable(&self) -> bool {
        self.app.width >= 30 && self.app.height >= 8
    }

    fn select(&mut self, index: usize) -> Result<(), String> {
        let mut app = prepare(index)?;
        app.handle(Event::Resize(self.app.width, self.app.height));
        self.app = app;
        self.index = index;
        self.help_scroll = 0;
        Ok(())
    }

    fn help_text(&self) -> String {
        format!(
            "Scene {}/{}: {}\n\n{}\n\nN / Right: next. P / Left: previous. R: restart from scene 1.\nL: toggle palette. H / F1 / Escape: close help. Q / Ctrl+Q / Ctrl+C: quit.\nUp / Down / Page keys: scroll this help.\n\nEditing is locked: typing, paste, Enter and application fixture keys are ignored. Scenes never advance on a timer.\n\nResize and use your terminal's native selection/copy controls. Review both palettes at 120×40, 80×24 and 40×12 in both terminals.\n\nNext means navigation, not approval. Generated events do not prove physical keys, native fonts, copy or usability. Report your observations separately.",
            self.index + 1,
            SCENE_COUNT,
            self.title(),
            OBSERVATIONS[self.index]
        )
    }

    fn scroll_step(&self, code: KeyCode) -> usize {
        if matches!(code, KeyCode::PageUp | KeyCode::PageDown) {
            usize::from(self.app.height.saturating_sub(4).clamp(1, 20))
        } else {
            1
        }
    }

    fn clamp_help_scroll(&mut self) {
        let popup = help_rect(Rect::new(
            0,
            0,
            self.app.width,
            self.app.height.saturating_add(1),
        ));
        let width = popup.width.saturating_sub(2).max(1);
        let height = popup.height.saturating_sub(2);
        let rows = Paragraph::new(self.help_text())
            .wrap(Wrap { trim: false })
            .line_count(width);
        self.help_scroll = self
            .help_scroll
            .min(rows.saturating_sub(usize::from(height)));
    }
}

fn help_rect(area: Rect) -> Rect {
    let width = area.width.saturating_sub(4).min(90);
    let height = area.height.saturating_sub(2).min(24);
    Rect::new(
        area.x.saturating_add((area.width - width) / 2),
        area.y.saturating_add((area.height - height) / 2),
        width,
        height,
    )
}

/// A preparation budget spans internal branch checks and the rebuilt checkpoint.
struct Preparation {
    app: App,
    scene: usize,
    events: usize,
}

impl Preparation {
    fn new(scene: usize) -> Self {
        Self {
            app: App::new(true),
            scene,
            events: 0,
        }
    }

    fn fresh(&mut self) {
        self.app = App::new(true);
    }

    fn error(&self, check: &str) -> String {
        let title = TITLES.get(self.scene).copied().unwrap_or("Unknown scene");
        format!("Tour scene {} ({title}): {check}", self.scene + 1)
    }

    fn check(&self, condition: bool, check: &str) -> Result<(), String> {
        if condition {
            Ok(())
        } else {
            Err(self.error(check))
        }
    }

    fn event(&mut self, event: Event) -> Result<(), String> {
        self.check(
            self.events < MAX_SCENE_EVENTS,
            "preparation event bound exceeded",
        )?;
        self.events += 1;
        self.app.handle(event);
        self.check(!self.app.exit, "preparation unexpectedly requested exit")
    }

    fn key(&mut self, code: KeyCode) -> Result<(), String> {
        self.event(Event::Key(KeyEvent::new(code, KeyModifiers::NONE)))
    }

    fn control(&mut self, character: char) -> Result<(), String> {
        self.event(Event::Key(KeyEvent::new(
            KeyCode::Char(character),
            KeyModifiers::CONTROL,
        )))
    }

    fn paste(&mut self, text: &str) -> Result<(), String> {
        self.event(Event::Paste(text.into()))
    }

    /// Direct shortcuts use exactly the target presented by the real renderer.
    fn paint(&mut self) -> Result<(), String> {
        let mut terminal = Terminal::new(TestBackend::new(120, 40))
            .map_err(|error| self.error(&format!("checkpoint setup failed: {error}")))?;
        terminal
            .draw(|frame| ui::draw(frame, &mut self.app, false))
            .map_err(|error| self.error(&format!("checkpoint draw failed: {error}")))?;
        Ok(())
    }

    fn draft(&self, text: &str, check: &str) -> Result<(), String> {
        self.check(self.app.editor.text() == text, check)
    }

    fn task_id(&self) -> Result<u64, String> {
        self.app
            .fixture
            .task
            .as_ref()
            .map(|task| task.target.id)
            .ok_or_else(|| self.error("expected active task"))
    }

    fn working(&mut self) -> Result<(), String> {
        self.paste("Explore the interaction prototype")?;
        self.key(KeyCode::Enter)?;
        self.key(KeyCode::F(5))?;
        self.check(
            self.app.fixture.task.is_some(),
            "initial work was not accepted",
        )
    }

    fn choice(&mut self, text: &str) -> Result<(), String> {
        self.paste(text)?;
        self.key(KeyCode::Enter)?;
        self.key(KeyCode::Enter)?;
        self.check(
            matches!(
                self.app.overlay,
                Some(Overlay::Submission { selected: None, .. })
            ) && self.app.fixture.pending.is_none(),
            "repeated Enter selected or submitted a choice",
        )?;
        self.draft(text, "unselected choice changed draft")
    }

    fn unknown(&mut self) -> Result<(RequestId, u64), String> {
        self.paste("Accepted before disconnect")?;
        self.key(KeyCode::Enter)?;
        let request = self
            .app
            .fixture
            .pending
            .as_ref()
            .map(|pending| pending.id)
            .ok_or_else(|| self.error("request did not enter pending state"))?;
        self.key(KeyCode::F(7))?;
        let task = self.task_id()?;
        self.paste("Independent draft")?;
        self.check(
            !self.app.fixture.connected
                && self
                    .app
                    .fixture
                    .pending
                    .as_ref()
                    .is_some_and(|pending| pending.id == request && pending.unknown)
                && self.app.fixture.display_task().is_none()
                && !self
                    .app
                    .fixture
                    .transcript
                    .iter()
                    .any(|entry| entry.text.contains("Accepted before disconnect")),
            "lost acknowledgement leaked an outcome or changed request identity",
        )?;
        self.draft("Independent draft", "unknown outcome changed newer draft")?;
        Ok((request, task))
    }
}

fn prepare(index: usize) -> Result<App, String> {
    let mut scene = Preparation::new(index);
    match index {
        0 => {
            let first = "Unicode: e\u{301} 👩🏽‍💻 中文";
            scene.paste(first)?;
            scene.event(Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT)))?;
            let before_paste = format!("{first}\n");
            scene.draft(&before_paste, "Option+Return did not insert a newline")?;
            scene.paste("Paste stays one edit.\nNo accidental send.")?;
            let complete = format!("{before_paste}Paste stays one edit.\nNo accidental send.");
            scene.control('z')?;
            scene.draft(&before_paste, "paste undo was not atomic")?;
            scene.control('y')?;
            scene.draft(&complete, "paste redo did not restore text")?;
            scene.check(
                scene.app.fixture.pending.is_none(),
                "editing submitted text",
            )?;
        }
        1 => {
            scene.working()?;
            let task = scene.task_id()?;
            scene.choice("Refine the active work")?;
            scene.key(KeyCode::Tab)?;
            scene.key(KeyCode::Enter)?;
            scene.check(
                scene
                    .app
                    .fixture
                    .pending
                    .as_ref()
                    .is_some_and(|pending| pending.action == Action::Steer),
                "explicit Steer did not capture a steer request",
            )?;
            scene.paste("Independent next draft")?;
            scene.key(KeyCode::F(5))?;
            scene.draft(
                "Independent next draft",
                "Steer acceptance changed newer draft",
            )?;
            scene.check(
                scene.task_id()? == task
                    && scene.app.fixture.pending.is_none()
                    && scene
                        .app
                        .fixture
                        .transcript
                        .iter()
                        .any(|entry| entry.text.contains("Refine the active work")),
                "Steer acceptance did not remain on the captured task",
            )?;
            scene.fresh();
            scene.working()?;
            let task = scene.task_id()?;
            scene.choice("Follow up after success")?;
            scene.key(KeyCode::Tab)?;
            scene.key(KeyCode::Tab)?;
            scene.key(KeyCode::Enter)?;
            scene.key(KeyCode::F(5))?;
            scene.check(
                scene.app.fixture.queued.len() == 1
                    && scene.app.fixture.queued.front().is_some_and(|queued| {
                        queued.text == "Follow up after success" && queued.action == Action::Queue
                    }),
                "Queue did not retain the captured follow-up",
            )?;
            scene.key(KeyCode::F(5))?;
            scene.check(
                scene.task_id()? != task
                    && scene.app.fixture.queued.is_empty()
                    && scene.app.fixture.transcript.iter().any(|entry| {
                        entry.text == "Fixture  Starting queued follow-up: Follow up after success"
                    }),
                "successful work did not start the queued turn",
            )?;
            scene.fresh();
            scene.working()?;
            scene.choice("Refine this, or save it for later")?;
        }
        2 => {
            scene.working()?;
            scene.paste("Keep this draft while controls change")?;
            scene.key(KeyCode::F(3))?;
            scene.control('o')?;
            scene.key(KeyCode::Enter)?;
            scene.check(
                scene
                    .app
                    .fixture
                    .task
                    .as_ref()
                    .is_some_and(|task| task.decision.is_some()),
                "unselected Enter answered the decision",
            )?;
            scene.key(KeyCode::F(3))?;
            scene.key(KeyCode::Tab)?;
            scene.key(KeyCode::Enter)?;
            scene.check(
                matches!(scene.app.overlay, Some(Overlay::Decision { .. }))
                    && scene.app.fixture.pending.is_none()
                    && scene
                        .app
                        .fixture
                        .task
                        .as_ref()
                        .is_some_and(|task| task.decision.is_none()),
                "expired decision lost focus or activated another action",
            )?;
            scene.draft(
                "Keep this draft while controls change",
                "decision expiry changed draft",
            )?;
        }
        3 => {
            scene.working()?;
            let task = scene.task_id()?;
            scene.paste("Studio draft")?;
            scene.paste(" + retained edit")?;
            scene.control('p')?;
            scene.paste("Observatory draft")?;
            scene.paste(" + retained edit")?;
            scene.control('p')?;
            scene.draft(
                "Studio draft + retained edit",
                "Studio draft lost across navigation",
            )?;
            scene.control('z')?;
            scene.draft("Studio draft", "Studio undo history changed")?;
            scene.control('y')?;
            scene.draft(
                "Studio draft + retained edit",
                "Studio redo history changed",
            )?;
            scene.check(
                scene.task_id()? == task,
                "project navigation retargeted work",
            )?;
            scene.control('p')?;
            scene.draft(
                "Observatory draft + retained edit",
                "Observatory draft lost",
            )?;
            scene.control('z')?;
            scene.draft("Observatory draft", "Observatory undo history changed")?;
            scene.control('y')?;
            scene.draft(
                "Observatory draft + retained edit",
                "Observatory redo history changed",
            )?;
            scene.check(
                scene.app.project() == Project::Observatory
                    && scene.app.fixture.task.is_none()
                    && scene
                        .app
                        .other_fixture()
                        .task
                        .as_ref()
                        .is_some_and(|other| other.target.id == task),
                "visible project or background task changed",
            )?;
        }
        4 => {
            scene.key(KeyCode::F(4))?;
            scene.paste("Rejected original")?;
            scene.key(KeyCode::Enter)?;
            scene.key(KeyCode::F(5))?;
            scene.check(
                scene.app.fixture.recoverable.len() == 1,
                "rejection did not retain text",
            )?;
            scene.paste("Newer draft")?;
            scene.control('o')?;
            scene.key(KeyCode::Enter)?;
            scene.check(
                scene.app.fixture.recoverable.len() == 1,
                "recovery had a default choice",
            )?;
            scene.key(KeyCode::Tab)?;
            scene.key(KeyCode::Enter)?;
            scene.draft(
                "Newer draft\nRejected original",
                "recovery did not append both drafts",
            )?;
            scene.control('z')?;
            scene.draft("Newer draft", "recovery undo was not one transaction")?;
            scene.control('y')?;
            scene.draft(
                "Newer draft\nRejected original",
                "recovery redo changed text",
            )?;
            scene.check(
                scene.app.fixture.recoverable.is_empty()
                    && scene.app.fixture.pending.is_none()
                    && scene.app.fixture.task.is_none(),
                "recovery submitted or retained a duplicate request",
            )?;
        }
        5 => {
            scene.unknown()?;
        }
        6 => {
            let (request, task) = scene.unknown()?;
            scene.key(KeyCode::F(2))?;
            scene.check(
                scene.app.fixture.connected
                    && scene.app.fixture.pending.is_none()
                    && scene.task_id()? == task
                    && scene
                        .app
                        .fixture
                        .last_event
                        .is_some_and(|event| event.request == Some(request)),
                "reconnect did not reconcile the original request",
            )?;
            scene.check(
                scene
                    .app
                    .fixture
                    .transcript
                    .iter()
                    .filter(|entry| entry.text.contains("Accepted before disconnect"))
                    .count()
                    == 1,
                "reconnect did not display the accepted message exactly once",
            )?;
            scene.key(KeyCode::F(2))?;
            scene.key(KeyCode::F(2))?;
            scene.check(
                scene.task_id()? == task
                    && scene.app.fixture.pending.is_none()
                    && scene
                        .app
                        .fixture
                        .transcript
                        .iter()
                        .filter(|entry| entry.text.contains("Accepted before disconnect"))
                        .count()
                        == 1,
                "repeated reconnect created another task or repeated the accepted request",
            )?;
            scene.draft("Independent draft", "reconnect changed newer draft")?;
        }
        7 => {
            reset_checkpoint(&mut scene)?;
            scene.key(KeyCode::Tab)?;
            scene.key(KeyCode::Enter)?;
            scene.check(
                scene.app.editor.is_empty()
                    && scene.app.fixture.pending.is_none()
                    && scene.app.fixture.task.is_none()
                    && scene.app.fixture.target().scope.generation == 2,
                "explicit reset did not clear the selected context",
            )?;
            scene.control('p')?;
            scene.check(
                scene.app.editor.is_empty()
                    && scene.app.fixture.pending.is_none()
                    && scene.app.fixture.task.is_none()
                    && scene.app.fixture.target().scope.generation == 2,
                "explicit reset did not clear the other context",
            )?;
            scene.fresh();
            reset_checkpoint(&mut scene)?;
        }
        8 => {
            scene.working()?;
            let task = scene.task_id()?;
            for (text, shortcut, state) in [
                (
                    "Keep the interface responsive",
                    's',
                    MessageState::Acknowledged,
                ),
                ("Explore command discovery next", 't', MessageState::Queued),
                (
                    "Then review the interaction details",
                    't',
                    MessageState::Queued,
                ),
            ] {
                scene.paste(text)?;
                scene.paint()?;
                scene.control(shortcut)?;
                let request = scene
                    .app
                    .fixture
                    .pending
                    .as_ref()
                    .map(|pending| pending.id)
                    .ok_or_else(|| scene.error("direct shortcut did not capture request"))?;
                scene.key(KeyCode::F(5))?;
                scene.check(
                    scene.app.fixture.message_tray().items.iter().any(|item| {
                        item.id == request && item.state == state && item.text.as_ref() == text
                    }),
                    "acknowledged shortcut lost its tray identity or text",
                )?;
            }
            scene.paste("My next thought can steer this work or wait its turn…")?;
            scene.paint()?;
            scene.check(
                scene.task_id()? == task
                    && scene.app.fixture.display_queued_count() == Some(2)
                    && scene.app.fixture.message_tray().items.len() == 4,
                "message tray checkpoint has incorrect work or queue state",
            )?;
        }
        9 => {
            let text = "A message keeps its complete text.\n\nInspect spacing and hard lines.\nCombining: e\u{301}\nJoined emoji: 👩🏽‍💻\nWide text: 界面\n\nCompletion changes the state,\nwhile this view keeps its focus.\n\nEnter here cannot submit or recover.\nEnd of the captured message.";
            scene.paste(text)?;
            scene.key(KeyCode::Enter)?;
            scene.key(KeyCode::F(5))?;
            scene.paste("Independent draft retained underneath")?;
            scene.paint()?;
            scene.control('b')?;
            let focus = scene.app.overlay.clone();
            scene.check(focus.is_some(), "tray inspection did not open")?;
            scene.key(KeyCode::F(5))?;
            scene.key(KeyCode::Enter)?;
            scene.check(
                scene.app.overlay == focus
                    && scene.app.fixture.pending.is_none()
                    && scene.app.fixture.task.is_none()
                    && scene.app.fixture.message_tray().items.iter().any(|item| {
                        item.state == MessageState::Completed && item.text.as_ref() == text
                    }),
                "task completion or Enter changed inspection focus or submitted text",
            )?;
            scene.check(
                scene
                    .app
                    .overlay_view()
                    .is_some_and(|view| view.detail.contains(text)),
                "inspection omitted the captured full text",
            )?;
            scene.draft(
                "Independent draft retained underneath",
                "inspection changed draft",
            )?;
        }
        _ => return Err(scene.error("scene index is outside the tour")),
    }
    Ok(scene.app)
}

fn reset_checkpoint(scene: &mut Preparation) -> Result<(), String> {
    scene.paste("Studio draft to retain")?;
    scene.control('p')?;
    scene.paste("Observatory draft to retain")?;
    scene.key(KeyCode::F(6))?;
    scene.key(KeyCode::Enter)?;
    scene.draft(
        "Observatory draft to retain",
        "unselected reset erased visible draft",
    )?;
    scene.control('p')?;
    scene.draft(
        "Studio draft to retain",
        "unselected reset erased other draft",
    )?;
    scene.key(KeyCode::F(6))?;
    scene.key(KeyCode::Enter)?;
    scene.check(
        matches!(scene.app.overlay, Some(Overlay::Reset { selected: false })),
        "reset did not retain its unselected confirmation",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(tour: &mut Tour, code: KeyCode) -> bool {
        tour.handle(Event::Key(KeyEvent::new(code, KeyModifiers::NONE)))
            .unwrap()
    }

    #[test]
    fn gt1_every_scene_is_checked_and_navigation_clamps_and_restarts() {
        let mut tour = Tour::new().unwrap();
        key(&mut tour, KeyCode::Left);
        assert_eq!(tour.index(), 0);
        for (index, title) in TITLES.iter().enumerate().skip(1) {
            key(&mut tour, KeyCode::Char('n'));
            assert_eq!(tour.index(), index);
            assert_eq!(tour.title(), *title);
        }
        key(&mut tour, KeyCode::Right);
        assert_eq!(tour.index(), SCENE_COUNT - 1);
        key(&mut tour, KeyCode::Char('p'));
        assert_eq!(tour.index(), SCENE_COUNT - 2);
        key(&mut tour, KeyCode::Char('r'));
        assert_eq!(tour.index(), 0);
    }

    #[test]
    fn gt1_failure_names_the_scene_and_check_and_bounds_event_preparation() {
        let mut preparation = Preparation::new(3);
        assert_eq!(
            preparation
                .check(false, "retained draft disappeared")
                .unwrap_err(),
            "Tour scene 4 (Project switch): retained draft disappeared"
        );
        for _ in 0..MAX_SCENE_EVENTS {
            preparation.key(KeyCode::Esc).unwrap();
        }
        assert!(
            preparation
                .key(KeyCode::Esc)
                .unwrap_err()
                .contains("event bound")
        );
        assert!(prepare(SCENE_COUNT).err().unwrap().contains("scene index"));
        let mut tour = Tour::new().unwrap();
        let draft = tour.app().editor.text();
        assert!(tour.select(SCENE_COUNT).is_err());
        assert_eq!(tour.index(), 0);
        assert_eq!(tour.app().editor.text(), draft);
    }

    #[test]
    fn gt2_only_unmodified_control_presses_activate_and_user_input_is_ignored() {
        let mut tour = Tour::new().unwrap();
        let original = tour.app().editor.text();
        for event in [
            Event::Paste("n\nq real user text".into()),
            Event::Key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::F(6), KeyModifiers::NONE)),
            Event::Key(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::SHIFT)),
            Event::Key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::ALT)),
            Event::Key(KeyEvent::new_with_kind(
                KeyCode::Right,
                KeyModifiers::NONE,
                KeyEventKind::Repeat,
            )),
            Event::Key(KeyEvent::new_with_kind(
                KeyCode::Char('q'),
                KeyModifiers::NONE,
                KeyEventKind::Release,
            )),
            Event::Key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE)),
        ] {
            assert!(!tour.handle(event).unwrap());
        }
        assert_eq!(tour.index(), 0);
        assert_eq!(tour.app().editor.text(), original);
        assert!(tour.app().fixture.pending.is_none());
        assert!(tour.app().overlay.is_none());
        assert!(key(&mut tour, KeyCode::Esc));
        assert!(
            tour.handle(Event::Key(KeyEvent::new(
                KeyCode::Char('c'),
                KeyModifiers::CONTROL
            )))
            .unwrap()
        );
    }

    #[test]
    fn gt2_help_is_bounded_and_tiny_geometry_retains_state_and_exit() {
        let mut tour = Tour::new().unwrap();
        tour.handle(Event::Resize(40, 12)).unwrap();
        key(&mut tour, KeyCode::Char('h'));
        for _ in 0..100 {
            key(&mut tour, KeyCode::PageDown);
        }
        let last_row = tour.help_scroll;
        assert!(last_row > 0 && last_row < 100);
        key(&mut tour, KeyCode::PageDown);
        assert_eq!(tour.help_scroll, last_row);
        key(&mut tour, KeyCode::Char('n'));
        assert_eq!(tour.index(), 0);
        tour.handle(Event::Resize(29, 7)).unwrap();
        assert!(!key(&mut tour, KeyCode::Esc));
        assert!(!tour.help);
        key(&mut tour, KeyCode::Right);
        key(&mut tour, KeyCode::Char('r'));
        assert_eq!(tour.index(), 0);
        key(&mut tour, KeyCode::Char('l'));
        assert!(tour.palette_inverted);
        tour.handle(Event::Resize(40, 12)).unwrap();
        key(&mut tour, KeyCode::Right);
        assert_eq!(tour.index(), 1);
        assert_eq!((tour.app().width, tour.app().height), (40, 11));
        assert!(key(&mut tour, KeyCode::Char('q')));
    }
}
