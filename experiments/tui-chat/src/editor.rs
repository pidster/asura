//! Bounded draft editing through rat-text's storage, history and display mapping.

use std::{fmt, ops::Range};

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use rat_text::{
    TextPosition, TextRange,
    text_area::{TextArea, TextAreaState, TextWrap},
    undo_buffer::UndoVec,
};
use ratatui::{
    buffer::Buffer,
    layout::Rect,
    style::{Modifier, Style},
    widgets::StatefulWidget,
};
use unicode_segmentation::UnicodeSegmentation;

pub const MAX_DRAFT_BYTES: usize = 65_536;
const MAX_UNDO_SEQUENCES: u32 = 100;
const MEASURE_ROWS: u16 = 7;

/// A captured first-token command name. Byte ranges are valid only for the
/// captured draft; completion checks the full draft again before changing it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandHeader {
    pub name: String,
    pub byte_range: Range<usize>,
    draft: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditError {
    TooLarge,
    ControlCharacter,
    Dependency(String),
}

impl fmt::Display for EditError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::TooLarge => write!(f, "Draft exceeds the 64 KiB limit; edit was rejected"),
            Self::ControlCharacter => {
                write!(f, "Control characters are not accepted; edit was rejected")
            }
            Self::Dependency(error) => write!(f, "Editor could not apply the edit: {error}"),
        }
    }
}

impl std::error::Error for EditError {}

pub struct Editor {
    state: TextAreaState,
    screen_cursor: Option<(u16, u16)>,
}

impl Default for Editor {
    fn default() -> Self {
        Self::new()
    }
}

impl Editor {
    pub fn new() -> Self {
        let mut state = TextAreaState::new();
        state.set_text_wrap(TextWrap::Hard);
        state.set_auto_indent(false);
        state.set_auto_quote(false);
        state.set_tab_width(2);
        state.set_newline("\n");
        // Each replacement contains multiple operations in one sequence. The
        // append after the first operation trims the completed history to 100.
        state.set_undo_buffer(Some(UndoVec::new(MAX_UNDO_SEQUENCES)));
        Self {
            state,
            screen_cursor: None,
        }
    }

    pub fn text(&self) -> String {
        self.state.text()
    }

    pub fn is_empty(&self) -> bool {
        self.state.is_empty()
    }

    /// User-perceived characters, including spaces and normalized newlines.
    pub fn character_count(&self) -> usize {
        self.text().graphemes(true).count()
    }

    /// Parse only a slash at the first grapheme, regardless of editor focus.
    /// The first space or hard line ends the name; all later text is untouched.
    pub fn command_name(&self) -> Option<CommandHeader> {
        let draft = self.text();
        if !draft.starts_with('/') {
            return None;
        }
        let end = draft.find([' ', '\n']).unwrap_or(draft.len());
        Some(CommandHeader {
            name: draft[..end].to_owned(),
            byte_range: 0..end,
            draft,
        })
    }

    /// A command name is editable only with an empty selection and the cursor
    /// on its first-line span. Arguments and following lines are ordinary edits.
    pub fn command_header(&self) -> Option<CommandHeader> {
        let header = self.command_name()?;
        if self.state.selection().start != self.state.selection().end {
            return None;
        }
        let cursor = self.state.cursor();
        if cursor.y != 0 {
            return None;
        }
        let cursor_byte = self
            .state
            .try_bytes_at_range(TextRange::new((0, 0), cursor))
            .ok()?
            .end;
        (cursor_byte <= header.byte_range.end).then_some(header)
    }

    /// F9 can capture the insertion point of an otherwise empty draft.
    pub fn empty_command_target(&self) -> Option<CommandHeader> {
        if !self.is_empty()
            || self.state.selection().start != self.state.selection().end
            || self.state.cursor() != TextPosition::new(0, 0)
        {
            return None;
        }
        Some(CommandHeader {
            name: String::new(),
            byte_range: 0..0,
            draft: String::new(),
        })
    }

