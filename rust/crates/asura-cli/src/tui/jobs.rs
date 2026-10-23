//! TUI job worker (tool-job-controller.md, J4 TUI): three attachment slots,
//! each on its own thread and attachment, because an attachment has one
//! pending request. The service owns every job decision; this module only
//! sends typed requests and returns their replies. Rendering and key handling
//! (`ui/job_view.rs`) perform no I/O.
//!
//! | Slot | Request |
//! | --- | --- |
//! | Status | Parked `JobList` scope project, `live_only`, `after_revision` |
//! | Observe | `JobObserve` for the open viewer in follow mode |
//! | Request | One `JobList` page, `JobInspect`, `JobRead`, `JobCancel` or `JobRetain` |
//!
//! Every wait has no time bound (owner, 2026-10-16): it ends on the reply,
//! the person's `/cancel` or Escape, a scope change, exit or a lost
//! connection. The only interval is the status slot's 1 s retry pacing after
//! a failed attachment, which ends nothing.
use super::{events::Notice, observation::Resolver, queue_watch::Scope};
use asura_control::{jobs as limits, pb};
use std::{
    io,
    net::Shutdown,
    os::unix::net::UnixStream,
    sync::{
        Arc, Mutex, TryLockError,
        atomic::{AtomicBool, Ordering},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};

/// List entries the TUI keeps for prefix resolution (J4 numeric limits).
pub(super) const MAX_CACHED_ENTRIES: usize = 64;
/// Pages kept per stream of the open viewer (J4 numeric limits).
pub(super) const MAX_PAGES_PER_STREAM: usize = 4;
/// Shortest job ID prefix a command accepts (J4-E7).
pub(super) const MIN_PREFIX_DIGITS: usize = 6;
/// Jobs a response's chooser offers (J4 TUI, activity row).
pub(super) const MAX_CHOOSER: usize = 9;
/// Entries per list page; the service's bound.
pub(super) const LIST_PAGE: u32 = limits::MAX_JOB_LIST_ENTRIES;
/// Pacing before a failed status attachment is tried again. It ends no wait.
const STATUS_RETRY: Duration = Duration::from_secs(1);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Stream {
    Stdout,
    Stderr,
}
impl Stream {
    pub fn code(self) -> i32 {
        match self {
            Self::Stdout => pb::JobStream::Stdout as i32,
            Self::Stderr => pb::JobStream::Stderr as i32,
        }
    }
    pub fn index(self) -> usize {
        match self {
            Self::Stdout => 0,
            Self::Stderr => 1,
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Self::Stdout => "stdout",
            Self::Stderr => "stderr",
        }
    }
    pub fn other(self) -> Self {
        match self {
            Self::Stdout => Self::Stderr,
            Self::Stderr => Self::Stdout,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Retain {
    Pin,
    Unpin,
    Delete,
    Note(String),
}
impl Retain {
    fn action(&self) -> pb::JobRetainAction {
        match self {
            Self::Pin => pb::JobRetainAction::Pin,
            Self::Unpin => pb::JobRetainAction::Unpin,
            Self::Delete => pb::JobRetainAction::Delete,
            Self::Note(_) => pb::JobRetainAction::Note,
        }
    }
}

/// One request-slot command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Command {
    /// One page: the selected project's jobs, or every project's with `all`.
    List {
        all: bool,
        project: Option<[u8; 16]>,
        before: Option<u64>,
    },
    Inspect {
        job: [u8; 16],
        project: Option<[u8; 16]>,
    },
    /// A page at `offset`, or the last page up to the watermark when `None`.
    Read {
        job: [u8; 16],
        project: Option<[u8; 16]>,
        stream: Stream,
        offset: Option<u64>,
    },
    Cancel {
        request_id: [u8; 16],
        job: [u8; 16],
        project: [u8; 16],
        kill: bool,
    },
    Retain {
        request_id: [u8; 16],
        job: [u8; 16],
        project: [u8; 16],
        action: Retain,
    },
}
impl Command {
    /// A mutation may commit after its wait ends: a stopped wait is
    /// `outcome_unconfirmed`, never a refusal.
    pub fn mutation(&self) -> bool {
        matches!(self, Self::Cancel { .. } | Self::Retain { .. })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Request {
    /// The service epoch the request was made for.
    pub epoch: String,
    pub command: Command,
    /// The composer draft that issued the request, kept until success.
    pub draft: Option<String>,
}

#[derive(Debug)]
pub(super) enum Reply {
    List(pb::JobListReply),
    Detail(pb::JobDetail),
    Page(pb::JobPage),
    Cancel(pb::JobCancelReply),
    Retain(pb::JobRetainReply),
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Failure {
    /// The service did not list `CAPABILITY_JOBS`.
    Unsupported,
    /// A read wait the person stopped; nothing changed.
    Cancelled,
    /// A mutation wait that ended without a reply; it may still commit.
    Unconfirmed,
    /// Lost connection, another service, `Draining` or a protocol fault.
    Unavailable(String),
}
#[derive(Debug)]
pub(super) struct Finished {
    pub request: Request,
    pub result: Result<Reply, Failure>,
}

/// The status slot's scope and its latest reply.
#[derive(Debug)]
pub(super) struct StatusUpdate {
    pub scope: Scope,
    pub result: Result<pb::JobListReply, Failure>,
}

/// One follow session of the open viewer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Follow {
    pub session: u64,
    pub epoch: String,
    pub job: [u8; 16],
    pub project: Option<[u8; 16]>,
    pub stream: Stream,
    /// Bytes of the stream the viewer holds.
    pub offset: u64,
    /// The state the viewer last saw.
    pub after_state: Option<i32>,
}
#[derive(Debug)]
pub(super) struct FollowUpdate {
    pub session: u64,
    pub result: Result<pb::JobPage, Failure>,
}

#[derive(Default)]
struct Cancellation {
    stopped: AtomicBool,
    socket: Mutex<Option<UnixStream>>,
}
impl Cancellation {
    fn stopped(&self) -> bool {
        self.stopped.load(Ordering::Acquire)
    }
    fn cancel(&self) {
        self.stopped.store(true, Ordering::Release);
        // Never wait for the worker in the terminal event loop.
        if let Ok(socket) = self.socket.try_lock()
            && let Some(socket) = socket.as_ref()
        {
            let _ = socket.shutdown(Shutdown::Both);
        }
    }
}

fn failure(error: asura_client::Error) -> Failure {
    match error {
        asura_client::Error::Cancelled => Failure::Cancelled,
        asura_client::Error::OutcomeUnconfirmed => Failure::Unconfirmed,
        error => Failure::Unavailable(error.to_string()),
    }
}

/// Attaches for one slot: the epoch must match and the service must list
/// `CAPABILITY_JOBS` (EN1).
fn attach(
    resolve: Resolver,
    epoch: &str,
    token: &Arc<Cancellation>,
) -> Result<asura_client::Client, Failure> {
    if token.stopped() {
        return Err(Failure::Cancelled);
    }
    let runtime = resolve(false)
        .map_err(|error| Failure::Unavailable(asura_client::Error::from(error).to_string()))?;
    let cancelled = || token.stopped();
    let mut client = asura_client::Client::attach(
        &runtime,
        concat!("asura/", env!("CARGO_PKG_VERSION")),
        &cancelled,
    )
    .map_err(|error| {
        if token.stopped() {
            Failure::Cancelled
        } else {
            failure(error)
        }
    })?;
    *token
        .socket
        .lock()
        .map_err(|_| Failure::Unavailable("service_unavailable".into()))? =
        Some(client.disconnect_handle().map_err(failure)?);
    if token.stopped() {
        token.cancel();
        return Err(Failure::Cancelled);
    }
    let stop = token.clone();
    client.set_cancel(Arc::new(move || stop.stopped()));
    if crate::conversation::hex(&client.service_epoch()) != epoch {
        return Err(Failure::Unavailable("service changed".into()));
    }
    if !client.supports(pb::Capability::Jobs) {
        return Err(Failure::Unsupported);
    }
    Ok(client)
}

fn list_request(all: bool, project: Option<[u8; 16]>, before: Option<u64>) -> pb::JobList {
    pb::JobList {
        scope: Some(if all {
            pb::JobScope::AllProjects
        } else {
            pb::JobScope::Project
        } as i32),
        project_id: (!all).then(|| project.map(|p| p.to_vec())).flatten(),
        conversation_id: None,
        before,
        limit: Some(LIST_PAGE),
        live_only: None,
        after_revision: None,
    }
}

fn read_request(
    job: [u8; 16],
    project: Option<[u8; 16]>,
    stream: Stream,
    offset: Option<u64>,
) -> pb::JobRead {
    pb::JobRead {
        job_id: Some(job.to_vec()),
        project_id: project.map(|p| p.to_vec()),
        stream: Some(stream.code()),
        offset,
        limit: Some(limits::MAX_JOB_PAGE_BYTES),
        at_end: offset.is_none().then_some(true),
    }
}

/// Sends one request-slot command on an attached client.
fn send(
    client: &mut asura_client::Client,
    command: &Command,
    cancelled: &dyn Fn() -> bool,
) -> Result<Reply, Failure> {
    Ok(match command {
        Command::List {
            all,
            project,
            before,
        } => Reply::List(
            client
                .jobs(list_request(*all, *project, *before), cancelled)
                .map_err(failure)?,
        ),
        Command::Inspect { job, project } => Reply::Detail(
            client
                .inspect_job(*job, *project, cancelled)
                .map_err(failure)?,
        ),
        Command::Read {
            job,
            project,
            stream,
            offset,
        } => Reply::Page(
            client
                .read_job(read_request(*job, *project, *stream, *offset), cancelled)
                .map_err(failure)?,
        ),
        Command::Cancel {
            request_id,
            job,
            project,
            kill,
        } => Reply::Cancel(
            client
                .cancel_job(
                    pb::JobCancel {
                        request_id: Some(request_id.to_vec()),
                        job_id: Some(job.to_vec()),
                        expected_project_id: Some(project.to_vec()),
                        kill: kill.then_some(true),
                    },
                    cancelled,
                )
                .map_err(failure)?,
        ),
        Command::Retain {
            request_id,
            job,
            project,
            action,
        } => Reply::Retain(
            client
                .retain_job(
                    pb::JobRetain {
                        request_id: Some(request_id.to_vec()),
                        job_id: Some(job.to_vec()),
                        expected_project_id: Some(project.to_vec()),
                        action: Some(action.action() as i32),
                        note: match action {
                            Retain::Note(text) => Some(text.clone()),
                            _ => None,
                        },
                        confirmed: (*action == Retain::Delete).then_some(true),
                    },
                    cancelled,
                )
                .map_err(failure)?,
        ),
    })
}

struct RequestJob {
    handle: JoinHandle<()>,
    cancel: Arc<Cancellation>,
    request: Request,
    reported: bool,
    /// The reply, stored before the thread's settlement notice. The slot
    /// threads share one router source, so a notice can be consumed before
    /// the thread has exited: the reply is read from here, not from join.
    reply: Arc<Mutex<Option<Finished>>>,
}
struct StatusJob {
    scope: Scope,
    cancel: Arc<Cancellation>,
    latest: Arc<Mutex<Option<StatusUpdate>>>,
    handle: JoinHandle<()>,
}
struct FollowJob {
    follow: Follow,
    cancel: Arc<Cancellation>,
    latest: Arc<Mutex<Option<FollowUpdate>>>,
    handle: JoinHandle<()>,
}

fn take<T>(mailbox: &Mutex<Option<T>>, notice: &Notice) -> Option<T> {
    match mailbox.try_lock() {
        Ok(mut value) => value.take(),
        Err(TryLockError::Poisoned(error)) => error.into_inner().take(),
        Err(TryLockError::WouldBlock) => {
            // The thread is publishing; look again on the next cycle.
            notice.ready();
            None
        }
    }
}
fn pending<T>(mailbox: &Mutex<Option<T>>) -> bool {
    match mailbox.try_lock() {
        Ok(value) => value.is_some(),
        Err(TryLockError::Poisoned(error)) => error.into_inner().is_some(),
        Err(TryLockError::WouldBlock) => true,
    }
}

#[derive(Default)]
pub(super) struct Worker {
    notice: Notice,
    request: Option<RequestJob>,
    status: Option<StatusJob>,
    follow: Option<FollowJob>,
    status_retry: Option<Instant>,
    /// The epoch whose service did not list `CAPABILITY_JOBS`; the status
    /// slot does not attach again for it.
    unsupported: Option<String>,
    /// The last follow session whose thread ended.
    follow_done: Option<u64>,
}
impl Worker {
    pub fn set_notice(&mut self, notice: Notice) {
        self.notice = notice;
    }
    /// Whether any slot thread still runs or holds an unread reply.
    pub fn running(&self) -> bool {
        self.request.is_some() || self.status.is_some() || self.follow.is_some()
    }
    /// The request slot stays occupied until its thread settles.
    pub fn request_busy(&self) -> bool {
        self.request.is_some()
    }
    pub fn next_deadline(&self) -> Option<Instant> {
        self.status_retry
    }

    pub fn submit(&mut self, resolve: Resolver, request: Request) -> Result<(), &'static str> {
        if self.request.is_some() {
            return Err("Job request pending; draft retained");
        }
        let cancel = Arc::new(Cancellation::default());
        let token = cancel.clone();
        let notice = self.notice.clone();
        let sent = request.clone();
        let reply: Arc<Mutex<Option<Finished>>> = Arc::default();
        let slot = reply.clone();
        let handle = std::thread::Builder::new()
            .name("asura-job-request".into())
            .spawn(move || {
                let _settlement = notice.guard();
                let result = (|| {
                    let mut client = attach(resolve, &sent.epoch, &token)?;
                    let cancelled = || token.stopped();
                    send(&mut client, &sent.command, &cancelled)
                })();
                if let Ok(mut socket) = token.socket.lock() {
                    socket.take();
                }
                // Stored before `_settlement` drops and notifies.
                if let Ok(mut slot) = slot.lock() {
                    *slot = Some(Finished {
                        request: sent,
                        result,
                    });
                }
            })
            .map_err(|_| "Cannot start job request; draft retained")?;
        self.request = Some(RequestJob {
            handle,
            cancel,
            request,
            reported: false,
            reply,
        });
        Ok(())
    }
    /// The finished request, or at once after the person's cancel: a read
    /// reports `Cancelled`, a mutation `Unconfirmed`. The slot stays busy
    /// until the thread settles, and a late reply is discarded.
    pub fn poll_request(&mut self) -> Option<Finished> {
        let job = self.request.as_mut()?;
        // The reply is read from its slot, stored before the thread's
        // settlement notice: the slot threads share one router source, so
        // a notice can be consumed before the thread has exited.
        let stored = job.reply.lock().ok().and_then(|mut slot| slot.take());
        let exited = job.handle.is_finished();
        let reported = job.reported;
        job.reported |= stored.is_some();
        if exited {
            let job = self.request.take().unwrap();
            let request = job.request.clone();
            let panicked = job.handle.join().is_err();
            return match stored {
                Some(finished) if !reported => Some(finished),
                None if !reported && panicked => Some(Finished {
                    request,
                    result: Err(Failure::Unavailable("job request failed".into())),
                }),
                _ => None,
            };
        }
        if stored.is_some() || reported {
            // The slot clears once the thread has exited: look again.
            self.notice.ready();
        }
        if let Some(finished) = stored {
            return (!reported).then_some(finished);
        }
        if !reported && job.cancel.stopped() {
            job.reported = true;
            return Some(Finished {
                request: job.request.clone(),
                result: Err(if job.request.command.mutation() {
                    Failure::Unconfirmed
                } else {
                    Failure::Cancelled
                }),
            });
        }
        None
    }
    pub fn cancel_request(&self) {
        if let Some(job) = &self.request {
            job.cancel.cancel();
        }
    }

    /// Keeps one parked status `JobList` for `scope`. A scope change stops
    /// the old one; a failed one starts again after the retry pacing; an
    /// epoch without the jobs capability is not attached again.
    pub fn synchronize_status(
        &mut self,
        resolve: Resolver,
        scope: Option<Scope>,
        now: Instant,
    ) -> io::Result<()> {
        if let Some(job) = &self.status {
            if scope.as_ref() != Some(&job.scope) {
                job.cancel.cancel();
            }
            if !job.handle.is_finished() {
                return Ok(());
            }
            if pending(&job.latest) {
                self.notice.ready();
                return Ok(());
            }
            let job = self.status.take().unwrap();
            let same = scope.as_ref() == Some(&job.scope);
            let _ = job.handle.join();
            self.notice.settled();
            self.status_retry = same.then_some(now + STATUS_RETRY);
        }
        let Some(scope) = scope else {
            self.status_retry = None;
            return Ok(());
        };
        if self.unsupported.as_deref() == Some(scope.epoch.as_str()) {
            self.status_retry = None;
            return Ok(());
        }
        if self.status_retry.is_some_and(|at| now < at) {
            return Ok(());
        }
        let cancel = Arc::new(Cancellation::default());
        let token = cancel.clone();
        let latest = Arc::new(Mutex::new(None));
        let mailbox = latest.clone();
        let notice = self.notice.clone();
        let worker_scope = scope.clone();
        let handle = std::thread::Builder::new()
            .name("asura-job-status".into())
            .spawn(move || {
                let _settlement = notice.guard();
                let publish = |result| {
                    if !token.stopped() {
                        *mailbox.lock().unwrap_or_else(|e| e.into_inner()) = Some(StatusUpdate {
                            scope: worker_scope.clone(),
                            result,
                        });
                        notice.ready();
                    }
                };
                let run = || -> Result<(), Failure> {
                    let mut client = attach(resolve, &worker_scope.epoch, &token)?;
                    let cancelled = || token.stopped();
                    let mut revision = None;
                    while !token.stopped() {
                        let reply = client
                            .jobs(
                                pb::JobList {
                                    scope: Some(pb::JobScope::Project as i32),
                                    project_id: Some(worker_scope.project.to_vec()),
                                    limit: Some(1),
                                    live_only: Some(true),
                                    after_revision: revision,
                                    ..Default::default()
                                },
                                &cancelled,
                            )
                            .map_err(failure)?;
                        if let Some(error) = reply.error {
                            return Err(Failure::Unavailable(error));
                        }
                        // A reply without a revision cannot park the next
                        // request; stop rather than loop at once.
                        let Some(next) = reply.revision else {
                            return Err(Failure::Unavailable("protocol".into()));
                        };
                        revision = Some(next);
                        publish(Ok(reply));
                    }
                    Ok(())
                };
                match run() {
                    Ok(()) | Err(Failure::Cancelled) => {}
                    Err(_) if token.stopped() => {}
                    Err(error) => publish(Err(error)),
                }
                if let Ok(mut socket) = token.socket.lock() {
                    socket.take();
                }
            })?;
        self.status = Some(StatusJob {
            scope,
            cancel,
            latest,
            handle,
        });
        self.status_retry = None;
        Ok(())
    }
    pub fn poll_status(&mut self) -> Option<StatusUpdate> {
        let job = self.status.as_ref()?;
        let value = take(&job.latest, &self.notice)?;
        if job.cancel.stopped() {
            return None;
        }
        if matches!(value.result, Err(Failure::Unsupported)) {
            self.unsupported = Some(value.scope.epoch.clone());
        }
        Some(value)
    }

    /// Keeps one `JobObserve` in flight for `follow`, on its own attachment.
    /// Another session, or none, cancels the wait; the attachment retires
    /// and the service drops the parked request. Closing never cancels a job.
    pub fn synchronize_follow(
        &mut self,
        resolve: Resolver,
        follow: Option<Follow>,
    ) -> io::Result<()> {
        if let Some(job) = &self.follow {
            if follow.as_ref().map(|f| f.session) != Some(job.follow.session) {
                job.cancel.cancel();
            }
            if !job.handle.is_finished() {
                return Ok(());
            }
            if pending(&job.latest) {
                self.notice.ready();
                return Ok(());
            }
            let job = self.follow.take().unwrap();
            let _ = job.handle.join();
            self.notice.settled();
            self.follow_done = Some(job.follow.session);
        }
        let Some(follow) = follow else {
            return Ok(());
        };
        // A session whose observe ended (settled, refused or lost) is not
        // started again; the viewer starts a new session with f or r.
        if self.follow_done == Some(follow.session) {
            return Ok(());
        }
        let cancel = Arc::new(Cancellation::default());
        let token = cancel.clone();
        let latest = Arc::new(Mutex::new(None));
        let mailbox = latest.clone();
        let notice = self.notice.clone();
        let target = follow.clone();
        let handle = std::thread::Builder::new()
            .name("asura-job-observe".into())
            .spawn(move || {
                let _settlement = notice.guard();
                let publish = |result| {
                    if !token.stopped() {
                        *mailbox.lock().unwrap_or_else(|e| e.into_inner()) = Some(FollowUpdate {
                            session: target.session,
                            result,
                        });
                        notice.ready();
                    }
                };
                let run = || -> Result<(), Failure> {
                    let mut client = attach(resolve, &target.epoch, &token)?;
                    let cancelled = || token.stopped();
                    let mut offset = target.offset;
                    let mut state = target.after_state;
                    while !token.stopped() {
                        let mut page = client
                            .observe_job(
                                pb::JobObserve {
                                    job_id: Some(target.job.to_vec()),
                                    project_id: target.project.map(|p| p.to_vec()),
                                    stream: Some(target.stream.code()),
                                    offset: Some(offset),
                                    at_end: None,
                                    after_state: state,
                                },
                                &cancelled,
                            )
                            .map_err(failure)?;
                        if let Some(error) = page.error {
                            return Err(Failure::Unavailable(error));
                        }
                        // Behind the end by more than this page: follow keeps
                        // the view at the end, so read the last page instead.
                        if behind(&page) {
                            page = client
                                .read_job(
                                    read_request(target.job, target.project, target.stream, None),
                                    &cancelled,
                                )
                                .map_err(failure)?;
                            if let Some(error) = page.error {
                                return Err(Failure::Unavailable(error));
                            }
                        }
                        // The next observe parks on bytes past this page's
                        // end or a state other than this page's state.
                        let next = page.pages.first().and_then(|p| p.next_offset);
                        let settled = page.entry.as_ref().and_then(|e| e.state)
                            == Some(pb::JobState::Settled as i32);
                        state = page.entry.as_ref().and_then(|e| e.state).or(state);
                        offset = next.unwrap_or(offset).max(offset);
                        publish(Ok(page));
                        if settled {
                            return Ok(());
                        }
                    }
                    Ok(())
                };
                match run() {
                    Ok(()) | Err(Failure::Cancelled) => {}
                    Err(_) if token.stopped() => {}
                    Err(error) => publish(Err(error)),
                }
                if let Ok(mut socket) = token.socket.lock() {
                    socket.take();
                }
            })?;
        self.follow = Some(FollowJob {
            follow,
            cancel,
            latest,
            handle,
        });
        Ok(())
    }
    pub fn poll_follow(&mut self) -> Option<FollowUpdate> {
        let job = self.follow.as_ref()?;
        let value = take(&job.latest, &self.notice)?;
        (!job.cancel.stopped()).then_some(value)
    }

    pub fn cancel(&self) {
        self.cancel_request();
        for cancel in [
            self.status.as_ref().map(|j| &j.cancel),
            self.follow.as_ref().map(|j| &j.cancel),
        ]
        .into_iter()
        .flatten()
        {
            cancel.cancel();
        }
    }
}
impl Drop for Worker {
    fn drop(&mut self) {
        self.cancel();
        let finished = |worker: &Self| {
            worker
                .request
                .as_ref()
                .is_none_or(|j| j.handle.is_finished())
                && worker
                    .status
                    .as_ref()
                    .is_none_or(|j| j.handle.is_finished())
                && worker
                    .follow
                    .as_ref()
                    .is_none_or(|j| j.handle.is_finished())
        };
        let end = Instant::now() + Duration::from_millis(100);
        while !finished(self) && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(2));
        }
        if let Some(job) = self.request.take().filter(|j| j.handle.is_finished()) {
            let _ = job.handle.join();
        }
        if let Some(job) = self.status.take().filter(|j| j.handle.is_finished()) {
            let _ = job.handle.join();
        }
        if let Some(job) = self.follow.take().filter(|j| j.handle.is_finished()) {
            let _ = job.handle.join();
        }
    }
}

/// The page ends before the stream's stored end by more than one page.
fn behind(page: &pb::JobPage) -> bool {
    page.pages.first().is_some_and(|p| {
        p.next_offset
            .zip(p.total_bytes)
            .is_some_and(|(next, total)| {
                total.saturating_sub(next) > u64::from(limits::MAX_JOB_PAGE_BYTES)
            })
    })
}

// Presentation helpers: pure functions of service facts (J4T-11).

/// A job handle as the person types it: 32 lowercase hexadecimal digits.
pub(super) fn handle(id: &[u8]) -> String {
    crate::conversation::hex(id)
}
/// The short handle shown in rows and notices.
pub(super) fn short(id: &[u8]) -> String {
    handle(id).chars().take(8).collect()
}
pub(super) fn entry_id(entry: &pb::JobEntry) -> Option<[u8; 16]> {
    entry.job_id.as_deref()?.try_into().ok()
}
pub(super) fn entry_project(entry: &pb::JobEntry) -> Option<[u8; 16]> {
    entry.project_id.as_deref()?.try_into().ok()
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum Resolution {
    Job([u8; 16]),
    /// Not a full handle or a hexadecimal prefix of at least 6 digits.
    Malformed,
    /// The matching handles among the cached entries.
    Ambiguous(Vec<String>),
    NoMatch,
}
/// J4-E7: the full handle, or a unique prefix of at least 6 hexadecimal
/// digits among the cached list entries.
pub(super) fn resolve(text: &str, cache: &[pb::JobEntry]) -> Resolution {
    let text = text.to_ascii_lowercase();
    if text.len() < MIN_PREFIX_DIGITS
        || text.len() > 32
        || !text.bytes().all(|b| b.is_ascii_hexdigit())
    {
        return Resolution::Malformed;
    }
    if text.len() == 32 {
        return match crate::conversation::parse_id(&text) {
            Ok(id) => Resolution::Job(id),
            Err(_) => Resolution::Malformed,
        };
    }
    let mut matches: Vec<[u8; 16]> = Vec::new();
    for id in cache.iter().filter_map(entry_id) {
        if handle(&id).starts_with(&text) && !matches.contains(&id) {
            matches.push(id);
        }
    }
    match matches.as_slice() {
        [] => Resolution::NoMatch,
        [id] => Resolution::Job(*id),
        many => Resolution::Ambiguous(many.iter().map(|id| handle(id)).collect()),
    }
}

/// Remembers listed entries, newest first, at most 64.
pub(super) fn remember(cache: &mut Vec<pb::JobEntry>, entry: &pb::JobEntry) {
    let Some(id) = entry_id(entry) else {
        return;
    };
    cache.retain(|e| entry_id(e) != Some(id));
    cache.insert(0, entry.clone());
    cache.truncate(MAX_CACHED_ENTRIES);
}

/// Age since the intent, or since settlement for a settled job: `12 s`,
/// `4 min`, `3 h` or `2 d`; a time of 0 shows `—`.
pub(super) fn age(entry: &pb::JobEntry, now_unix_ms: u64) -> String {
    let settled = entry.state == Some(pb::JobState::Settled as i32);
    let at = if settled {
        entry.settled_unix_ms
    } else {
        entry.intent_unix_ms
    }
    .unwrap_or(0);
    ago(at, now_unix_ms)
}
/// The time since `at` (Unix milliseconds) in the age units; 0 shows `—`.
pub(super) fn ago(at: u64, now_unix_ms: u64) -> String {
    if at == 0 {
        return "—".into();
    }
    let seconds = now_unix_ms.saturating_sub(at) / 1000;
    if seconds < 60 {
        format!("{seconds} s")
    } else if seconds < 3600 {
        format!("{} min", seconds / 60)
    } else if seconds < 86_400 {
        format!("{} h", seconds / 3600)
    } else {
        format!("{} d", seconds / 86_400)
    }
}

/// Job reason codes (tool-job-controller.md, job reason codes).
pub(super) fn reason_name(code: u32) -> &'static str {
    match code {
        0 => "none",
        1 => "model_cancel",
        2 => "turn_ended",
        3 => "client_cancel",
        4 => "person_stop",
        5 => "person_kill",
        6 => "identity_changed",
        7 => "shutdown",
        8 => "capture_fault",
        9 => "quota",
        10 => "enospc",
        11 => "restart",
        12 => "cwd_unavailable",
        13 => "project_changed",
        14 => "sandbox_unavailable",
        15 => "spawn_failed",
        16 => "capture_unavailable",
        17 => "protocol_fault",
        18 => "setup_failed",
        19 => "execution_off",
        20 => "outside_sandbox_off",
        _ => "unknown",
    }
}

/// The state, or for a settled job its result.
pub(super) fn state_text(entry: &pb::JobEntry) -> String {
    use pb::JobState as S;
    let reason = entry.reason.filter(|r| *r != 0).map(reason_name);
    let brief = entry
        .reason_text
        .as_deref()
        .filter(|t| !t.is_empty())
        .map(|t| literal(t, limits::MAX_JOB_REASON_TEXT_BYTES));
    let with = |name: &str| match &brief {
        Some(text) => format!("{name} ({text})"),
        None => name.to_owned(),
    };
    let state = entry.state.and_then(|s| S::try_from(s).ok());
    let mut text = match state {
        Some(S::Queued) => with("queued"),
        Some(S::Starting) => "starting".into(),
        Some(S::Running) => "running".into(),
        Some(S::CapturePaused) => match (&brief, entry.block_reason) {
            (Some(_), _) => with("paused"),
            (None, Some(2)) => "paused (quota)".into(),
            (None, Some(3)) => "paused (enospc)".into(),
            _ => "paused".into(),
        },
        Some(S::Stopping) => "stopping".into(),
        Some(S::Draining) => "draining".into(),
        Some(S::CleanupUnconfirmed) => "cleanup unconfirmed".into(),
        Some(S::Finalizing) => "finalizing".into(),
        Some(S::Settled) => {
            let exit = match (entry.exit_kind, entry.exit_value) {
                (Some(1), Some(value)) => Some(format!("exit {value}")),
                (Some(2), Some(value)) => Some(format!("signal {value}")),
                _ => None,
            };
            let mut text = match entry.outcome {
                Some(1 | 2) => exit.clone().unwrap_or_else(|| "exited".into()),
                Some(3) => "cancelled".into(),
                Some(4) => "not started".into(),
                Some(5) => "interrupted".into(),
                _ => "settled".into(),
            };
            if entry.outcome == Some(3)
                && let Some(exit) = exit
            {
                text.push_str(&format!(" · {exit}"));
            }
            if let Some(reason) = reason.filter(|_| entry.outcome != Some(5)) {
                text.push_str(&format!(" · {reason}"));
            }
            text
        }
        _ => "unknown".into(),
    };
    if entry.cancel_requested == Some(true) && state != Some(S::Settled) {
        text.push_str(" · cancel requested");
    }
    text
}

/// `!` for a person job, `!!` for one outside the sandbox, none for a
/// model job.
pub(super) fn origin_mark(entry: &pb::JobEntry) -> &'static str {
    match (entry.origin, entry.outside_sandbox) {
        (Some(2), Some(true)) => "!!",
        (Some(2), _) => "!",
        _ => "",
    }
}

pub(super) fn bytes(count: u64) -> String {
    if count >= 1_048_576 {
        format!("{:.1} MiB", count as f64 / 1_048_576.0)
    } else if count >= 1024 {
        format!("{:.1} KiB", count as f64 / 1024.0)
    } else {
        format!("{count} B")
    }
}

/// Page state names (`JobPageState`).
pub(super) fn page_state(code: Option<i32>) -> &'static str {
    match code.and_then(|c| pb::JobPageState::try_from(c).ok()) {
        Some(pb::JobPageState::Live) => "live",
        Some(pb::JobPageState::Complete) => "complete",
        Some(pb::JobPageState::Incomplete) => "incomplete",
        Some(pb::JobPageState::UnverifiedPrefix) => "unverified prefix",
        Some(pb::JobPageState::Expired) => "expired",
        Some(pb::JobPageState::Missing) => "missing",
        Some(pb::JobPageState::Corrupt) => "corrupt",
        Some(pb::JobPageState::Deleted) => "deleted",
        _ => "unknown",
    }
}

