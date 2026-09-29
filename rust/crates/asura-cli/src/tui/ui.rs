//! Local draft and literal presentation of read-only service observations.
mod activity;
mod composer;
mod markdown;
use super::editor::{CommandHeader, Editor, MAX_DRAFT_BYTES};
use composer::{Focus, Picker};
use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::{
    Frame,
    layout::Rect,
    style::{Color, Style},
    text::{Line, Span, Text},
    widgets::{Clear, Paragraph, Wrap},
};
use unicode_segmentation::UnicodeSegmentation;

#[derive(Clone)]
pub struct ServiceDetails {
    pub owned: bool,
    pub uptime_ms: u64,
    pub stored_memory: asura_control::pb::StoredMemoryStatus,
}
#[derive(Clone)]
pub struct View {
    pub configured_model: Option<String>,
    pub service: Option<ServiceDetails>,
    pub connection: &'static str,
    pub lifecycle: &'static str,
    pub installation: &'static str,
    pub reason: &'static str,
    pub epoch: Option<String>,
}

fn conversation_reason(reason: i32) -> &'static str {
    match reason {
        0 => "none",
        1 => "model unavailable",
        2 => "input limit",
        3 => "output limit",
        4 => "context limit",
        5 => "timeout",
        6 => "refusal",
        7 => "cancelled",
        8 => "protocol fault",
        9 => "helper failed",
        10 => "internal error",
        11 => "interrupted",
        12 => "result expired",
        _ => "unknown",
    }
}

const LOCAL_COMMANDS: [&str; 14] = [
    "/help", "/quit", "/exit", "/config", "/init", "/project", "/cancel", "/observe", "/retry",
    "/queue", "/models", "/tools", "/audit", "/new",
];

enum Overlay {
    Commands(CommandHeader, Option<usize>),
    Help(u16),
    ProjectOffer(String, bool),
    Config(String, u16),
    Models(super::models::Outcome, u16),
    Tools(u16),
    Audit(super::queue_watch::Scope, String, u16),
    Exit(bool),
    Paste(Option<bool>),
}
struct Capture {
    text: String,
    invalid: bool,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LocalInputState {
    Waiting,
    Sending,
    Unconfirmed,
    Rejected,
}
#[derive(Clone, Debug)]
struct LocalInput {
    request_id: [u8; 16],
    project: [u8; 16],
    text: String,
    state: LocalInputState,
}
pub struct App {
    editor: Editor,
    focus: Focus,
    queue_focused_id: Option<Vec<u8>>,
    picker: Option<Picker>,
    picker_models_pending: bool,
    picker_model_save: Option<String>,
    history: super::history::History,
    view: View,
    notice: String,
    overlay: Option<Overlay>,
    capture: Option<Capture>,
    audit_request: Option<(super::queue_watch::Scope, u32)>,
    audit_scope: Option<super::queue_watch::Scope>,
    audit_draft: Option<String>,
    audit_cancel: bool,
    audit_result: Option<(super::queue_watch::Scope, String)>,
    models_request: bool,
    models_cancel: bool,
    models_draft: Option<String>,
    models_result: Option<super::models::Outcome>,
    config_request: Option<super::config::Request>,
    config_draft: Option<String>,
    config_result: Option<String>,
    project_admin_request: Option<super::project_admin::Request>,
    project_admin_retained: Option<super::project_admin::Request>,
    project_admin_busy: bool,
    editor_generation: u64,
    result_title: &'static str,
    panel_scroll_max: u16,
    conversation_request: Option<super::conversation::Request>,
    retained_request: Option<super::conversation::Request>,
    retained_preserve_draft: bool,
    conversation_draft: Option<String>,
    conversation_editor_draft: Option<String>,
    selected_project: Option<[u8; 16]>,
    projects: Option<Vec<asura_control::pb::ProjectReply>>,
    launch_directory: Option<String>,
    project_discovery_started: bool,
    project_offer_seen: bool,
    project_offer: Option<String>,
    observed_model: Option<String>,
    model_context: Option<asura_control::pb::ModelContext>,
    context: super::context::Model,
    conversation_id: Option<Vec<u8>>,
    conversation_generation: u64,
    new_conversation_intent: Option<[u8; 16]>,
    restoration_scope: Option<([u8; 16], [u8; 16])>,
    restoration_ready: bool,
    pending_restored_operation: Option<[u8; 16]>,
    conversation_busy: bool,
    conversation_cancellable: bool,
    cancel_request: bool,
    active_operation: Option<Vec<u8>>,
    queue_request: Option<super::queue::Request>,
    queue_request_scope: Option<super::queue_watch::Scope>,
    queue_last_epoch: Option<String>,
    queue_retained: Option<super::queue::Request>,
    queue_editor: Option<String>,
    queue_busy: bool,
    queue_show: bool,
    local_outbox: Vec<LocalInput>,
    queue_order_revision: Option<u64>,
    queue_entries: Vec<asura_control::pb::ConversationQueueEntry>,
    queue_projection: super::queue_watch::Projection,
    queue_observed: Vec<Vec<u8>>,
    queue_history_scope: Option<super::queue_watch::Scope>,
    queue_watermark: Option<u64>,
    queue_follow_inputs: Vec<Vec<u8>>,
    transcript: Vec<(String, String)>,
    rendered_responses: Vec<Text<'static>>,
    activity: Vec<activity::Activity>,
    response_selection: Option<usize>,
    response_scroll: usize,
    response_scroll_max: usize,
    response_page: usize,
    history_focus: bool,
    history_scroll: usize,
    history_scroll_max: usize,
    history_page: usize,
}
impl App {
    pub fn new() -> Self {
        Self {
            editor: Editor::new(),
            focus: Focus::Editor,
            queue_focused_id: None,
            picker: None,
            picker_models_pending: false,
            picker_model_save: None,
            history: super::history::History::new(),
            view: View {
                configured_model: None,
                service: None,
                connection: "connecting",
                lifecycle: "unavailable",
                installation: "unavailable",
                reason: "",
                epoch: None,
            },
            notice: String::new(),
            overlay: None,
            capture: None,
            audit_request: None,
            audit_scope: None,
            audit_draft: None,
            audit_cancel: false,
            audit_result: None,
            models_request: false,
            models_cancel: false,
            models_draft: None,
            models_result: None,
            config_request: None,
            config_draft: None,
            config_result: None,
            project_admin_request: None,
            project_admin_retained: None,
            project_admin_busy: false,
            editor_generation: 0,
            result_title: "Configuration",
            panel_scroll_max: 0,
            retained_request: None,
            retained_preserve_draft: false,
            conversation_request: None,
            conversation_draft: None,
            conversation_editor_draft: None,
            selected_project: None,
            projects: None,
            launch_directory: None,
            project_discovery_started: false,
            project_offer_seen: false,
            project_offer: None,
            observed_model: None,
            model_context: None,
            context: super::context::Model::default(),
            conversation_id: None,
            conversation_generation: 0,
            new_conversation_intent: None,
            restoration_scope: None,
            restoration_ready: false,
            pending_restored_operation: None,
            conversation_busy: false,
            conversation_cancellable: false,
            cancel_request: false,
            active_operation: None,
            queue_request: None,
            queue_request_scope: None,
            queue_last_epoch: None,
            queue_retained: None,
            queue_editor: None,
            queue_busy: false,
            queue_show: false,
            local_outbox: Vec::new(),
            queue_order_revision: None,
            queue_entries: Vec::new(),
            queue_projection: Default::default(),
            queue_observed: Vec::new(),
            queue_history_scope: None,
            queue_watermark: None,
            queue_follow_inputs: Vec::new(),
            transcript: Vec::new(),
            rendered_responses: Vec::new(),
            activity: Vec::new(),
            response_selection: None,
            response_scroll: 0,
            response_scroll_max: 0,
            response_page: 1,
            history_focus: false,
            history_scroll: 0,
            history_scroll_max: 0,
            history_page: 1,
        }
    }
    pub(super) fn take_queue_request(
        &mut self,
        _now: std::time::Instant,
    ) -> Option<super::queue::Request> {
        if self.queue_request.is_none()
            && !self.queue_busy
            && self.queue_retained.is_none()
            && self.queue_scope().is_some()
            && let Some(index) = self
                .local_outbox
                .iter()
                .position(|item| item.state == LocalInputState::Waiting)
        {
            let item = &mut self.local_outbox[index];
            if self.selected_project != Some(item.project) {
                item.state = LocalInputState::Rejected;
                self.notice = "Local input project changed; text retained".into();
            } else {
                let request =
                    super::queue::Request::Submit(asura_control::pb::ConversationQueueSubmit {
                        request_id: Some(item.request_id.to_vec()),
                        project_id: Some(item.project.to_vec()),
                        conversation_id: self.conversation_id.clone(),
                        expected_generation: Some(self.conversation_generation),
                        new_conversation: Some(self.conversation_id.is_none()),
                        prompt: Some(item.text.clone()),
                    });
                item.state = LocalInputState::Sending;
                self.queue_retained = Some(request.clone());
                self.queue_busy = true;
                self.queue_request = Some(request);
            }
        }
        let request = self.queue_request.take()?;
        self.synchronize_queue_history_scope();
        self.queue_request_scope = self.queue_identity_scope();
        Some(request)
    }
    fn queue_identity_scope(&self) -> Option<super::queue_watch::Scope> {
        Some(super::queue_watch::Scope {
            project: self.selected_project?,
            epoch: self
                .view
                .epoch
                .clone()
                .or_else(|| self.queue_last_epoch.clone())?,
        })
    }

    pub(super) fn queue_scope(&self) -> Option<super::queue_watch::Scope> {
        if self.view.connection != "connected" || self.view.installation != "graph_ready" {
            return None;
        }
        Some(super::queue_watch::Scope {
            project: self.selected_project?,
            epoch: self.view.epoch.clone()?,
        })
    }
    pub(super) fn refresh_queue_scope(&mut self, now: std::time::Instant) -> bool {
        self.synchronize_queue_history_scope();
        let changed = self.queue_projection.scope(self.queue_scope());
        if changed {
            self.queue_order_revision = None;
        }
        let expired = self.queue_projection.expire(now);
        if changed && matches!(self.focus, Focus::Queue(_)) {
            self.focus = Focus::Editor;
        }
        if changed || expired {
            self.queue_entries = self.queue_projection.entries();
        }
        changed || expired
    }
    pub(super) fn queue_observed(&mut self, value: super::queue_watch::Update) -> bool {
        self.synchronize_queue_history_scope();
        let scope_changed = self.queue_projection.scope(self.queue_scope());
        if scope_changed {
            self.queue_entries.clear();
            self.queue_order_revision = None;
            if matches!(self.focus, Focus::Queue(_)) {
                self.focus = Focus::Editor;
            }
        }
        let changed = self.queue_projection.apply(value) || scope_changed;
        if self.queue_projection.has_snapshot() {
            if self.queue_projection.order_revision() > self.queue_order_revision {
                self.queue_order_revision = self.queue_projection.order_revision();
            }
            let entries = self.queue_projection.entries();
            self.track_queue_eligibility(&entries, true, false);
            self.queue_entries = entries;
            self.observe_queued();
        }
        changed
    }

    fn synchronize_queue_history_scope(&mut self) {
        if self.view.epoch.is_some() {
            self.queue_last_epoch = self.view.epoch.clone();
        }
        let Some(scope) = self.queue_scope() else {
            return;
        };
        if self.queue_history_scope.as_ref() != Some(&scope) {
            self.queue_history_scope = Some(scope);
            self.queue_watermark = None;
            self.queue_follow_inputs.clear();
            self.queue_observed.clear();
        }
    }
    fn track_queue_eligibility(
        &mut self,
        entries: &[asura_control::pb::ConversationQueueEntry],
        full: bool,
        acknowledged: bool,
    ) {
        let baseline = self.queue_watermark;
        for entry in entries {
            let Some(id) = entry.input_id.as_ref() else {
                continue;
            };
            if entry.state == Some(6) {
                self.queue_follow_inputs.retain(|known| known != id);
                continue;
            }
            let new_sequence = full
                && baseline
                    .is_some_and(|prior| entry.sequence.is_some_and(|sequence| sequence > prior));
            if (matches!(entry.state, Some(1..=3)) || acknowledged || new_sequence)
                && !self.queue_follow_inputs.contains(id)
            {
                if self.queue_follow_inputs.len() == 16 {
                    self.queue_follow_inputs.remove(0);
                }
                self.queue_follow_inputs.push(id.clone());
            }
        }
        if full || acknowledged {
            self.queue_watermark = Some(
                entries
                    .iter()
                    .filter_map(|entry| entry.sequence)
                    .max()
                    .unwrap_or(0)
                    .max(baseline.unwrap_or(0)),
            );
        }
    }