    /// Complete one captured name without reinterpreting a changed draft.
    /// `Ok(false)` means the target is stale or no longer editable; neither
    /// text nor undo history changes. The catalogue owns name resolution.
    pub fn complete_command_name(
        &mut self,
        expected: &CommandHeader,
        completed: &str,
    ) -> Result<bool, EditError> {
        if !completed.starts_with('/')
            || completed.chars().any(char::is_whitespace)
            || normalize(completed).ok().as_deref() != Some(completed)
        {
            return Ok(false);
        }
        let current = if expected.draft.is_empty() {
            self.empty_command_target()
        } else {
            self.command_header()
        };
        if current.as_ref() != Some(expected) {
            return Ok(false);
        }
        let inserted = if expected.draft.is_empty() {
            format!("{completed} ")
        } else {
            completed.to_owned()
        };
        if inserted == expected.name {
            return Ok(true);
        }
        self.replace(expected.byte_range.clone(), &inserted)?;
        Ok(true)
    }

    /// Normalize and validate before touching text, selection or undo history.
    pub fn insert(&mut self, input: &str) -> Result<(), EditError> {
        let normalized = normalize(input)?;
        let range = self
            .state
            .try_bytes_at_range(self.state.selection())
            .map_err(dependency_error)?;
        self.replace(range, &normalized)
    }

    /// Handle editing only. Enter, paste events and application shortcuts stay
    /// with the caller; this adapter never inherits the dependency's key map.
    pub fn key(&mut self, key: KeyEvent) -> Result<bool, EditError> {
        if key.kind == KeyEventKind::Release {
            return Ok(false);
        }
        let extend = key.modifiers.contains(KeyModifiers::SHIFT);
        let navigation = key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT;
        let document = key.modifiers == KeyModifiers::CONTROL
            || key.modifiers == (KeyModifiers::CONTROL | KeyModifiers::SHIFT);
        match key.code {
            KeyCode::Char('a') if key.modifiers == KeyModifiers::CONTROL => {
                self.state.select_all();
            }
            KeyCode::Char('z') if key.modifiers == KeyModifiers::CONTROL => {
                self.state.undo();
                self.state.scroll_cursor_to_visible();
            }
            KeyCode::Char('y') if key.modifiers == KeyModifiers::CONTROL => {
                self.state.redo();
                self.state.scroll_cursor_to_visible();
            }
            KeyCode::Left if navigation => {
                self.state.move_left(1, extend);
            }
            KeyCode::Right if navigation => {
                self.state.move_right(1, extend);
            }
            KeyCode::Up if navigation => {
                self.state.move_up(1, extend);
            }
            KeyCode::Down if navigation => {
                self.state.move_down(1, extend);
            }
            KeyCode::Home if navigation || document => {
                let row = if document { 0 } else { self.state.cursor().y };
                self.state.set_move_col(None);
                self.state.set_cursor((0, row), extend);
            }
            KeyCode::End if navigation || document => {
                let end = if document {
                    end_position(&self.text())
                } else {
                    let row = self.state.cursor().y;
                    TextPosition::new(self.state.line_width(row), row)
                };
                self.state.set_move_col(None);
                self.state.set_cursor(end, extend);
            }
            KeyCode::Backspace if key.modifiers.is_empty() => self.delete(true)?,
            KeyCode::Delete if key.modifiers.is_empty() => self.delete(false)?,
            KeyCode::Tab if key.modifiers.is_empty() => self.insert("  ")?,
            KeyCode::Char(c) if navigation => self.insert(c.encode_utf8(&mut [0; 4]))?,
            _ => return Ok(false),
        }
        // The store exposes a synthetic final row without a final LF. Keep
        // library navigation and selection inside the actual draft instead.
        let end = end_position(&self.text());
        let cursor = self.state.cursor().min(end);
        let anchor = self.state.anchor().min(end);
        if cursor != self.state.cursor() || anchor != self.state.anchor() {
            self.state.set_selection(anchor, cursor);
        }
        Ok(true)
    }

    fn delete(&mut self, backwards: bool) -> Result<(), EditError> {
        let text = self.text();
        let mut range = self
            .state
            .try_bytes_at_range(self.state.selection())
            .map_err(dependency_error)?;
        if range.is_empty() {
            if backwards {
                range.start = text
                    .get(..range.start)
                    .ok_or_else(|| EditError::Dependency("Invalid cursor byte position".into()))?
                    .grapheme_indices(true)
                    .next_back()
                    .map_or(range.start, |(start, _)| start);
            } else if let Some(grapheme) = text
                .get(range.end..)
                .ok_or_else(|| EditError::Dependency("Invalid cursor byte position".into()))?
                .graphemes(true)
                .next()
            {
                range.end += grapheme.len();
            }
        }
        if !range.is_empty() {
            self.replace(range, "")?;
        }
        Ok(())
    }