/// The EX12 line under a turn whose jobs outlived it (J4-E6).
pub(super) fn outlived_notice(count: u32) -> Option<String> {
    (count > 0).then(|| {
        format!(
            "↳ {count} job{} kept running after this turn · /jobs to view or cancel",
            if count == 1 { "" } else { "s" }
        )
    })
}

/// The status-row item: `jobs N` while jobs of the selected project are
/// live, and `exec off` while its effective execution value is off.
pub(super) fn status_text(live: u32, execution_enabled: Option<bool>) -> String {
    let mut items = Vec::new();
    if live > 0 {
        items.push(format!("jobs {live}"));
    }
    if execution_enabled == Some(false) {
        items.push("exec off".to_owned());
    }
    items.join(" · ")
}

/// The exit-confirmation line for a TUI-launched backend; absent for 0.
pub(super) fn exit_line(live_in_service: u32) -> Option<String> {
    match live_in_service {
        0 => None,
        1 => {
            Some("1 job is still running in all projects; it stops when the service stops.".into())
        }
        n => Some(format!(
            "{n} jobs are still running in all projects; they stop when the service stops."
        )),
    }
}

/// Reply error tokens as the person reads them.
pub(super) fn error_text(token: &str) -> String {
    use limits::error as e;
    match token {
        e::NOT_FOUND => "job not found".into(),
        e::PROJECT_MISMATCH => "the job belongs to another project".into(),
        e::KILL_NOT_PERMITTED => "kill applies only to your own commands".into(),
        e::NOT_SETTLED => "a running job's output cannot be deleted".into(),
        e::NOTES_FULL => "the job has 16 notes; no more can be added".into(),
        e::RETENTION_FULL => "the job has 64 retention changes; no more can be recorded".into(),
        e::NOTE_INVALID => "a note is 1 to 2048 bytes without control characters".into(),
        e::BUSY => "busy: too many job views are waiting; r retries".into(),
        e::UNAVAILABLE => "job records are unavailable".into(),
        e::INVALID_CURSOR => "the list changed; reloading from the newest jobs".into(),
        other => literal(other, 128),
    }
}

fn literal(text: &str, max: usize) -> String {
    text.chars()
        .scan(0, |used, c| {
            *used += c.len_utf8();
            (*used <= max).then_some(if c.is_control() { ' ' } else { c })
        })
        .collect()
}

#[cfg(test)]
mod tests;
