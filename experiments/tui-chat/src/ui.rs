//! Terminal-cell geometry and presentation; no fixture state transitions.

use crate::app::{App, ComposerStatus, Overlay, action_label, message_state_label};
use crate::{
    model::{
        Action, GitOperation, GitReference, GitStatus, MessageState, MessageTray, MessageView,
        ModelStatus, SubmitError, TranscriptKind,
    },
    viewport::{EntryLayout, LineKind},
};
use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
};
use unicode_segmentation::UnicodeSegmentation;

#[derive(Clone, Copy)]
pub struct Palette {
    pub text: Color,
    pub muted: Color,
    pub band: Color,
    pub user_band: Color,
    pub accent: Color,
    pub base: Color,
    pub project: Color,
    pub added: Color,
    pub removed: Color,
}

impl Palette {
    pub fn new(light: bool) -> Self {
        if light {
            Self {
                text: Color::Rgb(30, 41, 59),
                muted: Color::Rgb(71, 85, 105),
                band: Color::Rgb(226, 232, 240),
                user_band: Color::Rgb(212, 222, 234),
                accent: Color::Rgb(29, 78, 216),
                base: Color::Reset,
                project: Color::Rgb(14, 116, 144),
                added: Color::Rgb(21, 128, 61),
                removed: Color::Rgb(185, 28, 28),
            }
        } else {
            Self {
                text: Color::Rgb(230, 237, 240),
                muted: Color::Rgb(134, 174, 200),
                band: Color::Rgb(37, 59, 78),
                user_band: Color::Rgb(28, 46, 61),
                accent: Color::Rgb(207, 241, 255),
                base: Color::Reset,
                project: Color::Rgb(143, 211, 244),
                added: Color::Rgb(134, 239, 172),
                removed: Color::Rgb(252, 165, 165),
            }
        }
    }
}

#[derive(Debug)]
pub struct Geometry {
    pub transcript: Rect,
    pub suggestions: Rect,
    pub destination: Rect,
    pub notice: Rect,
    pub tray: Rect,
    pub upper: Rect,
    pub editor: Rect,
    pub lower: Rect,
    pub status: Rect,
    pub small: bool,
}

pub fn geometry(area: Rect, requested: u16) -> Geometry {
    geometry_with_tray(area, requested, 0)
}

pub fn geometry_with_tray(area: Rect, requested: u16, retained: usize) -> Geometry {
    composer_geometry(area, requested, retained, true, false)
}

/// Allocate the same delivered messages and meaningful notices as the renderer.
pub fn app_geometry(app: &App, area: Rect) -> Geometry {
    let items = inline_messages(app.fixture.message_tray());
    let notice = composer_notice(app, app.viewport.is_reading());
    let suggestions = app.command_suggestions();
    composer_geometry_with_suggestions(
        area,
        app.editor.visual_rows(area.width.saturating_sub(4)),
        items.len(),
        !notice.is_empty(),
        app.unread_output
            || decision_cue(app)
            || suggestions.is_some_and(|view| view.cue.is_some()),
        suggestions.map_or(0, |view| view.rows.len()),
    )
}

fn composer_geometry(
    area: Rect,
    requested: u16,
    relevant: usize,
    show_notice: bool,
    show_cue: bool,
) -> Geometry {
    composer_geometry_with_suggestions(area, requested, relevant, show_notice, show_cue, 0)
}

fn composer_geometry_with_suggestions(
    area: Rect,
    requested: u16,
    relevant: usize,
    show_notice: bool,
    show_cue: bool,
    suggested: usize,
) -> Geometry {
    let small = area.width < 30 || area.height < 8;
    let strips = u16::from(area.width >= 60 && area.height >= 16);
    let notice_height = u16::from(show_notice);
    let fixed = 2 + notice_height + 2 * strips;
    let allocate = |cue_height| {
        let editor_height = requested
            .clamp(1, if strips == 1 { 6 } else { 3 })
            .min(area.height.saturating_sub(fixed + cue_height + 3).max(1));
        let available = area
            .height
            .saturating_sub(fixed + cue_height + editor_height);
        let capacity = available
            .saturating_sub(3)
            .min(if strips == 1 { 4 } else { 2 });
        (editor_height, available, capacity)
    };
    let (_, _, capacity) = allocate(u16::from(show_cue));
    let collapsed = relevant > 0 && capacity < if relevant > 1 { 2 } else { 1 };
    let cue_height = u16::from(show_cue || collapsed);
    let (editor_height, available, tray_capacity) = allocate(cue_height);
    let tray_height = if collapsed {
        0
    } else {
        tray_capacity.min(u16::try_from(relevant).unwrap_or(u16::MAX))
    };
    let transcript_height = available.saturating_sub(tray_height);
    let destination_y = area.y.saturating_add(1 + transcript_height);
    let notice_y = destination_y.saturating_add(cue_height);
    let tray_y = notice_y.saturating_add(notice_height);
    let upper_y = tray_y.saturating_add(tray_height);
    let editor_y = upper_y.saturating_add(strips);
    let lower_y = editor_y.saturating_add(editor_height);
    let rect = |y, height| Rect::new(area.x, y, area.width, height).intersection(area);
    let mut geometry = Geometry {
        transcript: rect(area.y.saturating_add(1), transcript_height),
        suggestions: rect(destination_y, 0),
        destination: rect(destination_y, cue_height),
        notice: rect(notice_y, notice_height),
        tray: rect(tray_y, tray_height),
        upper: rect(upper_y, strips),
        editor: rect(editor_y, editor_height),
        lower: rect(lower_y, strips),
        status: rect(lower_y.saturating_add(strips), 1),
        small,
    };
    // Suggestions borrow only spare transcript rows. Pending messages, the
    // editor and its status keep their existing allocation at every size.
    if !small {
        let rows = u16::try_from(suggested)
            .unwrap_or(u16::MAX)
            .min(2)
            .min(geometry.transcript.height.saturating_sub(3));
        geometry.transcript.height -= rows;
        geometry.suggestions = rect(geometry.transcript.bottom(), rows);
    }
    geometry
}

fn decision_cue(app: &App) -> bool {
    !app.notice.is_empty()
        && app
            .fixture
            .display_task()
            .is_some_and(|task| task.decision.is_some())
}

/// This association uses only delivered identities, including frozen stale
/// Running rows. The authoritative task may already have finished or changed.
fn inline_messages(tray: MessageTray) -> Vec<MessageView> {
    let running: Vec<_> = tray
        .items
        .iter()
        .filter(|item| item.state == MessageState::Running)
        .filter_map(|item| item.execution_task.map(|task| (item.target.scope, task.id)))
        .collect();
    tray.items
        .into_iter()
        .filter(|item| match item.state {
            MessageState::Pending
            | MessageState::Unknown
            | MessageState::Held
            | MessageState::Queued => true,
            MessageState::Acknowledged if item.action == Action::Steer => item
                .target
                .task
                .is_some_and(|task| running.contains(&(item.target.scope, task.id))),
            _ => false,
        })
        .collect()
}

fn composer_notice(app: &App, reading: bool) -> String {
    if app.paste_mode || app.capture_error.is_some() {
        return app.capture_error.as_ref().map_or_else(
            || format!("{} bytes · Ctrl+V review", app.captured.len()),
            |error| format!("Ctrl+V review · {error}"),
        );
    }
    if !app.notice.is_empty() {
        return app.notice.clone();
    }
    let attention_count = app.attention().len();
    let failure = admission_notice(app);
    let (context, action) = if !app.fixture.connected {
        ("Disconnected · F2 reconnects".into(), Some("F2 reconnects"))
    } else if app.fixture.pending.is_some() && app.manual {
        ("F5 accepts".into(), Some("F5 accepts"))
    } else if app.fixture.delivery_blocked() {
        ("State unavailable · F2".into(), Some("F2 reconnects"))
    } else if let Some(failure) = failure {
        (failure.into(), Some("Ctrl+O reviews"))
    } else if attention_count > 1 {
        (
            format!("{attention_count} attention · ^O reviews"),
            Some("Ctrl+O reviews"),
        )
    } else if let Some(recoverable) = app.fixture.display_recoverable()
        && !recoverable.is_empty()
    {
        (
            format!("{} held · Ctrl+O recover", recoverable.len()),
            Some("Ctrl+O recover"),
        )
    } else if app
        .fixture
        .display_task()
        .is_some_and(|task| task.decision.is_some())
    {
        ("Decision · Ctrl+O reviews".into(), Some("Ctrl+O reviews"))
    } else if reading {
        ("Reading earlier · Ctrl+E".into(), Some("Ctrl+E latest"))
    } else if app.fixture.truncated {
        ("Earlier output trimmed.".into(), None)
    } else {
        (String::new(), None)
    };
    if app.fixture.reject_armed && !app.fixture.delivery_blocked() && failure.is_none() {
        match action {
            Some(action) => format!("Reject next · {action}"),
            None if app.fixture.truncated => "Reject next · Output trimmed".into(),
            None => "Next send will be rejected".into(),
        }
    } else {
        context
    }
}