    fn send_queue(&mut self, request: super::queue::Request) {
        if self.queue_busy {
            self.notice = "Queue request pending; draft retained".into();
            return;
        }
        if self.view.connection != "connected" {
            self.notice = "Disconnected; draft retained".into();
            return;
        }
        if !matches!(request, super::queue::Request::List { .. }) {
            self.queue_retained = Some(request.clone());
        }
        self.queue_editor = if matches!(request, super::queue::Request::Decision(_))
            && matches!(self.focus, Focus::Queue(_))
        {
            None
        } else {
            Some(self.editor.text())
        };
        self.queue_request = Some(request);
        self.queue_busy = true;
        self.notice = "Queue request pending; waiting for durable acknowledgement".into();
    }
    fn queue_target(&mut self, kind: u32) -> Option<asura_control::pb::ConversationEnqueue> {
        let prompt = self.editor.text();
        if self.queue_busy || self.queue_retained.is_some() {
            self.notice = "Resolve pending queue request first; draft retained".into();
            return None;
        }
        if !self.conversation_busy
            || !self.conversation_cancellable
            || self.active_operation.is_none()
            || self.conversation_id.is_none()
        {
            self.notice = "No acknowledged active target; draft retained".into();
            return None;
        }
        if self.view.connection != "connected" {
            self.notice = "Disconnected; draft retained".into();
            return None;
        }
        if prompt.trim().is_empty() || prompt.starts_with('/') || prompt.len() > 32768 {
            self.notice = "Queue requires a nonempty text input of at most 32 KiB".into();
            return None;
        }
        let project = self.selected_project?;
        Some(asura_control::pb::ConversationEnqueue {
            request_id: Some(asura_platform::random_id().to_vec()),
            project_id: Some(project.to_vec()),
            conversation_id: self.conversation_id.clone(),
            target_operation_id: self.active_operation.clone(),
            target_generation: Some(self.conversation_generation),
            kind: Some(kind),
            prompt: Some(prompt),
        })
    }
    fn queue_input(&mut self, kind: u32) {
        if let Some(request) = self.queue_target(kind) {
            self.send_queue(super::queue::Request::Enqueue(request));
        }
    }
    pub(super) fn queue_finished(&mut self, update: super::queue::Update) {
        if matches!(
            &update.request,
            super::queue::Request::Submit(_) | super::queue::Request::Reorder(_)
        ) {
            self.managed_queue_finished(update);
            return;
        }
        if self.queue_request_scope != self.queue_identity_scope() {
            if update.done {
                self.queue_busy = false;
                self.queue_retained = None;
                self.queue_editor = None;
                self.queue_show = false;
                self.queue_request_scope = None;
            }
            return;
        }
        self.synchronize_queue_history_scope();
        let explicit = self.queue_editor.is_some() || self.queue_retained.is_some();
        if update.done {
            self.queue_busy = false;
        }
        match update.outcome {
            Err(error) => {
                if explicit {
                    self.notice = literal(&error, 1024);
                }
                if update.done {
                    if !error.contains("unconfirmed") {
                        self.queue_retained = None;
                    }
                    self.queue_editor = None;
                    self.queue_show = false;
                }
            }
            Ok(reply) => {
                if !update.done {
                    return;
                }
                self.queue_projection.advance_revision(reply.revision);
                if reply.request_id.is_some() {
                    self.queue_retained = None;
                    self.notice = "Input accepted by service".into();
                }
                if self.queue_editor.take().as_deref() == Some(self.editor.text().as_str()) {
                    self.editor = Editor::new();
                }
                if self.queue_show {
                    self.result_title = "Input queue";
                    self.config_result = Some(if reply.entries.is_empty() {
                        "No queued inputs".into()
                    } else {
                        reply
                            .entries
                            .iter()
                            .map(|entry| {
                                format!(
                                    "{} · {}\n{}",
                                    crate::conversation::hex(
                                        entry.input_id.as_deref().unwrap_or_default()
                                    ),
                                    queue_state(entry.state),
                                    literal_multiline(
                                        entry.text.as_deref().unwrap_or_default(),
                                        32768
                                    )
                                )
                            })
                            .collect::<Vec<_>>()
                            .join("\n\n")
                    });
                    self.queue_show = false;
                }
                let project = self.selected_project.map(|p| p.to_vec());
                let entries: Vec<_> = reply
                    .entries
                    .into_iter()
                    .filter(|e| e.project_id == project)
                    .collect();
                self.track_queue_eligibility(
                    &entries,
                    matches!(
                        update.request,
                        super::queue::Request::List { input: None, .. }
                    ),
                    reply.request_id.is_some(),
                );
                if matches!(
                    update.request,
                    super::queue::Request::List { input: None, .. }
                ) {
                    self.queue_entries = entries;
                } else {
                    for entry in entries {
                        if let Some(old) = self
                            .queue_entries
                            .iter_mut()
                            .find(|old| old.input_id == entry.input_id)
                        {
                            *old = entry;
                        } else if self.queue_entries.len() < 16 {
                            self.queue_entries.push(entry);
                        }
                    }
                }
                self.queue_entries.sort_by_key(|e| e.sequence);
                self.observe_queued();
            }
        }
    }
    fn managed_queue_finished(&mut self, update: super::queue::Update) {
        let request_id = match &update.request {
            super::queue::Request::Submit(request) => request.request_id.as_deref(),
            super::queue::Request::Reorder(request) => request.request_id.as_deref(),
            _ => None,
        };
        let local = request_id.and_then(|id| {
            self.local_outbox
                .iter()
                .position(|item| item.request_id.as_slice() == id)
        });
        if self.queue_request_scope != self.queue_identity_scope() {
            if update.done {
                self.queue_busy = false;
                if let Some(index) = local {
                    self.local_outbox[index].state = LocalInputState::Unconfirmed;
                }
                self.notice = "Queue scope changed; exact request outcome unconfirmed".into();
            }
            return;
        }
        if update.done {
            self.queue_busy = false;
        }
        let reply = match update.outcome {
            Err(error) => {
                if let Some(index) = local {
                    self.local_outbox[index].state =
                        if !update.done || error.contains("unconfirmed") {
                            LocalInputState::Unconfirmed
                        } else {
                            LocalInputState::Rejected
                        };
                }
                if update.done && !error.contains("unconfirmed") {
                    self.queue_retained = None;
                }
                self.notice = literal(&error, 1024);
                return;
            }
            Ok(reply) if update.done => reply,
            Ok(_) => return,
        };
        self.queue_retained = None;
        self.queue_order_revision = reply.order_revision;
        self.queue_projection.advance_revision(reply.revision);
        if reply.stale_order == Some(true) {
            self.notice = "Queue order changed; select a position again".into();
        } else if let Some(index) = local {
            let accepted = reply.accepted_input_id.as_ref().and_then(|id| {
                reply
                    .entries
                    .iter()
                    .find(|entry| entry.input_id.as_ref() == Some(id))
            });
            if let Some(entry) = accepted {
                let generation = entry.generation.or(entry.target_generation).unwrap_or(0);
                if self.conversation_id == entry.conversation_id {
                    self.conversation_generation = self.conversation_generation.max(generation);
                } else {
                    self.conversation_generation = generation;
                }
                self.conversation_id = entry.conversation_id.clone();
                self.new_conversation_intent = None;
                self.restoration_ready = true;
                self.local_outbox.remove(index);
                self.notice = "Input accepted by service".into();
            } else {
                self.local_outbox[index].state = LocalInputState::Unconfirmed;
                self.queue_retained = Some(update.request.clone());
                self.notice = "Queue receipt lacked input identity; outcome unconfirmed".into();
                return;
            }
        } else {
            self.notice = "Queue order updated".into();
        }
        self.queue_entries = reply
            .entries
            .into_iter()
            .filter(|entry| entry.project_id == self.selected_project.map(|id| id.to_vec()))
            .collect();
        let entries = self.queue_entries.clone();
        self.track_queue_eligibility(&entries, true, true);
        self.observe_queued();
    }
    fn observe_queued(&mut self) {
        if self.conversation_busy || self.retained_request.is_some() || !self.restoration_ready {
            return;
        }
        let entry = self
            .queue_entries
            .iter()
            .find(|entry| {
                matches!(entry.state, Some(3..=5))
                    && entry
                        .input_id
                        .as_ref()
                        .is_some_and(|id| self.queue_follow_inputs.contains(id))
                    && (entry.conversation_id != self.conversation_id
                        || entry
                            .generation
                            .is_some_and(|generation| generation >= self.conversation_generation))
                    && entry
                        .operation_id
                        .as_ref()
                        .is_some_and(|id| !self.queue_observed.contains(id))
            })
            .cloned();
        let Some(entry) = entry else {
            return;
        };
        let Some(operation) = entry
            .operation_id
            .clone()
            .and_then(|v| <[u8; 16]>::try_from(v).ok())
        else {
            return;
        };
        self.queue_follow_inputs
            .retain(|id| entry.input_id.as_ref() != Some(id));
        self.queue_observed.push(operation.to_vec());
        if self.queue_observed.len() > 32 {
            self.queue_observed.remove(0);
        }
        self.conversation_id = entry.conversation_id;
        self.conversation_generation = entry.generation.unwrap_or(0);
        self.active_operation = Some(operation.to_vec());
        self.append_response(
            entry.text.unwrap_or_else(|| "Queued input".into()),
            Some(operation.to_vec()),
        );
        self.conversation_request = Some(super::conversation::Request::Setup(
            crate::conversation::Command::Observe(operation),
        ));
        self.conversation_busy = true;
        self.conversation_cancellable = true;
        self.notice = "Observing service-dispatched input".into();
    }
    pub(super) fn take_audit_request(&mut self) -> Option<(super::queue_watch::Scope, u32)> {
        self.audit_request.take()
    }
    pub(super) fn take_audit_cancel(&mut self) -> bool {
        std::mem::take(&mut self.audit_cancel)
    }
    pub(super) fn audit_finished(&mut self, outcome: super::audit::Outcome) {
        let submitted = self.audit_draft.take();
        let scope = self.audit_scope.take();
        if scope != self.queue_identity_scope() {
            return;
        }
        let Some(scope) = scope else {
            return;
        };
        if outcome.is_ok() && submitted.as_deref() == Some(self.editor.text().as_str()) {
            self.editor = Editor::new();
        }
        let text = match outcome {
            Ok(reply) => audit_table(&reply),
            Err(error) => format!("{}\nDraft retained.", literal(&error, 1024)),
        };
        self.audit_result = Some((scope, text));
        self.notice.clear();
    }
    pub(super) fn take_models_request(&mut self) -> bool {
        std::mem::take(&mut self.models_request)
    }
    pub(super) fn take_models_cancel(&mut self) -> bool {
        std::mem::take(&mut self.models_cancel)
    }
    pub(super) fn models_finished(&mut self, outcome: super::models::Outcome) {
        if self.picker_models_pending {
            self.picker_models_pending = false;
            if let Some(Picker::Models { result, .. }) = self.picker.as_mut() {
                *result = Some(outcome);
            }
            return;
        }
        let submitted = self.models_draft.take();
        if outcome.is_ok() && submitted.as_deref() == Some(self.editor.text().as_str()) {
            self.editor = Editor::new();
        }
        self.models_result = Some(outcome);
        self.notice.clear();
    }
    pub(super) fn take_config_request(&mut self) -> Option<super::config::Request> {
        self.config_request.take()
    }
    pub(super) fn config_finished(&mut self, outcome: super::config::Outcome) {
        if let Some(selector) = self.picker_model_save.take() {
            self.config_draft = None;
            match outcome {
                Ok(_) => {
                    self.view.configured_model = Some(selector);
                    self.observed_model = None;
                    self.model_context = None;
                    if self.picker.take().is_some() {
                        self.focus = Focus::Model;
                    }
                    self.notice = "Model selected for future inputs".into();
                }
                Err(error) => self.notice = literal(&error, 1024),
            }
            return;
        }
        let submitted = self.config_draft.take();
        if outcome.is_ok() && submitted.as_deref() == Some(self.editor.text().as_str()) {
            self.editor = Editor::new();
        }
        let message = match outcome {
            Ok(value) => value,
            Err(error) if error.contains("unconfirmed") => format!(
                "Config error: {error}\nOutcome unconfirmed. Use /config get to check before retrying.\nDraft retained."
            ),
            Err(error) => format!("Config error: {error}\nDraft retained."),
        };
        let message = message
            .lines()
            .take(128)
            .map(|line| literal(line, 4096))
            .collect::<Vec<_>>()
            .join("\n");
        self.notice.clear();
        self.result_title = "Configuration";
        self.config_result = Some(message);
    }
    pub(super) fn take_conversation_request(&mut self) -> Option<super::conversation::Request> {
        if !self.project_discovery_started
            && self.view.installation == "graph_ready"
            && self.view.connection == "connected"
            && !self.conversation_busy
            && self.retained_request.is_none()
            && self.config_draft.is_none()
        {
            self.project_discovery_started = true;
            self.conversation_request = Some(super::conversation::Request::DiscoverProject);
            self.conversation_busy = true;
        }
        if self.conversation_request.is_none()
            && !self.conversation_busy
            && self.retained_request.is_none()
            && self.view.connection == "connected"
            && self.view.installation == "graph_ready"
        {
            if let Some(operation) = self.pending_restored_operation.take() {
                let request = super::conversation::Request::Setup(
                    crate::conversation::Command::Observe(operation),
                );
                self.retained_request = Some(request.clone());
                self.conversation_request = Some(request);
                self.conversation_busy = true;
                self.conversation_cancellable = true;
                self.notice = "Resuming active response".into();
            } else if let (Some(project), Some(epoch)) = (
                self.selected_project,
                self.view
                    .epoch
                    .as_deref()
                    .and_then(|value| crate::conversation::parse_id(value).ok()),
            ) && self.new_conversation_intent != Some(project)
                && self.restoration_scope != Some((project, epoch))
            {
                self.restoration_scope = Some((project, epoch));
                self.restoration_ready = false;
                self.conversation_request =
                    Some(super::conversation::Request::Restore { project, epoch });
                self.conversation_busy = true;
                self.notice = "Restoring conversation".into();
            }
        }
        self.conversation_request.take()
    }
    pub(super) fn take_cancel_request(&mut self) -> bool {
        std::mem::take(&mut self.cancel_request)
    }
    pub(super) fn conversation_finished(&mut self, update: super::conversation::Update) {
        if let Some(directory) = update.launch_directory {
            self.launch_directory = Some(directory);
        }
        if let Some(path) = update.project_offer
            && !self.project_offer_seen
        {
            self.project_offer_seen = true;
            self.project_offer = Some(path);
        }
        if let Some(mut projects) = update.projects {
            if let Some(previous) = &self.projects {
                for project in &mut projects {
                    if let Some(known) = previous
                        .iter()
                        .find(|known| known.project_id == project.project_id)
                        && known.name_revision.unwrap_or(0) > project.name_revision.unwrap_or(0)
                    {
                        project.name = known.name.clone();
                        project.name_revision = known.name_revision;
                    }
                }
            }
            let still_current = self.selected_project.is_some_and(|id| {
                projects.iter().any(|p| {
                    p.project_id.as_deref() == Some(id.as_slice()) && p.current == Some(true)
                })
            });
            if !(update.preserve_project_selection && still_current)
                && self.selected_project != update.project
            {
                self.selected_project = update.project;
                self.conversation_id = None;
                self.conversation_generation = 0;
                self.new_conversation_intent = None;
                self.restoration_scope = None;
                self.restoration_ready = false;
                self.pending_restored_operation = None;
                self.transcript.clear();
                self.rendered_responses.clear();
                self.activity.clear();
                self.model_context = None;
            }
            if !projects.is_empty() {
                self.project_offer = None;
            }
            self.projects = Some(projects);
        } else if let Some(project) = update.project
            && self.selected_project != Some(project)
        {
            self.selected_project = Some(project);
            self.conversation_id = None;
            self.conversation_generation = 0;
            self.new_conversation_intent = None;
            self.restoration_scope = None;
            self.restoration_ready = false;
            self.pending_restored_operation = None;
            self.transcript.clear();
            self.rendered_responses.clear();
            self.activity.clear();
            self.model_context = None;
        }
        if let Some(restore) = update.restore
            && self.selected_project == Some(restore.project)
            && self
                .view
                .epoch
                .as_deref()
                .and_then(|value| crate::conversation::parse_id(value).ok())
                == Some(restore.epoch)
            && self.new_conversation_intent != Some(restore.project)
        {
            match restore.result {
                Ok(restored) => self.apply_restoration(restored),
                Err(error) => {
                    self.restoration_ready = false;
                    self.notice = format!(
                        "Conversation restoration unavailable: {}; use /retry or /new",
                        literal(&error, 512)
                    );
                }
            }
        }
        if let Some(accepted) = update.accepted {
            self.active_operation = accepted.operation_id.clone();
            self.observed_model = None;
            self.model_context = None;
            self.retained_request = None;
            self.conversation_id = accepted.conversation_id;
            self.conversation_generation = accepted.generation.unwrap_or(0);
            self.new_conversation_intent = None;
            self.restoration_ready = true;
            if let Some(draft) = self.conversation_draft.take() {
                if self.conversation_editor_draft.take().as_deref()
                    == Some(self.editor.text().as_str())
                {
                    self.editor = Editor::new();
                }
                self.append_response(draft, self.active_operation.clone());
            }
            self.notice = "Generating · /cancel requests cancellation".into();
        }
        if let Some(event) = update.event
            && let Some(response_index) = self.observe_activity(&event)
        {
            if self.active_operation != event.operation_id {
                self.model_context = None;
            }
            if let Some(context) = &event.model_context {
                if let Some(name) = &context.model_name {
                    self.observed_model = Some(literal(name, 256));
                }
                if context.basis == Some(1) {
                    self.model_context = Some(context.clone());
                }
            }
            self.active_operation = event.operation_id.clone();
            self.conversation_generation = event.generation.unwrap_or(self.conversation_generation);
            if event.kind != Some(1)
                && let Some((_, response)) = self.transcript.get_mut(response_index)
            {
                *response = literal_multiline(event.text.as_deref().unwrap_or_default(), 61440);
                if matches!(event.kind, Some(4..=6)) {
                    response.push_str(&format!(
                        "\n[Incomplete: {}]",
                        match event.kind {
                            Some(5) => "cancelled",
                            Some(6) => "interrupted",
                            _ => "failed",
                        }
                    ));
                }
                if let Some(rendered) = self.rendered_responses.get_mut(response_index) {
                    *rendered = markdown::render(response);
                }
            }
            if update.done {
                self.notice = if event.kind == Some(3) {
                    "Complete".into()
                } else {
                    format!(
                        "Incomplete response · {}",
                        conversation_reason(event.reason.unwrap_or(10))
                    )
                };
            }
        }
        if let Some(message) = update.message {
            if !update.success {
                self.activity_unavailable();
            }
            if update.success {
                self.retained_request = None;
                self.conversation_draft.take();
                if self.conversation_editor_draft.take().as_deref()
                    == Some(self.editor.text().as_str())
                {
                    self.editor = Editor::new();
                }
                self.notice.clear();
                self.result_title = "Setup";
                self.config_result = Some(literal_multiline(&message, 40960));
            } else {
                self.notice = literal(&message, 1024);
            }
        }
        if update.done {
            self.conversation_busy = false;
            self.conversation_cancellable = false;
            self.conversation_draft = None;
            self.conversation_editor_draft = None;
            self.observe_queued();
        }
    }
    fn apply_restoration(&mut self, restored: super::conversation::Restored) {
        self.conversation_id = restored.conversation_id;
        self.conversation_generation = restored.generation;
        self.transcript.clear();
        self.rendered_responses.clear();
        self.activity.clear();
        self.response_selection = None;
        self.response_scroll = 0;
        self.response_scroll_max = 0;
        self.pending_restored_operation = None;
        self.active_operation = None;
        self.model_context = None;
        let count = restored.turns.len();
        for turn in restored.turns {
            let operation = turn.event.operation_id.clone();
            self.active_operation = operation.clone();
            self.append_response(turn.prompt, operation.clone());
            if let Some(index) = self.observe_activity(&turn.event) {
                if let Some(context) = &turn.event.model_context {
                    if let Some(name) = &context.model_name {
                        self.observed_model = Some(literal(name, 256));
                    }
                    if context.basis == Some(1) {
                        self.model_context = Some(context.clone());
                    }
                }
                if turn.event.kind != Some(1)
                    && let Some((_, response)) = self.transcript.get_mut(index)
                {
                    *response =
                        literal_multiline(turn.event.text.as_deref().unwrap_or_default(), 61440);
                    if matches!(turn.event.kind, Some(4..=6)) {
                        response.push_str(&format!(
                            "\n[Incomplete: {}]",
                            match turn.event.kind {
                                Some(5) => "cancelled",
                                Some(6) => "interrupted",
                                _ => "failed",
                            }
                        ));
                    }
                    if let Some(rendered) = self.rendered_responses.get_mut(index) {
                        *rendered = markdown::render(response);
                    }
                }
            }
            if matches!(turn.event.kind, Some(1 | 2)) {
                self.pending_restored_operation = operation
                    .as_deref()
                    .and_then(|id| <[u8; 16]>::try_from(id).ok());
            }
        }
        self.active_operation = self.pending_restored_operation.map(|id| id.to_vec());
        self.restoration_ready = true;
        self.notice = if count == 0 {
            "No previous conversation in this project".into()
        } else {
            format!(
                "Restored {count} conversation turn{}",
                if count == 1 { "" } else { "s" }
            )
        };
    }
    fn start_conversation_request(&mut self, request: super::conversation::Request, draft: String) {
        self.conversation_cancellable = matches!(
            &request,
            super::conversation::Request::Setup(crate::conversation::Command::Observe(_))
        );
        self.retained_preserve_draft = false;
        self.retained_request = Some(request.clone());
        self.conversation_request = Some(request);
        self.conversation_draft = Some(draft);
        self.conversation_editor_draft = Some(self.editor.text());
        self.conversation_busy = true;
        self.notice = "Request pending".into();
    }
    pub fn set_view(&mut self, mut view: View) {
        view.epoch = view.epoch.map(|text| literal(&text, 128));
        if view.configured_model != self.view.configured_model {
            self.observed_model = None;
            self.model_context = None;
        }
        if view.epoch.is_some() && view.epoch != self.view.epoch {
            self.project_discovery_started = false;
            self.restoration_scope = None;
            self.restoration_ready = false;
            self.pending_restored_operation = None;
            self.close_picker();
            self.focus = Focus::Editor;
            self.observed_model = None;
            self.model_context = None;
        }
        if view.connection != "connected" {
            self.model_context = None;
        }
        if view.epoch.is_some() {
            self.queue_last_epoch = view.epoch.clone();
        }
        self.view = view;
    }

    pub(super) fn context_scope(&self) -> Option<super::context::Scope> {
        if self.view.connection != "connected" || self.view.installation != "graph_ready" {
            return None;
        }
        let project = self.selected_project?;
        let registered = self.projects.as_ref()?.iter().find(|p| {
            p.project_id.as_deref() == Some(project.as_slice()) && p.current == Some(true)
        })?;
        registered.location.as_ref()?;
        Some(super::context::Scope {
            project,
            path: self.context_directory()?.into(),
            epoch: self.view.epoch.clone()?,
        })
    }
    pub(super) fn next_deadline(&self) -> Option<std::time::Instant> {
        self.queue_projection
            .next_deadline()
            .into_iter()
            .chain(self.context.next_deadline())
            .min()
    }
    pub(super) fn refresh_context(&mut self, now: std::time::Instant) -> bool {
        let scope = self.context_scope();
        let changed = self.context.scope(scope);
        self.context.expire(now) || changed
    }
    pub(super) fn context_unavailable(&mut self) -> bool {
        self.context.unavailable()
    }
    pub(super) fn context_finished(&mut self, update: super::context::Update) -> bool {
        self.context.scope(self.context_scope());
        self.context.apply(update)
    }

    fn context_directory(&self) -> Option<&str> {
        let root = self.selected_project.and_then(|id| {
            self.projects
                .as_ref()?
                .iter()
                .find(|project| project.project_id.as_deref() == Some(id.as_slice()))?
                .location
                .as_deref()
        });
        match (self.launch_directory.as_deref(), root) {
            (Some(launch), Some(root)) if std::path::Path::new(launch).starts_with(root) => {
                Some(launch)
            }
            (_, Some(root)) => Some(root),
            (launch, None) => launch,
        }
    }

    fn project_name(&self, id: [u8; 16]) -> String {
        self.projects
            .as_ref()
            .and_then(|projects| {
                projects
                    .iter()
                    .find(|project| project.project_id.as_deref() == Some(id.as_slice()))
            })
            .map(project_label)
            .unwrap_or_else(|| crate::conversation::hex(&id[..4]))
    }

    pub(super) fn take_project_admin_request(&mut self) -> Option<super::project_admin::Request> {
        self.project_admin_request.take()
    }

    pub(super) fn project_admin_finished(&mut self, update: super::project_admin::Update) {
        let request = &update.request;
        let target = request.command.project_id.as_deref();
        self.project_admin_busy = !update.settled;
        let reply = match update.outcome {
            super::project_admin::Outcome::Definite(error) => {
                self.project_admin_retained = None;
                self.notice = format!("Project rename rejected: {error}; draft retained");
                return;
            }
            super::project_admin::Outcome::Unconfirmed(error) => {
                self.project_admin_retained = Some(request.clone());
                self.notice = format!("Project rename outcome unconfirmed: {error}; use /retry");
                return;
            }
            super::project_admin::Outcome::Reply(reply) => reply,
        };
        let same_epoch = self.view.epoch.as_deref() == Some(request.epoch.as_str())
            && update
                .service_epoch
                .is_some_and(|epoch| crate::conversation::hex(&epoch) == request.epoch);
        if !same_epoch {
            self.project_admin_retained = Some(request.clone());
            self.notice =
                "Project rename outcome unconfirmed after service change; use /retry".into();
            return;
        }
        let project = reply.current_project.or(reply.project);
        let Some(project) = project else {
            if let Some(error) = reply.error {
                self.project_admin_retained = None;
                self.notice = format!("Project rename rejected: {error}; draft retained");
            } else {
                self.project_admin_retained = Some(request.clone());
                self.notice = "Project rename reply incomplete; use /retry".into();
            }
            return;
        };
        if project.project_id.as_deref() != target {
            self.project_admin_retained = Some(request.clone());
            self.notice = "Project rename reply changed target; use /retry".into();
            return;
        }
        let mut newer_projection = false;
        if let Some(rows) = &mut self.projects
            && let Some(row) = rows
                .iter_mut()
                .find(|row| row.project_id.as_deref() == target)
        {
            if project.name_revision.unwrap_or(0) >= row.name_revision.unwrap_or(0) {
                *row = project.clone();
            } else {
                newer_projection = true;
            }
        }
        if let Some(error) = reply.error {
            self.project_admin_retained = None;
            self.notice = if error == "stale_project_name_revision" {
                "Project name changed elsewhere; current name shown. Draft retained".into()
            } else {
                format!("Project rename rejected: {error}; draft retained")
            };
            return;
        }
        self.project_admin_retained = None;
        if self.editor_generation == request.editor_generation
            && self.editor.text() == request.draft
        {
            self.editor = Editor::new();
        }
        self.notice = if newer_projection {
            "Project rename resolved; a newer name is already shown".into()
        } else {
            format!("Project renamed to {}", project_label(&project))
        };
    }

    fn status_header(&self) -> String {
        let connection = match self.view.connection {
            "connected" => format!(
                "Connected ({})",
                if self.view.service.as_ref().is_some_and(|s| s.owned) {
                    "process"
                } else {
                    "local"
                }
            ),
            "connecting" => "Connecting".into(),
            _ => "Disconnected".into(),
        };
        let (uptime, memory) = self.view.service.as_ref().map_or_else(
            || ("?".into(), "unknown ?".into()),
            |service| {
                let seconds = service.uptime_ms / 1000;
                let uptime = if seconds >= 3600 {
                    format!("{}h {}m", seconds / 3600, seconds / 60 % 60)
                } else if seconds >= 60 {
                    format!("{}m {}s", seconds / 60, seconds % 60)
                } else {
                    format!("{seconds}s")
                };
                let size = service.stored_memory.size_bytes.map_or_else(
                    || "?".into(),
                    |bytes| {
                        if bytes >= 1_048_576 {
                            format!("{:.1} MiB", bytes as f64 / 1_048_576.0)
                        } else if bytes >= 1024 {
                            format!("{:.1} KiB", bytes as f64 / 1024.0)
                        } else {
                            format!("{bytes} B")
                        }
                    },
                );
                let state = if service.stored_memory.available == Some(true) {
                    "ready"
                } else {
                    "unavailable"
                };
                let stale = if service.stored_memory.stale == Some(true)
                    && service.stored_memory.size_bytes.is_some()
                {
                    " stale"
                } else {
                    ""
                };
                (uptime, format!("{state} {size}{stale}"))
            },
        );
        let graph =
            if self.view.installation == "graph_ready" && self.view.reason == "graph_verified" {
                "ready verified".into()
            } else {
                format!("{} {}", self.view.installation, self.view.reason)
            };
        format!("Asura · {connection} · uptime {uptime} · memory: {memory} · graph: {graph}")
    }

