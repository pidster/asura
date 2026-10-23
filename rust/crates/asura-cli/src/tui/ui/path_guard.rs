//! The execution path guard in the TUI (execution-path-guard.md, TUI; owner
//! hold for `!!`, EPG-R5): the warning lines of a person command's log
//! entry, the held `!!` entry's line and its Enter and Esc decisions, and
//! `/paths`, `/paths approve PATH | all` and `/paths revoke PATH` with their
//! replies and errors. Rendering and key handling perform no I/O: the queue
//! worker sends the decisions and the path guard worker the requests.
use super::person::LogEntry;
use super::*;
use crate::tui::path_guard::{Outcome, Request, Update};
use asura_client::PathGuardRequest;
use asura_control::path_guard::{error as token, kind};
use asura_control::pb;

/// The hold reason of a `!!` command with a `model_changed` warning.
pub(super) const PATHS_CHANGED: &str = "paths_changed";
/// The held entry's line (EPG-E4).
pub(super) const HELD_TEXT: &str = "Held: confirm to run (paths changed)";
/// A held or checking entry the person dropped.
pub(super) const DROPPED_TEXT: &str = "↳ dropped; it did not run";
const USAGE: &str = "Use /paths, /paths approve PATH, /paths approve all or /paths revoke PATH";
const MAX_PATH_BYTES: usize = asura_control::path_guard::MAX_GUARD_PATH_BYTES;

/// The warning lines of a log entry or a `/paths` list, in kind order:
/// one line per kind, its paths joined, then `and N more` for the
/// warnings not listed. The `⚠` marker carries the meaning; the alert
/// colour is not the only signal.
pub(super) fn warning_lines(warnings: &[pb::PathWarning], overflow: u32) -> Vec<String> {
    let paths = |k: u32| -> Vec<String> {
        warnings
            .iter()
            .filter(|w| w.kind == Some(k))
            .filter_map(|w| w.path.as_deref())
            .map(|p| literal(p, MAX_PATH_BYTES))
            .collect()
    };
    let mut lines = Vec::new();
    for (k, lead, tail) in [
        (
            kind::MODEL_CHANGED,
            "changed by the model since your last command",
            "",
        ),
        (kind::NOT_APPROVED, "not approved", " · /paths approve"),
        (
            kind::CHANGED_SINCE_APPROVAL,
            "changed since you approved it",
            "",
        ),
        (kind::TOO_LARGE, "too large to approve (over 1 MiB)", ""),
    ] {
        let listed = paths(k);
        if !listed.is_empty() {
            lines.push(format!("⚠ {lead}: {}{tail}", listed.join(", ")));
        }
    }
    if warnings.iter().any(|w| w.kind == Some(kind::OVERFLOW)) {
        lines.push(
            "⚠ more paths than Asura checks: changes beyond the bound are not detected".into(),
        );
    }
    if overflow > 0 {
        lines.push(format!("⚠ and {overflow} more"));
    }
    lines
}

/// A kind 3 queue entry held with `paths_changed`.
pub(super) fn paths_held(entry: &pb::ConversationQueueEntry) -> bool {
    entry.state == Some(2) && entry.hold_reason.as_deref() == Some(PATHS_CHANGED)
}

/// The queue entry's path facts on its log entry. A list replaces the
/// shown one (a Resume that finds a newer change holds again with the new
/// list); an entry without a list keeps what was shown.
pub(super) fn queue_facts(log: &mut LogEntry, entry: &pb::ConversationQueueEntry) {
    if !entry.path_warnings.is_empty() || entry.path_overflow.is_some() {
        log.path_warnings = entry.path_warnings.clone();
        log.path_overflow = entry.path_overflow.unwrap_or(0);
    }
    log.paths_held = paths_held(entry);
    log.path_check = entry.state == Some(1)
        && entry.waiting_reason == Some(pb::QueueWaitReason::PathCheck as i32);
    if entry.state == Some(6) {
        log.dropped = true;
        log.paths_held = false;
    }
}

