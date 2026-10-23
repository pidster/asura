//! The job control handler (tool-job-controller.md, J4 control handler):
//! turns the client job requests `JobList`, `JobInspect`, `JobRead`,
//! `JobObserve`, `JobCancel` and `JobRetain` into job controller calls,
//! journal reads and kind 30 appends, and builds their replies with the
//! J34-W error tokens.
//!
//! The handler is a module of the conversation owner on the reactor. It
//! never blocks and never submits anything itself: it does bounded
//! in-memory work and hands its journal work to the owner
//! ([`JobControl::next_journal`]), which submits it on its serialized
//! ticket and reports each reply ([`JobControl::journal_done`]). Page reads
//! go to the payload owner's page pool through [`PagePort`]; their tickets
//! are polled in [`JobControl::poll`]. Parked requests (`JobObserve`, and
//! `JobList` with `after_revision`) are controller client waits keyed by
//! the handler's request key; the owner routes each ended client wait from
//! `JobController::drain_waits` to [`JobControl::wait_done`].
//!
//! No request has a time bound (no default time bounds). Each ends on its
//! reply, the attachment's detach ([`JobControl::retain`]) or shutdown
//! ([`JobControl::close`], answered `Draining`). A person note waits for
//! the secret rule set ([`Ctx::rules`]) without a time bound; the handler
//! never compiles the set. Bounds: one pending request
//! per attachment and at most 32 attachments; `jobs.clientViewers` pending
//! `JobRead`s, `JobObserve`s and parked `JobList`s (`busy` beyond); the page
//! pool's queue of 8 (`busy`); 64 remembered retain outcomes.

use crate::job_controller::{
    CancelReply, JobController, JobReason, JobView, Profile, Requester, WaitDone, WaitEnding,
    WaitKey, WaitStart,
};
use asura_control::{jobs as wire, pb};
use asura_platform::secrets::json::{self, Value};
use asura_platform::secrets::{self, RuleSet, SelfCheckError};
use asura_storage::authority::conversation::{
    self as journal, Id, JobBlockReason, JobCommand, JobOrigin, JobPauseReason, JobRecords,
    JobRetention, JobRunState, JobScope, OutputStream, RetentionAction, RetentionAuthor, ToolCall,
};
use asura_storage::authority::writer;
use asura_storage::payload::{self, Page, PagePool, PageReply, PageRequest, PageState, Ticket};
use pb::envelope::Body;
use std::collections::{BTreeMap, VecDeque};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

/// `jobs.clientViewers` is at most 32 (the controller's `CLIENT_WAITS`).
pub(crate) const MAX_CLIENT_VIEWERS: u32 = 32;
/// Retain outcomes remembered for a repeated request ID while the service
/// runs (kind 30: "returns the earlier outcome").
pub(crate) const RECENT_RETAINS: usize = 64;
/// Settlement rereads after a cancel found the job's kind 28 built but not
/// yet committed; a later race gives `requested: true` with the live entry.
const SETTLE_REREADS: u8 = 3;
/// `Draining` reply text.
const DRAINING: &str = "service_draining";

/// The job request bodies this handler serves.
pub(crate) fn handles(body: &Body) -> bool {
    matches!(
        body,
        Body::JobList(_)
            | Body::JobInspect(_)
            | Body::JobRead(_)
            | Body::JobObserve(_)
            | Body::JobCancel(_)
            | Body::JobRetain(_)
    )
}

/// The request validation schedule's priority class of a job request
/// (J4 control handler, routing): `JobCancel` 0 with `ConversationCancel`,
/// `JobList` and `JobObserve` 2, the rest 1.
pub(crate) fn priority(body: &Body) -> Option<u8> {
    match body {
        Body::JobCancel(_) => Some(0),
        Body::JobList(_) | Body::JobObserve(_) => Some(2),
        Body::JobInspect(_) | Body::JobRead(_) | Body::JobRetain(_) => Some(1),
        _ => None,
    }
}

/// The owner's project registry and execution setting, as the handler
/// reads them. Every method is an in-memory lookup.
pub(crate) trait Projects {
    /// `Some(true)` registered, `Some(false)` not registered, `None` not
    /// known in memory: the handler then reads `Project` from the journal.
    fn registered(&self, project: Id) -> Option<bool>;
    /// The registered location, for the absolute working directory of a
    /// `JobDetail`; `None` leaves `cwd` absent.
    fn location(&self, project: Id) -> Option<String>;
    /// The EX11 effective value of a project (`JobListReply.execution_enabled`).
    fn execution_enabled(&self, project: Id) -> bool;
}

/// One submitted page read. Dropping it withdraws the read (WB1): a queued
/// read leaves the pool's queue; one on a worker finishes and its reply is
/// discarded.
pub(crate) trait PageWait {
    /// Nonblocking: the reply once it has arrived; returned once.
    fn poll(&self) -> Option<PageReply>;
}
impl PageWait for Ticket {
    fn poll(&self) -> Option<PageReply> {
        Ticket::poll(self)
    }
}

/// The payload owner's page pool, as the handler submits client reads.
pub(crate) trait PagePort {
    /// Queues one client page read; `Err(Busy)` when the pool's queue of 8
    /// is full or the pool is closed.
    fn submit(&self, request: PageRequest) -> Result<Box<dyn PageWait>, PageState>;
}
impl PagePort for PagePool {
    fn submit(&self, request: PageRequest) -> Result<Box<dyn PageWait>, PageState> {
        PagePool::submit(self, request, payload::Origin::Client)
            .map(|ticket| Box::new(ticket) as Box<dyn PageWait>)
    }
}

/// What the handler reads from the owner at each call.
pub(crate) struct Ctx<'a> {
    pub jobs: &'a mut JobController,
    /// The page pool; `None` while the payload store is not ready or is
    /// unavailable (`unavailable`).
    pub pages: Option<&'a dyn PagePort>,
    pub projects: &'a dyn Projects,
    /// The installation ID: the payload ID derivation input.
    pub installation: Id,
    /// The journal holds normal records at its quota or disk floor: a
    /// retain waits for room before its kind 30 append.
    pub journal_held: bool,
    /// The secret rule set as `secrets::compiled()` reads it: `None` while
    /// no thread has compiled it. A person note waits (`Wait::Rules`)
    /// until it is `Some`; the handler never compiles it (AE-01).
    pub rules: Option<Result<&'static RuleSet, SelfCheckError>>,
    pub now: Instant,
}

/// Journal work for the owner's serialized ticket.
#[derive(Debug)]
pub(crate) enum JournalWork {
    /// A read: `Jobs`, `JobFacts` or `Project`.
    Read(writer::Command),
    /// A kind 30 append. The owner submits it as its other appends
    /// (expected revision from its replay, no admission fence) and adopts
    /// the reply's replay before it reports the reply.
    Append(journal::Record),
}

/// A committed kind 30 of a person request: the payload owner sets or
/// clears the pin hold, or queues the deletion of both payload files
/// (pin and delete). The handler has already incremented the job revision.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct Retained {
    pub job: Id,
    pub action: RetentionAction,
}

/// The job control handler.
pub(crate) struct JobControl {
    /// `jobs.clientViewers` (1 to 32).
    viewers: usize,
    next_key: u64,
    requests: BTreeMap<u64, Request>,
    /// Requests whose journal work waits for the owner's ticket, oldest first.
    outbox: VecDeque<u64>,
    replies: Vec<pb::Envelope>,
    retained: Vec<Retained>,
    learned: Vec<journal::ProjectRegistered>,
    /// Retains run one at a time, so the state each checks is the state
    /// its kind 30 appends to.
    retain_active: Option<u64>,
    retain_queue: VecDeque<u64>,
    recent: VecDeque<Recent>,
    closed: bool,
    /// The request being started, and its reply when it finished at once.
    starting: Option<u64>,
    immediate: Option<pb::Envelope>,
}

struct Recent {
    request: Vec<u8>,
    job: Id,
    action: RetentionAction,
    reply: pb::JobRetainReply,
}

struct Request {
    envelope: pb::Envelope,
    attachment: Option<[u8; 16]>,
    /// Counts toward `jobs.clientViewers`.
    viewer: bool,
    /// The attachment closed: the reply is discarded and no further work
    /// starts; an in-flight append still applies its effects.
    detached: bool,
    wait: Wait,
    /// A `Project` read is in flight for this project.
    registry_read: Option<Id>,
    /// Registry answers read from the journal for this request.
    known: Vec<(Id, bool)>,
    /// A held append was retried once without a reported hold.
    hold_retried: bool,
    work: Work,
}

enum Wait {
    None,
    /// Queued in the outbox.
    Journal(JournalWork),
    /// Taken by the owner; the reply has not arrived.
    InFlight,
    /// A parked controller client wait.
    Controller,
    /// Page reads, one or two streams.
    Pages(Vec<PendingPage>),
    /// A kind 30 append waiting for the journal hold to end. `seen`: the
    /// owner reported the hold since the refusal.
    Held {
        record: journal::Record,
        seen: bool,
    },
    /// A retain waiting for its turn.
    Turn,
    /// A note retain waiting for the secret rule set; it keeps its retain
    /// turn. Ends on readiness ([`JobControl::poll`]), detach or shutdown.
    Rules,
}

struct PendingPage {
    stream: OutputStream,
    offset: u64,
    wait: Box<dyn PageWait>,
    reply: Option<PageReply>,
}

/// The input that advances a request.
enum Input {
    Start,
    Journal(Box<writer::Result<writer::Reply>>),
    /// A parked controller wait ended (other than by shutdown).
    Wait,
    Pages(Vec<(OutputStream, u64, PageReply)>),
    /// A registry answer arrived; re-evaluate.
    Resume,
}