    fn replace(&mut self, range: Range<usize>, inserted: &str) -> Result<(), EditError> {
        let old = self.text();
        if old.get(range.clone()).is_none() {
            return Err(EditError::Dependency("Invalid selection byte range".into()));
        }
        let size = old.len() - range.len() + inserted.len();
        if size > MAX_DRAFT_BYTES {
            return Err(EditError::TooLarge);
        }
        if range.is_empty() && inserted.is_empty() {
            return Ok(());
        }
        let endpoint = range.start + inserted.len();
        let mut candidate = String::with_capacity(size);
        candidate.push_str(&old[..range.start]);
        candidate.push_str(inserted);
        candidate.push_str(&old[range.end..]);
        let cursor = position_after_byte(&candidate, endpoint);
        let full_range = TextRange::new((0, 0), end_position(&old));
        // Validate before mutation; insertion then uses the valid empty store.
        self.state
            .try_bytes_at_range(full_range)
            .map_err(dependency_error)?;
        // A full replacement adds remove + insert entries; an empty side adds
        // only one. Account for trim-before-append in both transaction shapes.
        self.state
            .value
            .set_undo_count(if old.is_empty() || candidate.is_empty() {
                MAX_UNDO_SEQUENCES - 1
            } else {
                MAX_UNDO_SEQUENCES
            });
        self.state.begin_undo_seq();
        let removed = self.state.value.remove_str_range(full_range);
        let did_remove = matches!(removed, Ok(true));
        let result = match removed {
            Ok(_) if candidate.is_empty() => Ok(false),
            Ok(_) => self
                .state
                .value
                .insert_str((0, 0).into(), &candidate)
                .map_err(dependency_error),
            Err(error) => Err(dependency_error(error)),
        };
        if result.is_ok() {
            self.state.set_cursor(cursor, false);
        }
        self.state.end_undo_seq();
        if let Err(error) = result {
            // Balance the sequence before recovery. Removal carries the original
            // cursor, anchor and bytes in the dependency's undo buffer.
            if did_remove {
                self.state.undo();
            }
            self.state.scroll_cursor_to_visible();
            return Err(error);
        }
        self.state.set_move_col(None);
        self.state.scroll_cursor_to_visible();
        Ok(())
    }

    /// Seven means at least seven rows; layout clamps this to six or three.
    /// Measurement uses the real hard-wrap widget without cloning the live state.
    pub fn visual_rows(&self, width: u16) -> u16 {
        if width == 0 {
            return 1;
        }
        let text = self.text();
        let mut measure = TextAreaState::new();
        measure.set_text(&text);
        let rect = Rect::new(0, 0, width, MEASURE_ROWS);
        let mut buffer = Buffer::empty(rect);
        TextArea::new()
            .text_wrap(TextWrap::Hard)
            .render(rect, &mut buffer, &mut measure);
        measure
            .pos_to_relative_screen(end_position(&text))
            .and_then(|(_, row)| u16::try_from(row).ok())
            .map_or(MEASURE_ROWS, |row| row.saturating_add(1).min(MEASURE_ROWS))
    }

    pub fn render(&mut self, area: Rect, buffer: &mut Buffer, style: Style, focused: bool) {
        let area = area.intersection(buffer.area);
        self.screen_cursor = None;
        if area.is_empty() {
            return;
        }
        if self.state.rendered.width != area.width {
            // An old wrap offset can fall inside a differently sized visual
            // row. Reset that offset before the widget locates the cursor.
            self.state.set_sub_row_offset(0);
        }
        self.state.scroll_cursor_to_visible();
        TextArea::new()
            .text_wrap(TextWrap::Hard)
            .style(style)
            .select_style(style.add_modifier(Modifier::REVERSED))
            .render(area, buffer, &mut self.state);
        if focused {
            // Upstream pos_to_screen excludes row/column zero. Translate its
            // public relative mapping and enforce viewport bounds here.
            self.screen_cursor = self
                .state
                .pos_to_relative_screen(self.state.cursor())
                .and_then(|(x, y)| Some((u16::try_from(x).ok()?, u16::try_from(y).ok()?)))
                .filter(|&(x, y)| x < area.width && y < area.height)
                .map(|(x, y)| (area.x + x, area.y + y));
        }
    }