/// `/paths` arguments: the rest of the line after `approve` or `revoke` is
/// the path, so a path may hold spaces.
pub(super) fn parse(draft: &str, project: [u8; 16]) -> Result<PathGuardRequest, &'static str> {
    let project_id = Some(project.to_vec());
    let rest = draft.trim().strip_prefix("/paths").ok_or(USAGE)?;
    if rest.is_empty() {
        return Ok(PathGuardRequest::List(pb::PathGuardList { project_id }));
    }
    let rest = rest.strip_prefix(char::is_whitespace).ok_or(USAGE)?.trim();
    let (verb, path) = rest.split_once(char::is_whitespace).ok_or(USAGE)?;
    let path = path.trim();
    if path.is_empty() || path.len() > MAX_PATH_BYTES {
        return Err(USAGE);
    }
    let request_id = Some(asura_platform::random_id().to_vec());
    match verb {
        "approve" if path == "all" => Ok(PathGuardRequest::Approve(pb::PathGuardApprove {
            request_id,
            project_id,
            path: None,
            all: Some(true),
        })),
        "approve" => Ok(PathGuardRequest::Approve(pb::PathGuardApprove {
            request_id,
            project_id,
            path: Some(path.to_owned()),
            all: None,
        })),
        "revoke" => Ok(PathGuardRequest::Revoke(pb::PathGuardRevoke {
            request_id,
            project_id,
            path: Some(path.to_owned()),
        })),
        _ => Err(USAGE),
    }
}

/// The path a request names, for its notices.
fn request_path(request: &PathGuardRequest) -> Option<&str> {
    match request {
        PathGuardRequest::Approve(v) => v.path.as_deref(),
        PathGuardRequest::Revoke(v) => v.path.as_deref(),
        PathGuardRequest::List(_) => None,
    }
}

/// The text of a reply's `error` token.
pub(super) fn error_text(error: &str, request: &PathGuardRequest) -> String {
    let path = request_path(request).map(|p| literal(p, MAX_PATH_BYTES));
    match (error, request) {
        (token::NOT_FOUND, PathGuardRequest::List(_)) => "Project not found".into(),
        (token::NOT_FOUND, PathGuardRequest::Approve(v)) if v.all == Some(true) => {
            "Project not found".into()
        }
        (token::NOT_FOUND, PathGuardRequest::Approve(_)) => {
            format!("No such file in the project: {}", path.unwrap_or_default())
        }
        (token::NOT_FOUND, PathGuardRequest::Revoke(_)) => {
            format!("Not approved: {}", path.unwrap_or_default())
        }
        (token::PATH_INVALID, _) => {
            "Give a path inside the project, relative to its folder, without ..".into()
        }
        (token::TOO_LARGE, _) => {
            "The file is over 1 MiB and cannot be approved; Asura warns of it before each command"
                .into()
        }
        (token::NOT_IMPLICIT, _) => "Only implicit-execution paths are approved: .git/hooks, \
             .git/config, .gitattributes, .gitmodules, .envrc and .cargo/config"
            .into(),
        (token::LIMIT, _) => {
            "The project has the most approvals (256 paths, or 4,096 approvals and revocations); \
             revoke one first"
                .into()
        }
        (token::BUSY, _) => "The path guard is busy; try again".into(),
        (token::UNAVAILABLE, _) => {
            "The path guard cannot read the journal, the project folder or the file".into()
        }
        (other, _) => format!("Path request failed: {}", literal(other, 64)),
    }
}

/// The `/paths` panel: what the next person command would show, then the
/// approved paths in byte order, then the count the reply did not list.
pub(super) fn reply_text(reply: &pb::PathGuardReply) -> String {
    let mut lines = vec!["Warnings before the next command".to_owned()];
    let warnings = warning_lines(&reply.pending, reply.overflow.unwrap_or(0));
    if warnings.is_empty() {
        lines.push("No path warnings".into());
    }
    lines.extend(warnings);
    lines.push(String::new());
    lines.push("Approved paths".into());
    if reply.approved.is_empty() && reply.approved_omitted.is_none() {
        lines.push("No approved paths".into());
    }
    lines.extend(reply.approved.iter().map(|p| literal(p, MAX_PATH_BYTES)));
    if let Some(n) = reply.approved_omitted.filter(|n| *n > 0) {
        lines.push(format!(
            "and {n} more approved path{}",
            if n == 1 { "" } else { "s" }
        ));
    }
    lines.join("\n")
}