/// What a request needs next.
enum Next {
    Reply(Box<Body>),
    Read(writer::Command),
    Append(journal::Record),
    Hold(journal::Record),
    Park,
    Pages(Vec<(OutputStream, PageRequest)>),
    Turn,
    Rules,
}

/// A registry check result.
enum Registry {
    Yes,
    No,
    Read(Id),
}

fn registry(known: &[(Id, bool)], projects: &dyn Projects, project: Id) -> Registry {
    let answer = known
        .iter()
        .find(|(p, _)| *p == project)
        .map(|(_, r)| *r)
        .or_else(|| projects.registered(project));
    match answer {
        Some(true) => Registry::Yes,
        Some(false) => Registry::No,
        None => Registry::Read(project),
    }
}

fn wall_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

fn id(value: &Option<Vec<u8>>) -> Id {
    value
        .as_deref()
        .and_then(|v| v.try_into().ok())
        .unwrap_or([0; 16])
}
fn optional_id(value: &Option<Vec<u8>>) -> Option<Id> {
    value.as_deref().and_then(|v| v.try_into().ok())
}

// ----- Replies -----

/// The error reply of a request's own type (J34-W token sets).
fn error_body(request: &Body, token: &str) -> Body {
    let error = Some(token.to_owned());
    match request {
        Body::JobList(_) => Body::JobListReply(pb::JobListReply {
            error,
            ..Default::default()
        }),
        Body::JobInspect(_) => Body::JobDetail(pb::JobDetail {
            error,
            ..Default::default()
        }),
        Body::JobRead(_) | Body::JobObserve(_) => Body::JobPage(pb::JobPage {
            error,
            ..Default::default()
        }),
        Body::JobCancel(v) => Body::JobCancelReply(pb::JobCancelReply {
            request_id: v.request_id.clone(),
            error,
            ..Default::default()
        }),
        Body::JobRetain(v) => Body::JobRetainReply(pb::JobRetainReply {
            request_id: v.request_id.clone(),
            error,
            ..Default::default()
        }),
        _ => draining(),
    }
}

/// `busy` where the reply type has it (list, detail, page); `unavailable`
/// for a mutation, whose token set has no `busy`.
fn busy_body(request: &Body) -> Body {
    match request {
        Body::JobCancel(_) | Body::JobRetain(_) => error_body(request, wire::error::UNAVAILABLE),
        _ => error_body(request, wire::error::BUSY),
    }
}

fn read_error(request: &Body, error: writer::Error) -> Body {
    match error {
        writer::Error::Busy => busy_body(request),
        _ => error_body(request, wire::error::UNAVAILABLE),
    }
}

fn draining() -> Body {
    Body::Error(pb::Error {
        code: Some(pb::ErrorCode::Draining as i32),
        message: Some(DRAINING.into()),
    })
}

/// The `Draining` reply of a job request (AE-06).
pub(crate) fn draining_reply(mut request: pb::Envelope) -> pb::Envelope {
    request.body = Some(draining());
    request
}
/// The `unavailable` reply of a job request, for an owner that has no
/// job controller (the start-time limits read failed).
pub(crate) fn unavailable_reply(mut request: pb::Envelope) -> pb::Envelope {
    let body = request
        .body
        .as_ref()
        .map_or_else(draining, |b| error_body(b, wire::error::UNAVAILABLE));
    request.body = Some(body);
    request
}

// ----- Entries -----

/// One job's journal facts: its listing and the kind 14 holding its
/// command (`JobFacts`).
struct Facts {
    records: Box<JobRecords>,
}
impl Facts {
    fn project(&self) -> Id {
        self.records.listing.project
    }
    fn settled(&self) -> bool {
        self.records.listing.settled.is_some()
    }
    fn origin(&self) -> JobOrigin {
        self.records.listing.intent.origin
    }
}

fn model_origin(origin: &JobOrigin) -> bool {
    matches!(origin, JobOrigin::Model { .. })
}

/// The stored, redacted `cwd` argument of an exec_cmd kind 14; `.` when
/// absent.
pub(crate) fn exec_cmd_cwd(call: &ToolCall) -> Option<String> {
    let Ok(Value::Object(members)) = json::parse(&call.arguments) else {
        return None;
    };
    Some(
        members
            .into_iter()
            .find_map(|(key, value)| match value {
                Value::String(text) if key == "cwd" => Some(text),
                _ => None,
            })
            .unwrap_or_else(|| ".".into()),
    )
}

/// The absolute working directory of a `JobDetail`: the project location
/// joined with the relative `cwd`.
fn absolute_cwd(location: &str, cwd: &str) -> Option<String> {
    let location = location.trim_end_matches('/');
    let path = match cwd.trim_start_matches("./") {
        "" | "." => location.to_owned(),
        rest => format!("{location}/{}", rest.trim_end_matches('/')),
    };
    (path.starts_with('/') && path.len() <= wire::MAX_JOB_CWD_BYTES && !path.contains('\0'))
        .then_some(path)
}