    #[cfg(test)]
    pub fn draft(&self) -> String {
        self.editor.text()
    }
    fn insert(&mut self, text: &str) {
        self.editor_generation = self.editor_generation.wrapping_add(1);
        self.history.reset_navigation();
        if let Err(error) = self.editor.insert(text) {
            self.notice = error.to_string();
        }
    }
    fn capture(&mut self, text: &str) {
        if let Some(capture) = &mut self.capture {
            if capture.text.len().saturating_add(text.len()) > 3 * MAX_DRAFT_BYTES {
                capture.invalid = true;
            } else if !capture.invalid {
                capture.text.push_str(text);
            }
        }
    }
    pub fn reject_paste(&mut self) {
        if self.overlay.is_some() {
            return;
        }
        if let Some(capture) = &mut self.capture {
            capture.invalid = true;
        } else {
            self.notice = super::editor::EditError::TooLarge.to_string();
        }
    }
    pub fn handle(&mut self, event: Event) -> bool {
        if let Event::Paste(text) = event {
            if self.overlay.is_none()
                && self.focus == Focus::Editor
                && self.picker.is_none()
                && self.response_selection.is_none()
            {
                if self.capture.is_some() {
                    self.capture(&text);
                } else {
                    self.insert(&text);
                }
            }
            return false;
        }
        let Event::Key(key) = event else {
            return false;
        };
        if key.kind == KeyEventKind::Release {
            return false;
        }
        // Actions require a distinct press. Editing repeats remain available.
        if key.kind != KeyEventKind::Press
            && (self.overlay.is_some()
                || self.capture.is_some()
                || matches!(key.code, KeyCode::Enter | KeyCode::Esc | KeyCode::F(_))
                || key.modifiers.contains(KeyModifiers::CONTROL)
                || (key.code == KeyCode::Tab && self.editor.command_header().is_some()))
        {
            return false;
        }
        self.editor_generation = self.editor_generation.wrapping_add(1);
        if matches!(self.overlay, Some(Overlay::Commands(..))) {
            let Some(Overlay::Commands(header, mut selected)) = self.overlay.take() else {
                unreachable!()
            };
            let names: Vec<_> = LOCAL_COMMANDS
                .iter()
                .copied()
                .filter(|name| name.starts_with(&header.name))
                .collect();
            match key.code {
                KeyCode::Esc => return false,
                KeyCode::Down | KeyCode::Right => {
                    selected = Some(selected.map_or(0, |i| (i + 1) % names.len()))
                }
                KeyCode::Up | KeyCode::Left => {
                    selected = Some(
                        selected.map_or(names.len() - 1, |i| (i + names.len() - 1) % names.len()),
                    )
                }
                KeyCode::Tab | KeyCode::Enter if key.modifiers.is_empty() => {
                    if let Some(index) = selected {
                        self.complete(&header, names[index]);
                        return false;
                    } else if key.code == KeyCode::Tab {
                        selected = Some(0);
                    }
                }
                _ => {}
            }
            self.overlay = Some(Overlay::Commands(header, selected));
            return false;
        }
        if let Some(overlay) = &mut self.overlay {
            match key.code {
                KeyCode::Esc => {
                    if matches!(overlay, Overlay::Paste(_)) {
                        self.capture = None;
                    }
                    self.overlay = None;
                }
                KeyCode::F(1) if matches!(overlay, Overlay::Help(_)) => self.overlay = None,
                KeyCode::Tab | KeyCode::Left | KeyCode::Right | KeyCode::Up | KeyCode::Down => {
                    match overlay {
                        Overlay::Config(_, offset)
                        | Overlay::Help(offset)
                        | Overlay::Models(_, offset)
                        | Overlay::Tools(offset)
                        | Overlay::Audit(_, _, offset) => {
                            if matches!(key.code, KeyCode::Up | KeyCode::Left) {
                                *offset = offset.saturating_sub(1);
                            } else if matches!(key.code, KeyCode::Down | KeyCode::Right) {
                                *offset = offset.saturating_add(1).min(self.panel_scroll_max);
                            }
                        }
                        Overlay::ProjectOffer(_, yes) => *yes = !*yes,
                        Overlay::Exit(discard) => *discard = !*discard,
                        Overlay::Paste(selection) => *selection = Some(!selection.unwrap_or(false)),
                        Overlay::Commands(..) => {}
                    }
                }
                KeyCode::Enter if key.modifiers.is_empty() => match overlay {
                    Overlay::ProjectOffer(path, yes) => {
                        let path = path.clone();
                        let yes = *yes;
                        self.overlay = None;
                        if yes {
                            let request = super::conversation::Request::Setup(
                                crate::conversation::Command::Register(
                                    asura_platform::random_id(),
                                    path,
                                ),
                            );
                            self.retained_preserve_draft = true;
                            self.retained_request = Some(request.clone());
                            self.conversation_request = Some(request);
                            self.conversation_busy = true;
                            self.conversation_draft = None;
                            self.conversation_editor_draft = None;
                            self.notice = "Registering project".into();
                        }
                    }
                    Overlay::Exit(discard) => {
                        let exit = *discard;
                        self.overlay = None;
                        return exit;
                    }
                    Overlay::Paste(Some(insert)) => {
                        let insert = *insert;
                        if !insert {
                            self.capture = None;
                            self.overlay = None;
                        } else if self
                            .capture
                            .as_ref()
                            .is_some_and(|capture| !capture.invalid)
                        {
                            let text = self.capture.as_ref().unwrap().text.clone();
                            match self.editor.insert(&text) {
                                Ok(()) => {
                                    self.capture = None;
                                    self.overlay = None;
                                }
                                Err(error) => self.notice = error.to_string(),
                            }
                        }
                    }
                    _ => {}
                },
                _ => {}
            }
            return false;
        }
        if self.capture.is_some() {
            match key.code {
                KeyCode::Char('v') if key.modifiers == KeyModifiers::CONTROL => {
                    self.overlay = Some(Overlay::Paste(None))
                }
                KeyCode::Enter if key.modifiers.is_empty() => self.capture("\n"),
                KeyCode::Char('j') if key.modifiers == KeyModifiers::CONTROL => self.capture("\n"),
                KeyCode::Tab if key.modifiers.is_empty() => self.capture("\t"),
                KeyCode::Char(c)
                    if key.modifiers.is_empty() || key.modifiers == KeyModifiers::SHIFT =>
                {
                    self.capture(c.encode_utf8(&mut [0; 4]))
                }
                _ => self.capture.as_mut().unwrap().invalid = true,
            }
            return false;
        }
        if key.modifiers == KeyModifiers::CONTROL {
            match key.code {
                KeyCode::Char('q' | 'c') => {
                    if self.editor.is_empty() && self.local_outbox.is_empty() {
                        return true;
                    }
                    self.overlay = Some(Overlay::Exit(false));
                    return false;
                }
                KeyCode::Char('s' | 't') => {
                    self.queue_input(if key.code == KeyCode::Char('s') { 2 } else { 1 });
                    return false;
                }
                KeyCode::Char('d') => return self.editor.is_empty(),
                KeyCode::Char('j') => return false,
                KeyCode::Char('v') => {
                    self.notice.clear();
                    self.capture = Some(Capture {
                        text: String::new(),
                        invalid: false,
                    });
                    return false;
                }
                _ => {}
            }
        }
        if self.activity_key(key) {
            return false;
        }
        if self.history_key(key) {
            return false;
        }
        if self.composer_key(key) {
            return false;
        }
        match key.code {
            KeyCode::Tab if key.modifiers.is_empty() && self.editor.command_header().is_some() => {
                let header = self.editor.command_header().unwrap();
                let names: Vec<_> = LOCAL_COMMANDS
                    .iter()
                    .copied()
                    .filter(|name| name.starts_with(&header.name))
                    .collect();
                match names.as_slice() {
                    [] => self.notice = "No matching local command".into(),
                    [name] => self.complete(&header, name),
                    _ => {
                        self.notice.clear();
                        self.overlay = Some(Overlay::Commands(header, None));
                    }
                }
            }
            KeyCode::F(1) => self.overlay = Some(Overlay::Help(0)),
            KeyCode::Enter if key.modifiers == KeyModifiers::ALT => self.insert("\n"),
            KeyCode::Enter if key.modifiers.is_empty() => return self.submit(),
            KeyCode::Esc => {}
            _ => {
                if matches!(
                    key.code,
                    KeyCode::Char(_) | KeyCode::Backspace | KeyCode::Delete
                ) {
                    self.history.reset_navigation();
                }
                if let Err(error) = self.editor.key(key) {
                    self.notice = error.to_string();
                }
            }
        }
        false
    }
    fn complete(&mut self, header: &CommandHeader, name: &str) {
        match self.editor.complete_command_name(header, name) {
            Ok(true) => self.notice.clear(),
            Ok(false) => self.notice = "Completion target changed; draft retained".into(),
            Err(error) => self.notice = error.to_string(),
        }
    }

    fn submit(&mut self) -> bool {
        let draft = self.editor.text();
        self.history.record(&draft);
        if !draft.starts_with('/') {
            if draft.trim().is_empty() {
                return false;
            }
            if draft.len() > 32768 {
                self.notice = "Conversation input exceeds 32 KiB; draft retained".into();
                return false;
            }
            let Some(project) = self.selected_project else {
                self.notice = "No project selected. Use /project add PATH".into();
                return false;
            };
            if !self.restoration_ready && self.new_conversation_intent != Some(project) {
                self.notice =
                    "Restore the conversation first; use /retry or /new. Draft retained".into();
                return false;
            }
            if self.local_outbox.len() >= 16 {
                self.notice = "Local input queue is full; draft retained".into();
                return false;
            }
            self.local_outbox.push(LocalInput {
                request_id: asura_platform::random_id(),
                project,
                text: draft,
                state: LocalInputState::Waiting,
            });
            self.editor = Editor::new();
            self.notice = "Pending local input; waiting for service receipt".into();
            return false;
        }
        let mut command = draft.splitn(3, char::is_whitespace);
        let name = command.next().unwrap_or_default();
        if name == "/new" {
            if draft.trim() != "/new" {
                self.notice = "Use /new without arguments; draft retained".into();
            } else if let Some(project) = self.selected_project {
                if self.conversation_busy
                    || self.retained_request.is_some()
                    || !self.local_outbox.is_empty()
                    || !self.unresolved().is_empty()
                {
                    self.notice = "Resolve the current request before /new; draft retained".into();
                } else {
                    self.conversation_id = None;
                    self.conversation_generation = 0;
                    self.new_conversation_intent = Some(project);
                    self.pending_restored_operation = None;
                    self.active_operation = None;
                    self.model_context = None;
                    self.transcript.clear();
                    self.rendered_responses.clear();
                    self.activity.clear();
                    self.response_selection = None;
                    self.response_scroll = 0;
                    self.response_scroll_max = 0;
                    self.editor = Editor::new();
                    self.notice = "New conversation ready".into();
                }
            } else {
                self.notice = "Select a project before /new; draft retained".into();
            }
            return false;
        }
        if name == "/queue" {
            let args: Vec<_> = draft.split_whitespace().collect();
            if self.queue_busy {
                self.notice = "Queue request still pending; draft retained".into();
                return false;
            }
            let Some(project) = self.selected_project else {
                self.notice = "Select a project first".into();
                return false;
            };
            let request = match args.as_slice() {
                ["/queue"] => Ok(super::queue::Request::List {
                    project,
                    input: None,
                }),
                ["/queue", id] => {
                    crate::conversation::parse_id(id).map(|input| super::queue::Request::List {
                        project,
                        input: Some(input),
                    })
                }
                ["/queue", action @ ("resume" | "drop"), id] => crate::conversation::parse_id(id)
                    .map(|input| {
                        super::queue::Request::Decision(
                            asura_control::pb::ConversationQueueDecision {
                                request_id: Some(asura_platform::random_id().to_vec()),
                                input_id: Some(input.to_vec()),
                                action: Some(if *action == "resume" { 1 } else { 2 }),
                                ..Default::default()
                            },
                        )
                    }),
                _ => Err("Use /queue [ID | resume ID | drop ID]".into()),
            };
            match request {
                Ok(request) => {
                    self.queue_show = true;
                    self.send_queue(request);
                }
                Err(error) => self.notice = error,
            }
            return false;
        }
        if name == "/retry" && draft.trim() == "/retry" && self.queue_retained.is_some() {
            if self.queue_busy {
                self.notice = "Queue request still settling".into();
            } else {
                let request = self.queue_retained.clone().unwrap();
                self.send_queue(request);
            }
            return false;
        }
        if name == "/retry" && draft.trim() == "/retry" && self.project_admin_retained.is_some() {
            if self.project_admin_busy {
                self.notice = "Project rename still settling".into();
            } else {
                let request = self.project_admin_retained.clone().unwrap();
                self.project_admin_request = Some(request);
                self.project_admin_busy = true;
                self.notice = "Resolving project rename with original request ID".into();
            }
            return false;
        }
        if name == "/retry" {
            if draft.trim() != "/retry" {
                self.notice = "Use /retry".into();
            } else if self.conversation_busy {
                self.notice = "Request still pending".into();
            } else if !self.restoration_ready
                && self.restoration_scope.is_some()
                && self.selected_project.is_some()
                && self.new_conversation_intent != self.selected_project
                && self.retained_request.is_none()
            {
                self.restoration_scope = None;
                self.editor = Editor::new();
                self.notice = "Retrying conversation restoration".into();
            } else if let Some(request) = self.retained_request.clone() {
                let preserve = self.retained_preserve_draft;
                self.start_conversation_request(request, draft);
                if preserve {
                    self.retained_preserve_draft = true;
                    self.conversation_draft = None;
                    self.conversation_editor_draft = None;
                }
            } else {
                self.notice = "No unconfirmed request to retry".into();
            }
            return false;
        }
        if name == "/cancel" {
            if draft.trim() != "/cancel" {
                self.notice = "Use /cancel".into();
            } else if self.audit_draft.is_some() {
                self.audit_cancel = true;
                self.editor = Editor::new();
                self.notice = "Audit cancellation requested".into();
            } else if self.models_draft.is_some() || self.picker_models_pending {
                self.models_cancel = true;
                self.editor = Editor::new();
                self.notice = "Model inventory cancellation requested".into();
            } else if !self.conversation_busy || !self.conversation_cancellable {
                self.notice = "No active conversation to cancel".into();
            } else {
                self.cancel_request = true;
                self.editor = Editor::new();
                self.notice = "Cancellation requested; waiting for durable result".into();
            }
            return false;
        }
        if matches!(name, "/init" | "/project" | "/observe") {
            if name == "/project" {
                let action = command.next().unwrap_or_default();
                if action == "rename" {
                    let new_name = command.next().unwrap_or_default();
                    if new_name.is_empty() {
                        self.notice = "Use /project rename <name>; draft retained".into();
                    } else if self.project_admin_busy || self.project_admin_retained.is_some() {
                        self.notice =
                            "Resolve previous project rename first; draft retained".into();
                    } else if let (Some(project), Some(epoch)) =
                        (self.selected_project, self.view.epoch.clone())
                    {
                        let Some(revision) = self
                            .projects
                            .as_ref()
                            .and_then(|rows| {
                                rows.iter().find(|row| {
                                    row.project_id.as_deref() == Some(project.as_slice())
                                })
                            })
                            .map(|row| row.name_revision.unwrap_or(0))
                        else {
                            self.notice = "Project registry unavailable; use /project list first; draft retained".into();
                            return false;
                        };
                        let request = super::project_admin::Request {
                            command: asura_control::pb::ProjectRename {
                                request_id: Some(asura_platform::random_id().to_vec()),
                                project_id: Some(project.to_vec()),
                                expected_name_revision: Some(revision),
                                name: Some(new_name.into()),
                            },
                            draft,
                            epoch,
                            editor_generation: self.editor_generation,
                        };
                        self.project_admin_request = Some(request.clone());
                        self.project_admin_retained = Some(request);
                        self.project_admin_busy = true;
                        self.notice = "Renaming project".into();
                    } else {
                        self.notice =
                            "Select a project with a connected service; draft retained".into();
                    }
                    return false;
                }
                // Restore the parser for the existing project commands.
                command = draft.splitn(3, char::is_whitespace);
                command.next();
            }
            if self.conversation_busy {
                self.notice = "Conversation request busy; draft retained".into();
                return false;
            }
            let action = command.next().unwrap_or_default();
            let arg = command.next().unwrap_or_default().trim();
            use crate::conversation::Command;
            let request=match (name,action,arg) {
                ("/init","","")=>Ok(Command::Initialize(asura_platform::random_id())),
                ("/project","list","")=>Ok(Command::List),
                ("/project","add",path) if !path.is_empty()=>Ok(Command::Register(asura_platform::random_id(),path.into())),
                ("/project","select",id)=>match crate::conversation::parse_id(id){Ok(id)=>{if self.select_project(id) { self.editor=Editor::new(); } return false},Err(e)=>Err(e)},
                ("/observe",id,"")=>crate::conversation::parse_id(id).map(Command::Observe),
                _=>Err("Use /init, /project add PATH, /project list, /project select ID, /project rename NAME, or /observe ID".into()),
            };
            match request {
                Ok(request) => self.start_conversation_request(
                    super::conversation::Request::Setup(request),
                    draft,
                ),
                Err(error) => self.notice = error,
            }
            return false;
        }
        if name == "/audit" {
            let words: Vec<_> = draft.split_whitespace().collect();
            let limit = match words.as_slice() {
                [_] => Some(16),
                [_, value] if value.bytes().all(|b| b.is_ascii_digit()) => {
                    value.parse::<u32>().ok().filter(|n| (1..=16).contains(n))
                }
                _ => None,
            };
            let Some(limit) = limit else {
                self.notice = "Use /audit [limit], 1 through 16; draft retained".into();
                return false;
            };
            if self.audit_draft.is_some() {
                self.notice = "Audit inspection busy; draft retained".into();
            } else if let Some(scope) = self.queue_scope() {
                self.audit_scope = Some(scope.clone());
                self.audit_request = Some((scope, limit));
                self.audit_draft = Some(draft);
                self.notice = "Audit inspection pending".into();
            } else {
                self.notice =
                    "Select a registered project with a connected service; draft retained".into();
            }
            return false;
        }
        if name == "/tools" {
            if draft.split_whitespace().count() != 1 {
                self.notice = "Use /tools without arguments; draft retained".into();
            } else {
                self.editor = Editor::new();
                self.notice.clear();
                self.overlay = Some(Overlay::Tools(0));
            }
            return false;
        }
        if name == "/models" {
            if draft.split_whitespace().count() != 1 {
                self.notice = "Use /models without arguments; draft retained".into();
            } else if self.models_draft.is_some() || self.picker_models_pending {
                self.notice = "Model inventory busy; draft retained".into();
            } else {
                self.models_draft = Some(draft);
                self.models_request = true;
                self.notice = "Model inventory pending".into();
            }
            return false;
        }
        if draft.split_whitespace().next() == Some("/config") {
            if self.config_draft.is_some() {
                self.notice = "Configuration request busy; draft retained".into();
            } else {
                match super::config::parse(&draft) {
                    Ok(request) => {
                        self.config_draft = Some(draft);
                        self.config_request = Some(request);
                        self.notice = "Configuration request pending".into();
                    }
                    Err(error) => self.notice = error.into(),
                }
            }
            return false;
        }
        let mut words = draft.split_whitespace();
        let name = words.next().unwrap_or_default();
        if !LOCAL_COMMANDS.contains(&name) {
            self.notice = "Unknown command; use /help. Draft retained".into();
        } else if words.next().is_some() {
            self.notice = "This command takes no arguments; draft retained".into();
        } else if name == "/help" {
            self.editor = Editor::new();
            self.notice.clear();
            self.overlay = Some(Overlay::Help(0));
        } else if matches!(name, "/quit" | "/exit") && !self.local_outbox.is_empty() {
            self.editor = Editor::new();
            self.overlay = Some(Overlay::Exit(false));
        } else {
            return true;
        }
        false
    }

