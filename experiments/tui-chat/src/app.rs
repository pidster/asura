//! Client focus, captured actions and retained per-project presentation state.

use std::sync::Arc;

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::editor::{CommandHeader, Editor, MAX_DRAFT_BYTES};
use crate::model::{
    Action, CommandAvailability, CommandCapture, CommandCategory, CommandDefinition,
    CommandOperation, CommandOrigin, DecisionAction, Fixture, MessageState, Project, ProjectStatus,
    RequestId, RetainedRequest, SubmitError, Target,
};
use crate::viewport::TranscriptViewport;

const SPINNER: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ComposerStatus {
    pub project: ProjectStatus,
    pub characters: usize,
    pub spinner: Option<char>,
}

#[derive(Clone)]
pub struct CommandSuggestionView {
    pub rows: Vec<String>,
    pub cue: Option<String>,
    pub incomplete: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PresentedCommand {
    header: CommandHeader,
    definition: CommandDefinition,
    project: Project,
    target: Target,
    draft_revision: u64,
    catalogue_revision: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Attention {
    Connection { target: Target },
    Recovery { id: RequestId },
    Decision { target: Target, id: u64 },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Overlay {
    Help,
    Exit {
        selected: bool,
    },
    Clear {
        selected: bool,
    },
    Reset {
        selected: bool,
    },
    PasteFinish {
        selected: Option<bool>,
    },
    Submission {
        target: Target,
        text: Arc<str>,
        draft_revision: u64,
        selected: Option<usize>,
    },
    CommandSubmission {
        presented: PresentedCommand,
        text: Arc<str>,
        selected: Option<usize>,
    },
    CommandBrowser {
        target: Option<CommandHeader>,
        project: Project,
        draft_revision: u64,
        catalogue_revision: u64,
        entries: Vec<CommandDefinition>,
        selected: Option<usize>,
    },
    Decision {
        target: Target,
        id: u64,
        prompt: Arc<str>,
        selected: Option<usize>,
    },
    Recovery {
        id: RequestId,
        selected: Option<usize>,
    },
    Attention {
        items: Vec<Attention>,
        selected: Option<usize>,
    },
    Connection {
        target: Target,
        pending_id: Option<RequestId>,
    },
    MessageList {
        items: Vec<RequestId>,
        selected: Option<usize>,
    },
    MessageInspector {
        id: RequestId,
        text: Arc<str>,
        action: Action,
    },
}

/// Presentation data consumed by `ui`; indices are stable within one overlay.
pub struct OverlayView {
    pub title: String,
    pub actions: Vec<String>,
    pub selected: Option<usize>,
    pub detail: String,
}

struct ProjectState {
    editor: Editor,
    fixture: Fixture,
    viewport: TranscriptViewport,
    draft_revision: u64,
    notice: String,
    seen_output_end: u128,
    unread_output: bool,
}

pub struct App {
    pub editor: Editor,
    pub overlay: Option<Overlay>,
    pub overlay_scroll: usize,
    pub paste_mode: bool,
    pub captured: String,
    pub capture_error: Option<String>,
    pub notice: String,
    pub fixture: Fixture,
    pub manual: bool,
    now_ms: u64,
    presented_target: Option<Target>,
    presented_command: Option<PresentedCommand>,
    command_suggestions: Option<CommandSuggestionView>,
    pub viewport: TranscriptViewport,
    pub exit: bool,
    pub width: u16,
    pub height: u16,
    project: Project,
    draft_revision: u64,
    seen_output_end: u128,
    pub unread_output: bool,
    other: ProjectState,
}

impl Default for App {
    fn default() -> Self {
        Self {
            editor: Editor::new(),
            overlay: None,
            overlay_scroll: 0,
            paste_mode: false,
            captured: String::new(),
            capture_error: None,
            notice: String::new(),
            fixture: Fixture::for_project(Project::Studio),
            manual: false,
            now_ms: 0,
            presented_target: None,
            presented_command: None,
            command_suggestions: None,
            viewport: TranscriptViewport::new(),
            exit: false,
            width: 80,
            height: 24,
            project: Project::Studio,
            draft_revision: 0,
            seen_output_end: 0,
            unread_output: false,
            other: ProjectState {
                editor: Editor::new(),
                fixture: Fixture::for_project(Project::Observatory),
                viewport: TranscriptViewport::new(),
                draft_revision: 0,
                notice: String::new(),
                seen_output_end: 0,
                unread_output: false,
            },
        }
    }
}

impl App {
    pub fn new(manual: bool) -> Self {
        Self {
            manual,
            ..Self::default()
        }
    }

    pub fn project(&self) -> Project {
        self.project
    }

    pub fn other_fixture(&self) -> &Fixture {
        &self.other.fixture
    }

    /// Observe display progress without moving the viewport's retained anchor.
    /// Prefix eviction changes `base_id`, so end identity remains monotonic.
    pub fn observe_transcript(&mut self, reading: bool) {
        let end = self.fixture.base_id() + self.fixture.transcript.len() as u128;
        if reading {
            self.unread_output |= end > self.seen_output_end;
        } else {
            self.unread_output = false;
            self.seen_output_end = end;
        }
    }

    pub fn advance(&mut self, now_ms: u64) -> bool {
        let previous_frame = self.now_ms / 125 % SPINNER.len() as u64;
        self.now_ms = now_ms;
        let animation_changed =
            previous_frame != now_ms / 125 % SPINNER.len() as u64 && self.animating();
        if self.manual {
            return animation_changed;
        }
        // Do not short-circuit: invisible work advances under the same clock.
        let selected_changed = self.fixture.advance(now_ms);
        let other_changed = self.other.fixture.advance(now_ms);
        selected_changed || other_changed || animation_changed
    }

    fn animating(&self) -> bool {
        self.usable()
            && !self.fixture.delivery_blocked()
            && self
                .fixture
                .display_task()
                .is_some_and(|task| task.decision.is_none())
    }

    /// Capture only the active target the user could see in this paint. A later
    /// input batch must never retarget a shortcut to an unseen successor.
    pub fn record_presentation(&mut self) {
        self.presented_target = if self.usable()
            && self.overlay.is_none()
            && !self.paste_mode
            && self.fixture.display_task().is_some()
            && self.fixture.can_submit(Action::Steer).is_ok()
        {
            Some(self.fixture.target())
        } else {
            None
        };
        self.presented_command = self.resolve_command();
    }

    pub fn command_suggestions(&self) -> Option<&CommandSuggestionView> {
        self.command_suggestions.as_ref()
    }

    fn catalogue_entries(&self) -> (u64, bool, Vec<CommandDefinition>) {
        let catalogue = self.fixture.command_catalogue();
        let mut entries = vec![local_quit(), local_help()];
        entries.extend(catalogue.entries);
        (catalogue.revision, catalogue.complete, entries)
    }

    fn resolve_command(&self) -> Option<PresentedCommand> {
        if !self.usable() || self.overlay.is_some() || self.paste_mode {
            return None;
        }
        let header = self.editor.command_name()?;
        let (revision, complete, entries) = self.catalogue_entries();
        let matches: Vec<_> = entries
            .into_iter()
            .filter(|entry| {
                entry.name == header.name || entry.aliases.contains(&header.name.as_str())
            })
            .collect();
        if matches.len() != 1 {
            return None;
        }
        let definition = matches.into_iter().next()?;
        if definition.availability != CommandAvailability::Available
            || (definition.category != CommandCategory::BuiltIn && !complete)
        {
            return None;
        }
        Some(PresentedCommand {
            header,
            definition,
            project: self.project,
            target: self.fixture.target(),
            draft_revision: self.draft_revision,
            catalogue_revision: revision,
        })
    }

    fn refresh_command_suggestions(&mut self) {
        self.command_suggestions = None;
        if self.overlay.is_some() || self.paste_mode || !self.usable() {
            return;
        }
        let Some(header) = self.editor.command_header() else {
            if self.editor.command_name().is_none() {
                return;
            }
            self.command_suggestions = Some(CommandSuggestionView {
                rows: Vec::new(),
                cue: Some("Enter command · ^S/^T send text · F9 browse".into()),
                incomplete: false,
            });
            return;
        };
        let (_, complete, entries) = self.catalogue_entries();
        let mut matches = command_matches(&entries, &header.name);
        matches.truncate(6);
        let rows = matches.iter().map(command_row).collect();
        self.command_suggestions = Some(CommandSuggestionView {
            rows,
            cue: Some("Enter command · ^S/^T send text · Tab complete · F9 browse".into()),
            incomplete: !complete,
        });
    }

    pub fn composer_status(&self) -> ComposerStatus {
        ComposerStatus {
            project: self.fixture.project_status(),
            characters: self.editor.character_count(),
            spinner: self
                .animating()
                .then(|| SPINNER[(self.now_ms / 125 % SPINNER.len() as u64) as usize]),
        }
    }

    pub fn draft_limit_reached(&self) -> bool {
        self.draft_revision == u64::MAX
    }

    pub fn handle(&mut self, event: Event) {
        match event {
            Event::Resize(width, height) => {
                self.width = width;
                self.height = height;
            }
            Event::Paste(text) if self.overlay.is_none() => {
                if self.paste_mode {
                    self.capture(&text);
                } else {
                    self.insert(&text);
                }
            }
            Event::Key(key) if key.kind != KeyEventKind::Release => self.handle_key(key),
            _ => {}
        }
        self.refresh_command_suggestions();
    }

    fn handle_key(&mut self, key: KeyEvent) {
        let press = key.kind == KeyEventKind::Press;
        let ctrl = key.modifiers == KeyModifiers::CONTROL;
        if self.paste_mode && self.overlay.is_none() {
            self.capture_key(key);
            return;
        }
        // Explicit global commands may change a captured target, but never
        // replace its focused action implicitly. Paste capture remains isolated.
        if press && !self.paste_mode {
            if key.code == KeyCode::F(10)
                && key.modifiers.is_empty()
                && (self.overlay.is_none()
                    || matches!(self.overlay, Some(Overlay::CommandBrowser { .. })))
            {
                if !self.fixture.toggle_command_sources() {
                    self.action_error("Command source revision limit reached.".into());
                }
                return;
            }
            if ctrl {
                match key.code {
                    KeyCode::Char('q' | 'c') => {
                        if self.has_unsaved_text() {
                            self.open(Overlay::Exit { selected: false });
                        } else {
                            self.exit = true;
                        }
                        return;
                    }
                    KeyCode::Char('p') => {
                        self.switch_project();
                        return;
                    }
                    KeyCode::Char('x') if self.usable() => {
                        self.fixture.stop();
                        self.notice.clear();
                        return;
                    }
                    _ => {}
                }
            } else if key.modifiers.is_empty() {
                match key.code {
                    KeyCode::F(2) if self.usable() => {
                        self.fixture.set_connected(!self.fixture.connected);
                        self.notice.clear();
                        return;
                    }
                    KeyCode::F(3) if self.usable() => {
                        self.fixture.toggle_decision();
                        self.notice.clear();
                        return;
                    }
                    KeyCode::F(4) if self.usable() => {
                        self.fixture.reject_next();
                        self.notice.clear();
                        return;
                    }
                    KeyCode::F(5) if self.usable() => {
                        if self.fixture.pending.is_some() {
                            self.fixture.acknowledge_pending();
                        } else {
                            self.fixture.complete(self.now_ms);
                        }
                        self.notice.clear();
                        return;
                    }
                    KeyCode::F(6) if self.usable() => {
                        self.open(Overlay::Reset { selected: false });
                        return;
                    }
                    KeyCode::F(7) if self.usable() => {
                        self.fixture.lose_acknowledgement(self.now_ms);
                        self.notice.clear();
                        return;
                    }
                    KeyCode::F(8) if self.usable() => {
                        self.fixture.fail();
                        self.notice.clear();
                        return;
                    }
                    _ => {}
                }
            }
        }
        if let Some(overlay) = self.overlay.clone() {
            if press {
                self.overlay_key(key, overlay);
            }
            return;
        }
        if key.code == KeyCode::Enter {
            if !press {
                return;
            }
            if key.modifiers == KeyModifiers::ALT || key.modifiers == KeyModifiers::SHIFT {
                self.insert("\n");
            } else if key.modifiers.is_empty() {
                self.send();
            }
            return;
        }
        if ctrl {
            match key.code {
                KeyCode::Char('v') if press => {
                    self.presented_target = None;
                    self.paste_mode = true;
                    self.captured.clear();
                    self.capture_error = None;
                }
                KeyCode::Char('l') if press => self.open(Overlay::Clear { selected: false }),
                KeyCode::Char('o') if press => self.open_attention(),
                KeyCode::Char('s') if press => self.direct_submit(Action::Steer),
                KeyCode::Char('t') if press => self.direct_submit(Action::Queue),
                KeyCode::Char('b') if press => self.open_messages(),
                KeyCode::Char('s' | 't' | 'b') => {}
                KeyCode::Char('e') if press => {
                    self.viewport.follow_latest();
                    self.unread_output = false;
                    self.notice.clear();
                }
                _ => self.edit_key(key),
            }
        } else {
            match key.code {
                KeyCode::F(1) if press => self.open(Overlay::Help),
                KeyCode::F(9) if press && key.modifiers.is_empty() => self.open_command_browser(),
                KeyCode::Esc if press => {}
                KeyCode::PageUp => self.viewport.scroll_rows(-5),
                KeyCode::PageDown => self.viewport.scroll_rows(5),
                KeyCode::Tab if key.modifiers.is_empty() => {
                    if self.editor.command_header().is_some() {
                        self.complete_header();
                    } else {
                        self.insert("  ");
                    }
                }
                _ => self.edit_key(key),
            }
        }
    }

    fn usable(&self) -> bool {
        self.width >= 30 && self.height >= 8
    }

    fn has_unsaved_text(&self) -> bool {
        !self.editor.is_empty()
            || !self.other.editor.is_empty()
            || self.fixture.has_protected_text()
            || self.other.fixture.has_protected_text()
    }

    fn open(&mut self, overlay: Overlay) {
        self.presented_target = None;
        self.presented_command = None;
        self.overlay = Some(overlay);
        self.overlay_scroll = 0;
        self.notice.clear();
    }

    fn switch_project(&mut self) {
        self.presented_target = None;
        self.presented_command = None;
        self.overlay = None;
        self.overlay_scroll = 0;
        std::mem::swap(&mut self.editor, &mut self.other.editor);
        std::mem::swap(&mut self.fixture, &mut self.other.fixture);
        std::mem::swap(&mut self.viewport, &mut self.other.viewport);
        std::mem::swap(&mut self.draft_revision, &mut self.other.draft_revision);
        std::mem::swap(&mut self.notice, &mut self.other.notice);
        std::mem::swap(&mut self.seen_output_end, &mut self.other.seen_output_end);
        std::mem::swap(&mut self.unread_output, &mut self.other.unread_output);
        self.project = match self.project {
            Project::Studio => Project::Observatory,
            Project::Observatory => Project::Studio,
        };
    }

    fn send(&mut self) {
        if !self.usable() {
            self.notice = "Resize to at least 30 × 8 before sending.".into();
            return;
        }
        let text = self.editor.text();
        if text.trim().is_empty() {
            return;
        }
        if self.draft_limit_reached() {
            self.notice = "Draft revision limit reached. Reset the trial before sending.".into();
            return;
        }
        if self.editor.command_name().is_some() {
            self.send_command(text);
            return;
        }
        if self.fixture.task.is_some() && self.fixture.connected && self.fixture.pending.is_none() {
            self.open(Overlay::Submission {
                target: self.fixture.target(),
                text: text.into(),
                draft_revision: self.draft_revision,
                selected: None,
            });
        } else {
            self.submit(
                self.fixture.target(),
                Action::NewTurn,
                self.draft_revision,
                text,
            );
        }
    }

    fn open_command_browser(&mut self) {
        let target = self
            .editor
            .command_header()
            .or_else(|| self.editor.empty_command_target());
        let (catalogue_revision, _, entries) = self.catalogue_entries();
        self.open(Overlay::CommandBrowser {
            target,
            project: self.project,
            draft_revision: self.draft_revision,
            catalogue_revision,
            entries,
            selected: None,
        });
    }

    fn complete_header(&mut self) {
        let Some(header) = self.editor.command_header() else {
            return;
        };
        let (_, complete, entries) = self.catalogue_entries();
        if !complete {
            self.open_command_browser();
            self.notice = "Command catalogue incomplete; completion unavailable.".into();
            return;
        }
        let matches = command_matches(&entries, &header.name);
        let available: Vec<_> = matches
            .iter()
            .filter(|entry| entry.availability == CommandAvailability::Available)
            .collect();
        if available.len() == 1 && matches.len() == 1 {
            let completion = completion_name(available[0], &header.name);
            self.complete_name(&header, completion);
        } else if available.len() > 1 && available.len() == matches.len() {
            let names: Vec<_> = available
                .iter()
                .map(|entry| completion_name(entry, &header.name))
                .collect();
            let prefix = common_name_prefix(&names);
            if prefix.len() > header.name.len() {
                self.complete_name(&header, &prefix);
            } else {
                self.open_command_browser();
            }
        } else if matches.is_empty() {
            self.notice = "No matching command; draft retained. F9 browses commands.".into();
        } else {
            self.open_command_browser();
            self.notice = "Matching command unavailable; draft retained.".into();
        }
    }

    fn complete_name(&mut self, header: &CommandHeader, name: &str) {
        match self.editor.complete_command_name(header, name) {
            Ok(true) => {
                if name != header.name {
                    self.bump_revision();
                }
                self.presented_command = None;
                self.notice.clear();
            }
            Ok(false) => self.action_error("Completion target changed; draft retained.".into()),
            Err(error) => self.action_error(error.to_string()),
        }
    }

    fn send_command(&mut self, text: String) {
        let Some(current) = self.resolve_command() else {
            self.action_error(
                "Unknown, incomplete or unavailable command; draft retained. F9 browses commands."
                    .into(),
            );
            return;
        };
        if self.presented_command.as_ref() != Some(&current) {
            self.action_error(format!(
                "{} · {} · {}. Press Enter again to invoke.",
                current.definition.name,
                category_label(current.definition.category),
                current.definition.purpose
            ));
            return;
        }
        let raw_body = &text[current.header.byte_range.end..];
        let arguments = if raw_body.is_empty() {
            ""
        } else {
            &raw_body[1..]
        };
        if is_local_quit(&current.definition) {
            if !arguments.trim().is_empty() {
                self.action_error("/quit takes no arguments; draft retained.".into());
            } else if self.other.editor.is_empty()
                && !self.fixture.has_protected_text()
                && !self.other.fixture.has_protected_text()
            {
                self.exit = true;
            } else {
                self.open(Overlay::Exit { selected: false });
            }
            return;
        }
        if is_local_help(&current.definition) {
            if !arguments.trim().is_empty() {
                self.action_error("/help takes no arguments; draft retained.".into());
            } else {
                self.editor = Editor::new();
                self.bump_revision();
                self.open(Overlay::Help);
            }
            return;
        }
        if self.fixture.task.is_some() && self.fixture.connected && self.fixture.pending.is_none() {
            self.open(Overlay::CommandSubmission {
                presented: current,
                text: text.into(),
                selected: None,
            });
        } else {
            self.submit_command(current, Action::NewTurn, text);
        }
    }

    fn submit_command(&mut self, presented: PresentedCommand, action: Action, text: String) {
        if self.project != presented.project
            || self.draft_revision != presented.draft_revision
            || !self.fixture.target_valid(presented.target)
        {
            self.action_error("Command target changed; draft retained.".into());
            return;
        }
        if !presented.definition.supported_modes.contains(&action) {
            self.action_error(SubmitError::UnsupportedCommandMode.to_string());
            return;
        }
        let raw_body = &text[presented.header.byte_range.end..];
        let arguments = if raw_body.is_empty() {
            ""
        } else {
            &raw_body[1..]
        };
        let capture = CommandCapture {
            definition_id: presented.definition.id,
            source_id: presented.definition.source_id,
            source_revision: presented.definition.source_revision,
            definition_revision: presented.definition.definition_revision,
            catalogue_revision: presented.catalogue_revision,
            target: presented.target,
            draft_revision: presented.draft_revision,
            action,
            arguments: arguments.to_owned(),
        };
        match self
            .fixture
            .submit_command_request(capture, text, self.now_ms)
        {
            Ok(_) => {
                self.presented_target = None;
                self.presented_command = None;
                self.editor = Editor::new();
                self.bump_revision();
                self.notice.clear();
                self.overlay = None;
            }
            Err(error) => self.action_error(error.to_string()),
        }
    }

    fn direct_submit(&mut self, action: Action) {
        if !self.usable() {
            self.action_error("Resize to at least 30 × 8 before sending.".into());
            return;
        }
        let Some(target) = self.presented_target else {
            self.action_error(
                "No presented active task. Draft retained; review the status bar.".into(),
            );
            return;
        };
        if !self.fixture.target_valid(target) {
            self.action_error("Target changed; draft kept.".into());
            return;
        }
        if let Err(error) = self.fixture.can_submit(action) {
            self.action_error(error.to_string());
            return;
        }
        if self.draft_limit_reached() {
            self.action_error(
                "Draft revision limit reached. Reset the trial before sending.".into(),
            );
            return;
        }
        let text = self.editor.text();
        if text.trim().is_empty() {
            self.action_error(SubmitError::Empty.to_string());
            return;
        }
        self.submit(target, action, self.draft_revision, text);
    }

    fn submit(&mut self, target: Target, action: Action, revision: u64, text: String) {
        match self
            .fixture
            .submit_request(target, action, revision, text, self.now_ms)
        {
            Ok(_) => {
                self.presented_target = None;
                self.editor = Editor::new();
                self.bump_revision();
                self.notice.clear();
                self.overlay = None;
            }
            Err(error) => self.action_error(error.to_string()),
        }
    }

    fn overlay_key(&mut self, key: KeyEvent, overlay: Overlay) {
        if let Overlay::CommandBrowser {
            target,
            project,
            draft_revision,
            catalogue_revision,
            entries,
            selected,
        } = overlay
        {
            self.command_browser_key(
                key,
                target,
                project,
                draft_revision,
                catalogue_revision,
                entries,
                selected,
            );
            return;
        }
        match key.code {
            KeyCode::Esc if key.modifiers.is_empty() => {
                if self.paste_mode {
                    self.cancel_capture();
                } else {
                    self.overlay = None;
                }
            }
            KeyCode::F(1) if !self.paste_mode && key.modifiers.is_empty() => self.overlay = None,
            KeyCode::PageUp => self.overlay_scroll = self.overlay_scroll.saturating_sub(3),
            KeyCode::PageDown => self.overlay_scroll = self.overlay_scroll.saturating_add(3),
            KeyCode::Tab
            | KeyCode::Down
            | KeyCode::Right
            | KeyCode::BackTab
            | KeyCode::Up
            | KeyCode::Left
                if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT =>
            {
                let backwards = matches!(key.code, KeyCode::BackTab | KeyCode::Up | KeyCode::Left)
                    || key.modifiers == KeyModifiers::SHIFT;
                self.overlay = Some(select(overlay, backwards));
            }
            KeyCode::Enter if key.modifiers.is_empty() => {
                if matches!(overlay, Overlay::Exit { selected: true }) {
                    self.exit = true;
                    return;
                }
                // Cancellation is safe in tiny geometry; activating any work,
                // decision, replacement or discard action is disabled.
                if matches!(
                    overlay,
                    Overlay::PasteFinish {
                        selected: Some(false)
                    }
                ) {
                    self.cancel_capture();
                    return;
                }
                if !self.usable() {
                    return;
                }
                match overlay {
                    Overlay::Clear { selected: true } => {
                        self.editor = Editor::new();
                        self.bump_revision();
                        self.overlay = None;
                    }
                    Overlay::Reset { selected: true } => self.reset(),
                    Overlay::PasteFinish {
                        selected: Some(true),
                    } if self.capture_error.is_none() => match self.editor.insert(&self.captured) {
                        Ok(()) => {
                            self.bump_revision();
                            self.cancel_capture();
                        }
                        Err(error) => self.capture_error = Some(error.to_string()),
                    },
                    Overlay::Submission {
                        target,
                        text,
                        draft_revision,
                        selected: Some(index),
                    } if self.fixture.target_valid(target) => {
                        let action = if index == 0 {
                            Action::Steer
                        } else {
                            Action::Queue
                        };
                        self.submit(target, action, draft_revision, text.to_string());
                    }
                    Overlay::CommandSubmission {
                        presented,
                        text,
                        selected: Some(index),
                    } => {
                        let action = if index == 0 {
                            Action::Steer
                        } else {
                            Action::Queue
                        };
                        self.submit_command(presented, action, text.to_string());
                    }
                    Overlay::Decision {
                        target,
                        id,
                        selected: Some(index),
                        ..
                    } => {
                        let action = if index == 0 {
                            DecisionAction::Continue
                        } else {
                            DecisionAction::Stop
                        };
                        match self.fixture.respond_decision(target, id, action) {
                            Ok(()) => {
                                self.overlay = None;
                                self.notice.clear();
                            }
                            Err(error) => self.action_error(error.to_string()),
                        }
                    }
                    Overlay::Recovery {
                        id,
                        selected: Some(index),
                    } if !self.fixture.delivery_blocked() => {
                        if index == 0 {
                            self.restore(id);
                        } else if self.fixture.discard_recovered(id) {
                            self.overlay = None;
                            self.notice.clear();
                        }
                    }
                    Overlay::Attention {
                        items,
                        selected: Some(index),
                    } => {
                        if let Some(item) = items.get(index) {
                            self.open_item(item.clone());
                        }
                    }
                    Overlay::MessageList {
                        items,
                        selected: Some(index),
                    } => {
                        if let Some(id) = items.get(index) {
                            self.open_message(*id);
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn command_browser_key(
        &mut self,
        key: KeyEvent,
        target: Option<CommandHeader>,
        project: Project,
        draft_revision: u64,
        catalogue_revision: u64,
        entries: Vec<CommandDefinition>,
        selected: Option<usize>,
    ) {
        if key.modifiers.is_empty() && matches!(key.code, KeyCode::Esc | KeyCode::F(1)) {
            self.overlay = None;
            return;
        }
        if key.code == KeyCode::PageUp {
            self.overlay_scroll = self.overlay_scroll.saturating_sub(3);
            return;
        }
        if key.code == KeyCode::PageDown {
            self.overlay_scroll = self.overlay_scroll.saturating_add(3);
            return;
        }
        if key.modifiers.is_empty()
            && matches!(
                key.code,
                KeyCode::Tab | KeyCode::Down | KeyCode::Up | KeyCode::BackTab
            )
        {
            let next = match (selected, key.code) {
                (None, KeyCode::Up | KeyCode::BackTab) => entries.len().checked_sub(1),
                (None, _) => (!entries.is_empty()).then_some(0),
                (Some(index), KeyCode::Down) => Some((index + 1) % entries.len()),
                (Some(index), KeyCode::Up | KeyCode::BackTab) => {
                    Some((index + entries.len() - 1) % entries.len())
                }
                (Some(_), KeyCode::Tab) => {
                    self.complete_browser_selection(
                        &target,
                        project,
                        draft_revision,
                        catalogue_revision,
                        &entries,
                        selected,
                    );
                    return;
                }
                _ => selected,
            };
            self.overlay = Some(Overlay::CommandBrowser {
                target,
                project,
                draft_revision,
                catalogue_revision,
                entries,
                selected: next,
            });
            return;
        }
        if key.modifiers.is_empty() && key.code == KeyCode::Enter && selected.is_some() {
            self.complete_browser_selection(
                &target,
                project,
                draft_revision,
                catalogue_revision,
                &entries,
                selected,
            );
        }
    }

    fn complete_browser_selection(
        &mut self,
        target: &Option<CommandHeader>,
        project: Project,
        draft_revision: u64,
        catalogue_revision: u64,
        entries: &[CommandDefinition],
        selected: Option<usize>,
    ) {
        let Some(entry) = selected.and_then(|index| entries.get(index)) else {
            return;
        };
        let Some(header) = target else {
            self.action_error(
                "Read-only browser: move to a command name or clear the draft first.".into(),
            );
            return;
        };
        if project != self.project || draft_revision != self.draft_revision {
            self.action_error("Browser target changed; draft retained.".into());
            return;
        }
        let (current_revision, complete, current_entries) = self.catalogue_entries();
        let current = current_entries
            .iter()
            .find(|candidate| candidate.id == entry.id);
        if entry.availability != CommandAvailability::Available
            || (entry.category != CommandCategory::BuiltIn
                && (!complete || current_revision != catalogue_revision))
            || current != Some(entry)
        {
            self.action_error("Command source changed or is unavailable; draft retained.".into());
            return;
        }
        let name = completion_name(entry, &header.name);
        match self.editor.complete_command_name(header, name) {
            Ok(true) => {
                self.bump_revision();
                self.presented_command = None;
                self.overlay = None;
                self.notice.clear();
            }
            Ok(false) => self.action_error("Completion target changed; draft retained.".into()),
            Err(error) => self.action_error(error.to_string()),
        }
    }

    fn reset(&mut self) {
        self.presented_target = None;
        // Models own generation changes. This is one explicit user confirmation
        // for discarding both contexts' in-memory state, including hidden drafts.
        let result = self
            .fixture
            .can_reset()
            .and_then(|()| self.other.fixture.can_reset())
            .and_then(|()| self.fixture.reset())
            .and_then(|()| self.other.fixture.reset());
        match result {
            Ok(()) => {
                self.editor = Editor::new();
                self.other.editor = Editor::new();
                self.viewport = TranscriptViewport::new();
                self.other.viewport = TranscriptViewport::new();
                self.draft_revision = 0;
                self.other.draft_revision = 0;
                self.seen_output_end = 0;
                self.other.seen_output_end = 0;
                self.unread_output = false;
                self.other.unread_output = false;
                self.notice.clear();
                self.other.notice.clear();
                self.overlay = None;
            }
            Err(error) => self.action_error(error.to_string()),
        }
    }

    pub fn attention(&self) -> Vec<Attention> {
        let mut items = Vec::new();
        if !self.fixture.connected || self.fixture.pending.is_some() {
            items.push(Attention::Connection {
                target: self.fixture.target(),
            });
        }
        if let Some(recoverable) = self.fixture.display_recoverable() {
            items.extend(recoverable.iter().map(|r| Attention::Recovery { id: r.id }));
        }
        if let Some(decision) = self
            .fixture
            .display_task()
            .and_then(|task| task.decision.as_ref())
        {
            items.push(Attention::Decision {
                target: decision.target,
                id: decision.id,
            });
        }
        items
    }

    fn open_attention(&mut self) {
        let items = self.attention();
        match items.as_slice() {
            [] => self.notice = "No attention items in this project.".into(),
            [item] => self.open_item(item.clone()),
            _ => self.open(Overlay::Attention {
                items,
                selected: None,
            }),
        }
    }

    fn open_messages(&mut self) {
        let tray = self.fixture.message_tray();
        match tray.items.as_slice() {
            [] => self.notice = "No retained messages in this project.".into(),
            [item] => self.open_message(item.id),
            items => self.open(Overlay::MessageList {
                items: items.iter().map(|item| item.id).collect(),
                selected: None,
            }),
        }
    }

    fn open_message(&mut self, id: RequestId) {
        if let Some(item) = self
            .fixture
            .message_tray()
            .items
            .into_iter()
            .find(|item| item.id == id)
        {
            self.open(Overlay::MessageInspector {
                id,
                text: item.text,
                action: item.action,
            });
        } else {
            self.action_error("This retained message has expired. Esc closes the list.".into());
        }
    }

    fn open_item(&mut self, item: Attention) {
        match item {
            Attention::Connection { target } => self.open(Overlay::Connection {
                target,
                pending_id: self.fixture.pending.as_ref().map(|pending| pending.id),
            }),
            Attention::Recovery { id } => self.open(Overlay::Recovery { id, selected: None }),
            Attention::Decision { target, id } => {
                let prompt = self
                    .fixture
                    .display_task()
                    .and_then(|task| task.decision.as_ref())
                    .filter(|decision| decision.id == id && decision.target == target)
                    .map_or_else(
                        || Arc::from("This decision has expired."),
                        |decision| Arc::from(decision.prompt.as_str()),
                    );
                self.open(Overlay::Decision {
                    target,
                    id,
                    prompt,
                    selected: None,
                });
            }
        }
    }

    fn restore(&mut self, id: RequestId) {
        let Some(retained) = self.visible_recovery(id) else {
            return;
        };
        // Reuse the editor for validation/normalization before moving the real
        // editor cursor. Even malformed test fixture text cannot damage a draft.
        let mut validated = Editor::new();
        if let Err(error) = validated.insert(&retained.text) {
            self.action_error(error.to_string());
            return;
        }
        let append = !self.editor.is_empty();
        let text = format!("{}{}", if append { "\n" } else { "" }, validated.text());
        if self.editor.text().len().saturating_add(text.len()) > MAX_DRAFT_BYTES {
            self.action_error(
                "Recovered text would exceed 64 KiB. Both versions are retained.".into(),
            );
            return;
        }
        if append
            && let Err(error) = self
                .editor
                .key(KeyEvent::new(KeyCode::End, KeyModifiers::CONTROL))
        {
            self.action_error(error.to_string());
            return;
        }
        match self.editor.insert(&text) {
            Ok(()) => {
                self.bump_revision();
                self.fixture.discard_recovered(id);
                self.overlay = None;
                self.notice = "Text restored to the draft. Review it before sending.".into();
            }
            Err(error) => self.action_error(error.to_string()),
        }
    }

    pub fn attention_label(&self, item: &Attention) -> String {
        match item {
            Attention::Connection { .. } => "Connection / pending".into(),
            Attention::Recovery { id } => self.visible_recovery(*id).map_or_else(
                || "Text unavailable".into(),
                |request| {
                    let excerpt: String = request
                        .text
                        .chars()
                        .take(32)
                        .map(|c| if c.is_whitespace() { ' ' } else { c })
                        .collect();
                    format!("Text {}: {excerpt}", id.sequence)
                },
            ),
            Attention::Decision { target, id } => {
                if self.decision_valid(*target, *id) {
                    "Review decision".into()
                } else {
                    "Decision expired".into()
                }
            }
        }
    }

    fn decision_valid(&self, target: Target, id: u64) -> bool {
        self.fixture.target_valid(target)
            && self
                .fixture
                .display_task()
                .and_then(|task| task.decision.as_ref())
                .is_some_and(|decision| decision.id == id && decision.target == target)
    }

    pub fn overlay_view(&self) -> Option<OverlayView> {
        let overlay = self.overlay.as_ref()?;
        let project = self.project.name();
        let choice = |title: &str, actions: Vec<String>, selected, detail: String| OverlayView {
            title: format!("{project} · {title}"),
            actions,
            selected,
            detail,
        };
        Some(match overlay {
            Overlay::Help => choice("Controls", vec![], None, HELP.into()),
            Overlay::Exit { selected } => choice("Unsaved text", vec!["Exit and discard".into()], selected.then_some(0),
                "Exit discards both projects' drafts, pending and held text.\nTab selects · Enter confirms · Esc keeps editing".into()),
            Overlay::Clear { selected } => choice("Clear draft", vec!["Clear draft".into()], selected.then_some(0),
                "Clear this draft and its undo history?\nTab selects · Enter confirms · Esc keeps editing".into()),
            Overlay::Reset { selected } => choice("Reset both projects", vec!["Reset and discard".into()], selected.then_some(0),
                "Reset both fixture generations and discard all drafts, pending, queued and held text.\nTab selects · Enter confirms · Esc cancels".into()),
            Overlay::PasteFinish { selected } => choice("Finish paste", vec![
                format!("Insert{}", if self.capture_error.is_some() { " (unavailable)" } else { "" }), "Cancel capture".into()],
                selected.map(|insert| usize::from(!insert)),
                format!("{} bytes captured. Draft unchanged.\n{}\nTab selects · Enter confirms · Esc cancels", self.captured.len(), self.capture_error.as_deref().unwrap_or("Insert captured text as one edit."))),
            Overlay::Submission { target, selected, text, .. } => {
                let valid = self.fixture.target_valid(*target);
                let suffix = if valid { "" } else { " (unavailable)" };
                choice("Steer or queue", vec![format!("Steer{suffix}"), format!("Queue{suffix}")], *selected,
                    format!("{project} / Conversation 1 · Task {}\n{}\nSteer changes this task. Queue follows its success.\nCaptured message:\n{text}\n\nTab/arrows select · Enter confirms · Esc cancels", target.task.map_or(0, |task| task.id),
                        if valid { "No action is selected by default." } else { "Target changed or ended. Dismiss and send again." }))
            }
            Overlay::CommandSubmission { presented, selected, text } => {
                let valid = self.project == presented.project
                    && self.draft_revision == presented.draft_revision
                    && self.fixture.target_valid(presented.target);
                let actions = [Action::Steer, Action::Queue].into_iter().map(|action| {
                    let enabled = valid && presented.definition.supported_modes.contains(&action);
                    format!("{}{}", action_label(action), if enabled { "" } else { " (unavailable)" })
                }).collect();
                choice("Command: Steer or queue", actions, *selected,
                    format!("{} · {} · {}\nCaptured command:\n{text}\n\nNo action selected by default. Tab/arrows select · Enter confirms · Esc cancels",
                        presented.definition.name, category_label(presented.definition.category), presented.definition.source_id))
            }
            Overlay::CommandBrowser { target, entries, selected, catalogue_revision, .. } => {
                let (revision, complete, current) = self.catalogue_entries();
                let actions = entries.iter().map(|entry| {
                    let stale = entry.category != CommandCategory::BuiltIn
                        && (!complete || revision != *catalogue_revision || current.iter().find(|candidate| candidate.id == entry.id) != Some(entry));
                    format!("{}{}", command_row(entry), if stale { " · changed" } else { "" })
                }).collect();
                let detail = selected.and_then(|index| entries.get(index)).map_or_else(
                    || format!("{}\nTab/arrows select · Tab/Enter complete · Esc closes", if target.is_some() { "Select a command. Completion does not invoke it." } else { "Read only: clear draft or move into its command name to insert." }),
                    |entry| {
                        let stale = entry.category != CommandCategory::BuiltIn
                            && (!complete || revision != *catalogue_revision || current.iter().find(|candidate| candidate.id == entry.id) != Some(entry));
                        format!("{} · {}\n{} · {} · {} · source rev {} · definition rev {}\n{} / Conversation 1 · {}\n{}",
                            entry.name, category_label(entry.category), entry.source_id,
                            if stale { "Changed (unavailable)" } else { availability_label(entry.availability) },
                            origin_label(entry.origin), entry.source_revision, entry.definition_revision, project, entry.purpose,
                            if stale { "Source changed; dismiss and browse again." } else { "Tab/Enter completes; a later Enter invokes." })
                    });
                choice("Commands", actions, *selected, detail)
            }
            Overlay::Decision { target, id, prompt, selected } => {
                let valid = self.decision_valid(*target, *id);
                let suffix = if valid { "" } else { " (unavailable)" };
                choice("Decision", vec![format!("Continue{suffix}"), format!("Stop{suffix}")], *selected,
                    format!("{project} / Conversation 1\n{}\n{prompt}\n\nTab/arrows select · Enter confirms · Esc cancels", if valid { "Answer this scoped fixture decision." } else { "Decision expired or target unavailable. Esc dismisses." }))
            }
            Overlay::Recovery { id, selected } => {
                let retained = self.visible_recovery(*id);
                let action = if self.editor.is_empty() { "Restore to draft" } else { "Append to draft" };
                let suffix = if retained.is_some() { "" } else { " (unavailable)" };
                choice("Recover text", vec![format!("{action}{suffix}"), format!("Discard text{suffix}")], *selected,
                    retained.map_or_else(|| "Retained text is unavailable. Esc dismisses.".into(), |r| format!("{project} / Conversation 1\n{}\n{}\n\n{}\nTab selects · Enter confirms · Esc keeps both", r.reason,
                        if self.editor.is_empty() { "Restore does not send." } else { "Append adds a newline and keeps your newer text." }, r.text)))
            }
            Overlay::Attention { items, selected } => choice("Attention", items.iter().map(|item| self.attention_label(item)).collect(), *selected,
                "Scoped items retain their identity while this list is open.\nTab/arrows select · Enter opens · Esc closes".into()),
            Overlay::Connection { pending_id, .. } => {
                let pending = self.fixture.pending.as_ref().filter(|p| Some(p.id) == *pending_id);
                let state = if pending.is_some_and(|p| p.unknown) { "Outcome unknown until reconciliation." }
                    else if pending.is_some() { "Awaiting acknowledgement. Draft remains editable." }
                    else if pending_id.is_some() { "Captured request resolved. Esc closes this view." }
                    else { "No pending request." };
                let text = pending.map_or_else(String::new, |p| format!("\n\nPending message:\n{}", p.text));
                choice("Connection / pending", vec![], None,
                    format!("{project} / Conversation 1\n{}\n{state}\nF2 reconnects without sending another copy. F5 acknowledges in manual mode.\nEsc returns to editing.{text}",
                        if self.fixture.connected { "Connected." } else { "Disconnected. Sending unavailable; draft retained." }))
            },
            Overlay::MessageList { items, selected } => {
                let tray = self.fixture.message_tray();
                let actions = items.iter().map(|id| {
                    tray.items.iter().find(|item| item.id == *id).map_or_else(
                        || format!("Message {} · expired", id.sequence),
                        |item| format!("{} · {} · {}{}", id.sequence, action_label(item.action), message_state_label(item.state), if item.stale { " · stale" } else { "" }))
                }).collect();
                choice("Message tray", actions, *selected,
                    format!("{}Tab/arrows select · Enter reads full text · Esc closes\nNo message is selected by default. Recovery uses Ctrl+O.",
                        if tray.history_trimmed { "Older history trimmed.\n" } else { "" }))
            }
            Overlay::MessageInspector { id, text, action } => {
                let tray = self.fixture.message_tray();
                let state = tray.items.iter().find(|item| item.id == *id).map_or_else(
                    || "Expired from retained history; captured text remains readable.".into(),
                    |item| format!("{}{}{}", message_state_label(item.state), if item.stale { " · stale; current state unavailable" } else { "" }, item.reason.as_ref().map_or_else(String::new, |reason| format!(" · {reason}"))));
                choice(&format!("Message {} · {}", id.sequence, action_label(*action)), vec![], None,
                    format!("{state}\nRead only · PgUp/PgDn scroll · Esc closes\n\n{text}"))
            }
        })
    }

    fn bump_revision(&mut self) {
        self.draft_revision = self.draft_revision.saturating_add(1);
    }

    fn visible_recovery(&self, id: RequestId) -> Option<&RetainedRequest> {
        self.fixture
            .display_recoverable()?
            .iter()
            .find(|request| request.id == id)
    }

    fn action_error(&mut self, message: String) {
        self.notice = message;
        self.overlay_scroll = 0;
    }

    fn insert(&mut self, text: &str) {
        match self.editor.insert(text) {
            Ok(()) => {
                self.bump_revision();
                self.notice.clear();
            }
            Err(error) => self.action_error(error.to_string()),
        }
    }

    fn capture(&mut self, text: &str) {
        if self.capture_error.is_some() {
            return;
        }
        if self.captured.len().saturating_add(text.len()) > 3 * MAX_DRAFT_BYTES {
            self.capture_error =
                Some("Captured paste is too large; the draft is unchanged.".into());
        } else {
            self.captured.push_str(text);
        }
    }

    fn capture_key(&mut self, key: KeyEvent) {
        if key.code == KeyCode::Char('v') && key.modifiers == KeyModifiers::CONTROL {
            if key.kind == KeyEventKind::Press {
                self.open(Overlay::PasteFinish { selected: None });
            }
            return;
        }
        match key.code {
            KeyCode::Enter if key.modifiers.is_empty() => self.capture("\n"),
            KeyCode::Char('j') if key.modifiers == KeyModifiers::CONTROL => self.capture("\n"),
            KeyCode::Tab if key.modifiers.is_empty() => self.capture("\t"),
            KeyCode::Char(c)
                if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT =>
            {
                self.capture(c.encode_utf8(&mut [0; 4]))
            }
            _ => {
                self.capture_error = Some(
                    "Control input rejected. Ctrl+V then Esc cancels capture; draft unchanged."
                        .into(),
                )
            }
        }
    }

    fn cancel_capture(&mut self) {
        self.captured.clear();
        self.capture_error = None;
        self.paste_mode = false;
        self.overlay = None;
    }

    fn edit_key(&mut self, key: KeyEvent) {
        let before = self.editor.text();
        match self.editor.key(key) {
            Ok(_) if self.editor.text() != before => {
                self.bump_revision();
                self.notice.clear();
            }
            Ok(_) => {}
            Err(error) => self.action_error(error.to_string()),
        }
    }
}

fn select(overlay: Overlay, backwards: bool) -> Overlay {
    let next = |selected: Option<usize>, count: usize| {
        if count == 0 {
            return None;
        }
        Some(match selected {
            None if backwards => count - 1,
            None => 0,
            Some(index) if backwards => (index + count - 1) % count,
            Some(index) => (index + 1) % count,
        })
    };
    match overlay {
        Overlay::Exit { selected } => Overlay::Exit {
            selected: !selected,
        },
        Overlay::Clear { selected } => Overlay::Clear {
            selected: !selected,
        },
        Overlay::Reset { selected } => Overlay::Reset {
            selected: !selected,
        },
        Overlay::PasteFinish { selected } => Overlay::PasteFinish {
            selected: next(selected.map(|v| usize::from(!v)), 2).map(|i| i == 0),
        },
        Overlay::Submission {
            target,
            text,
            draft_revision,
            selected,
        } => Overlay::Submission {
            target,
            text,
            draft_revision,
            selected: next(selected, 2),
        },
        Overlay::CommandSubmission {
            presented,
            text,
            selected,
        } => Overlay::CommandSubmission {
            presented,
            text,
            selected: next(selected, 2),
        },
        Overlay::Decision {
            target,
            id,
            prompt,
            selected,
        } => Overlay::Decision {
            target,
            id,
            prompt,
            selected: next(selected, 2),
        },
        Overlay::Recovery { id, selected } => Overlay::Recovery {
            id,
            selected: next(selected, 2),
        },
        Overlay::Attention { items, selected } => {
            let selected = next(selected, items.len());
            Overlay::Attention { items, selected }
        }
        Overlay::MessageList { items, selected } => {
            let selected = next(selected, items.len());
            Overlay::MessageList { items, selected }
        }
        other => other,
    }
}

fn local_quit() -> CommandDefinition {
    CommandDefinition {
        id: "builtin:quit",
        name: "/quit",
        aliases: &["/exit"],
        label: "Quit",
        purpose: "Exit this client session",
        category: CommandCategory::BuiltIn,
        operation: CommandOperation::ClientSession,
        origin: CommandOrigin::Asura,
        source_id: "asura",
        source_revision: 1,
        definition_revision: 1,
        availability: CommandAvailability::Available,
        supported_modes: &[],
    }
}

fn is_local_quit(definition: &CommandDefinition) -> bool {
    definition == &local_quit()
}

fn local_help() -> CommandDefinition {
    CommandDefinition {
        id: "builtin:help",
        name: "/help",
        aliases: &[],
        label: "Help",
        purpose: "Show controls in this client",
        category: CommandCategory::BuiltIn,
        operation: CommandOperation::ClientView,
        origin: CommandOrigin::Asura,
        source_id: "asura",
        source_revision: 1,
        definition_revision: 1,
        availability: CommandAvailability::Available,
        supported_modes: &[],
    }
}

fn is_local_help(definition: &CommandDefinition) -> bool {
    definition == &local_help()
}

fn category_label(category: CommandCategory) -> &'static str {
    match category {
        CommandCategory::BuiltIn => "Built-in",
        CommandCategory::Extension => "Extension",
        CommandCategory::Skill => "Skill",
    }
}

fn origin_label(origin: CommandOrigin) -> &'static str {
    match origin {
        CommandOrigin::Asura => "Asura",
        CommandOrigin::Integration => "Integration",
        CommandOrigin::ProjectSkill => "Project Skill",
    }
}

fn availability_label(availability: CommandAvailability) -> &'static str {
    match availability {
        CommandAvailability::Available => "Available",
        CommandAvailability::Unavailable => "Unavailable",
        CommandAvailability::Stale => "Stale",
        CommandAvailability::Revoked => "Revoked",
    }
}

fn command_row(entry: &CommandDefinition) -> String {
    format!(
        "{} · {}{} · {} · {}",
        category_label(entry.category),
        entry.name,
        if entry.aliases.is_empty() {
            String::new()
        } else {
            format!(" ({})", entry.aliases.join(", "))
        },
        availability_label(entry.availability),
        origin_label(entry.origin)
    )
}

fn completion_name<'a>(entry: &'a CommandDefinition, typed: &str) -> &'a str {
    if typed.is_empty() {
        return entry.name;
    }
    entry
        .aliases
        .iter()
        .copied()
        .find(|alias| alias.starts_with(typed))
        .unwrap_or(entry.name)
}

fn command_matches(entries: &[CommandDefinition], query: &str) -> Vec<CommandDefinition> {
    let mut matches: Vec<_> = entries
        .iter()
        .filter(|entry| {
            entry.name.starts_with(query)
                || entry.aliases.iter().any(|alias| alias.starts_with(query))
                || (query.len() > 1
                    && entry
                        .label
                        .to_ascii_lowercase()
                        .contains(&query[1..].to_ascii_lowercase()))
        })
        .cloned()
        .collect();
    // An unqualified built-in alias takes precedence over qualified source
    // prefixes: /ex completes /exit rather than competing with /ext:… .
    if query.len() > 1 && !query.contains(':') {
        let builtins: Vec<_> = matches
            .iter()
            .filter(|entry| {
                entry.category == CommandCategory::BuiltIn
                    && (entry.name.starts_with(query)
                        || entry.aliases.iter().any(|alias| alias.starts_with(query)))
            })
            .cloned()
            .collect();
        if !builtins.is_empty() {
            return builtins;
        }
    }
    matches.sort_by(|a, b| {
        let rank = |entry: &CommandDefinition| {
            if entry.name == query || entry.aliases.contains(&query) {
                0
            } else if entry.name.starts_with(query)
                || entry.aliases.iter().any(|alias| alias.starts_with(query))
            {
                1
            } else {
                2
            }
        };
        rank(a).cmp(&rank(b)).then_with(|| a.name.cmp(b.name))
    });
    matches
}

fn common_name_prefix(names: &[&str]) -> String {
    let Some(first) = names.first() else {
        return String::new();
    };
    let mut prefix = (*first).to_owned();
    for name in &names[1..] {
        while !name.starts_with(&prefix) {
            prefix.pop();
        }
    }
    prefix
}

pub fn action_label(action: Action) -> &'static str {
    match action {
        Action::NewTurn => "Message",
        Action::Steer => "Steer",
        Action::Queue => "Queue",
    }
}

pub fn message_state_label(state: MessageState) -> &'static str {
    match state {
        MessageState::Pending => "Pending",
        MessageState::Unknown => "Unknown",
        MessageState::Acknowledged => "Acknowledged",
        MessageState::Queued => "Queued",
        MessageState::Running => "Running",
        MessageState::Held => "Held",
        MessageState::Completed => "Completed",
        MessageState::Stopped => "Stopped",
        MessageState::Failed => "Failed",
    }
}

const HELP: &str = "Enter sends when idle; during work choose Steer or Queue.\nCtrl+S steers; Ctrl+T queues the draft for the presented task.\nCtrl+B inspects retained messages without changing them.\nOption+Return adds a newline.\nArrows / Home / End move; Shift extends selection.\nCtrl+Home / Ctrl+End move to document start/end.\nCtrl+A selects all; Ctrl+Z undo; Ctrl+Y redo.\nTab completes a slash-command name; otherwise inserts two spaces.\nF9 browses Built-ins, Extensions and Skills; Tab/Enter complete a selected result.\nEnter invokes a presented command; Ctrl+S/T send literal text.\nF10 revokes/restores synthetic command sources in this project.\nCtrl+V captures paste, then Ctrl+V reviews Insert/Cancel.\nPageUp / PageDown scroll; Ctrl+E follows latest output.\nCtrl+P switches Studio / Observatory and retains each draft.\nCtrl+O opens this project's scoped attention items.\nCtrl+X stops work and holds queued follow-ups.\nCtrl+L confirms clear draft; Ctrl+Q / Ctrl+C requests exit.\nF1 or /help opens controls; F1/Escape closes the view.\nF2 toggles connection; reconnect reconciles pending text.\nF3 raises/expires a synthetic decision during work.\nF4 toggles rejection of the next submission.\nF5 resolves pending acceptance, otherwise completes work.\nF6 confirms reset of both projects and all in-memory text.\nF7 accepts pending text then loses its acknowledgement.\nF8 fails active work and holds queued text.\n\nOverlays: Tab/arrows select, Enter activates only on press.\nThere is no default action. Expired actions remain disabled.\nPageUp / PageDown scroll details; selected actions stay visible.\nUse native selection and Command+C to copy output.\nBracketed paste inserts text; it never sends.\nFallback capture stages plain text as one edit.\nDuring capture: Ctrl+V then Esc cancels. Other control input invalidates capture.\nUnbracketed control input has no reliable origin boundary.\n\nProject paths, Git counts, model identity and context usage are synthetic.\nThe footer counts grapheme characters; spaces and newlines count.\nNarrow layouts shorten metadata; F marks fast mode.\nAll work, decisions and connections are synthetic fixtures.\nNo project content, models, network or tools are accessed.\n--manual disables automatic ticks; F5 steps work explicitly.";

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyEvent;
    fn key(code: KeyCode, modifiers: KeyModifiers) -> Event {
        Event::Key(KeyEvent::new(code, modifiers))
    }

    #[test]
    fn ct1_empty_browser_completes_canonical_quit_without_invoking() {
        let mut app = App::new(true);
        app.handle(key(KeyCode::F(9), KeyModifiers::NONE));
        assert!(matches!(
            app.overlay,
            Some(Overlay::CommandBrowser { selected: None, .. })
        ));
        app.handle(key(KeyCode::Tab, KeyModifiers::NONE));
        assert!(matches!(
            app.overlay,
            Some(Overlay::CommandBrowser {
                selected: Some(0),
                ..
            })
        ));
        app.handle(key(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(app.editor.text(), "/quit ");
        assert!(app.overlay.is_none());
        assert!(!app.exit);
        app.handle(key(KeyCode::Char('z'), KeyModifiers::CONTROL));
        assert!(app.editor.is_empty());
    }

    #[test]
    fn ct1b_alias_tab_completion_preserves_name_and_does_not_invoke() {
        let mut app = App::new(true);
        app.handle(Event::Paste("/ex".into()));
        app.handle(key(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(app.editor.text(), "/exit");
        assert!(!app.exit);
        app.handle(key(KeyCode::Char('z'), KeyModifiers::CONTROL));
        assert_eq!(app.editor.text(), "/ex");
        app.handle(key(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(app.editor.text(), "/exit");
    }

    #[test]
    fn ct1d_quote_prefix_disables_command_dispatch_and_keeps_exact_text() {
        let mut app = App::new(true);
        app.handle(Event::Paste("‘/tmp/file’".into()));
        assert!(app.command_suggestions().is_none());
        app.handle(key(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(
            app.fixture.pending.as_ref().unwrap().text.as_str(),
            "‘/tmp/file’"
        );
        assert!(!app.exit);
    }

    #[test]
    fn ct1f_quit_alias_uses_protected_exit_without_default() {
        let mut app = App::new(true);
        app.other.editor.insert("protected other draft").unwrap();
        app.handle(Event::Paste("/exit".into()));
        app.record_presentation();
        app.handle(key(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(
            app.overlay,
            Some(Overlay::Exit { selected: false })
        ));
        app.handle(key(KeyCode::Enter, KeyModifiers::NONE));
        assert!(!app.exit);
        app.handle(key(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(app.editor.text(), "/exit");
        assert_eq!(app.other.editor.text(), "protected other draft");
    }

    #[test]
    fn ct5_source_change_rejects_browser_completion_and_keeps_draft() {
        let mut app = App::new(true);
        app.handle(Event::Paste("/ext:checks/test".into()));
        app.handle(key(KeyCode::F(9), KeyModifiers::NONE));
        app.handle(key(KeyCode::Down, KeyModifiers::NONE));
        app.handle(key(KeyCode::Down, KeyModifiers::NONE));
        app.handle(key(KeyCode::Down, KeyModifiers::NONE));
        app.handle(key(KeyCode::F(10), KeyModifiers::NONE));
        assert!(
            app.overlay_view()
                .unwrap()
                .detail
                .contains("Changed (unavailable)")
        );
        app.handle(key(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(app.editor.text(), "/ext:checks/test");
        assert!(matches!(app.overlay, Some(Overlay::CommandBrowser { .. })));
        assert!(app.notice.contains("changed") || app.notice.contains("unavailable"));
    }

    #[test]
    fn ct4_skill_active_requires_explicit_queue_and_rejects_steer() {
        let mut app = working();
        app.handle(Event::Paste("/skill:project/review review this".into()));
        app.record_presentation();
        app.handle(key(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(
            app.overlay,
            Some(Overlay::CommandSubmission { selected: None, .. })
        ));
        app.handle(key(KeyCode::Enter, KeyModifiers::NONE));
        assert!(app.fixture.pending.is_none());
        app.handle(key(KeyCode::Tab, KeyModifiers::NONE));
        app.handle(key(KeyCode::Enter, KeyModifiers::NONE));
        assert!(app.fixture.pending.is_none());
        assert_eq!(app.editor.text(), "/skill:project/review review this");
        assert!(app.notice.contains("does not support"));
        app.handle(key(KeyCode::Tab, KeyModifiers::NONE));
        app.handle(key(KeyCode::Enter, KeyModifiers::NONE));
        assert!(app.fixture.pending.is_some());
        assert_eq!(
            app.fixture
                .pending
                .as_ref()
                .unwrap()
                .command
                .as_ref()
                .unwrap()
                .action,
            Action::Queue
        );
    }

    #[test]
    fn ct5_source_revoke_before_acceptance_retains_original_capture() {
        let mut app = App::new(true);
        app.handle(Event::Paste("/ext:checks/test body".into()));
        app.record_presentation();
        app.handle(key(KeyCode::Enter, KeyModifiers::NONE));
        let pending = app.fixture.pending.as_ref().unwrap();
        let capture = pending.command.clone().unwrap();
        assert_eq!(capture.arguments, "body");
        app.handle(key(KeyCode::F(10), KeyModifiers::NONE));
        app.handle(key(KeyCode::F(5), KeyModifiers::NONE));
        assert_eq!(app.fixture.recoverable.len(), 1);
        assert_eq!(app.fixture.recoverable[0].command.as_ref(), Some(&capture));
        assert!(app.fixture.task.is_none());
    }

    #[test]
    fn ct1e_unknown_slash_never_becomes_chat() {
        let mut app = App::new(true);
        app.handle(Event::Paste("/tmp/file".into()));
        app.record_presentation();
        app.handle(key(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(app.editor.text(), "/tmp/file");
        assert!(app.fixture.pending.is_none());
        assert!(app.notice.contains("Unknown"));
    }

    #[test]
    fn ct8_old_extension_prefix_is_unknown_without_a_chat_fallback() {
        let mut app = App::new(true);
        app.handle(Event::Paste("/extension:checks/test body".into()));
        app.record_presentation();
        app.handle(key(KeyCode::Enter, KeyModifiers::NONE));
        assert!(app.fixture.pending.is_none());
        assert_eq!(app.editor.text(), "/extension:checks/test body");
        assert!(app.notice.contains("Unknown"));
    }

    #[test]
    fn ct9f_old_first_party_source_name_is_unknown_without_fallback() {
        let mut app = App::new(true);
        app.handle(Event::Paste("/ext:asura/check note".into()));
        app.record_presentation();
        app.handle(key(KeyCode::Enter, KeyModifiers::NONE));
        assert!(app.fixture.pending.is_none());
        assert_eq!(app.editor.text(), "/ext:asura/check note");
        assert!(app.notice.contains("Unknown"));
    }

    #[test]
    fn ct8_short_extension_prefix_completes_to_canonical_name() {
        let mut app = App::new(true);
        app.handle(Event::Paste("/ext:ch".into()));
        app.handle(key(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(app.editor.text(), "/ext:checks/test");
        app.handle(key(KeyCode::Char('z'), KeyModifiers::CONTROL));
        assert_eq!(app.editor.text(), "/ext:ch");
    }

    #[test]
    fn cd2_command_cue_remains_in_arguments_without_suggestions_or_quoted_cue() {
        let mut app = App::new(true);
        app.handle(Event::Paste("/ext:checks/test body".into()));
        let view = app.command_suggestions().unwrap();
        assert!(view.rows.is_empty());
        assert!(view.cue.as_deref().unwrap().contains("Enter command"));
        app.handle(key(KeyCode::Char('a'), KeyModifiers::CONTROL));
        app.handle(Event::Paste("'/tmp/file".into()));
        assert!(app.command_suggestions().is_none());
    }

    #[test]
    fn cd4_f10_changes_sources_only_in_editor_or_command_browser() {
        let mut app = App::new(true);
        let original = app.fixture.command_catalogue().revision;
        app.handle(key(KeyCode::F(1), KeyModifiers::NONE));
        app.handle(key(KeyCode::F(10), KeyModifiers::NONE));
        assert_eq!(app.fixture.command_catalogue().revision, original);
        app.handle(key(KeyCode::Esc, KeyModifiers::NONE));
        app.handle(key(KeyCode::F(10), KeyModifiers::NONE));
        let revoked = app.fixture.command_catalogue().revision;
        assert!(revoked > original);
        app.handle(key(KeyCode::F(9), KeyModifiers::NONE));
        app.handle(key(KeyCode::F(10), KeyModifiers::NONE));
        assert!(app.fixture.command_catalogue().revision > revoked);
    }

    #[test]
    fn ct7_full_catalogue_filter_has_stable_identity_order_and_bounded_rows() {
        let entries: Vec<_> = (0..128)
            .map(|index| {
                let id: &'static str =
                    Box::leak(format!("extension:load/{index:03}").into_boxed_str());
                let name: &'static str =
                    Box::leak(format!("/ext:load/{index:03}").into_boxed_str());
                CommandDefinition {
                    id,
                    name,
                    aliases: &[],
                    label: "Load",
                    purpose: "Synthetic catalogue load test",
                    category: CommandCategory::Extension,
                    operation: CommandOperation::WorkRequest,
                    origin: CommandOrigin::Integration,
                    source_id: "extension:load",
                    source_revision: 1,
                    definition_revision: 1,
                    availability: CommandAvailability::Available,
                    supported_modes: &[Action::NewTurn],
                }
            })
            .collect();
        let matches = command_matches(&entries, "/ext:load/");
        assert_eq!(matches.len(), 128);
        assert_eq!(matches.first().unwrap().id, "extension:load/000");
        assert_eq!(matches.last().unwrap().id, "extension:load/127");
        let visible: Vec<_> = matches.iter().take(6).map(command_row).collect();
        assert_eq!(visible.len(), 6);
        assert!(visible.iter().all(|row| row.contains("Extension")));
    }

    #[test]
    fn ct9_only_fixed_quit_identity_can_take_client_session_route() {
        assert!(is_local_quit(&local_quit()));
        let fixture = Fixture::default();
        let mut contributed = fixture
            .command_catalogue()
            .entries
            .into_iter()
            .find(|entry| entry.id == "extension:core/check")
            .unwrap();
        assert!(!is_local_quit(&contributed));
        contributed.operation = CommandOperation::ClientSession;
        assert!(!is_local_quit(&contributed));
        contributed.id = "builtin:quit";
        contributed.name = "/quit";
        assert!(!is_local_quit(&contributed));
    }

    #[test]
    fn ct10_help_completion_opens_existing_local_view_without_fixture_request() {
        let mut app = App::new(true);
        app.handle(Event::Paste("/he".into()));
        app.handle(key(KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(app.editor.text(), "/help");
        assert!(app.overlay.is_none());
        app.record_presentation();
        app.handle(key(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(app.overlay, Some(Overlay::Help)));
        assert!(app.editor.is_empty());
        assert!(app.fixture.pending.is_none());
        assert!(app.fixture.task.is_none());
        app.handle(key(KeyCode::Esc, KeyModifiers::NONE));
        assert!(app.overlay.is_none());
        assert!(app.editor.is_empty());
    }

    #[test]
    fn ct10_help_stays_local_during_work_disconnect_and_source_revocation() {
        let mut app = working();
        let task = app.fixture.task.as_ref().unwrap().target.id;
        assert!(app.fixture.change_command_sources(false));
        assert!(app.fixture.set_connected(false));
        let catalogue_revision = app.fixture.command_catalogue().revision;
        app.handle(Event::Paste("/help".into()));
        app.record_presentation();
        app.handle(key(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(app.overlay, Some(Overlay::Help)));
        assert_eq!(app.fixture.task.as_ref().unwrap().target.id, task);
        assert!(app.fixture.pending.is_none());
        assert_eq!(app.fixture.command_catalogue().revision, catalogue_revision);
        assert!(!app.fixture.connected);
    }

    #[test]
    fn ct10_help_preserves_pending_request_identity() {
        let mut app = App::new(true);
        app.handle(Event::Paste("initial work".into()));
        app.handle(key(KeyCode::Enter, KeyModifiers::NONE));
        let pending = app.fixture.pending.as_ref().unwrap().id;
        app.handle(Event::Paste("/help".into()));
        app.record_presentation();
        app.handle(key(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(app.overlay, Some(Overlay::Help)));
        assert_eq!(app.fixture.pending.as_ref().unwrap().id, pending);
        assert!(app.editor.is_empty());
    }

    #[test]
    fn ct10_help_arguments_retain_draft_and_other_project_text() {
        let mut app = App::new(true);
        app.handle(key(KeyCode::Char('p'), KeyModifiers::CONTROL));
        app.handle(Event::Paste("Observatory draft".into()));
        app.handle(key(KeyCode::Char('p'), KeyModifiers::CONTROL));
        app.handle(Event::Paste("/help topic".into()));
        app.record_presentation();
        app.handle(key(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(app.editor.text(), "/help topic");
        assert!(app.notice.contains("takes no arguments"));
        assert!(app.overlay.is_none());
        app.handle(key(KeyCode::Char('z'), KeyModifiers::CONTROL));
        assert!(app.editor.is_empty());
        app.handle(Event::Paste("/help  ".into()));
        app.record_presentation();
        app.handle(key(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(app.overlay, Some(Overlay::Help)));
        app.handle(key(KeyCode::F(1), KeyModifiers::NONE));
        assert!(app.editor.is_empty());
        app.handle(key(KeyCode::Char('p'), KeyModifiers::CONTROL));
        assert_eq!(app.editor.text(), "Observatory draft");
    }

    #[test]
    fn ct10_contributed_client_view_claim_cannot_open_local_help() {
        assert!(is_local_help(&local_help()));
        let mut contributed = Fixture::default()
            .command_catalogue()
            .entries
            .into_iter()
            .find(|entry| entry.id == "extension:core/check")
            .unwrap();
        contributed.operation = CommandOperation::ClientView;
        contributed.name = "/help";
        assert!(!is_local_help(&contributed));
        contributed.id = "builtin:help";
        assert!(!is_local_help(&contributed));
    }

    #[test]
    fn ct10_project_switch_from_help_shows_destination_draft() {
        let mut app = App::new(true);
        app.handle(key(KeyCode::Char('p'), KeyModifiers::CONTROL));
        app.handle(Event::Paste("Observatory draft".into()));
        app.handle(key(KeyCode::Char('p'), KeyModifiers::CONTROL));
        app.handle(Event::Paste("/help".into()));
        app.record_presentation();
        app.handle(key(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(app.overlay, Some(Overlay::Help)));
        app.handle(key(KeyCode::Char('p'), KeyModifiers::CONTROL));
        assert!(app.overlay.is_none());
        assert_eq!(app.project(), Project::Observatory);
        assert_eq!(app.editor.text(), "Observatory draft");
        app.handle(key(KeyCode::Char('p'), KeyModifiers::CONTROL));
        assert!(app.editor.is_empty());
    }

    #[test]
    fn tp2_paste_and_newline_do_not_send() {
        let mut app = App::default();
        app.handle(Event::Paste("first\r\nsecond".into()));
        for kind in [
            KeyEventKind::Press,
            KeyEventKind::Repeat,
            KeyEventKind::Release,
        ] {
            app.handle(Event::Key(KeyEvent::new_with_kind(
                KeyCode::Char('j'),
                KeyModifiers::CONTROL,
                kind,
            )));
        }
        assert_eq!(app.editor.text(), "first\nsecond");
        assert!(app.fixture.pending.is_none());
        app.handle(key(KeyCode::Enter, KeyModifiers::ALT));
        assert_eq!(app.editor.text(), "first\nsecond\n");
        assert!(app.fixture.pending.is_none());
        app.handle(key(KeyCode::Char('v'), KeyModifiers::CONTROL));
        // Raw pasted LF decodes as Ctrl+J inside explicit capture.
        app.handle(key(KeyCode::Char('j'), KeyModifiers::CONTROL));
        assert_eq!(app.editor.text(), "first\nsecond\n");
        app.handle(key(KeyCode::Char('v'), KeyModifiers::CONTROL));
        app.handle(key(KeyCode::Tab, KeyModifiers::NONE));
        app.handle(key(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(app.editor.text(), "first\nsecond\n\n");
        assert!(app.fixture.pending.is_none());
    }

    #[test]
    fn tp2_late_ack_cannot_clear_newer_typing_and_busy_send_preserves_it() {
        let mut app = App::new(true);
        app.handle(Event::Paste("submitted".into()));
        app.handle(key(KeyCode::Enter, KeyModifiers::NONE));
        app.handle(Event::Paste("new draft".into()));
        app.handle(key(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(app.editor.text(), "new draft");
        assert_eq!(app.fixture.pending.as_ref().unwrap().text, "submitted");
        app.handle(key(KeyCode::F(5), KeyModifiers::NONE));
        assert_eq!(app.editor.text(), "new draft");
        assert!(app.fixture.pending.is_none());
    }

    #[test]
    fn tp2_release_repeat_and_tiny_geometry_cannot_send() {
        let mut app = App::default();
        app.handle(Event::Paste("kept".into()));
        for kind in [KeyEventKind::Repeat, KeyEventKind::Release] {
            app.handle(Event::Key(KeyEvent::new_with_kind(
                KeyCode::Enter,
                KeyModifiers::NONE,
                kind,
            )));
        }
        app.handle(Event::Resize(29, 7));
        app.handle(key(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(app.editor.text(), "kept");
        assert!(app.fixture.pending.is_none());
    }

    #[test]
    fn tp5_exit_requires_deliberate_choice_and_modal_paste_cannot_change_draft() {
        let mut app = App::default();
        app.handle(Event::Paste("kept".into()));
        app.handle(key(KeyCode::Char('q'), KeyModifiers::CONTROL));
        app.handle(key(KeyCode::Enter, KeyModifiers::NONE));
        app.handle(Event::Paste("discard".into()));
        assert!(!app.exit);
        assert_eq!(app.editor.text(), "kept");
        app.handle(key(KeyCode::Tab, KeyModifiers::NONE));
        app.handle(key(KeyCode::Enter, KeyModifiers::NONE));
        assert!(app.exit);
    }

    #[test]
    fn tp2_fallback_paste_is_one_atomic_edit_with_no_default_finish() {
        let mut app = App::default();
        app.handle(Event::Paste("original".into()));
        app.handle(key(KeyCode::Char('a'), KeyModifiers::CONTROL));
        app.handle(key(KeyCode::Char('v'), KeyModifiers::CONTROL));
        app.handle(Event::Paste("first\nsecond".into()));
        app.handle(key(KeyCode::Char('v'), KeyModifiers::CONTROL));
        app.handle(key(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(app.editor.text(), "original");
        assert!(app.paste_mode);
        app.handle(key(KeyCode::Tab, KeyModifiers::NONE));
        app.handle(key(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(app.editor.text(), "first\nsecond");
        assert!(app.fixture.pending.is_none());
        app.handle(key(KeyCode::Char('z'), KeyModifiers::CONTROL));
        assert_eq!(app.editor.text(), "original");
    }

    #[test]
    fn tp2_fallback_oversize_or_control_input_cannot_change_or_submit_draft() {
        let mut app = App::default();
        app.handle(Event::Paste("original".into()));
        app.handle(key(KeyCode::Char('v'), KeyModifiers::CONTROL));
        app.handle(Event::Paste("x".repeat(65_537)));
        app.handle(key(KeyCode::Char('v'), KeyModifiers::CONTROL));
        app.handle(key(KeyCode::Tab, KeyModifiers::NONE));
        app.handle(key(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(app.editor.text(), "original");
        assert!(app.capture_error.is_some());
        app.handle(key(KeyCode::Esc, KeyModifiers::NONE));
        app.handle(key(KeyCode::Char('v'), KeyModifiers::CONTROL));
        app.handle(key(KeyCode::Esc, KeyModifiers::NONE));
        app.handle(key(KeyCode::Enter, KeyModifiers::NONE));
        assert!(app.paste_mode);
        assert_eq!(app.editor.text(), "original");
        assert!(app.fixture.pending.is_none());
    }

    fn press(app: &mut App, code: KeyCode) {
        app.handle(key(code, KeyModifiers::NONE));
    }

    fn control(app: &mut App, letter: char) {
        app.handle(key(KeyCode::Char(letter), KeyModifiers::CONTROL));
    }

    fn working() -> App {
        let mut app = App::new(true);
        app.handle(Event::Paste("original task".into()));
        press(&mut app, KeyCode::Enter);
        press(&mut app, KeyCode::F(5));
        assert!(app.fixture.task.is_some());
        app
    }

    #[test]
    fn tp3_choice_has_no_default_and_expiry_cannot_submit() {
        let mut app = working();
        app.handle(Event::Paste("refinement".into()));
        control(&mut app, 'a');
        press(&mut app, KeyCode::Enter);
        let captured = app.overlay.clone();
        press(&mut app, KeyCode::Enter);
        app.handle(Event::Paste("must not replace".into()));
        assert_eq!(app.overlay, captured);
        assert_eq!(app.editor.text(), "refinement");
        press(&mut app, KeyCode::Tab);
        for kind in [KeyEventKind::Repeat, KeyEventKind::Release] {
            app.handle(Event::Key(KeyEvent::new_with_kind(
                KeyCode::Enter,
                KeyModifiers::NONE,
                kind,
            )));
        }
        assert!(app.fixture.pending.is_none());
        press(&mut app, KeyCode::F(5));
        assert!(app.fixture.task.is_none());
        press(&mut app, KeyCode::Enter);
        assert!(matches!(
            app.overlay,
            Some(Overlay::Submission {
                selected: Some(0),
                ..
            })
        ));
        assert!(app.fixture.pending.is_none());
        assert_eq!(app.editor.text(), "refinement");
        press(&mut app, KeyCode::Esc);
        app.handle(Event::Paste("replacement".into()));
        assert_eq!(
            app.editor.text(),
            "replacement",
            "original selection survived choice and expiry"
        );
        control(&mut app, 'z');
        assert_eq!(app.editor.text(), "refinement");
    }

    #[test]
    fn tp3_decision_arrival_keeps_editing_and_expiration_retains_focus() {
        let mut app = working();
        app.handle(Event::Paste("keep typing".into()));
        press(&mut app, KeyCode::F(3));
        assert!(app.overlay.is_none());
        app.handle(Event::Paste(" independently".into()));
        control(&mut app, 'o');
        press(&mut app, KeyCode::Enter);
        assert!(app.fixture.task.as_ref().unwrap().decision.is_some());
        press(&mut app, KeyCode::Tab);
        press(&mut app, KeyCode::F(3));
        press(&mut app, KeyCode::Enter);
        assert!(matches!(
            app.overlay,
            Some(Overlay::Decision {
                selected: Some(0),
                ..
            })
        ));
        assert!(app.fixture.pending.is_none());
        assert_eq!(app.editor.text(), "keep typing independently");
        let view = app.overlay_view().unwrap();
        assert!(view.actions.iter().all(|a| a.contains("unavailable")));
        press(&mut app, KeyCode::Esc);
        assert!(app.overlay.is_none());
    }

    #[test]
    fn tp4_choice_acceptance_orders_preserve_newer_draft() {
        for completion_first in [false, true] {
            let mut app = working();
            app.handle(Event::Paste("steer".into()));
            press(&mut app, KeyCode::Enter);
            press(&mut app, KeyCode::Tab);
            press(&mut app, KeyCode::Enter);
            assert!(app.editor.is_empty());
            app.handle(Event::Paste("new draft".into()));
            if completion_first {
                app.fixture.complete(1);
            }
            press(&mut app, KeyCode::F(5));
            if !completion_first {
                app.fixture.complete(1);
            }
            assert_eq!(app.editor.text(), "new draft");
            assert!(app.fixture.pending.is_none());
            assert_eq!(app.fixture.recoverable.len(), usize::from(completion_first));
            if completion_first {
                assert_eq!(app.fixture.recoverable[0].text, "steer");
            } else {
                assert!(
                    app.fixture
                        .transcript
                        .iter()
                        .any(|entry| entry.text.contains("steer"))
                );
            }
        }
    }

    #[test]
    fn tp4_navigation_retains_editor_selection_history_and_hidden_pending() {
        let mut app = App::default();
        app.handle(Event::Paste("Studio request".into()));
        press(&mut app, KeyCode::Enter);
        app.handle(Event::Paste("Studio draft".into()));
        control(&mut app, 'a');
        control(&mut app, 'p');
        assert_eq!(app.project(), Project::Observatory);
        app.handle(Event::Paste("Observatory draft".into()));
        assert!(app.advance(250));
        assert!(app.other_fixture().pending.is_none());
        assert!(app.other_fixture().task.is_some());
        assert_eq!(app.editor.text(), "Observatory draft");
        control(&mut app, 'p');
        app.handle(Event::Paste("replaced".into()));
        assert_eq!(app.editor.text(), "replaced");
        control(&mut app, 'z');
        assert_eq!(app.editor.text(), "Studio draft");
        control(&mut app, 'p');
        assert_eq!(app.editor.text(), "Observatory draft");
    }

    #[test]
    fn tp4_rejection_recovery_appends_to_whole_draft_as_one_undo() {
        let mut app = App::new(true);
        press(&mut app, KeyCode::F(4));
        app.handle(Event::Paste("rejected text".into()));
        press(&mut app, KeyCode::Enter);
        app.handle(Event::Paste("newer draft".into()));
        control(&mut app, 'a');
        press(&mut app, KeyCode::F(5));
        control(&mut app, 'o');
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.editor.text(), "newer draft");
        assert_eq!(app.fixture.recoverable.len(), 1);
        assert_eq!(app.overlay_view().unwrap().actions[0], "Append to draft");
        press(&mut app, KeyCode::Tab);
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.editor.text(), "newer draft\nrejected text");
        assert!(app.fixture.recoverable.is_empty());
        assert!(app.fixture.pending.is_none());
        control(&mut app, 'z');
        assert_eq!(app.editor.text(), "newer draft");
    }

    #[test]
    fn tp4_oversized_recovery_preserves_retained_text_and_new_selection() {
        let mut app = App::new(true);
        press(&mut app, KeyCode::F(4));
        app.handle(Event::Paste("recover me".into()));
        press(&mut app, KeyCode::Enter);
        app.handle(Event::Paste("x".repeat(MAX_DRAFT_BYTES)));
        control(&mut app, 'a');
        press(&mut app, KeyCode::F(5));
        control(&mut app, 'o');
        press(&mut app, KeyCode::Tab);
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.editor.text().len(), MAX_DRAFT_BYTES);
        assert_eq!(app.fixture.recoverable[0].text, "recover me");
        assert!(matches!(app.overlay, Some(Overlay::Recovery { .. })));
        press(&mut app, KeyCode::Esc);
        app.handle(Event::Paste("selection kept".into()));
        assert_eq!(app.editor.text(), "selection kept");
    }

    #[test]
    fn tp4_queue_stop_holds_text_until_explicit_discard() {
        let mut app = working();
        app.handle(Event::Paste("follow-up".into()));
        press(&mut app, KeyCode::Enter);
        press(&mut app, KeyCode::Tab);
        press(&mut app, KeyCode::Tab);
        press(&mut app, KeyCode::Enter);
        press(&mut app, KeyCode::F(5));
        assert_eq!(app.fixture.queued.len(), 1);
        app.handle(Event::Paste("independent".into()));
        control(&mut app, 'x');
        assert!(app.fixture.task.is_none());
        assert_eq!(app.fixture.recoverable[0].text, "follow-up");
        control(&mut app, 'o');
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.fixture.recoverable.len(), 1);
        press(&mut app, KeyCode::Tab);
        press(&mut app, KeyCode::Tab);
        press(&mut app, KeyCode::Enter);
        assert!(app.fixture.recoverable.is_empty());
        assert_eq!(app.editor.text(), "independent");
    }

    #[test]
    fn tp4_disconnect_reconnect_reconciles_without_new_submission() {
        let mut app = App::new(true);
        app.handle(Event::Paste("unknown outcome".into()));
        press(&mut app, KeyCode::Enter);
        let request = app.fixture.pending.as_ref().unwrap().id;
        app.handle(Event::Paste("new text".into()));
        press(&mut app, KeyCode::F(2));
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.fixture.pending.as_ref().unwrap().id, request);
        assert_eq!(app.editor.text(), "new text");
        press(&mut app, KeyCode::F(2));
        assert!(app.fixture.pending.is_none());
        assert_eq!(app.fixture.recoverable[0].id, request);
        assert_eq!(app.fixture.recoverable[0].text, "unknown outcome");
        assert_eq!(app.editor.text(), "new text");
    }

    #[test]
    fn tp3_tiny_geometry_disables_focused_actions_but_preserves_exit() {
        let mut app = working();
        app.handle(Event::Paste("keep".into()));
        press(&mut app, KeyCode::Enter);
        press(&mut app, KeyCode::Tab);
        app.handle(Event::Resize(29, 7));
        press(&mut app, KeyCode::Enter);
        press(&mut app, KeyCode::F(5));
        assert!(app.fixture.pending.is_none());
        assert!(app.fixture.task.is_some());
        assert_eq!(app.editor.text(), "keep");
        control(&mut app, 'q');
        press(&mut app, KeyCode::Tab);
        press(&mut app, KeyCode::Enter);
        assert!(app.exit);
    }

    #[test]
    fn tp5_exit_and_reset_include_hidden_protected_text_without_default() {
        let mut app = App::new(true);
        app.handle(Event::Paste("hidden pending".into()));
        press(&mut app, KeyCode::Enter);
        let old_scope = app.fixture.target().scope;
        control(&mut app, 'p');
        control(&mut app, 'q');
        press(&mut app, KeyCode::Enter);
        assert!(!app.exit);
        assert!(matches!(
            app.overlay,
            Some(Overlay::Exit { selected: false })
        ));
        press(&mut app, KeyCode::Esc);
        app.handle(Event::Paste("visible text".into()));
        press(&mut app, KeyCode::F(6));
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.editor.text(), "visible text");
        assert!(app.other_fixture().pending.is_some());
        press(&mut app, KeyCode::Tab);
        press(&mut app, KeyCode::Enter);
        assert!(app.editor.is_empty());
        assert!(app.other_fixture().pending.is_none());
        control(&mut app, 'p');
        assert_ne!(app.fixture.target().scope.generation, old_scope.generation);
    }

    #[test]
    fn tp4_second_context_reset_failure_preserves_both_contexts() {
        let mut app = working();
        app.handle(Event::Paste("Studio draft".into()));
        control(&mut app, 'p');
        app.handle(Event::Paste("Observatory pending".into()));
        press(&mut app, KeyCode::Enter);
        app.handle(Event::Paste("Observatory draft".into()));
        app.fixture.set_generation_for_test(u64::MAX);
        control(&mut app, 'p');
        let selected_target = app.fixture.target();
        let background_target = app.other.fixture.target();
        let background_request = app.other.fixture.pending.as_ref().unwrap().id;
        press(&mut app, KeyCode::F(6));
        press(&mut app, KeyCode::Tab);
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.fixture.target(), selected_target);
        assert_eq!(app.other.fixture.target(), background_target);
        assert_eq!(
            app.other.fixture.pending.as_ref().unwrap().id,
            background_request
        );
        assert_eq!(app.editor.text(), "Studio draft");
        assert_eq!(app.other.editor.text(), "Observatory draft");
        assert!(matches!(
            app.overlay,
            Some(Overlay::Reset { selected: true })
        ));
        assert!(app.notice.contains("identities exhausted"));
    }

    #[test]
    fn tp4_lost_ack_reconciles_one_effect_and_failure_holds_queue() {
        let mut app = App::new(true);
        app.handle(Event::Paste("one accepted task".into()));
        press(&mut app, KeyCode::Enter);
        let request = app.fixture.pending.as_ref().unwrap().id;
        app.handle(Event::Paste("new draft".into()));
        press(&mut app, KeyCode::F(7));
        assert!(!app.fixture.connected);
        assert!(app.fixture.pending.as_ref().unwrap().unknown);
        assert_eq!(app.fixture.pending.as_ref().unwrap().id, request);
        let target = app.fixture.task.as_ref().unwrap().target;
        press(&mut app, KeyCode::F(2));
        assert_eq!(app.fixture.task.as_ref().unwrap().target, target);
        assert!(app.fixture.pending.is_none());
        assert_eq!(app.editor.text(), "new draft");
        press(&mut app, KeyCode::Enter);
        press(&mut app, KeyCode::Up);
        press(&mut app, KeyCode::Enter);
        press(&mut app, KeyCode::F(5));
        assert_eq!(app.fixture.queued.len(), 1);
        app.handle(Event::Paste("newest text".into()));
        press(&mut app, KeyCode::F(8));
        assert!(app.fixture.task.is_none());
        assert_eq!(app.fixture.recoverable[0].text, "new draft");
        assert_eq!(app.editor.text(), "newest text");
    }
}
