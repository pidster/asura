//! Client-local help mode: the `?` switch, key routing, search panel and area
//! tracking. Content and search live in `tui/help.rs`. Nothing here sends a
//! request, records recall or touches a file.
use super::super::help::{self, Area, Kind, MAX_RESULTS, Query};
use super::*;
use crossterm::event::KeyEvent;

/// Marker for the first input row, and tint of the box and its strips.
pub(super) const HELP_INDIGO: Color = Color::Rgb(0x25, 0x2D, 0x4E);
const MAX_VISIBLE_ROWS: usize = 8;
const LABEL_WIDTH: usize = 7;
/// Prefix of the draft that search reads, so the `?` query never copies a full draft.
const QUERY_PREFIX_BYTES: usize = 256;

pub(super) enum Row {
    Entry(usize),
    Area(Area),
    Guide(&'static str),
}

pub(super) struct HelpMode {
    /// Area captured on entry; help mode cannot move focus.
    area: Area,
    query: Query,
    rows: Vec<Row>,
    /// Matches found, including those beyond the kept rows.
    total: usize,
    selected: usize,
    first: usize,
    /// Rows the last frame showed, for Page Up and Page Down.
    page: usize,
    /// The last frame had no room for the panel.
    hidden: bool,
    /// Rows exist for `query`; false until the first refresh.
    ready: bool,
}

impl App {
    /// Whether help mode owns the box. Used by styling and hints.
    pub(super) fn help_active(&self) -> bool {
        self.help.is_some()
    }

    fn help_may_start(&self) -> bool {
        self.help.is_none()
            && self.overlay.is_none()
            && self.capture.is_none()
            && self.picker.is_none()
            && self.response_selection.is_none()
            && !self.history_focus
            && self.focus == Focus::Editor
            // In command and outside mode `?` is shell text.
            && !self.mode.command()
            && self.editor.at_start()
    }

    fn start_help(&mut self) {
        // Entering help mode is an explicit edit and ends recall browsing.
        self.history.reset_navigation();
        self.help = Some(HelpMode {
            area: self.last_area,
            query: Query::default(),
            rows: Vec::new(),
            total: 0,
            selected: 0,
            first: 0,
            page: 4,
            hidden: false,
            ready: false,
        });
        self.help_sync();
    }

    /// `?` at byte 0 switches into help mode; it is never inserted as text.
    pub(super) fn help_entry_key(&mut self, key: KeyEvent) -> bool {
        if key.code != KeyCode::Char('?')
            || !(key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT)
            || !self.help_may_start()
        {
            return false;
        }
        self.start_help();
        true
    }

    /// A paste that begins with `?` at byte 0 enters help mode; the remainder
    /// becomes the query. Returns false when the paste is ordinary text.
    pub(super) fn help_entry_paste(&mut self, text: &str) -> bool {
        if !text.starts_with('?') || !self.help_may_start() {
            return false;
        }
        let rest = &text[1..];
        if !rest.is_empty()
            && let Err(error) = self.editor.insert(rest)
        {
            self.notice = error.to_string();
            return true;
        }
        self.start_help();
        true
    }

    /// Refresh results when the folded query changed. Selection keys never call this.
    pub(super) fn help_sync(&mut self) {
        let Some(help) = self.help.as_mut() else {
            return;
        };
        let query = Query::new(&self.editor.prefix(QUERY_PREFIX_BYTES));
        if help.ready && query == help.query {
            return;
        }
        help.ready = true;
        help.query = query;
        help.rows.clear();
        help.total = 0;
        help.selected = 0;
        help.first = 0;
        let Some(index) = help::index() else {
            return;
        };
        if help.query.is_blank() {
            help.rows.push(Row::Area(help.area));
            help.rows.extend(
                Area::ALL
                    .into_iter()
                    .filter(|area| *area != help.area)
                    .map(Row::Area),
            );
            help.rows
                .extend(help::GUIDE_ROWS.into_iter().map(Row::Guide));
        } else {
            let results = index.search(&help.query);
            help.total = results.total;
            help.rows = results.rows.into_iter().map(Row::Entry).collect();
        }
    }

    /// Keep the last-focused area for area help. Submitting from the box resets it.
    pub(super) fn track_area(&mut self) {
        let area = if matches!(self.focus, Focus::Queue(_)) {
            Some(Area::Queue)
        } else if matches!(self.focus, Focus::Project | Focus::Model) || self.picker.is_some() {
            Some(Area::StatusBar)
        } else if self.history_focus || self.response_selection.is_some() {
            Some(Area::Log)
        } else {
            None
        };
        if let Some(area) = area {
            self.last_area = area;
        }
    }

