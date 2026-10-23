//! Job list, job viewer, chooser and confirmations (tool-job-controller.md,
//! J4 TUI; composer-interactions.md, job list and job viewer; command flows
//! CF12–CF14). Rendering and key handling perform no I/O: every request is a
//! typed `jobs::Request` that the dispatcher hands to the job worker.
use super::*;
use crate::tui::jobs::{
    self as worker, Command, Failure, Finished, Follow, FollowUpdate, Reply, Request, Resolution,
    Retain, StatusUpdate, Stream,
};
use crate::tui::queue_watch::Scope;
use asura_control::{jobs::MAX_JOB_PAGE_BYTES, pb};
use crossterm::event::KeyEvent;

const UNSUPPORTED: &str = "This service does not support jobs";
const PENDING: &str = "Job request pending; draft retained";
const NO_MATCH: &str = "Use /jobs to find the job, or give the full handle; draft retained";
const JOB_USAGE: &str =
    "Use /job ID, /job cancel|pin|unpin|delete ID or /job note ID TEXT; draft retained";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Origin {
    Command,
    List,
    History,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Phase {
    Loading,
    Viewing,
    Following,
    Failed(String),
}
/// Where a loaded page leaves the view.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Land {
    Top,
    Bottom,
}
#[derive(Default)]
pub(super) struct StreamView {
    /// At most `MAX_PAGES_PER_STREAM` pages, by start offset.
    pages: Vec<pb::JobStreamPage>,
    /// The start of the shown page.
    current: Option<u64>,
    /// The first shown line of that page.
    line: usize,
}
impl StreamView {
    fn find(&self, start: u64) -> Option<&pb::JobStreamPage> {
        self.pages.iter().find(|p| p.start == Some(start))
    }
    pub(super) fn page(&self) -> Option<&pb::JobStreamPage> {
        self.find(self.current?)
    }
    /// The cached page that ends where `start` begins.
    fn before(&self, start: u64) -> Option<&pb::JobStreamPage> {
        self.pages.iter().find(|p| p.next_offset == Some(start))
    }
    fn insert(&mut self, page: pb::JobStreamPage) {
        let start = page.start.unwrap_or(0);
        self.pages.retain(|p| p.start != Some(start));
        self.pages.push(page);
        while self.pages.len() > worker::MAX_PAGES_PER_STREAM {
            // Drop the page farthest from the new one.
            let far = self
                .pages
                .iter()
                .enumerate()
                .max_by_key(|(_, p)| p.start.unwrap_or(0).abs_diff(start))
                .map(|(i, _)| i)
                .unwrap();
            self.pages.remove(far);
        }
    }
    #[cfg(test)]
    pub(super) fn cached(&self) -> usize {
        self.pages.len()
    }
}
pub(super) struct Detail {
    detail: Option<pb::JobDetail>,
    error: Option<String>,
    scroll: u16,
}
pub(super) struct Viewer {
    pub(super) job: [u8; 16],
    fence: Option<[u8; 16]>,
    pub(super) entry: Option<pb::JobEntry>,
    pub(super) origin: Origin,
    pub(super) stream: Stream,
    pub(super) streams: [StreamView; 2],
    pub(super) phase: Phase,
    land: Land,
    last_read: Option<Command>,
    pub(super) detail: Option<Detail>,
    /// Text rows at the last draw; page keys move by this many lines.
    rows: usize,
    pub(super) follow: Option<u64>,
}
impl Viewer {
    fn new(job: [u8; 16], fence: Option<[u8; 16]>, origin: Origin) -> Self {
        Self {
            job,
            fence,
            entry: None,
            origin,
            stream: Stream::Stdout,
            streams: Default::default(),
            phase: Phase::Loading,
            land: Land::Top,
            last_read: None,
            detail: None,
            rows: 1,
            follow: None,
        }
    }
    fn view(&self) -> &StreamView {
        &self.streams[self.stream.index()]
    }
    fn view_mut(&mut self) -> &mut StreamView {
        &mut self.streams[self.stream.index()]
    }
    fn settled(&self) -> bool {
        self.entry.as_ref().and_then(|e| e.state) == Some(pb::JobState::Settled as i32)
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Nav {
    Reload,
    Next,
    Previous,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum ListState {
    Loading,
    Ready,
    Failed(String),
}
pub(super) struct List {
    pub(super) all: bool,
    project: Option<[u8; 16]>,
    /// The `before` cursor of each page read so far; the last is shown.
    cursors: Vec<Option<u64>>,
    pub(super) entries: Vec<pb::JobEntry>,
    next_before: Option<u64>,
    pub(super) selected: usize,
    pub(super) state: ListState,
    /// The panel opens on the first reply.
    pub(super) visible: bool,
    nav: Nav,
    revision: Option<u64>,
    pub(super) note: Option<String>,
}
pub(super) struct Chooser {
    pub(super) ids: Vec<[u8; 16]>,
    pub(super) selected: usize,
}
pub(super) struct Confirm {
    pub(super) text: String,
    command: Command,
    draft: Option<String>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
enum Action {
    Cancel,
    Retain(Retain),
}
/// An action that waits for `JobInspect` to name the job's project.
struct Then {
    job: [u8; 16],
    action: Action,
    draft: Option<String>,
}

#[derive(Default)]
pub(super) struct Jobs {
    /// `Some(false)` after a service without `CAPABILITY_JOBS` answered.
    pub(super) supported: Option<bool>,
    /// The status slot's scope and its latest facts.
    status: Option<Scope>,
    pub(super) live: u32,
    pub(super) live_in_service: u32,
    pub(super) execution_enabled: Option<bool>,
    revision: Option<u64>,
    /// Listed entries for prefix resolution, newest first, at most 64.
    pub(super) cache: Vec<pb::JobEntry>,
    outgoing: Option<Request>,
    pending: Option<Request>,
    cancel: bool,
    pub(super) list: Option<List>,
    pub(super) viewer: Option<Viewer>,
    pub(super) chooser: Option<Chooser>,
    pub(super) confirm: Option<Confirm>,
    then: Option<Then>,
    session: u64,
}
impl Jobs {
    /// A job view holds focus and keys.
    pub(super) fn open(&self) -> bool {
        self.confirm.is_some()
            || self.viewer.is_some()
            || self.chooser.is_some()
            || self.list.as_ref().is_some_and(|l| l.visible)
    }
    fn busy(&self) -> bool {
        self.outgoing.is_some() || self.pending.is_some()
    }
    fn remember(&mut self, entry: &pb::JobEntry) {
        worker::remember(&mut self.cache, entry);
    }
    fn project_of(&self, job: [u8; 16]) -> Option<[u8; 16]> {
        self.cache
            .iter()
            .find(|e| worker::entry_id(e) == Some(job))
            .and_then(worker::entry_project)
    }
}

impl App {
    // Dispatcher interface.

    /// The status slot's scope: the selected registered project of a
    /// connected service.
    pub(in super::super) fn job_status_scope(&self) -> Option<Scope> {
        self.queue_scope()
    }
    pub(in super::super) fn take_job_request(&mut self) -> Option<Request> {
        // A person stop or kill that found the slot busy goes next.
        if self.jobs.outgoing.is_none()
            && self.jobs.pending.is_none()
            && let Some(command) = self.person.deferred.take()
            && !self.job_send(command.clone(), None, false)
        {
            self.person.deferred = Some(command);
        }
        let request = self.jobs.outgoing.take()?;
        self.jobs.pending = Some(request.clone());
        Some(request)
    }
    pub(in super::super) fn take_jobs_cancel(&mut self) -> bool {
        std::mem::take(&mut self.jobs.cancel)
    }
    /// The open viewer's follow session, while it follows.
    pub(in super::super) fn job_follow(&self) -> Option<Follow> {
        let viewer = self.jobs.viewer.as_ref()?;
        let session = viewer.follow?;
        let view = viewer.view();
        Some(Follow {
            session,
            epoch: self.view.epoch.clone()?,
            job: viewer.job,
            project: viewer.fence,
            stream: viewer.stream,
            offset: view.page().and_then(|p| p.next_offset).unwrap_or(0),
            after_state: viewer.entry.as_ref().and_then(|e| e.state),
        })
    }
    /// A new service epoch: capability and counts are unknown again, and
    /// every view of the old service closes. Jobs are unaffected (PC5).
    pub(super) fn jobs_service_changed(&mut self) {
        self.person.supported = None;
        let pending = self.jobs.pending.take();
        self.jobs = Jobs {
            pending,
            ..Default::default()
        };
    }

    pub(in super::super) fn jobs_status(&mut self, update: StatusUpdate) -> bool {
        if Some(&update.scope) != self.job_status_scope().as_ref() {
            return false;
        }
        let jobs = &mut self.jobs;
        jobs.status = Some(update.scope);
        match update.result {
            Err(Failure::Unsupported) => {
                jobs.supported = Some(false);
                jobs.live = 0;
                jobs.live_in_service = 0;
                jobs.execution_enabled = None;
            }
            Err(_) => {
                // Unknown until the slot attaches again; show nothing.
                jobs.live = 0;
                jobs.live_in_service = 0;
                jobs.execution_enabled = None;
                jobs.revision = None;
            }
            Ok(reply) => {
                jobs.supported = Some(true);
                jobs.live = reply.live_in_scope.unwrap_or(0);
                jobs.live_in_service = reply.live_in_service.unwrap_or(0);
                jobs.execution_enabled = reply.execution_enabled;
                jobs.revision = reply.revision;
                for entry in &reply.entries {
                    jobs.remember(entry);
                }
                self.refresh_job_list();
            }
        }
        true
    }
    /// An open list reloads its page when the job revision changed.
    fn refresh_job_list(&mut self) {
        let revision = self.jobs.revision;
        let busy = self.jobs.busy();
        let Some(list) = self.jobs.list.as_mut() else {
            return;
        };
        if !list.visible
            || list.state != ListState::Ready
            || list.note.is_some()
            || list.revision == revision
            || busy
        {
            return;
        }
        list.revision = revision;
        list.nav = Nav::Reload;
        let command = Command::List {
            all: list.all,
            project: list.project,
            before: list.cursors.last().copied().flatten(),
        };
        self.job_send(command, None, false);
    }

    /// Queues one request-slot command. A second while the slot is busy
    /// shows `job request pending`; the draft is kept.
    pub(super) fn job_send(&mut self, command: Command, draft: Option<String>, loud: bool) -> bool {
        if self.jobs.supported == Some(false) {
            self.notice = UNSUPPORTED.into();
            return false;
        }
        if self.jobs.busy() {
            if loud {
                self.notice = PENDING.into();
            }
            return false;
        }
        let Some(epoch) = self
            .view
            .epoch
            .clone()
            .filter(|_| self.view.connection == "connected")
        else {
            if loud {
                self.notice = "Service not connected; draft retained".into();
            }
            return false;
        };
        self.jobs.outgoing = Some(Request {
            epoch,
            command,
            draft,
        });
        true
    }

    // Commands.

    /// `/jobs` and `/job` (CF12–CF14).
    pub(super) fn job_command(&mut self, draft: String) {
        let trimmed = draft.trim_end();
        let mut words = trimmed.split_whitespace();
        let name = words.next().unwrap_or_default();
        if name == "/jobs" {
            let all = match (words.next(), words.next()) {
                (None, _) => false,
                (Some("all"), None) => true,
                _ => {
                    self.notice = "Use /jobs or /jobs all; draft retained".into();
                    return;
                }
            };
            if self.jobs.supported == Some(false) {
                self.notice = UNSUPPORTED.into();
                return;
            }
            let project = self.selected_project;
            if !all && project.is_none() {
                self.notice = "Select a project first; draft retained".into();
                return;
            }
            if self.job_send(
                Command::List {
                    all,
                    project,
                    before: None,
                },
                Some(draft.clone()),
                true,
            ) {
                self.jobs.list = Some(List {
                    all,
                    project,
                    cursors: vec![None],
                    entries: Vec::new(),
                    next_before: None,
                    selected: 0,
                    state: ListState::Loading,
                    visible: false,
                    nav: Nav::Reload,
                    revision: None,
                    note: None,
                });
                self.notice = "Job list pending · Esc stops waiting".into();
            }
            return;
        }
        // `/job` subcommands; a handle is never one of these words.
        let (action, id) = match words.next() {
            Some(word @ ("cancel" | "pin" | "unpin" | "delete" | "note")) => {
                (Some(word), words.next())
            }
            other => (None, other),
        };
        let Some(id) = id else {
            self.notice = JOB_USAGE.into();
            return;
        };
        let note = if action == Some("note") {
            // TEXT is the rest of the line after the handle.
            let text = trimmed
                .trim_start()
                .strip_prefix("/job")
                .map(str::trim_start)
                .and_then(|rest| rest.strip_prefix("note"))
                .map(str::trim_start)
                .and_then(|rest| rest.strip_prefix(id))
                .map(str::trim_start)
                .unwrap_or_default();
            if !asura_control::jobs::note_text_valid(text) {
                self.notice =
                    "A note is 1 to 2048 bytes without control characters; draft retained".into();
                return;
            }
            Some(text.to_owned())
        } else {
            if words.next().is_some() {
                self.notice = JOB_USAGE.into();
                return;
            }
            None
        };
        if self.jobs.supported == Some(false) {
            self.notice = UNSUPPORTED.into();
            return;
        }
        let job = match worker::resolve(id, &self.jobs.cache) {
            Resolution::Job(job) => job,
            Resolution::Malformed => {
                self.notice = JOB_USAGE.into();
                return;
            }
            Resolution::Ambiguous(handles) => {
                self.notice = literal(
                    &format!(
                        "Several jobs match: {}; draft retained",
                        handles
                            .iter()
                            .map(|h| &h[..12])
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                    1024,
                );
                return;
            }
            Resolution::NoMatch => {
                self.notice = NO_MATCH.into();
                return;
            }
        };
        let action = action.map(str::to_owned);
        let draft = Some(draft);
        match action.as_deref() {
            None => self.open_viewer(job, None, Origin::Command, draft),
            Some("cancel") => self.job_act(job, Action::Cancel, draft),
            Some("pin") => self.job_act(job, Action::Retain(Retain::Pin), draft),
            Some("unpin") => self.job_act(job, Action::Retain(Retain::Unpin), draft),
            Some("delete") => self.job_act(job, Action::Retain(Retain::Delete), draft),
            Some(_) => self.job_act(job, Action::Retain(Retain::Note(note.unwrap())), draft),
        }
    }
    /// The viewer at the end of stdout, for a person entry in the log (PC11).
    pub(super) fn open_viewer_at_end(&mut self, job: [u8; 16], fence: Option<[u8; 16]>) {
        self.open_viewer_from(job, fence, Origin::History, None, None);
        if let Some(viewer) = self.jobs.viewer.as_mut().filter(|v| v.job == job) {
            viewer.land = Land::Bottom;
        }
    }
    fn open_viewer(
        &mut self,
        job: [u8; 16],
        fence: Option<[u8; 16]>,
        origin: Origin,
        draft: Option<String>,
    ) {
        self.open_viewer_from(job, fence, origin, draft, Some(0));
    }
    fn open_viewer_from(
        &mut self,
        job: [u8; 16],
        fence: Option<[u8; 16]>,
        origin: Origin,
        draft: Option<String>,
        offset: Option<u64>,
    ) {
        let command = Command::Read {
            job,
            project: fence,
            stream: Stream::Stdout,
            offset,
        };
        if self.job_send(command.clone(), draft, true) {
            let mut viewer = Viewer::new(job, fence, origin);
            viewer.entry = self
                .jobs
                .cache
                .iter()
                .find(|e| worker::entry_id(e) == Some(job))
                .cloned();
            viewer.last_read = Some(command);
            self.jobs.viewer = Some(viewer);
            self.jobs.chooser = None;
        }
    }
    /// Cancel or retain: a job outside the selected project asks first for
    /// cancel, delete always asks; an unlisted job is inspected first so the
    /// request can name its project.
    fn job_act(&mut self, job: [u8; 16], action: Action, draft: Option<String>) {
        let Some(project) = self.jobs.project_of(job) else {
            if self.job_send(Command::Inspect { job, project: None }, None, true) {
                self.jobs.then = Some(Then { job, action, draft });
                self.notice = "Looking up the job".into();
            }
            return;
        };
        let request_id = asura_platform::random_id();
        let short = worker::short(&job);
        match action {
            Action::Cancel => {
                let command = Command::Cancel {
                    request_id,
                    job,
                    project,
                    kill: false,
                };
                if Some(project) != self.selected_project {
                    let name = self.project_name(project);
                    self.jobs.confirm = Some(Confirm {
                        text: format!("Cancel job {short} in project {name}? (y/n)"),
                        command,
                        draft,
                    });
                } else if self.job_send(command, draft, true) {
                    self.notice = format!("Cancelling job {short}");
                }
            }
            Action::Retain(action) => {
                let delete = action == Retain::Delete;
                let command = Command::Retain {
                    request_id,
                    job,
                    project,
                    action,
                };
                if delete {
                    self.jobs.confirm = Some(Confirm {
                        text: format!(
                            "Delete the output of job {short}? Its summary and notes stay. (y/n)"
                        ),
                        command,
                        draft,
                    });
                } else if self.job_send(command, draft, true) {
                    self.notice = format!("Job {short} request pending");
                }
            }
        }
    }
    /// `j` in response navigation: the viewer for the response's job, or a
    /// chooser of its jobs (at most 9).
    pub(super) fn open_response_jobs(&mut self, ids: Vec<[u8; 16]>) {
        if self.jobs.supported == Some(false) {
            self.notice = UNSUPPORTED.into();
            return;
        }
        match ids.as_slice() {
            [] => self.notice = "No job in this response".into(),
            [job] => self.open_viewer(*job, None, Origin::History, None),
            _ => {
                self.jobs.chooser = Some(Chooser {
                    ids: ids.into_iter().take(worker::MAX_CHOOSER).collect(),
                    selected: 0,
                })
            }
        }
    }
    /// `/cancel` and Escape stop a pending job wait.
    pub(super) fn cancel_job_wait(&mut self, clear: bool) -> bool {
        let Some(request) = self.jobs.pending.as_ref().or(self.jobs.outgoing.as_ref()) else {
            return false;
        };
        let mutation = request.command.mutation();
        if self.jobs.outgoing.take().is_some() {
            // Not yet sent: nothing changed.
            self.jobs.then = None;
            self.after_job_cancel();
            self.notice = "Job request cancelled".into();
        } else {
            self.jobs.cancel = true;
            self.notice = if mutation {
                "Job request wait stopped; outcome unconfirmed".into()
            } else {
                "Job request cancelled".into()
            };
        }
        if clear {
            self.editor = Editor::new();
        }
        true
    }
    /// Views waiting for a cancelled read close (CF12, CF13).
    fn after_job_cancel(&mut self) {
        if self.jobs.list.as_ref().is_some_and(|l| !l.visible) {
            self.jobs.list = None;
        } else if let Some(list) = self.jobs.list.as_mut()
            && list.state == ListState::Loading
        {
            list.state = ListState::Ready;
        }
        if self
            .jobs
            .viewer
            .as_ref()
            .is_some_and(|v| v.phase == Phase::Loading)
        {
            self.close_viewer();
        }
    }

    // Replies.

    pub(in super::super) fn jobs_finished(&mut self, finished: Finished) {
        if self.jobs.pending.as_ref() == Some(&finished.request) {
            self.jobs.pending = None;
        }
        let Finished { request, result } = finished;
        if self.view.epoch.as_deref() != Some(request.epoch.as_str()) {
            return;
        }
        let clear_draft = |app: &mut App| {
            if let Some(draft) = &request.draft
                && app.editor.text() == *draft
            {
                app.editor = Editor::new();
            }
        };
        let reply = match result {
            Ok(reply) => reply,
            Err(Failure::Unsupported) => {
                self.jobs.supported = Some(false);
                self.jobs.list = None;
                self.jobs.viewer = None;
                self.jobs.chooser = None;
                self.jobs.then = None;
                self.notice = UNSUPPORTED.into();
                return;
            }
            Err(failure) => {
                self.job_failed(&request.command, failure);
                return;
            }
        };
        self.jobs.supported = Some(true);
        match reply {
            Reply::List(reply) => {
                if let Some(error) = reply.error.as_deref() {
                    self.list_failed(error);
                    return;
                }
                for entry in &reply.entries {
                    self.jobs.remember(entry);
                }
                let Some(list) = self.jobs.list.as_mut() else {
                    return;
                };
                let first = !list.visible;
                match list.nav {
                    Nav::Next => {
                        list.cursors.push(match &request.command {
                            Command::List { before, .. } => *before,
                            _ => None,
                        });
                        list.selected = 0;
                    }
                    Nav::Previous => {
                        list.cursors.pop();
                        list.selected = reply.entries.len().saturating_sub(1);
                    }
                    Nav::Reload => {
                        list.selected = list.selected.min(reply.entries.len().saturating_sub(1))
                    }
                }
                list.entries = reply.entries;
                list.next_before = reply.next_before;
                list.revision = reply.revision.or(list.revision);
                list.state = ListState::Ready;
                list.visible = true;
                if first {
                    clear_draft(self);
                    self.notice.clear();
                }
            }
            Reply::Detail(detail) => {
                if let Some(then) = self
                    .jobs
                    .then
                    .take_if(|then| matches!(request.command, Command::Inspect { job, .. } if job == then.job))
                {
                    match (&detail.error, detail.entry.as_ref()) {
                        (Some(error), _) => {
                            self.notice = format!("Job request refused: {}", worker::error_text(error))
                        }
                        (None, Some(entry)) => {
                            self.jobs.remember(entry);
                            self.job_act(then.job, then.action, then.draft);
                        }
                        (None, None) => self.notice = "Job reply incomplete".into(),
                    }
                    return;
                }
                if let Some(entry) = &detail.entry {
                    self.jobs.remember(entry);
                }
                if let Some(viewer) = self.jobs.viewer.as_mut()
                    && matches!(request.command, Command::Inspect { job, .. } if job == viewer.job)
                    && let Some(panel) = viewer.detail.as_mut()
                {
                    panel.error = detail.error.as_deref().map(worker::error_text);
                    if let Some(entry) = &detail.entry {
                        viewer.entry = Some(entry.clone());
                    }
                    panel.detail = Some(detail);
                }
            }
            Reply::Page(page) => {
                let Command::Read { job, stream, .. } = &request.command else {
                    return;
                };
                let (job, stream) = (*job, *stream);
                if let Some(entry) = &page.entry {
                    self.jobs.remember(entry);
                }
                let Some(viewer) = self.jobs.viewer.as_mut().filter(|v| v.job == job) else {
                    return;
                };
                if let Some(error) = page.error.as_deref() {
                    viewer.phase = Phase::Failed(worker::error_text(error));
                    return;
                }
                clear_draft(self);
                let viewer = self.jobs.viewer.as_mut().unwrap();
                if let Some(entry) = page.entry {
                    viewer.entry = Some(entry);
                }
                let land = viewer.land;
                if let Some(page) = page.pages.into_iter().next() {
                    let start = page.start.unwrap_or(0);
                    let view = &mut viewer.streams[stream.index()];
                    view.insert(page);
                    view.current = Some(start);
                    view.line = if land == Land::Bottom { usize::MAX } else { 0 };
                }
                viewer.land = Land::Top;
                if viewer.phase == Phase::Loading || matches!(viewer.phase, Phase::Failed(_)) {
                    viewer.phase = Phase::Viewing;
                }
            }
            Reply::Cancel(reply) => {
                let Command::Cancel { job, .. } = &request.command else {
                    return;
                };
                let short = worker::short(job);
                if let Some(entry) = &reply.entry {
                    self.job_entry_changed(entry);
                }
                self.notice = match (reply.error.as_deref(), reply.requested) {
                    (Some(error), _) => {
                        format!(
                            "Cancel refused: {}; nothing changed",
                            worker::error_text(error)
                        )
                    }
                    (None, Some(false)) => format!(
                        "Job {short} already settled: {}",
                        reply
                            .entry
                            .as_ref()
                            .map(worker::state_text)
                            .unwrap_or_default()
                    ),
                    (None, _) => {
                        clear_draft(self);
                        format!("Stopping job {short}; /jobs shows the settlement")
                    }
                };
            }
            Reply::Retain(reply) => {
                let Command::Retain { job, action, .. } = &request.command else {
                    return;
                };
                let short = worker::short(job);
                if let Some(entry) = &reply.entry {
                    self.job_entry_changed(entry);
                }
                self.notice = match (reply.error.as_deref(), reply.changed) {
                    (Some(error), _) => {
                        format!("Job {short}: {}; draft retained", worker::error_text(error))
                    }
                    (None, Some(true)) => {
                        clear_draft(self);
                        match action {
                            Retain::Pin => format!("Pinned job {short}"),
                            Retain::Unpin => format!("Unpinned job {short}"),
                            Retain::Delete => format!("Deleted the output of job {short}"),
                            Retain::Note(_) => format!("Note added to job {short}"),
                        }
                    }
                    (None, _) => {
                        clear_draft(self);
                        match action {
                            Retain::Pin => format!("Job {short} is already pinned"),
                            Retain::Unpin => format!("Job {short} is not pinned"),
                            Retain::Delete => format!("Job {short} output is already deleted"),
                            Retain::Note(_) => format!("Job {short} note unchanged"),
                        }
                    }
                };
                if let Some(list) = self.jobs.list.as_mut()
                    && reply.error.is_none()
                {
                    list.note = None;
                }
            }
        }
    }
    fn job_entry_changed(&mut self, entry: &pb::JobEntry) {
        self.jobs.remember(entry);
        self.person_entry_changed(entry);
        let id = worker::entry_id(entry);
        if let Some(list) = self.jobs.list.as_mut() {
            for row in &mut list.entries {
                if worker::entry_id(row) == id {
                    *row = entry.clone();
                }
            }
        }
        if let Some(viewer) = self.jobs.viewer.as_mut()
            && Some(viewer.job) == id
        {
            viewer.entry = Some(entry.clone());
        }
    }
    fn list_failed(&mut self, error: &str) {
        let Some(list) = self.jobs.list.as_mut() else {
            return;
        };
        if error == asura_control::jobs::error::INVALID_CURSOR {
            // Reload from the newest page.
            list.cursors = vec![None];
            list.nav = Nav::Reload;
            list.state = ListState::Loading;
            let command = Command::List {
                all: list.all,
                project: list.project,
                before: None,
            };
            self.notice = worker::error_text(error);
            self.job_send(command, None, false);
            return;
        }
        list.state = ListState::Failed(worker::error_text(error));
        list.visible = true;
    }
    fn job_failed(&mut self, command: &Command, failure: Failure) {
        let text = match &failure {
            Failure::Cancelled => "request cancelled".to_owned(),
            Failure::Unconfirmed => "outcome unconfirmed".to_owned(),
            Failure::Unavailable(reason) => literal(reason, 256),
            Failure::Unsupported => UNSUPPORTED.to_owned(),
        };
        match command {
            Command::List { .. } => {
                if failure == Failure::Cancelled {
                    self.after_job_cancel();
                    self.notice = "Job list request cancelled".into();
                } else if let Some(list) = self.jobs.list.as_mut() {
                    list.state = ListState::Failed(format!("{text}; r retries"));
                    list.visible = true;
                }
            }
            Command::Inspect { job, .. } => {
                if self.jobs.then.as_ref().is_some_and(|t| t.job == *job) {
                    self.jobs.then = None;
                    self.notice = format!("Job lookup failed: {text}");
                } else if let Some(panel) =
                    self.jobs.viewer.as_mut().and_then(|v| v.detail.as_mut())
                {
                    panel.error = Some(text);
                }
            }
            Command::Read { job, .. } => {
                if failure == Failure::Cancelled {
                    if self
                        .jobs
                        .viewer
                        .as_ref()
                        .is_some_and(|v| v.job == *job && v.phase == Phase::Loading)
                    {
                        self.close_viewer();
                    }
                    self.notice = "Job request cancelled".into();
                } else if let Some(viewer) = self.jobs.viewer.as_mut().filter(|v| v.job == *job) {
                    viewer.phase = Phase::Failed(text);
                }
            }
            Command::Cancel { job, kill, .. } => {
                // A stop that did not reach the service can be repeated.
                self.person_stop_unconfirmed(*job, *kill);
                self.notice = if failure == Failure::Unconfirmed {
                    "Cancel outcome unconfirmed; repeating /job cancel is safe".into()
                } else {
                    format!("Cancel failed: {text}")
                };
            }
            Command::Retain { action, .. } => {
                self.notice = if failure == Failure::Unconfirmed {
                    if matches!(action, Retain::Note(_)) {
                        "Note outcome unconfirmed; check /job ID before repeating the note".into()
                    } else {
                        "Outcome unconfirmed; check /job ID before repeating".into()
                    }
                } else {
                    format!("Job request failed: {text}")
                };
            }
        }
    }
    pub(in super::super) fn jobs_followed(&mut self, update: FollowUpdate) -> bool {
        let Some(viewer) = self
            .jobs
            .viewer
            .as_mut()
            .filter(|v| v.follow == Some(update.session))
        else {
            return false;
        };
        match update.result {
            Err(failure) => {
                viewer.follow = None;
                viewer.phase = Phase::Failed(match failure {
                    Failure::Unavailable(reason) => format!("{}; r retries", literal(&reason, 256)),
                    _ => "follow stopped; r retries".into(),
                });
            }
            Ok(page) => {
                if let Some(entry) = page.entry {
                    worker::remember(&mut self.jobs.cache, &entry);
                    viewer.entry = Some(entry);
                }
                if let Some(page) = page.pages.into_iter().next() {
                    let start = page.start.unwrap_or(0);
                    let view = viewer.view_mut();
                    view.insert(page);
                    view.current = Some(start);
                    view.line = usize::MAX;
                }
                if viewer.settled() {
                    viewer.follow = None;
                    viewer.phase = Phase::Viewing;
                }
            }
        }
        true
    }

    // Keys.

    /// Job views take keys while open. Control chords fall through so that
    /// Ctrl+Q and Ctrl+C keep their meaning.
    pub(super) fn jobs_key(&mut self, key: KeyEvent) -> bool {
        if !self.jobs.open() {
            return false;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL)
            || key.modifiers.contains(KeyModifiers::ALT)
        {
            return false;
        }
        if self.jobs.confirm.is_some() {
            match key.code {
                KeyCode::Char('y' | 'Y') => {
                    let confirm = self.jobs.confirm.take().unwrap();
                    if self.job_send(confirm.command, confirm.draft, true) {
                        self.notice = "Job request pending".into();
                    }
                }
                KeyCode::Char('n' | 'N') | KeyCode::Esc => {
                    self.jobs.confirm = None;
                    self.notice = "Nothing sent".into();
                }
                _ => {}
            }
            return true;
        }
        if self.jobs.viewer.is_some() {
            self.viewer_key(key);
        } else if self.jobs.chooser.is_some() {
            self.chooser_key(key);
        } else {
            self.list_key(key);
        }
        true
    }
    fn chooser_key(&mut self, key: KeyEvent) {
        let chooser = self.jobs.chooser.as_mut().unwrap();
        match key.code {
            KeyCode::Up => chooser.selected = chooser.selected.saturating_sub(1),
            KeyCode::Down => {
                chooser.selected = (chooser.selected + 1).min(chooser.ids.len().saturating_sub(1))
            }
            KeyCode::Enter => {
                let job = chooser.ids[chooser.selected];
                self.open_viewer(job, None, Origin::History, None);
            }
            KeyCode::Esc => self.jobs.chooser = None,
            _ => {}
        }
    }
    fn list_key(&mut self, key: KeyEvent) {
        let list = self.jobs.list.as_mut().unwrap();
        if let Some(note) = list.note.as_mut() {
            match key.code {
                KeyCode::Esc => list.note = None,
                KeyCode::Backspace => {
                    note.pop();
                }
                KeyCode::Char(c)
                    if note.len() + c.len_utf8() <= asura_control::jobs::MAX_JOB_NOTE_BYTES =>
                {
                    note.push(c)
                }
                KeyCode::Enter => {
                    let text = note.clone();
                    let Some(job) = list.entries.get(list.selected).and_then(worker::entry_id)
                    else {
                        return;
                    };
                    if !asura_control::jobs::note_text_valid(&text) {
                        self.notice = "A note is 1 to 2048 bytes without control characters".into();
                        return;
                    }
                    self.job_act(job, Action::Retain(Retain::Note(text)), None);
                }
                _ => {}
            }
            return;
        }
        let count = list.entries.len();
        let previous = list.cursors.len() > 1;
        let next = list.next_before;
        let mut nav = None;
        match key.code {
            KeyCode::Esc => {
                // A page read not yet sent is dropped; a sent one is stopped.
                if list.state == ListState::Loading
                    && self.jobs.outgoing.take().is_none()
                    && self.jobs.pending.is_some()
                {
                    self.jobs.cancel = true;
                }
                self.jobs.list = None;
                self.notice.clear();
                return;
            }
            KeyCode::Up if list.selected == 0 && previous => nav = Some(Nav::Previous),
            KeyCode::Up => list.selected = list.selected.saturating_sub(1),
            KeyCode::Down if list.selected + 1 >= count && next.is_some() => nav = Some(Nav::Next),
            KeyCode::Down => list.selected = (list.selected + 1).min(count.saturating_sub(1)),
            KeyCode::PageDown if list.selected + 1 >= count && next.is_some() => {
                nav = Some(Nav::Next)
            }
            KeyCode::PageDown => list.selected = count.saturating_sub(1),
            KeyCode::PageUp if list.selected == 0 && previous => nav = Some(Nav::Previous),
            KeyCode::PageUp => list.selected = 0,
            KeyCode::Char('r') if matches!(list.state, ListState::Failed(_)) => {
                nav = Some(Nav::Reload)
            }
            _ => {}
        }
        if let Some(nav) = nav {
            let before = match nav {
                Nav::Next => next,
                Nav::Previous => list.cursors[list.cursors.len() - 2],
                Nav::Reload => list.cursors.last().copied().flatten(),
            };
            let command = Command::List {
                all: list.all,
                project: list.project,
                before,
            };
            if self.job_send(command, None, true) {
                let list = self.jobs.list.as_mut().unwrap();
                list.nav = nav;
                list.state = ListState::Loading;
            }
            return;
        }
        let Some(entry) = list.entries.get(list.selected).cloned() else {
            return;
        };
        let Some(job) = worker::entry_id(&entry) else {
            return;
        };
        match key.code {
            KeyCode::Enter => {
                let fence = worker::entry_project(&entry);
                self.open_viewer(job, fence, Origin::List, None);
            }
            KeyCode::Char('c') => self.job_act(job, Action::Cancel, None),
            KeyCode::Char('p') => self.job_act(
                job,
                Action::Retain(if entry.pinned == Some(true) {
                    Retain::Unpin
                } else {
                    Retain::Pin
                }),
                None,
            ),
            KeyCode::Char('d') => self.job_act(job, Action::Retain(Retain::Delete), None),
            KeyCode::Char('n') => list.note = Some(String::new()),
            _ => {}
        }
    }
    fn close_viewer(&mut self) {
        let Some(viewer) = self.jobs.viewer.take() else {
            return;
        };
        // Closing never cancels the job; it only stops this viewer's read:
        // dropped when not yet sent, otherwise the wait is stopped.
        let reads = |request: &Option<Request>| matches!(request, Some(Request { command: Command::Read { job, .. }, .. }) if *job == viewer.job);
        if viewer.phase == Phase::Loading {
            if reads(&self.jobs.outgoing) {
                self.jobs.outgoing = None;
            } else if reads(&self.jobs.pending) {
                self.jobs.cancel = true;
            }
        }
        // History keeps its response selection; the list stays open under
        // a viewer opened from it; a command returns to the editor.
        if viewer.origin == Origin::Command {
            self.jobs.list = None;
        }
    }
    fn viewer_read(&mut self, offset: Option<u64>, land: Land) {
        let viewer = self.jobs.viewer.as_ref().unwrap();
        let command = Command::Read {
            job: viewer.job,
            project: viewer.fence,
            stream: viewer.stream,
            offset,
        };
        if self.job_send(command.clone(), None, true) {
            let viewer = self.jobs.viewer.as_mut().unwrap();
            viewer.phase = Phase::Loading;
            viewer.land = land;
            viewer.last_read = Some(command);
        }
    }
    fn viewer_key(&mut self, key: KeyEvent) {
        let viewer = self.jobs.viewer.as_mut().unwrap();
        if let Some(panel) = viewer.detail.as_mut() {
            match key.code {
                KeyCode::Esc => viewer.detail = None,
                KeyCode::Up => panel.scroll = panel.scroll.saturating_sub(1),
                KeyCode::Down => panel.scroll = panel.scroll.saturating_add(1),
                _ => {}
            }
            return;
        }
        if key.code == KeyCode::Esc {
            self.close_viewer();
            return;
        }
        match (&viewer.phase, key.code) {
            (Phase::Loading, _) => return,
            (Phase::Failed(_), KeyCode::Char('r')) => {
                if let Some(command) = viewer.last_read.clone()
                    && self.job_send(command, None, true)
                {
                    self.jobs.viewer.as_mut().unwrap().phase = Phase::Loading;
                }
                return;
            }
            (Phase::Failed(_), _) => return,
            _ => {}
        }
        let following = viewer.phase == Phase::Following;
        let rows = viewer.rows.max(1);
        match key.code {
            KeyCode::Char('f') => {
                if following {
                    viewer.follow = None;
                    viewer.phase = Phase::Viewing;
                } else if viewer.settled() {
                    self.notice = "The job has settled; nothing to follow".into();
                } else {
                    self.jobs.session = self.jobs.session.wrapping_add(1);
                    let viewer = self.jobs.viewer.as_mut().unwrap();
                    viewer.follow = Some(self.jobs.session);
                    viewer.phase = Phase::Following;
                    viewer.view_mut().line = usize::MAX;
                }
            }
            KeyCode::Char('i') => {
                let (job, fence) = (viewer.job, viewer.fence);
                if self.job_send(
                    Command::Inspect {
                        job,
                        project: fence,
                    },
                    None,
                    true,
                ) {
                    self.jobs.viewer.as_mut().unwrap().detail = Some(Detail {
                        detail: None,
                        error: None,
                        scroll: 0,
                    });
                }
            }
            KeyCode::Tab => {
                viewer.follow = None;
                viewer.phase = Phase::Viewing;
                viewer.stream = viewer.stream.other();
                if viewer.view().page().is_none() {
                    // Each stream keeps its position; a new stream opens at 0.
                    self.viewer_read(Some(0), Land::Top);
                }
            }
            KeyCode::Home => {
                viewer.follow = None;
                viewer.phase = Phase::Viewing;
                let view = viewer.view_mut();
                if view.find(0).is_some() {
                    view.current = Some(0);
                    view.line = 0;
                } else {
                    self.viewer_read(Some(0), Land::Top);
                }
            }
            KeyCode::End if !following => self.viewer_read(None, Land::Bottom),
            KeyCode::Up | KeyCode::PageUp => {
                viewer.follow = None;
                viewer.phase = Phase::Viewing;
                let step = if key.code == KeyCode::Up { 1 } else { rows };
                let view = viewer.view_mut();
                let lines = view.page().map_or(0, |p| page_lines(p).len());
                view.line = view.line.min(lines.saturating_sub(rows));
                if view.line > 0 {
                    view.line = view.line.saturating_sub(step);
                } else if let Some(start) = view.current.filter(|s| *s > 0) {
                    if let Some(previous) = view.before(start).and_then(|p| p.start) {
                        view.current = Some(previous);
                        view.line = usize::MAX;
                    } else {
                        let offset = start.saturating_sub(u64::from(MAX_JOB_PAGE_BYTES));
                        self.viewer_read(Some(offset), Land::Bottom);
                    }
                }
            }
            KeyCode::Down | KeyCode::PageDown if !following => {
                let step = if key.code == KeyCode::Down { 1 } else { rows };
                let view = viewer.view_mut();
                let Some(page) = view.page() else {
                    return;
                };
                let lines = page_lines(page).len();
                let last = lines.saturating_sub(rows);
                let next = page.next_offset;
                let total = page.total_bytes.unwrap_or(0);
                view.line = view.line.min(last);
                if view.line < last {
                    view.line = (view.line + step).min(last);
                } else if let Some(next) = next.filter(|n| *n < total) {
                    if view.find(next).is_some() {
                        view.current = Some(next);
                        view.line = 0;
                    } else {
                        self.viewer_read(Some(next), Land::Top);
                    }
                }
            }
            _ => {}
        }
    }

    // Presentation.

    pub(super) fn jobs_hint(&self) -> Option<String> {
        let jobs = &self.jobs;
        Some(if jobs.confirm.is_some() {
            "y confirm · n or Esc cancel".into()
        } else if let Some(viewer) = &jobs.viewer {
            if viewer.detail.is_some() {
                "↑↓ scroll · Esc viewer".into()
            } else {
                match viewer.phase {
                    Phase::Loading => "Loading · Esc close".into(),
                    Phase::Failed(_) => "r retry · Esc close".into(),
                    Phase::Following => {
                        "f stop following · ↑ PgUp scroll back · Tab stream · i detail · Esc close"
                            .into()
                    }
                    Phase::Viewing => {
                        "Tab stream · ↑↓ PgUp/PgDn scroll · Home/End · f follow · i detail · Esc close"
                            .into()
                    }
                }
            }
        } else if jobs.chooser.is_some() {
            "↑↓ select · ↵ view · Esc back".into()
        } else {
            let list = jobs.list.as_ref().filter(|l| l.visible)?;
            if list.note.is_some() {
                "↵ add note · Esc discard".into()
            } else if matches!(list.state, ListState::Failed(_)) {
                "r retry · Esc close".into()
            } else {
                "↑↓ select · ↵ view · c cancel · p pin · d delete · n note · PgDn more · Esc close"
                    .into()
            }
        })
    }
    /// `jobs N` and `exec off` for the selected project; empty when the
    /// service lacks the jobs capability or the facts are unknown.
    pub(super) fn jobs_status_text(&self) -> String {
        if self.jobs.supported != Some(true)
            || self.jobs.status.is_none()
            || self.jobs.status != self.job_status_scope()
        {
            return String::new();
        }
        worker::status_text(self.jobs.live, self.jobs.execution_enabled)
    }
    /// A TUI-launched backend with live jobs asks before exit.
    pub(super) fn jobs_exit_line(&self) -> Option<String> {
        let owned = self.view.service.as_ref().is_some_and(|s| s.owned);
        let jobs = (owned && self.jobs.supported == Some(true))
            .then(|| worker::exit_line(self.jobs.live_in_service))
            .flatten();
        // Queued and running commands stop with a TUI-launched backend (PC5).
        match (jobs, self.person_exit_line()) {
            (Some(jobs), Some(commands)) => Some(format!("{jobs}\n{commands}")),
            (jobs, commands) => jobs.or(commands),
        }
    }
    fn row(&self, entry: &pb::JobEntry, all: bool, width: usize, now: u64) -> String {
        let id = worker::entry_id(entry)
            .map(|id| worker::short(&id))
            .unwrap_or_default();
        let mark = worker::origin_mark(entry);
        let command = literal(entry.command.as_deref().unwrap_or_default(), 192);
        let state = worker::state_text(entry);
        let age = worker::age(entry, now);
        let project = if all {
            worker::entry_project(entry)
                .map(|p| format!("[{}] ", self.project_name(p)))
                .unwrap_or_default()
        } else {
            String::new()
        };
        let marks = format!(
            "{}{}",
            if entry.pinned == Some(true) {
                " · pinned"
            } else {
                ""
            },
            match entry.notes.unwrap_or(0) {
                0 => String::new(),
                n => format!(" · ✎{n}"),
            }
        );
        if width < 60 {
            format!("{id} {mark}{project}{state} · {age}{marks} · {command}")
        } else {
            format!(
                "{id} {mark:<2} {project}{} · {state} · {age} · out {} err {}{marks}",
                shorten(&command, 32),
                worker::bytes(entry.stdout_bytes.unwrap_or(0)),
                worker::bytes(entry.stderr_bytes.unwrap_or(0)),
            )
        }
    }
    pub(super) fn draw_jobs(&mut self, frame: &mut Frame, area: Rect, style: Style) {
        if !self.jobs.open() || area.width < 10 || area.height < 3 {
            return;
        }
        let width = usize::from(area.width.saturating_sub(2));
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis() as u64);
        let mut rows: Vec<(String, Style)> = Vec::new();
        let selected = selection_style(style);
        let mut fill = false;
        if let Some(viewer) = &self.jobs.viewer {
            fill = true;
            rows = self.viewer_rows(
                viewer,
                width,
                usize::from(area.height.saturating_sub(2)),
                style,
            );
        } else if let Some(chooser) = &self.jobs.chooser {
            rows.push(("Jobs of this response · ↵ view · Esc back".into(), style));
            for (index, id) in chooser.ids.iter().enumerate() {
                let entry = self
                    .jobs
                    .cache
                    .iter()
                    .find(|e| worker::entry_id(e) == Some(*id));
                let text = match entry {
                    Some(entry) => self.row(entry, false, width, now),
                    None => worker::short(id),
                };
                rows.push((
                    text,
                    if index == chooser.selected {
                        selected
                    } else {
                        style
                    },
                ));
            }
        } else if let Some(list) = &self.jobs.list {
            let page = list.cursors.len();
            rows.push((
                format!(
                    "Jobs · {} · page {page}{}{} · Esc closes",
                    if list.all {
                        "all projects"
                    } else {
                        "this project"
                    },
                    if list.next_before.is_some() {
                        " · more"
                    } else {
                        ""
                    },
                    if list.state == ListState::Loading {
                        " · loading"
                    } else {
                        ""
                    },
                ),
                style,
            ));
            if let ListState::Failed(error) = &list.state {
                rows.push((format!("Job list error: {error}"), style));
            } else if list.entries.is_empty() {
                rows.push(("No jobs".into(), style));
            }
            for (index, entry) in list.entries.iter().enumerate() {
                rows.push((
                    self.row(entry, list.all, width, now),
                    if index == list.selected {
                        selected
                    } else {
                        style
                    },
                ));
            }
            if let Some(note) = &list.note {
                let id = list
                    .entries
                    .get(list.selected)
                    .and_then(worker::entry_id)
                    .map(|id| worker::short(&id))
                    .unwrap_or_default();
                rows.push((
                    format!("Note for job {id}: {}▏", literal(note, 2048)),
                    selected,
                ));
            }
        }
        if let Some(confirm) = &self.jobs.confirm {
            rows.push((literal(&confirm.text, 512), selected));
        }
        if rows.is_empty() {
            return;
        }
        let height = if fill {
            area.height
        } else {
            (rows.len() as u16 + 2).min(area.height)
        };
        let popup = Rect::new(area.x, area.bottom() - height, area.width, height);
        frame.render_widget(Clear, popup);
        frame.render_widget(Paragraph::new("").style(style), popup);
        let inner = Rect::new(popup.x + 1, popup.y + 1, popup.width - 2, popup.height - 2);
        // Keep the confirmation and the selected row in view.
        let skip = rows.len().saturating_sub(usize::from(inner.height));
        let skip = if fill || self.jobs.confirm.is_some() {
            skip
        } else {
            let selected_row = self.jobs.list.as_ref().map_or(0, |l| {
                l.selected + 1 + usize::from(matches!(l.state, ListState::Failed(_)))
            });
            skip.min(selected_row)
        };
        for (index, (text, style)) in rows.into_iter().skip(skip).enumerate() {
            line(frame, inner, index as u16, &text, style);
        }
    }
    fn viewer_rows(
        &self,
        viewer: &Viewer,
        width: usize,
        height: usize,
        style: Style,
    ) -> Vec<(String, Style)> {
        let id = worker::short(&viewer.job);
        let state = viewer
            .entry
            .as_ref()
            .map(worker::state_text)
            .unwrap_or_else(|| "?".into());
        let mut rows = Vec::new();
        if let Some(panel) = &viewer.detail {
            rows.push((format!("Job {id} detail · Esc returns"), style));
            let mut body = Vec::new();
            if let Some(error) = &panel.error {
                body.push(format!("Error: {error}"));
            }
            match &panel.detail {
                None if panel.error.is_none() => body.push("Loading…".into()),
                None => {}
                Some(detail) => {
                    body.push(format!("State: {state}"));
                    if let Some(entry) = &detail.entry {
                        body.push(format!(
                            "Output: stdout {} ({} lines) · stderr {} ({} lines){}",
                            worker::bytes(entry.stdout_bytes.unwrap_or(0)),
                            entry.stdout_lines.map_or("?".into(), |n| n.to_string()),
                            worker::bytes(entry.stderr_bytes.unwrap_or(0)),
                            entry.stderr_lines.map_or("?".into(), |n| n.to_string()),
                            if entry.deleted == Some(true) {
                                " · output deleted"
                            } else if entry.pinned == Some(true) {
                                " · pinned"
                            } else {
                                ""
                            }
                        ));
                        if let Some(project) = worker::entry_project(entry)
                            && Some(project) != self.selected_project
                        {
                            body.push(format!("Project: {}", self.project_name(project)));
                        }
                    }
                    body.push(format!(
                        "Cwd: {}",
                        literal(detail.cwd.as_deref().unwrap_or("?"), 4096)
                    ));
                    body.push("Command:".into());
                    for part in detail.command.as_deref().unwrap_or_default().split('\n') {
                        body.push(format!("  {}", literal(part, 8192)));
                    }
                    body.push(format!("Notes: {}", detail.notes.len()));
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map_or(0, |d| d.as_millis() as u64);
                    for note in &detail.notes {
                        let author = match note.author {
                            Some(1) => "you",
                            Some(2) => "model",
                            _ => "?",
                        };
                        let when = worker::ago(note.unix_ms.unwrap_or(0), now);
                        for (index, part) in note
                            .text
                            .as_deref()
                            .unwrap_or_default()
                            .split('\n')
                            .enumerate()
                        {
                            body.push(if index == 0 {
                                format!("  {author} · {when}: {}", literal(part, 2048))
                            } else {
                                format!("    {}", literal(part, 2048))
                            });
                        }
                    }
                }
            }
            let scroll = usize::from(panel.scroll).min(body.len().saturating_sub(1));
            rows.extend(body.into_iter().skip(scroll).map(|text| (text, style)));
            return rows;
        }
        let view = viewer.view();
        let page = view.page();
        let mut header = format!("Job {id} · {state} · {}", viewer.stream.name());
        if let Some(page) = page {
            let start = page.start.unwrap_or(0);
            header.push_str(&format!(
                " · bytes {start}–{} of {} · {}",
                page.next_offset.unwrap_or(start),
                page.total_bytes.unwrap_or(0),
                worker::page_state(page.state)
            ));
        }
        if let Some(entry) = &viewer.entry {
            if entry.pinned == Some(true) {
                header.push_str(" · pinned");
            }
            if let Some(project) = worker::entry_project(entry)
                && Some(project) != self.selected_project
            {
                header.push_str(&format!(" · project {}", self.project_name(project)));
            }
        }
        if viewer.phase == Phase::Following {
            header.push_str(" · following");
        }
        rows.push((header, style));
        let body = height.saturating_sub(1);
        match &viewer.phase {
            Phase::Loading if page.is_none() => rows.push(("Loading…".into(), style)),
            Phase::Failed(error) => rows.push((format!("Error: {error} · r retries"), style)),
            _ => {}
        }
        if matches!(viewer.phase, Phase::Failed(_)) {
            return rows;
        }
        if let Some(page) = page {
            let lines = page_lines(page);
            let deleted = page.state == Some(pb::JobPageState::Deleted as i32)
                || page.state == Some(pb::JobPageState::Expired as i32);
            if lines.is_empty() || (deleted && page.text.as_deref().unwrap_or_default().is_empty())
            {
                rows.push((
                    match page.state.and_then(|s| pb::JobPageState::try_from(s).ok()) {
                        Some(pb::JobPageState::Deleted) => {
                            "Output deleted; i shows the summary and notes".into()
                        }
                        Some(pb::JobPageState::Expired) => {
                            "Output expired; i shows the summary and notes".into()
                        }
                        _ => "No output in this range".into(),
                    },
                    style,
                ));
            } else {
                let first = view.line.min(lines.len().saturating_sub(body));
                for text in lines.into_iter().skip(first).take(body) {
                    rows.push((shorten(&text, width), style));
                }
            }
        }
        rows
    }
    /// Keeps the viewer's scroll inside its page for the drawn height.
    pub(super) fn settle_job_viewer(&mut self, height: u16) {
        let Some(viewer) = self.jobs.viewer.as_mut() else {
            return;
        };
        viewer.rows = usize::from(height.saturating_sub(3)).max(1);
        let rows = viewer.rows;
        let view = viewer.view_mut();
        let lines = view.page().map_or(0, |p| page_lines(p).len());
        view.line = view.line.min(lines.saturating_sub(rows));
    }
}

/// A page's text as display lines; a final newline ends the last line.
pub(super) fn page_lines(page: &pb::JobStreamPage) -> Vec<String> {
    let text = page.text.as_deref().unwrap_or_default();
    let text = text.strip_suffix('\n').unwrap_or(text);
    if text.is_empty() {
        return Vec::new();
    }
    text.split('\n').map(|line| literal(line, 8192)).collect()
}

#[cfg(test)]
mod tests;