/// The notice of a reply without an error.
fn done_text(request: &PathGuardRequest) -> String {
    match request {
        PathGuardRequest::List(_) => "Execution paths".into(),
        PathGuardRequest::Approve(v) if v.all == Some(true) => {
            "Approved every implicit-execution file shown".into()
        }
        PathGuardRequest::Approve(_) => format!(
            "Approved {}",
            literal(request_path(request).unwrap_or_default(), MAX_PATH_BYTES)
        ),
        PathGuardRequest::Revoke(_) => format!(
            "Revoked {}",
            literal(request_path(request).unwrap_or_default(), MAX_PATH_BYTES)
        ),
    }
}

/// What a log-focus key asked of a held or checking entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Step {
    /// Enter on a held entry: decision action 1.
    Resume,
    /// Esc on a held entry: decision action 2.
    Drop,
    /// Esc on an entry waiting with `path_check`: the writer drops only
    /// held inputs, so Hold (action 4), then Drop.
    HoldThenDrop,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Decision {
    pub request_id: Vec<u8>,
    pub input: [u8; 16],
    pub step: Step,
}

/// `/paths` and decision state beside the person log.
#[derive(Default)]
pub(super) struct Paths {
    pub request: Option<Request>,
    pub busy: bool,
    pub cancel: bool,
    pub decision: Option<Decision>,
}

impl App {
    // `/paths`.

    pub(super) fn paths_command(&mut self, draft: String) {
        if self.person.paths.busy {
            self.notice = "Path request pending; draft retained".into();
            return;
        }
        let Some(project) = self.selected_project else {
            self.notice = "Select a project first; draft retained".into();
            return;
        };
        if self.person.supported != Some(true) {
            self.notice = crate::tui::queue::COMMANDS_NOT_ENABLED.into();
            return;
        }
        match parse(&draft, project) {
            Ok(command) => {
                self.person.paths.request = Some(Request { command, draft });
                self.person.paths.busy = true;
                self.notice = "Path request pending · Esc stops waiting".into();
            }
            Err(usage) => self.notice = usage.into(),
        }
    }
    pub(in super::super) fn take_path_guard_request(&mut self) -> Option<Request> {
        self.person.paths.request.take()
    }
    pub(in super::super) fn take_path_guard_cancel(&mut self) -> bool {
        std::mem::take(&mut self.person.paths.cancel)
    }
    /// `/cancel` or Escape while a path request waits.
    pub(super) fn path_guard_stop(&mut self) -> bool {
        if !self.person.paths.busy {
            return false;
        }
        self.person.paths.cancel = true;
        self.notice = "Path request wait stopped; an approval may still be applied".into();
        true
    }
    /// The reply refreshes the list panel; an error keeps the draft.
    pub(in super::super) fn path_guard_finished(&mut self, update: Update) {
        self.person.paths.busy = false;
        let request = update.request;
        match update.outcome {
            Outcome::Reply(reply) => match reply.error.as_deref() {
                Some(error) => {
                    self.notice = format!("{}; draft retained", error_text(error, &request.command))
                }
                None => {
                    if self.editor.text() == request.draft {
                        self.editor = Editor::new();
                    }
                    self.notice = done_text(&request.command);
                    self.result_title = "Execution paths";
                    self.config_result = Some(reply_text(&reply));
                }
            },
            Outcome::Definite(error) => {
                self.notice = format!("{}; draft retained", literal(&error, 256));
            }
            Outcome::Unconfirmed(error) => {
                self.notice = format!(
                    "{}; the outcome is unconfirmed; /paths shows the current list",
                    literal(&error, 256)
                );
            }
        }
    }

    // Held and checking entries in log focus.