fn cut(text: &str, max: usize) -> String {
    let mut end = text.len().min(max);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

fn pause_code(reason: JobPauseReason) -> u32 {
    // The kind 28 capture reason codes: 2 `quota`, 3 `enospc`.
    match reason {
        JobPauseReason::Quota => 2,
        JobPauseReason::DiskSpace => 3,
    }
}

fn block_code(reason: JobBlockReason) -> u32 {
    reason as u32
}

/// The live part of an entry from the controller's view.
fn apply_view(entry: &mut pb::JobEntry, view: &JobView) {
    let state = view.snapshot.state;
    entry.state = Some(i32::from(state.code()));
    entry.block_reason = match state {
        JobRunState::Queued(reason) => Some(block_code(reason)),
        JobRunState::CapturePaused(reason) => Some(pause_code(reason)),
        _ => None,
    };
    entry.reason_text = match state {
        JobRunState::Queued(_) => view.text.as_deref().and_then(|text| {
            (!text.is_empty()
                && text.len() <= wire::MAX_JOB_REASON_TEXT_BYTES
                && !text.chars().any(char::is_control))
            .then(|| text.to_owned())
        }),
        _ => None,
    };
    entry.stdout_bytes = Some(view.snapshot.stdout_bytes);
    entry.stderr_bytes = Some(view.snapshot.stderr_bytes);
    entry.elapsed_ms = Some(view.snapshot.elapsed_ms);
    if let Some(reason) = view.cancel {
        entry.reason = Some(u32::from(reason as u8));
        entry.cancel_requested = Some(true);
    }
}

fn origin_fields(entry: &mut pb::JobEntry, origin: &JobOrigin) {
    match origin {
        JobOrigin::Model { .. } => entry.origin = Some(pb::JobOrigin::Model as i32),
        JobOrigin::Person { input } => {
            entry.origin = Some(pb::JobOrigin::Person as i32);
            entry.input_id = Some(input.to_vec());
        }
    }
}

fn entry_from_facts(facts: &Facts, view: Option<&JobView>) -> pb::JobEntry {
    let mut entry = entry_from_listing(&facts.records.listing, view);
    // The command source decides the profile of a settled person job the
    // controller no longer remembers: kind 31 field 7 bit 1 (PCR25).
    if facts.records.command.outside_sandbox() && entry.outside_sandbox.is_none() {
        let settled = facts.records.listing.settled.as_deref();
        outside_fields(&mut entry, None, settled);
    }
    entry
}

/// `JobEntry.outside_sandbox` and OD2 fields 28 and 29 of a `!!` job
/// (profile `person-outside-1`): `left_running` from the controller's view
/// while `draining`, or from kind 28 field 8 once settled; `shell_exited`
/// and the shell's exit (fields 12 and 13) only while a released job drains. Absent for every other profile.
fn outside_fields(
    entry: &mut pb::JobEntry,
    view: Option<&JobView>,
    settled: Option<&journal::JobSettled>,
) {
    entry.outside_sandbox = Some(true);
    let state = entry.state;
    if state == Some(pb::JobState::Settled as i32) {
        entry.left_running = settled
            .map(|s| s.escapees)
            .or_else(|| view.and_then(|v| v.left_running));
    } else if state == Some(pb::JobState::Draining as i32) {
        entry.left_running = view.and_then(|v| v.left_running);
        entry.shell_exited = view.is_some_and(|v| v.shell_exited).then_some(true);
        // The shell's kind 5 exit while the released job drains
        // (tool-job-controller.md `JobEntry` fields 12 and 13).
        if let Some((kind, value)) = view.filter(|v| v.shell_exited).and_then(|v| v.shell_exit) {
            entry.exit_kind = Some(u32::from(kind));
            entry.exit_value = Some(value);
        }
    }
}

/// One job's entry: settled facts from kind 28 once it committed; live
/// facts from the controller's view while unsettled. A job whose kind 28
/// the controller holds as committed but the read did not yet see is
/// `finalizing` (the journal is the source of settled facts). The command
/// is the listing's excerpt, which the journal worker builds (storage owns
/// `command_excerpt`).
fn entry_from_listing(listing: &journal::JobListing, view: Option<&JobView>) -> pb::JobEntry {
    let mut entry = pb::JobEntry {
        job_id: Some(listing.job.to_vec()),
        project_id: Some(listing.project.to_vec()),
        conversation_id: Some(listing.conversation.to_vec()),
        command: Some(listing.excerpt.clone()),
        intent_unix_ms: Some(listing.intent.time),
        sequence: Some(listing.intent_frame.sequence),
        cancel_requested: Some(listing.cancel.is_some()),
        pinned: Some(listing.retention.pinned),
        deleted: Some(listing.retention.deleted),
        notes: Some(listing.retention.notes),
        ..Default::default()
    };
    origin_fields(&mut entry, &listing.intent.origin);
    if let Some(cancel) = &listing.cancel {
        entry.reason = Some(u32::from(cancel.reason as u8));
    }
    match (&listing.settled, view) {
        (Some(s), _) => {
            entry.state = Some(pb::JobState::Settled as i32);
            entry.outcome = Some(u32::from(s.outcome as u8));
            entry.reason = Some(u32::from(s.reason as u8));
            entry.exit_kind = Some(u32::from(s.exit_kind));
            entry.exit_value = Some(s.exit_value);
            entry.stdout_bytes = Some(s.stdout.capture.stored_bytes);
            entry.stderr_bytes = Some(s.stderr.capture.stored_bytes);
            entry.stdout_lines = Some(s.stdout.lines);
            entry.stderr_lines = Some(s.stderr.lines);
            entry.settled_unix_ms = Some(s.settled_time);
            entry.elapsed_ms = Some(s.elapsed_ms);
            entry.capture_state = Some(u32::from(s.capture_state as u8));
        }
        (None, Some(view)) if !view.settled() => apply_view(&mut entry, view),
        (None, _) => {
            entry.state = Some(pb::JobState::Finalizing as i32);
            entry.stdout_bytes = Some(0);
            entry.stderr_bytes = Some(0);
        }
    }
    if view.is_some_and(|v| v.profile == Profile::PersonOutside1) {
        outside_fields(&mut entry, view, listing.settled.as_deref());
    }
    entry
}

/// An entry from the controller alone, while the journal is unavailable
/// (J4 failure handling: the live list works from memory). Its command is
/// the excerpt the controller built at admission (live-only command
/// excerpt); it has no retention state. `None` for a job whose kind 26 has
/// not committed or that settled.
fn entry_from_view(view: &JobView) -> Option<pb::JobEntry> {
    if view.settled() {
        return None;
    }
    let mut entry = pb::JobEntry {
        job_id: Some(view.job.to_vec()),
        project_id: Some(view.project.to_vec()),
        conversation_id: Some(view.agent.conversation.to_vec()),
        command: Some(view.excerpt.clone()),
        intent_unix_ms: Some(view.intent_ms),
        sequence: Some(view.sequence.filter(|s| *s > 0)?),
        cancel_requested: Some(false),
        ..Default::default()
    };
    origin_fields(&mut entry, &view.origin.journal());
    apply_view(&mut entry, view);
    if view.profile == Profile::PersonOutside1 {
        outside_fields(&mut entry, Some(view), None);
    }
    Some(entry)
}

// ----- Pages -----

/// The stored bytes the handler knows of a stream: kind 28's for a settled
/// job, the controller's view for a live one.
fn known_total(facts: Option<&Facts>, view: Option<&JobView>, stream: OutputStream) -> u64 {
    if let Some(s) = facts.and_then(|f| f.records.listing.settled.as_deref()) {
        return match stream {
            OutputStream::Stdout => s.stdout.capture.stored_bytes,
            OutputStream::Stderr => s.stderr.capture.stored_bytes,
        };
    }
    view.map_or(0, |v| match stream {
        OutputStream::Stdout => v.snapshot.stdout_bytes,
        OutputStream::Stderr => v.snapshot.stderr_bytes,
    })
}

fn stream_code(stream: OutputStream) -> i32 {
    match stream {
        OutputStream::Stdout => pb::JobStream::Stdout as i32,
        OutputStream::Stderr => pb::JobStream::Stderr as i32,
    }
}

fn empty_page(
    stream: OutputStream,
    at: u64,
    total: u64,
    state: pb::JobPageState,
) -> pb::JobStreamPage {
    pb::JobStreamPage {
        stream: Some(stream_code(stream)),
        start: Some(at),
        text: Some(String::new()),
        next_offset: Some(at),
        total_bytes: Some(total),
        state: Some(state as i32),
    }
}

/// A page reply as a `JobStreamPage`; `Err` with the error token for a
/// read that failed. `live` is the job's liveness for a stale cursor.
fn stream_page(
    stream: OutputStream,
    offset: u64,
    reply: PageReply,
    known: u64,
    live: bool,
) -> Result<pb::JobStreamPage, &'static str> {
    use pb::JobPageState as S;
    let PageReply::Page(page) = reply else {
        return Err(wire::error::UNAVAILABLE);
    };
    let Page {
        state,
        start,
        text,
        next_offset,
        total,
        ..
    } = page;
    let readable = match state {
        PageState::Live => S::Live,
        PageState::Complete => S::Complete,
        PageState::Incomplete => S::Incomplete,
        PageState::UnverifiedPrefix => S::UnverifiedPrefix,
        PageState::Expired | PageState::Missing | PageState::Corrupt | PageState::Deleted => {
            let state = match state {
                PageState::Expired => S::Expired,
                PageState::Missing => S::Missing,
                PageState::Corrupt => S::Corrupt,
                _ => S::Deleted,
            };
            return Ok(empty_page(stream, offset.min(known), known, state));
        }
        PageState::StaleCursor => {
            // The offset is past the stored bytes: an empty page at the end.
            let state = if live { S::Live } else { S::Complete };
            return Ok(empty_page(stream, total, total, state));
        }
        PageState::Busy | PageState::Denied => return Err(wire::error::UNAVAILABLE),
    };
    if next_offset < start || next_offset > total {
        return Err(wire::error::UNAVAILABLE);
    }
    Ok(pb::JobStreamPage {
        stream: Some(stream_code(stream)),
        start: Some(start),
        text: Some(text),
        next_offset: Some(next_offset),
        total_bytes: Some(total),
        state: Some(readable as i32),
    })
}

// ----- Request kinds -----

enum Work {
    List(ListWork),
    Inspect(InspectWork),
    View(ViewWork),
    Cancel(CancelWork),
    Retain(RetainWork),
}

/// Everything a step may touch besides its own work.
struct Step<'a, 'b> {
    key: u64,
    ctx: &'a mut Ctx<'b>,
    known: &'a [(Id, bool)],
    body: &'a Body,
    retained: &'a mut Vec<Retained>,
}
impl Step<'_, '_> {
    fn registry(&self, project: Id) -> Registry {
        registry(self.known, self.ctx.projects, project)
    }
    fn view(&self, job: Id) -> Option<JobView> {
        self.ctx.jobs.view(job, self.ctx.now)
    }
    fn error(&self, token: &str) -> Next {
        Next::Reply(Box::new(error_body(self.body, token)))
    }
    fn journal_error(&self, error: writer::Error) -> Next {
        Next::Reply(Box::new(read_error(self.body, error)))
    }
}

/// Unwraps a `JobFacts` reply.
fn facts_of(result: Box<writer::Result<writer::Reply>>) -> writer::Result<Option<Facts>> {
    (*result).map(|reply| reply.job_records.map(|records| Facts { records }))
}

// ----- JobList -----

#[derive(Clone, Copy, PartialEq, Eq)]
enum ListPhase {
    Park,
    Registry,
    /// A live-only list with `before`: one `Jobs` read for `before_found`.
    Cursor,
    CursorRead,
    Gather,
    GatherRead,
}

/// One accepted row: its listing from a `Jobs` read, or `None` for a
/// live-only row built from the controller's view.
struct Row {
    sequence: u64,
    job: Id,
    listing: Option<Box<journal::JobListing>>,
}

struct ListWork {
    scope: JobScope,
    project: Option<Id>,
    before: Option<u64>,
    limit: usize,
    live_only: bool,
    after_revision: Option<u64>,
    phase: ListPhase,
    /// Accepted rows, newest first, at most `limit + 1`.
    rows: Vec<Row>,
    /// The next journal page starts below this sequence.
    raw_before: Option<u64>,
    /// Journal listings not yet checked against the registry.
    unchecked: VecDeque<journal::JobListing>,
    raw_done: bool,
    /// The cursor was checked (`before_found`), or there is none.
    cursor_checked: bool,
}

impl ListWork {
    fn new(v: &pb::JobList) -> Self {
        let project = optional_id(&v.project_id);
        let scope = match v.scope {
            Some(3) => JobScope::Conversation(id(&v.conversation_id)),
            Some(1) => JobScope::Project(project.unwrap_or([0; 16])),
            _ => JobScope::AllProjects,
        };
        Self {
            scope,
            project,
            before: v.before,
            limit: v
                .limit
                .unwrap_or(wire::MAX_JOB_LIST_ENTRIES)
                .clamp(1, wire::MAX_JOB_LIST_ENTRIES) as usize,
            live_only: v.live_only == Some(true),
            after_revision: v.after_revision,
            phase: if v.after_revision.is_some() {
                ListPhase::Park
            } else {
                ListPhase::Registry
            },
            rows: Vec::new(),
            raw_before: v.before,
            unchecked: VecDeque::new(),
            raw_done: false,
            cursor_checked: v.before.is_none(),
        }
    }