fn admission_notice(app: &App) -> Option<&'static str> {
    if app.draft_limit_reached() {
        return Some("Draft limit · reset trial");
    }
    let action = if app.fixture.display_task().is_some() {
        Action::Steer
    } else {
        Action::NewTurn
    };
    match app.fixture.can_submit(action) {
        Err(SubmitError::Capacity) => Some(if app.attention().is_empty() {
            "8 protected · capacity full"
        } else {
            "Capacity full · Ctrl+O"
        }),
        Err(SubmitError::IdentityExhausted) => Some("IDs exhausted · restart"),
        _ => None,
    }
}

fn inset(rect: Rect, left: u16) -> Rect {
    Rect::new(
        rect.x.saturating_add(left.min(rect.width)),
        rect.y,
        rect.width.saturating_sub(left + 1),
        rect.height,
    )
}

pub fn draw(frame: &mut Frame<'_>, app: &mut App, light: bool) {
    draw_with_overlay_hint(frame, app, light, " Esc closes · PgUp/PgDn ");
}

/// Uses the same layout while allowing a locked tour to advertise its own controls.
pub fn draw_with_overlay_hint(frame: &mut Frame<'_>, app: &mut App, light: bool, hint: &str) {
    let area = frame.area();
    draw_in_area(frame, app, light, area, hint);
}

/// Draw the ordinary interface in a translated region, including cursor and overlays.
pub fn draw_in_area(frame: &mut Frame<'_>, app: &mut App, light: bool, area: Rect, hint: &str) {
    let area = area.intersection(frame.area());
    app.width = area.width;
    app.height = area.height;
    app.record_presentation();
    let command_suggestions = app.command_suggestions().cloned();
    let p = Palette::new(light);
    let base = Style::default().fg(p.text).bg(p.base);
    frame.render_widget(Block::default().style(base), area);
    app.observe_transcript(app.viewport.is_reading());
    let items = inline_messages(app.fixture.message_tray());
    let notice = composer_notice(app, app.viewport.is_reading());
    let g = composer_geometry_with_suggestions(
        area,
        app.editor.visual_rows(area.width.saturating_sub(4)),
        items.len(),
        !notice.is_empty(),
        app.unread_output
            || decision_cue(app)
            || command_suggestions
                .as_ref()
                .is_some_and(|view| view.cue.is_some()),
        command_suggestions
            .as_ref()
            .map_or(0, |view| view.rows.len()),
    );
    if g.small {
        let text = match &app.overlay {
            Some(Overlay::Exit { selected: true }) => {
                "Discard all text and exit?\nEnter confirms · Esc cancels"
            }
            Some(Overlay::Exit { selected: false }) => {
                "Unsaved text. Discard and exit?\nTab selects · Enter confirms · Esc cancels"
            }
            _ => "Resize to 30 × 8 or larger.\nDraft retained. Ctrl+Q requests exit.",
        };
        frame.render_widget(
            Paragraph::new(text).style(base).wrap(Wrap { trim: false }),
            area,
        );
        return;
    }
    let other = app.other_fixture();
    let background = if other.delivery_blocked() {
        "state unavailable"
    } else if other
        .display_recoverable()
        .is_some_and(|items| !items.is_empty())
    {
        "text held"
    } else if other.pending.is_some() {
        "pending"
    } else if other.display_task().is_some_and(|t| t.decision.is_some()) {
        "decision"
    } else if other.display_task().is_some() {
        "working"
    } else {
        "idle"
    };
    let other_name = match app.project() {
        crate::model::Project::Studio => "Observatory",
        crate::model::Project::Observatory => "Studio",
    };
    let header = Line::from(vec![
        Span::styled(
            " ASURA ",
            Style::default().fg(p.accent).add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("prototype · Ctrl+P {other_name}: {background}"),
            Style::default().fg(p.muted),
        ),
    ]);
    frame.render_widget(
        Paragraph::new(header),
        Rect::new(area.x, area.y, area.width, 1),
    );
    // Pending text has a scoped inspector, not a borrowed transcript identity.
    // Only delivered entries participate in the positional reading anchor.
    let entries: Vec<_> = app.fixture.transcript.iter().collect();
    let layouts: Vec<_> = entries
        .iter()
        .enumerate()
        .map(|(index, entry)| {
            let user = matches!(entry.kind, TranscriptKind::User(_));
            let padded = user && g.upper.height > 0;
            let next_padded = g.upper.height > 0
                && entries
                    .get(index + 1)
                    .is_some_and(|next| matches!(next.kind, TranscriptKind::User(_)));
            EntryLayout::new(
                &entry.text,
                inset(g.transcript, if user { 3 } else { 1 }).width,
                padded,
                !padded && !next_padded,
            )
        })
        .collect();
    let plan = app
        .viewport
        .plan(&layouts, app.fixture.base_id(), g.transcript.height);
    if plan.anchor_evicted {
        app.notice = "Reading trimmed · Ctrl+E".into();
    }
    let mut y = g.transcript.y;
    'entries: for (index, (entry, layout)) in entries
        .iter()
        .zip(&layouts)
        .enumerate()
        .skip(plan.start_entry)
    {
        let user = matches!(entry.kind, TranscriptKind::User(_));
        let start = if index == plan.start_entry {
            plan.line_offset
        } else {
            0
        };
        for (line_index, line) in layout.lines.iter().enumerate().skip(start) {
            if y >= g.transcript.bottom() {
                break 'entries;
            }
            let skip = if index == plan.start_entry && line_index == start {
                plan.row_offset
            } else {
                0
            };
            let height = line
                .rows
                .saturating_sub(usize::from(skip))
                .min(usize::from(g.transcript.bottom() - y)) as u16;
            let row = Rect::new(g.transcript.x, y, g.transcript.width, height);
            match line.kind {
                LineKind::Upper | LineKind::Lower => {
                    let glyph = if line.kind == LineKind::Upper {
                        "▄"
                    } else {
                        "▀"
                    };
                    frame.render_widget(
                        Paragraph::new(glyph.repeat(usize::from(row.width)))
                            .style(Style::default().fg(p.user_band).bg(p.base)),
                        row,
                    );
                }
                LineKind::Text => {
                    let style =
                        Style::default()
                            .fg(p.text)
                            .bg(if user { p.user_band } else { p.base });
                    frame.render_widget(Block::default().style(style), row);
                    frame.render_widget(
                        Paragraph::new(line.text)
                            .style(style)
                            .wrap(Wrap { trim: false })
                            .scroll((skip, 0)),
                        inset(row, if user { 3 } else { 1 }),
                    );
                    // Paragraph resets the continuation cells of wide glyphs.
                    // Restore the band's background without altering glyphs or
                    // foreground styles, including those otherwise empty cells.
                    frame.buffer_mut().set_style(
                        row,
                        Style::default().bg(if user { p.user_band } else { p.base }),
                    );
                    if user && line_index == usize::from(g.upper.height > 0) && skip == 0 {
                        frame.render_widget(
                            Paragraph::new("›").style(style.fg(p.accent)),
                            Rect::new(row.x + 1, row.y, 1, 1),
                        );
                    }
                }
                LineKind::Separator => {}
            }
            y += height;
        }
    }
    app.observe_transcript(plan.reading);
    if let Some(view) = &command_suggestions {
        for (index, row) in view
            .rows
            .iter()
            .take(usize::from(g.suggestions.height))
            .enumerate()
        {
            let rect = inset(
                Rect::new(
                    g.suggestions.x,
                    g.suggestions.y + index as u16,
                    g.suggestions.width,
                    1,
                ),
                1,
            );
            frame.render_widget(
                Paragraph::new(message_excerpt("", row, rect.width))
                    .style(Style::default().fg(p.muted)),
                rect,
            );
        }
    }
    let mut cues = Vec::new();
    if app.unread_output {
        cues.push("New output".to_owned());
    }
    if g.tray.height == 0 && !items.is_empty() {
        cues.push(format!("{} ^B", items.len()));
    }
    if decision_cue(app) {
        cues.push(
            if cues.is_empty() {
                "Decision · Ctrl+O"
            } else {
                "^O"
            }
            .to_owned(),
        );
    }
    if cues.is_empty()
        && let Some(cue) = command_suggestions
            .as_ref()
            .and_then(|view| view.cue.as_deref())
    {
        cues.push(cue.to_owned());
    }
    frame.render_widget(
        Paragraph::new(cues.join(" · ")).style(Style::default().fg(p.accent)),
        inset(g.destination, 1),
    );
    // Planning can discover an expired reading anchor and replace its notice.
    // The pre-layout reading accessor has already reserved the necessary row.
    let notice = if plan.anchor_evicted {
        composer_notice(app, plan.reading)
    } else {
        notice
    };
    frame.render_widget(
        Paragraph::new(notice).style(Style::default().fg(p.muted)),
        inset(g.notice, 1),
    );
    if g.tray.height > 0 {
        let heading_rows = u16::from(items.len() > usize::from(g.tray.height));
        let shown = usize::from(g.tray.height - heading_rows).min(items.len());
        if heading_rows > 0 {
            let hidden = items.len() - shown;
            let heading = if area.width < 60 {
                format!("{hidden} hidden · ^B")
            } else {
                format!("{hidden} hidden · Ctrl+B")
            };
            frame.render_widget(
                Paragraph::new(heading).style(Style::default().fg(p.muted)),
                inset(Rect::new(g.tray.x, g.tray.y, g.tray.width, 1), 1),
            );
        }
        for (index, item) in items.iter().take(shown).enumerate() {
            let row = inset(
                Rect::new(
                    g.tray.x,
                    g.tray.y + heading_rows + index as u16,
                    g.tray.width,
                    1,
                ),
                1,
            );
            let action = if item.action == Action::Queue && item.state == MessageState::Queued {
                String::new()
            } else {
                format!("{} · ", action_label(item.action))
            };
            let prefix = format!(
                "{action}{}{}: ",
                message_state_label(item.state),
                if item.stale { " (stale)" } else { "" }
            );
            frame.render_widget(
                Paragraph::new(message_excerpt(&prefix, &item.text, row.width))
                    .style(Style::default().fg(p.text)),
                row,
            );
        }
    }
    for (rect, glyph) in [(g.upper, "▄"), (g.lower, "▀")] {
        frame.render_widget(
            Paragraph::new(glyph.repeat(usize::from(rect.width)))
                .style(Style::default().fg(p.band).bg(p.base)),
            rect,
        );
    }
    frame.render_widget(
        Block::default().style(Style::default().bg(p.band)),
        g.editor,
    );
    frame.render_widget(
        Paragraph::new("›").style(Style::default().fg(p.accent).bg(p.band)),
        inset(g.editor, 1),
    );
    app.editor.render(
        inset(g.editor, 3),
        frame.buffer_mut(),
        Style::default().fg(p.text).bg(p.band),
        app.overlay.is_none(),
    );
    if app.overlay.is_none()
        && let Some(cursor) = app.editor.cursor()
    {
        frame.set_cursor_position(cursor);
    }
    draw_status(
        frame,
        app.composer_status(),
        inset(g.status, 1),
        area.width < 60,
        p,
    );
    if let Some(view) = app.overlay_view() {
        let width = area.width.saturating_sub(4).min(76);
        let height = area.height.saturating_sub(2).min(15);
        let popup = Rect::new(
            area.x + (area.width - width) / 2,
            area.y + (area.height - height) / 2,
            width,
            height,
        );
        frame.render_widget(Clear, popup);
        let block = Block::default()
            .borders(Borders::ALL)
            .title(format!(" {} ", view.title))
            .title_bottom(hint)
            .border_style(Style::default().fg(p.accent))
            .style(base);
        let inner = block.inner(popup);
        frame.render_widget(block, popup);
        // Actions have their own viewport. Long details cannot scroll a selected
        // action away, including the last recovery in a compact attention list.
        let command_browser = matches!(app.overlay, Some(Overlay::CommandBrowser { .. }));
        let max_actions = if command_browser && inner.height <= 5 {
            // At the 30 × 8 minimum, keep the selected row visible while
            // leaving room for its full source and availability in details.
            1
        } else {
            usize::from(inner.height.saturating_sub(1))
        };
        let action_height = max_actions.min(view.actions.len());
        let start_action = view
            .selected
            .unwrap_or(0)
            .saturating_sub(action_height.saturating_sub(1));
        let actions: Vec<Line<'_>> = view
            .actions
            .iter()
            .enumerate()
            .skip(start_action)
            .take(action_height)
            .map(|(index, action)| {
                Line::from(message_excerpt(
                    "",
                    &format!(
                        "{} {action}",
                        if view.selected == Some(index) {
                            "›"
                        } else {
                            " "
                        }
                    ),
                    inner.width,
                ))
            })
            .collect();
        let action_rect = Rect::new(inner.x, inner.y, inner.width, action_height as u16);
        frame.render_widget(Paragraph::new(actions).style(base), action_rect);
        let details = Rect::new(
            inner.x,
            inner.y + action_height as u16,
            inner.width,
            inner.height.saturating_sub(action_height as u16),
        );
        let detail = if app.notice.is_empty() {
            view.detail
        } else {
            format!("{}\n{}", app.notice, view.detail)
        };
        let mut text = Text::from(detail);
        let content = Paragraph::new(text.clone()).wrap(Wrap { trim: false });
        let max_scroll = content
            .line_count(details.width)
            .saturating_sub(usize::from(details.height));
        app.overlay_scroll = app.overlay_scroll.min(max_scroll);
        let mut remaining = app.overlay_scroll;
        let mut skip = 0;
        for line in &text.lines {
            let rows = Paragraph::new(line.clone())
                .wrap(Wrap { trim: false })
                .line_count(details.width);
            if remaining < rows {
                break;
            }
            remaining -= rows;
            skip += 1;
        }
        text.lines.drain(..skip);
        // Each bounded logical line fits u16 wrapped rows at minimum width;
        // the global logical-line offset may exceed it and is skipped above.
        let row = u16::try_from(remaining).unwrap_or(u16::MAX);
        frame.render_widget(
            Paragraph::new(text)
                .wrap(Wrap { trim: false })
                .scroll((row, 0))
                .style(base),
            details,
        );
    }
}