    /// Enter in log focus: runs the held `!!` entry in view. False when the
    /// entry in view is not held (Enter then opens the viewer).
    pub(super) fn person_log_enter(&mut self) -> bool {
        let Some((input, entry)) = self.person_log_decidable() else {
            return false;
        };
        if !entry.paths_held {
            return false;
        }
        self.send_path_decision(input, Step::Resume);
        true
    }
    /// Esc in log focus: drops the held or checking entry in view. False
    /// when there is none (Esc then leaves the log).
    pub(super) fn person_log_escape(&mut self) -> bool {
        let Some((input, entry)) = self.person_log_decidable() else {
            return false;
        };
        if entry.paths_held {
            self.send_path_decision(input, Step::Drop);
        } else if entry.path_check {
            self.send_path_decision(input, Step::HoldThenDrop);
        } else {
            return false;
        }
        true
    }
    /// The log-focus hint's lead for the entry in view.
    pub(super) fn person_log_hint(&self) -> Option<&'static str> {
        let (_, entry) = self.person_log_decidable()?;
        if entry.paths_held {
            Some("↵ run the held command · Esc drop it")
        } else if entry.path_check {
            Some("Esc drop the command")
        } else {
            None
        }
    }
    /// Enter in queue focus on an entry held with `paths_changed` (EPG-T2):
    /// the same Resume as Enter on its log entry.
    pub(super) fn person_queue_resume(&mut self, input: [u8; 16]) {
        self.send_path_decision(input, Step::Resume);
    }
    /// The focused queue entry is held with `paths_changed`.
    pub(super) fn queue_focus_paths_held(&self) -> bool {
        self.queue_focused_id.as_ref().is_some_and(|id| {
            self.queue_entries
                .iter()
                .any(|e| e.input_id.as_ref() == Some(id) && paths_held(e))
        })
    }
    fn person_log_decidable(&self) -> Option<([u8; 16], &LogEntry)> {
        let entry = self.person.log.get(self.person_log_target()?)?;
        Some((entry.input?, entry))
    }
    fn send_path_decision(&mut self, input: [u8; 16], step: Step) {
        if self.queue_busy {
            self.notice = "Queue request pending; press the key again after it settles".into();
            return;
        }
        let request_id = asura_platform::random_id().to_vec();
        let action = match step {
            Step::Resume => 1,
            Step::Drop => 2,
            Step::HoldThenDrop => 4,
        };
        self.send_queue(super::super::queue::Request::Decision(
            pb::ConversationQueueDecision {
                request_id: Some(request_id.clone()),
                input_id: Some(input.to_vec()),
                action: Some(action),
                ..Default::default()
            },
        ));
        if !self.queue_busy {
            return; // Refused before sending; the notice says why.
        }
        // A key decision never takes the draft with it.
        self.queue_editor = None;
        self.queue_show = false;
        self.person.paths.decision = Some(Decision {
            request_id,
            input,
            step,
        });
        self.notice = match step {
            Step::Resume => "Confirming the held command",
            Step::Drop | Step::HoldThenDrop => "Dropping the command",
        }
        .into();
    }
    /// The decision this queue update answers, if it is the log's.
    pub(super) fn path_decision_of(&self, request: &super::super::queue::Request) -> Option<Step> {
        let super::super::queue::Request::Decision(decision) = request else {
            return None;
        };
        self.person
            .paths
            .decision
            .as_ref()
            .filter(|d| decision.request_id.as_deref() == Some(d.request_id.as_slice()))
            .map(|d| d.step)
    }
    /// After the queue's own handling of a settled decision: the log's
    /// notice, the entry's state, and the Drop after a Hold.
    pub(super) fn path_decision_done(&mut self, outcome: Result<bool, String>) {
        let Some(decision) = self.person.paths.decision.take() else {
            return;
        };
        let entry = self
            .person
            .log
            .iter_mut()
            .find(|e| e.input == Some(decision.input));
        match (outcome, decision.step) {
            (Ok(true), Step::Resume) => {
                if let Some(entry) = entry {
                    entry.paths_held = false;
                }
                self.notice =
                    "Command confirmed; Asura checks its paths again, then runs it".into();
            }
            (Ok(true), Step::Drop) => {
                if let Some(entry) = entry {
                    entry.paths_held = false;
                    entry.path_check = false;
                    entry.dropped = true;
                    entry.fold_done = true;
                }
                self.notice = "Command dropped; it did not run".into();
            }
            (Ok(true), Step::HoldThenDrop) => {
                self.send_path_decision(decision.input, Step::Drop);
            }
            (Ok(false), _) => {
                self.notice = "Queue reply did not answer the decision; /queue shows it".into();
            }
            (Err(error), Step::HoldThenDrop) if error.contains("input_not_preparing") => {
                self.notice = "The command already started; /cancel stops it".into();
            }
            (Err(error), step) => {
                self.notice = format!(
                    "Could not {} the command: {}",
                    if step == Step::Resume { "run" } else { "drop" },
                    literal(&error, 256)
                );
            }
        }
    }
}

#[cfg(test)]
#[path = "path_guard/tests.rs"]
mod tests;