    fn step(&mut self, s: &mut Step, mut input: Input) -> Next {
        loop {
            match self.phase {
                ListPhase::Park => {
                    if let Input::Wait = input {
                        self.phase = ListPhase::Registry;
                        continue;
                    }
                    let after = self.after_revision.unwrap_or(0);
                    match s.ctx.jobs.park_list(s.key, after) {
                        WaitStart::Parked => return Next::Park,
                        WaitStart::Busy => return s.error(wire::error::BUSY),
                        WaitStart::Done(_) | WaitStart::NotLive => {
                            self.phase = ListPhase::Registry;
                        }
                    }
                }
                ListPhase::Registry => {
                    if let Some(project) = self.project {
                        match s.registry(project) {
                            Registry::No => return s.error(wire::error::NOT_FOUND),
                            Registry::Read(p) => return Next::Read(project_read(p)),
                            Registry::Yes => {}
                        }
                    }
                    self.phase = if self.live_only {
                        ListPhase::Cursor
                    } else {
                        ListPhase::Gather
                    };
                }
                // A live-only list reads no listings; its cursor still
                // names a kind 26 sequence in scope (`before_found`).
                ListPhase::Cursor => match self.before {
                    Some(before) if !self.cursor_checked => {
                        self.phase = ListPhase::CursorRead;
                        return Next::Read(writer::Command::Jobs {
                            scope: self.scope,
                            before: Some(before),
                            limit: 1,
                        });
                    }
                    _ => self.phase = ListPhase::Gather,
                },
                ListPhase::CursorRead => {
                    let Input::Journal(result) = std::mem::replace(&mut input, Input::Resume)
                    else {
                        return s.error(wire::error::UNAVAILABLE);
                    };
                    match *result {
                        Ok(reply) if !reply.before_found => {
                            return s.error(wire::error::INVALID_CURSOR);
                        }
                        // The live list works from memory: without the
                        // journal the check is skipped.
                        Ok(_) | Err(writer::Error::Unavailable) => {}
                        Err(e) => return s.journal_error(e),
                    }
                    self.cursor_checked = true;
                    self.phase = ListPhase::Gather;
                }
                ListPhase::Gather => {
                    if self.live_only {
                        let mut views = s.ctx.jobs.live_views(Some(self.scope), s.ctx.now);
                        views.retain(|v| {
                            v.sequence
                                .is_some_and(|q| self.before.is_none_or(|b| q < b))
                                && self.in_scope_project(v.project)
                        });
                        self.rows = views
                            .into_iter()
                            .take(self.limit + 1)
                            .map(|v| Row {
                                sequence: v.sequence.unwrap_or(0),
                                job: v.job,
                                listing: None,
                            })
                            .collect();
                        return Next::Reply(Box::new(Body::JobListReply(self.reply(s))));
                    }
                    // Check the listings already read against the registry.
                    while let Some(listing) = self.unchecked.front() {
                        if self.rows.len() > self.limit {
                            break;
                        }
                        let project = listing.project;
                        let keep = match self.scope {
                            JobScope::AllProjects => match s.registry(project) {
                                Registry::Yes => true,
                                Registry::No => false,
                                Registry::Read(p) => return Next::Read(project_read(p)),
                            },
                            _ => self.in_scope_project(project),
                        };
                        let listing = self.unchecked.pop_front().expect("front");
                        if keep {
                            self.rows.push(Row {
                                sequence: listing.intent_frame.sequence,
                                job: listing.job,
                                listing: Some(Box::new(listing)),
                            });
                        }
                    }
                    if self.rows.len() > self.limit || self.raw_done {
                        self.unchecked.clear();
                        return Next::Reply(Box::new(Body::JobListReply(self.reply(s))));
                    }
                    self.phase = ListPhase::GatherRead;
                    return Next::Read(writer::Command::Jobs {
                        scope: self.scope,
                        before: self.raw_before,
                        limit: journal::MAX_JOB_LISTING_READ,
                    });
                }
                ListPhase::GatherRead => {
                    let Input::Journal(result) = std::mem::replace(&mut input, Input::Resume)
                    else {
                        return s.error(wire::error::UNAVAILABLE);
                    };
                    let reply = match *result {
                        Ok(reply) => reply,
                        Err(e) => return s.journal_error(e),
                    };
                    // The first page is read below the cursor: its
                    // `before_found` is the cursor check.
                    if !self.cursor_checked {
                        if !reply.before_found {
                            return s.error(wire::error::INVALID_CURSOR);
                        }
                        self.cursor_checked = true;
                    }
                    let jobs = reply.jobs;
                    self.raw_done = jobs.len() < journal::MAX_JOB_LISTING_READ;
                    match jobs.last() {
                        Some(last) => self.raw_before = Some(last.intent_frame.sequence),
                        None => self.raw_done = true,
                    }
                    // Strictly newest first, below the cursor: a page that
                    // does not advance ends the gathering.
                    let floor = self.rows.last().map(|r| r.sequence);
                    self.unchecked.extend(jobs.into_iter().filter(|l| {
                        let q = l.intent_frame.sequence;
                        floor.is_none_or(|f| q < f) && self.before.is_none_or(|b| q < b)
                    }));
                    self.phase = ListPhase::Gather;
                }
            }
        }
    }

    fn in_scope_project(&self, project: Id) -> bool {
        match self.scope {
            JobScope::AllProjects => true,
            _ => self.project.is_none_or(|p| p == project),
        }
    }

    fn reply(&mut self, s: &mut Step) -> pb::JobListReply {
        let shown = self.rows.len().min(self.limit);
        let more = self.rows.len() > self.limit;
        let mut entries: Vec<pb::JobEntry> = Vec::with_capacity(shown);
        for row in self.rows.iter().take(shown) {
            let view = s.view(row.job);
            let entry = match &row.listing {
                Some(listing) => Some(entry_from_listing(listing, view.as_ref())),
                None => view.as_ref().and_then(entry_from_view),
            };
            // One entry per job, strictly newest first.
            if let Some(entry) = entry
                && entries
                    .last()
                    .is_none_or(|last| last.sequence > entry.sequence)
            {
                entries.push(entry);
            }
        }
        let jobs = &*s.ctx.jobs;
        let service = jobs.live_count(None);
        let scope = jobs.live_count(Some(self.scope)).min(service);
        let next_before = if more {
            entries.last().and_then(|e| e.sequence)
        } else {
            None
        };
        let execution = match self.scope {
            JobScope::AllProjects => None,
            _ => self.project.map(|p| s.ctx.projects.execution_enabled(p)),
        };
        pb::JobListReply {
            entries,
            next_before,
            revision: Some(jobs.revision()),
            live_in_scope: Some(scope),
            live_in_service: Some(service),
            execution_enabled: execution,
            error: None,
        }
    }
}

fn project_read(project: Id) -> writer::Command {
    writer::Command::Project { project }
}

// ----- JobInspect -----

struct InspectWork {
    job: Id,
    fence: Option<Id>,
    facts: Option<Facts>,
    /// `JobDetail.path_warnings`: a person job's warnings from its input's
    /// kind 32 acknowledgement; `None` until read (a model job has none).
    warnings: Option<Vec<pb::PathWarning>>,
    /// The `PathRecord` read is in flight.
    path_read: bool,
}
impl InspectWork {
    fn step(&mut self, s: &mut Step, input: Input) -> Next {
        match input {
            Input::Start => return Next::Read(writer::Command::JobFacts { job: self.job }),
            Input::Journal(result) if self.path_read => {
                self.path_read = false;
                match *result {
                    Ok(reply) => {
                        let project = self.facts.as_ref().map(Facts::project);
                        self.warnings = Some(detail_path_warnings(
                            reply.path_record.as_ref().map(|(_, r)| r.as_ref()),
                            project,
                        ));
                    }
                    Err(e) => return s.journal_error(e),
                }
            }
            Input::Journal(result) => match facts_of(result) {
                Ok(Some(facts)) => self.facts = Some(facts),
                Ok(None) => return s.error(wire::error::NOT_FOUND),
                Err(e) => return s.journal_error(e),
            },
            _ => {}
        }
        let Some(facts) = &self.facts else {
            return s.error(wire::error::UNAVAILABLE);
        };
        if self.fence.is_some_and(|p| p != facts.project()) {
            return s.error(wire::error::PROJECT_MISMATCH);
        }
        match s.registry(facts.project()) {
            Registry::No => return s.error(wire::error::NOT_FOUND),
            Registry::Read(p) => return Next::Read(project_read(p)),
            Registry::Yes => {}
        }
        // A person job's warnings: one bounded `PathRecord` read of its
        // input's kind 32 on the journal worker (at most 16 entries).
        if self.warnings.is_none() {
            match facts.origin() {
                JobOrigin::Person { input } => {
                    self.path_read = true;
                    return Next::Read(writer::Command::PathRecord { reference: input });
                }
                JobOrigin::Model { .. } => self.warnings = Some(Vec::new()),
            }
        }
        let view = s.view(self.job);
        let records = &facts.records;
        let command = records.command.command().unwrap_or_default();
        let cwd = s
            .ctx
            .projects
            .location(facts.project())
            .zip(match &records.command {
                JobCommand::Model(call) => exec_cmd_cwd(call),
                JobCommand::Person(command) => Some(command.cwd.clone()),
            })
            .and_then(|(location, cwd)| absolute_cwd(&location, &cwd));
        let notes = detail_notes(records);
        Next::Reply(Box::new(Body::JobDetail(pb::JobDetail {
            entry: Some(entry_from_facts(facts, view.as_ref())),
            command: Some(cut(&command, wire::MAX_JOB_COMMAND_BYTES).replace('\0', " ")),
            cwd,
            notes,
            error: None,
            path_warnings: self.warnings.take().unwrap_or_default(),
        })))
    }
}

/// `JobDetail.path_warnings` from a person input's kind 32: the entries of
/// its `acknowledged` record in the job's project, as stored (display-
/// redacted, at most 16). Any other record, or none (the command ran
/// before the guard, or was never checked), gives no warnings. The record
/// keeps no listing overflow field in `JobDetail`; the entries alone are
/// returned.
fn detail_path_warnings(
    record: Option<&journal::ExecutionPathRecord>,
    project: Option<Id>,
) -> Vec<pb::PathWarning> {
    let Some(record) = record
        .filter(|r| r.action == journal::PathAction::Acknowledged && Some(r.project) == project)
    else {
        return Vec::new();
    };
    record
        .entries
        .iter()
        .map(|e| pb::PathWarning {
            kind: Some(e.kind as u32),
            path: Some(e.path.clone()),
        })
        .filter(asura_control::path_guard::warning_valid)
        .take(asura_control::path_guard::MAX_PATH_WARNINGS)
        .collect()
}