/// Status metadata is literal, bounded single-line text, never terminal syntax.
fn metadata(text: &str, max_bytes: usize) -> String {
    let mut end = text.len().min(max_bytes);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let mut clusters = text[..end].graphemes(true);
    if end < text.len() {
        clusters.next_back();
    }
    clusters
        .flat_map(str::chars)
        .map(|ch| {
            if ch.is_control() || matches!(ch, '\u{2028}' | '\u{2029}') {
                ' '
            } else {
                ch
            }
        })
        .collect()
}

fn cell_width(text: &str) -> usize {
    usize::try_from(unicode_display_width::width(text)).unwrap_or(usize::MAX)
}

fn shorten(text: &str, cells: usize) -> String {
    if cell_width(text) <= cells {
        return text.into();
    }
    if cells == 0 {
        return String::new();
    }
    let mut output = String::new();
    let mut width = 0;
    for cluster in text.graphemes(true) {
        let next = cell_width(cluster);
        if width + next > cells - 1 {
            break;
        }
        output.push_str(cluster);
        width += next;
    }
    output.push('…');
    output
}

#[derive(Clone, Copy)]
enum StatusRole {
    Project,
    Added,
    Removed,
    Muted,
}

#[derive(Clone, Copy)]
enum FieldSeparator {
    Dot,
    Space,
    None,
}

impl FieldSeparator {
    fn text(self) -> &'static str {
        match self {
            Self::Dot => " · ",
            Self::Space => " ",
            Self::None => "",
        }
    }

    fn width(self) -> usize {
        match self {
            Self::Dot => 3,
            Self::Space => 1,
            Self::None => 0,
        }
    }
}

struct StatusField {
    text: String,
    role: StatusRole,
    separator: FieldSeparator,
}

impl StatusField {
    fn new(text: impl Into<String>, role: StatusRole) -> Self {
        Self {
            text: text.into(),
            role,
            separator: FieldSeparator::Dot,
        }
    }

    fn muted(text: impl Into<String>) -> Self {
        Self::new(text, StatusRole::Muted)
    }

    fn attached(mut self) -> Self {
        self.separator = FieldSeparator::Space;
        self
    }

    fn joined(mut self) -> Self {
        self.separator = FieldSeparator::None;
        self
    }
}

struct StatusGroup(Vec<StatusField>);

impl StatusGroup {
    fn new(fields: Vec<StatusField>) -> Self {
        Self(
            fields
                .into_iter()
                .filter(|field| !field.text.is_empty())
                .collect(),
        )
    }

    fn width(&self) -> usize {
        self.0
            .iter()
            .enumerate()
            .map(|(index, field)| {
                cell_width(&field.text)
                    + if index == 0 {
                        0
                    } else {
                        field.separator.width()
                    }
            })
            .sum()
    }