    pub fn draw(&mut self, frame: &mut Frame) {
        if matches!(&self.overlay,Some(Overlay::Audit(scope,_,_)) if Some(scope)!=self.queue_identity_scope().as_ref())
        {
            self.overlay = None;
        }
        if self.overlay.is_none()
            && self.picker.is_none()
            && let Some((scope, text)) = self.audit_result.take()
            && Some(&scope) == self.queue_identity_scope().as_ref()
        {
            self.overlay = Some(Overlay::Audit(scope, text, 0));
        }

        if self.overlay.is_none()
            && self.picker.is_none()
            && let Some(result) = self.models_result.take()
        {
            self.overlay = Some(Overlay::Models(result, 0));
        }
        if self.overlay.is_none()
            && self.picker.is_none()
            && let Some(result) = self.config_result.take()
        {
            self.overlay = Some(Overlay::Config(result, 0));
        }
        if self.overlay.is_none()
            && self.picker.is_none()
            && self.capture.is_none()
            && !self.conversation_busy
            && let Some(path) = self.project_offer.take()
        {
            self.overlay = Some(Overlay::ProjectOffer(path, true));
        }
        let full_area = frame.area();
        let area = full_area;
        let text = Style::default().fg(Color::Rgb(230, 237, 240));
        let muted = Style::default().fg(Color::Rgb(134, 174, 200));
        let band = text.bg(Color::Rgb(37, 59, 78));
        let panel = text.bg(Color::Rgb(55, 66, 76));
        frame.render_widget(Clear, area);
        let too_small = area.width < 30 || area.height < 8;
        let area = Rect::new(
            area.x + u16::from(area.width > 0),
            area.y,
            area.width.saturating_sub(2),
            area.height,
        );
        if too_small {
            frame.render_widget(
                Paragraph::new("Resize terminal to at least 30 × 8\nCtrl+Q to exit").style(text),
                area,
            );
            self.draw_overlay(frame, area, panel);
            return;
        }
        frame.render_widget(
            Paragraph::new("").style(panel),
            Rect::new(full_area.x, full_area.y, full_area.width, 1),
        );
        let header = self.status_header();
        line(frame, area, 0, &header, panel);
        line(
            frame,
            full_area,
            1,
            &"▀".repeat(full_area.width as usize),
            Style::default().fg(Color::Rgb(55, 66, 76)),
        );
        line(
            frame,
            area,
            2,
            &format!("Lifecycle {}", self.view.lifecycle),
            muted,
        );
        line(
            frame,
            area,
            3,
            &format!("Installation {}", self.view.installation),
            muted,
        );
        if area.height >= 12 {
            line(frame, area, 4, &literal(self.view.reason, 128), muted);
            if let Some(epoch) = &self.view.epoch {
                line(frame, area, 5, &format!("Epoch {epoch}"), muted);
            }
        }
        if self.view.connection == "unavailable" && area.height >= 12 {
            line(
                frame,
                area,
                6,
                if self.view.reason == "incompatible_protocol" {
                    "Existing backend incompatible · restart it with the current build"
                } else {
                    "Backend unavailable · see ~/.asura/logs/asura.log"
                },
                text,
            );
        }
        let strips = u16::from(area.width >= 60 && area.height >= 16);
        let rows = self
            .editor
            .visual_rows(area.width.saturating_sub(2))
            .clamp(1, if area.width >= 60 { 6 } else { 3 })
            .min(area.height.saturating_sub(7 + 2 * strips).max(1));
        let picker_height = self.picker_height(area.height);
        let editor_y = area
            .bottom()
            .saturating_sub(2 + picker_height + strips + rows);
        let tray_height =
            self.draw_queue(frame, full_area, editor_y.saturating_sub(strips + 1), panel);
        if !self.transcript.is_empty() {
            let history_area = Rect::new(
                full_area.x,
                area.y + 2,
                full_area.width,
                editor_y.saturating_sub(area.y + 4 + strips + tray_height),
            );
            self.response_page = usize::from(history_area.height).max(1);
            self.history_page = usize::from(history_area.height).max(1);
            self.response_scroll_max = render_history(
                frame,
                history_area,
                History {
                    transcript: &self.transcript,
                    rendered_responses: &self.rendered_responses,
                    activity: &self.activity,
                },
                self.response_selection
                    .map(|index| (index, self.response_scroll)),
                self.history_focus.then_some(self.history_scroll),
                strips,
                (text, text.bg(Color::Rgb(38, 51, 65))),
            );
            self.response_scroll = self.response_scroll.min(self.response_scroll_max);
            self.history_scroll_max = self.response_scroll_max;
            self.history_scroll = self.history_scroll.min(self.history_scroll_max);
        } else {
            self.history_scroll_max = 0;
            self.history_scroll = 0;
            self.history_focus = false;
        }

        let notice = if self.capture.is_some() && self.notice.is_empty() {
            "Paste capture · Ctrl+V to finish · draft unchanged"
        } else {
            &self.notice
        };
        if !notice.is_empty() {
            line(
                frame,
                area,
                editor_y.saturating_sub(area.y + strips + 1),
                notice,
                text,
            );
        }
        if strips > 0 {
            let top_separator = if self.overlay.is_some() || tray_height > 0 {
                "█"
            } else {
                "▄"
            };
            line(
                frame,
                full_area,
                editor_y - area.y - 1,
                &top_separator.repeat(full_area.width as usize),
                Style::default().fg(Color::Rgb(37, 59, 78)),
            );
        }
        let editor_area = Rect::new(full_area.x, editor_y, full_area.width, rows);
        frame.render_widget(Paragraph::new("").style(band), editor_area);
        frame.render_widget(
            Paragraph::new("›").style(band.fg(Color::Rgb(143, 211, 244))),
            Rect::new(area.x, editor_y, 1, 1),
        );
        let focused = self.overlay.is_none()
            && self.capture.is_none()
            && self.focus == Focus::Editor
            && self.response_selection.is_none()
            && !self.history_focus
            && self.picker.is_none();
        self.editor.render(
            Rect::new(area.x + 2, editor_y, area.width - 2, rows),
            frame.buffer_mut(),
            band,
            focused,
        );
        if strips > 0 {
            line(
                frame,
                full_area,
                editor_y + rows - area.y,
                &"▀".repeat(full_area.width as usize),
                Style::default().fg(Color::Rgb(37, 59, 78)),
            );
        }
        let status_area = Rect::new(area.x, area.bottom() - 2 - picker_height, area.width, 1);
        let project_name = self
            .selected_project
            .map(|id| self.project_name(id))
            .unwrap_or_else(|| "No project".into());
        let model_name = self
            .observed_model
            .as_deref()
            .or(self.view.configured_model.as_deref())
            .unwrap_or("?");
        let git = self.context.report();
        let (project, tail, model) = status_groups(
            self.editor.character_count(),
            area.width,
            &project_name,
            model_name,
            self.context_directory(),
            git.as_ref(),
            self.model_context.as_ref(),
        );
        let right_width = unicode_display_width::width(&model) as u16;
        frame.render_widget(
            Paragraph::new(Line::from(
                std::iter::once(Span::styled(
                    project,
                    self.focus_style(
                        Focus::Project,
                        Style::default().fg(Color::Rgb(143, 211, 244)),
                    ),
                ))
                .chain(tail)
                .collect::<Vec<_>>(),
            ))
            .style(muted),
            Rect::new(
                status_area.x,
                status_area.y,
                status_area.width - right_width - 2,
                1,
            ),
        );
        frame.render_widget(
            Paragraph::new(model).style(self.focus_style(Focus::Model, muted)),
            Rect::new(
                status_area.right() - right_width,
                status_area.y,
                right_width,
                1,
            ),
        );
        self.draw_picker(
            frame,
            Rect::new(
                full_area.x,
                status_area.y + 1,
                full_area.width,
                picker_height,
            ),
            panel,
        );
        line(
            frame,
            area,
            area.height - 1,
            &self.hints(),
            Style::default().fg(Color::Rgb(145, 145, 145)),
        );
        if focused && let Some(cursor) = self.editor.cursor() {
            frame.set_cursor_position(cursor);
        }
        self.draw_overlay(
            frame,
            Rect::new(
                full_area.x,
                full_area.y + 2,
                full_area.width,
                editor_y.saturating_sub(
                    full_area.y + 2 + strips + u16::from(!notice.is_empty()) + tray_height,
                ),
            ),
            panel,
        );
    }
    fn draw_overlay(&mut self, frame: &mut Frame, area: Rect, style: Style) {
        self.panel_scroll_max = 0;
        let Some(overlay) = &mut self.overlay else {
            return;
        };
        let content = match overlay {
            Overlay::Commands(header, selected) => {
                let rows: Vec<_> = LOCAL_COMMANDS
                    .iter()
                    .filter(|name| name.starts_with(&header.name))
                    .enumerate()
                    .map(|(i, name)| {
                        format!("{} {name}", if *selected == Some(i) { "›" } else { " " })
                    })
                    .collect();
                format!(
                    "Local commands\n{}\nArrows select · Tab/Enter complete\nEsc cancel · Enter again to run",
                    rows.join("\n")
                )
            }
            Overlay::Models(result, _) => {
                let table = match result {
                    Ok(reply) => models_table(reply, area.width.saturating_sub(2) as usize),
                    Err(error) => format!(
                        "Model inventory error: {}\nDraft retained.",
                        literal(error, 1024)
                    ),
                };
                let scrolls = Paragraph::new(table.as_str())
                    .wrap(Wrap { trim: false })
                    .line_count(area.width.saturating_sub(2))
                    + 1
                    > usize::from(area.height.saturating_sub(2));
                format!(
                    "Models · Esc closes{}\n{table}",
                    if scrolls { " · arrows scroll" } else { "" }
                )
            }
            Overlay::Config(value, _) => {
                let content = format!("{} · Esc closes\n{value}", self.result_title);
                if Paragraph::new(content.clone())
                    .wrap(Wrap { trim: false })
                    .line_count(area.width.saturating_sub(2))
                    > usize::from(area.height.saturating_sub(2))
                {
                    format!(
                        "{} · Esc closes · arrows scroll\n{value}",
                        self.result_title
                    )
                } else {
                    content
                }
            }
            Overlay::ProjectOffer(path, yes) => format!(
                "Use this directory as a project?\n{}\n{}\nTab/arrows select · Enter confirms · Esc declines",
                literal(path, 4096),
                if *yes { "[Yes]    No" } else { " Yes    [No]" }
            ),
            Overlay::Audit(_, value, _) => {
                let content = format!("Audit · Esc closes\n{value}");
                if Paragraph::new(content.clone())
                    .wrap(Wrap { trim: false })
                    .line_count(area.width.saturating_sub(2))
                    > usize::from(area.height.saturating_sub(2))
                {
                    content.replacen(
                        "Audit · Esc closes",
                        "Audit · Esc closes · arrows scroll",
                        1,
                    )
                } else {
                    content
                }
            }
            Overlay::Tools(_) => {
                let table = tools_table(area.width.saturating_sub(2) as usize);
                let content = format!(
                    "Tools · Esc closes\nRegistered tools; availability depends on grants and model.\n{table}"
                );
                let scrolls = Paragraph::new(content.clone())
                    .wrap(Wrap { trim: false })
                    .line_count(area.width.saturating_sub(2))
                    > usize::from(area.height.saturating_sub(2));
                if scrolls {
                    content.replacen(
                        "Tools · Esc closes",
                        "Tools · Esc closes · arrows scroll",
                        1,
                    )
                } else {
                    content
                }
            }
            Overlay::Help(_) => {
                let table = help_table(area.width.saturating_sub(2) as usize);
                let scrolls =
                    table.lines().count() + 1 > usize::from(area.height.saturating_sub(2));
                format!(
                    "Editor help · Esc closes{}\n{table}",
                    if scrolls { " · arrows scroll" } else { "" }
                )
            }
            Overlay::Exit(discard) => format!(
                "{}\n{}\nTab/arrows select · Enter confirm · Esc cancel",
                if self.local_outbox.is_empty() {
                    "Discard draft and exit?".to_string()
                } else {
                    format!(
                        "Exit with {} provisional local input(s)? These are not yet durable.",
                        self.local_outbox.len()
                    )
                },
                if *discard {
                    "Keep editing    [Discard and exit]"
                } else {
                    "[Keep editing]    Discard and exit"
                }
            ),
            Overlay::Paste(choice) => format!(
                "Finish paste\n{}\n{}\nTab selects · Enter confirms · Esc cancels",
                match choice {
                    Some(true) => "[Insert]    Cancel capture",
                    Some(false) => "Insert    [Cancel capture]",
                    None => "Insert    Cancel capture (select an action)",
                },
                if self.capture.as_ref().is_some_and(|c| c.invalid) {
                    "Capture rejected; cancel to keep draft"
                } else {
                    "Draft unchanged until Insert"
                }
            ),
        };
        if area.width < 3 || area.height < 3 {
            return;
        }
        let inner_width = area.width - 2;
        let line_count = Paragraph::new(content.as_str())
            .wrap(Wrap { trim: false })
            .line_count(inner_width);
        let height = line_count.saturating_add(2).min(usize::from(area.height)) as u16;
        let popup = Rect::new(
            area.x,
            area.bottom().saturating_sub(height),
            area.width,
            height,
        );
        frame.render_widget(Clear, popup);
        frame.render_widget(Paragraph::new("").style(style), popup);
        let inner = Rect::new(popup.x + 1, popup.y + 1, popup.width - 2, popup.height - 2);
        let offset = match overlay {
            Overlay::Config(_, offset)
            | Overlay::Help(offset)
            | Overlay::Models(_, offset)
            | Overlay::Tools(offset)
            | Overlay::Audit(_, _, offset) => {
                self.panel_scroll_max = line_count
                    .saturating_sub(usize::from(inner.height))
                    .min(usize::from(u16::MAX)) as u16;
                *offset = (*offset).min(self.panel_scroll_max);
                *offset
            }
            _ => 0,
        };
        frame.render_widget(
            Paragraph::new(content)
                .style(style)
                .wrap(Wrap { trim: false })
                .scroll((offset, 0)),
            inner,
        );
    }
}
fn models_table(reply: &asura_control::pb::ModelsReply, width: usize) -> String {
    fn wrap(value: &str, width: usize) -> Vec<String> {
        let mut lines = vec![String::new()];
        for grapheme in value.graphemes(true) {
            let line = lines.last_mut().unwrap();
            if unicode_display_width::width(line) + unicode_display_width::width(grapheme)
                > width as u64
            {
                lines.push(String::new());
            }
            if unicode_display_width::width(grapheme) <= width as u64 {
                lines.last_mut().unwrap().push_str(grapheme);
            } else {
                lines.last_mut().unwrap().push('?');
            }
        }
        lines
    }
    let width = width.max(1);
    let mut lines = Vec::new();
    if width >= 24 {
        let available = width - 10;
        let provider = 8.min(available / 3);
        let status = 11.min(available / 3);
        let widths = [1, available - provider - status, provider, status];
        let mut row = |cells: [&str; 4]| {
            let wrapped: Vec<_> = cells
                .iter()
                .zip(widths)
                .map(|(cell, width)| wrap(cell, width))
                .collect();
            for index in 0..wrapped.iter().map(Vec::len).max().unwrap_or(0) {
                let mut cells = Vec::new();
                for (cell, width) in wrapped.iter().zip(widths) {
                    let value = cell.get(index).map(String::as_str).unwrap_or("");
                    cells.push(format!(
                        "{}{}",
                        value,
                        " ".repeat(
                            width.saturating_sub(unicode_display_width::width(value) as usize)
                        )
                    ));
                }
                lines.push(cells.join(" │ "));
            }
        };
        row(["*", "Model", "Provider", "Status"]);
        for model in &reply.models {
            let selector = literal(model.selector.as_deref().unwrap_or("?"), 1024);
            let provider = literal(model.provider.as_deref().unwrap_or("?"), 32);
            let marker = if model.selector == reply.configured_model {
                "*"
            } else {
                ""
            };
            row([
                marker,
                &selector,
                &provider,
                model_inventory_status(model.status),
            ]);
        }
    } else {
        // At very narrow widths, retain all fields without horizontal clipping.
        for model in &reply.models {
            let marker = if model.selector == reply.configured_model {
                "* "
            } else {
                ""
            };
            for field in [
                format!(
                    "{marker}{}",
                    literal(model.selector.as_deref().unwrap_or("?"), 1024)
                ),
                literal(model.provider.as_deref().unwrap_or("?"), 32),
                model_inventory_status(model.status).into(),
            ] {
                lines.extend(wrap(&field, width));
            }
        }
    }
    if reply.models.is_empty() {
        lines.extend(wrap("No models discovered", width));
    }
    for model in &reply.models {
        if let Some(detail) = &model.detail {
            lines.extend(wrap(
                &format!(
                    "{}: {}",
                    literal(model.selector.as_deref().unwrap_or("?"), 1024),
                    literal(detail, 256)
                ),
                width,
            ));
        }
    }
    if !reply.issues.is_empty() {
        lines.push(String::new());
        lines.extend(wrap("Provider issues", width));
    }
    for issue in &reply.issues {
        lines.extend(wrap(
            &format!(
                "{}: {}",
                literal(issue.provider.as_deref().unwrap_or("?"), 32),
                literal(issue.reason.as_deref().unwrap_or("unknown"), 256)
            ),
            width,
        ));
    }
    lines.extend(wrap(
        "* configured · installed/listed do not verify inference",
        width,
    ));
    lines.join("\n")
}
fn model_inventory_status(status: Option<u32>) -> &'static str {
    match status {
        Some(1) => "available",
        Some(2) => "installed",
        Some(3) => "listed",
        Some(4) => "unavailable",
        _ => "unchecked",
    }
}

fn audit_table(reply: &asura_control::pb::AuditReply) -> String {
    let mut lines = vec![];
    if let Some(h) = &reply.health {
        let state = match h.state {
            Some(1) => "starting",
            Some(2) => "active",
            Some(3) => "disabled",
            Some(4) => "stale",
            _ => "unavailable",
        };
        lines.push(format!(
            "Health: {state} · dropped {} · hydrated {}",
            h.dropped.unwrap_or(0),
            h.hydrated.unwrap_or(false)
        ));
        lines.push(format!(
            "Window: recent · capacity 256 · older omitted {}",
            h.older_omitted.unwrap_or(false)
        ));
        lines.push(format!(
            "Active settings: enabled {} · keep files {} · max bytes {}",
            h.enabled.unwrap_or(false),
            h.keep_files
                .map_or_else(|| "unknown".into(), |n| n.to_string()),
            h.max_file_bytes
                .map_or_else(|| "unknown".into(), |n| n.to_string())
        ));
    }
    if let Some(error) = &reply.error {
        lines.push(literal(error, 128));
    }
    lines.push("Sequence │ Event        │ Outcome │ Reason".into());
    lines.push("─────────┼──────────────┼─────────┼───────".into());
    for e in &reply.entries {
        let kind = match e.kind {
            Some(1) => "admission",
            Some(2) => "conversation",
            _ => "tool",
        };
        let reason = [
            "none",
            "request conflict",
            "generation conflict",
            "unavailable",
            "denied",
            "invalid",
            "busy",
            "limit",
            "timeout",
            "cancelled",
            "internal",
        ]
        .get(e.reason.unwrap_or(0) as usize)
        .copied()
        .unwrap_or("unknown");
        let outcomes: &[&str] = match e.kind {
            Some(1) => &["unknown", "accepted", "rejected", "unconfirmed"],
            Some(2) => &["unknown", "completed", "failed", "cancelled", "interrupted"],
            _ => &[
                "unknown",
                "success",
                "denied",
                "invalid",
                "unavailable",
                "timeout",
                "cancelled",
                "limit",
            ],
        };
        let outcome = outcomes
            .get(e.outcome.unwrap_or(0) as usize)
            .copied()
            .unwrap_or("unknown");
        lines.push(format!(
            "{:<8} │ {kind:<12} │ {outcome:<11} │ {reason}",
            e.sequence.unwrap_or(0)
        ));
        if let Some(expected) = e.requested_generation {
            lines.push(format!(
                "  requested generation {expected} · current {}",
                e.current_generation
                    .map_or_else(|| "unknown".into(), |n| n.to_string())
            ));
        }
    }
    if reply.entries.is_empty() {
        lines.push("No recent project records".into());
    }
    lines.join("\n")
}

fn tools_table(width: usize) -> String {
    let left = 24.min(width.saturating_sub(3) / 2).max(1);
    let right = width.saturating_sub(left + 3).max(1);
    let wrap = |text: &str, limit: usize| -> Vec<String> {
        text.chars()
            .collect::<Vec<_>>()
            .chunks(limit)
            .map(|chunk| chunk.iter().collect())
            .collect()
    };
    let mut lines = vec![
        format!("{:<left$} │ Description", "Tool"),
        format!("{}─┼─{}", "─".repeat(left), "─".repeat(right)),
    ];
    for tool in asura_service::tools::REGISTRY {
        let names = wrap(tool.name, left);
        let descriptions = wrap(tool.description, right);
        for row in 0..names.len().max(descriptions.len()) {
            lines.push(format!(
                "{:<left$} │ {}",
                names.get(row).map(String::as_str).unwrap_or(""),
                descriptions.get(row).map(String::as_str).unwrap_or("")
            ));
        }
    }
    lines.join("\n")
}

fn help_table(width: usize) -> String {
    let rows = [
        ("Command / key", "Action"),
        ("/help or F1", "Show command help"),
        ("/quit or /exit", "Close this client"),
        ("/new", "Start a new conversation in this project"),
        ("/models", "List models; * marks configured selection"),
        ("/tools", "List registered tool names and descriptions"),
        (
            "/audit [1..16]",
            "Read recent audit metadata for this project",
        ),
        ("/config", "Show current YAML"),
        ("/config get NAME", "Read a setting"),
        (
            "/config set NAME VALUE",
            "Write a setting; dotted names select nested keys",
        ),
        ("/init", "Check installation setup"),
        ("/project add PATH", "Register and select a project"),
        ("/project list", "List projects"),
        ("/project select ID", "Select a project"),
        ("/project rename NAME", "Rename the selected project"),
        ("/queue", "List queued inputs"),
        ("/queue ID", "Show a queued input"),
        ("/queue resume ID", "Resume held input"),
        ("/queue drop ID", "Drop held input"),
        ("/cancel", "Cancel active work"),
        ("/observe ID", "Observe a recorded operation"),
        ("/retry", "Retry with the original request ID"),
        ("Enter", "Submit; queue automatically during active work"),
        (
            "Down at final input row",
            "Focus project/model status; Enter opens selector",
        ),
        (
            "Up at first input row",
            "Focus queued inputs, or scroll conversation history",
        ),
        ("[ / ] in queue", "Move an undispatched input up / down"),
        (
            "Enter in queue",
            "Send selected input now; active response restarts",
        ),
        ("Ctrl+P / Ctrl+N", "Recall submitted input / restore draft"),
        (
            "F6",
            "Browse responses; Enter expands activity; Esc returns",
        ),
        ("Ctrl+S / Ctrl+T", "Steer / queue input"),
        ("Alt+Enter", "Insert a newline"),
        ("Tab", "Complete commands or insert spaces"),
        ("Arrows / Shift+arrows", "Move cursor / select text"),
        ("Ctrl+A", "Select all text"),
        ("Ctrl+Z / Ctrl+Y", "Undo / redo"),
        ("Ctrl+V", "Start or finish paste capture"),
        ("Ctrl+Q / Ctrl+C", "Exit; confirm if a draft exists"),
        ("Esc", "Close the current panel"),
    ];
    let left = 28.min(width.saturating_sub(3) / 2).max(1);
    let right = width.saturating_sub(left + 3).max(1);
    let wrap = |value: &str, limit: usize| {
        let mut lines = vec![String::new()];
        for word in value.split_whitespace() {
            let line = lines.last_mut().unwrap();
            if !line.is_empty() && line.len() + 1 + word.len() > limit {
                lines.push(word.to_owned());
            } else {
                if !line.is_empty() {
                    line.push(' ');
                }
                line.push_str(word);
            }
        }
        lines
    };
    let mut lines = Vec::new();
    for (index, (command, action)) in rows.into_iter().enumerate() {
        let commands = wrap(command, left);
        let actions = wrap(action, right);
        for row in 0..commands.len().max(actions.len()) {
            lines.push(format!(
                "{:<left$} │ {}",
                commands.get(row).map(String::as_str).unwrap_or(""),
                actions.get(row).map(String::as_str).unwrap_or("")
            ));
        }
        if index == 0 {
            lines.push(format!("{}─┼─{}", "─".repeat(left), "─".repeat(right)));
        }
    }
    lines.join("\n")
}
fn queue_state(state: Option<u32>) -> &'static str {
    match state {
        Some(1) => "Queued",
        Some(2) => "Held",
        Some(3) => "Running",
        Some(4) => "Complete",
        Some(5) => "Failed",
        Some(6) => "Dropped",
        _ => "Unknown",
    }
}
// Fit presentation-only unknown values without manufacturing backend observations.
fn project_label(project: &asura_control::pb::ProjectReply) -> String {
    literal(&crate::conversation::project_display_name(project), 128)
}

struct History<'a> {
    transcript: &'a [(String, String)],
    rendered_responses: &'a [Text<'static>],
    activity: &'a [activity::Activity],
}

fn render_history(
    frame: &mut Frame,
    area: Rect,
    history: History<'_>,
    selection: Option<(usize, usize)>,
    full_scroll: Option<usize>,
    strips: u16,
    styles: (Style, Style),
) -> usize {
    let (text, band) = styles;
    struct Block {
        paragraph: Paragraph<'static>,
        height: usize,
        inset: u16,
        style: Style,
        marker: Option<&'static str>,
    }
    let mut blocks = Vec::new();
    for (index, (prompt, response)) in history.transcript.iter().enumerate() {
        if selection.is_some_and(|(selected, _)| selected != index) {
            continue;
        }
        let log = history.activity.get(index).map(activity::Activity::display);
        let live = history
            .activity
            .get(index)
            .is_some_and(activity::Activity::active);
        let log_style = if selection.is_some() {
            text.bg(Color::Rgb(70, 82, 94))
                .fg(Color::Rgb(245, 248, 250))
        } else {
            text.fg(Color::Rgb(139, 161, 177))
        };
        for (content, inset, style, marker) in [
            (
                (strips > 0).then(|| Text::raw("▄".repeat(area.width as usize))),
                0,
                Style::default().fg(band.bg.unwrap_or(Color::Reset)),
                None,
            ),
            (
                Some(Text::raw(literal_multiline(prompt, 32768))),
                3,
                band,
                Some("›"),
            ),
            (
                (strips > 0).then(|| Text::raw("▀".repeat(area.width as usize))),
                0,
                Style::default().fg(band.bg.unwrap_or(Color::Reset)),
                None,
            ),
            (
                log.clone().filter(|_| !live).map(Text::raw),
                1,
                log_style,
                None,
            ),
            (
                Some(
                    history
                        .rendered_responses
                        .get(index)
                        .cloned()
                        .unwrap_or_else(|| Text::raw(response.clone())),
                ),
                3,
                text,
                (!response.is_empty()).then_some("▪"),
            ),
            (log.filter(|_| live).map(Text::raw), 1, log_style, None),
            (Some(Text::default()), 0, text, None),
        ] {
            let Some(content) = content else { continue };
            let width = area.width.saturating_sub(inset + u16::from(inset > 0));
            let paragraph = Paragraph::new(content)
                .style(style)
                .wrap(Wrap { trim: false });
            let height = paragraph.line_count(width).max(1);
            blocks.push(Block {
                paragraph,
                height,
                inset,
                style,
                marker,
            });
        }
    }
    let total: usize = blocks.iter().map(|block| block.height).sum();
    let max_scroll = total.saturating_sub(area.height as usize);
    let mut skip = selection
        .map(|(_, scroll)| scroll.min(max_scroll))
        .or_else(|| full_scroll.map(|scroll| scroll.min(max_scroll)))
        .unwrap_or(max_scroll);
    let mut y = area.y;
    frame.render_widget(Clear, area);
    for block in blocks {
        if skip >= block.height {
            skip -= block.height;
            continue;
        }
        let height = (block.height - skip).min((area.bottom() - y) as usize) as u16;
        if height == 0 {
            break;
        }
        let row = Rect::new(area.x, y, area.width, height);
        frame.render_widget(Paragraph::new("").style(block.style), row);
        let body = Rect::new(
            area.x + block.inset,
            y,
            area.width
                .saturating_sub(block.inset + u16::from(block.inset > 0)),
            height,
        );
        frame.render_widget(block.paragraph.scroll((skip as u16, 0)), body);
        if let Some(marker) = block.marker.filter(|_| skip == 0) {
            frame.render_widget(
                Paragraph::new(marker).style(block.style.fg(if marker == "▪" {
                    Color::Rgb(37, 59, 78)
                } else {
                    Color::Rgb(143, 211, 244)
                })),
                Rect::new(area.x + 1, y, 1, 1),
            );
        }
        y += height;
        skip = 0;
    }
    max_scroll
}