    pub fn cursor(&self) -> Option<(u16, u16)> {
        self.screen_cursor
    }
}

fn dependency_error(error: impl fmt::Display) -> EditError {
    EditError::Dependency(error.to_string())
}

fn normalize(input: &str) -> Result<String, EditError> {
    let mut result = String::with_capacity(input.len().min(MAX_DRAFT_BYTES));
    let mut chars = input.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                result.push('\n');
            }
            '\n' | '\u{2028}' | '\u{2029}' => result.push('\n'),
            '\t' => result.push_str("  "),
            c if c.is_control() => return Err(EditError::ControlCharacter),
            c => result.push(c),
        }
        if result.len() > MAX_DRAFT_BYTES {
            return Err(EditError::TooLarge);
        }
    }
    Ok(result)
}

fn end_position(text: &str) -> TextPosition {
    position_after_byte(text, text.len())
}

fn position_after_byte(text: &str, byte: usize) -> TextPosition {
    let mut position = TextPosition::new(0, 0);
    for (start, grapheme) in text.grapheme_indices(true) {
        if start >= byte {
            break;
        }
        if grapheme == "\n" {
            position.y += 1;
            position.x = 0;
        } else {
            position.x += 1;
        }
    }
    position
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(editor: &mut Editor, code: KeyCode, modifiers: KeyModifiers) {
        assert!(editor.key(KeyEvent::new(code, modifiers)).unwrap());
    }

    fn draw(editor: &mut Editor, width: u16, height: u16) -> Buffer {
        let area = Rect::new(0, 0, width, height);
        let mut buffer = Buffer::empty(area);
        editor.render(area, &mut buffer, Style::default(), true);
        buffer
    }

    #[test]
    fn ct1d_only_a_leading_slash_activates_the_command_scanner() {
        for draft in [
            "'/tmp/file",
            "\"/tmp/file\"",
            "“/tmp/file”",
            "`/tmp/file`",
            " /quit",
            "hello\n/quit",
        ] {
            let mut editor = Editor::new();
            editor.insert(draft).unwrap();
            assert!(editor.command_name().is_none(), "{draft}");
            assert!(editor.command_header().is_none(), "{draft}");
            press(&mut editor, KeyCode::Tab, KeyModifiers::NONE);
            assert_eq!(editor.text(), format!("{draft}  "));
        }
        let mut editor = Editor::new();
        editor.insert("/quit").unwrap();
        assert_eq!(editor.command_name().unwrap().name, "/quit");
    }

    #[test]
    fn ct1b_name_completion_and_empty_insertion_each_undo_once() {
        let mut editor = Editor::new();
        let empty = editor.empty_command_target().unwrap();
        assert!(editor.complete_command_name(&empty, "/quit").unwrap());
        assert_eq!(editor.text(), "/quit ");
        press(&mut editor, KeyCode::Char('z'), KeyModifiers::CONTROL);
        assert_eq!(editor.text(), "");
        editor.insert("/ex").unwrap();
        let header = editor.command_header().unwrap();
        assert_eq!(header.name, "/ex");
        assert_eq!(header.byte_range, 0..3);
        assert!(editor.complete_command_name(&header, "/exit").unwrap());
        assert_eq!(editor.text(), "/exit");
        press(&mut editor, KeyCode::Char('z'), KeyModifiers::CONTROL);
        assert_eq!(editor.text(), "/ex");
    }

    #[test]
    fn ct2_multiline_unicode_arguments_survive_name_completion_and_undo() {
        let mut editor = Editor::new();
        editor.insert("/ext e\u{301} 👩🏽‍💻\n/path 名").unwrap();
        editor.state.set_cursor((4, 0), false);
        let before_cursor = editor.state.cursor();
        let header = editor.command_header().unwrap();
        assert_eq!(header.byte_range, 0..4);
        assert!(
            editor
                .complete_command_name(&header, "/ext:checks/test")
                .unwrap()
        );
        assert_eq!(editor.text(), "/ext:checks/test e\u{301} 👩🏽‍💻\n/path 名");
        assert_eq!(editor.state.cursor(), TextPosition::new(16, 0));
        press(&mut editor, KeyCode::Char('z'), KeyModifiers::CONTROL);
        assert_eq!(editor.text(), "/ext e\u{301} 👩🏽‍💻\n/path 名");
        assert_eq!(editor.state.cursor(), before_cursor);
    }

    #[test]
    fn ct2_stale_span_and_selected_text_reject_without_mutation() {
        let mut editor = Editor::new();
        editor.insert("/qu body").unwrap();
        editor.state.set_cursor((3, 0), false);
        let captured = editor.command_header().unwrap();
        editor.insert("x").unwrap();
        let before = editor.text();
        let history = editor.state.undo_buffer().unwrap().open_undo();
        assert!(!editor.complete_command_name(&captured, "/quit").unwrap());
        assert_eq!(editor.text(), before);
        assert_eq!(editor.state.undo_buffer().unwrap().open_undo(), history);

        editor.state.set_selection((0, 0), (2, 0));
        assert!(editor.command_header().is_none());
        let history = editor.state.undo_buffer().unwrap().open_undo();
        assert!(!editor.complete_command_name(&captured, "/quit").unwrap());
        assert_eq!(editor.text(), before);
        assert_eq!(editor.state.undo_buffer().unwrap().open_undo(), history);

        let mut editor = Editor::new();
        editor.insert("/qu body").unwrap();
        editor.state.set_cursor((3, 0), false);
        let captured = editor.command_header().unwrap();
        editor.state.set_cursor((8, 0), false);
        editor.insert("!").unwrap();
        editor.state.set_cursor((3, 0), false);
        let history = editor.state.undo_buffer().unwrap().open_undo();
        assert!(!editor.complete_command_name(&captured, "/quit").unwrap());
        assert_eq!(editor.text(), "/qu body!");
        assert_eq!(editor.state.undo_buffer().unwrap().open_undo(), history);
    }

    #[test]
    fn ct2_name_parser_is_cursor_independent_but_header_is_not() {
        let mut editor = Editor::new();
        editor.insert("/quit arg\ncontinued").unwrap();
        assert_eq!(editor.command_name().unwrap().name, "/quit");
        assert!(editor.command_header().is_none());
        editor.state.set_cursor((3, 0), false);
        assert_eq!(editor.command_header().unwrap().name, "/quit");
        editor.state.set_cursor((7, 0), false);
        assert!(editor.command_header().is_none());
    }

    #[test]
    fn ps1_character_count_follows_graphemes_normalization_and_undo() {
        let mut editor = Editor::new();
        assert_eq!(editor.character_count(), 0);
        editor.insert("e\u{301} 👩🏽‍💻\r\n界").unwrap();
        assert_eq!(editor.character_count(), 5);
        press(&mut editor, KeyCode::Char('a'), KeyModifiers::CONTROL);
        editor.insert(" \n").unwrap();
        assert_eq!(editor.character_count(), 2);
        press(&mut editor, KeyCode::Char('z'), KeyModifiers::CONTROL);
        assert_eq!(editor.character_count(), 5);
        press(&mut editor, KeyCode::Char('y'), KeyModifiers::CONTROL);
        assert_eq!(editor.character_count(), 2);
        assert!(editor.insert("\u{1b}").is_err());
        assert_eq!(editor.character_count(), 2);
    }

    #[test]
    fn tp2_normalization_and_rejected_controls_are_atomic() {
        let mut editor = Editor::new();
        editor.insert("a\r\nb\rc\td").unwrap();
        assert_eq!(editor.text(), "a\nb\nc  d");
        press(&mut editor, KeyCode::Char('a'), KeyModifiers::CONTROL);
        let selection = editor.state.selection();
        let history = editor.state.undo_buffer().unwrap().open_undo();
        for input in ["ok\u{1b}[31m", "ok\0", "ok\u{7f}", "ok\u{85}"] {
            assert_eq!(editor.insert(input), Err(EditError::ControlCharacter));
            assert_eq!(editor.text(), "a\nb\nc  d");
            assert_eq!(editor.state.selection(), selection);
            assert_eq!(editor.state.undo_buffer().unwrap().open_undo(), history);
        }
    }

    #[test]
    fn tp2_grapheme_deletion_and_movement() {
        for cluster in ["e\u{301}", "👨‍👩‍👧‍👦", "🇬🇧", "👍🏽", "界"] {
            let mut editor = Editor::new();
            editor.insert(&format!("a{cluster}z")).unwrap();
            press(&mut editor, KeyCode::Left, KeyModifiers::NONE);
            press(&mut editor, KeyCode::Backspace, KeyModifiers::NONE);
            assert_eq!(editor.text(), "az", "{cluster}");
            press(&mut editor, KeyCode::Char('z'), KeyModifiers::CONTROL);
            assert_eq!(editor.text(), format!("a{cluster}z"));
            press(&mut editor, KeyCode::Left, KeyModifiers::NONE);
            press(&mut editor, KeyCode::Delete, KeyModifiers::NONE);
            assert_eq!(editor.text(), "az");
        }
    }

    #[test]
    fn tp2_cluster_merging_insertions_and_deletions() {
        for (before, position, inserted, after) in [
            ("e", 1, "\u{301}", "e\u{301}"),
            ("👩👩", 1, "\u{200d}", "👩‍👩"),
            ("🇬", 1, "🇧", "🇬🇧"),
            ("👍", 1, "🏽", "👍🏽"),
        ] {
            let mut editor = Editor::new();
            editor.insert(before).unwrap();
            editor.state.set_cursor((position, 0), false);
            editor.insert(inserted).unwrap();
            assert_eq!(editor.text(), after);
            assert_eq!(editor.state.cursor(), TextPosition::new(1, 0));
            press(&mut editor, KeyCode::Char('z'), KeyModifiers::CONTROL);
            assert_eq!(editor.text(), before);
            assert_eq!(editor.state.cursor(), TextPosition::new(position, 0));
            press(&mut editor, KeyCode::Char('y'), KeyModifiers::CONTROL);
            assert_eq!(editor.text(), after);
            assert_eq!(editor.state.cursor(), TextPosition::new(1, 0));
        }
        let mut editor = Editor::new();
        editor.insert("🇬 🇧").unwrap();
        editor.state.set_cursor((1, 0), false);
        press(&mut editor, KeyCode::Delete, KeyModifiers::NONE);
        assert_eq!(editor.text(), "🇬🇧");
        assert_eq!(editor.state.cursor(), TextPosition::new(1, 0));
        press(&mut editor, KeyCode::Backspace, KeyModifiers::NONE);
        assert!(editor.is_empty());
    }

    #[test]
    fn tp2_paste_replacement_is_one_undo_with_exact_selection() {
        let mut editor = Editor::new();
        editor.insert("before 👨‍👩‍👧‍👦 after").unwrap();
        editor.state.set_selection((7, 0), (8, 0));
        let original_cursor = editor.state.cursor();
        let original_anchor = editor.state.anchor();
        editor.insert("first\r\nsecond\tline").unwrap();
        assert_eq!(editor.text(), "before first\nsecond  line after");
        let new_cursor = editor.state.cursor();
        press(&mut editor, KeyCode::Char('z'), KeyModifiers::CONTROL);
        assert_eq!(editor.text(), "before 👨‍👩‍👧‍👦 after");
        assert_eq!(editor.state.cursor(), original_cursor);
        assert_eq!(editor.state.anchor(), original_anchor);
        press(&mut editor, KeyCode::Char('y'), KeyModifiers::CONTROL);
        assert_eq!(editor.text(), "before first\nsecond  line after");
        assert_eq!(editor.state.cursor(), new_cursor);
    }

    #[test]
    fn tp2_size_boundary_accounts_for_selection_and_normalization() {
        let mut editor = Editor::new();
        let full = "x".repeat(MAX_DRAFT_BYTES);
        editor.insert(&full).unwrap();
        assert_eq!(editor.insert("é"), Err(EditError::TooLarge));
        assert_eq!(editor.text(), full);
        press(&mut editor, KeyCode::Char('a'), KeyModifiers::CONTROL);
        let cursor = editor.state.cursor();
        let anchor = editor.state.anchor();
        let history = editor.state.undo_buffer().unwrap().open_undo();
        assert_eq!(
            editor.insert(&"\t".repeat(MAX_DRAFT_BYTES / 2 + 1)),
            Err(EditError::TooLarge)
        );
        assert_eq!(editor.text(), full);
        assert_eq!(editor.state.cursor(), cursor);
        assert_eq!(editor.state.anchor(), anchor);
        assert_eq!(editor.state.undo_buffer().unwrap().open_undo(), history);
        editor.insert(&"é".repeat(MAX_DRAFT_BYTES / 2)).unwrap();
        assert_eq!(editor.text().len(), MAX_DRAFT_BYTES);
        press(&mut editor, KeyCode::Char('z'), KeyModifiers::CONTROL);
        assert_eq!(editor.text(), full);
    }

    #[test]
    fn tp2_history_retains_at_most_one_hundred_sequences() {
        let mut editor = Editor::new();
        for _ in 0..130 {
            editor.insert("a").unwrap();
        }
        for _ in 0..100 {
            press(&mut editor, KeyCode::Char('z'), KeyModifiers::CONTROL);
        }
        assert_eq!(editor.text(), "a".repeat(30));
        press(&mut editor, KeyCode::Char('z'), KeyModifiers::CONTROL);
        assert_eq!(editor.text(), "a".repeat(30));
        for _ in 0..100 {
            press(&mut editor, KeyCode::Char('y'), KeyModifiers::CONTROL);
        }
        assert_eq!(editor.text(), "a".repeat(130));
    }

    #[test]
    fn tp2_history_bound_survives_empty_replacements_and_branching() {
        let mut editor = Editor::new();
        for _ in 0..130 {
            editor.insert("a").unwrap();
            press(&mut editor, KeyCode::Backspace, KeyModifiers::NONE);
            assert!(editor.state.undo_buffer().unwrap().open_undo() <= 100);
        }
        for _ in 0..20 {
            press(&mut editor, KeyCode::Char('z'), KeyModifiers::CONTROL);
        }
        editor.insert("branch").unwrap();
        press(&mut editor, KeyCode::Char('y'), KeyModifiers::CONTROL);
        assert_eq!(editor.text(), "branch");
        assert!(editor.state.undo_buffer().unwrap().open_undo() <= 100);
    }

    #[test]
    fn tp2_large_multiline_unicode_corpus_keeps_boundaries() {
        let mut editor = Editor::new();
        let corpus = "e\u{301} 👨‍👩‍👧‍👦 🇬🇧 👍🏽 界\n".repeat(600);
        assert!(corpus.len() < MAX_DRAFT_BYTES);
        editor.insert(&corpus).unwrap();
        assert_eq!(editor.state.cursor(), TextPosition::new(0, 600));
        draw(&mut editor, 38, 6);
        assert!(editor.cursor().is_some());
        press(&mut editor, KeyCode::Backspace, KeyModifiers::NONE);
        assert_eq!(editor.text(), corpus.trim_end_matches('\n'));
        press(&mut editor, KeyCode::Char('z'), KeyModifiers::CONTROL);
        assert_eq!(editor.text(), corpus);
        assert_eq!(editor.state.cursor(), TextPosition::new(0, 600));
    }

    #[test]
    fn tp2_unicode_separator_text_remains_editable() {
        let mut editor = Editor::new();
        editor.insert("a\u{2028}b\u{2029}c").unwrap();
        assert_eq!(editor.text(), "a\nb\nc");
        editor.insert("d").unwrap();
        assert_eq!(editor.text(), "a\nb\ncd");
        press(&mut editor, KeyCode::Char('z'), KeyModifiers::CONTROL);
        assert_eq!(editor.text(), "a\nb\nc");
        press(&mut editor, KeyCode::Char('y'), KeyModifiers::CONTROL);
        assert_eq!(editor.text(), "a\nb\ncd");
        editor.state.set_selection((0, 1), (1, 2));
        editor.insert("X\u{2028}Y").unwrap();
        assert_eq!(editor.text(), "a\nX\nYd");
        press(&mut editor, KeyCode::Char('z'), KeyModifiers::CONTROL);
        assert_eq!(editor.text(), "a\nb\ncd");
        assert_eq!(editor.state.anchor(), TextPosition::new(0, 1));
        assert_eq!(editor.state.cursor(), TextPosition::new(1, 2));
    }

    #[test]
    fn tp2_document_and_line_ends_use_real_text() {
        let mut editor = Editor::new();
        editor.insert(" first\n界e\u{301}").unwrap();
        press(&mut editor, KeyCode::Home, KeyModifiers::CONTROL);
        assert_eq!(editor.state.cursor(), TextPosition::new(0, 0));
        press(&mut editor, KeyCode::End, KeyModifiers::NONE);
        assert_eq!(editor.state.cursor(), TextPosition::new(6, 0));
        press(&mut editor, KeyCode::End, KeyModifiers::CONTROL);
        assert_eq!(editor.state.cursor(), TextPosition::new(2, 1));
        editor.insert("\n").unwrap();
        press(&mut editor, KeyCode::End, KeyModifiers::CONTROL);
        assert_eq!(editor.state.cursor(), TextPosition::new(0, 2));
    }

    #[test]
    fn tp1_wrapped_navigation_resize_and_cursor_stay_consistent() {
        let mut editor = Editor::new();
        editor.insert("abcdefghijkl").unwrap();
        assert_eq!(editor.visual_rows(5), 3);
        draw(&mut editor, 5, 3);
        assert_eq!(editor.cursor(), Some((2, 2)));
        press(&mut editor, KeyCode::Up, KeyModifiers::NONE);
        draw(&mut editor, 5, 3);
        assert_eq!(editor.state.cursor(), TextPosition::new(7, 0));
        let original = (editor.text(), editor.state.cursor(), editor.state.anchor());
        draw(&mut editor, 3, 2);
        assert_eq!(
            (editor.text(), editor.state.cursor(), editor.state.anchor()),
            original
        );
        assert!(editor.cursor().is_some_and(|(x, y)| x < 3 && y < 2));
        press(&mut editor, KeyCode::Home, KeyModifiers::CONTROL);
        draw(&mut editor, 5, 3);
        assert_eq!(editor.cursor(), Some((0, 0)));
        editor.render(
            Rect::new(0, 0, 0, 0),
            &mut Buffer::empty(Rect::default()),
            Style::default(),
            true,
        );
        assert_eq!(editor.cursor(), None);
        assert_eq!(editor.visual_rows(0), 1);
    }

    #[test]
    fn tp1_unicode_width_and_multiline_measurement_use_widget_mapping() {
        let mut editor = Editor::new();
        editor.insert("界e\u{301}👨‍👩‍👧‍👦").unwrap();
        draw(&mut editor, 8, 3);
        assert_eq!(editor.cursor(), Some((5, 0)));
        assert_eq!(editor.visual_rows(4), 2);
        editor.insert("\n\n\n\n\n\n\n").unwrap();
        assert_eq!(editor.visual_rows(20), MEASURE_ROWS);
        draw(&mut editor, 20, 3);
        assert!(editor.cursor().is_some_and(|(_, y)| y < 3));
    }

    #[test]
    fn tp2_vertical_movement_stays_inside_actual_draft() {
        let mut editor = Editor::new();
        editor.insert("ab界cdefgh").unwrap();
        draw(&mut editor, 5, 3);
        press(&mut editor, KeyCode::Home, KeyModifiers::CONTROL);
        draw(&mut editor, 5, 3);
        press(&mut editor, KeyCode::Down, KeyModifiers::NONE);
        draw(&mut editor, 5, 3);
        assert_eq!(editor.state.cursor(), TextPosition::new(4, 0));
        press(&mut editor, KeyCode::End, KeyModifiers::CONTROL);
        draw(&mut editor, 5, 3);
        for _ in 0..3 {
            press(&mut editor, KeyCode::Down, KeyModifiers::NONE);
            draw(&mut editor, 5, 3);
            assert!(editor.state.cursor() <= end_position(&editor.text()));
        }
        editor.insert("z").unwrap();
        assert_eq!(editor.text(), "ab界cdefghz");
    }

    #[test]
    fn tp2_application_keys_and_release_do_not_edit() {
        let mut editor = Editor::new();
        for key in [
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL),
            KeyEvent::new(KeyCode::Char('v'), KeyModifiers::CONTROL),
            KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT),
            KeyEvent::new_with_kind(
                KeyCode::Char('x'),
                KeyModifiers::NONE,
                KeyEventKind::Release,
            ),
        ] {
            assert!(!editor.key(key).unwrap());
        }
        assert!(editor.is_empty());
        assert!(
            editor
                .key(KeyEvent::new_with_kind(
                    KeyCode::Char('x'),
                    KeyModifiers::NONE,
                    KeyEventKind::Repeat
                ))
                .unwrap()
        );
        assert_eq!(editor.text(), "x");
    }
}