    fn following_width(&self) -> usize {
        self.width() + self.0.first().map_or(0, |field| field.separator.width())
    }

    fn line(self, palette: Palette) -> Line<'static> {
        let muted = Style::default().fg(palette.muted).bg(palette.base);
        let mut spans = Vec::with_capacity(self.0.len() * 2);
        for field in self.0 {
            if !spans.is_empty() {
                spans.push(Span::styled(field.separator.text(), muted));
            }
            let foreground = match field.role {
                StatusRole::Project => palette.project,
                StatusRole::Added => palette.added,
                StatusRole::Removed => palette.removed,
                StatusRole::Muted => palette.muted,
            };
            spans.push(Span::styled(field.text, muted.fg(foreground)));
        }
        Line::from(spans)
    }

    #[cfg(test)]
    fn text(&self) -> String {
        let mut result = String::new();
        for (index, field) in self.0.iter().enumerate() {
            if index > 0 {
                result.push_str(field.separator.text());
            }
            result.push_str(&field.text);
        }
        result
    }
}

fn model_group(model: ModelStatus, compact: bool, budget: usize) -> StatusGroup {
    let percentage = model
        .context_used_percent
        .filter(|value| *value <= 100)
        .map_or_else(|| "?".into(), |value| value.to_string());
    let percentage = format!("{percentage}%{}", if compact { "" } else { " used" });
    let fast = if model.fast {
        if compact { "F" } else { "fast" }
    } else {
        ""
    };
    let identity = format!(
        "{} {}",
        metadata(model.name, 256),
        metadata(model.version, 256)
    );
    let fixed = cell_width(&percentage) + 3 + if model.fast { 1 + cell_width(fast) } else { 0 };
    StatusGroup::new(vec![
        StatusField::muted(percentage),
        StatusField::muted(shorten(&identity, budget.saturating_sub(fixed))),
        StatusField::muted(fast).attached(),
    ])
}

fn draft_fields(status: ComposerStatus, compact: bool) -> Vec<StatusField> {
    let mut fields = Vec::new();
    if status.characters > 0 {
        fields.push(StatusField::muted(format!(
            "{}{}",
            status.characters,
            if compact { "c" } else { " chars" }
        )));
    }
    if let Some(spinner) = status.spinner {
        fields.push(StatusField::muted(spinner.to_string()).attached());
    }
    fields
}

fn git_fields(git: GitStatus, counts: bool, branch: bool) -> Vec<StatusField> {
    match git {
        GitStatus::Unavailable => vec![StatusField::muted("git ?")],
        GitStatus::NotRepository => Vec::new(),
        GitStatus::Available {
            reference,
            added,
            removed,
            operation,
        } => {
            let mut fields = Vec::new();
            if branch {
                let reference = match reference {
                    GitReference::Branch(name) => metadata(name, 256),
                    GitReference::Detached(id) => format!("detached:{}", metadata(id, 256)),
                };
                fields.push(StatusField::muted(reference));
            }
            if counts {
                fields.push(StatusField::new(format!("+{added}"), StatusRole::Added));
                fields.push(StatusField::new(format!("-{removed}"), StatusRole::Removed).joined());
            }
            if let Some(operation) = operation {
                fields.push(StatusField::muted(match operation {
                    GitOperation::Merge => "merge",
                    GitOperation::Rebase => "rebase",
                }));
            }
            fields
        }
    }
}

fn project_tail(status: ComposerStatus, compact: bool, counts: bool, branch: bool) -> StatusGroup {
    let mut fields = if compact {
        Vec::new()
    } else {
        git_fields(status.project.git, counts, branch)
    };
    fields.extend(draft_fields(status, compact));
    StatusGroup::new(fields)
}

fn project_group(status: ComposerStatus, compact: bool, budget: usize) -> StatusGroup {
    let project = status.project.project.name();
    if !compact {
        let path = metadata(status.project.path, 1024);
        // Shorten the path first; omit counts, then branch only if even a
        // one-cell path cannot coexist with the other requested fields.
        for (counts, branch) in [(true, true), (false, true), (false, false)] {
            let tail = project_tail(status, false, counts, branch);
            let fixed = cell_width(project) + 3 + tail.following_width();
            if budget > fixed {
                let mut fields = vec![
                    StatusField::new(project, StatusRole::Project),
                    StatusField::muted(shorten(&path, budget - fixed)),
                ];
                fields.extend(tail.0);
                return StatusGroup::new(fields);
            }
        }
    }
    let tail = project_tail(status, compact, false, false);
    let remaining = budget.saturating_sub(tail.following_width());
    let mut fields = vec![StatusField::new(
        shorten(project, remaining),
        StatusRole::Project,
    )];
    fields.extend(tail.0);
    StatusGroup::new(fields)
}

fn status_groups(
    status: ComposerStatus,
    compact: bool,
    available: usize,
) -> (StatusGroup, StatusGroup) {
    if available < 3 {
        return (
            StatusGroup::new(vec![StatusField::new(
                shorten(status.project.project.name(), available),
                StatusRole::Project,
            )]),
            StatusGroup::new(Vec::new()),
        );
    }
    let full_right = model_group(status.project.model, compact, usize::MAX);
    let full_left = project_group(status, compact, usize::MAX);
    let tail = project_tail(status, compact, false, false);
    let project = status.project.project.name();
    let project_cue = project.graphemes(true).next().map_or(0, cell_width) + 1;
    let left_minimum = project_cue.min(cell_width(project)) + tail.following_width();
    let right_budget = if full_left.width() + 2 + full_right.width() <= available {
        full_right.width()
    } else {
        full_right
            .width()
            .min(available / 2)
            .min(available.saturating_sub(left_minimum + 2))
    };
    let right = model_group(status.project.model, compact, right_budget);
    let left_budget = available.saturating_sub(right.width() + 2);
    (project_group(status, compact, left_budget), right)
}

fn draw_status(
    frame: &mut Frame<'_>,
    status: ComposerStatus,
    area: Rect,
    compact: bool,
    palette: Palette,
) {
    let (left, right) = status_groups(status, compact, usize::from(area.width));
    let right_width = u16::try_from(right.width())
        .unwrap_or(area.width)
        .min(area.width);
    let left_width = area
        .width
        .saturating_sub(right_width + if right_width > 0 { 2 } else { 0 });
    let style = Style::default().fg(palette.muted).bg(palette.base);
    frame.render_widget(
        Paragraph::new(left.line(palette)).style(style),
        Rect::new(area.x, area.y, left_width, area.height),
    );
    frame.render_widget(
        Paragraph::new(right.line(palette)).style(style),
        Rect::new(area.right() - right_width, area.y, right_width, area.height),
    );
}