fn status_groups(
    count: usize,
    width: u16,
    project_name: &str,
    model_name: &str,
    directory: Option<&str>,
    git: Option<&super::context::Report>,
    context: Option<&asura_control::pb::ModelContext>,
) -> (String, Vec<Span<'static>>, String) {
    let compact = width < 60;
    let available = usize::from(width - 2);
    let count = if count == 0 {
        String::new()
    } else if compact {
        format!(" · {count}c")
    } else {
        format!(" · {count} chars")
    };
    let right_budget = (available / 2).min(available.saturating_sub(count.len() + 4));
    let measured = context
        .and_then(
            |value| match (value.input_tokens, value.capacity_tokens, value.basis) {
                (Some(input), Some(capacity), Some(1)) if capacity > 0 && input <= capacity => {
                    Some(format!(
                        "{}% input",
                        u64::from(input) * 100 / u64::from(capacity)
                    ))
                }
                _ => None,
            },
        )
        .unwrap_or_else(|| "0%".into());
    let model = format!(
        "{measured} · {}",
        shorten(model_name, right_budget.saturating_sub(measured.len() + 3))
    );
    let left_budget = available - unicode_display_width::width(&model) as usize - 2;
    let mut tail = vec![Span::raw(count.clone())];
    if !compact {
        let path = directory
            .map(|path| literal(path, 4096))
            .unwrap_or_else(|| "path ?".into());
        let git_text = git.map(|value| value.label()).unwrap_or_default();
        let path_budget = left_budget.saturating_sub(
            unicode_display_width::width(project_name) as usize
                + count.chars().count()
                + 3
                + git.map_or(0, |_| unicode_display_width::width(&git_text) as usize + 3),
        );
        let path = shorten_path(&path, path_budget);
        for (with_path, with_git) in [(true, true), (false, true), (true, false)] {
            let prefix = if with_path && !path.is_empty() {
                format!(" · {path}")
            } else {
                String::new()
            };
            let selected_git = git.filter(|_| with_git);
            let fields = format!(
                "{prefix}{}",
                selected_git
                    .map(|_| format!(" · {git_text}"))
                    .unwrap_or_default()
            );
            if !fields.is_empty()
                && unicode_display_width::width(project_name) as usize
                    + unicode_display_width::width(&fields) as usize
                    + count.chars().count()
                    <= left_budget
            {
                tail = vec![Span::raw(prefix)];
                if let Some(git) = selected_git {
                    tail.push(Span::raw(format!(" · {} ", literal(&git.heading(), 1200))));
                    tail.push(Span::styled(
                        git.addition(),
                        Style::default().fg(Color::Rgb(134, 239, 172)),
                    ));
                    tail.push(Span::styled(
                        git.deletion(),
                        Style::default().fg(Color::Rgb(252, 165, 165)),
                    ));
                }
                tail.push(Span::raw(count.clone()));
                break;
            }
        }
    }
    let tail_width: usize = tail
        .iter()
        .map(|span| unicode_display_width::width(&span.content) as usize)
        .sum();
    let project = shorten(project_name, left_budget.saturating_sub(tail_width));
    (project, tail, model)
}

fn shorten_path(path: &str, budget: usize) -> String {
    let components: Vec<_> = path.split('/').filter(|part| !part.is_empty()).collect();
    let trimmed;
    let path = if components.len() > 4 {
        trimmed = format!("…/{}", components[components.len() - 4..].join("/"));
        trimmed.as_str()
    } else {
        path
    };
    if unicode_display_width::width(path) as usize <= budget {
        return path.into();
    }
    if budget == 0 {
        return String::new();
    }
    let mut suffix = String::new();
    for grapheme in path.graphemes(true).rev() {
        if unicode_display_width::width(grapheme) as usize
            + unicode_display_width::width(&suffix) as usize
            + 1
            > budget
        {
            break;
        }
        suffix.insert_str(0, grapheme);
    }
    format!("…{suffix}")
}

