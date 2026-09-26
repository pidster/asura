//! In-memory scenario driver for the two-context interaction experiment.
//!
//! This module owns fixture task transitions and captured command validation.
//! It performs no I/O and never reads or changes a client's editable draft.

use std::collections::VecDeque;
use std::fmt;
use std::sync::Arc;

const ENTRY_LIMIT: usize = 200;
const BYTE_LIMIT: usize = 256 * 1024;
pub const PROTECTED_LIMIT: usize = 8;
const SETTLED_LIMIT: usize = 8;
const DRAFT_LIMIT: usize = 65_536;
const ACCEPTANCE_MS: u64 = 250;
const TASK_MS: u64 = 12_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Project {
    Studio,
    Observatory,
}

impl Project {
    pub fn name(self) -> &'static str {
        match self {
            Self::Studio => "Studio",
            Self::Observatory => "Observatory",
        }
    }
}

/// Synthetic project observations for the selected fixture, never host telemetry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProjectStatus {
    pub project: Project,
    pub path: &'static str,
    pub git: GitStatus,
    pub model: ModelStatus,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GitStatus {
    Unavailable,
    NotRepository,
    Available {
        reference: GitReference,
        added: u32,
        removed: u32,
        operation: Option<GitOperation>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GitReference {
    Branch(&'static str),
    Detached(&'static str),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GitOperation {
    Merge,
    Rebase,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ModelStatus {
    pub name: &'static str,
    pub version: &'static str,
    pub fast: bool,
    pub context_used_percent: Option<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Scope {
    pub generation: u64,
    pub project: Project,
    pub conversation: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TaskRef {
    pub id: u64,
    pub revision: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Target {
    pub scope: Scope,
    pub task: Option<TaskRef>,
    connection_revision: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RequestId {
    pub scope: Scope,
    pub sequence: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    NewTurn,
    Steer,
    Queue,
}

/// Delivered, inert metadata for the offline command interaction trial.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommandCategory {
    BuiltIn,
    Extension,
    Skill,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommandOperation {
    ClientView,
    ClientSession,
    WorkRequest,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommandOrigin {
    Asura,
    Integration,
    ProjectSkill,
}

#[derive(Clone, Copy)]
enum ExtensionSource {
    Core,
    Checks,
    Other,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommandAvailability {
    Available,
    Unavailable,
    Stale,
    Revoked,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandDefinition {
    pub id: &'static str,
    pub name: &'static str,
    pub aliases: &'static [&'static str],
    pub label: &'static str,
    pub purpose: &'static str,
    pub category: CommandCategory,
    pub operation: CommandOperation,
    pub origin: CommandOrigin,
    pub source_id: &'static str,
    pub source_revision: u64,
    pub definition_revision: u64,
    pub availability: CommandAvailability,
    pub supported_modes: &'static [Action],
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandCatalogue {
    pub revision: u64,
    pub complete: bool,
    pub entries: Vec<CommandDefinition>,
}

/// App-captured identity and payload; the fixture never resolves a name again.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandCapture {
    pub definition_id: &'static str,
    pub source_id: &'static str,
    pub source_revision: u64,
    pub definition_revision: u64,
    pub catalogue_revision: u64,
    pub target: Target,
    pub draft_revision: u64,
    pub action: Action,
    pub arguments: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TranscriptKind {
    Output,
    User(Action),
}

/// Display provenance travels with text through publication and retention.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TranscriptEntry {
    pub kind: TranscriptKind,
    pub text: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MessageState {
    Pending,
    Unknown,
    Acknowledged,
    Queued,
    Running,
    Held,
    Completed,
    Stopped,
    Failed,
}

impl MessageState {
    fn settled(self) -> bool {
        matches!(
            self,
            Self::Acknowledged | Self::Completed | Self::Stopped | Self::Failed
        )
    }

    fn priority(self) -> u8 {
        match self {
            Self::Pending | Self::Unknown => 0,
            Self::Held => 1,
            Self::Queued => 2,
            Self::Acknowledged => 3,
            Self::Running => 4,
            _ => 5,
        }
    }
}

/// A receipt or turn projection, never a second executable request queue.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MessageView {
    pub id: RequestId,
    /// The immutable prerequisite captured when the user submitted this text.
    pub target: Target,
    pub action: Action,
    pub text: Arc<str>,
    pub state: MessageState,
    /// Assignment identity; later task revisions do not rewrite this receipt.
    pub execution_task: Option<TaskRef>,
    pub reason: Option<String>,
    pub stale: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MessageTray {
    pub items: Vec<MessageView>,
    pub history_trimmed: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DecisionAction {
    Continue,
    Stop,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    Accepted,
    Rejected(String),
}

#[derive(Debug)]
pub struct Pending {
    pub id: RequestId,
    pub text: String,
    pub target: Target,
    pub action: Action,
    pub draft_revision: u64,
    pub unknown: bool,
    pub command: Option<CommandCapture>,
    due_ms: u64,
    outcome: Option<Outcome>,
}

/// Text retained independently of the evictable display transcript.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RetainedRequest {
    pub id: RequestId,
    pub text: String,
    pub target: Target,
    pub action: Action,
    pub draft_revision: u64,
    pub reason: String,
    pub command: Option<CommandCapture>,
}

impl Pending {
    fn retained(&self, reason: String) -> RetainedRequest {
        RetainedRequest {
            id: self.id,
            text: self.text.clone(),
            target: self.target,
            action: self.action,
            draft_revision: self.draft_revision,
            reason,
            command: self.command.clone(),
        }
    }

    pub fn recorded_outcome(&self) -> Option<&Outcome> {
        self.outcome.as_ref()
    }
}

#[derive(Clone, Debug)]
pub struct Decision {
    pub id: u64,
    pub target: Target,
    pub prompt: String,
}

#[derive(Debug)]
pub struct Task {
    pub target: TaskRef,
    pub progress: u8,
    pub decision: Option<Decision>,
    started_ms: u64,
    next_progress_ms: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EventKind {
    Accepted,
    Rejected,
    SourceUnavailable,
    Progress,
    DecisionChanged,
    Completed,
    Stopped,
    Failed,
    ConnectionChanged,
    Reconciled,
    Reset,
}

/// The most recent fixture observation; no unbounded event log is retained.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FixtureEvent {
    pub scope: Scope,
    pub sequence: u128,
    pub request: Option<RequestId>,
    pub kind: EventKind,
}

#[derive(Debug, PartialEq, Eq)]
pub enum SubmitError {
    Pending,
    Empty,
    TooLarge,
    IdentityExhausted,
    Disconnected,
    StaleTarget,
    RequiresChoice,
    NoActiveTask,
    Capacity,
    StaleDecision,
    StaleCommand,
    UnsupportedCommandMode,
    InvalidCommandArguments,
}

impl fmt::Display for SubmitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Pending => "Waiting for the previous message; your draft is retained.",
            Self::Empty => "Write a message before sending.",
            Self::TooLarge => "The message exceeds 64 KiB; your draft is retained.",
            Self::IdentityExhausted => "Fixture identities exhausted; restart the trial.",
            Self::Disconnected => "This context is disconnected; your text is retained.",
            Self::StaleTarget => {
                "The captured task or connection changed; dismiss and choose again."
            }
            Self::RequiresChoice => "Work is active; explicitly choose Steer or Queue.",
            Self::NoActiveTask => "The captured work has ended; your text is retained.",
            Self::Capacity => {
                "Eight messages are protected; restore or discard held text before sending."
            }
            Self::StaleDecision => {
                "This decision is no longer available; dismiss and review again."
            }
            Self::StaleCommand => {
                "The captured command source changed or is unavailable; your draft is retained."
            }
            Self::UnsupportedCommandMode => {
                "This command does not support the selected action; your draft is retained."
            }
            Self::InvalidCommandArguments => {
                "This command's arguments are invalid; your draft is retained."
            }
        })
    }
}

pub struct Fixture {
    pub transcript: VecDeque<TranscriptEntry>,
    pub pending: Option<Pending>,
    pub truncated: bool,
    pub connected: bool,
    pub task: Option<Task>,
    pub queued: VecDeque<RetainedRequest>,
    pub recoverable: VecDeque<RetainedRequest>,
    pub reject_armed: bool,
    pub last_event: Option<FixtureEvent>,
    scope: Scope,
    base_id: u128,
    next_request: u64,
    next_task: u64,
    next_decision: u64,
    connection_revision: u64,
    event_sequence: u128,
    now_ms: u64,
    delivery_held: bool,
    display_backlog: VecDeque<TranscriptEntry>,
    backlog_bytes: usize,
    backlog_truncated: bool,
    messages: MessageTray,
    delivered_messages: MessageTray,
    catalogue_revision: u64,
    extension_revision: u64,
    core_revision: u64,
    other_revision: u64,
    skill_revision: u64,
    extension_availability: CommandAvailability,
    core_availability: CommandAvailability,
    other_availability: CommandAvailability,
    skill_availability: CommandAvailability,
    catalogue_complete: bool,
}

impl Default for Fixture {
    fn default() -> Self {
        Self::for_project(Project::Studio)
    }
}

impl Fixture {
    pub fn for_project(project: Project) -> Self {
        Self {
            transcript: [
                format!("{} · Explore a calmer way to work.", project.name()),
                "Everything here is simulated. Drafts and work stay with their project.".into(),
                "Try multiline input, Steer or Queue, and switch while work continues.".into(),
                "Enter sends · Option+Return adds a line · Ctrl+P switches · F1 shows controls"
                    .into(),
            ]
            .into_iter()
            .map(|text| TranscriptEntry {
                kind: TranscriptKind::Output,
                text,
            })
            .collect(),
            pending: None,
            truncated: false,
            connected: true,
            task: None,
            queued: VecDeque::new(),
            recoverable: VecDeque::new(),
            reject_armed: false,
            last_event: None,
            scope: Scope {
                generation: 1,
                project,
                conversation: 1,
            },
            base_id: 0,
            next_request: 1,
            next_task: 1,
            next_decision: 1,
            connection_revision: 1,
            event_sequence: 0,
            now_ms: 0,
            delivery_held: false,
            display_backlog: VecDeque::new(),
            backlog_bytes: 0,
            backlog_truncated: false,
            messages: MessageTray::default(),
            delivered_messages: MessageTray::default(),
            catalogue_revision: 1,
            extension_revision: 1,
            core_revision: 1,
            other_revision: 1,
            skill_revision: 1,
            extension_availability: CommandAvailability::Available,
            core_availability: CommandAvailability::Available,
            other_availability: CommandAvailability::Available,
            skill_availability: CommandAvailability::Available,
            catalogue_complete: true,
        }
    }

    pub fn base_id(&self) -> u128 {
        self.base_id
    }

    /// Delivered service fixtures only. Fixed client-local built-ins belong to
    /// the app and are deliberately absent from this catalogue.
    pub fn command_catalogue(&self) -> CommandCatalogue {
        if self.delivery_blocked() {
            return CommandCatalogue {
                revision: self.catalogue_revision,
                complete: false,
                entries: Vec::new(),
            };
        }
        self.command_catalogue_internal()
    }

    fn command_catalogue_internal(&self) -> CommandCatalogue {
        CommandCatalogue {
            revision: self.catalogue_revision,
            complete: self.catalogue_complete,
            entries: vec![
                self.extension_definition(ExtensionSource::Core),
                self.extension_definition(ExtensionSource::Checks),
                self.extension_definition(ExtensionSource::Other),
                CommandDefinition {
                    id: "skill:project/review",
                    name: "/skill:project/review",
                    aliases: &[],
                    label: "Review",
                    purpose: "Review with a synthetic Skill",
                    category: CommandCategory::Skill,
                    operation: CommandOperation::WorkRequest,
                    origin: CommandOrigin::ProjectSkill,
                    source_id: "skill:project",
                    source_revision: self.skill_revision,
                    definition_revision: self.skill_revision,
                    availability: self.skill_availability,
                    supported_modes: &[Action::NewTurn, Action::Queue],
                },
            ],
        }
    }

    /// The same synthetic contribution contract supplies Asura and integration work.
    fn extension_definition(&self, source: ExtensionSource) -> CommandDefinition {
        let (id, name, label, purpose, source_id, revision, availability, origin) = match source {
            ExtensionSource::Core => (
                "extension:core/check",
                "/ext:core/check",
                "Check",
                "Run a synthetic Asura check",
                "extension:core",
                self.core_revision,
                self.core_availability,
                CommandOrigin::Asura,
            ),
            ExtensionSource::Checks => (
                "extension:checks/test",
                "/ext:checks/test",
                "Test",
                "Run a synthetic check",
                "extension:checks",
                self.extension_revision,
                self.extension_availability,
                CommandOrigin::Integration,
            ),
            ExtensionSource::Other => (
                "extension:other/review",
                "/ext:other/review",
                "Review",
                "Review with a synthetic extension",
                "extension:other",
                self.other_revision,
                self.other_availability,
                CommandOrigin::Integration,
            ),
        };
        CommandDefinition {
            id,
            name,
            aliases: &[],
            label,
            purpose,
            category: CommandCategory::Extension,
            operation: CommandOperation::WorkRequest,
            origin,
            source_id,
            source_revision: revision,
            definition_revision: revision,
            availability,
            supported_modes: &[Action::NewTurn, Action::Steer, Action::Queue],
        }
    }

    pub fn toggle_command_sources(&mut self) -> bool {
        let available = self.extension_availability != CommandAvailability::Available;
        self.change_command_sources(available)
    }

    pub fn change_command_sources(&mut self, available: bool) -> bool {
        let Some(catalogue) = self.catalogue_revision.checked_add(1) else {
            return false;
        };
        let Some(extension) = self.extension_revision.checked_add(1) else {
            return false;
        };
        let Some(core) = self.core_revision.checked_add(1) else {
            return false;
        };
        let Some(other) = self.other_revision.checked_add(1) else {
            return false;
        };
        let Some(skill) = self.skill_revision.checked_add(1) else {
            return false;
        };
        self.catalogue_revision = catalogue;
        self.extension_revision = extension;
        self.core_revision = core;
        self.other_revision = other;
        self.skill_revision = skill;
        let state = if available {
            CommandAvailability::Available
        } else {
            CommandAvailability::Revoked
        };
        self.extension_availability = state;
        self.core_availability = state;
        self.other_availability = state;
        self.skill_availability = state;
        if !available {
            self.hold_revoked_commands(None);
        }
        true
    }

    pub fn replace_command_source(&mut self, source_id: &str, available: bool) -> bool {
        let Some(catalogue) = self.catalogue_revision.checked_add(1) else {
            return false;
        };
        let (revision, availability) = match source_id {
            "extension:core" => (&mut self.core_revision, &mut self.core_availability),
            "extension:checks" => (
                &mut self.extension_revision,
                &mut self.extension_availability,
            ),
            "extension:other" => (&mut self.other_revision, &mut self.other_availability),
            "skill:project" => (&mut self.skill_revision, &mut self.skill_availability),
            _ => return false,
        };
        let Some(next) = revision.checked_add(1) else {
            return false;
        };
        self.catalogue_revision = catalogue;
        *revision = next;
        *availability = if available {
            CommandAvailability::Available
        } else {
            CommandAvailability::Revoked
        };
        if !available {
            self.hold_revoked_commands(Some(source_id));
        }
        true
    }

    /// An incomplete snapshot cannot establish absence or unique completion.
    pub fn set_catalogue_complete(&mut self, complete: bool) {
        self.catalogue_complete = complete;
    }

    fn validate_command(&self, capture: &CommandCapture) -> Result<(), SubmitError> {
        let catalogue = self.command_catalogue_internal();
        if !catalogue.complete || catalogue.revision != capture.catalogue_revision {
            return Err(SubmitError::StaleCommand);
        }
        let Some(definition) = catalogue.entries.iter().find(|definition| {
            definition.id == capture.definition_id && definition.source_id == capture.source_id
        }) else {
            return Err(SubmitError::StaleCommand);
        };
        if definition.category == CommandCategory::BuiltIn
            || definition.operation != CommandOperation::WorkRequest
            || definition.availability != CommandAvailability::Available
            || definition.source_revision != capture.source_revision
            || definition.definition_revision != capture.definition_revision
        {
            return Err(SubmitError::StaleCommand);
        }
        if !definition.supported_modes.contains(&capture.action) {
            return Err(SubmitError::UnsupportedCommandMode);
        }
        Ok(())
    }

    fn hold_revoked_commands(&mut self, source_id: Option<&str>) {
        let mut retained = VecDeque::new();
        while let Some(mut queued) = self.queued.pop_front() {
            if queued
                .command
                .as_ref()
                .is_some_and(|command| source_id.is_none_or(|source| command.source_id == source))
            {
                let reason = "Command source unavailable before queued work began.".to_string();
                queued.reason = reason.clone();
                self.change_message(queued.id, MessageState::Held, None, Some(reason));
                self.record(EventKind::SourceUnavailable, Some(queued.id));
                self.recoverable.push_back(queued);
            } else {
                retained.push_back(queued);
            }
        }
        self.queued = retained;
        self.refresh_messages();
    }

    /// Fixed observations are scoped to this project and masked while delivery
    /// is unavailable. Configured identity remains known during that interval.
    pub fn project_status(&self) -> ProjectStatus {
        let mut status = match self.scope.project {
            Project::Studio => ProjectStatus {
                project: Project::Studio,
                path: "~/src/studio",
                git: GitStatus::Available {
                    reference: GitReference::Branch("main"),
                    added: 24,
                    removed: 8,
                    operation: None,
                },
                model: ModelStatus {
                    name: "demo",
                    version: "v1",
                    fast: true,
                    context_used_percent: Some(18),
                },
            },
            Project::Observatory => ProjectStatus {
                project: Project::Observatory,
                path: "~/src/observatory",
                git: GitStatus::Available {
                    reference: GitReference::Branch("feature/chat"),
                    added: 103,
                    removed: 21,
                    operation: Some(GitOperation::Rebase),
                },
                model: ModelStatus {
                    name: "demo",
                    version: "v2",
                    fast: false,
                    context_used_percent: Some(42),
                },
            },
        };
        if self.delivery_blocked() {
            status.git = GitStatus::Unavailable;
            status.model.context_used_percent = None;
        }
        status
    }

    pub fn target(&self) -> Target {
        Target {
            scope: self.scope,
            task: self.task.as_ref().map(|task| task.target),
            connection_revision: self.connection_revision,
        }
    }

    pub fn target_valid(&self, target: Target) -> bool {
        self.connected && target == self.target()
    }

    /// Authoritative fixture effects can exist before their outcome reaches the
    /// client. Presentation must show unavailable state during that interval.
    pub fn delivery_blocked(&self) -> bool {
        !self.connected || self.delivery_held
    }

    pub fn display_task(&self) -> Option<&Task> {
        if self.delivery_blocked() {
            None
        } else {
            self.task.as_ref()
        }
    }

    pub fn display_queued_count(&self) -> Option<usize> {
        (!self.delivery_blocked()).then_some(self.queued.len())
    }

    pub fn display_recoverable(&self) -> Option<&VecDeque<RetainedRequest>> {
        (!self.delivery_blocked()).then_some(&self.recoverable)
    }

    /// Read only the last delivered projection. The client knows the pending
    /// identity and text, but not its recorded outcome or execution assignment.
    pub fn message_tray(&self) -> MessageTray {
        let mut tray = self.delivered_messages.clone();
        let blocked = self.delivery_blocked();
        for message in &mut tray.items {
            message.stale = blocked;
        }
        if let Some(pending) = &self.pending
            && let Some(message) = self
                .messages
                .items
                .iter()
                .find(|item| item.id == pending.id)
        {
            let mut message = message.clone();
            message.state = if pending.unknown {
                MessageState::Unknown
            } else {
                MessageState::Pending
            };
            message.execution_task = None;
            message.reason = None;
            message.stale = false;
            tray.items.retain(|item| item.id != pending.id);
            tray.items.push(message);
        }
        // Internal order records transitions, so reversing before this stable
        // sort gives settled receipts newest first. Queue IDs preserve FIFO.
        tray.items.reverse();
        tray.items.sort_by_key(|item| {
            (
                item.state.priority(),
                if item.state == MessageState::Queued {
                    item.id.sequence
                } else {
                    0
                },
            )
        });
        tray
    }

    /// Admission for status hints uses exactly the submission guards, without
    /// allocating an identity or inspecting/copying an editable draft.
    pub fn can_submit(&self, action: Action) -> Result<(), SubmitError> {
        self.admit(self.target(), action)
    }

    fn admit(&self, target: Target, action: Action) -> Result<(), SubmitError> {
        if self.pending.is_some() {
            return Err(SubmitError::Pending);
        }
        self.validate(target, action)?;
        if self.protected_count() >= PROTECTED_LIMIT {
            return Err(SubmitError::Capacity);
        }
        self.next_request
            .checked_add(1)
            .ok_or(SubmitError::IdentityExhausted)?;
        Ok(())
    }

    fn change_message(
        &mut self,
        id: RequestId,
        state: MessageState,
        execution_task: Option<TaskRef>,
        reason: Option<String>,
    ) {
        let Some(index) = self.messages.items.iter().position(|item| item.id == id) else {
            return;
        };
        let mut message = self.messages.items.remove(index);
        message.state = state;
        message.execution_task = execution_task;
        message.reason = reason;
        self.messages.items.push(message);
    }

    fn finish_turn(&mut self, task: TaskRef, state: MessageState, reason: Option<String>) {
        let executing = self.messages.items.iter().find(|item| {
            item.state == MessageState::Running
                && item
                    .execution_task
                    .is_some_and(|execution| execution.id == task.id)
        });
        if let Some(message) = executing {
            self.change_message(message.id, state, message.execution_task, reason);
        }
    }

    fn refresh_messages(&mut self) {
        let pending = self.pending.as_ref().map(|request| request.id);
        let settled =
            |message: &&MessageView| message.state.settled() && Some(message.id) != pending;
        while self.messages.items.iter().filter(settled).count() > SETTLED_LIMIT {
            let Some(index) = self
                .messages
                .items
                .iter()
                .position(|item| item.state.settled() && Some(item.id) != pending)
            else {
                break;
            };
            self.messages.items.remove(index);
            self.messages.history_trimmed = true;
        }
        if !self.delivery_blocked() {
            self.delivered_messages = self.messages.clone();
        }
    }

    /// Count distinct protected requests; an accepted queued request can also
    /// be awaiting acknowledgement without consuming a second reservation.
    pub fn protected_count(&self) -> usize {
        self.queued.len()
            + self.recoverable.len()
            + usize::from(self.pending.as_ref().is_some_and(|pending| {
                !self.queued.iter().any(|entry| entry.id == pending.id)
                    && !self.recoverable.iter().any(|entry| entry.id == pending.id)
            }))
    }

    pub fn has_protected_text(&self) -> bool {
        self.protected_count() != 0
    }

    fn validate(&self, target: Target, action: Action) -> Result<(), SubmitError> {
        if !self.connected {
            return Err(SubmitError::Disconnected);
        }
        if !self.target_valid(target) {
            return Err(SubmitError::StaleTarget);
        }
        match (action, self.task.is_some()) {
            (Action::NewTurn, true) => Err(SubmitError::RequiresChoice),
            (Action::Steer | Action::Queue, false) => Err(SubmitError::NoActiveTask),
            _ => Ok(()),
        }
    }

    /// Convenience for a fresh idle turn. Interactive clients use the captured
    /// request API so an overlay cannot silently retarget its action.
    pub fn submit(&mut self, text: String, now_ms: u64) -> Result<RequestId, SubmitError> {
        self.submit_request(self.target(), Action::NewTurn, 0, text, now_ms)
    }

    pub fn submit_request(
        &mut self,
        target: Target,
        action: Action,
        draft_revision: u64,
        text: String,
        now_ms: u64,
    ) -> Result<RequestId, SubmitError> {
        self.submit_request_inner(target, action, draft_revision, text, now_ms, None)
    }

    pub fn submit_command_request(
        &mut self,
        capture: CommandCapture,
        raw_text: String,
        now_ms: u64,
    ) -> Result<RequestId, SubmitError> {
        if raw_text.len() > DRAFT_LIMIT {
            return Err(SubmitError::TooLarge);
        }
        self.validate_command(&capture)?;
        let definition = self
            .command_catalogue_internal()
            .entries
            .into_iter()
            .find(|definition| definition.id == capture.definition_id)
            .ok_or(SubmitError::StaleCommand)?;
        let raw_body = raw_text
            .strip_prefix(definition.name)
            .ok_or(SubmitError::InvalidCommandArguments)?;
        let argument_body = if raw_body.is_empty() {
            ""
        } else if raw_body.starts_with([' ', '\n']) {
            &raw_body[1..]
        } else {
            return Err(SubmitError::InvalidCommandArguments);
        };
        if argument_body != capture.arguments {
            return Err(SubmitError::InvalidCommandArguments);
        }
        self.submit_request_inner(
            capture.target,
            capture.action,
            capture.draft_revision,
            raw_text,
            now_ms,
            Some(capture),
        )
    }

    fn submit_request_inner(
        &mut self,
        target: Target,
        action: Action,
        draft_revision: u64,
        text: String,
        now_ms: u64,
        command: Option<CommandCapture>,
    ) -> Result<RequestId, SubmitError> {
        if self.pending.is_some() {
            return Err(SubmitError::Pending);
        }
        if text.trim().is_empty() {
            return Err(SubmitError::Empty);
        }
        if text.len() > DRAFT_LIMIT {
            return Err(SubmitError::TooLarge);
        }
        self.admit(target, action)?;
        let next = self
            .next_request
            .checked_add(1)
            .ok_or(SubmitError::IdentityExhausted)?;
        let id = RequestId {
            scope: self.scope,
            sequence: self.next_request,
        };
        self.next_request = next;
        self.now_ms = self.now_ms.max(now_ms);
        self.messages.items.push(MessageView {
            id,
            target,
            action,
            text: Arc::from(text.as_str()),
            state: MessageState::Pending,
            execution_task: None,
            reason: None,
            stale: false,
        });
        self.pending = Some(Pending {
            id,
            text,
            target,
            action,
            draft_revision,
            unknown: false,
            command,
            due_ms: self.now_ms.saturating_add(ACCEPTANCE_MS),
            outcome: None,
        });
        self.refresh_messages();
        Ok(id)
    }

    pub fn advance(&mut self, now_ms: u64) -> bool {
        self.now_ms = self.now_ms.max(now_ms);
        let mut changed = false;
        if self.connected
            && self
                .pending
                .as_ref()
                .is_some_and(|pending| self.now_ms >= pending.due_ms)
        {
            changed |= self.acknowledge_pending();
        }
        if self.task.as_ref().is_some_and(|task| {
            task.decision.is_none() && self.now_ms.saturating_sub(task.started_ms) >= TASK_MS
        }) {
            changed |= self.complete(self.now_ms);
        } else if let Some(task) = &mut self.task
            && self.now_ms >= task.next_progress_ms
        {
            let elapsed = self.now_ms.saturating_sub(task.started_ms).min(TASK_MS);
            let progress = ((elapsed * 100 / TASK_MS).min(99)) as u8;
            task.next_progress_ms = self.now_ms.saturating_add(ACCEPTANCE_MS);
            if task.progress != progress {
                task.progress = progress;
                self.record(EventKind::Progress, None);
                changed = true;
            }
        }
        changed
    }

    /// Record acceptance separately from delivery for deterministic lost-ack
    /// and ordering trials. Repeated calls never repeat accepted effects.
    pub fn accept_pending(&mut self, now_ms: u64) -> bool {
        self.now_ms = self.now_ms.max(now_ms);
        let Some(mut pending) = self.pending.take() else {
            return false;
        };
        if pending.outcome.is_some() || !self.connected {
            self.pending = Some(pending);
            return false;
        }
        // Begin withholding before applying an effect: `apply` appends its
        // outcome at the correct event position, but no acknowledgement is
        // client-visible until `acknowledge` or reconnect flushes this backlog.
        self.delivery_held = true;
        let validation =
            self.validate(pending.target, pending.action)
                .and_then(|()| match &pending.command {
                    Some(command) => self.validate_command(command),
                    None => Ok(()),
                });
        let result = match validation {
            Err(error) => Err(error.to_string()),
            Ok(()) if self.reject_armed => {
                self.reject_armed = false;
                Err("The fixture rejected this message at acceptance.".into())
            }
            Ok(()) => self.apply(&pending).map_err(|error| error.to_string()),
        };
        pending.outcome = Some(match result {
            Ok(()) => {
                self.record(EventKind::Accepted, Some(pending.id));
                Outcome::Accepted
            }
            Err(reason) => {
                self.change_message(pending.id, MessageState::Held, None, Some(reason.clone()));
                self.append(format!("Fixture  Message rejected: {reason}"));
                self.record(EventKind::Rejected, Some(pending.id));
                Outcome::Rejected(reason)
            }
        });
        self.pending = Some(pending);
        self.refresh_messages();
        true
    }

    fn apply(&mut self, pending: &Pending) -> Result<(), SubmitError> {
        match pending.action {
            Action::NewTurn => {
                let task = self.new_task()?;
                self.change_message(pending.id, MessageState::Running, Some(task.target), None);
                self.task = Some(task);
                self.append_entry(TranscriptEntry {
                    kind: TranscriptKind::User(pending.action),
                    text: pending.text.clone(),
                });
                self.append("Fixture  Message received. Simulated work has started.".into());
            }
            Action::Steer => {
                self.bump_revision()?;
                self.refresh_decision_target();
                self.change_message(pending.id, MessageState::Acknowledged, None, None);
                self.append_entry(TranscriptEntry {
                    kind: TranscriptKind::User(pending.action),
                    text: format!("Steer  {}", pending.text),
                });
                self.append(
                    "Fixture  Steering instruction acknowledged for the captured task.".into(),
                );
            }
            Action::Queue => {
                self.change_message(pending.id, MessageState::Queued, None, None);
                self.queued
                    .push_back(pending.retained("Queued after the captured task succeeds.".into()));
                self.append_entry(TranscriptEntry {
                    kind: TranscriptKind::User(pending.action),
                    text: format!("Queue  {}", pending.text),
                });
                self.append("Fixture  Follow-up queued for this conversation.".into());
            }
        }
        Ok(())
    }

    pub fn acknowledge_pending(&mut self) -> bool {
        match self.pending.as_ref() {
            Some(pending) => self.acknowledge(pending.id),
            None => false,
        }
    }

    /// Simulate a response lost after the owner records an outcome. Reconnect
    /// must resolve this same request; the client never creates a retry here.
    pub fn lose_acknowledgement(&mut self, now_ms: u64) -> bool {
        if !self.connected || self.pending.is_none() {
            return false;
        }
        self.accept_pending(now_ms);
        self.set_connected(false);
        true
    }

    #[cfg(test)]
    pub(crate) fn set_generation_for_test(&mut self, generation: u64) {
        self.scope.generation = generation;
    }

    pub fn acknowledge(&mut self, id: RequestId) -> bool {
        if !self.connected || self.pending.as_ref().is_none_or(|pending| pending.id != id) {
            return false;
        }
        self.accept_pending(self.now_ms);
        let Some(pending) = self.pending.take() else {
            return false;
        };
        match pending.outcome.as_ref() {
            Some(Outcome::Accepted) => {}
            Some(Outcome::Rejected(reason)) => {
                self.recoverable.push_back(pending.retained(reason.clone()));
            }
            None => {
                self.pending = Some(pending);
                return false;
            }
        }
        self.deliver_backlog();
        true
    }

    fn new_task(&mut self) -> Result<Task, SubmitError> {
        let next = self
            .next_task
            .checked_add(1)
            .ok_or(SubmitError::IdentityExhausted)?;
        let target = TaskRef {
            id: self.next_task,
            revision: 1,
        };
        self.next_task = next;
        Ok(Task {
            target,
            progress: 0,
            decision: None,
            started_ms: self.now_ms,
            next_progress_ms: self.now_ms.saturating_add(ACCEPTANCE_MS),
        })
    }

    /// Success starts at most the next accepted follow-up. Remaining queued
    /// requests stay protected until success consumes them or an outcome holds them.
    pub fn complete(&mut self, now_ms: u64) -> bool {
        self.now_ms = self.now_ms.max(now_ms);
        let Some(task) = self.task.take() else {
            return false;
        };
        self.finish_turn(task.target, MessageState::Completed, None);
        self.append("Fixture  Work completed successfully.".into());
        self.record(EventKind::Completed, None);
        if !self.queued.is_empty() {
            match self.new_task() {
                Ok(task) => {
                    if let Some(queued) = self.queued.pop_front() {
                        self.change_message(
                            queued.id,
                            MessageState::Running,
                            Some(task.target),
                            None,
                        );
                        self.append(format!(
                            "Fixture  Starting queued follow-up: {}",
                            queued.text
                        ));
                    }
                    self.task = Some(task);
                }
                Err(error) => self.hold_queue(&error.to_string()),
            }
        }
        self.refresh_messages();
        true
    }

    pub fn stop(&mut self) -> bool {
        if !self.connected {
            return false;
        }
        self.end_unsuccessfully(
            EventKind::Stopped,
            "Work stopped; queued text is held for recovery.",
        )
    }

    /// Synthetic failure trigger for the same recovery path as cancellation.
    pub fn fail(&mut self) -> bool {
        self.end_unsuccessfully(
            EventKind::Failed,
            "Work failed; queued text is held for recovery.",
        )
    }

    fn end_unsuccessfully(&mut self, kind: EventKind, reason: &str) -> bool {
        let Some(task) = self.task.take() else {
            return false;
        };
        let state = if kind == EventKind::Stopped {
            MessageState::Stopped
        } else {
            MessageState::Failed
        };
        self.finish_turn(task.target, state, Some(reason.into()));
        self.hold_queue(reason);
        self.append(format!("Fixture  {reason}"));
        self.record(kind, None);
        self.refresh_messages();
        true
    }

    fn hold_queue(&mut self, reason: &str) {
        while let Some(mut queued) = self.queued.pop_front() {
            queued.reason = reason.into();
            self.change_message(queued.id, MessageState::Held, None, Some(reason.into()));
            self.recoverable.push_back(queued);
        }
    }

    fn bump_revision(&mut self) -> Result<(), SubmitError> {
        let task = self.task.as_mut().ok_or(SubmitError::NoActiveTask)?;
        task.target.revision = task
            .target
            .revision
            .checked_add(1)
            .ok_or(SubmitError::IdentityExhausted)?;
        Ok(())
    }

    fn refresh_decision_target(&mut self) {
        let target = self.target();
        if let Some(task) = &mut self.task
            && let Some(decision) = &mut task.decision
        {
            decision.target = target;
        }
    }

    pub fn toggle_decision(&mut self) -> bool {
        if !self.connected || self.task.is_none() {
            return false;
        }
        let raising = self
            .task
            .as_ref()
            .is_some_and(|task| task.decision.is_none());
        let next = if raising {
            let Some(next) = self.next_decision.checked_add(1) else {
                return false;
            };
            next
        } else {
            self.next_decision
        };
        if self.bump_revision().is_err() {
            return false;
        }
        let target = self.target();
        if let Some(task) = &mut self.task {
            task.decision = raising.then(|| Decision {
                id: self.next_decision,
                target,
                prompt: "Fixture decision: continue the simulated work or stop it? Your draft remains separate.".into(),
            });
        }
        self.next_decision = next;
        self.append(
            if raising {
                "Fixture  A decision needs review; editing remains available."
            } else {
                "Fixture  The decision expired. Dismiss any open review before continuing."
            }
            .into(),
        );
        self.record(EventKind::DecisionChanged, None);
        true
    }

    pub fn respond_decision(
        &mut self,
        target: Target,
        decision_id: u64,
        action: DecisionAction,
    ) -> Result<(), SubmitError> {
        if !self.target_valid(target)
            || self
                .task
                .as_ref()
                .and_then(|task| task.decision.as_ref())
                .is_none_or(|decision| decision.id != decision_id || decision.target != target)
        {
            return Err(SubmitError::StaleDecision);
        }
        match action {
            DecisionAction::Continue => {
                self.bump_revision()?;
                if let Some(task) = &mut self.task {
                    task.decision = None;
                }
                self.append("Fixture  Decision acknowledged: continue work.".into());
                self.record(EventKind::DecisionChanged, None);
            }
            DecisionAction::Stop => {
                self.stop();
            }
        }
        Ok(())
    }

    pub fn set_connected(&mut self, connected: bool) -> bool {
        if self.connected == connected {
            return false;
        }
        let Some(revision) = self.connection_revision.checked_add(1) else {
            return false;
        };
        self.connection_revision = revision;
        self.connected = connected;
        if !connected {
            self.delivery_held = true;
        }
        self.refresh_decision_target();
        if let Some(pending) = &mut self.pending {
            pending.unknown = !connected;
        }
        self.append(
            if connected {
                "Fixture  Reconnected; reconciling the recorded request, without resubmission."
            } else {
                "Fixture  Disconnected; any pending outcome is unknown."
            }
            .into(),
        );
        self.record(EventKind::ConnectionChanged, None);
        if connected {
            // An already recorded outcome is delivered. Otherwise acceptance
            // rejects the old connection revision under the original identity.
            let id = self.pending.as_ref().map(|pending| pending.id);
            self.acknowledge_pending();
            self.record(EventKind::Reconciled, id);
            self.deliver_backlog();
        }
        true
    }

    pub fn reject_next(&mut self) {
        self.reject_armed = !self.reject_armed;
    }

    pub fn recovered(&self, id: RequestId) -> Option<&RetainedRequest> {
        self.recoverable.iter().find(|request| request.id == id)
    }

    /// Remove only after explicit discard or successful bounded client restore.
    pub fn discard_recovered(&mut self, id: RequestId) -> bool {
        if self.delivery_blocked()
            || self
                .pending
                .as_ref()
                .is_some_and(|pending| pending.id == id)
        {
            return false;
        }
        let Some(index) = self.recoverable.iter().position(|request| request.id == id) else {
            return false;
        };
        self.recoverable.remove(index);
        self.messages.items.retain(|message| message.id != id);
        self.refresh_messages();
        true
    }

    /// Check all contexts before resetting any of them. This in-memory driver
    /// is single-threaded, so no fixture transition can race that client batch.
    pub fn can_reset(&self) -> Result<(), SubmitError> {
        self.next_generation().map(|_| ())
    }

    fn next_generation(&self) -> Result<u64, SubmitError> {
        self.scope
            .generation
            .checked_add(1)
            .ok_or(SubmitError::IdentityExhausted)
    }

    /// The client must obtain its explicit discard confirmation before calling.
    pub fn reset(&mut self) -> Result<(), SubmitError> {
        let generation = self.next_generation()?;
        let project = self.scope.project;
        *self = Self::for_project(project);
        self.scope.generation = generation;
        self.record(EventKind::Reset, None);
        Ok(())
    }

    fn record(&mut self, kind: EventKind, request: Option<RequestId>) {
        self.event_sequence = self.event_sequence.saturating_add(1);
        self.last_event = Some(FixtureEvent {
            scope: self.scope,
            sequence: self.event_sequence,
            request,
            kind,
        });
    }

    fn append(&mut self, entry: String) {
        self.append_entry(TranscriptEntry {
            kind: TranscriptKind::Output,
            text: entry,
        });
    }

    fn append_entry(&mut self, entry: TranscriptEntry) {
        if self.delivery_blocked() {
            self.backlog_bytes += entry.text.len();
            self.display_backlog.push_back(entry);
            while self.display_backlog.len() > ENTRY_LIMIT || self.backlog_bytes > BYTE_LIMIT {
                if let Some(removed) = self.display_backlog.pop_front() {
                    self.backlog_bytes -= removed.text.len();
                    self.backlog_truncated = true;
                }
            }
            return;
        }
        self.append_delivered(entry);
    }

    fn deliver_backlog(&mut self) {
        if !self.connected || self.pending.is_some() {
            return;
        }
        self.delivery_held = false;
        self.truncated |= self.backlog_truncated;
        self.backlog_truncated = false;
        self.backlog_bytes = 0;
        while let Some(entry) = self.display_backlog.pop_front() {
            self.append_delivered(entry);
        }
        self.refresh_messages();
    }

    fn append_delivered(&mut self, entry: TranscriptEntry) {
        // Display-entry identities are allocated at publication, independently
        // of authoritative FixtureEvent sequences. A discarded backlog prefix
        // has never received an identity that a viewport could retain. Keeping
        // older delivered entries therefore cannot alias or reuse their IDs.
        self.transcript.push_back(entry);
        while self.transcript.len() > ENTRY_LIMIT
            || self
                .transcript
                .iter()
                .map(|entry| entry.text.len())
                .sum::<usize>()
                > BYTE_LIMIT
        {
            self.transcript.pop_front();
            self.base_id += 1;
            self.truncated = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn capture(
        fixture: &Fixture,
        id: &'static str,
        action: Action,
        arguments: &str,
    ) -> CommandCapture {
        let catalogue = fixture.command_catalogue();
        let definition = catalogue
            .entries
            .iter()
            .find(|entry| entry.id == id)
            .unwrap();
        CommandCapture {
            definition_id: definition.id,
            source_id: definition.source_id,
            source_revision: definition.source_revision,
            definition_revision: definition.definition_revision,
            catalogue_revision: catalogue.revision,
            target: fixture.target(),
            draft_revision: 7,
            action,
            arguments: arguments.into(),
        }
    }

    #[test]
    fn ct3_catalogue_keeps_colliding_labels_distinct_and_local_builtins_outside_fixture() {
        let fixture = Fixture::default();
        let catalogue = fixture.command_catalogue();
        assert!(catalogue.complete);
        assert!(catalogue.entries.len() <= 128);
        assert!(
            catalogue
                .entries
                .iter()
                .all(|entry| entry.category != CommandCategory::BuiltIn)
        );
        assert!(
            !catalogue.entries.iter().any(|entry| entry.name == "/quit"
                || entry.name == "/exit"
                || entry.name == "/help")
        );
        let review: Vec<_> = catalogue
            .entries
            .iter()
            .filter(|entry| entry.label == "Review")
            .collect();
        assert_eq!(review.len(), 2);
        assert_ne!(review[0].id, review[1].id);
        assert_ne!(review[0].source_id, review[1].source_id);
        for entry in &catalogue.entries {
            assert!(entry.name.len() <= 128);
            assert!(entry.purpose.len() <= 2048);
            assert!(!entry.name.chars().any(char::is_control));
        }
    }

    #[test]
    fn ct4_skill_steer_and_changed_arguments_are_rejected_without_detaching_text() {
        let mut fixture = working();
        let skill = capture(
            &fixture,
            "skill:project/review",
            Action::Steer,
            "review this",
        );
        assert_eq!(
            fixture.submit_command_request(skill, "/skill:project/review review this".into(), 250),
            Err(SubmitError::UnsupportedCommandMode)
        );
        assert!(fixture.pending.is_none());
        let allowed = capture(
            &fixture,
            "skill:project/review",
            Action::Queue,
            "review this",
        );
        assert_eq!(
            fixture.submit_command_request(allowed, "/skill:project/review another".into(), 250),
            Err(SubmitError::InvalidCommandArguments)
        );
        assert!(fixture.pending.is_none());
    }

    #[test]
    fn ct5b_revoke_before_acceptance_rejects_original_identity_and_retains_capture() {
        let mut fixture = working();
        let capture = capture(
            &fixture,
            "skill:project/review",
            Action::Queue,
            "review this",
        );
        let raw = "/skill:project/review review this";
        let id = fixture
            .submit_command_request(capture.clone(), raw.into(), 250)
            .unwrap();
        assert!(fixture.change_command_sources(false));
        assert!(fixture.accept_pending(500));
        assert!(
            matches!(fixture.pending.as_ref().unwrap().recorded_outcome(), Some(Outcome::Rejected(reason)) if reason.contains("command source changed"))
        );
        assert!(fixture.acknowledge(id));
        assert!(fixture.queued.is_empty());
        let held = fixture.recovered(id).unwrap();
        assert_eq!(held.id, id);
        assert_eq!(held.text, raw);
        assert_eq!(held.command.as_ref(), Some(&capture));
        assert_eq!(fixture.last_event.unwrap().request, Some(id));
    }

    #[test]
    fn ct5c_accept_before_revoke_blocks_queued_effect_under_same_identity() {
        let mut fixture = working();
        let capture = capture(
            &fixture,
            "skill:project/review",
            Action::Queue,
            "review this",
        );
        let id = fixture
            .submit_command_request(
                capture.clone(),
                "/skill:project/review review this".into(),
                250,
            )
            .unwrap();
        assert!(fixture.accept_pending(500));
        assert_eq!(
            fixture.pending.as_ref().unwrap().recorded_outcome(),
            Some(&Outcome::Accepted)
        );
        assert_eq!(fixture.queued.front().unwrap().id, id);
        assert!(fixture.change_command_sources(false));
        assert!(fixture.queued.is_empty());
        assert_eq!(
            fixture.last_event.unwrap().kind,
            EventKind::SourceUnavailable
        );
        assert_eq!(fixture.last_event.unwrap().request, Some(id));
        assert_eq!(
            fixture.recovered(id).unwrap().command.as_ref(),
            Some(&capture)
        );
        assert!(fixture.acknowledge(id));
        assert_eq!(fixture.recovered(id).unwrap().id, id);
        assert_eq!(message(&fixture, id).state, MessageState::Held);
        assert!(fixture.complete(1000));
        assert!(fixture.task.is_none());
        assert!(!fixture.acknowledge(id));
    }

    #[test]
    fn ct5c_single_source_revoke_preserves_another_sources_queued_work() {
        let mut fixture = working();
        let checks = capture(
            &fixture,
            "extension:checks/test",
            Action::Queue,
            "test this",
        );
        let id = fixture
            .submit_command_request(checks, "/ext:checks/test test this".into(), 250)
            .unwrap();
        assert!(fixture.acknowledge(id));
        assert!(fixture.replace_command_source("skill:project", false));
        assert_eq!(fixture.queued.front().unwrap().id, id);
        assert!(fixture.recovered(id).is_none());
        assert!(!fixture.replace_command_source("unknown", false));
    }

    #[test]
    fn ct6_unknown_accepted_command_reconciles_without_a_second_effect() {
        let mut fixture = working();
        let capture = capture(
            &fixture,
            "extension:checks/test",
            Action::Queue,
            "test this",
        );
        let id = fixture
            .submit_command_request(capture, "/ext:checks/test test this".into(), 250)
            .unwrap();
        assert!(fixture.lose_acknowledgement(500));
        assert!(fixture.pending.as_ref().unwrap().unknown);
        assert_eq!(fixture.queued.len(), 1);
        assert!(fixture.set_connected(true));
        assert!(fixture.pending.is_none());
        assert_eq!(fixture.queued.len(), 1);
        assert_eq!(fixture.queued.front().unwrap().id, id);
        assert!(!fixture.acknowledge(id));
    }

    #[test]
    fn ct9_first_party_and_integration_share_work_contract_without_reserved_names() {
        let fixture = Fixture::default();
        let catalogue = fixture.command_catalogue();
        let core = catalogue
            .entries
            .iter()
            .find(|entry| entry.id == "extension:core/check")
            .unwrap();
        let checks = catalogue
            .entries
            .iter()
            .find(|entry| entry.id == "extension:checks/test")
            .unwrap();
        assert_eq!(core.name, "/ext:core/check");
        assert_eq!(core.source_id, "extension:core");
        assert_eq!(core.origin, CommandOrigin::Asura);
        assert_eq!(checks.origin, CommandOrigin::Integration);
        assert!(!catalogue.entries.iter().any(|entry| {
            entry.name == "/ext:asura/check" || entry.id == "extension:asura/check"
        }));
        for entry in [core, checks] {
            assert_eq!(entry.category, CommandCategory::Extension);
            assert_eq!(entry.operation, CommandOperation::WorkRequest);
            assert!(entry.aliases.is_empty());
            assert_ne!(entry.name, "/quit");
            assert_ne!(entry.name, "/exit");
            assert_eq!(
                entry.supported_modes,
                &[Action::NewTurn, Action::Steer, Action::Queue]
            );
        }
    }

    #[test]
    fn ct9_first_party_targeted_revoke_before_acceptance_retains_identity() {
        let mut fixture = working();
        let capture = capture(&fixture, "extension:core/check", Action::Queue, "note");
        let raw = "/ext:core/check note";
        let id = fixture
            .submit_command_request(capture.clone(), raw.into(), 250)
            .unwrap();
        assert!(fixture.replace_command_source("extension:core", false));
        assert!(fixture.accept_pending(500));
        assert!(matches!(
            fixture.pending.as_ref().unwrap().recorded_outcome(),
            Some(Outcome::Rejected(_))
        ));
        assert!(fixture.acknowledge(id));
        assert_eq!(fixture.recovered(id).unwrap().text, raw);
        assert_eq!(
            fixture.recovered(id).unwrap().command.as_ref(),
            Some(&capture)
        );
        assert!(
            fixture
                .command_catalogue()
                .entries
                .iter()
                .find(|entry| entry.id == "extension:checks/test")
                .is_some_and(|entry| entry.availability == CommandAvailability::Available)
        );
    }

    #[test]
    fn ct9_first_party_accepted_queue_is_held_without_reviving_old_capture() {
        let mut fixture = working();
        let capture = capture(&fixture, "extension:core/check", Action::Queue, "note");
        let id = fixture
            .submit_command_request(capture.clone(), "/ext:core/check note".into(), 250)
            .unwrap();
        assert!(fixture.accept_pending(500));
        assert_eq!(fixture.queued.front().unwrap().id, id);
        assert!(fixture.replace_command_source("extension:core", false));
        assert!(fixture.queued.is_empty());
        assert_eq!(
            fixture.recovered(id).unwrap().command.as_ref(),
            Some(&capture)
        );
        assert!(fixture.acknowledge(id));
        assert_eq!(message(&fixture, id).state, MessageState::Held);
        assert!(fixture.replace_command_source("extension:core", true));
        assert_eq!(
            fixture.validate_command(&capture),
            Err(SubmitError::StaleCommand)
        );
        assert!(fixture.queued.is_empty());
    }

    #[test]
    fn ct9_first_party_lost_ack_reconciles_once_under_original_id() {
        let mut fixture = working();
        let capture = capture(&fixture, "extension:core/check", Action::Queue, "note");
        let id = fixture
            .submit_command_request(capture, "/ext:core/check note".into(), 250)
            .unwrap();
        assert!(fixture.lose_acknowledgement(500));
        assert!(fixture.set_connected(true));
        assert!(fixture.pending.is_none());
        assert_eq!(fixture.queued.len(), 1);
        assert_eq!(fixture.queued.front().unwrap().id, id);
        assert!(!fixture.acknowledge(id));
    }

    fn working() -> Fixture {
        let mut fixture = Fixture::default();
        fixture.submit("initial work".into(), 0).unwrap();
        assert!(fixture.advance(250));
        fixture
    }

    fn send(fixture: &mut Fixture, action: Action, text: &str) -> RequestId {
        fixture
            .submit_request(fixture.target(), action, 7, text.into(), 250)
            .unwrap()
    }

    fn queue(fixture: &mut Fixture, text: &str) -> RequestId {
        let id = send(fixture, Action::Queue, text);
        assert!(fixture.acknowledge(id));
        id
    }

    fn message(fixture: &Fixture, id: RequestId) -> MessageView {
        let tray = fixture.message_tray();
        assert_eq!(tray.items.iter().filter(|item| item.id == id).count(), 1);
        tray.items.into_iter().find(|item| item.id == id).unwrap()
    }

    #[test]
    fn ps1_project_status_defaults_belong_to_the_selected_fixture() {
        for (project, path, reference, added, removed, operation, version, fast, percent) in [
            (
                Project::Studio,
                "~/src/studio",
                "main",
                24,
                8,
                None,
                "v1",
                true,
                18,
            ),
            (
                Project::Observatory,
                "~/src/observatory",
                "feature/chat",
                103,
                21,
                Some(GitOperation::Rebase),
                "v2",
                false,
                42,
            ),
        ] {
            let mut fixture = Fixture::for_project(project);
            let status = fixture.project_status();
            assert_eq!(status.project, project);
            assert_eq!(status.path, path);
            assert_eq!(
                status.git,
                GitStatus::Available {
                    reference: GitReference::Branch(reference),
                    added,
                    removed,
                    operation,
                }
            );
            assert_eq!(
                status.model,
                ModelStatus {
                    name: "demo",
                    version,
                    fast,
                    context_used_percent: Some(percent),
                }
            );

            fixture.submit("synthetic work".into(), 0).unwrap();
            assert!(fixture.advance(250));
            assert!(fixture.advance(6_000));
            assert_eq!(fixture.project_status(), status);
        }
    }

    #[test]
    fn ps2_project_status_disconnect_hides_observations_and_reconnect_restores_them() {
        for project in [Project::Studio, Project::Observatory] {
            let mut fixture = Fixture::for_project(project);
            let known = fixture.project_status();
            assert!(fixture.set_connected(false));
            let unknown = fixture.project_status();
            assert_eq!(unknown.git, GitStatus::Unavailable);
            assert_eq!(unknown.model.context_used_percent, None);
            assert_eq!(unknown.project, known.project);
            assert_eq!(unknown.path, known.path);
            assert_eq!(unknown.model.name, known.model.name);
            assert_eq!(unknown.model.version, known.model.version);
            assert_eq!(unknown.model.fast, known.model.fast);

            assert!(fixture.set_connected(true));
            assert_eq!(fixture.project_status(), known);
        }
    }

    #[test]
    fn ps2_project_status_held_delivery_stays_unknown_as_hidden_work_advances() {
        for project in [Project::Studio, Project::Observatory] {
            let mut fixture = Fixture::for_project(project);
            let known = fixture.project_status();
            let id = fixture.submit("held acknowledgement".into(), 0).unwrap();
            assert!(fixture.accept_pending(250));
            assert!(fixture.connected);
            assert!(fixture.task.is_some());
            let held = fixture.project_status();
            assert_eq!(held.git, GitStatus::Unavailable);
            assert_eq!(held.model.context_used_percent, None);

            assert!(fixture.complete(20_000));
            assert!(fixture.task.is_none());
            assert_eq!(fixture.project_status(), held);
            assert!(fixture.acknowledge(id));
            assert_eq!(fixture.project_status(), known);
        }
    }

    #[test]
    fn ps2_project_status_reset_restores_the_same_project_defaults() {
        for project in [Project::Studio, Project::Observatory] {
            let mut fixture = Fixture::for_project(project);
            let original = fixture.project_status();
            let generation = fixture.target().scope.generation;
            fixture.submit("discarded work".into(), 0).unwrap();
            assert!(fixture.lose_acknowledgement(250));
            assert_eq!(fixture.project_status().git, GitStatus::Unavailable);

            fixture.reset().unwrap();
            assert_ne!(fixture.target().scope.generation, generation);
            assert_eq!(fixture.project_status(), original);
        }
    }

    #[test]
    fn ps2_project_status_fixture_metadata_is_bounded_single_line_and_valid() {
        for project in [Project::Studio, Project::Observatory] {
            let status = Fixture::for_project(project).project_status();
            let GitStatus::Available { reference, .. } = status.git else {
                panic!("the fixture must start with a known Git observation");
            };
            let (GitReference::Branch(reference) | GitReference::Detached(reference)) = reference;
            for (value, limit) in [
                (status.project.name(), 256),
                (status.path, 1_024),
                (reference, 256),
                (status.model.name, 256),
                (status.model.version, 256),
            ] {
                assert!(!value.is_empty());
                assert!(value.len() <= limit);
                assert!(!value.chars().any(char::is_control));
            }
            assert!(
                status
                    .model
                    .context_used_percent
                    .is_some_and(|percent| percent <= 100)
            );
        }
    }

    #[test]
    fn ts1_user_provenance_survives_lost_acknowledgement_once_for_each_action() {
        for (action, prefix) in [
            (Action::NewTurn, ""),
            (Action::Steer, "Steer  "),
            (Action::Queue, "Queue  "),
        ] {
            let mut fixture = if action == Action::NewTurn {
                Fixture::default()
            } else {
                working()
            };
            let before = fixture.transcript.clone();
            let payload = "Fixture  user-authored text\nYou · Queue  still user-authored";
            let expected = TranscriptEntry {
                kind: TranscriptKind::User(action),
                text: format!("{prefix}{payload}"),
            };
            let id = send(&mut fixture, action, payload);
            assert_eq!(fixture.transcript, before);
            assert!(fixture.lose_acknowledgement(500));
            assert_eq!(fixture.transcript, before);
            assert_eq!(fixture.display_backlog.front(), Some(&expected));
            assert_eq!(fixture.display_backlog[1].kind, TranscriptKind::Output);

            assert!(fixture.set_connected(true));
            assert_eq!(fixture.transcript[before.len()], expected);
            assert!(!fixture.acknowledge(id));
            fixture.set_connected(false);
            fixture.set_connected(true);
            assert_eq!(
                fixture
                    .transcript
                    .iter()
                    .filter(|entry| **entry == expected)
                    .count(),
                1
            );
        }
    }

    #[test]
    fn ts1_output_and_rejected_inputs_never_acquire_user_provenance() {
        for action in [Action::NewTurn, Action::Steer, Action::Queue] {
            let mut fixture = if action == Action::NewTurn {
                Fixture::default()
            } else {
                working()
            };
            for text in ["You  quoted", "You · Steer  quoted", "You · Queue  quoted"] {
                fixture.append(text.into());
                assert_eq!(
                    fixture.transcript.back(),
                    Some(&TranscriptEntry {
                        kind: TranscriptKind::Output,
                        text: text.into(),
                    })
                );
            }
            let before = fixture.transcript.clone();
            fixture.reject_next();
            let id = send(&mut fixture, action, "You  rejected payload");
            fixture.lose_acknowledgement(500);
            assert_eq!(fixture.transcript, before);
            assert!(
                fixture
                    .display_backlog
                    .iter()
                    .all(|entry| entry.kind == TranscriptKind::Output)
            );
            fixture.set_connected(true);
            assert!(
                fixture
                    .transcript
                    .iter()
                    .skip(before.len())
                    .all(|entry| entry.kind == TranscriptKind::Output)
            );
            assert_eq!(fixture.recovered(id).unwrap().text, "You  rejected payload");
        }
    }

    #[test]
    fn cp1_admission_matches_submission_without_consuming_identity_or_text() {
        let mut fixture = Fixture::default();
        for action in [Action::NewTurn, Action::Steer, Action::Queue] {
            let before = fixture.message_tray();
            let next = fixture.next_request;
            let admission = fixture.can_submit(action);
            assert_eq!(fixture.message_tray(), before);
            assert_eq!(fixture.next_request, next);
            if action == Action::NewTurn {
                assert_eq!(admission, Ok(()));
            } else {
                assert_eq!(admission, Err(SubmitError::NoActiveTask));
                assert_eq!(
                    fixture
                        .submit_request(fixture.target(), action, 0, "draft".into(), 0)
                        .map(|_| ()),
                    admission
                );
            }
        }
        fixture.submit("work".into(), 0).unwrap();
        assert_eq!(fixture.can_submit(Action::Steer), Err(SubmitError::Pending));
        fixture.acknowledge_pending();
        assert_eq!(
            fixture.can_submit(Action::NewTurn),
            Err(SubmitError::RequiresChoice)
        );
        assert_eq!(fixture.can_submit(Action::Steer), Ok(()));
        assert_eq!(fixture.can_submit(Action::Queue), Ok(()));
        for index in 0..PROTECTED_LIMIT {
            queue(&mut fixture, &format!("queued {index}"));
        }
        assert_eq!(
            fixture.can_submit(Action::Steer),
            Err(SubmitError::Capacity)
        );
        assert_eq!(
            fixture.can_submit(Action::Queue),
            Err(SubmitError::Capacity)
        );
        fixture.set_connected(false);
        assert_eq!(
            fixture.can_submit(Action::Queue),
            Err(SubmitError::Disconnected)
        );
        fixture.reset().unwrap();
        fixture.next_request = u64::MAX;
        assert_eq!(
            fixture.can_submit(Action::NewTurn),
            Err(SubmitError::IdentityExhausted)
        );
        assert!(fixture.message_tray().items.is_empty());
    }

    #[test]
    fn cp2_turn_queue_and_receipt_keep_original_identity_and_full_text() {
        let mut fixture = Fixture::default();
        let initial_target = fixture.target();
        let initial = fixture.submit("initial\n完整 text".into(), 0).unwrap();
        let pending = message(&fixture, initial);
        assert_eq!(pending.state, MessageState::Pending);
        assert_eq!(pending.target, initial_target);
        assert_eq!(&*pending.text, "initial\n完整 text");
        assert_eq!(pending.execution_task, None);
        fixture.accept_pending(250);
        assert_eq!(message(&fixture, initial), pending);
        fixture.acknowledge(initial);
        let executing = message(&fixture, initial);
        assert_eq!(executing.state, MessageState::Running);
        assert_eq!(executing.execution_task, fixture.target().task);
        assert!(Arc::ptr_eq(&pending.text, &executing.text));

        let steer = send(&mut fixture, Action::Steer, "a steering receipt");
        fixture.acknowledge(steer);
        let queued_target = fixture.target();
        let first = queue(&mut fixture, "first\nqueued follow-up");
        let second = queue(&mut fixture, "second queued follow-up");
        assert_eq!(message(&fixture, steer).state, MessageState::Acknowledged);
        assert_eq!(message(&fixture, steer).execution_task, None);
        assert_eq!(message(&fixture, first).state, MessageState::Queued);
        assert_eq!(message(&fixture, first).target, queued_target);
        assert_eq!(message(&fixture, first).execution_task, None);
        let queued_ids: Vec<_> = fixture
            .message_tray()
            .items
            .iter()
            .filter(|item| item.state == MessageState::Queued)
            .map(|item| item.id)
            .collect();
        assert_eq!(queued_ids, [first, second]);
        let presented_ids: Vec<_> = fixture
            .message_tray()
            .items
            .iter()
            .map(|item| item.id)
            .collect();
        assert_eq!(presented_ids, [first, second, steer, initial]);

        fixture.complete(500);
        assert_eq!(message(&fixture, initial).state, MessageState::Completed);
        let successor = message(&fixture, first);
        assert_eq!(successor.state, MessageState::Running);
        assert_eq!(successor.target, queued_target);
        assert_eq!(successor.execution_task, fixture.target().task);
        assert_ne!(
            successor.execution_task.map(|task| task.id),
            queued_target.task.map(|task| task.id)
        );
        assert_eq!(message(&fixture, second).state, MessageState::Queued);
        fixture.complete(501);
        assert_eq!(message(&fixture, first).state, MessageState::Completed);
        assert_eq!(message(&fixture, second).state, MessageState::Running);
        assert_eq!(message(&fixture, steer).state, MessageState::Acknowledged);
        assert_eq!(&*message(&fixture, first).text, "first\nqueued follow-up");
    }

    #[test]
    fn cp2_completion_before_acceptance_holds_the_captured_message() {
        for action in [Action::Steer, Action::Queue] {
            let mut fixture = working();
            let target = fixture.target();
            let id = send(&mut fixture, action, "retain captured request");
            fixture.complete(500);
            assert_eq!(message(&fixture, id).state, MessageState::Pending);
            fixture.acknowledge(id);
            let held = message(&fixture, id);
            assert_eq!(held.state, MessageState::Held);
            assert_eq!(held.target, target);
            assert_eq!(held.execution_task, None);
            assert!(
                held.reason
                    .as_ref()
                    .is_some_and(|reason| reason.contains("changed"))
            );
            assert_eq!(&*held.text, "retain captured request");
            assert!(fixture.task.is_none());
        }
    }

    #[test]
    fn cp2_unacknowledged_effects_and_offline_completion_never_leak_into_tray() {
        for action in [Action::NewTurn, Action::Steer, Action::Queue] {
            let mut fixture = if action == Action::NewTurn {
                Fixture::default()
            } else {
                working()
            };
            let prior = fixture.message_tray();
            let target = fixture.target();
            let id = send(&mut fixture, action, "unknown\n完整 payload");
            let submitted = message(&fixture, id);
            fixture.accept_pending(500);
            assert_eq!(message(&fixture, id), submitted);
            assert_eq!(fixture.display_queued_count(), None);
            fixture.set_connected(false);
            fixture.complete(501);
            if action == Action::Queue {
                fixture.complete(502);
            }
            let unknown = message(&fixture, id);
            assert_eq!(unknown.state, MessageState::Unknown);
            assert_eq!(unknown.target, target);
            assert_eq!(unknown.execution_task, None);
            assert_eq!(unknown.reason, None);
            assert!(!unknown.stale);
            assert!(Arc::ptr_eq(&unknown.text, &submitted.text));
            for mut previous in prior.items {
                previous.stale = true;
                assert_eq!(message(&fixture, previous.id), previous);
            }
            assert_eq!(fixture.message_tray().items[0].id, id);
            fixture.set_connected(true);
            let observed = message(&fixture, id);
            assert_eq!(
                observed.state,
                if action == Action::Steer {
                    MessageState::Acknowledged
                } else {
                    MessageState::Completed
                }
            );
            assert_eq!(observed.target, target);
            assert!(!observed.stale);
            assert_eq!(&*observed.text, "unknown\n完整 payload");
            assert!(!fixture.acknowledge(id));
            let reconciled = fixture.message_tray();
            fixture.set_connected(false);
            fixture.set_connected(true);
            assert_eq!(fixture.message_tray(), reconciled);
        }
    }

    #[test]
    fn cp2_unacknowledged_queue_reconciles_to_running_successor_without_regression() {
        let mut fixture = working();
        let id = send(&mut fixture, Action::Queue, "next task");
        fixture.lose_acknowledgement(500);
        fixture.complete(501);
        let successor = fixture.target().task;
        assert_eq!(message(&fixture, id).state, MessageState::Unknown);
        fixture.set_connected(true);
        assert_eq!(message(&fixture, id).state, MessageState::Running);
        assert_eq!(message(&fixture, id).execution_task, successor);
        assert!(fixture.queued.is_empty());
        assert!(!fixture.acknowledge(id));
        assert_eq!(fixture.target().task, successor);
    }

    #[test]
    fn cp2_stop_and_failure_change_executing_turn_and_hold_queue_but_not_steer_receipt() {
        for fail in [false, true] {
            let mut fixture = working();
            let initial = fixture.message_tray().items[0].id;
            let steer = send(&mut fixture, Action::Steer, "received steering");
            fixture.acknowledge(steer);
            let queued = queue(&mut fixture, "held follow-up");
            assert!(if fail { fixture.fail() } else { fixture.stop() });
            assert_eq!(
                message(&fixture, initial).state,
                if fail {
                    MessageState::Failed
                } else {
                    MessageState::Stopped
                }
            );
            assert_eq!(message(&fixture, steer).state, MessageState::Acknowledged);
            assert_eq!(message(&fixture, queued).state, MessageState::Held);
            assert_eq!(
                message(&fixture, queued).reason.as_deref(),
                Some(fixture.recovered(queued).unwrap().reason.as_str())
            );
            assert_eq!(fixture.message_tray().items[0].id, queued);
            assert!(fixture.discard_recovered(queued));
            assert!(
                !fixture
                    .message_tray()
                    .items
                    .iter()
                    .any(|item| item.id == queued)
            );
        }
    }

    #[test]
    fn cp2_held_before_delivery_is_unknown_until_reconciled_and_cannot_be_discarded() {
        for fail in [false, true] {
            let mut fixture = working();
            let id = send(&mut fixture, Action::Queue, "full\nheld text");
            fixture.accept_pending(500);
            assert!(if fail { fixture.fail() } else { fixture.stop() });
            assert_eq!(message(&fixture, id).state, MessageState::Pending);
            assert!(!fixture.discard_recovered(id));
            fixture.set_connected(false);
            assert_eq!(message(&fixture, id).state, MessageState::Unknown);
            assert!(!fixture.discard_recovered(id));
            assert_eq!(fixture.pending.as_ref().unwrap().text, "full\nheld text");
            assert_eq!(fixture.recovered(id).unwrap().text, "full\nheld text");
            fixture.set_connected(true);
            assert_eq!(message(&fixture, id).state, MessageState::Held);
            assert_eq!(&*message(&fixture, id).text, "full\nheld text");
            assert!(fixture.discard_recovered(id));
            assert!(
                !fixture
                    .message_tray()
                    .items
                    .iter()
                    .any(|item| item.id == id)
            );
            assert!(fixture.recovered(id).is_none());
        }
    }

    #[test]
    fn cp2_interruption_before_acceptance_retains_the_request_and_terminal_turn() {
        for fail in [false, true] {
            for action in [Action::Steer, Action::Queue] {
                let mut fixture = working();
                let original = fixture.message_tray().items[0].id;
                let id = send(&mut fixture, action, "interrupted before acceptance");
                assert!(if fail { fixture.fail() } else { fixture.stop() });
                assert_eq!(message(&fixture, id).state, MessageState::Pending);
                assert_eq!(
                    message(&fixture, original).state,
                    if fail {
                        MessageState::Failed
                    } else {
                        MessageState::Stopped
                    }
                );
                fixture.acknowledge(id);
                let held = message(&fixture, id);
                assert_eq!(held.state, MessageState::Held);
                assert_eq!(held.execution_task, None);
                assert_eq!(&*held.text, "interrupted before acceptance");
                assert_eq!(fixture.protected_count(), 1);
                assert!(fixture.task.is_none());
            }
        }
    }

    #[test]
    fn cp2_successor_identity_exhaustion_holds_every_queued_identity() {
        let mut fixture = working();
        let initial = fixture.message_tray().items[0].id;
        let first = queue(&mut fixture, "first successor");
        let second = queue(&mut fixture, "second successor");
        fixture.next_task = u64::MAX;
        assert!(fixture.complete(500));
        assert_eq!(message(&fixture, initial).state, MessageState::Completed);
        for id in [first, second] {
            let held = message(&fixture, id);
            assert_eq!(held.state, MessageState::Held);
            assert_eq!(held.execution_task, None);
            assert!(
                held.reason
                    .as_ref()
                    .is_some_and(|reason| reason.contains("exhausted"))
            );
            assert_eq!(&*held.text, fixture.recovered(id).unwrap().text);
        }
        assert!(fixture.task.is_none());
        assert_eq!(fixture.protected_count(), 2);
    }

    #[test]
    fn cp2_disconnect_before_acceptance_and_rejection_retain_one_held_identity() {
        for disconnect in [false, true] {
            let mut fixture = Fixture::default();
            let id = fixture.submit("retain rejected text".into(), 0).unwrap();
            if disconnect {
                fixture.set_connected(false);
                assert_eq!(message(&fixture, id).state, MessageState::Unknown);
                fixture.set_connected(true);
            } else {
                fixture.reject_next();
                fixture.accept_pending(250);
                assert_eq!(message(&fixture, id).state, MessageState::Pending);
                fixture.acknowledge(id);
            }
            let held = message(&fixture, id);
            assert_eq!(held.state, MessageState::Held);
            assert!(held.reason.is_some());
            assert_eq!(&*held.text, "retain rejected text");
            assert_eq!(fixture.message_tray().items.len(), 1);
            assert!(!fixture.acknowledge(id));
            assert!(fixture.discard_recovered(id));
            assert!(fixture.message_tray().items.is_empty());
        }
    }

    #[test]
    fn cp2_foreign_duplicate_and_obsolete_acknowledgements_do_not_change_tray() {
        let mut studio = Fixture::default();
        let mut observatory = Fixture::for_project(Project::Observatory);
        let old = studio.submit("old studio".into(), 0).unwrap();
        let foreign = observatory.submit("observatory".into(), 0).unwrap();
        let initial = studio.message_tray();
        assert!(!studio.acknowledge(foreign));
        assert_eq!(studio.message_tray(), initial);
        studio.reset().unwrap();
        let fresh = studio.submit("fresh studio".into(), 0).unwrap();
        let reset = studio.message_tray();
        assert!(!studio.acknowledge(old));
        assert!(!studio.acknowledge(foreign));
        assert_eq!(studio.message_tray(), reset);
        studio.acknowledge(fresh);
        let accepted = studio.message_tray();
        assert!(!studio.acknowledge(fresh));
        assert_eq!(studio.message_tray(), accepted);
        assert_eq!(observatory.message_tray().items[0].id, foreign);
        assert_eq!(
            observatory.message_tray().items[0].state,
            MessageState::Pending
        );
    }

    #[test]
    fn cp3_recent_settled_history_prunes_without_evicting_any_protected_text() {
        let mut fixture = working();
        let running = fixture.message_tray().items[0].id;
        let receipts: Vec<_> = (0..12)
            .map(|index| {
                let id = send(&mut fixture, Action::Steer, &format!("receipt {index}"));
                fixture.acknowledge(id);
                id
            })
            .collect();
        let tray = fixture.message_tray();
        assert!(tray.history_trimmed);
        assert_eq!(tray.items.len(), SETTLED_LIMIT + 1);
        let retained: Vec<_> = tray
            .items
            .iter()
            .filter(|item| item.state == MessageState::Acknowledged)
            .map(|item| item.id)
            .collect();
        assert_eq!(
            retained,
            receipts[4..].iter().rev().copied().collect::<Vec<_>>()
        );
        let protected: Vec<_> = (0..7)
            .map(|index| queue(&mut fixture, &format!("queue {index}")))
            .collect();
        let pending = send(&mut fixture, Action::Queue, "eighth protected");
        assert_eq!(fixture.message_tray().items.len(), 17);
        assert_eq!(fixture.protected_count(), PROTECTED_LIMIT);
        fixture.accept_pending(500);
        fixture.stop();
        assert_eq!(fixture.message_tray().items.len(), 17);
        assert_eq!(message(&fixture, running).state, MessageState::Running);
        fixture.acknowledge(pending);
        let tray = fixture.message_tray();
        assert_eq!(tray.items.len(), 16);
        assert_eq!(message(&fixture, running).state, MessageState::Stopped);
        for id in protected.into_iter().chain([pending]) {
            assert_eq!(message(&fixture, id).state, MessageState::Held);
            assert_eq!(
                &*message(&fixture, id).text,
                fixture.recovered(id).unwrap().text
            );
        }
        fixture.reset().unwrap();
        assert_eq!(fixture.message_tray(), MessageTray::default());
    }

    #[test]
    fn cp3_pending_steer_receipt_is_not_pruned_before_delivery() {
        let mut fixture = working();
        for _ in 0..SETTLED_LIMIT {
            let id = send(&mut fixture, Action::Steer, "settled receipt");
            fixture.acknowledge(id);
        }
        let id = send(&mut fixture, Action::Steer, "pending receipt must survive");
        assert!(!fixture.message_tray().history_trimmed);
        fixture.accept_pending(500);
        fixture.complete(501);
        // Hidden settlement may expire history, but neither it nor its trim
        // indicator becomes observed before this original request is delivered.
        assert!(fixture.messages.history_trimmed);
        assert!(!fixture.message_tray().history_trimmed);
        assert_eq!(&*message(&fixture, id).text, "pending receipt must survive");
        assert_eq!(message(&fixture, id).state, MessageState::Pending);
        fixture.acknowledge(id);
        assert_eq!(message(&fixture, id).state, MessageState::Acknowledged);
        assert_eq!(fixture.message_tray().items.len(), SETTLED_LIMIT);
        assert!(fixture.message_tray().history_trimmed);
    }

    #[test]
    fn cp3_full_unicode_payload_survives_transcript_eviction_and_snapshot_expiry() {
        let mut fixture = working();
        let payload = "👩🏽‍💻 e\u{301}\n完整 line\n".repeat(1800);
        assert!(payload.len() <= DRAFT_LIMIT);
        let id = send(&mut fixture, Action::Steer, &payload);
        fixture.acknowledge(id);
        let pinned = message(&fixture, id).text;
        for _ in 0..300 {
            fixture.append("display eviction".repeat(1000));
        }
        assert!(fixture.truncated);
        assert_eq!(&*message(&fixture, id).text, payload);
        assert!(Arc::ptr_eq(&pinned, &message(&fixture, id).text));
        for _ in 0..SETTLED_LIMIT {
            let later = send(&mut fixture, Action::Steer, "new receipt");
            fixture.acknowledge(later);
        }
        assert!(
            !fixture
                .message_tray()
                .items
                .iter()
                .any(|item| item.id == id)
        );
        assert!(fixture.message_tray().history_trimmed);
        assert_eq!(&*pinned, payload);
    }

    #[test]
    fn cp3_projection_snapshots_share_text_and_stay_bounded_through_offline_transitions() {
        fn assert_bounds(fixture: &Fixture) {
            let logical_bytes =
                |tray: &MessageTray| tray.items.iter().map(|item| item.text.len()).sum::<usize>();
            for projection in [&fixture.messages, &fixture.delivered_messages] {
                assert!(projection.items.len() <= 17);
                assert!(logical_bytes(projection) <= 17 * DRAFT_LIMIT);
                for (index, item) in projection.items.iter().enumerate() {
                    assert!(
                        !projection.items[index + 1..]
                            .iter()
                            .any(|other| other.id == item.id)
                    );
                }
            }
            assert!(fixture.message_tray().items.len() <= 18);
            for observed in &fixture.delivered_messages.items {
                if let Some(authoritative) = fixture
                    .messages
                    .items
                    .iter()
                    .find(|item| item.id == observed.id)
                {
                    assert!(Arc::ptr_eq(&observed.text, &authoritative.text));
                }
            }
            // Deliberately count shared projection payloads twice and include
            // bounded transcript/backlog copies: even this overestimate fits.
            let total = logical_bytes(&fixture.messages)
                + logical_bytes(&fixture.delivered_messages)
                + fixture
                    .pending
                    .as_ref()
                    .map_or(0, |pending| pending.text.len())
                + fixture
                    .queued
                    .iter()
                    .chain(&fixture.recoverable)
                    .map(|item| item.text.len())
                    .sum::<usize>()
                + fixture
                    .transcript
                    .iter()
                    .chain(&fixture.display_backlog)
                    .map(|entry| entry.text.len())
                    .sum::<usize>();
            assert!(total < 4 * 1024 * 1024, "retained message bytes: {total}");
        }
        let mut fixture = Fixture::default();
        let payload = "x".repeat(DRAFT_LIMIT);
        fixture.submit(payload.clone(), 0).unwrap();
        fixture.acknowledge_pending();
        for _ in 0..12 {
            let id = send(&mut fixture, Action::Steer, &payload);
            fixture.acknowledge(id);
            assert_bounds(&fixture);
        }
        for _ in 0..7 {
            queue(&mut fixture, &payload);
            assert_bounds(&fixture);
        }
        let pending = send(&mut fixture, Action::Queue, &payload);
        assert_bounds(&fixture);
        fixture.lose_acknowledgement(500);
        for instant in 501..=509 {
            fixture.complete(instant);
            assert_bounds(&fixture);
        }
        fixture.set_connected(true);
        assert_bounds(&fixture);
        assert_eq!(message(&fixture, pending).state, MessageState::Completed);
        fixture.reset().unwrap();
        assert_bounds(&fixture);
        assert_eq!(fixture.message_tray(), MessageTray::default());
    }

    #[test]
    fn tp2_acceptance_is_delayed_and_duplicate_ack_is_ignored() {
        let mut fixture = Fixture::default();
        let before = fixture.transcript.len();
        let id = fixture.submit("first".into(), 100).unwrap();
        assert!(!fixture.advance(349));
        assert_eq!(fixture.transcript.len(), before);
        assert_eq!(
            fixture.submit("second".into(), 101),
            Err(SubmitError::Pending)
        );
        assert_eq!(fixture.pending.as_ref().unwrap().text, "first");
        assert!(fixture.advance(350));
        assert!(!fixture.acknowledge(id));
        assert_eq!(fixture.transcript.len(), before + 2);
        assert!(fixture.task.is_some());
    }

    #[test]
    fn tp2_invalid_and_oversized_requests_never_consume_identity_or_capacity() {
        let mut fixture = Fixture::default();
        assert_eq!(fixture.submit(" \n".into(), 0), Err(SubmitError::Empty));
        assert_eq!(
            fixture.submit("x".repeat(DRAFT_LIMIT + 1), 0),
            Err(SubmitError::TooLarge)
        );
        assert_eq!(fixture.protected_count(), 0);
        assert_eq!(
            fixture.submit("x".repeat(DRAFT_LIMIT), 0).unwrap().sequence,
            1
        );
    }

    #[test]
    fn tp2_active_work_cannot_implicitly_receive_a_new_turn() {
        let mut fixture = working();
        assert_eq!(
            fixture.submit("implicit".into(), 300),
            Err(SubmitError::RequiresChoice)
        );
        assert!(fixture.pending.is_none());
        let id = send(&mut fixture, Action::Steer, "explicit steering");
        let pending = fixture.pending.as_ref().unwrap();
        assert_eq!(pending.draft_revision, 7);
        assert_eq!(pending.action, Action::Steer);
        assert!(fixture.acknowledge(id));
        assert!(
            fixture
                .transcript
                .iter()
                .any(|line| line.text.contains("explicit steering"))
        );
    }

    #[test]
    fn tp2_transcript_eviction_does_not_discard_pending_or_rejected_text() {
        let mut fixture = working();
        let user_entry = fixture
            .transcript
            .iter()
            .find(|entry| entry.kind == TranscriptKind::User(Action::NewTurn))
            .unwrap()
            .clone();
        send(&mut fixture, Action::Steer, "pending survives");
        for _ in 0..300 {
            fixture.append("retained display entry".repeat(1000));
        }
        assert!(fixture.truncated);
        assert!(fixture.transcript.len() <= ENTRY_LIMIT);
        assert!(
            fixture
                .transcript
                .iter()
                .map(|entry| entry.text.len())
                .sum::<usize>()
                <= BYTE_LIMIT
        );
        assert!(!fixture.transcript.contains(&user_entry));
        assert!(
            fixture
                .transcript
                .iter()
                .all(|entry| entry.kind == TranscriptKind::Output)
        );
        assert_eq!(fixture.pending.as_ref().unwrap().text, "pending survives");
        fixture.reject_next();
        fixture.acknowledge_pending();
        for _ in 0..300 {
            fixture.append("later display".into());
        }
        assert_eq!(
            fixture.recoverable.front().unwrap().text,
            "pending survives"
        );
    }

    #[test]
    fn tp3_progress_does_not_invalidate_choice_but_decision_change_does() {
        let mut fixture = working();
        let captured = fixture.target();
        assert!(fixture.advance(750));
        assert!(fixture.target_valid(captured));
        assert!(fixture.toggle_decision());
        assert!(!fixture.target_valid(captured));
        assert_eq!(
            fixture.submit_request(captured, Action::Queue, 1, "old".into(), 800),
            Err(SubmitError::StaleTarget)
        );
        assert!(fixture.pending.is_none());
    }

    #[test]
    fn tp3_expired_decision_cannot_answer_the_replacement_decision() {
        let mut fixture = working();
        fixture.toggle_decision();
        let old = fixture.task.as_ref().unwrap().decision.clone().unwrap();
        fixture.toggle_decision();
        fixture.toggle_decision();
        let replacement = fixture.task.as_ref().unwrap().decision.clone().unwrap();
        assert_ne!(old.id, replacement.id);
        assert_eq!(
            fixture.respond_decision(old.target, old.id, DecisionAction::Stop),
            Err(SubmitError::StaleDecision)
        );
        assert!(fixture.task.is_some());
        assert!(
            fixture
                .respond_decision(replacement.target, replacement.id, DecisionAction::Continue)
                .is_ok()
        );
        assert!(fixture.task.as_ref().unwrap().decision.is_none());
        assert_eq!(
            fixture.respond_decision(replacement.target, replacement.id, DecisionAction::Stop),
            Err(SubmitError::StaleDecision)
        );
    }

    #[test]
    fn tp3_decision_blocks_automatic_completion_until_explicit_continue() {
        let mut fixture = working();
        fixture.toggle_decision();
        let decision = fixture.task.as_ref().unwrap().decision.clone().unwrap();
        fixture.advance(20_000);
        assert!(fixture.task.is_some());
        fixture
            .respond_decision(decision.target, decision.id, DecisionAction::Continue)
            .unwrap();
        fixture.advance(20_001);
        assert!(fixture.task.is_none());
    }

    #[test]
    fn tp3_continue_before_expiry_applies_once() {
        let mut fixture = working();
        fixture.toggle_decision();
        let decision = fixture.task.as_ref().unwrap().decision.clone().unwrap();
        fixture
            .respond_decision(decision.target, decision.id, DecisionAction::Continue)
            .unwrap();
        let target = fixture.target();
        assert_eq!(
            fixture.respond_decision(decision.target, decision.id, DecisionAction::Continue),
            Err(SubmitError::StaleDecision)
        );
        assert_eq!(fixture.target(), target);
    }

    #[test]
    fn tp4_completion_before_acceptance_rejects_captured_steer_and_queue() {
        for action in [Action::Steer, Action::Queue] {
            let mut fixture = working();
            let id = send(&mut fixture, action, "retain on stale task");
            fixture.complete(500);
            assert!(fixture.acknowledge(id));
            assert!(fixture.task.is_none());
            assert!(fixture.queued.is_empty());
            assert_eq!(fixture.recovered(id).unwrap().text, "retain on stale task");
            assert_eq!(fixture.recovered(id).unwrap().draft_revision, 7);
        }
    }

    #[test]
    fn tp4_acceptance_before_completion_applies_steer_once() {
        let mut fixture = working();
        let id = send(&mut fixture, Action::Steer, "applied once");
        assert!(fixture.accept_pending(500));
        assert!(!fixture.accept_pending(501));
        assert!(fixture.complete(502));
        assert!(fixture.acknowledge(id));
        assert!(!fixture.acknowledge(id));
        assert!(fixture.recoverable.is_empty());
        assert_eq!(
            fixture
                .transcript
                .iter()
                .filter(|line| line.text.contains("applied once"))
                .count(),
            1
        );
    }

    #[test]
    fn tp4_acceptance_before_completion_starts_queued_turn_once() {
        let mut fixture = working();
        let original = fixture.task.as_ref().unwrap().target.id;
        let id = send(&mut fixture, Action::Queue, "follow-up");
        assert!(fixture.accept_pending(500));
        assert_eq!(fixture.queued.len(), 1);
        assert_eq!(fixture.protected_count(), 1);
        assert!(fixture.complete(501));
        let next = fixture.task.as_ref().unwrap().target.id;
        assert_ne!(original, next);
        assert!(fixture.acknowledge(id));
        assert!(!fixture.acknowledge(id));
        assert_eq!(fixture.task.as_ref().unwrap().target.id, next);
        assert!(fixture.queued.is_empty());
        assert_eq!(
            fixture.transcript.back(),
            Some(&TranscriptEntry {
                kind: TranscriptKind::Output,
                text: "Fixture  Starting queued follow-up: follow-up".into(),
            })
        );
    }

    #[test]
    fn tp4_revision_change_before_acceptance_retains_request() {
        let mut fixture = working();
        let id = send(&mut fixture, Action::Steer, "old revision");
        fixture.toggle_decision();
        assert!(fixture.acknowledge(id));
        assert_eq!(fixture.recovered(id).unwrap().text, "old revision");
        assert!(fixture.task.as_ref().unwrap().decision.is_some());
    }

    #[test]
    fn tp4_two_contexts_reject_each_others_targets_and_acknowledgements() {
        let mut studio = Fixture::default();
        let mut observatory = Fixture::for_project(Project::Observatory);
        let studio_id = studio.submit("studio".into(), 0).unwrap();
        let obs_id = observatory.submit("observatory".into(), 0).unwrap();
        assert_ne!(studio_id, obs_id);
        assert!(!observatory.acknowledge(studio_id));
        assert_eq!(observatory.pending.as_ref().unwrap().text, "observatory");
        assert!(studio.acknowledge(studio_id));
        assert!(observatory.acknowledge(obs_id));
        assert_eq!(
            observatory.submit_request(
                studio.target(),
                Action::Steer,
                1,
                "wrong project".into(),
                500
            ),
            Err(SubmitError::StaleTarget)
        );
        assert!(
            !observatory
                .transcript
                .iter()
                .any(|line| line.text == "studio")
        );
    }

    #[test]
    fn tp4_disconnect_before_acceptance_reconciles_original_id_to_rejection() {
        let mut fixture = Fixture::default();
        let id = fixture.submit("unknown request".into(), 0).unwrap();
        assert!(fixture.set_connected(false));
        assert!(fixture.pending.as_ref().unwrap().unknown);
        assert!(!fixture.advance(1000));
        assert!(!fixture.acknowledge(id));
        assert!(fixture.set_connected(true));
        assert!(fixture.pending.is_none());
        assert!(fixture.task.is_none());
        assert_eq!(fixture.recovered(id).unwrap().text, "unknown request");
        assert_eq!(fixture.next_request, 2);
        assert!(!fixture.acknowledge(id));
        assert_eq!(fixture.last_event.unwrap().kind, EventKind::Reconciled);
    }

    #[test]
    fn tp4_acceptance_before_disconnect_replays_success_without_a_second_task() {
        let mut fixture = Fixture::default();
        let id = fixture
            .submit("accepted before disconnect".into(), 0)
            .unwrap();
        fixture.accept_pending(250);
        let task_id = fixture.task.as_ref().unwrap().target.id;
        fixture.set_connected(false);
        assert!(fixture.pending.as_ref().unwrap().unknown);
        fixture.set_connected(true);
        assert!(fixture.pending.is_none());
        assert!(fixture.recoverable.is_empty());
        assert_eq!(fixture.task.as_ref().unwrap().target.id, task_id);
        assert_eq!(fixture.next_request, 2);
        assert!(!fixture.acknowledge(id));
        assert_eq!(
            fixture
                .transcript
                .iter()
                .filter(|line| line.text.contains("Message received"))
                .count(),
            1
        );
    }

    #[test]
    fn tp4_disconnect_cycle_invalidates_open_choice_and_reopens_decision_fresh() {
        let mut fixture = working();
        fixture.toggle_decision();
        let old = fixture.task.as_ref().unwrap().decision.clone().unwrap();
        fixture.set_connected(false);
        assert_eq!(
            fixture.submit("blocked".into(), 500),
            Err(SubmitError::Disconnected)
        );
        assert!(!fixture.stop());
        fixture.set_connected(true);
        assert!(!fixture.target_valid(old.target));
        assert_eq!(
            fixture.respond_decision(old.target, old.id, DecisionAction::Stop),
            Err(SubmitError::StaleDecision)
        );
        let fresh = fixture.task.as_ref().unwrap().decision.clone().unwrap();
        assert_eq!(old.id, fresh.id);
        assert!(
            fixture
                .respond_decision(fresh.target, fresh.id, DecisionAction::Continue)
                .is_ok()
        );
    }

    #[test]
    fn tp4_rejection_preserves_original_text_and_explicit_recovery_releases_capacity() {
        let mut fixture = Fixture::default();
        fixture.reject_next();
        let id = fixture.submit("rejected text".into(), 0).unwrap();
        assert!(fixture.advance(250));
        assert_eq!(fixture.recovered(id).unwrap().text, "rejected text");
        assert!(!fixture.reject_armed);
        assert_eq!(fixture.protected_count(), 1);
        let newer = fixture.submit("newer request".into(), 300).unwrap();
        assert!(!fixture.acknowledge(id));
        assert_eq!(fixture.pending.as_ref().unwrap().id, newer);
        assert!(fixture.discard_recovered(id));
        assert!(!fixture.discard_recovered(id));
        assert_eq!(fixture.pending.as_ref().unwrap().text, "newer request");
    }

    #[test]
    fn tp4_queue_is_fifo_and_success_starts_exactly_one_follow_up() {
        let mut fixture = working();
        queue(&mut fixture, "first follow-up");
        queue(&mut fixture, "second follow-up");
        assert!(fixture.complete(1000));
        assert_eq!(fixture.queued.len(), 1);
        assert_eq!(fixture.queued.front().unwrap().text, "second follow-up");
        assert!(
            fixture
                .transcript
                .back()
                .unwrap()
                .text
                .contains("first follow-up")
        );
        assert!(fixture.complete(1001));
        assert!(fixture.queued.is_empty());
        assert!(
            fixture
                .transcript
                .back()
                .unwrap()
                .text
                .contains("second follow-up")
        );
    }

    #[test]
    fn tp4_cancel_and_failure_hold_queued_text_without_restarting() {
        for fail in [false, true] {
            let mut fixture = working();
            let id = queue(&mut fixture, "held on unsuccessful work");
            assert!(if fail { fixture.fail() } else { fixture.stop() });
            assert!(fixture.task.is_none());
            assert!(fixture.queued.is_empty());
            assert_eq!(
                fixture.recovered(id).unwrap().text,
                "held on unsuccessful work"
            );
            fixture.advance(99_999);
            assert!(fixture.task.is_none());
            assert_eq!(fixture.recoverable.len(), 1);
        }
    }

    #[test]
    fn tp4_decision_stop_holds_queue_using_the_same_cancellation_path() {
        let mut fixture = working();
        let queued = queue(&mut fixture, "recover after decision");
        fixture.toggle_decision();
        let decision = fixture.task.as_ref().unwrap().decision.clone().unwrap();
        fixture
            .respond_decision(decision.target, decision.id, DecisionAction::Stop)
            .unwrap();
        assert!(fixture.task.is_none());
        assert_eq!(
            fixture.recovered(queued).unwrap().text,
            "recover after decision"
        );
    }

    #[test]
    fn tp4_full_reservation_survives_cancellation_and_pending_rejection() {
        let mut fixture = working();
        let ids: Vec<_> = (0..7)
            .map(|i| queue(&mut fixture, &format!("queued {i}")))
            .collect();
        let pending = send(&mut fixture, Action::Steer, "last reserved request");
        assert_eq!(fixture.protected_count(), PROTECTED_LIMIT);
        fixture.stop();
        assert_eq!(fixture.protected_count(), PROTECTED_LIMIT);
        fixture.acknowledge(pending);
        assert_eq!(fixture.recoverable.len(), PROTECTED_LIMIT);
        for id in ids {
            assert!(fixture.recovered(id).is_some());
        }
        assert_eq!(
            fixture.recovered(pending).unwrap().text,
            "last reserved request"
        );
        assert_eq!(
            fixture.submit("cannot detach".into(), 600),
            Err(SubmitError::Capacity)
        );
        assert!(fixture.pending.is_none());
        fixture.discard_recovered(pending);
        assert!(
            fixture
                .submit("capacity released explicitly".into(), 600)
                .is_ok()
        );
    }

    #[test]
    fn tp4_eight_queued_requests_prevent_any_ninth_detachment() {
        let mut fixture = working();
        for i in 0..8 {
            queue(&mut fixture, &format!("queue {i}"));
        }
        for action in [Action::Queue, Action::Steer] {
            assert_eq!(
                fixture.submit_request(fixture.target(), action, 1, "ninth".into(), 500),
                Err(SubmitError::Capacity)
            );
            assert!(fixture.pending.is_none());
        }
        fixture.stop();
        assert_eq!(fixture.recoverable.len(), 8);
    }

    #[test]
    fn tp4_lost_queue_ack_does_not_double_count_its_reserved_recovery_slot() {
        let mut fixture = working();
        for i in 0..7 {
            queue(&mut fixture, &format!("queue {i}"));
        }
        let last = send(&mut fixture, Action::Queue, "last accepted queue");
        fixture.accept_pending(500);
        assert_eq!(fixture.queued.len(), 8);
        assert_eq!(fixture.protected_count(), 8);
        fixture.stop();
        assert_eq!(fixture.recoverable.len(), 8);
        assert_eq!(fixture.protected_count(), 8);
        fixture.acknowledge(last);
        assert_eq!(fixture.recoverable.len(), 8);
        assert_eq!(fixture.protected_count(), 8);
    }

    #[test]
    fn tp4_reset_rejects_old_generation_ack_and_captured_commands() {
        let mut fixture = Fixture::default();
        let old_target = fixture.target();
        let old_id = fixture.submit("before reset".into(), 0).unwrap();
        fixture.reset().unwrap();
        assert_eq!(
            fixture.target().scope.generation,
            old_target.scope.generation + 1
        );
        assert_eq!(
            fixture.submit_request(old_target, Action::NewTurn, 0, "stale generation".into(), 0),
            Err(SubmitError::StaleTarget)
        );
        let new_id = fixture.submit("after reset".into(), 0).unwrap();
        assert_ne!(old_id, new_id);
        assert_eq!(old_id.sequence, new_id.sequence);
        assert!(!fixture.acknowledge(old_id));
        assert_eq!(fixture.pending.as_ref().unwrap().text, "after reset");
        assert!(fixture.acknowledge(new_id));
        assert!(
            !fixture
                .transcript
                .iter()
                .any(|line| line.text.contains("before reset"))
        );
    }

    #[test]
    fn tp4_task_identity_and_request_exhaustion_preserve_text() {
        let mut fixture = Fixture {
            next_request: u64::MAX,
            ..Fixture::default()
        };
        assert_eq!(
            fixture.submit("kept by client".into(), 0),
            Err(SubmitError::IdentityExhausted)
        );
        assert!(fixture.pending.is_none());
        fixture.next_request = 1;
        fixture.next_task = u64::MAX;
        let id = fixture.submit("kept by fixture".into(), 0).unwrap();
        fixture.acknowledge(id);
        assert_eq!(fixture.recovered(id).unwrap().text, "kept by fixture");
        assert!(fixture.task.is_none());
    }

    #[test]
    fn tp4_exhausted_generation_refuses_reset_without_discarding_state() {
        let mut fixture = working();
        let id = queue(&mut fixture, "keep queued text");
        fixture.scope.generation = u64::MAX;
        let before = fixture.target();
        assert_eq!(fixture.can_reset(), Err(SubmitError::IdentityExhausted));
        assert_eq!(fixture.reset(), Err(SubmitError::IdentityExhausted));
        assert_eq!(fixture.target(), before);
        assert!(fixture.task.is_some());
        assert_eq!(fixture.queued.front().unwrap().id, id);
        assert_eq!(fixture.queued.front().unwrap().text, "keep queued text");
    }

    #[test]
    fn tp4_events_are_scoped_and_monotonic_within_a_generation() {
        let mut fixture = working();
        let accepted = fixture.last_event.unwrap();
        fixture.advance(750);
        let progress = fixture.last_event.unwrap();
        assert_eq!(accepted.scope, progress.scope);
        assert!(progress.sequence > accepted.sequence);
        assert_eq!(accepted.kind, EventKind::Accepted);
        assert_eq!(progress.kind, EventKind::Progress);
        assert!(accepted.request.is_some());
        fixture.reset().unwrap();
        let reset = fixture.last_event.unwrap();
        assert_ne!(reset.scope.generation, accepted.scope.generation);
        assert_eq!(reset.kind, EventKind::Reset);
    }

    #[test]
    fn tp4_lost_steer_ack_is_not_presented_until_reconciliation_and_only_once() {
        let mut fixture = working();
        let original = fixture.task.as_ref().unwrap().target;
        let before = fixture.transcript.clone();
        let id = send(
            &mut fixture,
            Action::Steer,
            "steering whose acknowledgement is lost",
        );
        assert!(fixture.lose_acknowledgement(500));
        assert_eq!(fixture.task.as_ref().unwrap().target.id, original.id);
        assert_eq!(
            fixture.task.as_ref().unwrap().target.revision,
            original.revision + 1
        );
        assert!(fixture.delivery_blocked());
        assert!(fixture.display_task().is_none());
        assert_eq!(fixture.display_queued_count(), None);
        assert!(fixture.display_recoverable().is_none());
        assert_eq!(fixture.transcript, before);
        assert!(fixture.pending.as_ref().unwrap().unknown);

        assert!(fixture.set_connected(true));
        assert!(!fixture.delivery_blocked());
        assert_eq!(
            fixture.display_task().unwrap().target.revision,
            original.revision + 1
        );
        assert_eq!(
            fixture
                .transcript
                .iter()
                .filter(|line| line.text.contains("Steering instruction acknowledged"))
                .count(),
            1
        );
        assert!(!fixture.acknowledge(id));
        fixture.set_connected(false);
        fixture.set_connected(true);
        assert_eq!(
            fixture
                .transcript
                .iter()
                .filter(|line| line.text.contains("Steering instruction acknowledged"))
                .count(),
            1
        );
        assert_eq!(
            fixture.task.as_ref().unwrap().target.revision,
            original.revision + 1
        );
    }

    #[test]
    fn tp4_lost_new_turn_ack_does_not_publish_work_or_offline_progress() {
        let mut fixture = Fixture::default();
        let before = fixture.transcript.clone();
        fixture
            .submit("new work behind a lost acknowledgement".into(), 0)
            .unwrap();
        assert!(fixture.lose_acknowledgement(250));
        assert!(fixture.task.is_some());
        assert!(fixture.advance(6000));
        assert!(fixture.task.as_ref().unwrap().progress > 0);
        assert!(fixture.display_task().is_none());
        assert_eq!(fixture.transcript, before);
        fixture.set_connected(true);
        assert!(fixture.display_task().unwrap().progress > 0);
        assert_eq!(
            fixture
                .transcript
                .iter()
                .filter(|line| line.text.contains("Message received"))
                .count(),
            1
        );
    }

    #[test]
    fn tp4_reconciled_display_keeps_acceptance_before_offline_completion() {
        let mut fixture = working();
        let before = fixture.transcript.clone();
        send(&mut fixture, Action::Steer, "finish after acceptance");
        fixture.lose_acknowledgement(500);
        assert!(fixture.complete(1000));
        assert!(fixture.task.is_none());
        assert_eq!(fixture.transcript, before);
        fixture.set_connected(true);
        let position = |needle: &str| {
            fixture
                .transcript
                .iter()
                .position(|line| line.text.contains(needle))
                .unwrap()
        };
        assert!(position("Steering instruction acknowledged") < position("Disconnected"));
        assert!(position("Disconnected") < position("Work completed successfully"));
        assert!(position("Work completed successfully") < position("Reconnected"));
        assert!(fixture.display_task().is_none());
        assert!(!fixture.delivery_blocked());
    }

    #[test]
    fn tp4_stop_before_ack_delivery_is_presented_after_queue_acceptance() {
        let mut fixture = working();
        let before = fixture.transcript.clone();
        let id = send(&mut fixture, Action::Queue, "held after accepted queue");
        assert!(fixture.accept_pending(500));
        assert!(fixture.stop());
        assert_eq!(
            fixture.recovered(id).unwrap().text,
            "held after accepted queue"
        );
        assert!(fixture.display_recoverable().is_none());
        assert_eq!(fixture.transcript, before);
        assert!(fixture.acknowledge(id));
        let accepted = fixture
            .transcript
            .iter()
            .position(|line| line.text.contains("Follow-up queued"))
            .unwrap();
        let stopped = fixture
            .transcript
            .iter()
            .position(|line| line.text.contains("Work stopped"))
            .unwrap();
        assert!(accepted < stopped);
        assert_eq!(fixture.display_recoverable().unwrap().len(), 1);
        assert_eq!(fixture.protected_count(), 1);
        assert!(!fixture.acknowledge(id));
    }

    #[test]
    fn tp4_recorded_rejection_precedes_later_completion_when_delivery_resumes() {
        let mut fixture = working();
        let before = fixture.transcript.clone();
        fixture.reject_next();
        let id = send(&mut fixture, Action::Steer, "rejected before completion");
        fixture.lose_acknowledgement(500);
        fixture.complete(750);
        assert_eq!(fixture.transcript, before);
        fixture.set_connected(true);
        let rejection = fixture
            .transcript
            .iter()
            .position(|line| line.text.contains("Message rejected"))
            .unwrap();
        let completion = fixture
            .transcript
            .iter()
            .position(|line| line.text.contains("Work completed successfully"))
            .unwrap();
        assert!(rejection < completion);
        assert_eq!(
            fixture.recovered(id).unwrap().text,
            "rejected before completion"
        );
    }

    #[test]
    fn tp4_display_backlog_bounds_never_evict_pending_or_held_request_text() {
        for payload_bytes in [8, 40_000] {
            let mut fixture = working();
            let held = queue(&mut fixture, "protected queued text");
            let pending = send(&mut fixture, Action::Steer, "protected pending text");
            let before = fixture.transcript.clone();
            fixture.lose_acknowledgement(500);
            fixture.fail();
            for _ in 0..300 {
                fixture.append("x".repeat(payload_bytes));
            }
            assert!(fixture.display_backlog.len() <= ENTRY_LIMIT);
            assert!(
                fixture
                    .display_backlog
                    .iter()
                    .all(|entry| entry.kind == TranscriptKind::Output)
            );
            assert!(
                fixture
                    .display_backlog
                    .iter()
                    .map(|entry| entry.text.len())
                    .sum::<usize>()
                    <= BYTE_LIMIT
            );
            assert_eq!(fixture.transcript, before);
            assert_eq!(fixture.pending.as_ref().unwrap().id, pending);
            assert_eq!(
                fixture.pending.as_ref().unwrap().text,
                "protected pending text"
            );
            assert_eq!(
                fixture.recovered(held).unwrap().text,
                "protected queued text"
            );
            fixture.set_connected(true);
            assert!(fixture.truncated);
            assert!(fixture.display_backlog.is_empty());
            assert_eq!(fixture.backlog_bytes, 0);
            assert!(fixture.transcript.len() <= ENTRY_LIMIT);
            assert!(
                fixture
                    .transcript
                    .iter()
                    .map(|entry| entry.text.len())
                    .sum::<usize>()
                    <= BYTE_LIMIT
            );
            assert_eq!(
                fixture.recovered(held).unwrap().text,
                "protected queued text"
            );
        }
    }

    #[test]
    fn tp4_reset_discards_backlog_and_old_delivery_cannot_publish_it() {
        let mut fixture = working();
        let old_id = send(&mut fixture, Action::Steer, "stale hidden display");
        fixture.lose_acknowledgement(500);
        fixture.reset().unwrap();
        assert!(!fixture.delivery_blocked());
        assert!(!fixture.acknowledge(old_id));
        assert!(!fixture.set_connected(true));
        assert!(
            !fixture
                .transcript
                .iter()
                .any(|line| line.text.contains("stale hidden display"))
        );
    }

    #[test]
    fn tp4_discarded_unpublished_prefix_cannot_reuse_a_visible_entry_identity() {
        let mut fixture = working();
        let anchor_id = fixture.base_id() + 1;
        let anchor_text = fixture.transcript[1].clone();
        let previous_end = fixture.base_id() + fixture.transcript.len() as u128;
        fixture.set_connected(false);
        for i in 0..300 {
            fixture.append(format!("unpublished {i}: {}", "x".repeat(40_000)));
        }
        assert!(fixture.backlog_truncated);
        fixture.set_connected(true);
        assert!(fixture.truncated);
        let anchor_offset = (anchor_id - fixture.base_id()) as usize;
        assert_eq!(fixture.transcript[anchor_offset], anchor_text);
        let first_new = fixture
            .transcript
            .iter()
            .position(|line| line.text.starts_with("unpublished"))
            .unwrap();
        assert_eq!(fixture.base_id() + first_new as u128, previous_end);
        assert!(
            fixture.transcript[first_new]
                .text
                .starts_with("unpublished 294:")
        );
        assert_eq!(
            fixture
                .transcript
                .iter()
                .filter(|line| **line == anchor_text)
                .count(),
            1
        );
    }
}