    /// Route one key while help mode is active and no overlay or capture owns
    /// keys. `None` leaves the key to the existing global handlers.
    pub(super) fn help_key(&mut self, key: KeyEvent) -> Option<bool> {
        let mods = key.modifiers;
        let plain = mods.is_empty();
        match key.code {
            KeyCode::Char('q' | 'c' | 'v') if mods == KeyModifiers::CONTROL => return None,
            KeyCode::F(1) => return None,
            KeyCode::Char('s' | 't' | 'd') if mods == KeyModifiers::CONTROL => {
                self.notice = "Leave help mode first".into();
            }
            // Recall, focus movement, completion and new rows do not run in help mode.
            KeyCode::Char('p' | 'n' | 'j') if mods == KeyModifiers::CONTROL => {}
            KeyCode::Up | KeyCode::Down
                if mods.intersects(KeyModifiers::SHIFT | KeyModifiers::CONTROL) => {}
            KeyCode::F(_) | KeyCode::Tab | KeyCode::BackTab => {}
            KeyCode::Enter if !plain => {}
            KeyCode::Esc if plain => self.help = None,
            KeyCode::Backspace if plain && self.editor.at_start() => self.help = None,
            KeyCode::Up | KeyCode::Down | KeyCode::PageUp | KeyCode::PageDown if plain => {
                self.help_select(key.code);
            }
            KeyCode::PageUp | KeyCode::PageDown => {}
            KeyCode::Enter => self.help_open(),
            _ => {
                if let Err(error) = self.editor.key(key) {
                    self.notice = error.to_string();
                }
                self.help_sync();
            }
        }
        Some(false)
    }

    fn help_select(&mut self, code: KeyCode) {
        let Some(help) = self.help.as_mut() else {
            return;
        };
        let last = help.rows.len().saturating_sub(1);
        let page = help.page.max(1);
        help.selected = match code {
            KeyCode::Up => help.selected.saturating_sub(1),
            KeyCode::Down => (help.selected + 1).min(last),
            KeyCode::PageUp => help.selected.saturating_sub(page),
            _ => (help.selected + page).min(last),
        };
    }

    /// Open the selected row as an overlay. Nothing is sent or recorded.
    fn help_open(&mut self) {
        let Some(help) = &self.help else {
            return;
        };
        let Some(index) = help::index() else {
            self.notice = help::UNAVAILABLE.into();
            return;
        };
        let Some(row) = help.rows.get(help.selected) else {
            self.notice = "No help matches".into();
            return;
        };
        let (title, markdown) = match row {
            Row::Entry(i) => (index.entry(*i).map(|e| e.title.clone()), index.markdown(*i)),
            Row::Area(area) => (Some(area.name().to_owned()), index.area_markdown(*area)),
            Row::Guide(id) => (
                index.title(id).map(str::to_owned),
                index.find(id).and_then(|i| index.markdown(i)),
            ),
        };
        let (Some(title), Some(markdown)) = (title, markdown) else {
            self.notice = help::UNAVAILABLE.into();
            return;
        };
        self.overlay = Some(Overlay::HelpEntry(
            literal(&title, 256),
            markdown::render(&markdown),
            0,
        ));
    }

    /// Hint row text for help mode, or `None` outside it.
    pub(super) fn help_hint(&self) -> Option<String> {
        let help = self.help.as_ref()?;
        if !help.hidden {
            return Some("↑↓ select · ↵ open · ⌫ at start or Esc leaves help".into());
        }
        let Some(index) = help::index() else {
            return Some(help::UNAVAILABLE.into());
        };
        let title = help
            .rows
            .get(help.selected)
            .and_then(|row| match row {
                Row::Entry(i) => index.entry(*i).map(|e| e.title.clone()),
                Row::Area(area) => Some(area.name().to_owned()),
                Row::Guide(id) => index.title(id).map(str::to_owned),
            })
            .unwrap_or_else(|| "No help matches".into());
        Some(format!("? {} · ↵ open", literal(&title, 120)))
    }