fn line(frame: &mut Frame, area: Rect, row: u16, text: &str, style: Style) {
    if row < area.height {
        frame.render_widget(
            Paragraph::new(shorten(text, area.width as usize)).style(style),
            Rect::new(area.x, area.y + row, area.width, 1),
        );
    }
}
fn literal(text: &str, max: usize) -> String {
    sanitize(text, max, false)
}
fn literal_multiline(text: &str, max: usize) -> String {
    sanitize(text, max, true)
}
fn sanitize(text: &str, max: usize, newlines: bool) -> String {
    text.chars()
        .scan(0, |bytes, c| {
            *bytes += c.len_utf8();
            (*bytes <= max).then_some(
                if (c.is_control() && !(newlines && c == '\n'))
                    || matches!(c, '\u{2028}' | '\u{2029}')
                {
                    ' '
                } else {
                    c
                },
            )
        })
        .collect()
}
fn shorten(text: &str, cells: usize) -> String {
    if unicode_display_width::width(text) <= cells as u64 {
        return text.into();
    }
    if cells == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut width = 0;
    for cluster in text.graphemes(true) {
        let n = unicode_display_width::width(cluster) as usize;
        if width + n > cells - 1 {
            break;
        }
        out.push_str(cluster);
        width += n;
    }
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyEvent;
    use ratatui::{Terminal, backend::TestBackend};
    fn key(app: &mut App, code: KeyCode, modifiers: KeyModifiers) -> bool {
        app.handle(Event::Key(KeyEvent::new(code, modifiers)))
    }
    fn render(app: &mut App, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|c| c.symbol())
            .collect()
    }
    #[test]
    fn project_rename_captures_selected_identity_and_keeps_model_worker_independent() {
        let mut app = App::new();
        app.insert("/project rename my project");
        app.submit();
        assert!(app.take_project_admin_request().is_none());
        assert_eq!(app.draft(), "/project rename my project");
        app.editor = Editor::new();
        app.projects = Some(vec![asura_control::pb::ProjectReply {
            project_id: Some(vec![1; 16]),
            location: Some("/work/asura".into()),
            registry_revision: Some(1),
            name: Some("Asura".into()),
            name_revision: Some(4),
            current: Some(true),
            ..Default::default()
        }]);
        app.selected_project = Some([1; 16]);
        app.view.epoch = Some(crate::conversation::hex(&[9; 16]));
        app.conversation_busy = true;
        app.insert("/project rename my project");
        app.submit();
        let request = app
            .take_project_admin_request()
            .expect("independent admin slot");
        assert_eq!(
            request.command.project_id.as_deref(),
            Some([1; 16].as_slice())
        );
        assert_eq!(request.command.expected_name_revision, Some(4));
        assert_eq!(request.command.name.as_deref(), Some("my project"));
        assert_eq!(app.draft(), "/project rename my project");
    }

    #[test]
    fn project_rename_late_result_updates_only_target_and_uncertain_retry_keeps_id() {
        let mut app = App::new();
        app.projects = Some(
            (1..=2)
                .map(|id| asura_control::pb::ProjectReply {
                    project_id: Some(vec![id; 16]),
                    location: Some(format!("/work/project{id}")),
                    registry_revision: Some(1),
                    current: Some(true),
                    name_revision: Some(0),
                    ..Default::default()
                })
                .collect(),
        );
        app.selected_project = Some([1; 16]);
        app.view.epoch = Some(crate::conversation::hex(&[9; 16]));
        app.insert("/project rename shared name");
        app.submit();
        let request = app.take_project_admin_request().unwrap();
        let original_id = request.command.request_id.clone();
        app.selected_project = Some([2; 16]);
        app.project_admin_finished(super::super::project_admin::Update {
            request: request.clone(),
            outcome: super::super::project_admin::Outcome::Unconfirmed("connection lost".into()),
            service_epoch: None,
            settled: true,
        });
        app.editor = Editor::new();
        app.insert("/retry");
        app.submit();
        let retry = app.take_project_admin_request().unwrap();
        assert_eq!(retry.command.request_id, original_id);
        let changed = asura_control::pb::ProjectReply {
            project_id: Some(vec![1; 16]),
            location: Some("/work/project1".into()),
            registry_revision: Some(1),
            current: Some(true),
            name: Some("Shared name".into()),
            name_revision: Some(1),
            ..Default::default()
        };
        app.project_admin_finished(super::super::project_admin::Update {
            request: retry,
            outcome: super::super::project_admin::Outcome::Reply(
                asura_control::pb::ProjectRenameReply {
                    project: Some(changed.clone()),
                    current_project: Some(changed),
                    changed: Some(true),
                    error: None,
                },
            ),
            service_epoch: Some([9; 16]),
            settled: true,
        });
        assert_eq!(app.project_name([1; 16]), "Shared name");
        assert_eq!(app.project_name([2; 16]), "Project2");
        assert_eq!(app.selected_project, Some([2; 16]));
        assert!(app.project_admin_retained.is_none());
    }
    #[test]
    fn project_rename_rejection_keeps_draft_and_historical_reply_never_lowers_revision() {
        let mut app = App::new();
        let current = asura_control::pb::ProjectReply {
            project_id: Some(vec![1; 16]),
            location: Some("/work/project".into()),
            registry_revision: Some(1),
            current: Some(true),
            name: Some("Current".into()),
            name_revision: Some(3),
            ..Default::default()
        };
        app.projects = Some(vec![current.clone()]);
        app.selected_project = Some([1; 16]);
        app.view.epoch = Some(crate::conversation::hex(&[9; 16]));
        app.insert("/project rename old name");
        app.submit();
        let request = app.take_project_admin_request().unwrap();
        app.project_admin_finished(super::super::project_admin::Update {
            request: request.clone(),
            outcome: super::super::project_admin::Outcome::Definite("invalid_project_name".into()),
            service_epoch: Some([9; 16]),
            settled: true,
        });
        assert_eq!(app.draft(), "/project rename old name");
        assert!(app.project_admin_retained.is_none());
        app.editor = Editor::new();
        app.insert("/project rename old name");
        app.submit();
        let request = app.take_project_admin_request().unwrap();
        let mut historical = current.clone();
        historical.name = Some("Old name".into());
        historical.name_revision = Some(2);
        app.insert("x");
        app.project_admin_finished(super::super::project_admin::Update {
            request,
            outcome: super::super::project_admin::Outcome::Reply(
                asura_control::pb::ProjectRenameReply {
                    project: Some(historical.clone()),
                    current_project: Some(historical),
                    changed: Some(true),
                    error: None,
                },
            ),
            service_epoch: Some([9; 16]),
            settled: true,
        });
        assert_eq!(app.project_name([1; 16]), "Current");
        assert_eq!(app.draft(), "/project rename old namex");
    }

    #[test]
    fn equal_newer_draft_survives_rename_reply() {
        let mut app = App::new();
        app.projects = Some(vec![asura_control::pb::ProjectReply {
            project_id: Some(vec![1; 16]),
            location: Some("/work/project".into()),
            registry_revision: Some(1),
            current: Some(true),
            name_revision: Some(0),
            ..Default::default()
        }]);
        app.selected_project = Some([1; 16]);
        app.view.epoch = Some(crate::conversation::hex(&[9; 16]));
        app.insert("/project rename shared name");
        app.submit();
        let request = app.take_project_admin_request().unwrap();
        app.editor = Editor::new();
        app.insert("/project rename shared name");
        let project = asura_control::pb::ProjectReply {
            project_id: Some(vec![1; 16]),
            location: Some("/work/project".into()),
            registry_revision: Some(1),
            current: Some(true),
            name: Some("Shared name".into()),
            name_revision: Some(1),
            ..Default::default()
        };
        app.project_admin_finished(super::super::project_admin::Update {
            request,
            outcome: super::super::project_admin::Outcome::Reply(
                asura_control::pb::ProjectRenameReply {
                    project: Some(project.clone()),
                    current_project: Some(project),
                    changed: Some(true),
                    error: None,
                },
            ),
            service_epoch: Some([9; 16]),
            settled: true,
        });
        assert_eq!(app.draft(), "/project rename shared name");
    }
    #[test]
    fn project_rename_deadline_keeps_single_slot_until_late_settlement() {
        let mut app = App::new();
        app.projects = Some(vec![asura_control::pb::ProjectReply {
            project_id: Some(vec![1; 16]),
            location: Some("/work/project".into()),
            registry_revision: Some(1),
            current: Some(true),
            name_revision: Some(0),
            ..Default::default()
        }]);
        app.selected_project = Some([1; 16]);
        app.view.epoch = Some(crate::conversation::hex(&[9; 16]));
        app.insert("/project rename one");
        app.submit();
        let request = app.take_project_admin_request().unwrap();
        app.project_admin_finished(super::super::project_admin::Update {
            request: request.clone(),
            outcome: super::super::project_admin::Outcome::Unconfirmed("deadline".into()),
            service_epoch: None,
            settled: false,
        });
        assert!(app.project_admin_busy);
        app.editor = Editor::new();
        app.insert("/project rename two");
        app.submit();
        assert!(app.take_project_admin_request().is_none());
        assert_eq!(
            app.project_admin_retained
                .as_ref()
                .unwrap()
                .command
                .request_id,
            request.command.request_id
        );
        app.project_admin_finished(super::super::project_admin::Update {
            request,
            outcome: super::super::project_admin::Outcome::Definite("invalid_project_name".into()),
            service_epoch: Some([9; 16]),
            settled: true,
        });
        assert!(!app.project_admin_busy);
        assert_eq!(app.draft(), "/project rename two");
    }
    #[test]
    fn delayed_project_list_cannot_regress_a_newer_renamed_projection() {
        let mut app = App::new();
        app.selected_project = Some([1; 16]);
        let row = |name: &str, revision| asura_control::pb::ProjectReply {
            project_id: Some(vec![1; 16]),
            location: Some("/work/project".into()),
            registry_revision: Some(1),
            current: Some(true),
            name: Some(name.into()),
            name_revision: Some(revision),
            ..Default::default()
        };
        app.projects = Some(vec![row("New name", 2)]);
        app.conversation_finished(super::super::conversation::Update {
            projects: Some(vec![row("Old name", 1)]),
            preserve_project_selection: true,
            done: true,
            success: true,
            ..Default::default()
        });
        let displayed = &app.projects.as_ref().unwrap()[0];
        assert_eq!(displayed.name.as_deref(), Some("New name"));
        assert_eq!(displayed.name_revision, Some(2));
        assert_eq!(app.project_name([1; 16]), "New name");
    }
    #[test]
    fn composer_top_separator_fills_when_status_overlay_opens() {
        let mut app = App::new();
        let half = "▄".repeat(80);
        let full = "█".repeat(80);
        assert!(render(&mut app, 80, 24).contains(&half));
        key(&mut app, KeyCode::F(1), KeyModifiers::NONE);
        let open = render(&mut app, 80, 24);
        assert!(open.contains(&full));
        assert!(!open.contains(&half));
        key(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        let closed = render(&mut app, 80, 24);
        assert!(closed.contains(&half));
        assert!(!closed.contains(&full));
    }
    #[test]
    fn new_command_clears_visible_conversation_only_when_safe() {
        let mut app = App::new();
        app.selected_project = Some([7; 16]);
        app.conversation_id = Some(vec![8; 16]);
        app.conversation_generation = 3;
        app.append_response("old prompt".into(), Some(vec![9; 16]));
        app.insert("/new extra");
        key(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(app.draft(), "/new extra");
        assert_eq!(app.conversation_generation, 3);
        app.editor = Editor::new();
        app.insert("/new");
        app.conversation_busy = true;
        key(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(app.conversation_generation, 3);
        assert_eq!(app.draft(), "/new");
        app.conversation_busy = false;
        key(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(app.conversation_id, None);
        assert_eq!(app.conversation_generation, 0);
        assert_eq!(app.new_conversation_intent, Some([7; 16]));
        assert!(app.transcript.is_empty());
        assert_eq!(app.draft(), "");
    }
    #[test]
    fn restored_project_cursor_drives_next_submit_until_explicit_new() {
        let mut app = App::new();
        let project = [7; 16];
        let epoch = [6; 16];
        app.view.connection = "connected";
        app.view.installation = "graph_ready";
        app.view.epoch = Some(crate::conversation::hex(&epoch));
        app.project_discovery_started = true;
        app.selected_project = Some(project);
        assert!(matches!(
            app.take_conversation_request(),
            Some(super::super::conversation::Request::Restore { project: p, epoch: e })
                if p == project && e == epoch
        ));
        app.conversation_finished(super::super::conversation::Update {
            restore: Some(super::super::conversation::RestoreUpdate {
                project,
                epoch,
                result: Ok(super::super::conversation::Restored {
                    conversation_id: Some(vec![8; 16]),
                    generation: 2,
                    turns: vec![super::super::conversation::RestoredTurn {
                        prompt: "earlier".into(),
                        event: asura_control::pb::ConversationEvent {
                            operation_id: Some(vec![9; 16]),
                            generation: Some(2),
                            cursor: Some(u64::MAX),
                            kind: Some(3),
                            text: Some("completed".into()),
                            ..Default::default()
                        },
                    }],
                }),
            }),
            done: true,
            ..Default::default()
        });
        assert!(app.restoration_ready);
        assert_eq!(app.transcript, vec![("earlier".into(), "completed".into())]);
        app.insert("next");
        app.submit();
        let Some(request @ super::super::queue::Request::Submit(_)) =
            app.take_queue_request(std::time::Instant::now())
        else {
            panic!("restored queue submission missing")
        };
        let super::super::queue::Request::Submit(sent) = &request else {
            unreachable!()
        };
        assert_eq!(sent.conversation_id, Some(vec![8; 16]));
        assert_eq!(sent.expected_generation, Some(2));
        acknowledge_local_input(&mut app, request, [8; 16]);
        app.conversation_busy = false;
        app.retained_request = None;
        app.queue_entries.clear();
        app.editor = Editor::new();
        app.insert("/new");
        app.submit();
        assert_eq!(app.new_conversation_intent, Some(project));
        app.insert("fresh");
        app.submit();
        let Some(super::super::queue::Request::Submit(request)) =
            app.take_queue_request(std::time::Instant::now())
        else {
            panic!("new conversation queue submission missing")
        };
        assert_eq!(request.conversation_id, None);
        assert_eq!(request.expected_generation, Some(0));
    }
    #[test]
    fn help_table_wraps_columns_and_scrolls_only_when_needed() {
        for width in [28, 38, 78, 118] {
            let table = help_table(width);
            assert!(table.contains("Action"));
            assert!(
                table
                    .lines()
                    .all(|line| unicode_display_width::width(line) <= width as u64)
            );
            assert!(table.contains("/retry"));
        }
        let mut app = App::new();
        key(&mut app, KeyCode::F(1), KeyModifiers::NONE);
        let screen = render(&mut app, 80, 60);
        assert!(screen.contains("Command / key"));
        assert!(screen.contains("Action"));
        assert!(!screen.contains("arrows scroll"));
        key(&mut app, KeyCode::Down, KeyModifiers::NONE);
        assert!(matches!(app.overlay, Some(Overlay::Help(0))));
        let compact = render(&mut app, 40, 16);
        assert!(compact.contains("arrows") && compact.contains("scroll"));
        assert!(app.panel_scroll_max > 0);
        for _ in 0..200 {
            key(&mut app, KeyCode::Down, KeyModifiers::NONE);
        }
        assert!(render(&mut app, 40, 16).contains("Close the"));
        key(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        assert!(app.overlay.is_none());
        key(&mut app, KeyCode::F(1), KeyModifiers::NONE);
        assert!(matches!(app.overlay, Some(Overlay::Help(0))));
    }
    fn inventory() -> asura_control::pb::ModelsReply {
        use asura_control::pb::{ModelInventoryEntry, ModelInventoryIssue, ModelsReply};
        ModelsReply {
            configured_model: Some("coreai:qwen".into()),
            models: [
                ("system", "system", 1),
                ("coreai:qwen", "coreai", 2),
                ("ollama:example", "ollama", 3),
                ("mlx:missing", "mlx", 5),
                ("bad", "unknown", 4),
            ]
            .into_iter()
            .map(|(selector, provider, status)| ModelInventoryEntry {
                selector: Some(selector.into()),
                provider: Some(provider.into()),
                status: Some(status),
                detail: None,
            })
            .collect(),
            issues: vec![ModelInventoryIssue {
                provider: Some("mlx".into()),
                reason: Some("inventory_limit".into()),
            }],
            error: None,
        }
    }
    #[test]
    fn models_completion_arity_busy_and_draft_preservation() {
        let mut app = App::new();
        app.insert("/mod");
        key(&mut app, KeyCode::Tab, KeyModifiers::NONE);
        assert_eq!(app.draft(), "/models");
        key(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert!(app.take_models_request());
        key(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert!(!app.take_models_request());
        assert!(app.notice.contains("busy"));
        app.models_finished(Err("models_busy".into()));
        assert_eq!(app.draft(), "/models");
        key(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert!(app.take_models_request());
        app.editor = Editor::new();
        app.insert("new draft");
        app.models_finished(Ok(inventory()));
        assert_eq!(app.draft(), "new draft");
        app.editor = Editor::new();
        app.insert("/models extra");
        key(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert!(!app.take_models_request());
        assert_eq!(app.draft(), "/models extra");
        assert!(app.notice.contains("without arguments"));
    }
    #[test]
    fn models_table_marks_selection_and_scrolls_only_on_overflow() {
        let reply = inventory();
        for width in [1, 12, 24, 40, 80, 140] {
            let table = models_table(&reply, width);
            for line in table.lines() {
                assert!(
                    unicode_display_width::width(line) <= width as u64,
                    "{width}: {line}"
                );
            }
        }
        let table = models_table(&reply, 100);
        assert!(table.lines().any(|row| row.starts_with("* │ coreai:qwen")));
        for text in [
            "available",
            "installed",
            "listed",
            "unchecked",
            "unavailable",
            "Provider issues",
            "inventory_limit",
        ] {
            assert!(table.contains(text), "{text}");
        }
        let mut app = App::new();
        app.models_finished(Ok(reply));
        let screen = render(&mut app, 100, 35);
        assert!(screen.contains("Models · Esc closes"));
        assert!(!screen.contains("arrows scroll"));
        key(&mut app, KeyCode::Down, KeyModifiers::NONE);
        assert!(matches!(app.overlay, Some(Overlay::Models(_, 0))));
        render(&mut app, 60, 12);
        assert!(app.panel_scroll_max > 0);
        for _ in 0..100 {
            key(&mut app, KeyCode::Down, KeyModifiers::NONE);
        }
        assert!(
            matches!(app.overlay, Some(Overlay::Models(_, offset)) if offset == app.panel_scroll_max)
        );
        render(&mut app, 100, 35);
        assert!(matches!(app.overlay, Some(Overlay::Models(_, 0))));
    }
    #[test]
    fn models_cancel_and_result_wait_for_existing_panel() {
        let mut app = App::new();
        app.insert("/models");
        key(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert!(app.take_models_request());
        app.editor = Editor::new();
        app.insert("/cancel");
        key(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert!(app.take_models_cancel());
        assert!(!app.take_cancel_request());
        key(&mut app, KeyCode::F(1), KeyModifiers::NONE);
        app.models_finished(Err("cancelled".into()));
        assert!(render(&mut app, 100, 35).contains("Editor help"));
        key(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        assert!(render(&mut app, 100, 35).contains("Model inventory error: cancelled"));
    }
    #[test]
    fn models_success_clears_only_its_submitted_draft() {
        let mut app = App::new();
        app.insert("/models");
        key(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert!(app.take_models_request());
        app.models_finished(Ok(inventory()));
        assert!(app.draft().is_empty());
    }

    #[test]
    fn config_scroll_only_overflows_and_clamps_after_resize() {
        let mut app = App::new();
        app.config_finished(Ok((0..8)
            .map(|i| format!("key{i}: value"))
            .collect::<Vec<_>>()
            .join("\n")));
        assert!(!render(&mut app, 80, 24).contains("arrows scroll"));
        key(&mut app, KeyCode::Down, KeyModifiers::NONE);
        assert!(matches!(app.overlay, Some(Overlay::Config(_, 0))));
        assert!(render(&mut app, 80, 10).contains("arrows scroll"));
        assert!(app.panel_scroll_max > 0);
        for _ in 0..30 {
            key(&mut app, KeyCode::Down, KeyModifiers::NONE);
        }
        assert!(
            matches!(app.overlay, Some(Overlay::Config(_, offset)) if offset == app.panel_scroll_max)
        );
        assert!(render(&mut app, 80, 10).contains("key7: value"));
        assert!(!render(&mut app, 80, 24).contains("arrows scroll"));
        assert!(matches!(app.overlay, Some(Overlay::Config(_, 0))));
        // Header plus four YAML rows exactly fill this five-row content viewport.
        app.overlay = None;
        app.config_finished(Ok("a: 1\nb: 2\nc: 3\nd: 4".into()));
        assert!(!render(&mut app, 80, 12).contains("arrows scroll"));
        assert_eq!(app.panel_scroll_max, 0);
    }
    #[test]
    fn history_has_muted_band_and_aligned_response_marker() {
        for (width, height) in [(80, 24), (40, 14)] {
            let mut app = App::new();
            app.transcript
                .push(("hello\nsecond line".into(), "Hello! How can I help?".into()));
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal.draw(|frame| app.draw(frame)).unwrap();
            let buffer = terminal.backend().buffer();
            let strips = u16::from(width >= 60 && height >= 16);
            let prompt_y = 2 + strips;
            let editor_y = height - 3 - strips;
            for x in 0..width {
                assert_eq!(buffer[(x, prompt_y)].bg, Color::Rgb(38, 51, 65));
                assert_eq!(buffer[(x, editor_y)].bg, Color::Rgb(37, 59, 78));
                assert_eq!(buffer[(x, prompt_y + 1)].bg, Color::Rgb(38, 51, 65));
            }
            assert_eq!(
                buffer[(1, prompt_y)].symbol(),
                buffer[(1, editor_y)].symbol()
            );
            let response_y = prompt_y + 2 + strips;
            assert_eq!(buffer[(1, response_y)].symbol(), "▪");
            assert_eq!(buffer[(2, response_y)].symbol(), " ");
            assert_eq!(buffer[(3, response_y)].symbol(), "H");
            assert_eq!(buffer[(1, response_y)].bg, Color::Reset);
            assert_eq!(buffer[(1, response_y)].fg, buffer[(0, editor_y)].bg);
            assert_eq!(buffer[(3, prompt_y)].symbol(), "h");
            assert_eq!(buffer[(3, prompt_y + 1)].symbol(), "s");
            for y in prompt_y..=prompt_y + 1 {
                assert_eq!(buffer[(0, y)].symbol(), " ");
                assert_eq!(buffer[(2, y)].symbol(), " ");
                assert_eq!(buffer[(width - 1, y)].symbol(), " ");
            }
            assert_eq!(buffer[(1, prompt_y + 1)].symbol(), " ");
            if let Some(dir) = std::env::var_os("ASURA_TUI_PREVIEW_DIR") {
                let cells: Vec<_> = buffer.content.iter().map(|c| serde_json::json!({"symbol":c.symbol(),"fg":format!("{:?}",c.fg),"bg":format!("{:?}",c.bg)})).collect();
                std::fs::create_dir_all(&dir).unwrap();
                std::fs::write(
                    std::path::PathBuf::from(dir).join(format!("history-{width}.json")),
                    serde_json::to_vec(
                        &serde_json::json!({"width":width,"height":height,"cells":cells}),
                    )
                    .unwrap(),
                )
                .unwrap();
            }
            app.transcript = vec![("wrapped words ".repeat(150), "Done".into())];
            terminal.draw(|frame| app.draw(frame)).unwrap();
            let buffer = terminal.backend().buffer();
            // The viewport starts partway through the old input. It retains the
            // full band and gutter without inventing a second prompt marker.
            assert_eq!(buffer[(0, 2)].bg, Color::Rgb(38, 51, 65));
            assert_eq!(buffer[(width - 1, 2)].bg, Color::Rgb(38, 51, 65));
            assert_eq!(buffer[(1, 2)].symbol(), " ");
            assert_eq!(buffer[(2, 2)].symbol(), " ");
        }
    }

    #[test]
    fn conversation_markdown_event_renders_bold_cells_and_keeps_literal_input() {
        use ratatui::style::Modifier;
        let mut app = App::new();
        let operation = vec![9; 16];
        app.append_response("**literal input**".into(), Some(operation.clone()));
        app.active_operation = Some(operation.clone());
        app.insert("saved draft");
        app.conversation_finished(super::super::conversation::Update {
            event: Some(asura_control::pb::ConversationEvent {
                operation_id: Some(operation.clone()),
                generation: Some(1),
                cursor: Some(1),
                kind: Some(3),
                text: Some("Hi **bold** and `code`.".into()),
                ..Default::default()
            }),
            done: true,
            success: true,
            ..Default::default()
        });
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
        terminal.draw(|frame| app.draw(frame)).unwrap();
        let buffer = terminal.backend().buffer();
        let input_y = 3;
        let response_y = 6;
        assert_eq!(buffer[(3, input_y)].symbol(), "*");
        assert_eq!(buffer[(6, response_y)].symbol(), "b");
        assert!(buffer[(6, response_y)].modifier.contains(Modifier::BOLD));
        assert_ne!(buffer[(6, response_y)].symbol(), "*");

        key(&mut app, KeyCode::F(6), KeyModifiers::NONE);
        assert_eq!(app.response_selection, Some(0));
        assert_eq!(app.draft(), "saved draft");
        app.conversation_finished(super::super::conversation::Update {
            event: Some(asura_control::pb::ConversationEvent {
                operation_id: Some(operation),
                generation: Some(1),
                cursor: Some(2),
                kind: Some(3),
                text: Some("Now **updated**.".into()),
                ..Default::default()
            }),
            ..Default::default()
        });
        assert!(
            app.rendered_responses[0].lines[0]
                .spans
                .iter()
                .any(|span| span.content == "updated"
                    && span.style.add_modifier.contains(Modifier::BOLD))
        );
    }

    #[test]
    fn config_panel_has_padding_bottom_alignment_and_distinct_background() {
        let mut app = App::new();
        app.config_finished(Ok(
            "audit:\n  enabled: true\n  keepFiles: 5\n  maxFileBytes: 10485760".into(),
        ));
        for (width, height) in [(80, 24), (50, 14)] {
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal.draw(|frame| app.draw(frame)).unwrap();
            let buffer = terminal.backend().buffer();
            let panel = Color::Rgb(55, 66, 76);
            let input = Color::Rgb(37, 59, 78);
            let strips = u16::from(width >= 60 && height >= 16);
            let bottom = height - 3 - 2 * strips;
            assert_eq!(buffer[(0, 0)].bg, panel);
            assert_eq!(buffer[(0, height - 3 - strips)].bg, input);
            assert_eq!(buffer[(0, bottom - 1)].bg, panel);
            for x in 0..width {
                assert_eq!(buffer[(x, bottom - 1)].symbol(), " ");
            }
            for y in 2..bottom {
                if buffer[(0, y)].bg == panel {
                    assert_eq!(buffer[(0, y)].symbol(), " ");
                    assert_eq!(buffer[(width - 1, y)].symbol(), " ");
                    assert_eq!(buffer[(width - 1, y)].bg, panel);
                }
            }
            let text: String = buffer.content.iter().map(|cell| cell.symbol()).collect();
            assert_eq!(text.matches("audit:").count(), 1);
            if let Some(dir) = std::env::var_os("ASURA_TUI_PREVIEW_DIR") {
                let cells: Vec<_> = buffer.content.iter().map(|c| serde_json::json!({"symbol":c.symbol(),"fg":format!("{:?}",c.fg),"bg":format!("{:?}",c.bg)})).collect();
                std::fs::create_dir_all(&dir).unwrap();
                std::fs::write(
                    std::path::PathBuf::from(dir).join(format!("config-{width}.json")),
                    serde_json::to_vec(
                        &serde_json::json!({"width":width,"height":height,"cells":cells}),
                    )
                    .unwrap(),
                )
                .unwrap();
            }
        }
    }
    #[test]
    fn incompatible_backend_explains_recovery_and_preserves_draft() {
        let mut app = App::new();
        app.handle(Event::Paste("draft".into()));
        app.set_view(View {
            configured_model: None,
            service: None,
            connection: "unavailable",
            lifecycle: "unavailable",
            installation: "unavailable",
            reason: "incompatible_protocol",
            epoch: None,
        });
        let display = render(&mut app, 90, 24);
        assert!(display.contains("Existing backend incompatible"));
        assert!(display.contains("restart it with the current build"));
        assert!(!display.contains("see ~/.asura/logs"));
        assert_eq!(app.draft(), "draft");
    }
    #[test]
    fn service_header_uses_observed_ownership_uptime_memory_and_graph() {
        let mut app = App::new();
        app.view.connection = "connected";
        app.view.installation = "graph_ready";
        app.view.reason = "graph_verified";
        app.view.service = Some(ServiceDetails {
            owned: true,
            uptime_ms: 3_661_000,
            stored_memory: asura_control::pb::StoredMemoryStatus {
                available: Some(true),
                size_bytes: Some(1_048_576),
                sampled_uptime_ms: Some(3_660_000),
                stale: Some(false),
                size_reason: Some(1),
            },
        });
        assert_eq!(
            app.status_header(),
            "Asura · Connected (process) · uptime 1h 1m · memory: ready 1.0 MiB · graph: ready verified"
        );
        app.view.service.as_mut().unwrap().owned = false;
        app.view.service.as_mut().unwrap().stored_memory.stale = Some(true);
        assert!(app.status_header().contains("Connected (local)"));
        assert!(app.status_header().contains("1.0 MiB stale"));
        app.view.service = None;
        app.view.connection = "unavailable";
        app.view.installation = "unavailable";
        app.view.reason = "status_expired";
        let header = app.status_header();
        assert!(header.contains("Disconnected · uptime ? · memory: unknown ?"));
        assert!(!header.contains("ready verified"));
        for (w, h) in [(120, 24), (40, 12), (2, 2), (0, 0)] {
            render(&mut app, w, h);
        }
    }

    #[test]
    fn unicode_draft_survives_unavailable_submission_resize_and_status_change() {
        let mut app = App::new();
        app.handle(Event::Paste("界e\u{301}👩‍💻 draft".into()));
        let expected = app.draft();
        key(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert!(render(&mut app, 90, 24).contains("No project selected."));
        app.set_view(View {
            configured_model: None,
            service: None,
            connection: "connected",
            lifecycle: "serving",
            installation: "uninitialized",
            reason: "authority_not_installed",
            epoch: Some("abc\x1b[2J".into()),
        });
        let display = render(&mut app, 90, 24);
        for label in [
            "Connected (local)",
            "Lifecycle serving",
            "Installation uninitialized",
            "No project",
            "?",
        ] {
            assert!(display.contains(label), "{label}");
        }
        assert!(!display.contains('\x1b'));
        if let Some(directory) = std::env::var_os("ASURA_TUI_PREVIEW_DIR") {
            let directory = std::path::PathBuf::from(directory);
            std::fs::create_dir_all(&directory).unwrap();
            for (width, height) in [(80, 24), (50, 14), (15, 5)] {
                let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
                terminal.draw(|frame| app.draw(frame)).unwrap();
                let cells: Vec<_> = terminal
                    .backend()
                    .buffer()
                    .content
                    .iter()
                    .map(|cell| {
                        serde_json::json!({
                            "symbol": cell.symbol(),
                            "fg": format!("{:?}", cell.fg),
                            "bg": format!("{:?}", cell.bg),
                        })
                    })
                    .collect();
                let preview = serde_json::json!({"width": width, "height": height, "cells": cells});
                std::fs::write(
                    directory.join(format!("tui-{width}x{height}.json")),
                    serde_json::to_vec_pretty(&preview).unwrap(),
                )
                .unwrap();
            }
        }
        render(&mut app, 15, 5);
        assert_eq!(app.draft(), expected);
        key(&mut app, KeyCode::Char('z'), KeyModifiers::CONTROL);
        assert_eq!(app.draft(), "");
    }
    #[test]
    fn status_is_outside_input_with_separate_unknown_groups() {
        for (width, height) in [(120, 40), (80, 24), (60, 16), (40, 12), (30, 8)] {
            let mut app = App::new();
            app.handle(Event::Paste("x".repeat(MAX_DRAFT_BYTES)));
            let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
            terminal.draw(|frame| app.draw(frame)).unwrap();
            let buffer = terminal.backend().buffer();
            let row: String = (0..width)
                .map(|x| buffer[(x, height - 2)].symbol())
                .collect();
            assert!(row.contains("0% · ?"), "{row}");
            assert!(row.contains("65536"), "{row}");
            assert!(!row.contains("Ctrl") && !row.contains("F1"));
            assert_eq!(buffer[(1, height - 2)].fg, Color::Rgb(143, 211, 244));
            for x in 0..width {
                assert_eq!(buffer[(x, height - 2)].bg, Color::Reset);
            }
            let (project, tail, model) = status_groups(
                MAX_DRAFT_BYTES,
                width,
                "No project",
                "?",
                None,
                Some(&super::super::context::Report::unknown()),
                None,
            );
            assert!(
                unicode_display_width::width(&format!(
                    "{project}{}  {model}",
                    tail.iter()
                        .map(|span| span.content.as_ref())
                        .collect::<String>()
                )) <= u64::from(width - 2)
            );
            if width >= 80 {
                assert!(row.contains("path ? · ? ? +?-?"));
                assert_eq!(buffer[(1, height - 3)].symbol(), "▀");
            }
        }
    }

    #[test]
    fn model_context_percentage_uses_measured_input_and_never_output_usage() {
        let measured = asura_control::pb::ModelContext {
            model_name: Some("Native model".into()),
            input_tokens: Some(1024),
            capacity_tokens: Some(4096),
            basis: Some(1),
        };
        let (_, _, model) = status_groups(
            0,
            200,
            "Project p",
            "Native model",
            None,
            None,
            Some(&measured),
        );
        assert_eq!(model, "25% input · Native model");
        for invalid in [
            asura_control::pb::ModelContext {
                capacity_tokens: Some(0),
                ..measured.clone()
            },
            asura_control::pb::ModelContext {
                basis: Some(2),
                ..measured.clone()
            },
            asura_control::pb::ModelContext {
                input_tokens: Some(5000),
                ..measured.clone()
            },
        ] {
            let (_, _, model) = status_groups(
                0,
                200,
                "Project p",
                "Native model",
                None,
                None,
                Some(&invalid),
            );
            assert_eq!(model, "0% · Native model");
        }
        let mut app = App::new();
        app.model_context = Some(measured);
        app.observed_model = Some("Native model".into());
        app.set_view(View {
            configured_model: None,
            service: None,
            connection: "connected",
            lifecycle: "serving",
            installation: "graph_ready",
            reason: "",
            epoch: Some("new epoch".into()),
        });
        assert!(app.model_context.is_none() && app.observed_model.is_none());
    }

    #[test]
    fn git_totals_use_experiment_colours_without_colouring_branch_or_path() {
        let report = super::super::context::Report {
            branch: "feature+12-3".into(),
            files: Some(2),
            added: Some(12),
            deleted: Some(3),
        };
        let (project, spans, model) = status_groups(
            0,
            200,
            "Project sample",
            "system",
            Some("/project/+12-3"),
            Some(&report),
            None,
        );
        let text = spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(text.ends_with("feature+12-3 2 +12-3"), "{text}");
        assert!(!text.contains("git ") && !text.contains("dirty") && !text.contains("clean"));
        let mut terminal = Terminal::new(TestBackend::new(200, 1)).unwrap();
        terminal
            .draw(|frame| {
                frame.render_widget(Paragraph::new(Line::from(spans.clone())), frame.area())
            })
            .unwrap();
        let buffer = terminal.backend().buffer();
        let added_start = unicode_display_width::width(&text[..text.rfind("+12").unwrap()]) as u16;
        for x in added_start..added_start + 3 {
            assert_eq!(buffer[(x, 0)].fg, Color::Rgb(134, 239, 172));
        }
        for x in added_start + 3..added_start + 5 {
            assert_eq!(buffer[(x, 0)].fg, Color::Rgb(252, 165, 165));
        }
        for x in 0..added_start {
            assert_eq!(buffer[(x, 0)].fg, Color::Reset);
        }
        assert!(unicode_display_width::width(&format!("{project}{text}  {model}")) <= 198);
        for width in [40, 60, 80, 120] {
            let (project, spans, model) = status_groups(
                0,
                width,
                "Project sample",
                "system",
                Some("/project/+12-3"),
                Some(&report),
                None,
            );
            let text = spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>();
            assert!(
                unicode_display_width::width(&format!("{project}{text}  {model}"))
                    <= u64::from(width - 2)
            );
            assert_eq!(
                spans.iter().filter(|span| span.style.fg.is_some()).count(),
                if text.contains("feature+12-3") { 2 } else { 0 }
            );
        }
    }

    #[test]
    fn paste_rejection_remains_visible_above_input() {
        let mut app = App::new();
        app.handle(Event::Paste("old".into()));
        key(&mut app, KeyCode::Char('v'), KeyModifiers::CONTROL);
        app.handle(Event::Paste("x".repeat(MAX_DRAFT_BYTES)));
        key(&mut app, KeyCode::Char('v'), KeyModifiers::CONTROL);
        key(&mut app, KeyCode::Tab, KeyModifiers::NONE);
        key(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert!(!app.notice.is_empty());
        assert!(render(&mut app, 120, 24).contains(&app.notice));
        assert_eq!(app.draft(), "old");
    }

    #[test]
    fn cancel_requires_a_live_conversation_and_retry_rejects_arguments() {
        use super::super::conversation::Request;
        use crate::conversation::Command;
        for request in [
            None,
            Some(Request::DiscoverProject),
            Some(Request::Setup(Command::List)),
        ] {
            let mut app = App::new();
            if let Some(request) = request {
                app.start_conversation_request(request, String::new());
            }
            app.insert("/cancel");
            key(&mut app, KeyCode::Enter, KeyModifiers::NONE);
            assert_eq!(app.draft(), "/cancel");
            assert!(!app.take_cancel_request());
            assert_eq!(app.notice, "No active conversation to cancel");
        }
        for request in [Request::Setup(Command::Observe([1; 16]))] {
            let mut app = App::new();
            app.start_conversation_request(request, String::new());
            app.insert("/cancel");
            key(&mut app, KeyCode::Enter, KeyModifiers::NONE);
            assert!(app.take_cancel_request());
            assert!(app.draft().is_empty());
            app.conversation_finished(super::super::conversation::Update {
                done: true,
                ..Default::default()
            });
            app.insert("/cancel");
            key(&mut app, KeyCode::Enter, KeyModifiers::NONE);
            assert!(!app.take_cancel_request());
            assert_eq!(app.draft(), "/cancel");
        }
        let mut app = App::new();
        app.retained_request = Some(Request::Setup(Command::Initialize([7; 16])));
        app.insert("/retry extra");
        key(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(app.notice, "Use /retry");
        assert_eq!(app.draft(), "/retry extra");
        assert!(app.conversation_request.is_none());
        assert!(
            matches!(app.retained_request, Some(Request::Setup(Command::Initialize(id))) if id == [7; 16])
        );
    }

    #[test]
    fn tab_completion_is_contextual_and_never_invokes() {
        for (prefix, full) in [("/he", "/help"), ("/qui", "/quit"), ("/ex", "/exit")] {
            let mut app = App::new();
            app.handle(Event::Paste(prefix.into()));
            assert!(!key(&mut app, KeyCode::Tab, KeyModifiers::NONE));
            assert_eq!(app.draft(), full);
            assert!(app.overlay.is_none());
            key(&mut app, KeyCode::Tab, KeyModifiers::NONE);
            key(&mut app, KeyCode::Char('z'), KeyModifiers::CONTROL);
            assert_eq!(app.draft(), prefix);
        }
        let mut app = App::new();
        app.handle(Event::Paste("/".into()));
        key(&mut app, KeyCode::Tab, KeyModifiers::NONE);
        assert!(render(&mut app, 80, 24).contains("Local commands"));
        key(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(app.draft(), "/");
        key(&mut app, KeyCode::Tab, KeyModifiers::NONE);
        assert_eq!(app.draft(), "/");
        assert!(!key(&mut app, KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(app.draft(), "/help");
        assert!(app.overlay.is_none());
        key(&mut app, KeyCode::Char('z'), KeyModifiers::CONTROL);
        key(&mut app, KeyCode::Tab, KeyModifiers::NONE);
        key(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        assert_eq!(app.draft(), "/");
        for text in ["plain", "/help args", "/help\nnext", " /he"] {
            let mut app = App::new();
            app.handle(Event::Paste(text.into()));
            key(&mut app, KeyCode::Tab, KeyModifiers::NONE);
            assert_eq!(app.draft(), format!("{text}  "));
        }
        let mut app = App::new();
        app.handle(Event::Paste("/xyz".into()));
        key(&mut app, KeyCode::Tab, KeyModifiers::NONE);
        assert_eq!(app.draft(), "/xyz");
        assert_eq!(app.notice, "No matching local command");
        key(&mut app, KeyCode::Char('a'), KeyModifiers::CONTROL);
        key(&mut app, KeyCode::Tab, KeyModifiers::NONE);
        assert_eq!(app.draft(), "  ");
    }

    #[test]
    fn queue_and_quit_prefix_requires_explicit_completion_choice() {
        let mut app = App::new();
        app.handle(Event::Paste("/qu".into()));
        assert!(!key(&mut app, KeyCode::Tab, KeyModifiers::NONE));
        assert_eq!(app.draft(), "/qu");
        let screen = render(&mut app, 100, 24);
        assert!(screen.contains("/queue"));
        assert!(screen.contains("/quit"));
        assert!(!key(&mut app, KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(app.draft(), "/qu");
        assert!(app.queue_request.is_none());
    }

    #[test]
    fn config_submission_and_results_preserve_newer_drafts() {
        let mut app = App::new();
        app.handle(Event::Paste("/con".into()));
        key(&mut app, KeyCode::Tab, KeyModifiers::NONE);
        assert_eq!(app.draft(), "/config");
        app.handle(Event::Paste(" set model ollama:granite4.1:8b".into()));
        assert!(!key(&mut app, KeyCode::Enter, KeyModifiers::NONE));
        let request = app.take_config_request().unwrap();
        assert_eq!(request.value.as_deref(), Some("ollama:granite4.1:8b"));
        key(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert!(app.take_config_request().is_none());
        app.handle(Event::Paste("-edited".into()));
        let edited = app.draft();
        app.config_finished(Ok("Saved model\nollama:granite4.1:8b".into()));
        assert_eq!(app.draft(), edited);
        assert!(render(&mut app, 80, 24).contains("Saved model"));
        key(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        key(&mut app, KeyCode::Char('a'), KeyModifiers::CONTROL);
        app.handle(Event::Paste("/config get model".into()));
        key(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        app.take_config_request().unwrap();
        app.config_finished(Err("bad\x1b[2J".into()));
        assert_eq!(app.draft(), "/config get model");
        assert!(!render(&mut app, 80, 24).contains('\x1b'));
        key(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        key(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        app.take_config_request().unwrap();
        app.config_finished(Ok("model\nollama:granite4.1:8b".into()));
        assert!(app.draft().is_empty());
    }

    #[test]
    fn config_result_waits_for_help_to_close() {
        let mut app = App::new();
        app.handle(Event::Paste("/config get audit".into()));
        key(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        app.take_config_request().unwrap();
        key(&mut app, KeyCode::F(1), KeyModifiers::NONE);
        app.config_finished(Ok("audit\nenabled: true\nkeepFiles: 5".into()));
        assert!(render(&mut app, 80, 24).contains("Editor help"));
        key(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        assert!(render(&mut app, 80, 24).contains("keepFiles: 5"));
    }

    #[test]
    fn local_commands_consume_only_valid_commands() {
        for command in ["/quit", "/exit", "/quit \n"] {
            let mut app = App::new();
            assert!(!app.handle(Event::Paste(command.into())));
            assert!(key(&mut app, KeyCode::Enter, KeyModifiers::NONE));
        }
        let mut app = App::new();
        app.handle(Event::Paste("/help  ".into()));
        assert!(!key(&mut app, KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(app.overlay, Some(Overlay::Help(0))));
        assert!(app.draft().is_empty());
        key(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        key(&mut app, KeyCode::Char('z'), KeyModifiers::CONTROL);
        assert!(app.draft().is_empty());
        for command in [
            "/exit now",
            "/help extra",
            "/quit\nprose",
            "/unknown",
            "/HELP",
            "/",
            " /quit",
        ] {
            let mut app = App::new();
            app.handle(Event::Paste(command.into()));
            assert!(!key(&mut app, KeyCode::Enter, KeyModifiers::NONE));
            assert_eq!(app.draft(), command);
            assert!(app.overlay.is_none());
            key(&mut app, KeyCode::Char('z'), KeyModifiers::CONTROL);
            assert!(app.draft().is_empty());
        }
    }

    #[test]
    fn dirty_exit_defaults_to_keep_and_repeat_actions_are_inert() {
        let mut app = App::new();
        app.handle(Event::Paste("draft".into()));
        assert!(!key(&mut app, KeyCode::Char('q'), KeyModifiers::CONTROL));
        assert!(render(&mut app, 80, 24).contains("[Keep editing]"));
        assert!(!key(&mut app, KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(app.draft(), "draft");
        for code in [KeyCode::Enter, KeyCode::Char('q'), KeyCode::F(1)] {
            app.handle(Event::Key(KeyEvent::new_with_kind(
                code,
                KeyModifiers::CONTROL,
                KeyEventKind::Repeat,
            )));
        }
        assert!(app.overlay.is_none());
        key(&mut app, KeyCode::Char('q'), KeyModifiers::CONTROL);
        key(&mut app, KeyCode::Tab, KeyModifiers::NONE);
        assert!(key(&mut app, KeyCode::Enter, KeyModifiers::NONE));
    }
    #[test]
    fn queue_subscription_preserves_explicit_command_and_fences_project_change() {
        let mut app = App::new();
        app.view.connection = "connected";
        app.view.installation = "graph_ready";
        app.view.epoch = Some("epoch".into());
        app.selected_project = Some([1; 16]);
        app.restoration_ready = true;
        let now = std::time::Instant::now();
        app.refresh_queue_scope(now);
        app.editor.insert("/queue").unwrap();
        app.queue_busy = true;
        app.queue_editor = Some("/queue".into());
        let update = || super::super::queue_watch::Update {
            scope: super::super::queue_watch::Scope {
                project: [1; 16],
                epoch: "epoch".into(),
            },
            source: 1,
            received: now,
            result: Ok(asura_control::pb::ConversationQueueReply {
                entries: vec![asura_control::pb::ConversationQueueEntry {
                    project_id: Some(vec![1; 16]),
                    input_id: Some(vec![2; 16]),
                    ..Default::default()
                }],
                revision: Some(1),
                pending: Some(false),
                ..Default::default()
            }),
        };
        assert!(app.queue_observed(update()));
        assert!(app.queue_busy);
        assert_eq!(app.queue_editor.as_deref(), Some("/queue"));
        assert_eq!(app.draft(), "/queue");
        app.selected_project = Some([3; 16]);
        app.queue_observed(update());
        assert!(app.queue_entries.is_empty());
        assert!(app.take_queue_request(now).is_none());
    }
    #[test]
    fn configured_model_is_fallback_and_actual_variant_clears_on_selector_change() {
        let mut app = App::new();
        let mut view = app.view.clone();
        view.configured_model = Some("system".into());
        app.set_view(view.clone());
        app.observed_model = Some("Native variant".into());
        app.set_view(view.clone());
        assert_eq!(app.observed_model.as_deref(), Some("Native variant"));
        view.configured_model = Some("ollama:example".into());
        app.set_view(view);
        assert!(app.observed_model.is_none());
        assert_eq!(app.view.configured_model.as_deref(), Some("ollama:example"));
    }
    #[test]
    fn rejected_transport_paste_preserves_draft_and_invalidates_capture() {
        let mut app = App::new();
        app.handle(Event::Paste("old".into()));
        app.reject_paste();
        assert_eq!(app.draft(), "old");
        assert!(app.notice.contains("64 KiB"));
        assert!(!key(&mut app, KeyCode::Char('x'), KeyModifiers::NONE));
        assert_eq!(app.draft(), "oldx");
        key(&mut app, KeyCode::Char('v'), KeyModifiers::CONTROL);
        app.reject_paste();
        assert!(app.capture.as_ref().unwrap().invalid);
        assert_eq!(app.draft(), "oldx");
    }
    #[test]
    fn capture_is_atomic_and_requires_explicit_confirmation() {
        let mut app = App::new();
        app.handle(Event::Paste("old".into()));
        key(&mut app, KeyCode::Char('v'), KeyModifiers::CONTROL);
        app.handle(Event::Paste("界\r\nnew".into()));
        key(&mut app, KeyCode::Char('j'), KeyModifiers::CONTROL);
        key(&mut app, KeyCode::Char('v'), KeyModifiers::CONTROL);
        key(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(app.draft(), "old");
        key(&mut app, KeyCode::Tab, KeyModifiers::NONE);
        key(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert_eq!(app.draft(), "old界\nnew\n");
        key(&mut app, KeyCode::Char('z'), KeyModifiers::CONTROL);
        assert_eq!(app.draft(), "old");
        app.handle(Event::Paste("x".repeat(MAX_DRAFT_BYTES)));
        assert_eq!(app.draft(), "old");
    }
    #[test]
    fn initialization_result_is_visible_and_clears_only_the_submitted_draft() {
        let mut app = App::new();
        app.insert("/init");
        key(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert!(matches!(
            app.take_conversation_request(),
            Some(super::super::conversation::Request::Setup(
                crate::conversation::Command::Initialize(_)
            ))
        ));
        assert!(render(&mut app, 100, 24).contains("Request pending"));
        app.conversation_finished(super::super::conversation::Update {
            message: Some(
                "Installation already initialized.\nUse /project list or /project add PATH.".into(),
            ),
            success: true,
            done: true,
            ..Default::default()
        });
        let screen = render(&mut app, 100, 24);
        assert!(screen.contains("Setup"));
        assert!(screen.contains("Installation already initialized."));
        assert!(screen.contains("/project list"));
        assert_eq!(app.draft(), "");
        assert!(!app.conversation_busy);
        assert!(app.selected_project.is_none());
    }
    #[test]
    fn conversation_requires_selection_and_preserves_draft_until_acceptance() {
        let mut app = App::new();
        app.insert("Hello");
        app.submit();
        assert!(app.take_conversation_request().is_none());
        assert_eq!(app.draft(), "Hello");
        app.view.connection = "connected";
        app.view.installation = "graph_ready";
        app.view.epoch = Some("epoch".into());
        app.selected_project = Some([3; 16]);
        app.restoration_ready = true;
        app.submit();
        let Some(request) = app.take_queue_request(std::time::Instant::now()) else {
            panic!("queue submit missing")
        };
        let super::super::queue::Request::Submit(sent) = &request else {
            panic!("queue submit expected")
        };
        assert_eq!(sent.prompt.as_deref(), Some("Hello"));
        assert_eq!(app.draft(), "");
        assert_eq!(app.local_outbox[0].text, "Hello");
        acknowledge_local_input(&mut app, request, [5; 16]);
        assert!(app.local_outbox.is_empty());
        let mut running = app.queue_entries[0].clone();
        running.state = Some(3);
        running.operation_id = Some(vec![4; 16]);
        running.generation = Some(1);
        app.queue_entries = vec![running];
        app.observe_queued();
        assert_eq!(app.transcript[0].0, "Hello");
        assert_eq!(app.conversation_generation, 1);
        let screen = render(&mut app, 100, 24);
        assert!(screen.contains("?"));
        assert!(screen.contains("03030303"));
        assert!(!screen.contains("No project"));
        app.conversation_finished(super::super::conversation::Update {
            event: Some(asura_control::pb::ConversationEvent {
                operation_id: Some(vec![4; 16]),
                cursor: Some(u64::MAX),
                generation: Some(1),
                kind: Some(6),
                text: Some("Partial".into()),
                reason: Some(11),
                usage_tokens: None,
                usage_known: Some(false),
                tools: Vec::new(),
                model_context: Some(asura_control::pb::ModelContext {
                    model_name: Some("Native variant".into()),
                    ..Default::default()
                }),
            }),
            done: true,
            ..Default::default()
        });
        assert!(app.transcript[0].1.contains("[Incomplete: interrupted]"));
        assert!(!app.conversation_busy);
        assert!(render(&mut app, 100, 24).contains("Native variant"));
    }
    #[test]
    fn context_path_is_launch_subdirectory_and_changes_with_project_scope() {
        let mut app = App::new();
        app.launch_directory = Some("/work/asura/src/ui".into());
        app.projects = Some(vec![
            asura_control::pb::ProjectReply {
                project_id: Some(vec![1; 16]),
                location: Some("/work/asura".into()),
                current: Some(true),
                ..Default::default()
            },
            asura_control::pb::ProjectReply {
                project_id: Some(vec![2; 16]),
                location: Some("/work/wisp".into()),
                current: Some(true),
                ..Default::default()
            },
        ]);
        app.selected_project = Some([1; 16]);
        assert_eq!(app.context_directory(), Some("/work/asura/src/ui"));
        assert!(render(&mut app, 120, 24).contains("/work/asura/src/ui"));
        app.selected_project = Some([2; 16]);
        assert_eq!(app.context_directory(), Some("/work/wisp"));
        app.selected_project = Some([1; 16]);
        assert_eq!(app.context_directory(), Some("/work/asura/src/ui"));
        assert_eq!(
            shorten_path("/Users/name/src/github.com/pidster/asura", 200),
            "…/src/github.com/pidster/asura"
        );
        assert_eq!(
            shorten_path("/one/two/three/four", 200),
            "/one/two/three/four"
        );
        assert_eq!(shorten_path("/", 200), "/");
        for budget in 0..20 {
            let path = shorten_path("/work/日本語/🙂/src", budget);
            assert!(unicode_display_width::width(&path) as usize <= budget);
            if budget >= 5 {
                assert!(path.ends_with("/src"));
            }
        }
    }

    #[test]
    fn registered_projects_are_visible_selected_and_refreshed_on_new_epoch() {
        use super::super::conversation::Update;
        use asura_control::pb::ProjectReply;
        let project = |id: u8, path: &str| ProjectReply {
            project_id: Some(vec![id; 16]),
            location: Some(path.into()),
            current: Some(true),
            ..Default::default()
        };
        let mut app = project_discovery_app();
        app.conversation_finished(Update {
            preserve_project_selection: true,
            projects: Some(vec![project(3, "/work/asura"), project(4, "/work/wisp")]),
            project: Some([3; 16]),
            success: true,
            done: true,
            ..Default::default()
        });
        assert_eq!(app.selected_project, Some([3; 16]));
        let screen = render(&mut app, 100, 24);
        assert!(screen.contains("Asura · /work/asura"), "{screen}");
        app.focus = Focus::Project;
        key(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        let picker_screen = render(&mut app, 100, 24);
        assert!(picker_screen.contains("Asura") && picker_screen.contains("Wisp"));
        key(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        assert!(!screen.contains("No projects"));
        assert_eq!(app.draft(), "draft before discovery");
        app.selected_project = Some([4; 16]);
        app.set_view(View {
            configured_model: None,
            service: None,
            connection: "connected",
            lifecycle: "serving",
            installation: "graph_ready",
            reason: "graph_verified",
            epoch: Some("new-epoch".into()),
        });
        assert!(matches!(
            app.take_conversation_request(),
            Some(super::super::conversation::Request::DiscoverProject)
        ));
        app.conversation_finished(Update {
            preserve_project_selection: true,
            projects: Some(vec![project(3, "/work/asura"), project(4, "/work/wisp")]),
            project: Some([3; 16]),
            success: true,
            done: true,
            ..Default::default()
        });
        assert_eq!(app.selected_project, Some([4; 16]));
        assert!(render(&mut app, 100, 24).contains("Wisp · /work/wisp"));
        assert_eq!(app.draft(), "draft before discovery");
        app.conversation_finished(Update {
            preserve_project_selection: true,
            projects: Some(vec![project(3, "/work/asura")]),
            project: Some([3; 16]),
            done: true,
            success: true,
            ..Default::default()
        });
        assert_eq!(app.selected_project, Some([3; 16]));
    }

    fn project_discovery_app() -> App {
        let mut app = App::new();
        app.set_view(View {
            configured_model: None,
            service: None,
            connection: "connected",
            lifecycle: "serving",
            installation: "graph_ready",
            reason: "graph_verified",
            epoch: None,
        });
        app.insert("draft before discovery");
        assert!(matches!(
            app.take_conversation_request(),
            Some(super::super::conversation::Request::DiscoverProject)
        ));
        app
    }
    fn offer_project(app: &mut App) {
        app.conversation_finished(super::super::conversation::Update {
            project_offer: Some("/private/tmp/current-project".into()),
            success: true,
            done: true,
            ..Default::default()
        });
    }
    #[test]
    fn first_project_offer_waits_for_overlay_and_preserves_draft_on_registration() {
        let mut app = project_discovery_app();
        app.insert(" and while discovery runs");
        let draft = app.draft();
        app.overlay = Some(Overlay::Help(0));
        offer_project(&mut app);
        assert!(render(&mut app, 100, 24).contains("Editor help"));
        assert!(!matches!(app.overlay, Some(Overlay::ProjectOffer(..))));
        key(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        let screen = render(&mut app, 100, 24);
        assert!(screen.contains("Use this directory as a project?"));
        assert!(screen.contains("/private/tmp/current-project"));
        assert!(screen.contains("[Yes]    No"));
        assert_eq!(app.draft(), draft);
        key(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        assert!(
            matches!(app.take_conversation_request(),Some(super::super::conversation::Request::Setup(crate::conversation::Command::Register(_,path))) if path=="/private/tmp/current-project")
        );
        assert!(app.selected_project.is_none());
        assert_eq!(app.draft(), draft);
        app.conversation_finished(super::super::conversation::Update {
            project: Some([3; 16]),
            message: Some("Project registered".into()),
            success: true,
            done: true,
            ..Default::default()
        });
        assert_eq!(app.selected_project, Some([3; 16]));
        assert_eq!(app.draft(), draft);
        assert!(app.take_conversation_request().is_none());
    }
    #[test]
    fn first_project_no_and_escape_do_not_write_or_repeat() {
        for escape in [false, true] {
            let mut app = project_discovery_app();
            let draft = app.draft();
            offer_project(&mut app);
            render(&mut app, 100, 24);
            if escape {
                key(&mut app, KeyCode::Esc, KeyModifiers::NONE);
            } else {
                key(&mut app, KeyCode::Tab, KeyModifiers::NONE);
                assert!(render(&mut app, 100, 24).contains("[No]"));
                key(&mut app, KeyCode::Enter, KeyModifiers::NONE);
            }
            assert_eq!(app.draft(), draft);
            assert!(app.selected_project.is_none());
            assert!(app.take_conversation_request().is_none());
            assert!(!render(&mut app, 100, 24).contains("Use this directory as a project?"));
        }
    }
    #[test]
    fn existing_projects_and_discovery_failure_never_offer_registration() {
        for failed in [false, true] {
            let mut app = project_discovery_app();
            let draft = app.draft();
            app.conversation_finished(super::super::conversation::Update {
                message: failed.then(|| "Project discovery failed".into()),
                success: !failed,
                done: true,
                ..Default::default()
            });
            let screen = render(&mut app, 100, 24);
            assert!(!screen.contains("Use this directory as a project?"));
            if failed {
                assert!(screen.contains("Project discovery failed"));
            }
            assert_eq!(app.draft(), draft);
            assert!(app.take_conversation_request().is_none());
        }
    }
    #[test]
    fn failed_first_project_registration_retains_draft_without_retry() {
        let mut app = project_discovery_app();
        let draft = app.draft();
        offer_project(&mut app);
        render(&mut app, 100, 24);
        key(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        app.take_conversation_request().unwrap();
        app.conversation_finished(super::super::conversation::Update {
            message: Some("project_stale".into()),
            done: true,
            ..Default::default()
        });
        assert!(render(&mut app, 100, 24).contains("project_stale"));
        assert_eq!(app.draft(), draft);
        assert!(app.selected_project.is_none());
        assert!(app.take_conversation_request().is_none());
    }
    #[test]
    fn retry_of_offered_registration_keeps_identity_and_preserves_current_draft() {
        let mut app = project_discovery_app();
        offer_project(&mut app);
        render(&mut app, 100, 24);
        key(&mut app, KeyCode::Enter, KeyModifiers::NONE);
        let Some(super::super::conversation::Request::Setup(
            crate::conversation::Command::Register(original_id, _),
        )) = app.take_conversation_request()
        else {
            panic!("registration missing")
        };
        app.conversation_finished(super::super::conversation::Update {
            message: Some("outcome_unconfirmed".into()),
            done: true,
            ..Default::default()
        });
        assert_eq!(app.draft(), "draft before discovery");
        app.editor = Editor::new();
        app.insert("/retry");
        app.submit();
        assert!(
            matches!(app.take_conversation_request(),Some(super::super::conversation::Request::Setup(crate::conversation::Command::Register(id,_))) if id==original_id)
        );
        assert!(app.retained_preserve_draft);
        app.conversation_finished(super::super::conversation::Update {
            project: Some([3; 16]),
            message: Some("Project registered".into()),
            success: true,
            done: true,
            ..Default::default()
        });
        assert_eq!(app.draft(), "/retry");
        assert_eq!(app.selected_project, Some([3; 16]));
    }
    fn working_queue_app() -> App {
        let mut app = App::new();
        app.view.connection = "connected";
        app.view.installation = "graph_ready";
        app.view.epoch = Some("epoch".into());
        app.selected_project = Some([1; 16]);
        app.restoration_ready = true;
        app.conversation_id = Some(vec![2; 16]);
        app.conversation_generation = 1;
        app.active_operation = Some(vec![3; 16]);
        app.conversation_busy = true;
        app.conversation_cancellable = true;
        app.insert("follow up");
        app
    }
    fn acknowledge_local_input(
        app: &mut App,
        request: super::super::queue::Request,
        conversation: [u8; 16],
    ) {
        let super::super::queue::Request::Submit(sent) = &request else {
            panic!("managed queue submit expected")
        };
        let input_id = vec![77; 16];
        app.queue_finished(super::super::queue::Update {
            request: request.clone(),
            outcome: Ok(asura_control::pb::ConversationQueueReply {
                entries: vec![asura_control::pb::ConversationQueueEntry {
                    input_id: Some(input_id.clone()),
                    project_id: sent.project_id.clone(),
                    conversation_id: Some(conversation.to_vec()),
                    target_generation: sent.expected_generation,
                    new_conversation: sent.new_conversation,
                    kind: Some(1),
                    state: Some(1),
                    text: sent.prompt.clone(),
                    sequence: Some(1),
                    order_position: Some(0),
                    ..Default::default()
                }],
                request_id: sent.request_id.clone(),
                accepted_input_id: Some(input_id),
                order_revision: Some(1),
                stale_order: Some(false),
                revision: Some(1),
                full_text: Some(false),
                pending: Some(false),
            }),
            done: true,
        });
    }
    fn queue_entry(state: u32) -> asura_control::pb::ConversationQueueEntry {
        asura_control::pb::ConversationQueueEntry {
            input_id: Some(vec![4; 16]),
            project_id: Some(vec![1; 16]),
            conversation_id: Some(vec![2; 16]),
            target_operation_id: Some(vec![3; 16]),
            target_generation: Some(1),
            kind: Some(1),
            state: Some(state),
            text: Some("follow up".into()),
            operation_id: matches!(state, 3..=5).then(|| vec![5; 16]),
            generation: matches!(state, 3..=5).then_some(2),
            sequence: Some(7),
            ..Default::default()
        }
    }
    #[test]
    fn active_enter_stages_multiple_local_inputs_without_waiting_for_ack() {
        let mut app = working_queue_app();
        app.submit();
        assert!(app.overlay.is_none());
        assert_eq!(app.draft(), "");
        assert_eq!(app.local_outbox.len(), 1);
        let first = app.take_queue_request(std::time::Instant::now()).unwrap();
        assert!(matches!(first, super::super::queue::Request::Submit(_)));
        app.insert("second input");
        app.submit();
        assert_eq!(app.draft(), "");
        assert_eq!(app.local_outbox.len(), 2);
        assert_ne!(
            app.local_outbox[0].request_id,
            app.local_outbox[1].request_id
        );
        assert_eq!(app.local_outbox[0].state, LocalInputState::Sending);
        assert_eq!(app.local_outbox[1].state, LocalInputState::Waiting);
        assert!(render(&mut app, 100, 24).contains("Pending local"));
    }
    #[test]
    fn queue_acknowledgement_preserves_newer_draft_and_captured_request() {
        let mut app = working_queue_app();
        key(&mut app, KeyCode::Char('t'), KeyModifiers::CONTROL);
        let request = app.take_queue_request(std::time::Instant::now()).unwrap();
        let super::super::queue::Request::Enqueue(sent) = request.clone() else {
            panic!("enqueue")
        };
        assert_eq!(sent.target_operation_id, Some(vec![3; 16]));
        assert_eq!(sent.kind, Some(1));
        app.insert(" newer");
        app.queue_finished(super::super::queue::Update {
            request,
            outcome: Ok(asura_control::pb::ConversationQueueReply {
                entries: vec![queue_entry(1)],
                request_id: sent.request_id,
                full_text: Some(false),
                revision: Some(1),
                pending: Some(false),
                ..Default::default()
            }),
            done: true,
        });
        assert_eq!(app.draft(), "follow up newer");
        assert!(app.queue_retained.is_none());
        let screen = render(&mut app, 100, 24);
        assert!(screen.contains("Queued"));
        assert!(screen.contains("follow up"));
    }
    #[test]
    fn queue_retry_preserves_original_identity_after_unknown_outcome() {
        let mut app = working_queue_app();
        app.queue_input(2);
        let original = app.take_queue_request(std::time::Instant::now()).unwrap();
        let super::super::queue::Request::Enqueue(sent) = original.clone() else {
            panic!("enqueue")
        };
        app.queue_finished(super::super::queue::Update {
            request: original,
            outcome: Err("outcome unconfirmed".into()),
            done: true,
        });
        app.editor = Editor::new();
        app.insert("/retry");
        app.submit();
        let super::super::queue::Request::Enqueue(retried) =
            app.take_queue_request(std::time::Instant::now()).unwrap()
        else {
            panic!("retry")
        };
        assert_eq!(sent, retried);
        assert_eq!(retried.kind, Some(2));
    }
    #[test]
    fn definite_queue_rejection_retains_text_without_blocking_a_new_choice() {
        let mut app = working_queue_app();
        app.queue_input(1);
        let request = app.take_queue_request(std::time::Instant::now()).unwrap();
        app.queue_finished(super::super::queue::Update {
            request,
            outcome: Err("capacity_exhausted; draft retained".into()),
            done: true,
        });
        assert_eq!(app.draft(), "follow up");
        assert!(app.queue_retained.is_none());
        app.submit();
        assert_eq!(app.local_outbox.len(), 1);
        assert!(matches!(
            app.take_queue_request(std::time::Instant::now()),
            Some(super::super::queue::Request::Submit(_))
        ));
    }
    #[test]
    fn held_projection_does_not_execute_and_resume_names_exact_input() {
        let mut app = working_queue_app();
        app.conversation_busy = false;
        app.editor = Editor::new();
        app.queue_finished(super::super::queue::Update {
            request: super::super::queue::Request::List {
                project: [1; 16],
                input: None,
            },
            outcome: Ok(asura_control::pb::ConversationQueueReply {
                entries: vec![queue_entry(2)],
                request_id: None,
                full_text: Some(false),
                revision: Some(1),
                pending: Some(false),
                ..Default::default()
            }),
            done: true,
        });
        assert!(app.conversation_request.is_none());
        app.insert(&format!(
            "/queue resume {}",
            crate::conversation::hex(&[4; 16])
        ));
        app.submit();
        let super::super::queue::Request::Decision(request) =
            app.take_queue_request(std::time::Instant::now()).unwrap()
        else {
            panic!("decision")
        };
        assert_eq!(request.input_id, Some(vec![4; 16]));
        assert_eq!(request.action, Some(1));
    }
    #[test]
    fn service_dispatched_successor_is_observed_once_without_client_submission() {
        let mut app = working_queue_app();
        app.conversation_busy = false;
        app.editor = Editor::new();
        app.queue_follow_inputs.push(vec![4; 16]);
        app.queue_request_scope = app.queue_scope();
        let update = || super::super::queue::Update {
            request: super::super::queue::Request::List {
                project: [1; 16],
                input: None,
            },
            outcome: Ok(asura_control::pb::ConversationQueueReply {
                entries: vec![queue_entry(3)],
                request_id: None,
                full_text: Some(false),
                revision: Some(1),
                pending: Some(false),
                ..Default::default()
            }),
            done: true,
        };
        app.queue_finished(update());
        assert!(
            matches!(app.conversation_request,Some(super::super::conversation::Request::Setup(crate::conversation::Command::Observe(id))) if id==[5;16])
        );
        assert_eq!(app.transcript.len(), 1);
        app.queue_finished(update());
        assert_eq!(app.transcript.len(), 1);
        assert_eq!(app.conversation_generation, 2);
    }
    #[test]
    fn historical_queued_generation_three_does_not_lower_restored_cursor() {
        let mut app = working_queue_app();
        app.conversation_busy = false;
        app.conversation_generation = 4;
        app.active_operation = None;
        app.editor = Editor::new();
        let mut historical = queue_entry(4);
        historical.target_generation = Some(2);
        historical.generation = Some(3);
        historical.sequence = Some(69);
        app.queue_finished(super::super::queue::Update {
            request: super::super::queue::Request::List {
                project: [1; 16],
                input: None,
            },
            outcome: Ok(asura_control::pb::ConversationQueueReply {
                entries: vec![historical],
                request_id: None,
                full_text: Some(false),
                revision: Some(87),
                pending: Some(false),
                ..Default::default()
            }),
            done: true,
        });
        assert!(
            app.conversation_request.is_none(),
            "terminal generation-3 history must not resume after the service reached generation 4"
        );
        app.insert("new input");
        app.submit();
        let Some(super::super::queue::Request::Submit(request)) =
            app.take_queue_request(std::time::Instant::now())
        else {
            panic!("restored input must enter the service queue")
        };
        assert_eq!(request.conversation_id, Some(vec![2; 16]));
        assert_eq!(request.expected_generation, Some(4));
    }

    fn queue_snapshot(
        app: &mut App,
        entries: Vec<asura_control::pb::ConversationQueueEntry>,
        revision: u64,
    ) {
        app.queue_request_scope = app.queue_scope();
        app.queue_finished(super::super::queue::Update {
            request: super::super::queue::Request::List {
                project: [1; 16],
                input: None,
            },
            outcome: Ok(asura_control::pb::ConversationQueueReply {
                entries,
                request_id: None,
                full_text: Some(false),
                revision: Some(revision),
                pending: Some(false),
                ..Default::default()
            }),
            done: true,
        });
    }
    #[test]
    fn queue_fast_completion_and_new_terminal_entries_remain_observable() {
        for initial in [vec![], vec![queue_entry(1)]] {
            let mut app = working_queue_app();
            app.conversation_busy = false;
            app.editor = Editor::new();
            queue_snapshot(&mut app, initial, 1);
            assert!(app.conversation_request.is_none());
            queue_snapshot(&mut app, vec![queue_entry(4)], 2);
            assert!(
                matches!(app.conversation_request,Some(super::super::conversation::Request::Setup(crate::conversation::Command::Observe(id))) if id==[5;16])
            );
        }
    }
    #[test]
    fn eligible_queue_completion_cannot_lower_same_conversation_generation() {
        let mut app = working_queue_app();
        app.conversation_busy = false;
        app.editor = Editor::new();
        queue_snapshot(&mut app, vec![queue_entry(1)], 1);
        app.conversation_generation = 4;
        let mut old = queue_entry(4);
        old.generation = Some(3);
        queue_snapshot(&mut app, vec![old], 2);
        assert!(app.conversation_request.is_none());
        assert_eq!(app.conversation_generation, 4);
    }
    #[test]
    fn queue_history_baseline_survives_disconnect_but_resets_on_new_epoch() {
        let mut app = working_queue_app();
        app.conversation_busy = false;
        app.editor = Editor::new();
        app.view.installation = "graph_ready";
        app.view.epoch = Some("epoch-one".into());
        app.synchronize_queue_history_scope();
        queue_snapshot(&mut app, vec![queue_entry(4)], 1);
        assert!(app.conversation_request.is_none());
        app.view.connection = "unavailable";
        app.refresh_queue_scope(std::time::Instant::now());
        app.view.connection = "connected";
        app.refresh_queue_scope(std::time::Instant::now());
        queue_snapshot(&mut app, vec![queue_entry(4)], 2);
        assert!(app.conversation_request.is_none());
        app.view.epoch = Some("epoch-two".into());
        app.synchronize_queue_history_scope();
        assert_eq!(app.queue_watermark, None);
        assert!(app.queue_follow_inputs.is_empty());
        queue_snapshot(&mut app, vec![queue_entry(3)], 3);
        assert!(app.conversation_request.is_some());
    }

    #[test]
    fn watch_baseline_history_and_ack_before_delayed_snapshot_are_distinct() {
        let mut app = working_queue_app();
        app.conversation_busy = false;
        app.conversation_id = None;
        app.conversation_generation = 0;
        app.active_operation = None;
        app.editor = Editor::new();
        app.view.installation = "graph_ready";
        app.view.epoch = Some("epoch".into());
        let now = std::time::Instant::now();
        let watch = |entries, revision, source| super::super::queue_watch::Update {
            scope: super::super::queue_watch::Scope {
                project: [1; 16],
                epoch: "epoch".into(),
            },
            source,
            received: now + std::time::Duration::from_millis(revision),
            result: Ok(asura_control::pb::ConversationQueueReply {
                entries,
                request_id: None,
                full_text: Some(false),
                revision: Some(revision),
                pending: Some(false),
                ..Default::default()
            }),
        };
        let mut history = queue_entry(4);
        history.generation = Some(3);
        history.sequence = Some(69);
        app.queue_observed(watch(vec![history], 87, 1));
        assert!(app.conversation_request.is_none());
        assert_eq!(app.conversation_generation, 0);
        // Acknowledged work must survive an absent row in a delayed full snapshot.
        let mut pending = queue_entry(1);
        pending.sequence = Some(100);
        pending.input_id = Some(vec![7; 16]);
        app.track_queue_eligibility(&[pending.clone()], false, true);
        app.queue_observed(watch(vec![], 88, 1));
        let mut complete = pending;
        complete.state = Some(4);
        complete.operation_id = Some(vec![8; 16]);
        complete.generation = Some(4);
        app.queue_observed(watch(vec![complete], 101, 1));
        assert!(
            matches!(app.conversation_request,Some(super::super::conversation::Request::Setup(crate::conversation::Command::Observe(id))) if id==[8;16])
        );
    }
    #[test]
    fn first_empty_watch_snapshot_still_establishes_history_baseline() {
        let mut app = working_queue_app();
        app.conversation_busy = false;
        app.editor = Editor::new();
        app.view.installation = "graph_ready";
        app.view.epoch = Some("epoch".into());
        let now = std::time::Instant::now();
        for (revision, entries) in [(1, vec![]), (2, vec![queue_entry(4)])] {
            app.queue_observed(super::super::queue_watch::Update {
                scope: super::super::queue_watch::Scope {
                    project: [1; 16],
                    epoch: "epoch".into(),
                },
                source: 1,
                received: now + std::time::Duration::from_millis(revision),
                result: Ok(asura_control::pb::ConversationQueueReply {
                    entries,
                    request_id: None,
                    full_text: Some(false),
                    revision: Some(revision),
                    pending: Some(false),
                    ..Default::default()
                }),
            });
        }
        assert!(app.conversation_request.is_some());
    }

    #[test]
    fn stale_explicit_queue_reply_cannot_create_a_new_scope_baseline() {
        for changed_project in [false, true] {
            let mut app = working_queue_app();
            app.view.installation = "graph_ready";
            app.view.epoch = Some("old".into());
            app.conversation_busy = false;
            app.conversation_id = None;
            app.conversation_generation = 0;
            app.queue_request = Some(super::super::queue::Request::List {
                project: [1; 16],
                input: None,
            });
            let request = app.take_queue_request(std::time::Instant::now()).unwrap();
            if changed_project {
                app.selected_project = Some([9; 16]);
            } else {
                app.view.epoch = Some("new".into());
            }
            app.refresh_queue_scope(std::time::Instant::now());
            app.queue_finished(super::super::queue::Update {
                request,
                outcome: Ok(asura_control::pb::ConversationQueueReply {
                    entries: vec![],
                    request_id: None,
                    full_text: Some(false),
                    revision: Some(87),
                    pending: Some(false),
                    ..Default::default()
                }),
                done: true,
            });
            assert_eq!(app.queue_watermark, None);
            let mut history = queue_entry(4);
            history.project_id = app.selected_project.map(|id| id.to_vec());
            history.sequence = Some(69);
            history.generation = Some(3);
            app.queue_observed(super::super::queue_watch::Update {
                scope: app.queue_scope().unwrap(),
                source: 1,
                received: std::time::Instant::now(),
                result: Ok(asura_control::pb::ConversationQueueReply {
                    entries: vec![history],
                    request_id: None,
                    full_text: Some(false),
                    revision: Some(88),
                    pending: Some(false),
                    ..Default::default()
                }),
            });
            assert!(app.conversation_request.is_none());
            assert_eq!(app.conversation_generation, 0);
        }
    }
    #[test]
    fn transient_disconnect_preserves_queue_ack_and_unconfirmed_identity() {
        for acknowledged in [false, true] {
            let mut app = working_queue_app();
            app.view.installation = "graph_ready";
            app.view.epoch = Some("same".into());
            app.submit();
            let request = app.take_queue_request(std::time::Instant::now()).unwrap();
            let original = app.queue_retained.clone();
            app.view.connection = "unavailable";
            app.view.epoch = None;
            app.view.installation = "unavailable";
            app.refresh_queue_scope(std::time::Instant::now());
            let outcome = if acknowledged {
                Ok(asura_control::pb::ConversationQueueReply {
                    entries: vec![queue_entry(1)],
                    request_id: Some(vec![4; 16]),
                    full_text: Some(false),
                    revision: Some(1),
                    pending: Some(false),
                    ..Default::default()
                })
            } else {
                Err("request_outcome_unconfirmed".into())
            };
            app.queue_finished(super::super::queue::Update {
                request,
                outcome,
                done: true,
            });
            app.view.connection = "connected";
            app.view.epoch = Some("same".into());
            app.view.installation = "graph_ready";
            app.refresh_queue_scope(std::time::Instant::now());
            if acknowledged {
                assert!(app.queue_retained.is_some());
                assert_eq!(app.local_outbox[0].text, "follow up");
                assert_eq!(app.local_outbox[0].state, LocalInputState::Unconfirmed);
            } else {
                assert!(app.queue_retained.is_some());
                assert_eq!(format!("{:?}", app.queue_retained), format!("{original:?}"));
                assert_eq!(app.local_outbox[0].text, "follow up");
                assert_eq!(app.draft(), "");
            }
        }
    }

    #[test]
    fn known_new_epoch_remains_authoritative_after_it_becomes_unavailable() {
        let mut app = working_queue_app();
        app.view.installation = "graph_ready";
        app.view.epoch = Some("A".into());
        app.submit();
        let request = app.take_queue_request(std::time::Instant::now()).unwrap();
        let mut view = app.view.clone();
        view.epoch = Some("B".into());
        app.set_view(view);
        let mut view = app.view.clone();
        view.epoch = None;
        view.connection = "unavailable";
        app.set_view(view);
        app.queue_finished(super::super::queue::Update {
            request,
            outcome: Ok(asura_control::pb::ConversationQueueReply {
                entries: vec![queue_entry(1)],
                request_id: Some(vec![4; 16]),
                full_text: Some(false),
                revision: Some(1),
                pending: Some(false),
                ..Default::default()
            }),
            done: true,
        });
        assert_eq!(app.draft(), "");
        assert_eq!(app.local_outbox[0].text, "follow up");
        assert_eq!(app.local_outbox[0].state, LocalInputState::Unconfirmed);
        assert_eq!(app.queue_watermark, None);
    }
    #[test]
    fn tools_command_uses_registry_and_preserves_invalid_draft() {
        let mut app = App::new();
        app.insert("/tools extra");
        app.submit();
        assert_eq!(app.draft(), "/tools extra");
        assert!(app.overlay.is_none());
        app.editor = Editor::new();
        app.insert("/too");
        key(&mut app, KeyCode::Tab, KeyModifiers::NONE);
        assert_eq!(app.draft(), "/tools");
        app.submit();
        assert_eq!(app.draft(), "");
        assert!(matches!(app.overlay, Some(Overlay::Tools(0))));
        let screen = render(&mut app, 200, 60);
        for tool in asura_service::tools::REGISTRY {
            assert!(screen.contains(tool.name));
            assert!(screen.contains(tool.description));
        }
        assert!(!screen.contains("arrows scroll"));
        key(&mut app, KeyCode::Down, KeyModifiers::NONE);
        assert!(matches!(app.overlay, Some(Overlay::Tools(0))));
        let screen = render(&mut app, 40, 16);
        assert!(screen.contains("arrows scroll"));
        assert!(app.panel_scroll_max > 0);
        key(&mut app, KeyCode::Down, KeyModifiers::NONE);
        assert!(matches!(app.overlay, Some(Overlay::Tools(1))));
        key(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        assert!(app.overlay.is_none());
        app.insert("/tools");
        app.submit();
        assert!(matches!(app.overlay, Some(Overlay::Tools(0))));
        assert!(help_table(78).contains("/tools"));
        assert!(
            !app.models_request
                && app.config_request.is_none()
                && app.conversation_request.is_none()
        );
    }
    fn audit_reply() -> asura_control::pb::AuditReply {
        asura_control::pb::AuditReply {
            project_id: Some(vec![1; 16]),
            health: Some(asura_control::pb::AuditHealth {
                state: Some(3),
                enabled: Some(false),
                keep_files: Some(5),
                max_file_bytes: Some(10485760),
                dropped: Some(0),
                window_capacity: Some(256),
                hydrated: Some(true),
                older_omitted: Some(false),
                reason: None,
            }),
            entries: vec![],
            error: None,
        }
    }
    #[test]
    fn audit_command_validates_and_fences_scope_and_panels() {
        let mut app = App::new();
        app.insert("/audit");
        app.submit();
        assert!(app.audit_request.is_none());
        assert_eq!(app.draft(), "/audit");
        app.view.connection = "connected";
        app.view.installation = "graph_ready";
        app.view.epoch = Some("a".into());
        app.selected_project = Some([1; 16]);
        for text in ["/audit 0", "/audit 17", "/audit -1", "/audit 1 extra"] {
            app.editor = Editor::new();
            app.insert(text);
            app.submit();
            assert!(app.audit_request.is_none());
            assert_eq!(app.draft(), text);
        }
        app.editor = Editor::new();
        app.insert("/au");
        key(&mut app, KeyCode::Tab, KeyModifiers::NONE);
        assert_eq!(app.draft(), "/audit");
        app.submit();
        assert_eq!(app.take_audit_request().unwrap().1, 16);
        app.audit_finished(Ok(audit_reply()));
        let screen = render(&mut app, 100, 40);
        assert!(screen.contains("disabled"));
        assert!(screen.contains("recent"));
        assert!(!screen.contains("arrows scroll"));
        assert_eq!(app.draft(), "");
        key(&mut app, KeyCode::Esc, KeyModifiers::NONE);
        app.insert("/audit 1");
        app.submit();
        assert_eq!(app.take_audit_request().unwrap().1, 1);
        app.selected_project = Some([2; 16]);
        app.audit_finished(Ok(audit_reply()));
        assert!(app.audit_result.is_none());
        assert_eq!(app.draft(), "/audit 1");
        app.selected_project = Some([1; 16]);
        app.submit();
        app.take_audit_request();
        app.view.epoch = Some("b".into());
        app.audit_finished(Ok(audit_reply()));
        assert!(app.audit_result.is_none());
    }
}