/// `JobDetail.notes`: person notes (author 1) with their kind 30 text and
/// model notes (author 2) with the cited kind 14 `text` from
/// `JobRecords.model_notes`, together, oldest first by kind 30 sequence,
/// at most 16.
fn detail_notes(records: &JobRecords) -> Vec<pb::JobNote> {
    let mut notes: Vec<(u64, u32, u64, &str)> = records
        .retention
        .iter()
        .filter(|(_, r)| r.action == RetentionAction::Note)
        .filter_map(|(frame, r)| match r.author {
            RetentionAuthor::Person { .. } => Some((frame.sequence, 1, r.time, r.note.as_str())),
            RetentionAuthor::Model { .. } => records
                .model_notes
                .iter()
                .find(|(at, _)| at == frame)
                .map(|(_, text)| (frame.sequence, 2, r.time, text.as_str())),
        })
        .filter(|(.., text)| wire::note_text_valid(text))
        .collect();
    notes.sort_by_key(|(sequence, ..)| *sequence);
    notes
        .into_iter()
        .take(wire::MAX_JOB_NOTES as usize)
        .map(|(_, author, time, text)| pb::JobNote {
            author: Some(author),
            unix_ms: Some(time),
            text: Some(text.to_owned()),
        })
        .collect()
}

// ----- JobRead and JobObserve -----

#[derive(Clone, Copy)]
enum Streams {
    /// One stream: the offset (`None` with `at_end` on a read), whether to
    /// follow the end, and the page limit.
    One {
        stream: OutputStream,
        offset: Option<u64>,
        at_end: bool,
        limit: u32,
    },
    /// Both streams at their end (`JobObserve` with `at_end` and no stream).
    Both,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ViewPhase {
    Start,
    Parked,
    FactsRead,
    Checked,
    Pages,
}

struct ViewWork {
    job: Id,
    fence: Option<Id>,
    streams: Streams,
    /// `JobObserve`: parks first; `after_state` is the client's state code.
    observe: Option<Option<u8>>,
    phase: ViewPhase,
    facts: Option<Facts>,
}
impl ViewWork {
    fn read(v: &pb::JobRead) -> Self {
        Self {
            job: id(&v.job_id),
            fence: optional_id(&v.project_id),
            streams: Streams::One {
                stream: OutputStream::from_code(v.stream.unwrap_or(1) as u8)
                    .unwrap_or(OutputStream::Stdout),
                offset: v.offset,
                at_end: v.at_end == Some(true),
                limit: v
                    .limit
                    .unwrap_or(wire::MAX_JOB_PAGE_BYTES)
                    .clamp(1, wire::MAX_JOB_PAGE_BYTES),
            },
            observe: None,
            phase: ViewPhase::Start,
            facts: None,
        }
    }
    fn observe(v: &pb::JobObserve) -> Self {
        let streams = match v.stream.and_then(|s| OutputStream::from_code(s as u8)) {
            Some(stream) => Streams::One {
                stream,
                offset: v.offset,
                at_end: v.at_end == Some(true),
                limit: wire::MAX_JOB_PAGE_BYTES,
            },
            None => Streams::Both,
        };
        Self {
            job: id(&v.job_id),
            fence: optional_id(&v.project_id),
            streams,
            observe: Some(v.after_state.and_then(|s| u8::try_from(s).ok())),
            phase: ViewPhase::Start,
            facts: None,
        }
    }

    fn step(&mut self, s: &mut Step, input: Input) -> Next {
        match (self.phase, input) {
            (ViewPhase::Start, _) => {
                if let Some(after) = self.observe
                    && let Some(view) = s.view(self.job)
                {
                    if self.fence.is_some_and(|p| p != view.project) {
                        return s.error(wire::error::PROJECT_MISMATCH);
                    }
                    match s.registry(view.project) {
                        Registry::No => return s.error(wire::error::NOT_FOUND),
                        Registry::Read(p) => return Next::Read(project_read(p)),
                        Registry::Yes => {}
                    }
                    let stream = match self.streams {
                        Streams::One { stream, offset, .. } => Some((stream, offset.unwrap_or(0))),
                        Streams::Both => None,
                    };
                    match s
                        .ctx
                        .jobs
                        .observe(s.key, self.job, stream, after.and_then(state_of_code))
                    {
                        WaitStart::Parked => {
                            self.phase = ViewPhase::Parked;
                            return Next::Park;
                        }
                        WaitStart::Busy => return s.error(wire::error::BUSY),
                        WaitStart::Done(_) | WaitStart::NotLive => {}
                    }
                }
                self.phase = ViewPhase::FactsRead;
                Next::Read(writer::Command::JobFacts { job: self.job })
            }
            (ViewPhase::Parked, _) => {
                self.phase = ViewPhase::FactsRead;
                Next::Read(writer::Command::JobFacts { job: self.job })
            }
            (ViewPhase::FactsRead, Input::Journal(result)) => {
                match facts_of(result) {
                    Ok(Some(facts)) => self.facts = Some(facts),
                    Ok(None) => return s.error(wire::error::NOT_FOUND),
                    // A live job's pages are read without the journal.
                    Err(e) => {
                        if !s.view(self.job).is_some_and(|v| !v.settled()) {
                            return s.journal_error(e);
                        }
                    }
                }
                self.phase = ViewPhase::Checked;
                self.check(s)
            }
            (ViewPhase::Checked, Input::Resume) => self.check(s),
            (ViewPhase::Pages, Input::Pages(replies)) => self.reply(s, replies),
            _ => s.error(wire::error::UNAVAILABLE),
        }
    }

    fn project(&self, s: &Step) -> Option<Id> {
        match &self.facts {
            Some(f) => Some(f.project()),
            None => s.view(self.job).map(|v| v.project),
        }
    }

    fn check(&mut self, s: &mut Step) -> Next {
        let Some(project) = self.project(s) else {
            return s.error(wire::error::NOT_FOUND);
        };
        if self.fence.is_some_and(|p| p != project) {
            return s.error(wire::error::PROJECT_MISMATCH);
        }
        match s.registry(project) {
            Registry::No => return s.error(wire::error::NOT_FOUND),
            Registry::Read(p) => return Next::Read(project_read(p)),
            Registry::Yes => {}
        }
        let view = s.view(self.job);
        let streams: Vec<(OutputStream, u64, u32)> = match self.streams {
            Streams::One {
                stream,
                offset,
                at_end,
                limit,
            } => {
                let total = known_total(self.facts.as_ref(), view.as_ref(), stream);
                let end = total.saturating_sub(u64::from(limit));
                let offset = match (offset, at_end) {
                    (Some(offset), true) => offset.max(end),
                    (Some(offset), false) => offset,
                    (None, _) => end,
                };
                vec![(stream, offset, limit)]
            }
            Streams::Both => [OutputStream::Stdout, OutputStream::Stderr]
                .into_iter()
                .map(|stream| {
                    let total = known_total(self.facts.as_ref(), view.as_ref(), stream);
                    let limit = wire::MAX_JOB_PAGE_BYTES;
                    (stream, total.saturating_sub(u64::from(limit)), limit)
                })
                .collect(),
        };
        // No payload exists before the spawn, nor after a settlement with
        // no process: an empty page, without a read.
        let no_payload = match (&self.facts, &view) {
            (Some(f), _) if f.settled() => f
                .records
                .listing
                .settled
                .as_deref()
                .is_some_and(|s| s.capture_state == journal::JobCaptureState::NotStarted),
            (_, Some(v)) => v.snapshot.state.before_spawn(),
            _ => false,
        };
        if no_payload {
            let state = if self.facts.as_ref().is_some_and(Facts::settled) {
                pb::JobPageState::Complete
            } else {
                pb::JobPageState::Live
            };
            let pages = streams
                .iter()
                .map(|(stream, _, _)| empty_page(*stream, 0, 0, state))
                .collect();
            return self.page_reply(s, pages);
        }
        let payloads = payload::job_payloads(s.ctx.installation, self.job);
        self.phase = ViewPhase::Pages;
        Next::Pages(
            streams
                .into_iter()
                .map(|(stream, offset, limit)| {
                    let index = match stream {
                        OutputStream::Stdout => 0,
                        OutputStream::Stderr => 1,
                    };
                    (
                        stream,
                        PageRequest {
                            payload: payloads[index],
                            offset,
                            limit: limit as usize,
                            scope: None,
                        },
                    )
                })
                .collect(),
        )
    }

    fn reply(&mut self, s: &mut Step, replies: Vec<(OutputStream, u64, PageReply)>) -> Next {
        let view = s.view(self.job);
        let live = !self.facts.as_ref().is_some_and(Facts::settled);
        let mut pages = Vec::with_capacity(replies.len());
        for (stream, offset, reply) in replies {
            let known = known_total(self.facts.as_ref(), view.as_ref(), stream);
            match stream_page(stream, offset, reply, known, live) {
                Ok(page) => pages.push(page),
                Err(token) => return s.error(token),
            }
        }
        self.page_reply(s, pages)
    }