    /// Draw the panel where the queue panel would be. Returns its height; zero
    /// when the space cannot hold a header and one row.
    pub(super) fn draw_help_panel(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        bottom: u16,
        style: Style,
    ) -> u16 {
        let queued = self.queue_focus_ids().len();
        let Some(help) = self.help.as_mut() else {
            return 0;
        };
        let index = help::index();
        let available = usize::from(bottom.saturating_sub(area.y + 3));
        let count = if index.is_some() { help.rows.len() } else { 1 };
        // Padding goes first when space is short; result rows shrink before it.
        let padded = available >= 4;
        let shown = if available < 2 {
            0
        } else {
            count
                .min(MAX_VISIBLE_ROWS)
                .min(available - 1 - if padded { 2 } else { 0 })
        };
        help.hidden = available < 2;
        if help.hidden {
            return 0;
        }
        help.page = shown.max(1);
        help.selected = help.selected.min(count.saturating_sub(1));
        if help.selected < help.first {
            help.first = help.selected;
        } else if shown > 0 && help.selected >= help.first + shown {
            help.first = help.selected + 1 - shown;
        }
        help.first = help.first.min(count.saturating_sub(shown));
        let status = if index.is_none() {
            "Unavailable".to_owned()
        } else if help.query.is_blank() {
            "Area help".to_owned()
        } else if help.total == 0 {
            "No matches".to_owned()
        } else if help.total > MAX_RESULTS {
            format!("{MAX_RESULTS}+ results")
        } else {
            format!("{} results", help.total)
        };
        let mut header = format!("? Help · {status}");
        if count > shown && shown > 0 {
            header.push_str(&format!(" · {}/{count}", help.selected + 1));
        }
        if queued > 0 {
            header.push_str(&format!(" · {queued} queued"));
        }
        let pad = if padded { 2 } else { 0 };
        let height = (1 + shown + pad) as u16;
        let rect = Rect::new(area.x, bottom - height, area.width, height);
        frame.render_widget(Clear, rect);
        frame.render_widget(Paragraph::new("").style(style), rect);
        let inner = Rect::new(
            rect.x + 1,
            rect.y + u16::from(padded),
            rect.width.saturating_sub(2),
            1 + shown as u16,
        );
        line(frame, inner, 0, &header, style);
        let rows: Vec<(String, String, String)> = (help.first..help.first + shown)
            .map(|position| match (index, help.rows.get(position)) {
                (Some(index), Some(Row::Entry(i))) => {
                    index.entry(*i).map_or_else(Default::default, |entry| {
                        (
                            kind_label(entry.kind, entry.planned).into(),
                            entry.title.clone(),
                            entry.summary.clone(),
                        )
                    })
                }
                (Some(_), Some(Row::Area(area))) => (
                    "Area".into(),
                    if *area == help.area {
                        format!("{} (last used)", area.name())
                    } else {
                        area.name().into()
                    },
                    String::new(),
                ),
                (Some(index), Some(Row::Guide(id))) => (
                    "Guide".into(),
                    index.title(id).unwrap_or_default().into(),
                    String::new(),
                ),
                _ => (String::new(), help::UNAVAILABLE.into(), String::new()),
            })
            .collect();
        let wide = inner.width >= 60;
        let title_width = rows
            .iter()
            .map(|(_, title, _)| unicode_display_width::width(&literal(title, 512)) as usize)
            .max()
            .unwrap_or(0)
            .min(usize::from(inner.width).saturating_sub(2 + LABEL_WIDTH + 2 + 12));
        for (offset, (label, title, summary)) in rows.iter().enumerate() {
            let selected = help.first + offset == help.selected && index.is_some();
            let mark = if selected { "›" } else { " " };
            let title = literal(title, 512);
            let text = if wide {
                let title = shorten(&title, title_width);
                let fill =
                    title_width.saturating_sub(unicode_display_width::width(&title) as usize);
                format!(
                    "{mark} {label:<LABEL_WIDTH$}  {title}{}  {}",
                    " ".repeat(fill),
                    literal(summary, 512)
                )
            } else {
                format!("{mark} {label:<LABEL_WIDTH$}  {title}")
            };
            line(
                frame,
                inner,
                1 + offset as u16,
                &text,
                if selected {
                    selection_style(style)
                } else {
                    style
                },
            );
        }
        height
    }
}

/// Label of a result row; planned topics say so.
fn kind_label(kind: Kind, planned: bool) -> &'static str {
    if planned { "Planned" } else { kind.label() }
}

#[cfg(test)]
mod tests;