/// Only visit the visible prefix of each excerpt. Arc-backed full messages are
/// never flattened or copied for normal tray paints.
fn message_excerpt(prefix: &str, text: &str, cells: u16) -> String {
    // Unicode permits arbitrarily long combining clusters and zero-cell text.
    // Bound segmentation work as well as displayed width. If a byte prefix is
    // cut, omit its final cluster because the unseen suffix might extend it.
    const PREFIX_BYTES: usize = 1024;
    let mut end = text.len().min(PREFIX_BYTES);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let capped = end < text.len();
    let mut graphemes = text[..end].graphemes(true);
    if capped {
        graphemes.next_back();
    }
    let limit = u64::from(cells);
    let mut result = String::new();
    let mut width = 0;
    let mut previous = None;
    let mut truncated = capped;
    for grapheme in prefix.graphemes(true).chain(graphemes) {
        let displayed = if grapheme.contains(['\n', '\r']) {
            " "
        } else {
            grapheme
        };
        let next = unicode_display_width::width(displayed);
        if width + next > limit {
            truncated = true;
            break;
        }
        if next > 0 {
            previous = Some((result.len(), width));
        }
        result.push_str(displayed);
        width += next;
    }
    if truncated {
        if width == limit
            && let Some((bytes, before)) = previous
        {
            result.truncate(bytes);
            width = before;
        }
        if width < limit {
            result.push('…');
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
    use ratatui::{Terminal, backend::TestBackend};

    fn status_text(status: ComposerStatus, compact: bool, available: usize) -> (String, String) {
        let (left, right) = status_groups(status, compact, available);
        (left.text(), right.text())
    }

    fn key(app: &mut App, code: KeyCode, modifiers: KeyModifiers) {
        app.handle(Event::Key(KeyEvent::new(code, modifiers)));
    }

    fn visible(app: &mut App, width: u16, height: u16, light: bool) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| draw(frame, app, light)).unwrap();
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

    fn working() -> App {
        let mut app = App::new(true);
        app.handle(Event::Paste("fixture task".into()));
        key(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        key(&mut app, KeyCode::F(5), KeyModifiers::NONE);
        app
    }

    #[test]
    fn ps1_four_states_have_one_project_footer_and_right_aligned_model() {
        for light in [false, true] {
            for running in [false, true] {
                for drafting in [false, true] {
                    let mut app = if running { working() } else { App::new(true) };
                    if drafting {
                        app.handle(Event::Paste("e\u{301} 👩🏽‍💻\n".into()));
                    }
                    let screen = visible(&mut app, 120, 40, light);
                    let footer = screen.lines().last().unwrap();
                    assert!(
                        footer.starts_with(" Studio · ~/src/studio · main · +24-8"),
                        "{footer}"
                    );
                    assert!(footer.ends_with("18% used · demo v1 fast "), "{footer}");
                    assert_eq!(footer.contains(" · 4 chars"), drafting, "{footer}");
                    assert_eq!(footer.contains('⠋'), running, "{footer}");
                    assert!(!screen.contains("Studio / Conversation"), "{screen}");
                    assert_eq!(
                        app_geometry(&app, Rect::new(0, 0, 120, 40))
                            .destination
                            .height,
                        0
                    );
                }
            }
        }
    }

    #[test]
    fn ps2_git_unknown_clean_detached_operations_and_invalid_utilization_are_distinct() {
        let mut status = App::new(true).composer_status();
        status.project.git = GitStatus::Unavailable;
        status.project.model.context_used_percent = Some(101);
        let (left, right) = status_text(status, false, 118);
        assert!(left.contains("git ?"), "{left}");
        assert!(right.starts_with("?% used"), "{right}");
        status.project.git = GitStatus::NotRepository;
        assert!(!status_text(status, false, 118).0.contains('['));
        for operation in [None, Some(GitOperation::Merge), Some(GitOperation::Rebase)] {
            status.project.git = GitStatus::Available {
                reference: GitReference::Detached("a1b2c3"),
                added: 0,
                removed: 0,
                operation,
            };
            let (left, _) = status_text(status, false, 118);
            assert!(left.contains("detached:a1b2c3"), "{left}");
            let expected = match operation {
                None => "+0-0",
                Some(GitOperation::Merge) => "+0-0 · merge",
                Some(GitOperation::Rebase) => "+0-0 · rebase",
            };
            assert!(left.contains(expected), "{left}");
        }
    }

    #[test]
    fn ps3_fitting_preserves_count_activity_operation_and_model_in_separate_cells() {
        let mut status = App::new(true).composer_status();
        status.project.project = crate::model::Project::Observatory;
        status.project.path = "~/src/界👩🏽‍💻e\u{301}/a-very-long-workspace-path/with/more/components";
        status.project.model.name = "a-very-long-model-界👩🏽‍💻e\u{301}-identifier";
        status.project.model.version = "v123456789";
        status.project.git = GitStatus::Available {
            reference: GitReference::Branch("a-very-long-branch-name"),
            added: u32::MAX,
            removed: u32::MAX,
            operation: Some(GitOperation::Rebase),
        };
        for width in 30..=130 {
            for characters in [0, 1, 65_536] {
                for spinner in [None, Some('⠋')] {
                    status.characters = characters;
                    status.spinner = spinner;
                    let compact = width < 60;
                    let (left, right) = status_text(status, compact, width - 2);
                    assert!(
                        cell_width(&left) + 2 + cell_width(&right) <= width - 2,
                        "{width}: {left} / {right}"
                    );
                    assert!(left.starts_with('O'), "{width}: {left}");
                    assert_eq!(left.contains('⠋'), spinner.is_some());
                    if characters > 0 {
                        assert!(
                            left.contains(&format!(
                                "{characters}{}",
                                if compact { "c" } else { " chars" }
                            )),
                            "{width}: {left}"
                        );
                    }
                    assert!(
                        right.starts_with(if compact { "18% · " } else { "18% used · " }),
                        "{width}: {right}"
                    );
                    assert!(
                        right.ends_with(if compact { " F" } else { " fast" }),
                        "{width}: {right}"
                    );
                    assert_eq!(left.contains("rebase"), !compact, "{width}: {left}");
                    let area = Rect::new(3, 2, width as u16 - 2, 1);
                    let mut terminal =
                        Terminal::new(TestBackend::new(width as u16 + 6, 5)).unwrap();
                    let completed = terminal
                        .draw(|frame| {
                            draw_status(frame, status, area, compact, Palette::new(false))
                        })
                        .unwrap();
                    let right_x = area.right() - cell_width(&right) as u16;
                    assert_eq!(completed.buffer[(right_x - 1, area.y)].symbol(), " ");
                    assert_eq!(completed.buffer[(right_x - 2, area.y)].symbol(), " ");
                    assert_eq!(completed.buffer[(right_x, area.y)].symbol(), "1");
                    assert_eq!(completed.buffer[(area.x - 1, area.y)].symbol(), " ");
                    assert_eq!(completed.buffer[(area.right(), area.y)].symbol(), " ");
                }
            }
        }
    }

    #[test]
    fn ps3_literal_metadata_and_shortening_preserve_whole_graphemes() {
        assert_eq!(
            metadata("a\n\r\t\u{1b}[2J\u{2028}b\u{85}c", 256),
            "a    [2J b c"
        );
        assert_eq!(shorten("e\u{301}👩🏽‍💻界x", 4), "e\u{301}👩🏽‍💻…");
        assert_eq!(metadata("e\u{301}", 1), "");
        for cells in 0..16 {
            let text = shorten("👩🏽‍💻 e\u{301} 界 long", cells);
            assert!(cell_width(&text) <= cells);
            assert!(!text.contains("👩…"));
        }
        let mut status = App::new(true).composer_status();
        status.project.path = "~/src/\n\u{1b}[2J\runsafe";
        status.project.model.name = "demo\n\u{1b}[0m";
        let (left, right) = status_text(status, false, 118);
        assert!(!left.chars().chain(right.chars()).any(char::is_control));
    }

    #[test]
    fn ps3_typed_field_colours_do_not_leak_into_literal_metadata_or_separators() {
        for light in [false, true] {
            let palette = Palette::new(light);
            let mut status = App::new(true).composer_status();
            // Text resembling another field does not acquire its semantic role.
            status.project.git = GitStatus::Available {
                reference: GitReference::Branch("+24"),
                added: 24,
                removed: 8,
                operation: None,
            };
            status.project.model.name = "-8";
            let area = Rect::new(3, 2, 118, 1);
            let mut terminal = Terminal::new(TestBackend::new(126, 5)).unwrap();
            let completed = terminal
                .draw(|frame| draw_status(frame, status, area, false, palette))
                .unwrap();
            let row = (area.x..area.right())
                .map(|x| completed.buffer[(x, area.y)].symbol())
                .collect::<String>();
            let project_x = area.x;
            for offset in 0..6 {
                assert_eq!(
                    completed.buffer[(project_x + offset, area.y)].fg,
                    palette.project
                );
            }
            let addition_positions = row
                .match_indices("+24")
                .map(|(index, _)| area.x + cell_width(&row[..index]) as u16)
                .collect::<Vec<_>>();
            assert_eq!(addition_positions.len(), 2, "{row}");
            for (start, colour, next_colour) in [
                (addition_positions[0], palette.muted, palette.muted),
                (addition_positions[1], palette.added, palette.removed),
            ] {
                for offset in 0..3 {
                    assert_eq!(
                        completed.buffer[(start + offset, area.y)].fg,
                        colour,
                        "{row}"
                    );
                }
                assert_eq!(completed.buffer[(start + 3, area.y)].fg, next_colour);
            }
            let removal_positions = row
                .match_indices("-8")
                .map(|(index, _)| area.x + cell_width(&row[..index]) as u16)
                .collect::<Vec<_>>();
            assert_eq!(removal_positions.len(), 2, "{row}");
            for (start, colour) in [
                (removal_positions[0], palette.removed),
                (removal_positions[1], palette.muted),
            ] {
                for offset in 0..2 {
                    assert_eq!(
                        completed.buffer[(start + offset, area.y)].fg,
                        colour,
                        "{row}"
                    );
                }
                assert_eq!(completed.buffer[(start + 2, area.y)].fg, palette.muted);
            }
            for x in area.x..area.right() {
                assert_eq!(completed.buffer[(x, area.y)].bg, Color::Reset);
            }
        }
    }

    #[test]
    fn ps3_shortened_project_retains_first_grapheme_ellipsis_and_cyan_at_minimum_width() {
        for light in [false, true] {
            let palette = Palette::new(light);
            let mut status = App::new(true).composer_status();
            status.project.project = crate::model::Project::Observatory;
            status.characters = 65_536;
            status.spinner = Some('⠋');
            let area = Rect::new(3, 2, 28, 1);
            let (left, right) = status_text(status, true, 28);
            assert!(left.starts_with("O…"), "{left}");
            assert!(left.contains("65536c"));
            assert!(left.ends_with('⠋'));
            assert!(right.starts_with("18%"));
            assert!(right.ends_with('F'));
            let mut terminal = Terminal::new(TestBackend::new(34, 5)).unwrap();
            let completed = terminal
                .draw(|frame| draw_status(frame, status, area, true, palette))
                .unwrap();
            assert_eq!(completed.buffer[(area.x, area.y)].symbol(), "O");
            assert_eq!(completed.buffer[(area.x + 1, area.y)].symbol(), "…");
            assert_eq!(completed.buffer[(area.x, area.y)].fg, palette.project);
            assert_eq!(completed.buffer[(area.x + 1, area.y)].fg, palette.project);
            assert_eq!(completed.buffer[(area.x + 2, area.y)].fg, palette.muted);
            assert_eq!(completed.buffer[(area.x - 1, area.y)].symbol(), " ");
            assert_eq!(completed.buffer[(area.right(), area.y)].symbol(), " ");
        }
    }

    #[test]
    fn ps3_default_main_background_retains_editor_and_user_bands_in_both_palettes() {
        for light in [false, true] {
            let palette = Palette::new(light);
            assert_eq!(palette.base, Color::Reset);
            let mut app = working();
            app.handle(Event::Paste("draft".into()));
            let area = Rect::new(3, 2, 80, 24);
            let mut terminal = Terminal::new(TestBackend::new(86, 28)).unwrap();
            let completed = terminal
                .draw(|frame| draw_in_area(frame, &mut app, light, area, ""))
                .unwrap();
            let g = app_geometry(&app, area);
            for row in [g.upper, g.lower, g.status] {
                for x in row.x..row.right() {
                    assert_eq!(completed.buffer[(x, row.y)].bg, Color::Reset);
                }
            }
            for x in g.editor.x..g.editor.right() {
                assert_eq!(completed.buffer[(x, g.editor.y)].bg, palette.band);
            }
            let mut user_cells = 0;
            let mut default_cells = 0;
            for y in g.transcript.y..g.transcript.bottom() {
                for x in g.transcript.x..g.transcript.right() {
                    let bg = completed.buffer[(x, y)].bg;
                    assert!(
                        bg == Color::Reset || bg == palette.user_band,
                        "{x},{y}: {bg:?}"
                    );
                    user_cells += usize::from(bg == palette.user_band);
                    default_cells += usize::from(bg == Color::Reset);
                }
            }
            assert!(user_cells >= usize::from(g.transcript.width));
            assert!(default_cells >= usize::from(g.transcript.width));
        }
    }

    #[test]
    fn ps4_capacity_notice_advertises_only_an_available_attention_route() {
        let mut app = working();
        for _ in 0..crate::model::PROTECTED_LIMIT {
            app.fixture
                .submit_request(app.fixture.target(), Action::Queue, 0, "later".into(), 0)
                .unwrap();
            assert!(app.fixture.acknowledge_pending());
        }
        assert_eq!(composer_notice(&app, false), "8 protected · capacity full");
        app.fixture.toggle_decision();
        assert_eq!(composer_notice(&app, false), "Capacity full · Ctrl+O");
        app.fixture.reject_next();
        assert_eq!(composer_notice(&app, false), "Capacity full · Ctrl+O");
        app.fixture.set_connected(false);
        assert_eq!(composer_notice(&app, false), "Disconnected · F2 reconnects");
    }

    #[test]
    fn ps3_explicit_error_preserves_decision_route_and_minimum_geometry() {
        let mut app = working();
        app.fixture.toggle_decision();
        app.notice = "Captured action expired".into();
        let screen = visible(&mut app, 30, 8, false);
        assert!(screen.contains("Captured action expired"), "{screen}");
        assert!(screen.contains("Decision · Ctrl+O"), "{screen}");
        let g = app_geometry(&app, Rect::new(0, 0, 30, 8));
        assert_eq!(g.destination.height, 1);
        assert_eq!(g.notice.height, 1);
        assert!(g.transcript.height >= 3);
        assert!(g.editor.height >= 1);
    }

    #[test]
    fn cp6_inline_steering_follows_delivered_task_identity_across_revisions_and_disconnect() {
        let mut app = working();
        let mut expected = Vec::new();
        for text in ["first refinement", "second refinement"] {
            expected.push(
                app.fixture
                    .submit_request(app.fixture.target(), Action::Steer, 0, text.into(), 0)
                    .unwrap(),
            );
            assert!(app.fixture.acknowledge_pending());
        }
        let queued = app
            .fixture
            .submit_request(
                app.fixture.target(),
                Action::Queue,
                0,
                "next task".into(),
                0,
            )
            .unwrap();
        assert!(app.fixture.acknowledge_pending());
        expected.reverse();
        expected.insert(0, queued);
        let delivered = inline_messages(app.fixture.message_tray());
        assert_eq!(
            delivered.iter().map(|item| item.id).collect::<Vec<_>>(),
            expected
        );
        assert_ne!(delivered[1].target.task, delivered[2].target.task);
        assert!(app.fixture.set_connected(false));
        assert!(app.fixture.complete(1));
        assert!(app.fixture.display_task().is_none());
        let frozen = inline_messages(app.fixture.message_tray());
        assert_eq!(
            frozen.iter().map(|item| item.id).collect::<Vec<_>>(),
            expected,
            "hidden successor must not remove the delivered task's steering"
        );
        assert!(frozen.iter().all(|item| item.stale));
        assert!(app.fixture.set_connected(true));
        assert!(inline_messages(app.fixture.message_tray()).is_empty());
        assert_eq!(app.fixture.message_tray().items.len(), 4);
    }

    #[test]
    fn cp6_inline_steering_requires_matching_scope_task_and_action() {
        let mut app = working();
        app.fixture
            .submit_request(
                app.fixture.target(),
                Action::Steer,
                0,
                "refinement".into(),
                0,
            )
            .unwrap();
        assert!(app.fixture.acknowledge_pending());
        let tray = app.fixture.message_tray();
        assert_eq!(inline_messages(tray.clone()).len(), 1);
        for mismatch in 0..6 {
            let mut changed = tray.clone();
            let steer = changed
                .items
                .iter_mut()
                .find(|item| item.action == Action::Steer)
                .unwrap();
            match mismatch {
                0 => steer.target.scope.generation += 1,
                1 => steer.target.scope.conversation += 1,
                2 => steer.target.scope.project = crate::model::Project::Observatory,
                3 => steer.target.task.as_mut().unwrap().id += 1,
                4 => steer.target.task = None,
                _ => steer.action = Action::NewTurn,
            }
            assert!(inline_messages(changed).is_empty(), "mismatch {mismatch}");
        }
    }

    #[test]
    fn ps3_idle_and_settled_history_allocate_no_rows_above_input() {
        let mut app = App::new(true);
        for settled in [false, true] {
            if settled {
                app.fixture.submit("completed input".into(), 0).unwrap();
                app.fixture.acknowledge_pending();
                app.fixture.complete(1);
            }
            for (width, height) in [(30, 8), (40, 12), (80, 24)] {
                let area = Rect::new(3, 2, width, height);
                let g = app_geometry(&app, area);
                assert_eq!(g.destination.height, 0);
                assert_eq!(g.notice.height, 0);
                assert_eq!(g.tray.height, 0);
                assert_eq!(g.destination.bottom(), g.upper.y);
                assert_eq!(g.upper.bottom(), g.editor.y);
            }
        }
        app.notice = "Explicit feedback".into();
        assert_eq!(app_geometry(&app, Rect::new(0, 0, 80, 24)).notice.height, 1);
        app.notice.clear();
        app.viewport.scroll_rows(-1);
        assert_eq!(app_geometry(&app, Rect::new(0, 0, 80, 24)).notice.height, 1);
    }

    #[test]
    fn cp6_notice_adds_actions_without_repeating_status_facts() {
        let mut app = App::new(true);
        assert!(composer_notice(&app, false).is_empty());
        app.fixture.submit("pending".into(), 0).unwrap();
        assert_eq!(composer_notice(&app, false), "F5 accepts");
        app.manual = false;
        assert!(composer_notice(&app, false).is_empty());
        app.fixture.set_connected(false);
        assert_eq!(composer_notice(&app, false), "Disconnected · F2 reconnects");
        app.paste_mode = true;
        app.captured = "paste".into();
        assert_eq!(composer_notice(&app, false), "5 bytes · Ctrl+V review");
        app.capture_error = Some("Capture failed".into());
        assert_eq!(
            composer_notice(&app, false),
            "Ctrl+V review · Capture failed"
        );
    }

    #[test]
    fn cp6_tray_uses_each_row_until_overflow_and_collapses_a_heading_only_row() {
        let area = Rect::new(3, 2, 80, 24);
        for count in 0..=4 {
            let g = composer_geometry(area, 1, count, false, false);
            assert_eq!(usize::from(g.tray.height), count);
        }
        assert_eq!(composer_geometry(area, 1, 5, false, false).tray.height, 4);
        let minimum = Rect::new(3, 2, 30, 8);
        assert_eq!(
            composer_geometry(minimum, 1, 1, false, false).tray.height,
            1
        );
        let collapsed = composer_geometry(minimum, 1, 2, true, false);
        assert_eq!(collapsed.tray.height, 0);
        assert_eq!(collapsed.transcript.height, 3);
        assert_eq!(collapsed.destination.height, 1);
        assert_eq!(composer_geometry(minimum, 1, 1, true, false).tray.height, 1);
    }

    #[test]
    fn ct7_suggestions_use_only_spare_rows_above_tray() {
        for area in [
            Rect::new(0, 0, 30, 8),
            Rect::new(3, 2, 40, 12),
            Rect::new(3, 2, 80, 24),
        ] {
            for relevant in [0, 1, 8] {
                for notice in [false, true] {
                    let baseline = composer_geometry(area, 1, relevant, notice, true);
                    let suggested =
                        composer_geometry_with_suggestions(area, 1, relevant, notice, true, 128);
                    assert!(suggested.suggestions.height <= 2);
                    assert!(suggested.transcript.height >= 3);
                    assert_eq!(suggested.transcript.bottom(), suggested.suggestions.y);
                    assert_eq!(suggested.suggestions.bottom(), baseline.transcript.bottom());
                    assert_eq!(suggested.tray, baseline.tray);
                    assert_eq!(suggested.editor, baseline.editor);
                    assert_eq!(suggested.status, baseline.status);
                    assert_eq!(
                        suggested.transcript.height + suggested.suggestions.height,
                        baseline.transcript.height
                    );
                }
            }
        }
    }

    #[test]
    fn ct7_inactive_suggestions_leave_geometry_unchanged() {
        for width in [30, 40, 80] {
            for height in [8, 12, 24] {
                let area = Rect::new(0, 0, width, height);
                let plain = composer_geometry(area, 2, 2, true, false);
                let inactive = composer_geometry_with_suggestions(area, 2, 2, true, false, 0);
                assert_eq!(inactive.suggestions.height, 0);
                assert_eq!(inactive.transcript, plain.transcript);
                assert_eq!(inactive.tray, plain.tray);
                assert_eq!(inactive.editor, plain.editor);
                assert_eq!(inactive.status, plain.status);
            }
        }
    }

    #[test]
    fn ct7_compact_command_browser_keeps_selected_identity_and_route_visible() {
        for light in [false, true] {
            let mut app = App::new(true);
            key(&mut app, KeyCode::F(9), KeyModifiers::NONE);
            let unselected = visible(&mut app, 30, 8, light);
            assert!(unselected.contains("Commands"), "{unselected}");
            assert!(unselected.contains("Select a command"), "{unselected}");
            key(&mut app, KeyCode::Down, KeyModifiers::NONE);
            let selected = visible(&mut app, 30, 8, light);
            assert!(selected.contains('›'), "{selected}");
            assert!(selected.contains("asura · Available"), "{selected}");
        }
    }

    #[test]
    fn ct7_passive_suggestions_are_near_input_and_never_in_status() {
        for light in [false, true] {
            let mut app = App::new(true);
            let idle = visible(&mut app, 80, 24, light);
            assert!(!idle.contains("Tab complete"), "{idle}");
            key(&mut app, KeyCode::Char('/'), KeyModifiers::NONE);
            key(&mut app, KeyCode::Char('q'), KeyModifiers::NONE);
            let screen = visible(&mut app, 80, 24, light);
            let g = app_geometry(&app, Rect::new(0, 0, 80, 24));
            assert!(g.suggestions.height > 0, "{screen}");
            assert!(screen.contains("Built-in · /quit"), "{screen}");
            assert!(screen.contains("Tab complete"), "{screen}");
            assert!(!screen.lines().last().unwrap().contains("Tab complete"));
            assert!(g.suggestions.bottom() <= g.tray.y);
            key(&mut app, KeyCode::Char('x'), KeyModifiers::NONE);
            assert_eq!(
                app_geometry(&app, Rect::new(0, 0, 80, 24))
                    .suggestions
                    .height,
                0
            );
        }
    }

    #[test]
    fn cp3_excerpts_bound_bytes_and_do_not_split_wide_or_combining_clusters() {
        assert_eq!(message_excerpt("", "👩🏽‍💻界x", 4), "👩🏽‍💻…");
        assert_eq!(message_excerpt("", "e\u{301}\n界", 8), "e\u{301} 界");
        let huge_cluster = format!("e{}", "\u{301}".repeat(32_000));
        assert_eq!(message_excerpt("Steer: ", &huge_cluster, 80), "Steer: …");
        let zero_cells = "\u{200b}".repeat(21_000);
        let excerpt = message_excerpt("Queue: ", &zero_cells, 80);
        assert!(excerpt.len() <= 1040);
        assert!(excerpt.ends_with('…'));
        for width in 0..80 {
            let excerpt = message_excerpt("Steer: ", "e\u{301} 👩🏽‍💻 界".repeat(200).as_str(), width);
            assert!(unicode_display_width::width(&excerpt) <= u64::from(width));
            assert!(!excerpt.contains("👩…"));
        }
    }

    #[test]
    fn cp3_translated_tray_geometry_is_bounded_and_preserves_minimums() {
        for width in [0, 1, 29, 30, 40, 60, 80, 120] {
            for height in 0..45 {
                for rows in [1, 3, 6, 30] {
                    for retained in [0, 1, 8, 18] {
                        for show_notice in [false, true] {
                            let area = Rect::new(3, 2, width, height);
                            let g = composer_geometry(area, rows, retained, show_notice, true);
                            for rect in [
                                g.transcript,
                                g.suggestions,
                                g.destination,
                                g.notice,
                                g.tray,
                                g.upper,
                                g.editor,
                                g.lower,
                                g.status,
                            ] {
                                assert_eq!(rect.intersection(area), rect, "{area:?} {g:?}");
                            }
                            if !g.small {
                                assert!(g.transcript.height >= 3);
                                assert!(g.editor.height >= 1);
                                assert_eq!(g.status.height, 1);
                                assert_eq!(g.notice.height, u16::from(show_notice));
                                assert!(
                                    g.tray.height
                                        <= if width >= 60 && height >= 16 { 4 } else { 2 }
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn tp1_rectangles_stay_inside_terminal_and_keep_editor() {
        for width in 0..130 {
            for height in 0..45 {
                for rows in [1, 3, 6, 30] {
                    let area = Rect::new(0, 0, width, height);
                    let g = geometry(area, rows);
                    for rect in [
                        g.transcript,
                        g.suggestions,
                        g.destination,
                        g.notice,
                        g.upper,
                        g.editor,
                        g.lower,
                        g.status,
                    ] {
                        assert_eq!(rect.intersection(area), rect, "{area:?} {g:?}");
                    }
                    if !g.small {
                        assert!(g.editor.height >= 1);
                        assert!(g.transcript.height >= 3);
                        assert_eq!(g.status.height, 1);
                    }
                }
            }
        }
    }

    #[test]
    fn tp3_compact_overlay_keeps_selected_action_visible_while_details_scroll() {
        for light in [false, true] {
            let mut app = working();
            app.handle(Event::Paste("retained draft".into()));
            key(&mut app, KeyCode::F(3), KeyModifiers::NONE);
            app.fixture
                .task
                .as_mut()
                .unwrap()
                .decision
                .as_mut()
                .unwrap()
                .prompt =
                "Long fixture explanation with wide 界 and joined 👩🏽‍💻 content. ".repeat(100);
            key(&mut app, KeyCode::Char('o'), KeyModifiers::CONTROL);
            key(&mut app, KeyCode::Up, KeyModifiers::NONE);
            for _ in 0..20 {
                key(&mut app, KeyCode::PageDown, KeyModifiers::NONE);
            }
            let screen = visible(&mut app, 30, 8, light);
            assert!(screen.contains("Continue"), "{screen}");
            assert!(screen.contains("› Stop"), "{screen}");
            assert!(app.overlay_scroll > 0);
            assert_eq!(app.editor.text(), "retained draft");
            key(&mut app, KeyCode::F(3), KeyModifiers::NONE);
            let screen = visible(&mut app, 30, 8, light);
            assert!(screen.contains("› Stop (unavailable)"), "{screen}");
            key(&mut app, KeyCode::Enter, KeyModifiers::NONE);
            assert!(app.overlay.is_some());
            assert!(app.fixture.pending.is_none());
        }
    }

    #[test]
    fn tp3_compact_attention_list_scrolls_to_last_captured_action() {
        let mut app = App::new(true);
        for index in 0..7 {
            key(&mut app, KeyCode::F(4), KeyModifiers::NONE);
            app.handle(Event::Paste(format!("rejected {index}")));
            key(&mut app, KeyCode::Enter, KeyModifiers::NONE);
            key(&mut app, KeyCode::F(5), KeyModifiers::NONE);
        }
        app.handle(Event::Paste("active task".into()));
        key(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        key(&mut app, KeyCode::F(5), KeyModifiers::NONE);
        key(&mut app, KeyCode::F(3), KeyModifiers::NONE);
        key(&mut app, KeyCode::Char('o'), KeyModifiers::CONTROL);
        assert_eq!(app.overlay_view().unwrap().actions.len(), 8);
        key(&mut app, KeyCode::Up, KeyModifiers::NONE);
        let screen = visible(&mut app, 30, 8, false);
        assert!(screen.contains("› Review decision"), "{screen}");
        key(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert!(matches!(
            app.overlay,
            Some(Overlay::Decision { selected: None, .. })
        ));
    }

    #[test]
    fn tp4_recovery_failure_is_visible_inside_overlay_and_text_is_retained() {
        let mut app = App::new(true);
        key(&mut app, KeyCode::F(4), KeyModifiers::NONE);
        app.handle(Event::Paste("recover me".into()));
        key(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        key(&mut app, KeyCode::F(5), KeyModifiers::NONE);
        app.handle(Event::Paste("x".repeat(crate::editor::MAX_DRAFT_BYTES)));
        key(&mut app, KeyCode::Char('o'), KeyModifiers::CONTROL);
        key(&mut app, KeyCode::Tab, KeyModifiers::NONE);
        key(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        let screen = visible(&mut app, 80, 24, false);
        assert!(
            screen.contains("Recovered text would exceed 64 KiB"),
            "{screen}"
        );
        assert!(screen.contains("› Append to draft"), "{screen}");
        assert_eq!(app.fixture.recoverable[0].text, "recover me");
        assert_eq!(app.editor.text().len(), crate::editor::MAX_DRAFT_BYTES);
        key(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        key(&mut app, KeyCode::F(1), KeyModifiers::NONE);
        assert!(
            app.notice.is_empty(),
            "new overlay clears stale action failure"
        );
    }

    #[test]
    fn tp1_dynamic_notice_and_tiny_rectangles_preserve_draft() {
        let mut app = working();
        app.handle(Event::Paste("keep e\u{301} 👩🏽‍💻 界".into()));
        key(&mut app, KeyCode::F(3), KeyModifiers::NONE);
        let screen = visible(&mut app, 80, 24, false);
        assert!(screen.contains("Decision · Ctrl+O reviews"), "{screen}");
        assert!(app.overlay.is_none());
        key(&mut app, KeyCode::F(2), KeyModifiers::NONE);
        let screen = visible(&mut app, 80, 24, false);
        assert!(screen.contains("F2 reconnects"), "{screen}");
        assert!(screen.contains("Disconnected"), "{screen}");
        assert!(
            !screen.contains("Decision · Ctrl+O reviews"),
            "disconnected state cannot claim current decision availability: {screen}"
        );
        assert_eq!(
            app.attention().len(),
            1,
            "only connection state is available until reconciliation"
        );
        key(&mut app, KeyCode::Char('o'), KeyModifiers::CONTROL);
        for (width, height) in [(0, 0), (1, 1), (29, 7), (30, 8), (40, 12)] {
            visible(&mut app, width, height, false);
            assert_eq!(app.editor.text(), "keep e\u{301} 👩🏽‍💻 界");
        }
    }

    #[test]
    fn tp3_maximum_newline_rich_recovery_detail_remains_fully_reachable() {
        let mut app = App::new(true);
        key(&mut app, KeyCode::F(4), KeyModifiers::NONE);
        app.handle(Event::Paste(format!("a{}X", "\n".repeat(65_534))));
        key(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        key(&mut app, KeyCode::F(5), KeyModifiers::NONE);
        key(&mut app, KeyCode::Char('o'), KeyModifiers::CONTROL);
        key(&mut app, KeyCode::Tab, KeyModifiers::NONE);
        // Reach the last rows with the same public PageDown route, even though
        // headers put the final marker beyond the old u16 global-scroll bound.
        for _ in 0..22_000 {
            key(&mut app, KeyCode::PageDown, KeyModifiers::NONE);
        }
        let mut screen = visible(&mut app, 30, 8, false);
        assert!(app.overlay_scroll > usize::from(u16::MAX));
        for _ in 0..10 {
            if screen.contains('X') {
                break;
            }
            key(&mut app, KeyCode::PageUp, KeyModifiers::NONE);
            screen = visible(&mut app, 30, 8, false);
        }
        assert!(
            screen.contains('X'),
            "last captured-text marker must be reachable: {screen}"
        );
        assert!(screen.contains("› Restore to draft"), "{screen}");
        assert_eq!(app.fixture.recoverable[0].text.len(), 65_536);
        assert!(app.editor.is_empty());
    }

    #[test]
    fn tp4_new_output_cue_survives_decision_notice_and_project_navigation() {
        let mut app = working();
        for index in 0..20 {
            app.fixture.complete(index);
            app.fixture
                .submit(format!("history {index}"), index)
                .unwrap();
            app.fixture.acknowledge_pending();
        }
        visible(&mut app, 40, 12, false);
        key(&mut app, KeyCode::PageUp, KeyModifiers::NONE);
        let before = visible(&mut app, 40, 12, false);
        assert!(!app.unread_output);
        key(&mut app, KeyCode::F(3), KeyModifiers::NONE);
        let screen = visible(&mut app, 40, 12, false);
        assert!(screen.contains("New output"), "{screen}");
        assert!(screen.contains("Decision · Ctrl+O reviews"), "{screen}");
        assert_eq!(
            before.lines().nth(1),
            screen.lines().nth(1),
            "reading anchor stays put"
        );
        key(&mut app, KeyCode::Char('p'), KeyModifiers::CONTROL);
        visible(&mut app, 40, 12, false);
        assert!(!app.unread_output);
        key(&mut app, KeyCode::Char('p'), KeyModifiers::CONTROL);
        assert!(visible(&mut app, 40, 12, false).contains("New output"));
        key(&mut app, KeyCode::Char('e'), KeyModifiers::CONTROL);
        assert!(!visible(&mut app, 40, 12, false).contains("New output"));
        assert!(!app.unread_output);
    }

    #[test]
    fn tp4_lost_ack_hides_authoritative_progress_and_steer_until_reconciliation() {
        let mut app = App::new(true);
        app.handle(Event::Paste("first request".into()));
        key(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        key(&mut app, KeyCode::F(7), KeyModifiers::NONE);
        let screen = visible(&mut app, 80, 24, false);
        assert!(screen.contains("Disconnected"), "{screen}");
        assert!(screen.contains("Unknown:"), "{screen}");
        assert!(
            !screen.contains("Message received"),
            "unacknowledged work must not be presented as accepted: {screen}"
        );
        assert!(!screen.contains("Working"), "{screen}");
        assert_eq!(app.attention().len(), 1);
        key(&mut app, KeyCode::F(2), KeyModifiers::NONE);
        assert!(visible(&mut app, 80, 24, false).contains("Message received"));
        app.handle(Event::Paste("refine once".into()));
        key(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        key(&mut app, KeyCode::Tab, KeyModifiers::NONE);
        key(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        app.handle(Event::Paste("new independent draft".into()));
        key(&mut app, KeyCode::F(7), KeyModifiers::NONE);
        let screen = visible(&mut app, 80, 24, false);
        assert!(screen.contains("Unknown:"), "{screen}");
        assert!(
            !screen.contains("Steering instruction acknowledged"),
            "{screen}"
        );
        assert!(!screen.contains("Working"), "{screen}");
        key(&mut app, KeyCode::F(2), KeyModifiers::NONE);
        let screen = visible(&mut app, 80, 24, false);
        assert!(
            screen.contains("Steering instruction acknowledged"),
            "{screen}"
        );
        assert_eq!(app.editor.text(), "new independent draft");
    }
}