    fn page_reply(&self, s: &mut Step, pages: Vec<pb::JobStreamPage>) -> Next {
        let view = s.view(self.job);
        let entry = match &self.facts {
            Some(facts) => Some(entry_from_facts(facts, view.as_ref())),
            None => view.as_ref().and_then(entry_from_view),
        };
        let Some(entry) = entry else {
            return s.error(wire::error::UNAVAILABLE);
        };
        Next::Reply(Box::new(Body::JobPage(pb::JobPage {
            entry: Some(entry),
            pages,
            error: None,
        })))
    }
}

/// A `JobRunState` with the given state code; only the code is compared.
fn state_of_code(code: u8) -> Option<JobRunState> {
    Some(match code {
        1 => JobRunState::Queued(JobBlockReason::AgentSlots),
        2 => JobRunState::Starting,
        3 => JobRunState::Running,
        4 => JobRunState::CapturePaused(JobPauseReason::Quota),
        5 => JobRunState::Stopping,
        6 => JobRunState::Draining,
        7 => JobRunState::CleanupUnconfirmed,
        8 => JobRunState::Finalizing,
        9 => JobRunState::Settled,
        _ => return None,
    })
}

// ----- JobCancel -----

#[derive(Clone, Copy, PartialEq, Eq)]
enum CancelPhase {
    Start,
    FactsBefore,
    FactsAfter,
    Settling,
}

struct CancelWork {
    job: Id,
    expected: Id,
    kill: bool,
    phase: CancelPhase,
    facts: Option<Facts>,
    reply: Option<CancelReply>,
    rereads: u8,
}
impl CancelWork {
    fn step(&mut self, s: &mut Step, input: Input) -> Next {
        match (self.phase, input) {
            (CancelPhase::Start, _) => {
                let Some(view) = s.view(self.job) else {
                    self.phase = CancelPhase::FactsBefore;
                    return Next::Read(writer::Command::JobFacts { job: self.job });
                };
                if let Some(next) = self.checks(s, view.project, &view.origin.journal()) {
                    return next;
                }
                self.cancel(s, &view.origin.journal())
            }
            (CancelPhase::FactsBefore, Input::Journal(result)) => {
                match facts_of(result) {
                    Ok(Some(facts)) => self.facts = Some(facts),
                    Ok(None) => return s.error(wire::error::NOT_FOUND),
                    Err(_) => return s.error(wire::error::UNAVAILABLE),
                }
                self.before_cancel(s)
            }
            (CancelPhase::FactsBefore, Input::Resume) => self.before_cancel(s),
            (CancelPhase::FactsAfter, Input::Journal(result)) => {
                match facts_of(result) {
                    Ok(Some(facts)) => self.facts = Some(facts),
                    Ok(None) => return s.error(wire::error::NOT_FOUND),
                    Err(_) => self.facts = None,
                }
                self.reply(s)
            }
            (CancelPhase::Settling, _) => {
                self.phase = CancelPhase::FactsAfter;
                Next::Read(writer::Command::JobFacts { job: self.job })
            }
            _ => s.error(wire::error::UNAVAILABLE),
        }
    }

    /// The fence, the kill rule and the registry, before any effect.
    fn checks(&self, s: &Step, project: Id, origin: &JobOrigin) -> Option<Next> {
        if project != self.expected {
            return Some(s.error(wire::error::PROJECT_MISMATCH));
        }
        if self.kill && model_origin(origin) {
            return Some(s.error(wire::error::KILL_NOT_PERMITTED));
        }
        match s.registry(project) {
            Registry::No => Some(s.error(wire::error::NOT_FOUND)),
            Registry::Read(p) => Some(Next::Read(project_read(p))),
            Registry::Yes => None,
        }
    }

    fn before_cancel(&mut self, s: &mut Step) -> Next {
        let Some(facts) = &self.facts else {
            return s.error(wire::error::UNAVAILABLE);
        };
        let origin = facts.origin();
        if let Some(next) = self.checks(s, facts.project(), &origin) {
            return next;
        }
        if facts.settled() {
            self.reply = Some(CancelReply::AlreadySettled(
                facts
                    .records
                    .listing
                    .settled
                    .as_deref()
                    .map_or(journal::JobOutcome::Interrupted, |s| s.outcome),
            ));
            return self.reply(s);
        }
        self.cancel(s, &origin)
    }

    /// The controller call: requester client, `client_cancel`, for a model
    /// job; requester person, `person_stop` or `person_kill`, for a person
    /// job. Then the entry is read.
    fn cancel(&mut self, s: &mut Step, origin: &JobOrigin) -> Next {
        let (by, reason) = if model_origin(origin) {
            (Requester::Client, JobReason::ClientCancel)
        } else if self.kill {
            (Requester::Person, JobReason::PersonKill)
        } else {
            (Requester::Person, JobReason::PersonStop)
        };
        let reply = s.ctx.jobs.cancel(self.job, by, reason, self.kill);
        match reply {
            CancelReply::NotFound => return s.error(wire::error::UNAVAILABLE),
            CancelReply::NotPermitted if self.kill => {
                return s.error(wire::error::KILL_NOT_PERMITTED);
            }
            CancelReply::NotPermitted => return s.error(wire::error::UNAVAILABLE),
            _ => {}
        }
        self.reply = Some(reply);
        self.phase = CancelPhase::FactsAfter;
        Next::Read(writer::Command::JobFacts { job: self.job })
    }

    fn reply(&mut self, s: &mut Step) -> Next {
        let view = s.view(self.job);
        let settled_reply = matches!(self.reply, Some(CancelReply::AlreadySettled(_)));
        let entry = match &self.facts {
            Some(facts) => {
                if settled_reply && !facts.settled() {
                    // Kind 28 is built but its commit was not yet read:
                    // wait for the settlement, then read again.
                    if self.rereads < SETTLE_REREADS {
                        self.rereads += 1;
                        let ending = s.ctx.jobs.observe(
                            s.key,
                            self.job,
                            Some((OutputStream::Stdout, u64::MAX)),
                            None,
                        );
                        match ending {
                            WaitStart::Parked => {
                                self.phase = CancelPhase::Settling;
                                return Next::Park;
                            }
                            WaitStart::Done(_) | WaitStart::NotLive => {
                                self.phase = CancelPhase::FactsAfter;
                                return Next::Read(writer::Command::JobFacts { job: self.job });
                            }
                            // No wait place: the live entry, `requested: true`.
                            WaitStart::Busy => {}
                        }
                    }
                }
                entry_from_facts(facts, view.as_ref())
            }
            // The journal is unavailable: the entry from memory.
            None if !settled_reply => match view.as_ref().and_then(entry_from_view) {
                Some(entry) => entry,
                None => return s.error(wire::error::UNAVAILABLE),
            },
            None => return s.error(wire::error::UNAVAILABLE),
        };
        // `requested` reports the cancel that this request made: an accepted
        // trigger is `true` even when the job settled at once (a queued
        // start settles `not_started` in the same step); `false` only for a
        // job that had settled before, whose entry is then SETTLED.
        let requested = !settled_reply || entry.state != Some(pb::JobState::Settled as i32);
        let request_id = match s.body {
            Body::JobCancel(v) => v.request_id.clone(),
            _ => None,
        };
        Next::Reply(Box::new(Body::JobCancelReply(pb::JobCancelReply {
            request_id,
            entry: Some(entry),
            requested: Some(requested),
            error: None,
        })))
    }
}

// ----- JobRetain -----

#[derive(Clone, Copy, PartialEq, Eq)]
enum RetainPhase {
    Start,
    FactsRead,
    Checked,
    Appending,
}

struct RetainWork {
    job: Id,
    expected: Id,
    action: RetentionAction,
    note: Option<String>,
    /// `note` holds the redacted text.
    redacted: bool,
    request: Id,
    phase: RetainPhase,
    facts: Option<Facts>,
}
impl RetainWork {
    fn new(v: &pb::JobRetain) -> Self {
        Self {
            job: id(&v.job_id),
            expected: id(&v.expected_project_id),
            action: RetentionAction::from_code(v.action.unwrap_or(0) as u8)
                .unwrap_or(RetentionAction::Pin),
            note: v.note.clone(),
            redacted: false,
            request: id(&v.request_id),
            phase: RetainPhase::Start,
            facts: None,
        }
    }

    fn step(&mut self, s: &mut Step, input: Input) -> Next {
        match (self.phase, input) {
            (RetainPhase::Start, _) => {
                self.phase = RetainPhase::FactsRead;
                Next::Read(writer::Command::JobFacts { job: self.job })
            }
            (RetainPhase::FactsRead, Input::Journal(result)) => {
                match facts_of(result) {
                    Ok(Some(facts)) => self.facts = Some(facts),
                    Ok(None) => return s.error(wire::error::NOT_FOUND),
                    Err(_) => return s.error(wire::error::UNAVAILABLE),
                }
                self.phase = RetainPhase::Checked;
                self.check(s)
            }
            (RetainPhase::Checked, Input::Resume) => self.check(s),
            (RetainPhase::Appending, Input::Journal(result)) => match *result {
                Ok(_) => self.committed(s),
                Err(writer::Error::JournalQuota | writer::Error::JournalDiskSpace) => {
                    // Held at the journal quota: nothing was written; wait
                    // for room (J1-0) and append again.
                    match self.record() {
                        Ok(record) => Next::Hold(record),
                        Err(token) => s.error(token),
                    }
                }
                Err(_) => s.error(wire::error::UNAVAILABLE),
            },
            _ => s.error(wire::error::UNAVAILABLE),
        }
    }

