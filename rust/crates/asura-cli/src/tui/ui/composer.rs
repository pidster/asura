//! Local focus and selection. Requests continue through the canonical workers.
use super::*;
use crossterm::event::KeyEvent;

#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub(super) enum Focus {
    #[default]
    Editor,
    Project,
    Model,
    Queue(usize),
}
pub(super) enum Picker {
    Projects {
        selected: usize,
        rows: Vec<asura_control::pb::ProjectReply>,
    },
    Models {
        selected: usize,
        result: Option<super::super::models::Outcome>,
    },
}
impl App {
    pub(super) fn close_picker(&mut self) {
        if self.picker_models_pending {
            self.models_cancel = true;
        }
        self.picker = None;
    }
    pub(super) fn select_project(&mut self, id: [u8; 16]) -> bool {
        if self.conversation_busy
            || self.retained_request.is_some()
            || self.queue_busy
            || self.queue_retained.is_some()
            || !self.local_outbox.is_empty()
        {
            self.notice = "Finish active work before changing project".into();
            return false;
        }
        if !self.projects.as_ref().is_some_and(|rows| {
            rows.iter().any(|row| {
                row.project_id.as_deref() == Some(id.as_slice()) && row.current == Some(true)
            })
        }) {
            self.notice = "Project unavailable; refresh with /project list".into();
            return false;
        }
        self.selected_project = Some(id);
        self.conversation_id = None;
        self.conversation_generation = 0;
        self.new_conversation_intent = None;
        self.restoration_scope = None;
        self.restoration_ready = false;
        self.pending_restored_operation = None;
        self.active_operation = None;
        self.observed_model = None;
        self.model_context = None;
        self.queue_entries.clear();
        self.queue_observed.clear();
        self.transcript.clear();
        self.rendered_responses.clear();
        self.activity.clear();
        self.response_selection = None;
        self.response_scroll = 0;
        self.context = Default::default();
        self.notice = "Project selected".into();
        true
    }
    fn recall(&mut self, previous: bool) -> bool {
        let recalled = if previous {
            self.history.previous(&self.editor.text())
        } else {
            self.history.next()
        };
        if let Some(text) = recalled {
            let _ = self
                .editor
                .key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL));
            if let Err(error) = self.editor.insert(&text) {
                self.notice = error.to_string();
            }
            true
        } else {
            false
        }
    }
    pub(super) fn unresolved(&self) -> Vec<&asura_control::pb::ConversationQueueEntry> {
        self.queue_entries
            .iter()
            .filter(|entry| matches!(entry.state, Some(1 | 2)))
            .collect()
    }
    fn queue_focus_ids(&self) -> Vec<Vec<u8>> {
        self.unresolved()
            .into_iter()
            .filter_map(|entry| entry.input_id.clone())
            .chain(
                self.local_outbox
                    .iter()
                    .map(|item| item.request_id.to_vec()),
            )
            .collect()
    }
    fn move_queue_item(&mut self, id: &[u8], up: bool) {
        if let Some(index) = self
            .local_outbox
            .iter()
            .position(|item| item.request_id.as_slice() == id)
        {
            let adjacent = if up {
                index.checked_sub(1)
            } else {
                index.checked_add(1)
            };
            if let Some(other) = adjacent.filter(|&other| other < self.local_outbox.len())
                && self.local_outbox[index].state == LocalInputState::Waiting
                && self.local_outbox[other].state == LocalInputState::Waiting
            {
                self.local_outbox.swap(index, other);
                self.notice = "Pending local order changed".into();
            } else {
                self.notice = "Only unsent local inputs can move".into();
            }
            return;
        }
        if self.queue_busy || self.queue_retained.is_some() {
            self.notice = "Queue request pending; move not sent".into();
            return;
        }
        let lane: Vec<_> = self
            .unresolved()
            .into_iter()
            .filter(|entry| entry.input_id.as_deref() == Some(id))
            .filter_map(|entry| entry.conversation_id.clone())
            .next()
            .map(|conversation| {
                self.unresolved()
                    .into_iter()
                    .filter(|entry| entry.conversation_id.as_ref() == Some(&conversation))
                    .cloned()
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let Some(index) = lane
            .iter()
            .position(|entry| entry.input_id.as_deref() == Some(id))
        else {
            self.notice = "Selected input is no longer queued".into();
            return;
        };
        let adjacent = if up {
            index.checked_sub(1)
        } else {
            index.checked_add(1)
        };
        let Some(other) = adjacent.filter(|&other| other < lane.len()) else {
            self.notice = "Input is already at this end of its queue".into();
            return;
        };
        if lane[index].new_conversation.is_none() || lane[other].new_conversation.is_none() {
            self.notice = "Legacy queued inputs cannot move".into();
            return;
        }
        let Some(revision) = self.queue_order_revision else {
            self.notice = "Queue order unavailable; wait for a fresh snapshot".into();
            return;
        };
        let after = if up {
            index
                .checked_sub(2)
                .and_then(|prior| lane[prior].input_id.clone())
        } else {
            lane[other].input_id.clone()
        };
        self.send_queue(super::super::queue::Request::Reorder(
            asura_control::pb::ConversationQueueReorder {
                request_id: Some(asura_platform::random_id().to_vec()),
                input_id: Some(id.to_vec()),
                after_input_id: after,
                expected_order_revision: Some(revision),
            },
        ));
    }
    fn send_queue_item_to_front(&mut self, id: &[u8]) {
        if self.queue_busy || self.queue_retained.is_some() {
            self.notice = "Queue request pending; move not sent".into();
            return;
        }
        let Some(entry) = self
            .unresolved()
            .into_iter()
            .find(|entry| entry.input_id.as_deref() == Some(id))
        else {
            self.notice = "Selected input is no longer queued".into();
            return;
        };
        if entry.new_conversation.is_none() {
            self.notice = "Legacy queued inputs cannot move".into();
            return;
        }
        let Some(revision) = self.queue_order_revision else {
            self.notice = "Queue order unavailable; wait for a fresh snapshot".into();
            return;
        };
        self.send_queue(super::super::queue::Request::Reorder(
            asura_control::pb::ConversationQueueReorder {
                request_id: Some(asura_platform::random_id().to_vec()),
                input_id: Some(id.to_vec()),
                after_input_id: None,
                expected_order_revision: Some(revision),
            },
        ));
    }
    pub(super) fn composer_key(&mut self, key: KeyEvent) -> bool {
        if key.modifiers == KeyModifiers::CONTROL
            && matches!(key.code, KeyCode::Char('p' | 'n'))
            && self.picker.is_none()
        {
            self.focus = Focus::Editor;
            self.recall(key.code == KeyCode::Char('p'));
            return true;
        }
        if self.picker.is_some() {
            if key.code == KeyCode::Esc {
                self.close_picker();
                return true;
            }
            if !key.modifiers.is_empty() {
                return true;
            }
            if self.picker_model_save.is_some() {
                return true;
            }
            let picker = self.picker.take().unwrap();
            match picker {
                Picker::Projects { mut selected, rows } => {
                    let count = rows.len();
                    navigate(&mut selected, count, key.code);
                    if key.code == KeyCode::Enter {
                        let id = rows
                            .get(selected)
                            .and_then(|p| p.project_id.clone())
                            .and_then(|id| id.try_into().ok());
                        if let Some(id) = id
                            && self.select_project(id)
                        {
                            self.focus = Focus::Project;
                            return true;
                        }
                    }
                    self.picker = Some(Picker::Projects { selected, rows });
                }
                Picker::Models {
                    mut selected,
                    result,
                } => {
                    let rows = result
                        .as_ref()
                        .and_then(|r| r.as_ref().ok())
                        .map(|r| &r.models);
                    navigate(&mut selected, rows.map_or(0, Vec::len), key.code);
                    if key.code == KeyCode::Enter
                        && let Some(model) = rows.and_then(|r| r.get(selected))
                    {
                        if !matches!(model.status, Some(1..=3)) {
                            self.notice = format!(
                                "Model {} · {}",
                                model_inventory_status(model.status),
                                literal(model.detail.as_deref().unwrap_or_default(), 256)
                            );
                        } else if self.config_draft.is_some() {
                            self.notice = "Configuration request pending".into();
                        } else if let Some(selector) = model.selector.clone() {
                            // Quote through JSON, a valid YAML scalar, so selectors cannot
                            // become YAML values of another type or add configuration keys.
                            let value =
                                serde_json::to_string(&selector).expect("string serialization");
                            self.config_request = Some(super::super::config::Request {
                                draft: String::new(),
                                key: "model".into(),
                                value: Some(value),
                            });
                            self.config_draft = Some(String::new());
                            self.picker_model_save = Some(selector);
                            self.notice = "Saving model selection".into();
                        }
                    }
                    self.picker = Some(Picker::Models { selected, result });
                }
            }
            return true;
        }
        if !key.modifiers.is_empty() {
            return self.focus != Focus::Editor;
        }
        match self.focus {
            Focus::Editor => match key.code {
                KeyCode::Down if self.editor.at_visual_bottom() => {
                    if !self.recall(false) {
                        self.focus = Focus::Project;
                    }
                    true
                }
                KeyCode::Up if self.editor.at_visual_top() => {
                    let ids = self.queue_focus_ids();
                    let count = ids.len();
                    if count > 0 {
                        self.queue_focused_id = Some(ids[count - 1].clone());
                        self.focus = Focus::Queue(count - 1);
                    } else if !self.transcript.is_empty() {
                        self.history_focus = true;
                        self.history_scroll = self.history_scroll_max.saturating_sub(1);
                    }
                    true
                }
                _ => false,
            },
            Focus::Project | Focus::Model => {
                match key.code {
                    KeyCode::Up | KeyCode::Esc => self.focus = Focus::Editor,
                    KeyCode::Left | KeyCode::Right | KeyCode::Tab => {
                        self.focus = if self.focus == Focus::Project {
                            Focus::Model
                        } else {
                            Focus::Project
                        }
                    }
                    KeyCode::Enter if self.focus == Focus::Project => {
                        let selected = self
                            .projects
                            .as_ref()
                            .and_then(|rows| {
                                rows.iter().position(|row| {
                                    row.project_id.as_deref()
                                        == self.selected_project.as_ref().map(|id| id.as_slice())
                                })
                            })
                            .unwrap_or(0);
                        self.picker = Some(Picker::Projects {
                            selected,
                            rows: self.projects.clone().unwrap_or_default(),
                        });
                    }
                    KeyCode::Enter => {
                        if self.models_draft.is_some() || self.picker_models_pending {
                            self.notice = "Model inventory is still settling".into();
                        } else {
                            self.picker = Some(Picker::Models {
                                selected: 0,
                                result: None,
                            });
                            self.picker_models_pending = true;
                            self.models_request = true;
                            self.notice.clear();
                        }
                    }
                    _ => {}
                }
                true
            }
            Focus::Queue(_) => {
                let ids = self.queue_focus_ids();
                let count = ids.len();
                let Some(mut selected) = ids
                    .iter()
                    .position(|id| Some(id) == self.queue_focused_id.as_ref())
                else {
                    self.focus = Focus::Editor;
                    self.notice = "Selected input is no longer queued".into();
                    return true;
                };
                match key.code {
                    KeyCode::Esc => {
                        self.focus = Focus::Editor;
                        return true;
                    }
                    KeyCode::Down if selected + 1 == count => {
                        self.focus = Focus::Editor;
                        return true;
                    }
                    KeyCode::Char('[' | ']') => {
                        self.move_queue_item(&ids[selected], key.code == KeyCode::Char('['));
                    }
                    KeyCode::Enter => {
                        if self.queue_busy || self.queue_retained.is_some() {
                            self.notice = "Queue request pending; use /retry if unconfirmed".into();
                            return true;
                        }
                        if let Some(index) = self
                            .local_outbox
                            .iter()
                            .position(|item| item.request_id.as_slice() == ids[selected].as_slice())
                        {
                            if self.local_outbox[index].state == LocalInputState::Rejected {
                                if self.editor.is_empty() {
                                    let text = self.local_outbox[index].text.clone();
                                    if let Err(error) = self.editor.insert(&text) {
                                        self.notice = error.to_string();
                                    } else {
                                        self.local_outbox.remove(index);
                                        self.focus = Focus::Editor;
                                        self.queue_focused_id = None;
                                        self.notice = "Rejected input restored to editor".into();
                                    }
                                } else {
                                    self.notice =
                                        "Clear the editor before restoring rejected input".into();
                                }
                            } else {
                                self.notice =
                                    "Wait for the service receipt; local input is provisional"
                                        .into();
                            }
                            return true;
                        }
                        let Some(entry) = self
                            .unresolved()
                            .into_iter()
                            .find(|entry| {
                                entry.input_id.as_deref() == Some(ids[selected].as_slice())
                            })
                            .cloned()
                        else {
                            self.notice = "Selected input is no longer queued".into();
                            return true;
                        };
                        let active = self.conversation_busy
                            && self.conversation_cancellable
                            && self.active_operation.is_some()
                            && entry.conversation_id == self.conversation_id;
                        if !active {
                            if entry.state == Some(2) {
                                self.notice = "Held input needs /queue resume ID".into();
                            } else {
                                self.send_queue_item_to_front(&ids[selected]);
                            }
                        } else {
                            let request = asura_control::pb::ConversationQueueDecision {
                                request_id: Some(asura_platform::random_id().to_vec()),
                                input_id: Some(ids[selected].clone()),
                                action: Some(3),
                                target_operation_id: self.active_operation.clone(),
                                target_generation: Some(self.conversation_generation),
                            };
                            self.send_queue(super::super::queue::Request::Decision(request));
                        }
                    }
                    _ => navigate(&mut selected, count, key.code),
                }
                let updated = self.queue_focus_ids();
                if let Some(id) = updated.get(selected) {
                    self.queue_focused_id = Some(id.clone());
                    self.focus = Focus::Queue(selected);
                } else {
                    self.focus = Focus::Editor;
                    self.queue_focused_id = None;
                }
                true
            }
        }
    }
    pub(super) fn hints(&self) -> String {
        let newline_chord = if cfg!(target_os = "macos") {
            "⌥↵"
        } else {
            "Alt+↵"
        };
        if self.overlay.is_some() {
            return "Esc close · arrows navigate · Enter confirm".into();
        }
        if self.response_selection.is_some() {
            return "↑↓ responses · Enter activity · PgUp/PgDn scroll · Esc input".into();
        }
        if self.history_focus {
            return "↑↓ scroll history · PgUp/PgDn page · Home/End · Esc input".into();
        }
        if self.picker_model_save.is_some() {
            return "Saving model selection · draft preserved".into();
        }
        if self.picker.is_some() {
            return "↑↓ select · Enter apply · Esc close".into();
        }
        match self.focus {
            Focus::Editor if self.conversation_busy => format!(
                "↵ queue · {newline_chord} newline · ↑ queue/history · F6 responses · ↓ status"
            ),
            Focus::Editor => {
                format!("↵ send · {newline_chord} newline · ↑ history · ^P/^N recall · ↓ status")
            }
            Focus::Project | Focus::Model => {
                "←→ / Tab select item · Enter open · ↑ / Esc input".into()
            }
            Focus::Queue(_) if self.queue_busy => "Queue request pending · Esc input".into(),
            Focus::Queue(_) if self.conversation_busy => {
                "↑↓ select · [ move up · ] move down · ↵ send now · Esc input".into()
            }
            Focus::Queue(_) => {
                "↑↓ select · [ move up · ] move down · ↵ send now · Esc input".into()
            }
        }
    }
    pub(super) fn focus_style(&self, focus: Focus, style: Style) -> Style {
        if self.focus == focus {
            selection_style(style)
        } else {
            style
        }
    }
    pub(super) fn picker_height(&self, height: u16) -> u16 {
        if self.picker.is_none() {
            return 0;
        }
        let rows = match self.picker.as_ref().unwrap() {
            Picker::Projects { rows, .. } => rows.len().max(1),
            Picker::Models { result, .. } => result
                .as_ref()
                .and_then(|r| r.as_ref().ok())
                .map_or(1, |r| {
                    r.models.len().max(1) + usize::from(!r.issues.is_empty())
                }),
        };
        (rows as u16 + 2).min(8).min(height.saturating_sub(7))
    }
    pub(super) fn draw_picker(&self, frame: &mut Frame, area: Rect, style: Style) {
        let Some(picker) = &self.picker else {
            return;
        };
        if area.height == 0 {
            return;
        }
        frame.render_widget(Paragraph::new("").style(style), area);
        let issue_header = match picker {
            Picker::Models {
                result: Some(Ok(reply)),
                ..
            } if !reply.issues.is_empty() => Some(format!(
                "Partial inventory · {}",
                reply
                    .issues
                    .iter()
                    .map(|issue| format!(
                        "{}: {}",
                        literal(issue.provider.as_deref().unwrap_or("?"), 48),
                        literal(issue.reason.as_deref().unwrap_or("unavailable"), 80)
                    ))
                    .collect::<Vec<_>>()
                    .join(" · ")
            )),
            _ => None,
        };
        let (rows, selected): (Vec<String>, usize) = match picker {
            Picker::Projects { selected, rows } => (
                rows.iter()
                    .map(|row| {
                        format!(
                            "{} · {}{}",
                            project_label(row),
                            row.location.as_deref().unwrap_or_default(),
                            if row.current == Some(true) {
                                ""
                            } else {
                                " · unavailable"
                            }
                        )
                    })
                    .collect(),
                *selected,
            ),
            Picker::Models { selected, result } => (
                match result {
                    None => vec!["Discovering models… · Esc cancels".into()],
                    Some(Err(error)) => vec![literal(error, 256)],
                    Some(Ok(reply)) => reply
                        .models
                        .iter()
                        .map(|row| {
                            format!(
                                "{} · {}",
                                literal(row.selector.as_deref().unwrap_or("?"), 256),
                                model_inventory_status(row.status)
                            )
                        })
                        .collect(),
                },
                *selected,
            ),
        };
        let rows = if rows.is_empty() {
            vec!["No entries available".into()]
        } else {
            rows
        };
        let selected = selected.min(rows.len() - 1);
        let available = area.height.saturating_sub(2).max(1);
        // Preserve one selected row even at the smallest viewport. Normal sizes
        // retain top/bottom padding and a fixed, non-selectable issue header.
        let header_rows = u16::from(issue_header.is_some() && available > 1);
        let inset = u16::from(area.height > 2);
        if header_rows > 0 {
            frame.render_widget(
                Paragraph::new(issue_header.unwrap()).style(style),
                Rect::new(area.x + 1, area.y + inset, area.width.saturating_sub(2), 1),
            );
        }
        let visible = (available - header_rows) as usize;
        let start = selected.saturating_sub(visible - 1);
        let inset = inset + header_rows;
        for (offset, row) in rows.iter().enumerate().skip(start).take(visible) {
            let highlight = if offset == selected {
                selection_style(style)
            } else {
                style
            };
            frame.render_widget(
                Paragraph::new(row.as_str()).style(highlight),
                Rect::new(
                    area.x + 1,
                    area.y + inset + (offset - start) as u16,
                    area.width.saturating_sub(2),
                    1,
                ),
            );
        }
    }
    pub(super) fn draw_queue(
        &self,
        frame: &mut Frame,
        area: Rect,
        bottom: u16,
        style: Style,
    ) -> u16 {
        let entries = self.unresolved();
        let mut rows: Vec<(Vec<u8>, String)> = entries
            .iter()
            .filter_map(|entry| {
                Some((
                    entry.input_id.clone()?,
                    format!(
                        ">> {}{}",
                        if entry.state == Some(2) {
                            "Held · "
                        } else {
                            ""
                        },
                        literal(entry.text.as_deref().unwrap_or_default(), 256)
                    ),
                ))
            })
            .collect();
        rows.extend(self.local_outbox.iter().map(|item| {
            let label = match item.state {
                LocalInputState::Waiting => "Pending local",
                LocalInputState::Sending => "Sending",
                LocalInputState::Unconfirmed => "Unconfirmed",
                LocalInputState::Rejected => "Rejected local",
            };
            (
                item.request_id.to_vec(),
                format!(">> {label} · {}", literal(&item.text, 256)),
            )
        }));
        if rows.is_empty() {
            return 0;
        }
        let selected = if matches!(self.focus, Focus::Queue(_)) {
            rows.iter()
                .position(|row| Some(&row.0) == self.queue_focused_id.as_ref())
        } else {
            None
        };
        let capacity = 4;
        let start = selected
            .unwrap_or(rows.len().saturating_sub(1))
            .saturating_sub(capacity - 1);
        let visible = rows
            .iter()
            .enumerate()
            .skip(start)
            .take(capacity)
            .collect::<Vec<_>>();
        let height = (visible.len() as u16 + 3).min(bottom.saturating_sub(area.y + 3));
        if height < 3 {
            return 0;
        }
        let rect = Rect::new(area.x, bottom - height, area.width, height);
        frame.render_widget(Paragraph::new("").style(style), rect);
        let inner = Rect::new(
            rect.x + 1,
            rect.y + 1,
            rect.width.saturating_sub(2),
            rect.height - 2,
        );
        line(
            frame,
            inner,
            0,
            &format!(
                "• Queued messages:{}",
                if rows.len() > visible.len() {
                    format!(" +{}", rows.len() - visible.len())
                } else {
                    String::new()
                }
            ),
            style,
        );
        for (index, (position, (_, row))) in visible
            .into_iter()
            .take(inner.height.saturating_sub(1) as usize)
            .enumerate()
        {
            line(
                frame,
                inner,
                index as u16 + 1,
                row,
                if selected == Some(position) {
                    selection_style(style)
                } else {
                    style
                },
            );
        }
        height
    }
}
fn selection_style(style: Style) -> Style {
    style
        .bg(Color::Rgb(70, 82, 94))
        .fg(Color::Rgb(245, 248, 250))
        .remove_modifier(ratatui::style::Modifier::REVERSED)
}
fn navigate(selected: &mut usize, count: usize, key: KeyCode) {
    *selected = (*selected).min(count.saturating_sub(1));
    match key {
        KeyCode::Up => *selected = selected.saturating_sub(1),
        KeyCode::Down => *selected = (*selected + 1).min(count.saturating_sub(1)),
        KeyCode::Home => *selected = 0,
        KeyCode::End => *selected = count.saturating_sub(1),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};
    fn key(app: &mut App, code: KeyCode) {
        app.handle(Event::Key(KeyEvent::new(code, KeyModifiers::NONE)));
    }
    fn project(id: u8, current: bool) -> asura_control::pb::ProjectReply {
        asura_control::pb::ProjectReply {
            project_id: Some(vec![id; 16]),
            location: Some(format!("/projects/project{id}")),
            current: Some(current),
            ..Default::default()
        }
    }
    fn draw(app: &mut App, name: &str, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let buffer = terminal.backend().buffer();
        if let Some(dir) = std::env::var_os("ASURA_TUI_PREVIEW_DIR") {
            std::fs::create_dir_all(&dir).unwrap();
            let cells: Vec<_> = buffer.content.iter().map(|cell| serde_json::json!({"symbol":cell.symbol(), "fg":format!("{:?}",cell.fg), "bg":format!("{:?}",cell.bg),"modifier":format!("{:?}",cell.modifier)})).collect();
            std::fs::write(
                std::path::PathBuf::from(dir).join(format!("composer-{name}-{width}.json")),
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
    fn editor_hints_use_platform_key_symbols() {
        let mut app = App::new();
        let newline = if cfg!(target_os = "macos") {
            "⌥↵"
        } else {
            "Alt+↵"
        };
        let idle = app.hints();
        assert!(idle.contains("^P/^N recall"));
        assert!(idle.contains(&format!("{newline} newline")));
        assert!(!idle.contains("Ctrl+"));
        assert!(draw(&mut app, "key-hints", 110, 26).contains(newline));

        app.conversation_busy = true;
        let busy = app.hints();
        assert!(busy.contains(&format!("{newline} newline")));
        assert!(busy.contains("↵ queue"));
    }
    #[test]
    fn status_project_picker_preserves_draft_and_rejects_stale_or_busy() {
        let mut app = App::new();
        app.projects = Some(vec![project(1, true), project(2, false), project(3, true)]);
        app.selected_project = Some([1; 16]);
        app.insert("draft");
        draw(&mut app, "editor", 100, 26);
        key(&mut app, KeyCode::Down);
        assert_eq!(app.focus, Focus::Project);
        assert!(draw(&mut app, "status", 100, 26).contains("Enter open"));
        key(&mut app, KeyCode::Enter);
        assert!(draw(&mut app, "projects", 100, 26).contains("project2 · unavailable"));
        key(&mut app, KeyCode::Down);
        key(&mut app, KeyCode::Enter);
        assert_eq!(app.selected_project, Some([1; 16]));
        key(&mut app, KeyCode::Down);
        app.conversation_busy = true;
        key(&mut app, KeyCode::Enter);
        assert_eq!(app.selected_project, Some([1; 16]));
        app.conversation_busy = false;
        app.retained_request = Some(super::super::super::conversation::Request::DiscoverProject);
        key(&mut app, KeyCode::Enter);
        assert_eq!(app.selected_project, Some([1; 16]));
        app.retained_request = None;
        key(&mut app, KeyCode::Enter);
        assert_eq!(app.selected_project, Some([3; 16]));
        assert_eq!(app.draft(), "draft");
        assert!(app.picker.is_none());
        app.view.connection = "connected";
        app.view.installation = "graph_ready";
        app.view.epoch = Some("epoch".into());
        assert!(app.refresh_queue_scope(std::time::Instant::now()));
        assert_eq!(app.focus, Focus::Project);
        key(&mut app, KeyCode::Tab);
        key(&mut app, KeyCode::Enter);
        assert!(app.take_models_request());
        assert!(app.conversation_request.is_none());
        assert_eq!(app.draft(), "draft");
    }
    #[test]
    fn model_picker_late_result_is_suppressed_and_save_adopts_only_on_ack() {
        let mut app = App::new();
        app.insert("keep this draft");
        app.focus = Focus::Model;
        key(&mut app, KeyCode::Enter);
        assert!(app.take_models_request());
        key(&mut app, KeyCode::Esc);
        assert!(app.take_models_cancel());
        app.models_finished(Err("cancelled".into()));
        assert!(app.models_result.is_none());
        key(&mut app, KeyCode::Enter);
        app.models_finished(Ok(asura_control::pb::ModelsReply {
            models: vec![asura_control::pb::ModelInventoryEntry {
                selector: Some("ollama:example".into()),
                status: Some(3),
                ..Default::default()
            }],
            ..Default::default()
        }));
        draw(&mut app, "models", 100, 26);
        key(&mut app, KeyCode::Enter);
        assert_eq!(app.take_config_request().unwrap().key, "model");
        assert_eq!(app.view.configured_model, None);
        app.config_finished(Err("denied".into()));
        assert_eq!(app.view.configured_model, None);
        assert!(app.picker.is_some());
        key(&mut app, KeyCode::Enter);
        key(&mut app, KeyCode::Esc);
        key(&mut app, KeyCode::Esc);
        assert_eq!(app.focus, Focus::Editor);
        app.config_finished(Ok("Saved model".into()));
        assert_eq!(app.focus, Focus::Editor);
        assert_eq!(app.view.configured_model.as_deref(), Some("ollama:example"));
        assert_eq!(app.draft(), "keep this draft");
        assert!(app.config_result.is_none());
    }
    #[test]
    fn control_history_restores_unsent_draft_before_entering_status() {
        let mut app = App::new();
        app.history.record("/models");
        app.insert("unsent");
        draw(&mut app, "history", 80, 24);
        app.handle(Event::Key(KeyEvent::new(
            KeyCode::Char('p'),
            KeyModifiers::CONTROL,
        )));
        assert_eq!(app.draft(), "/models");
        app.handle(Event::Key(KeyEvent::new(
            KeyCode::Char('n'),
            KeyModifiers::CONTROL,
        )));
        assert_eq!(app.draft(), "unsent");
        assert_eq!(app.focus, Focus::Editor);
        key(&mut app, KeyCode::Down);
        assert_eq!(app.focus, Focus::Project);
        key(&mut app, KeyCode::Esc);
        assert_eq!(app.focus, Focus::Editor);
    }
    #[test]
    fn up_from_editor_scrolls_conversation_without_changing_draft() {
        let mut app = App::new();
        for index in 0..8 {
            app.append_response(format!("prompt {index}"), None);
            app.transcript[index].1 = format!("answer {index} with enough text to fill a row");
            app.rendered_responses[index] = markdown::render(&app.transcript[index].1);
        }
        app.insert("unsent draft");
        draw(&mut app, "history-bottom", 80, 20);
        assert!(app.history_scroll_max > 0);
        key(&mut app, KeyCode::Up);
        assert!(app.history_focus);
        assert_eq!(app.history_scroll, app.history_scroll_max - 1);
        assert_eq!(app.draft(), "unsent draft");
        key(&mut app, KeyCode::Home);
        assert_eq!(app.history_scroll, 0);
        assert!(draw(&mut app, "history-top", 80, 20).contains("prompt 0"));
        key(&mut app, KeyCode::End);
        assert_eq!(app.history_scroll, app.history_scroll_max);
        key(&mut app, KeyCode::Down);
        assert!(!app.history_focus);
        assert_eq!(app.draft(), "unsent draft");
    }
    #[test]
    fn queue_promotion_names_same_input_and_never_clears_unrelated_draft() {
        let mut app = App::new();
        app.view.connection = "connected";
        app.selected_project = Some([1; 16]);
        app.conversation_id = Some(vec![2; 16]);
        app.active_operation = Some(vec![3; 16]);
        app.conversation_generation = 4;
        app.conversation_busy = true;
        app.conversation_cancellable = true;
        app.queue_entries = (0..7)
            .map(|i| asura_control::pb::ConversationQueueEntry {
                input_id: Some(vec![i; 16]),
                conversation_id: Some(vec![2; 16]),
                state: Some(1),
                text: Some(format!("queued input {i}")),
                ..Default::default()
            })
            .collect();
        app.insert("unrelated draft");
        draw(&mut app, "queue", 100, 26);
        key(&mut app, KeyCode::Up);
        assert_eq!(app.focus, Focus::Queue(6));
        assert!(draw(&mut app, "queue-focus", 100, 26).contains(">> queued input 6"));
        key(&mut app, KeyCode::Enter);
        let request = app.take_queue_request(std::time::Instant::now()).unwrap();
        let super::super::super::queue::Request::Decision(decision) = &request else {
            panic!("decision");
        };
        assert_eq!(decision.action, Some(3));
        assert_eq!(decision.input_id, Some(vec![6; 16]));
        assert_eq!(decision.target_operation_id, Some(vec![3; 16]));
        assert_eq!(decision.target_generation, Some(4));
        app.queue_finished(super::super::super::queue::Update {
            request,
            outcome: Ok(asura_control::pb::ConversationQueueReply {
                request_id: Some(vec![8; 16]),
                ..Default::default()
            }),
            done: true,
        });
        assert_eq!(app.draft(), "unrelated draft");
        app.conversation_busy = false;
        app.queue_entries[6].state = Some(2);
        key(&mut app, KeyCode::Enter);
        assert!(app.queue_request.is_none());
        assert!(app.notice.contains("/queue resume"));
    }
    #[test]
    fn local_outbox_reorders_only_unsent_rows_without_changing_text_or_identity() {
        let mut app = App::new();
        app.selected_project = Some([1; 16]);
        app.restoration_ready = true;
        for text in ["first", "second", "third"] {
            app.insert(text);
            app.submit();
        }
        let identities: Vec<_> = app.local_outbox.iter().map(|row| row.request_id).collect();
        app.move_queue_item(&identities[2], true);
        assert_eq!(
            app.local_outbox
                .iter()
                .map(|row| row.text.as_str())
                .collect::<Vec<_>>(),
            vec!["first", "third", "second"]
        );
        assert_eq!(
            app.local_outbox
                .iter()
                .map(|row| row.request_id)
                .collect::<Vec<_>>(),
            vec![identities[0], identities[2], identities[1]]
        );
        app.local_outbox[0].state = LocalInputState::Sending;
        app.queue_busy = true;
        app.move_queue_item(&identities[0], false);
        assert_eq!(app.local_outbox[0].request_id, identities[0]);
        app.move_queue_item(&identities[1], true);
        assert_eq!(app.local_outbox[1].request_id, identities[1]);
    }
    #[test]
    fn service_move_uses_revision_and_same_lane_predecessor() {
        let mut app = App::new();
        app.view.connection = "connected";
        app.selected_project = Some([1; 16]);
        app.queue_order_revision = Some(9);
        app.queue_entries = (1..=3)
            .map(|id| asura_control::pb::ConversationQueueEntry {
                input_id: Some(vec![id; 16]),
                conversation_id: Some(vec![4; 16]),
                new_conversation: Some(false),
                state: Some(1),
                order_position: Some(id as u32),
                ..Default::default()
            })
            .collect();
        app.move_queue_item(&[3; 16], true);
        let super::super::super::queue::Request::Reorder(request) =
            app.queue_request.as_ref().unwrap()
        else {
            panic!("reorder required")
        };
        assert_eq!(request.input_id, Some(vec![3; 16]));
        assert_eq!(request.after_input_id, Some(vec![1; 16]));
        assert_eq!(request.expected_order_revision, Some(9));
        assert_eq!(
            app.queue_entries[2].input_id,
            Some(vec![3; 16]),
            "client must wait for durable order receipt"
        );
    }
    #[test]
    fn changed_queue_projection_cannot_promote_a_different_input() {
        let mut app = App::new();
        app.view.connection = "connected";
        app.conversation_busy = true;
        app.conversation_cancellable = true;
        app.active_operation = Some(vec![9; 16]);
        app.queue_entries = (1..=3)
            .map(|id| asura_control::pb::ConversationQueueEntry {
                input_id: Some(vec![id; 16]),
                state: Some(1),
                text: Some(format!("input {id}")),
                ..Default::default()
            })
            .collect();
        draw(&mut app, "queue-stable", 100, 26);
        key(&mut app, KeyCode::Up);
        app.queue_entries.remove(0);
        key(&mut app, KeyCode::Enter);
        let super::super::super::queue::Request::Decision(decision) =
            app.take_queue_request(std::time::Instant::now()).unwrap()
        else {
            panic!("decision");
        };
        assert_eq!(decision.input_id, Some(vec![3; 16]));
        app.queue_busy = false;
        app.queue_retained = None;
        app.queue_entries.pop();
        key(&mut app, KeyCode::Enter);
        assert!(app.queue_request.is_none());
        assert_eq!(app.focus, Focus::Editor);
    }
    #[test]
    fn project_picker_uses_displayed_snapshot_and_validates_live_registry() {
        let mut app = App::new();
        app.projects = Some(vec![project(1, true), project(2, true)]);
        app.focus = Focus::Project;
        key(&mut app, KeyCode::Enter);
        key(&mut app, KeyCode::Down);
        app.projects.as_mut().unwrap().reverse();
        key(&mut app, KeyCode::Enter);
        assert_eq!(app.selected_project, Some([2; 16]));
        key(&mut app, KeyCode::Enter);
        app.projects.as_mut().unwrap()[0].current = Some(false);
        key(&mut app, KeyCode::Enter);
        assert!(app.picker.is_some());
        assert!(app.notice.contains("unavailable"));
    }
    #[test]
    fn selector_resize_and_long_list_keep_selection_visible() {
        let mut app = App::new();
        app.projects = Some((1..=64).map(|id| project(id, true)).collect());
        app.focus = Focus::Project;
        key(&mut app, KeyCode::Enter);
        key(&mut app, KeyCode::End);
        for (width, height) in [(100, 26), (40, 12), (30, 8), (8, 3), (0, 0), (100, 26)] {
            let screen = draw(&mut app, "resize", width, height);
            if width >= 30 && height >= 8 {
                assert!(screen.contains("Project64"));
            }
        }
        key(&mut app, KeyCode::Esc);
        assert!(app.picker.is_none());
        app.focus = Focus::Model;
        app.picker = Some(Picker::Models {
            selected: 63,
            result: Some(Ok(asura_control::pb::ModelsReply {
                models: (0..64)
                    .map(|index| asura_control::pb::ModelInventoryEntry {
                        selector: Some(format!("model{index}")),
                        status: Some(3),
                        ..Default::default()
                    })
                    .collect(),
                issues: vec![asura_control::pb::ModelInventoryIssue {
                    provider: Some("ollama".into()),
                    reason: Some("endpoint_unavailable".into()),
                }],
                ..Default::default()
            })),
        });
        for (width, height) in [(100, 26), (40, 12), (30, 8)] {
            let screen = draw(&mut app, "partial-models", width, height);
            assert!(screen.contains("model63"));
            if height >= 12 {
                assert!(screen.contains("Partial inventory"));
            }
        }
    }
}
