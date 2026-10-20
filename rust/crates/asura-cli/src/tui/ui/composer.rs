//! Local focus and selection. Requests continue through the canonical workers.
use super::*;
use crossterm::event::KeyEvent;

#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub(super) enum Focus {
    #[default]
    Editor,
    Project,
    Model,
    /// The thinking level item (thinking-level.md).
    Thinking,
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
    /// The five thinking levels (thinking-level.md).
    Thinking { selected: usize },
}
impl App {
    pub(super) fn close_picker(&mut self) {
        if self.picker_models_pending {
            self.models_cancel = true;
        }
        if matches!(self.picker, Some(Picker::Projects { .. })) {
            self.stop_edge_read();
        }
        self.picker = None;
    }
    pub(super) fn select_project(&mut self, id: [u8; 16]) -> bool {
        let row = self.project_row(id).cloned();
        self.select_project_row(id, row)
    }
    /// The selection guard and transition (CF06b). `row` comes from the
    /// window, the selected project, or an exact `only` read.
    pub(super) fn select_project_row(
        &mut self,
        id: [u8; 16],
        row: Option<asura_control::pb::ProjectReply>,
    ) -> bool {
        if self.conversation_busy
            || self.retained_request.is_some()
            || self.queue_busy
            || self.queue_retained.is_some()
            || !self.local_outbox.is_empty()
        {
            self.notice = "Finish active work before changing project".into();
            return false;
        }
        let row = row.as_ref();
        if row.is_none_or(|row| row.current != Some(true)) {
            self.notice = if row.is_some_and(crate::conversation::overlaps_asura_state) {
                format!(
                    "Project unavailable: {}",
                    crate::conversation::OVERLAPS_ASURA_STATE
                )
            } else {
                "Project unavailable; refresh with /project list".into()
            };
            return false;
        }
        self.paging.selected_row = (!self.in_window(id)).then(|| row.cloned()).flatten();
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
    pub(super) fn queue_focus_ids(&self) -> Vec<Vec<u8>> {
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
    /// Focus the queue's last unresolved item. False when the queue is empty.
    fn enter_queue_last(&mut self) -> bool {
        let ids = self.queue_focus_ids();
        let Some(id) = ids.last() else {
            return false;
        };
        self.queue_focused_id = Some(id.clone());
        self.focus = Focus::Queue(ids.len() - 1);
        true
    }
    /// Focus the conversation log at its latest row. False without a transcript.
    fn enter_log(&mut self) -> bool {
        if self.transcript.is_empty() {
            return false;
        }
        self.focus = Focus::Editor;
        self.history_focus = true;
        self.history_scroll = self.history_scroll_max;
        true
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
        if !self
            .unresolved()
            .into_iter()
            .any(|entry| entry.input_id.as_deref() == Some(id))
        {
            self.notice = "Selected input is no longer queued".into();
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
            if self.picker_model_save.is_some() || self.thinking_save.is_some() {
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
                    // CF06b: the selection reached an edge of the window.
                    if matches!(
                        key.code,
                        KeyCode::Up | KeyCode::Down | KeyCode::Home | KeyCode::End
                    ) {
                        self.request_edge_page(selected, count);
                    }
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
                Picker::Thinking { mut selected } => {
                    navigate(&mut selected, super::thinking::ROWS.len(), key.code);
                    self.picker = Some(Picker::Thinking { selected });
                    if key.code == KeyCode::Enter {
                        self.save_thinking(super::thinking::ROWS[selected].1, None);
                    }
                }
            }
            return true;
        }
        if key.modifiers == KeyModifiers::CONTROL && self.focus == Focus::Editor {
            match key.code {
                KeyCode::Up => {
                    self.enter_queue_last();
                    return true;
                }
                KeyCode::Down => {
                    self.focus = Focus::Project;
                    return true;
                }
                _ => {}
            }
        }
        // The focus chain runs log, queue, editor, status bar from top to
        // bottom. The log owns its own Shift+Down in `history_key`.
        if key.modifiers == KeyModifiers::SHIFT && matches!(key.code, KeyCode::Up | KeyCode::Down) {
            let up = key.code == KeyCode::Up;
            match self.focus {
                Focus::Editor if up => {
                    if !self.enter_queue_last() {
                        self.enter_log();
                    }
                }
                Focus::Editor => self.focus = Focus::Project,
                Focus::Project | Focus::Model | Focus::Thinking if up => self.focus = Focus::Editor,
                Focus::Project | Focus::Model | Focus::Thinking => {}
                Focus::Queue(_) if up => {
                    self.enter_log();
                }
                Focus::Queue(_) => self.focus = Focus::Editor,
            }
            return true;
        }
        if !key.modifiers.is_empty() {
            return self.focus != Focus::Editor;
        }
        match self.focus {
            // Browsing recall steps entries from any row. Otherwise Up and Down
            // move one visual row and act only at the draft's first or final row.
            Focus::Editor => match key.code {
                KeyCode::Up if self.history.is_browsing() || self.editor.on_first_visual_row() => {
                    self.recall(true);
                    true
                }
                KeyCode::Down if self.history.is_browsing() => {
                    self.recall(false);
                    true
                }
                KeyCode::Down if self.editor.on_final_visual_row() => {
                    self.focus = Focus::Project;
                    true
                }
                _ => false,
            },
            Focus::Project | Focus::Model | Focus::Thinking => {
                match key.code {
                    KeyCode::Up | KeyCode::Esc => self.focus = Focus::Editor,
                    // Project, model and thinking level, in a ring.
                    KeyCode::Right | KeyCode::Tab => {
                        self.focus = match self.focus {
                            Focus::Project => Focus::Model,
                            Focus::Model => Focus::Thinking,
                            _ => Focus::Project,
                        }
                    }
                    KeyCode::Left => {
                        self.focus = match self.focus {
                            Focus::Project => Focus::Thinking,
                            Focus::Thinking => Focus::Model,
                            _ => Focus::Project,
                        }
                    }
                    KeyCode::Enter if self.focus == Focus::Thinking => {
                        self.open_thinking_picker();
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
                    KeyCode::Up if selected == 0 && self.enter_log() => return true,
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
        if let Some(hint) = self.help_hint() {
            return hint;
        }
        if self.response_selection.is_some() {
            return "↑↓ responses · Enter activity · t thinking · PgUp/PgDn scroll · Esc input"
                .into();
        }
        if self.history_focus {
            let below = if self.queue_focus_ids().is_empty() {
                "input"
            } else {
                "queue"
            };
            return format!("↑↓ scroll history · PgUp/PgDn page · ⇧↓ {below} · Esc input");
        }
        if self.picker_model_save.is_some() {
            return "Saving model selection · draft preserved".into();
        }
        if self.thinking_save.is_some_and(|(_, picker)| picker) {
            return "Saving thinking level · draft preserved".into();
        }
        if self.picker.is_some() {
            return "↑↓ select · Enter apply · Esc close".into();
        }
        // Shift+Up names the pane it will focus; it is omitted with neither.
        let above = if !self.queue_focus_ids().is_empty() {
            " · ⇧↑ queue"
        } else if !self.transcript.is_empty() {
            " · ⇧↑ history"
        } else {
            ""
        };
        let history_above = if self.transcript.is_empty() {
            ""
        } else {
            " · ⇧↑ history"
        };
        match self.focus {
            Focus::Editor => format!(
                "{} · {newline_chord} newline · ↑↓ recall{above} · ⇧↓ status",
                if self.conversation_busy {
                    "↵ queue"
                } else {
                    "↵ send"
                }
            ),
            Focus::Project | Focus::Model | Focus::Thinking => {
                "←→ / Tab select item · Enter open · ↑ / Esc input".into()
            }
            Focus::Queue(_) if self.queue_busy => "Queue request pending · Esc input".into(),
            Focus::Queue(_) => format!(
                "↑↓ select · [ move up · ] move down · ↵ send now{history_above} · Esc input"
            ),
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
            Picker::Thinking { .. } => super::thinking::ROWS.len(),
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
                            } else if crate::conversation::overlaps_asura_state(row) {
                                " · unavailable: contains Asura state"
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
            Picker::Thinking { selected } => (self.thinking_rows(), *selected),
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
                        if let Some(step) = super::queue_wait_label(entry) {
                            format!("{step} · Esc holds · ")
                        } else if entry.state == Some(2) {
                            format!(
                                "Held ({}) · ",
                                entry
                                    .hold_reason
                                    .as_deref()
                                    .unwrap_or("reason unavailable")
                                    .replace('_', " ")
                            )
                        } else {
                            String::new()
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
pub(super) fn selection_style(style: Style) -> Style {
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
    use rat_text::TextPosition;
    use ratatui::{Terminal, backend::TestBackend};
    fn key(app: &mut App, code: KeyCode) {
        app.handle(Event::Key(KeyEvent::new(code, KeyModifiers::NONE)));
    }
    fn modified_key(app: &mut App, code: KeyCode, modifiers: KeyModifiers) {
        app.handle(Event::Key(KeyEvent::new(code, modifiers)));
    }
    fn queued(id: u8) -> asura_control::pb::ConversationQueueEntry {
        asura_control::pb::ConversationQueueEntry {
            input_id: Some(vec![id; 16]),
            state: Some(1),
            text: Some(format!("queued {id}")),
            ..Default::default()
        }
    }
    fn with_long_log(app: &mut App) {
        for index in 0..8 {
            app.append_response(format!("prompt {index}"), None);
            app.transcript[index].1 = format!("answer {index} with enough text to fill a row");
            app.rendered_responses[index] = markdown::render(&app.transcript[index].1);
        }
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
        assert!(idle.contains("↑↓ recall"));
        assert!(idle.contains("⇧↓ status"));
        assert!(
            !idle.contains("⇧↑"),
            "Shift+Up does nothing without queue or log"
        );
        assert!(idle.contains(&format!("{newline} newline")));
        assert!(!idle.contains("Ctrl+"));
        assert!(draw(&mut app, "key-hints", 110, 26).contains(newline));

        app.conversation_busy = true;
        let busy = app.hints();
        assert!(busy.contains(&format!("{newline} newline")));
        assert!(busy.contains("↵ queue"));

        app.append_response("prompt".into(), None);
        assert!(app.hints().contains("⇧↑ history"));
        app.queue_entries.push(queued(7));
        assert!(app.hints().contains("⇧↑ queue"));
        modified_key(&mut app, KeyCode::Up, KeyModifiers::SHIFT);
        assert!(app.hints().contains("⇧↑ history"));
        modified_key(&mut app, KeyCode::Up, KeyModifiers::SHIFT);
        let log = app.hints();
        assert!(log.contains("↑↓ scroll history") && log.contains("⇧↓ queue"));
        app.queue_entries.clear();
        assert!(app.hints().contains("⇧↓ input"));
    }
    #[test]
    fn status_project_picker_preserves_draft_and_rejects_stale_or_busy() {
        let mut app = App::new();
        app.projects = Some(vec![project(1, true), project(2, false), project(3, true)]);
        app.selected_project = Some([1; 16]);
        app.insert("draft");
        draw(&mut app, "editor", 100, 26);
        modified_key(&mut app, KeyCode::Down, KeyModifiers::CONTROL);
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
        modified_key(&mut app, KeyCode::Down, KeyModifiers::CONTROL);
        assert_eq!(app.focus, Focus::Project);
        key(&mut app, KeyCode::Esc);
        assert_eq!(app.focus, Focus::Editor);
    }
    #[test]
    fn plain_down_enters_status_only_from_final_visual_input_row() {
        let mut app = App::new();
        draw(&mut app, "empty-status-boundary", 80, 24);
        key(&mut app, KeyCode::Down);
        assert_eq!(app.focus, Focus::Project);
        key(&mut app, KeyCode::Up);
        app.insert("first line\nsecond line with 🦀 and enough text to wrap at narrow width");
        draw(&mut app, "multiline-status-boundary", 32, 24);
        modified_key(&mut app, KeyCode::Home, KeyModifiers::CONTROL);
        draw(&mut app, "multiline-start", 32, 24);
        key(&mut app, KeyCode::Down);
        assert_eq!(app.focus, Focus::Editor);
        modified_key(&mut app, KeyCode::End, KeyModifiers::CONTROL);
        draw(&mut app, "multiline-end", 32, 24);
        let draft = app.draft();
        key(&mut app, KeyCode::Down);
        assert_eq!(app.focus, Focus::Project);
        assert_eq!(app.draft(), draft);
        key(&mut app, KeyCode::Esc);
        draw(&mut app, "resized-status-boundary", 100, 24);
        key(&mut app, KeyCode::Down);
        assert_eq!(app.focus, Focus::Project);
        assert_eq!(app.draft(), draft);
    }
    #[test]
    fn plain_down_restores_recall_before_entering_status() {
        let mut app = App::new();
        app.history.record("first");
        app.history.record("second\nline");
        app.insert("unsent\n🦀 draft");
        draw(&mut app, "recall", 80, 24);
        key(&mut app, KeyCode::Up);
        assert_eq!(
            app.draft(),
            "unsent\n🦀 draft",
            "Up first moves to the top row"
        );
        key(&mut app, KeyCode::Up);
        assert_eq!(app.draft(), "second\nline");
        // Browsing steps entries even though the cursor is on the final row.
        key(&mut app, KeyCode::Up);
        assert_eq!(app.draft(), "first");
        key(&mut app, KeyCode::Up);
        assert_eq!(app.draft(), "first");
        key(&mut app, KeyCode::Down);
        assert_eq!(app.draft(), "second\nline");
        key(&mut app, KeyCode::Down);
        assert_eq!(app.draft(), "unsent\n🦀 draft");
        assert_eq!(app.focus, Focus::Editor);
        draw(&mut app, "restored-recall", 80, 24);
        key(&mut app, KeyCode::Down);
        assert_eq!(app.draft(), "unsent\n🦀 draft");
        assert_eq!(app.focus, Focus::Project);
        assert!(!app.history_focus);
    }
    #[test]
    fn shift_horizontal_selection_does_not_scroll_history() {
        let mut app = App::new();
        app.append_response("prompt".into(), None);
        app.insert("draft");
        draw(&mut app, "selection", 80, 24);
        modified_key(&mut app, KeyCode::Left, KeyModifiers::SHIFT);
        assert!(!app.history_focus);
        assert_eq!(app.draft(), "draft");
        app.insert("X");
        assert_eq!(
            app.draft(),
            "drafX",
            "Shift+Left must select the last character"
        );
        modified_key(&mut app, KeyCode::Up, KeyModifiers::SHIFT);
        assert!(app.history_focus);
        modified_key(&mut app, KeyCode::Left, KeyModifiers::SHIFT);
        assert!(!app.history_focus);
        assert_eq!(app.draft(), "drafX");
    }
    #[test]
    fn rn1_up_and_down_move_one_visual_row_then_recall_or_enter_status() {
        let mut app = App::new();
        app.history.record("earlier");
        app.insert("alpha one\nbravo two");
        draw(&mut app, "rows", 80, 24);
        for _ in 0..3 {
            key(&mut app, KeyCode::Left);
        }
        assert_eq!(app.editor.selection_points().1, TextPosition::new(6, 1));
        key(&mut app, KeyCode::Up);
        draw(&mut app, "rows", 80, 24);
        assert_eq!(app.editor.selection_points().1, TextPosition::new(6, 0));
        assert_eq!(app.draft(), "alpha one\nbravo two");
        key(&mut app, KeyCode::Down);
        draw(&mut app, "rows", 80, 24);
        assert_eq!(app.editor.selection_points().1, TextPosition::new(6, 1));
        assert_eq!(app.focus, Focus::Editor);
        key(&mut app, KeyCode::Up);
        draw(&mut app, "rows", 80, 24);
        key(&mut app, KeyCode::Up);
        assert_eq!(app.draft(), "earlier", "Up on the first row starts recall");
        key(&mut app, KeyCode::Down);
        assert_eq!(app.draft(), "alpha one\nbravo two");
        draw(&mut app, "rows", 80, 24);
        key(&mut app, KeyCode::Down);
        assert_eq!(app.focus, Focus::Project, "Down on the final row");
        key(&mut app, KeyCode::Up);

        // Hard wrapping: the 32-column terminal gives the editor 28 columns.
        let mut app = App::new();
        app.history.record("earlier");
        let text = "abcdefghijklmnopqrstuvwxyz0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefgh";
        app.insert(text);
        draw(&mut app, "wrapped-rows", 32, 24);
        assert_eq!(app.editor.selection_points().1, TextPosition::new(70, 0));
        key(&mut app, KeyCode::Up);
        draw(&mut app, "wrapped-rows", 32, 24);
        assert_eq!(app.editor.selection_points().1, TextPosition::new(42, 0));
        key(&mut app, KeyCode::Up);
        draw(&mut app, "wrapped-rows", 32, 24);
        assert_eq!(app.editor.selection_points().1, TextPosition::new(14, 0));
        assert_eq!(app.draft(), text);
        key(&mut app, KeyCode::Down);
        draw(&mut app, "wrapped-rows", 32, 24);
        assert_eq!(app.editor.selection_points().1, TextPosition::new(42, 0));
        assert_eq!(app.focus, Focus::Editor);
        key(&mut app, KeyCode::Up);
        draw(&mut app, "wrapped-rows", 32, 24);
        key(&mut app, KeyCode::Up);
        assert_eq!(app.draft(), "earlier");
        key(&mut app, KeyCode::Down);
        assert_eq!(app.draft(), text);
        draw(&mut app, "wrapped-rows", 32, 24);
        key(&mut app, KeyCode::Down);
        assert_eq!(app.focus, Focus::Project);
        assert_eq!(app.draft(), text);
        key(&mut app, KeyCode::Esc);

        // A scrolled editor hides the first row; Up moves rows until it shows.
        app.editor = Editor::new();
        let lines = (0..10).map(|n| format!("line {n}")).collect::<Vec<_>>();
        app.insert(&lines.join("\n"));
        draw(&mut app, "scrolled-rows", 32, 24);
        assert!(!app.editor.first_row_visible());
        for row in (0..9).rev() {
            key(&mut app, KeyCode::Up);
            draw(&mut app, "scrolled-rows", 32, 24);
            assert_eq!(app.editor.selection_points().1.y, row);
        }
        assert!(app.editor.first_row_visible());
        assert!(!app.history.is_browsing());
        key(&mut app, KeyCode::Up);
        assert_eq!(app.draft(), "earlier");
    }
    #[test]
    fn rn2_browsing_steps_entries_from_any_row_until_an_edit() {
        let mut app = App::new();
        app.history.record("older");
        app.history.record("one\ntwo\nthree");
        app.insert("draft 界");
        draw(&mut app, "browse", 80, 24);
        key(&mut app, KeyCode::Up);
        assert_eq!(app.draft(), "one\ntwo\nthree");
        draw(&mut app, "browse", 80, 24);
        // Cursor movement does not end browsing; Up still steps entries.
        key(&mut app, KeyCode::Left);
        modified_key(&mut app, KeyCode::Left, KeyModifiers::ALT);
        key(&mut app, KeyCode::Up);
        assert_eq!(app.draft(), "older");
        key(&mut app, KeyCode::Down);
        assert_eq!(app.draft(), "one\ntwo\nthree");
        draw(&mut app, "browse", 80, 24);
        // An edit ends browsing. Up then moves a row instead of recalling.
        key(&mut app, KeyCode::Char('x'));
        assert_eq!(app.draft(), "one\ntwo\nthreex");
        assert!(!app.history.is_browsing());
        draw(&mut app, "browse", 80, 24);
        key(&mut app, KeyCode::Up);
        assert_eq!(app.draft(), "one\ntwo\nthreex");
        assert_eq!(app.editor.selection_points().1.y, 1);
        key(&mut app, KeyCode::Down);
        draw(&mut app, "browse", 80, 24);
        key(&mut app, KeyCode::Down);
        assert_eq!(
            app.focus,
            Focus::Project,
            "no longer browsing at the final row"
        );
        key(&mut app, KeyCode::Esc);
        // The edited text becomes the saved draft of the next recall.
        modified_key(&mut app, KeyCode::Char('p'), KeyModifiers::CONTROL);
        assert_eq!(app.draft(), "one\ntwo\nthree");
        modified_key(&mut app, KeyCode::Char('n'), KeyModifiers::CONTROL);
        assert_eq!(app.draft(), "one\ntwo\nthreex");
        // Option+Backspace is an edit too.
        modified_key(&mut app, KeyCode::Char('p'), KeyModifiers::CONTROL);
        modified_key(&mut app, KeyCode::Backspace, KeyModifiers::ALT);
        assert_eq!(app.draft(), "one\ntwo\n");
        assert!(!app.history.is_browsing());
    }
    #[test]
    fn rn3_word_keys_reach_the_editor_from_the_composer() {
        let mut app = App::new();
        app.insert("héllo wörld");
        draw(&mut app, "words", 80, 24);
        for modifiers in [KeyModifiers::ALT, KeyModifiers::CONTROL] {
            modified_key(&mut app, KeyCode::Left, modifiers);
            assert_eq!(app.editor.selection_points().1, TextPosition::new(6, 0));
            modified_key(&mut app, KeyCode::Right, modifiers);
            assert_eq!(app.editor.selection_points().1, TextPosition::new(11, 0));
        }
        modified_key(&mut app, KeyCode::Char('b'), KeyModifiers::ALT);
        modified_key(&mut app, KeyCode::Char('b'), KeyModifiers::ALT);
        modified_key(&mut app, KeyCode::Char('f'), KeyModifiers::ALT);
        assert_eq!(app.editor.selection_points().1, TextPosition::new(5, 0));
        modified_key(
            &mut app,
            KeyCode::Right,
            KeyModifiers::ALT | KeyModifiers::SHIFT,
        );
        assert_eq!(
            app.editor.selection_points(),
            (TextPosition::new(5, 0), TextPosition::new(11, 0))
        );
        assert_eq!(app.focus, Focus::Editor);
        assert_eq!(app.draft(), "héllo wörld");
        modified_key(&mut app, KeyCode::End, KeyModifiers::CONTROL);
        modified_key(&mut app, KeyCode::Backspace, KeyModifiers::ALT);
        assert_eq!(app.draft(), "héllo ");
    }
    #[test]
    fn fn1_shift_up_enters_queue_then_log_and_is_ignored_with_neither() {
        let mut app = App::new();
        app.insert("draft");
        modified_key(&mut app, KeyCode::Up, KeyModifiers::SHIFT);
        assert!(!app.history_focus);
        assert_eq!(app.focus, Focus::Editor);
        assert_eq!(app.draft(), "draft");
        // Queue without transcript: the log is outside the chain.
        app.queue_entries = vec![queued(1), queued(2)];
        draw(&mut app, "queue-only", 80, 24);
        modified_key(&mut app, KeyCode::Up, KeyModifiers::SHIFT);
        assert_eq!(app.focus, Focus::Queue(1), "the queue's last item");
        key(&mut app, KeyCode::Up);
        assert_eq!(app.focus, Focus::Queue(0));
        key(&mut app, KeyCode::Up);
        assert_eq!(app.focus, Focus::Queue(0));
        modified_key(&mut app, KeyCode::Up, KeyModifiers::SHIFT);
        assert_eq!(app.focus, Focus::Queue(0));
        assert!(!app.history_focus);
        key(&mut app, KeyCode::Esc);
        // Queue and transcript: Shift+Up enters the queue before the log.
        with_long_log(&mut app);
        draw(&mut app, "queue-and-log", 80, 20);
        assert!(app.history_scroll_max > 0);
        modified_key(&mut app, KeyCode::Up, KeyModifiers::SHIFT);
        assert_eq!(app.focus, Focus::Queue(1));
        assert!(!app.history_focus);
        key(&mut app, KeyCode::Up);
        key(&mut app, KeyCode::Up);
        assert!(app.history_focus, "Up on the first item enters the log");
        assert_eq!(app.focus, Focus::Editor);
        assert_eq!(app.history_scroll, app.history_scroll_max, "latest row");
        key(&mut app, KeyCode::Esc);
        modified_key(&mut app, KeyCode::Up, KeyModifiers::SHIFT);
        modified_key(&mut app, KeyCode::Up, KeyModifiers::SHIFT);
        assert!(app.history_focus, "Shift+Up from the queue enters the log");
        assert_eq!(app.history_scroll, app.history_scroll_max);
        key(&mut app, KeyCode::Esc);
        // Transcript without queue: Shift+Up enters the log directly.
        app.queue_entries.clear();
        modified_key(&mut app, KeyCode::Up, KeyModifiers::SHIFT);
        assert!(app.history_focus);
        assert_eq!(app.history_scroll, app.history_scroll_max);
        assert_eq!(app.draft(), "draft");
    }
    #[test]
    fn fn2_shift_down_steps_log_queue_editor_status_and_escape_returns() {
        let mut app = App::new();
        app.insert("draft");
        with_long_log(&mut app);
        app.queue_entries = vec![queued(1), queued(2)];
        draw(&mut app, "focus-down", 80, 20);
        modified_key(&mut app, KeyCode::Up, KeyModifiers::SHIFT);
        modified_key(&mut app, KeyCode::Up, KeyModifiers::SHIFT);
        assert!(app.history_focus);
        modified_key(&mut app, KeyCode::Down, KeyModifiers::SHIFT);
        assert!(!app.history_focus);
        assert_eq!(app.focus, Focus::Queue(0), "the queue's first item");
        modified_key(&mut app, KeyCode::Down, KeyModifiers::SHIFT);
        assert_eq!(app.focus, Focus::Editor);
        assert!(!app.history_focus);
        modified_key(&mut app, KeyCode::Down, KeyModifiers::SHIFT);
        assert_eq!(app.focus, Focus::Project);
        modified_key(&mut app, KeyCode::Up, KeyModifiers::SHIFT);
        assert_eq!(app.focus, Focus::Editor);
        // Escape returns from the queue and from the log.
        modified_key(&mut app, KeyCode::Up, KeyModifiers::SHIFT);
        assert_eq!(app.focus, Focus::Queue(1));
        key(&mut app, KeyCode::Down);
        assert_eq!(app.focus, Focus::Editor, "Down past the last item");
        modified_key(&mut app, KeyCode::Up, KeyModifiers::SHIFT);
        key(&mut app, KeyCode::Esc);
        assert_eq!(app.focus, Focus::Editor);
        modified_key(&mut app, KeyCode::Up, KeyModifiers::SHIFT);
        modified_key(&mut app, KeyCode::Up, KeyModifiers::SHIFT);
        key(&mut app, KeyCode::Esc);
        assert!(!app.history_focus);
        assert_eq!(app.focus, Focus::Editor);
        // An empty queue is skipped on the way down.
        app.queue_entries.clear();
        modified_key(&mut app, KeyCode::Up, KeyModifiers::SHIFT);
        assert!(app.history_focus);
        modified_key(&mut app, KeyCode::Down, KeyModifiers::SHIFT);
        assert!(!app.history_focus);
        assert_eq!(app.focus, Focus::Editor);
        // Status returns with Up and Escape too.
        modified_key(&mut app, KeyCode::Down, KeyModifiers::SHIFT);
        key(&mut app, KeyCode::Up);
        assert_eq!(app.focus, Focus::Editor);
        modified_key(&mut app, KeyCode::Down, KeyModifiers::SHIFT);
        key(&mut app, KeyCode::Esc);
        assert_eq!(app.focus, Focus::Editor);
        assert_eq!(app.draft(), "draft");
    }
    #[test]
    fn fn3_log_arrows_scroll_one_row_and_clamp_without_recall() {
        let mut app = App::new();
        app.history.record("submitted");
        with_long_log(&mut app);
        app.insert("unsent draft");
        draw(&mut app, "history-bottom", 80, 20);
        let max = app.history_scroll_max;
        assert!(max > 2);
        modified_key(&mut app, KeyCode::Up, KeyModifiers::SHIFT);
        assert!(app.history_focus);
        assert_eq!(app.history_scroll, max);
        key(&mut app, KeyCode::Down);
        assert_eq!(app.history_scroll, max, "Down clamps at the latest row");
        key(&mut app, KeyCode::Up);
        assert_eq!(app.history_scroll, max - 1);
        key(&mut app, KeyCode::Up);
        assert_eq!(app.history_scroll, max - 2);
        key(&mut app, KeyCode::Down);
        assert_eq!(app.history_scroll, max - 1);
        modified_key(&mut app, KeyCode::Up, KeyModifiers::SHIFT);
        assert_eq!(
            app.history_scroll,
            max - 1,
            "Shift+Up in the log is a no-op"
        );
        assert!(app.history_focus);
        key(&mut app, KeyCode::Home);
        assert_eq!(app.history_scroll, 0);
        key(&mut app, KeyCode::Up);
        assert_eq!(app.history_scroll, 0, "Up clamps at the top");
        assert!(draw(&mut app, "history-top", 80, 20).contains("prompt 0"));
        key(&mut app, KeyCode::PageDown);
        assert_eq!(app.history_scroll, app.history_page.min(max));
        key(&mut app, KeyCode::PageUp);
        assert_eq!(app.history_scroll, 0);
        key(&mut app, KeyCode::End);
        assert_eq!(app.history_scroll, max);
        assert!(app.history_focus);
        assert_eq!(app.draft(), "unsent draft", "log keys never recall");
        assert!(!app.history.is_browsing());
        key(&mut app, KeyCode::Esc);
        assert!(!app.history_focus);
        assert_eq!(app.draft(), "unsent draft");
    }
    #[test]
    fn fn4_focus_round_trips_keep_draft_cursor_selection_and_recall() {
        let mut app = App::new();
        app.history.record("older 界");
        with_long_log(&mut app);
        app.queue_entries = vec![queued(1)];
        app.insert("first 🦀\nsecond");
        draw(&mut app, "round-trip", 80, 20);
        key(&mut app, KeyCode::Up);
        modified_key(&mut app, KeyCode::Right, KeyModifiers::SHIFT);
        modified_key(&mut app, KeyCode::Right, KeyModifiers::SHIFT);
        let points = app.editor.selection_points();
        assert_ne!(points.0, points.1);
        let journey = [
            (KeyCode::Up, KeyModifiers::SHIFT),
            (KeyCode::Up, KeyModifiers::SHIFT),
            (KeyCode::Up, KeyModifiers::NONE),
            (KeyCode::Down, KeyModifiers::SHIFT),
            (KeyCode::Down, KeyModifiers::SHIFT),
            (KeyCode::Down, KeyModifiers::SHIFT),
            (KeyCode::Up, KeyModifiers::SHIFT),
            (KeyCode::Up, KeyModifiers::CONTROL),
            (KeyCode::Esc, KeyModifiers::NONE),
            (KeyCode::Down, KeyModifiers::CONTROL),
            (KeyCode::Esc, KeyModifiers::NONE),
        ];
        for (code, modifiers) in journey {
            modified_key(&mut app, code, modifiers);
            draw(&mut app, "round-trip", 80, 20);
        }
        assert_eq!(app.focus, Focus::Editor);
        assert!(!app.history_focus);
        assert_eq!(app.draft(), "first 🦀\nsecond");
        assert_eq!(app.editor.selection_points(), points);
        // Recall state survives the same round trip.
        app.insert("edited");
        let draft = app.draft();
        modified_key(&mut app, KeyCode::Char('p'), KeyModifiers::CONTROL);
        assert_eq!(app.draft(), "older 界");
        let points = app.editor.selection_points();
        for (code, modifiers) in journey {
            modified_key(&mut app, code, modifiers);
            draw(&mut app, "round-trip", 80, 20);
        }
        assert!(app.history.is_browsing());
        assert_eq!(app.draft(), "older 界");
        assert_eq!(app.editor.selection_points(), points);
        key(&mut app, KeyCode::Down);
        assert_eq!(app.draft(), draft, "Down restores the saved draft");
    }
    #[test]
    fn held_queue_row_explains_known_and_unknown_causes() {
        let mut app = App::new();
        app.queue_entries = vec![
            asura_control::pb::ConversationQueueEntry {
                input_id: Some(vec![1; 16]),
                state: Some(2),
                text: Some("first".into()),
                hold_reason: Some("model_unavailable".into()),
                ..Default::default()
            },
            asura_control::pb::ConversationQueueEntry {
                input_id: Some(vec![2; 16]),
                state: Some(2),
                text: Some("second".into()),
                ..Default::default()
            },
        ];
        let screen = draw(&mut app, "held", 100, 26);
        assert!(screen.contains("Held (model unavailable) · first"));
        assert!(screen.contains("Held (reason unavailable) · second"));
    }
    /// WS-01 (TUI): a queued row names its pre-admission waiting step.
    #[test]
    fn preparing_queue_row_names_the_waiting_step_and_cancel_hint() {
        let mut app = App::new();
        let row = |id: u8, state: u32, step: Option<i32>, text: &str| {
            asura_control::pb::ConversationQueueEntry {
                input_id: Some(vec![id; 16]),
                state: Some(state),
                text: Some(text.into()),
                waiting_step: step,
                ..Default::default()
            }
        };
        app.queue_entries = vec![row(1, 1, Some(2), "first"), row(2, 1, None, "second")];
        let screen = draw(&mut app, "starting", 100, 26);
        assert!(screen.contains(">> Starting model · Esc holds · first"));
        assert!(screen.contains(">> second"));
        app.queue_entries = vec![row(1, 1, Some(1), "first")];
        let screen = draw(&mut app, "preparing", 100, 26);
        assert!(screen.contains(">> Preparing model · Esc holds · first"));
        // Only a Queued row can name a step.
        app.queue_entries = vec![row(1, 2, Some(2), "first")];
        let screen = draw(&mut app, "held-step", 100, 26);
        assert!(screen.contains("Held (reason unavailable) · first"));
        assert!(!screen.contains("Starting model"));
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
        modified_key(&mut app, KeyCode::Up, KeyModifiers::CONTROL);
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
        modified_key(&mut app, KeyCode::Up, KeyModifiers::CONTROL);
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
    /// SI-U5: the picker and an explicit selection name the overlap.
    #[test]
    fn overlapping_project_shows_its_reason_and_is_not_selected() {
        let mut app = App::new();
        let mut overlapping = project(2, false);
        overlapping.error = Some(crate::conversation::OVERLAPS_ASURA_STATE.into());
        app.projects = Some(vec![project(1, true), overlapping]);
        app.focus = Focus::Project;
        key(&mut app, KeyCode::Enter);
        assert!(draw(&mut app, "picker", 100, 26).contains("unavailable: contains Asura state"));
        key(&mut app, KeyCode::Down);
        key(&mut app, KeyCode::Enter);
        assert_eq!(app.selected_project, None);
        assert!(
            app.notice
                .contains("Project unavailable: project_overlaps_asura_state"),
            "{}",
            app.notice
        );
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