    fn check(&mut self, s: &mut Step) -> Next {
        let Some(facts) = &self.facts else {
            return s.error(wire::error::UNAVAILABLE);
        };
        if facts.project() != self.expected {
            return s.error(wire::error::PROJECT_MISMATCH);
        }
        match s.registry(facts.project()) {
            Registry::No => return s.error(wire::error::NOT_FOUND),
            Registry::Read(p) => return Next::Read(project_read(p)),
            Registry::Yes => {}
        }
        let summary = facts.records.listing.retention;
        let settled = facts.settled();
        if self.action == RetentionAction::Delete && !settled {
            return s.error(wire::error::NOT_SETTLED);
        }
        if self.action == RetentionAction::Note && !self.redacted {
            let Some(text) = self.note.as_deref() else {
                return s.error(wire::error::NOTE_INVALID);
            };
            if !wire::note_text_valid(text) {
                return s.error(wire::error::NOTE_INVALID);
            }
            // The rule set is compiled off the reactor; until it is ready
            // the note waits, keeping its retain turn (no time bound).
            let rules = match s.ctx.rules.clone() {
                None => return Next::Rules,
                Some(Err(_)) => return s.error(wire::error::UNAVAILABLE),
                Some(Ok(rules)) => rules,
            };
            match person_note(rules, text) {
                Ok(text) => {
                    self.note = Some(text);
                    self.redacted = true;
                }
                Err(token) => return s.error(token),
            }
        }
        if !summary.allows(self.action, settled) {
            return self.reply(s, false);
        }
        if !summary.has_room(self.action) {
            return s.error(
                if self.action == RetentionAction::Note
                    && summary.notes as usize >= journal::MAX_JOB_NOTES
                {
                    wire::error::NOTES_FULL
                } else {
                    wire::error::RETENTION_FULL
                },
            );
        }
        let record = match self.record() {
            Ok(record) => record,
            Err(token) => return s.error(token),
        };
        self.phase = RetainPhase::Appending;
        if s.ctx.journal_held {
            Next::Hold(record)
        } else {
            Next::Append(record)
        }
    }

    fn record(&self) -> Result<journal::Record, &'static str> {
        Ok(journal::Record::JobRetention(Box::new(JobRetention {
            job: self.job,
            action: self.action,
            author: RetentionAuthor::Person {
                request: self.request,
            },
            time: wall_ms(),
            note: match self.action {
                RetentionAction::Note => self.note.clone().ok_or(wire::error::NOTE_INVALID)?,
                _ => String::new(),
            },
        })))
    }

    /// Kind 30 committed: the job revision changes and the payload owner
    /// applies the pin hold or the deletion.
    fn committed(&mut self, s: &mut Step) -> Next {
        let Some(facts) = &mut self.facts else {
            return s.error(wire::error::UNAVAILABLE);
        };
        facts.records.listing.retention.apply(self.action);
        s.ctx.jobs.bump_revision();
        s.retained.push(Retained {
            job: self.job,
            action: self.action,
        });
        self.reply(s, true)
    }

    fn reply(&self, s: &mut Step, changed: bool) -> Next {
        let Some(facts) = &self.facts else {
            return s.error(wire::error::UNAVAILABLE);
        };
        let view = s.view(self.job);
        let request_id = match s.body {
            Body::JobRetain(v) => v.request_id.clone(),
            _ => None,
        };
        Next::Reply(Box::new(Body::JobRetainReply(pb::JobRetainReply {
            request_id,
            entry: Some(entry_from_facts(facts, view.as_ref())),
            changed: Some(changed),
            error: None,
        })))
    }
}

/// A person note: the text rule, then redaction with each detected secret
/// as the unnumbered placeholder `$ASURA_SECRET_0` (`secrets::note_redacted`:
/// placeholders the person typed stay as typed), then the rule again on the
/// redacted text (kind 30 field 6). `rules` is the ready set; this never
/// compiles it.
pub(crate) fn person_note(rules: &'static RuleSet, text: &str) -> Result<String, &'static str> {
    if !wire::note_text_valid(text) {
        return Err(wire::error::NOTE_INVALID);
    }
    let redacted = secrets::note_redacted(rules, text);
    if !wire::note_text_valid(&redacted) {
        return Err(wire::error::NOTE_INVALID);
    }
    Ok(redacted)
}

// ----- The handler -----

impl JobControl {
    /// `client_viewers` is `jobs.clientViewers`, clamped to 1–32.
    pub(crate) fn new(client_viewers: u32) -> Self {
        Self {
            viewers: client_viewers.clamp(1, MAX_CLIENT_VIEWERS) as usize,
            next_key: 1,
            requests: BTreeMap::new(),
            outbox: VecDeque::new(),
            replies: Vec::new(),
            retained: Vec::new(),
            learned: Vec::new(),
            retain_active: None,
            retain_queue: VecDeque::new(),
            recent: VecDeque::new(),
            closed: false,
            starting: None,
            immediate: None,
        }
    }

    /// Pending requests (tests).
    #[cfg(test)]
    pub(crate) fn pending(&self) -> usize {
        self.requests.len()
    }

    fn viewers_in_use(&self) -> usize {
        self.requests.values().filter(|r| r.viewer).count()
    }

    /// One validated job request (the codec already applied presence,
    /// bounds and direction). `Some` is the reply now; `None` means the
    /// request is pending and its reply arrives through
    /// [`Self::drain_replies`].
    pub(crate) fn request(&mut self, request: pb::Envelope, ctx: &mut Ctx) -> Option<pb::Envelope> {
        let Some(body) = request.body.as_ref().filter(|b| handles(b)) else {
            return Some(unavailable_reply(request));
        };
        if self.closed {
            return Some(draining_reply(request));
        }
        let viewer = match body {
            Body::JobRead(_) | Body::JobObserve(_) => true,
            Body::JobList(v) => v.after_revision.is_some(),
            _ => false,
        };
        // The viewer bound (J4-E5): one more pending read, observe or
        // parked list is `busy`.
        if viewer && self.viewers_in_use() >= self.viewers {
            let body = busy_body(body);
            return Some(reply(request, body));
        }
        // A repeated retain request ID returns the earlier outcome.
        if let Body::JobRetain(v) = body
            && let Some(recent) = self.recent.iter().find(|r| {
                Some(&r.request) == v.request_id.as_ref()
                    && Some(&r.job[..]) == v.job_id.as_deref()
                    && Some(r.action as i32) == v.action
            })
        {
            let body = Body::JobRetainReply(recent.reply.clone());
            return Some(reply(request, body));
        }
        let work = match body {
            Body::JobList(v) => Work::List(ListWork::new(v)),
            Body::JobInspect(v) => Work::Inspect(InspectWork {
                job: id(&v.job_id),
                fence: optional_id(&v.project_id),
                facts: None,
                warnings: None,
                path_read: false,
            }),
            Body::JobRead(v) => Work::View(ViewWork::read(v)),
            Body::JobObserve(v) => Work::View(ViewWork::observe(v)),
            Body::JobCancel(v) => Work::Cancel(CancelWork {
                job: id(&v.job_id),
                expected: id(&v.expected_project_id),
                kill: v.kill == Some(true),
                phase: CancelPhase::Start,
                facts: None,
                reply: None,
                rereads: 0,
            }),
            Body::JobRetain(v) => Work::Retain(RetainWork::new(v)),
            _ => return Some(unavailable_reply(request)),
        };
        let key = self.next_key;
        self.next_key += 1;
        let attachment = request
            .attachment_id
            .as_deref()
            .and_then(|a| a.try_into().ok());
        let retain = matches!(work, Work::Retain(_));
        self.requests.insert(
            key,
            Request {
                envelope: request,
                attachment,
                viewer,
                detached: false,
                wait: Wait::None,
                registry_read: None,
                known: Vec::new(),
                hold_retried: false,
                work,
            },
        );
        self.starting = Some(key);
        if retain && self.retain_active.is_some() {
            self.apply(key, Next::Turn, ctx);
        } else {
            if retain {
                self.retain_active = Some(key);
            }
            self.drive(key, Input::Start, ctx);
        }
        self.starting = None;
        // A request that finished at once replies now.
        self.immediate.take()
    }

    /// The next journal work, oldest first, when the owner's ticket is
    /// free. The owner reports its reply with [`Self::journal_done`]; when
    /// it cannot submit (journal unavailable), it reports the error at once.
    pub(crate) fn next_journal(&mut self) -> Option<(u64, JournalWork)> {
        while let Some(key) = self.outbox.pop_front() {
            let Some(request) = self.requests.get_mut(&key) else {
                continue;
            };
            if let Wait::Journal(work) = std::mem::replace(&mut request.wait, Wait::InFlight) {
                return Some((key, work));
            }
        }
        None
    }

    /// Journal work waits for the owner's ticket (tests).
    #[cfg(test)]
    pub(crate) fn has_journal_work(&self) -> bool {
        !self.outbox.is_empty()
    }

    /// The reply to journal work from [`Self::next_journal`]. A reply for a
    /// request that no longer exists is ignored.
    pub(crate) fn journal_done(
        &mut self,
        key: u64,
        result: writer::Result<writer::Reply>,
        ctx: &mut Ctx,
    ) {
        let Some(request) = self.requests.get_mut(&key) else {
            return;
        };
        if !matches!(request.wait, Wait::InFlight) {
            return;
        }
        request.wait = Wait::None;
        if let Some(project) = request.registry_read.take() {
            // A registry answer: registered when the journal finds it.
            match result {
                Ok(reply) => {
                    let found = reply
                        .projects
                        .into_iter()
                        .find(|p| p.registration.project == project);
                    if let Some(listing) = &found {
                        self.learned.push(listing.registration.clone());
                    }
                    request.known.push((project, found.is_some()));
                    self.drive(key, Input::Resume, ctx);
                }
                Err(error) => {
                    let body = read_error(request_body(request), error);
                    self.finish(key, Some(body), ctx);
                }
            }
            return;
        }
        self.drive(key, Input::Journal(Box::new(result)), ctx);
    }

    /// A client wait ended (`WaitKey::Client` from
    /// `JobController::drain_waits`). `Closed` answers `Draining`.
    pub(crate) fn wait_done(&mut self, done: WaitDone, ctx: &mut Ctx) {
        let WaitKey::Client(key) = done.key else {
            return;
        };
        let Some(request) = self.requests.get_mut(&key) else {
            return;
        };
        if !matches!(request.wait, Wait::Controller) {
            return;
        }
        request.wait = Wait::None;
        if done.end == WaitEnding::Closed {
            self.finish(key, Some(draining()), ctx);
            return;
        }
        self.drive(key, Input::Wait, ctx);
    }

    /// A note retain waits for the secret rule set: the owner retries a
    /// failed warm-thread spawn while this is true.
    pub(crate) fn waiting_for_rules(&self) -> bool {
        self.requests
            .values()
            .any(|r| matches!(r.wait, Wait::Rules))
    }

    /// Each reactor poll: collects page replies, resumes kind 30 appends
    /// whose journal hold ended, and resumes a note waiting for the rule
    /// set once `ctx.rules` is `Some`. In-memory work only.
    pub(crate) fn poll(&mut self, ctx: &mut Ctx) {
        let mut ready = Vec::new();
        let mut rules_ready = Vec::new();
        for (key, request) in self.requests.iter_mut() {
            match &mut request.wait {
                Wait::Rules if ctx.rules.is_some() => {
                    request.wait = Wait::None;
                    rules_ready.push(*key);
                }
                Wait::Pages(pages) => {
                    for page in pages.iter_mut() {
                        if page.reply.is_none() {
                            page.reply = page.wait.poll();
                        }
                    }
                    if pages.iter().all(|p| p.reply.is_some())
                        && let Wait::Pages(pages) = std::mem::replace(&mut request.wait, Wait::None)
                    {
                        let replies: Vec<_> = pages
                            .into_iter()
                            .filter_map(|p| p.reply.map(|r| (p.stream, p.offset, r)))
                            .collect();
                        ready.push((*key, replies));
                    }
                }
                // The hold ended: append again. A refusal the owner never
                // reported as a hold is retried once, then waits for the
                // owner to report the hold and its end (no busy loop).
                Wait::Held { seen, .. } if ctx.journal_held => *seen = true,
                Wait::Held { seen, .. } if *seen || !request.hold_retried => {
                    request.hold_retried |= !*seen;
                    if let Wait::Held { record, .. } =
                        std::mem::replace(&mut request.wait, Wait::None)
                    {
                        request.wait = Wait::Journal(JournalWork::Append(record));
                        self.outbox.push_back(*key);
                    }
                }
                _ => {}
            }
        }
        for (key, replies) in ready {
            self.drive(key, Input::Pages(replies), ctx);
        }
        for key in rules_ready {
            self.drive(key, Input::Resume, ctx);
        }
    }

    /// Drops the requests whose attachment closed (the `retain_observers`
    /// rule): a parked wait is removed from the controller, page reads are
    /// withdrawn, queued journal work is dropped. Work the owner already
    /// submitted finishes with its reply discarded; a kind 30 that commits
    /// still applies its effects. Closing never cancels a job.
    pub(crate) fn retain(&mut self, attachments: &[[u8; 16]], ctx: &mut Ctx) {
        let gone: Vec<u64> = self
            .requests
            .iter()
            .filter(|(_, r)| !r.detached && r.attachment.is_some_and(|a| !attachments.contains(&a)))
            .map(|(key, _)| *key)
            .collect();
        for key in gone {
            self.detach(key, ctx);
        }
    }

    fn detach(&mut self, key: u64, ctx: &mut Ctx) {
        let Some(request) = self.requests.get_mut(&key) else {
            return;
        };
        request.detached = true;
        match request.wait {
            Wait::InFlight => {}
            Wait::Controller => {
                ctx.jobs.cancel_wait(WaitKey::Client(key));
                self.finish(key, None, ctx);
            }
            Wait::Turn => {
                self.retain_queue.retain(|k| *k != key);
                self.finish(key, None, ctx);
            }
            _ => {
                self.outbox.retain(|k| *k != key);
                self.finish(key, None, ctx);
            }
        }
    }

    /// Shutdown (AE-06): every pending request answers `Draining`; later
    /// requests too. Work already submitted to the journal finishes with
    /// its reply discarded.
    pub(crate) fn close(&mut self, ctx: &mut Ctx) {
        self.closed = true;
        let keys: Vec<u64> = self.requests.keys().copied().collect();
        for key in keys {
            let Some(request) = self.requests.get_mut(&key) else {
                continue;
            };
            if request.detached {
                continue;
            }
            let reply = draining_reply(request.envelope.clone());
            self.replies.push(reply);
            self.detach(key, ctx);
        }
    }

    /// Replies ready to send, oldest first.
    pub(crate) fn drain_replies(&mut self, out: &mut Vec<pb::Envelope>) {
        out.append(&mut self.replies);
    }
    /// Committed person kind 30 records for the payload owner.
    pub(crate) fn drain_retained(&mut self, out: &mut Vec<Retained>) {
        out.append(&mut self.retained);
    }
    /// Registrations read from the journal for registry checks; the owner
    /// adds them to its recent project set.
    pub(crate) fn drain_learned(&mut self, out: &mut Vec<journal::ProjectRegistered>) {
        out.append(&mut self.learned);
    }

    // ----- Driver -----

    fn drive(&mut self, key: u64, input: Input, ctx: &mut Ctx) {
        let Some(mut request) = self.requests.remove(&key) else {
            return;
        };
        let body = request.envelope.body.take().unwrap_or_else(draining);
        let next = {
            let mut step = Step {
                key,
                ctx,
                known: &request.known,
                body: &body,
                retained: &mut self.retained,
            };
            match &mut request.work {
                Work::List(w) => w.step(&mut step, input),
                Work::Inspect(w) => w.step(&mut step, input),
                Work::View(w) => w.step(&mut step, input),
                Work::Cancel(w) => w.step(&mut step, input),
                Work::Retain(w) => w.step(&mut step, input),
            }
        };
        request.envelope.body = Some(body);
        self.requests.insert(key, request);
        self.apply(key, next, ctx);
    }

    fn apply(&mut self, key: u64, next: Next, ctx: &mut Ctx) {
        let Some(request) = self.requests.get_mut(&key) else {
            return;
        };
        if let Next::Reply(body) = next {
            self.finish(key, Some(*body), ctx);
            return;
        }
        // A detached request starts no further work.
        if request.detached {
            self.finish(key, None, ctx);
            return;
        }
        match next {
            Next::Reply(_) => {}
            Next::Read(command) => {
                if let writer::Command::Project { project } = command {
                    request.registry_read = Some(project);
                }
                request.wait = Wait::Journal(JournalWork::Read(command));
                self.outbox.push_back(key);
            }
            Next::Append(record) => {
                request.wait = Wait::Journal(JournalWork::Append(record));
                self.outbox.push_back(key);
            }
            Next::Hold(record) => {
                request.wait = Wait::Held {
                    record,
                    seen: ctx.journal_held,
                }
            }
            Next::Park => request.wait = Wait::Controller,
            Next::Turn => {
                request.wait = Wait::Turn;
                self.retain_queue.push_back(key);
            }
            // The retain keeps `retain_active`: the others stay queued.
            Next::Rules => request.wait = Wait::Rules,
            Next::Pages(reads) => {
                let busy = error_body(request_body(request), wire::error::BUSY);
                let Some(pages) = ctx.pages else {
                    let body = error_body(request_body(request), wire::error::UNAVAILABLE);
                    self.finish(key, Some(body), ctx);
                    return;
                };
                let mut pending = Vec::with_capacity(reads.len());
                for (stream, read) in reads {
                    match pages.submit(read) {
                        Ok(wait) => pending.push(PendingPage {
                            stream,
                            offset: read.offset,
                            wait,
                            reply: None,
                        }),
                        // The page pool's queue of 8 is full (J4-E5);
                        // dropping the other read withdraws it.
                        Err(_) => {
                            self.finish(key, Some(busy), ctx);
                            return;
                        }
                    }
                }
                request.wait = Wait::Pages(pending);
            }
        }
    }

    fn finish(&mut self, key: u64, body: Option<Body>, ctx: &mut Ctx) {
        let Some(request) = self.requests.remove(&key) else {
            return;
        };
        self.outbox.retain(|k| *k != key);
        // A detached request's reply is discarded; a kind 30 it committed
        // already queued its effects.
        if let (Some(body), false) = (body, request.detached) {
            if let (Body::JobRetain(v), Body::JobRetainReply(reply)) =
                (request_body(&request), &body)
            {
                self.remember(v, reply.clone());
            }
            let envelope = reply(request.envelope, body);
            if self.starting == Some(key) {
                self.immediate = Some(envelope);
            } else {
                self.replies.push(envelope);
            }
        }
        if self.retain_active == Some(key) {
            self.retain_active = None;
            while let Some(next) = self.retain_queue.pop_front() {
                if self.requests.contains_key(&next) {
                    self.retain_active = Some(next);
                    if let Some(r) = self.requests.get_mut(&next) {
                        r.wait = Wait::None;
                    }
                    self.drive(next, Input::Start, ctx);
                    break;
                }
            }
        }
    }

    fn remember(&mut self, v: &pb::JobRetain, reply: pb::JobRetainReply) {
        let (Some(request), Some(action)) = (
            v.request_id.clone(),
            v.action
                .and_then(|a| u8::try_from(a).ok())
                .and_then(RetentionAction::from_code),
        ) else {
            return;
        };
        if self.recent.len() == RECENT_RETAINS {
            self.recent.pop_front();
        }
        self.recent.push_back(Recent {
            request,
            job: id(&v.job_id),
            action,
            reply,
        });
    }
}

fn request_body(request: &Request) -> &Body {
    request.envelope.body.as_ref().expect("job request body")
}

fn reply(mut request: pb::Envelope, body: Body) -> pb::Envelope {
    request.body = Some(body);
    request
}

#[cfg(test)]
mod tests;
