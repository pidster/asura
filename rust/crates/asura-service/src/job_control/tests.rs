//! Job control handler unit tests (tool-job-controller.md, J4 validation:
//! the handler parts of J4T-04 to J4T-07). The real job controller runs
//! with a scripted slot and payload port; the journal, the registry and the
//! page pool are in-memory fixtures. Every reply passes the control codec
//! (J34-W token sets). No test starts a process, a thread or a timer.
use super::*;
use crate::job_controller::{
    Admission, Admitted, AgentKey, CaptureOutcome, Origin, PayloadPort, Profile, Short, SlotEvent,
    SlotFactory, SlotPort, SlotStart, SlotStartError, StartRequest,
};
use crate::{capture_worker, tools};
use asura_control::{Direction, validate_semantics};
use asura_platform::exec_cmd::{Exit, StopKind};
use asura_platform::{PollInterest, RuntimeDirectory};
use asura_storage::authority::conversation::{
    ArgumentForm, CaptureReason, CaptureState, CaptureStream, JobCleanup, JobListing, JobSettled,
    JobStream, Locator, ProjectRegistered, Record, ToolCallSource, ToolDecision, ToolOperation,
};
use asura_storage::config::JobLimits;
use asura_storage::payload::{PayloadStore, Victim};
use std::cell::{Cell, RefCell};
use std::collections::BTreeSet;
use std::rc::Rc;
use std::sync::Arc;

const P1: Id = [0x11; 16];
const P2: Id = [0x22; 16];
const P3: Id = [0x33; 16];
const C1: Id = [0xc1; 16];
const C2: Id = [0xc2; 16];
const C3: Id = [0xc3; 16];
const INSTALLATION: Id = [0x77; 16];
const EPOCH: [u8; 16] = [0x99; 16];
/// A detectable secret, assembled at run time (no token-shaped literal).
fn secret() -> String {
    ["Pat-7731", "-lark"].concat()
}

// ----- Fixtures: slot and payload port -----

#[derive(Default)]
struct SlotLog {
    scripts: BTreeMap<Id, VecDeque<SlotEvent>>,
    idle: BTreeSet<Id>,
    stops: Vec<(Id, StopKind)>,
}
type Slots = Rc<RefCell<SlotLog>>;
struct Slot {
    log: Slots,
    job: Option<Id>,
}
impl SlotPort for Slot {
    fn start(&mut self, start: SlotStart) -> Result<(), SlotStartError> {
        self.job = Some(start.job);
        Ok(())
    }
    fn stop(&mut self, kind: StopKind) {
        if let Some(job) = self.job {
            self.log.borrow_mut().stops.push((job, kind));
        }
    }
    fn discard_capture(&mut self) {}
    fn close_capture(&mut self) {}
    fn interests(&self, _out: &mut Vec<PollInterest>) {}
    fn next_deadline(&self, _now: Instant) -> Option<Instant> {
        None
    }
    fn poll(&mut self, _now: Instant, out: &mut Vec<SlotEvent>) {
        let Some(job) = self.job else {
            return;
        };
        if let Some(script) = self.log.borrow_mut().scripts.get_mut(&job) {
            out.extend(script.drain(..));
        }
    }
    fn idle(&self) -> bool {
        self.job
            .is_some_and(|job| self.log.borrow().idle.contains(&job))
    }
}
fn factory(log: Slots) -> SlotFactory {
    Box::new(move || {
        Box::new(Slot {
            log: log.clone(),
            job: None,
        })
    })
}

struct Payloads {
    store: Arc<PayloadStore>,
    stored: BTreeMap<Id, [u64; 2]>,
}
impl PayloadPort for Payloads {
    fn ready(&self) -> bool {
        true
    }
    fn pending_installs(&self) -> usize {
        0
    }
    fn space_epoch(&self) -> u64 {
        0
    }
    fn admit(&mut self, _job: Id, _origin: &Origin) -> Result<Admitted, Short> {
        Ok(Admitted {
            store: self.store.clone(),
            victims: Vec::new(),
        })
    }
    fn release(&mut self, _job: Id, _victims: &[Victim]) {}
    fn committed(
        &mut self,
        _job: Id,
        _settled: &JobSettled,
        _sequence: u64,
        _capture: Option<capture_worker::Handle>,
    ) {
    }
    fn abandon(&mut self, _job: Id, _capture: Option<capture_worker::Handle>) {}
    fn stored(&self, job: Id) -> [u64; 2] {
        self.stored.get(&job).copied().unwrap_or_default()
    }
}

/// A private scratch home, never `~/.asura`.
struct Scratch(std::path::PathBuf);
impl Scratch {
    fn new() -> Self {
        use std::os::unix::fs::DirBuilderExt;
        let path = std::path::PathBuf::from(format!(
            "/private/tmp/asura-job-control-{:x}",
            u128::from_ne_bytes(asura_platform::random_id())
        ));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .expect("scratch home");
        Self(path)
    }
    fn runtime(&self) -> RuntimeDirectory {
        RuntimeDirectory::scratch(&self.0, true).expect("scratch runtime")
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

// ----- Fixtures: journal, registry and pages -----

fn call(command: &str) -> ToolCall {
    let mut quoted = String::new();
    asura_platform::secrets::json::write_canonical_string(&mut quoted, command);
    let arguments = format!("{{\"command\":{quoted},\"cwd\":\"src\"}}");
    ToolCall {
        operation: [1; 16],
        generation: 1,
        ordinal: 1,
        source: ToolCallSource::ProviderJson,
        name: "exec_cmd".into(),
        name_bytes: 8,
        tool_operation: Some(ToolOperation::ExecCmd),
        decision: ToolDecision::Run,
        fault: None,
        reason: None,
        form: ArgumentForm::Canonical,
        arguments_bytes: arguments.len() as u32,
        arguments,
        number_kinds: vec![],
        unnumbered: 0,
        rule_set_revision: 1,
        rule_set_digest: [0; 32],
        command_cut: false,
        whole_command: false,
        refusal: None,
    }
}

#[derive(Default)]
struct Journal {
    jobs: BTreeMap<Id, JobRecords>,
    /// Project and conversation of each submitted job (kind 26 holds neither).
    submitted: BTreeMap<Id, (Id, Id, String)>,
    projects: BTreeSet<Id>,
    sequence: u64,
    unavailable: bool,
    /// Appends answer `JournalQuota` (held, nothing written).
    held: bool,
    appended: Vec<JobRetention>,
    reads: Vec<&'static str>,
}
impl Journal {
    fn commit(&mut self, job: Id, record: &Record) -> u64 {
        self.sequence += 1;
        let at = Locator {
            sequence: self.sequence,
            ..Locator::NONE
        };
        match record {
            Record::JobIntent(intent) => {
                let (project, conversation, command) =
                    self.submitted.get(&job).cloned().expect("submitted job");
                self.jobs.insert(
                    job,
                    JobRecords {
                        listing: JobListing {
                            job,
                            conversation,
                            project,
                            intent: *intent,
                            intent_frame: at,
                            cancel: None,
                            cancel_frame: Locator::NONE,
                            settled: None,
                            settled_frame: Locator::NONE,
                            command: Locator::NONE,
                            excerpt: journal::command_excerpt(&command),
                            retention: Default::default(),
                        },
                        command: Box::new(call(&command)),
                        retention: Vec::new(),
                        model_notes: Vec::new(),
                    },
                );
            }
            Record::JobCancelRequested(c) => {
                let r = self.jobs.get_mut(&job).expect("kind 26 first");
                r.listing.cancel = Some(*c);
                r.listing.cancel_frame = at;
            }
            Record::JobSettled(s) => {
                let r = self.jobs.get_mut(&job).expect("kind 26 first");
                r.listing.settled = Some(s.clone());
                r.listing.settled_frame = at;
            }
            other => panic!("unexpected controller record {other:?}"),
        }
        self.sequence
    }
    fn answer(&mut self, work: JournalWork) -> writer::Result<writer::Reply> {
        if self.unavailable {
            return Err(writer::Error::Unavailable);
        }
        let mut reply = writer::Reply::default();
        match work {
            JournalWork::Read(writer::Command::JobFacts { job }) => {
                self.reads.push("JobFacts");
                reply.job_records = self.jobs.get(&job).cloned().map(Box::new);
            }
            JournalWork::Read(writer::Command::Jobs {
                scope,
                before,
                limit,
            }) => {
                self.reads.push("Jobs");
                assert!((1..=9).contains(&limit), "Jobs limit 1 to 9");
                let mut jobs: Vec<JobListing> = self
                    .jobs
                    .values()
                    .map(|r| r.listing.clone())
                    .filter(|l| before.is_none_or(|b| l.intent_frame.sequence < b))
                    .filter(|l| match scope {
                        JobScope::Conversation(c) => l.conversation == c,
                        JobScope::Project(p) => l.project == p,
                        JobScope::AllProjects => true,
                    })
                    .collect();
                jobs.sort_by_key(|l| std::cmp::Reverse(l.intent_frame.sequence));
                jobs.truncate(limit);
                reply.jobs = jobs;
            }
            JournalWork::Read(writer::Command::Project { project }) => {
                self.reads.push("Project");
                if self.projects.contains(&project) {
                    reply.projects.push(writer::ProjectListing {
                        registration: ProjectRegistered {
                            request: [1; 16],
                            digest: [0; 32],
                            project,
                            location: "/private/tmp/project".into(),
                            device: 1,
                            inode: 2,
                            registry_revision: 1,
                            visibility: 1,
                        },
                        name: "project".into(),
                        name_revision: 1,
                        error: None,
                    });
                }
            }
            JournalWork::Append(Record::JobRetention(r)) => {
                if self.held {
                    return Err(writer::Error::JournalQuota);
                }
                self.sequence += 1;
                let records = self.jobs.get_mut(&r.job).expect("named job");
                assert!(
                    records
                        .listing
                        .retention
                        .allows(r.action, records.listing.settled.is_some()),
                    "replay rule: {r:?}"
                );
                records.listing.retention.apply(r.action);
                records.retention.push((
                    Locator {
                        sequence: self.sequence,
                        ..Locator::NONE
                    },
                    (*r).clone(),
                ));
                self.appended.push(*r);
            }
            other => panic!("unexpected journal work {other:?}"),
        }
        Ok(reply)
    }
}

#[derive(Default)]
struct Registry {
    /// `None` entries are unknown in memory (a `Project` read decides).
    known: BTreeMap<Id, Option<bool>>,
    execution_off: BTreeSet<Id>,
}
impl Projects for Registry {
    fn registered(&self, project: Id) -> Option<bool> {
        self.known.get(&project).copied().unwrap_or(Some(true))
    }
    fn location(&self, _project: Id) -> Option<String> {
        Some("/private/tmp/project".into())
    }
    fn execution_enabled(&self, project: Id) -> bool {
        !self.execution_off.contains(&project)
    }
}

/// Page reads answered from in-memory payload bytes. `hold` keeps replies
/// back until released; `capacity` is the pool queue (8 in production).
#[derive(Default)]
struct Pages {
    bytes: RefCell<BTreeMap<Id, (PageState, Vec<u8>)>>,
    hold: Cell<bool>,
    capacity: Cell<usize>,
    outstanding: Rc<Cell<usize>>,
    submitted: RefCell<Vec<PageRequest>>,
    held: RefCell<Vec<Rc<RefCell<Option<PageReply>>>>>,
}
struct Wait {
    reply: Rc<RefCell<Option<PageReply>>>,
    outstanding: Rc<Cell<usize>>,
}
impl PageWait for Wait {
    fn poll(&self) -> Option<PageReply> {
        self.reply.borrow_mut().take()
    }
}
impl Drop for Wait {
    fn drop(&mut self) {
        self.outstanding.set(self.outstanding.get() - 1);
    }
}
impl Pages {
    fn page(&self, request: PageRequest) -> PageReply {
        let bytes = self.bytes.borrow();
        let Some((state, data)) = bytes.get(&request.payload) else {
            return PageReply::Page(Page {
                state: PageState::Missing,
                start: request.offset,
                text: String::new(),
                next_offset: request.offset,
                total: 0,
                digest: None,
                watermark: None,
            });
        };
        let total = data.len() as u64;
        if request.offset > total {
            return PageReply::Page(Page {
                state: PageState::StaleCursor,
                start: request.offset,
                text: String::new(),
                next_offset: total,
                total,
                digest: None,
                watermark: None,
            });
        }
        let start = request.offset as usize;
        let end = (start + request.limit).min(data.len());
        let readable = matches!(
            state,
            PageState::Live | PageState::Complete | PageState::Incomplete
        );
        let (text, next) = if readable {
            (
                String::from_utf8_lossy(&data[start..end]).into_owned(),
                end as u64,
            )
        } else {
            (String::new(), request.offset)
        };
        PageReply::Page(Page {
            state: *state,
            start: request.offset,
            text,
            next_offset: next,
            total: if readable { total } else { 0 },
            digest: None,
            watermark: None,
        })
    }
    fn release(&self) {
        self.hold.set(false);
        for slot in self.held.borrow_mut().drain(..) {
            let _ = slot;
        }
    }
}
impl PagePort for Pages {
    fn submit(&self, request: PageRequest) -> Result<Box<dyn PageWait>, PageState> {
        if self.outstanding.get() >= self.capacity.get() {
            return Err(PageState::Busy);
        }
        self.outstanding.set(self.outstanding.get() + 1);
        self.submitted.borrow_mut().push(request);
        let reply = Rc::new(RefCell::new(None));
        if self.hold.get() {
            self.held.borrow_mut().push(reply.clone());
        } else {
            *reply.borrow_mut() = Some(self.page(request));
        }
        Ok(Box::new(Wait {
            reply,
            outstanding: self.outstanding.clone(),
        }))
    }
}

// ----- Harness -----

struct Env {
    ctl: JobController,
    slots: Slots,
    payloads: Payloads,
    journal: Journal,
    registry: Registry,
    pages: Pages,
    pages_ready: bool,
    held: bool,
    now: Instant,
    runtime: RuntimeDirectory,
    _scratch: Scratch,
}
impl Env {
    fn ctx(&mut self) -> Ctx<'_> {
        Ctx {
            jobs: &mut self.ctl,
            pages: if self.pages_ready {
                Some(&self.pages)
            } else {
                None
            },
            projects: &self.registry,
            installation: INSTALLATION,
            journal_held: self.held,
            now: self.now,
        }
    }
}

struct H {
    jc: JobControl,
    env: Env,
    replies: Vec<pb::Envelope>,
    counter: u64,
}
impl H {
    fn new(viewers: u32) -> Self {
        let scratch = Scratch::new();
        let runtime = scratch.runtime();
        let slots = Slots::default();
        let mut ctl = JobController::with_audit(
            JobLimits::default(),
            None,
            runtime.clone(),
            factory(slots.clone()),
        );
        ctl.recover(Vec::new());
        let store = Arc::new(PayloadStore::open(&runtime).expect("payload store"));
        let mut journal = Journal::default();
        journal.projects.extend([P1, P2, P3]);
        let pages = Pages::default();
        pages.capacity.set(8);
        let mut h = Self {
            jc: JobControl::new(viewers),
            env: Env {
                ctl,
                slots,
                payloads: Payloads {
                    store,
                    stored: BTreeMap::new(),
                },
                journal,
                registry: Registry::default(),
                pages,
                pages_ready: true,
                held: false,
                now: Instant::now(),
                runtime,
                _scratch: scratch,
            },
            replies: Vec::new(),
            counter: 0,
        };
        h.settle();
        h
    }

    /// Runs the owner's loop until nothing moves: controller records,
    /// client waits, handler journal work and page polls.
    fn settle(&mut self) {
        for _ in 0..500 {
            let mut moved = false;
            let env = &mut self.env;
            env.ctl.poll(env.now, &mut env.payloads);
            while let Some((job, record)) = env.ctl.next_record() {
                let sequence = env.journal.commit(job, &record);
                env.ctl.record_done(job, Ok(sequence));
                env.ctl.poll(env.now, &mut env.payloads);
                moved = true;
            }
            let mut done = Vec::new();
            env.ctl.drain_waits(&mut done);
            for d in done {
                moved = true;
                self.jc.wait_done(d, &mut self.env.ctx());
            }
            while let Some((key, work)) = self.jc.next_journal() {
                moved = true;
                let result = self.env.journal.answer(work);
                self.jc.journal_done(key, result, &mut self.env.ctx());
            }
            self.jc.poll(&mut self.env.ctx());
            let before = self.replies.len();
            self.jc.drain_replies(&mut self.replies);
            moved |= self.replies.len() > before || self.jc.has_journal_work();
            if !moved {
                break;
            }
        }
    }

    fn envelope(&mut self, attachment: u8, body: Body) -> pb::Envelope {
        self.counter += 1;
        let envelope = pb::Envelope {
            service_epoch: Some(EPOCH.to_vec()),
            attachment_id: Some(vec![attachment; 16]),
            request_counter: Some(self.counter),
            body: Some(body),
        };
        validate_semantics(&envelope, Direction::ClientToServer)
            .unwrap_or_else(|e| panic!("invalid test request {envelope:?}: {e:?}"));
        envelope
    }

    /// Sends a request on `attachment`; returns the reply when it came now
    /// or after the loop settled, else `None` (pending).
    fn send(&mut self, attachment: u8, body: Body) -> Option<Body> {
        let envelope = self.envelope(attachment, body);
        if let Some(reply) = self.jc.request(envelope, &mut self.env.ctx()) {
            return Some(checked(reply));
        }
        self.settle();
        self.take(attachment)
    }

    /// The reply for `attachment`, if one arrived.
    fn take(&mut self, attachment: u8) -> Option<Body> {
        let at = self
            .replies
            .iter()
            .position(|r| r.attachment_id.as_deref() == Some(&[attachment; 16][..]))?;
        Some(checked(self.replies.remove(at)))
    }

    fn submit(&mut self, origin: Origin, conversation: Id, project: Id, command: &str) -> Id {
        let request = StartRequest {
            origin,
            agent: AgentKey {
                conversation,
                agent: 0,
            },
            scope: tools::ExecutionScope {
                project,
                path: "/private/tmp/project".into(),
                device: 1,
                inode: 2,
                boundary: self.env.runtime.state_boundary(),
            },
            profile: Profile::Model1,
            command: command.into(),
            cwd: "src".into(),
            secrets: Vec::new(),
            secret_command: None,
            redaction: asura_platform::secrets::JobRedaction::new(
                asura_platform::secrets::detection().expect("rules"),
                &[],
            ),
            duplicate_digest: [1; 32],
        };
        let job = origin.job_id();
        self.env
            .journal
            .submitted
            .insert(job, (project, conversation, command.into()));
        let Admission::Accepted { job: accepted } = self.env.ctl.submit(request, self.env.now)
        else {
            panic!("start refused");
        };
        assert_eq!(accepted, job);
        self.settle();
        job
    }
    /// A model job, committed and running.
    fn running(&mut self, n: u8, conversation: Id, project: Id) -> Id {
        let job = self.submit(model(n), conversation, project, "cargo build --workspace");
        self.script(job, [SlotEvent::Spawned]);
        job
    }
    fn script(&mut self, job: Id, events: impl IntoIterator<Item = SlotEvent>) {
        self.env
            .slots
            .borrow_mut()
            .scripts
            .entry(job)
            .or_default()
            .extend(events);
        self.settle();
    }
    /// Runs a spawned job to `exited 0` with `stdout` stored bytes and
    /// releases its slot.
    fn finish(&mut self, job: Id, stdout: u64) {
        let exit = Exit::Exited(0);
        self.script(
            job,
            [
                SlotEvent::LeaderExited(exit),
                SlotEvent::ProcessSettled {
                    exit,
                    cleanup: JobCleanup::Confirmed,
                    escapees: 0,
                    elapsed_ms: 42,
                    output_held: false,
                },
                SlotEvent::CaptureSettled(Box::new(CaptureOutcome {
                    state: CaptureState::Complete,
                    reason: CaptureReason::None,
                    stdout: JobStream {
                        capture: CaptureStream {
                            observed_bytes: stdout,
                            stored_bytes: stdout,
                            digest: Some([7; 32]),
                            eof: true,
                            ..Default::default()
                        },
                        lines: 2,
                    },
                    stderr: JobStream {
                        capture: CaptureStream {
                            eof: true,
                            digest: Some([8; 32]),
                            ..Default::default()
                        },
                        lines: 0,
                    },
                    distinct_numbers: 0,
                    numbers: Vec::new(),
                    pause_count: 0,
                    first_pause_ms: 0,
                    paused_ms: 0,
                    final_reservation: 1024 * 1024,
                    capture: None,
                })),
                SlotEvent::ScratchCleaned,
            ],
        );
        self.env.slots.borrow_mut().idle.insert(job);
        self.settle();
    }
    fn stored(&mut self, job: Id, bytes: [u64; 2]) {
        self.env.payloads.stored.insert(job, bytes);
        self.settle();
    }
    fn output(&mut self, job: Id, state: PageState, stdout: &[u8]) {
        let [payload, _] = payload::job_payloads(INSTALLATION, job);
        self.env
            .pages
            .bytes
            .borrow_mut()
            .insert(payload, (state, stdout.to_vec()));
    }
    fn kind_27(&self, job: Id) -> Option<journal::JobCancelRequested> {
        self.env.journal.jobs.get(&job)?.listing.cancel
    }
}

fn model(n: u8) -> Origin {
    Origin::Model {
        operation: [n; 16],
        generation: 1,
        ordinal: 1,
    }
}
fn person(n: u8) -> Origin {
    Origin::Person { input: [n; 16] }
}

/// Every reply must pass the control codec's server-to-client rules.
fn checked(reply: pb::Envelope) -> Body {
    validate_semantics(&reply, Direction::ServerToClient)
        .unwrap_or_else(|e| panic!("invalid reply {reply:?}: {e:?}"));
    reply.body.expect("reply body")
}

fn list(scope: i32, project: Option<Id>, conversation: Option<Id>) -> pb::JobList {
    pb::JobList {
        scope: Some(scope),
        project_id: project.map(|p| p.to_vec()),
        conversation_id: conversation.map(|c| c.to_vec()),
        ..Default::default()
    }
}
fn observe(job: Id, offset: u64) -> Body {
    Body::JobObserve(pb::JobObserve {
        job_id: Some(job.to_vec()),
        stream: Some(pb::JobStream::Stdout as i32),
        offset: Some(offset),
        ..Default::default()
    })
}
fn read(job: Id, offset: u64) -> Body {
    Body::JobRead(pb::JobRead {
        job_id: Some(job.to_vec()),
        stream: Some(pb::JobStream::Stdout as i32),
        offset: Some(offset),
        ..Default::default()
    })
}
fn inspect(job: Id, fence: Option<Id>) -> Body {
    Body::JobInspect(pb::JobInspect {
        job_id: Some(job.to_vec()),
        project_id: fence.map(|p| p.to_vec()),
    })
}
fn cancel(request: u8, job: Id, project: Id, kill: bool) -> Body {
    Body::JobCancel(pb::JobCancel {
        request_id: Some(vec![request; 16]),
        job_id: Some(job.to_vec()),
        expected_project_id: Some(project.to_vec()),
        kill: kill.then_some(true),
    })
}
fn retain(request: u8, job: Id, project: Id, action: RetentionAction) -> Body {
    Body::JobRetain(pb::JobRetain {
        request_id: Some(vec![request; 16]),
        job_id: Some(job.to_vec()),
        expected_project_id: Some(project.to_vec()),
        action: Some(action as i32),
        note: None,
        confirmed: (action == RetentionAction::Delete).then_some(true),
    })
}
fn note(request: u8, job: Id, project: Id, text: &str) -> Body {
    Body::JobRetain(pb::JobRetain {
        request_id: Some(vec![request; 16]),
        job_id: Some(job.to_vec()),
        expected_project_id: Some(project.to_vec()),
        action: Some(RetentionAction::Note as i32),
        note: Some(text.into()),
        confirmed: None,
    })
}

fn page(body: Option<Body>) -> pb::JobPage {
    match body {
        Some(Body::JobPage(v)) => v,
        other => panic!("JobPage expected, got {other:?}"),
    }
}
fn listing(body: Option<Body>) -> pb::JobListReply {
    match body {
        Some(Body::JobListReply(v)) => v,
        other => panic!("JobListReply expected, got {other:?}"),
    }
}
fn detail(body: Option<Body>) -> pb::JobDetail {
    match body {
        Some(Body::JobDetail(v)) => v,
        other => panic!("JobDetail expected, got {other:?}"),
    }
}
fn cancelled(body: Option<Body>) -> pb::JobCancelReply {
    match body {
        Some(Body::JobCancelReply(v)) => v,
        other => panic!("JobCancelReply expected, got {other:?}"),
    }
}
fn retained(body: Option<Body>) -> pb::JobRetainReply {
    match body {
        Some(Body::JobRetainReply(v)) => v,
        other => panic!("JobRetainReply expected, got {other:?}"),
    }
}
fn state(entry: &Option<pb::JobEntry>) -> Option<i32> {
    entry.as_ref().and_then(|e| e.state)
}
const SETTLED: i32 = pb::JobState::Settled as i32;
const RUNNING: i32 = pb::JobState::Running as i32;

// ----- J4T-04: parked requests, viewer bound, detach, shutdown -----

/// J4T-04: a parked `JobObserve` returns when the named stream's stored
/// bytes pass the client's offset, and again at settlement; nothing but
/// those events ends it (no timer).
#[test]
fn j4t_04_observe_ends_on_new_output_and_settlement() {
    let mut h = H::new(8);
    let job = h.running(1, C1, P1);
    h.output(job, PageState::Live, b"hello\n");
    assert!(
        h.send(1, observe(job, 0)).is_none(),
        "parks: nothing stored"
    );
    // Time alone ends nothing.
    h.env.now += std::time::Duration::from_secs(3600);
    h.settle();
    assert!(h.take(1).is_none());
    h.stored(job, [6, 0]);
    let reply = page(h.take(1));
    assert_eq!(state(&reply.entry), Some(RUNNING));
    assert_eq!(reply.pages.len(), 1);
    assert_eq!(reply.pages[0].text.as_deref(), Some("hello\n"));
    assert_eq!(reply.pages[0].state, Some(pb::JobPageState::Live as i32));
    // The client holds 6 bytes: the next observe parks until settlement.
    assert!(h.send(1, observe(job, 6)).is_none());
    h.output(job, PageState::Complete, b"hello\n");
    h.finish(job, 6);
    let reply = page(h.take(1));
    let entry = reply.entry.expect("entry");
    assert_eq!(entry.state, Some(SETTLED));
    assert_eq!(entry.stdout_bytes, Some(6));
    assert_eq!(
        reply.pages[0].state,
        Some(pb::JobPageState::Complete as i32)
    );
    assert_eq!(h.jc.pending(), 0);
}

/// J4T-04: an observe whose `after_state` already differs, or of a settled
/// job, returns at once; with no stream it pages both streams at the end.
#[test]
fn j4t_04_observe_returns_at_once_when_state_differs() {
    let mut h = H::new(8);
    let job = h.running(1, C1, P1);
    h.output(job, PageState::Live, b"abc");
    let body = Body::JobObserve(pb::JobObserve {
        job_id: Some(job.to_vec()),
        stream: Some(pb::JobStream::Stdout as i32),
        offset: Some(0),
        after_state: Some(pb::JobState::Starting as i32),
        ..Default::default()
    });
    let reply = page(h.send(1, body));
    assert_eq!(state(&reply.entry), Some(RUNNING));
    h.finish(job, 3);
    let both = Body::JobObserve(pb::JobObserve {
        job_id: Some(job.to_vec()),
        at_end: Some(true),
        ..Default::default()
    });
    let reply = page(h.send(1, both));
    assert_eq!(state(&reply.entry), Some(SETTLED));
    let streams: Vec<_> = reply.pages.iter().map(|p| p.stream).collect();
    assert_eq!(
        streams,
        [
            Some(pb::JobStream::Stdout as i32),
            Some(pb::JobStream::Stderr as i32)
        ]
    );
}

/// J4T-04: a `JobList` with `after_revision` parks until the job revision
/// differs; an older revision returns at once.
#[test]
fn j4t_04_list_parks_until_the_revision_changes() {
    let mut h = H::new(8);
    let revision = h.env.ctl.revision();
    let mut query = list(1, Some(P1), None);
    query.after_revision = Some(revision);
    query.live_only = Some(true);
    assert!(h.send(1, Body::JobList(query.clone())).is_none());
    h.settle();
    assert!(h.take(1).is_none(), "no change, no reply");
    let job = h.running(1, C1, P1);
    let reply = listing(h.take(1));
    assert!(reply.revision > Some(revision));
    assert_eq!(reply.live_in_scope, Some(1));
    assert_eq!(reply.live_in_service, Some(1));
    assert_eq!(reply.entries.len(), 1);
    assert_eq!(reply.entries[0].job_id.as_deref(), Some(&job[..]));
    assert_eq!(reply.execution_enabled, Some(true));
    // An older revision is answered at once.
    let reply = listing(h.send(1, Body::JobList(query)));
    assert!(reply.error.is_none());
}

/// J4T-04: detach drops a parked observe and a pending read; closing never
/// cancels the job.
#[test]
fn j4t_04_detach_drops_parked_and_pending_requests() {
    let mut h = H::new(8);
    let job = h.running(1, C1, P1);
    assert!(h.send(1, observe(job, 0)).is_none());
    h.env.pages.hold.set(true);
    h.output(job, PageState::Live, b"x");
    h.stored(job, [1, 0]);
    assert!(h.send(2, read(job, 0)).is_none(), "page read pending");
    assert_eq!(h.jc.pending(), 2);
    h.jc.retain(&[], &mut h.env.ctx());
    assert_eq!(h.jc.pending(), 0);
    assert!(
        !h.env.ctl.cancel_wait(WaitKey::Client(1)),
        "the parked wait left the controller"
    );
    assert_eq!(
        h.env.pages.outstanding.get(),
        0,
        "the page read was withdrawn"
    );
    h.env.pages.release();
    h.settle();
    assert!(h.replies.is_empty(), "no reply after detach");
    assert!(h.env.slots.borrow().stops.is_empty(), "the job continues");
    assert!(h.kind_27(job).is_none());
}

/// J4T-04: `jobs.clientViewers` pending reads, observes and parked lists;
/// one more is `busy`. The page pool's queue is a separate bound: a full
/// queue is `busy` while viewer places remain.
#[test]
fn j4t_04_viewer_bound_and_page_queue_are_separate() {
    let mut h = H::new(2);
    let job = h.running(1, C1, P1);
    assert!(h.send(1, observe(job, 0)).is_none());
    assert!(h.send(2, observe(job, 0)).is_none());
    let reply = page(h.send(3, read(job, 0)));
    assert_eq!(reply.error.as_deref(), Some(wire::error::BUSY));
    // Inspect does not count toward the viewer bound.
    let reply = detail(h.send(4, inspect(job, None)));
    assert!(reply.error.is_none());
    // A detach frees a place; a full page queue is still `busy`.
    h.jc.retain(&[[2; 16]], &mut h.env.ctx());
    h.env.pages.capacity.set(0);
    h.stored(job, [1, 0]);
    let reply = page(h.send(3, read(job, 0)));
    assert_eq!(reply.error.as_deref(), Some(wire::error::BUSY));
    h.env.pages.capacity.set(8);
    h.output(job, PageState::Live, b"x");
    let reply = page(h.send(3, read(job, 0)));
    assert!(reply.error.is_none());
}

/// J4T-04 (AE-06): shutdown answers parked and pending requests with
/// `Draining`, and every later request.
#[test]
fn j4t_04_shutdown_answers_draining() {
    let mut h = H::new(8);
    let job = h.running(1, C1, P1);
    assert!(h.send(1, observe(job, 0)).is_none());
    let mut query = list(2, None, None);
    query.after_revision = Some(h.env.ctl.revision());
    assert!(h.send(2, Body::JobList(query)).is_none());
    h.env.ctl.close();
    h.jc.close(&mut h.env.ctx());
    h.settle();
    for attachment in [1, 2] {
        match h.take(attachment) {
            Some(Body::Error(e)) => assert_eq!(e.code, Some(pb::ErrorCode::Draining as i32)),
            other => panic!("Draining expected, got {other:?}"),
        }
    }
    match h.send(3, inspect(job, None)) {
        Some(Body::Error(e)) => assert_eq!(e.code, Some(pb::ErrorCode::Draining as i32)),
        other => panic!("Draining expected, got {other:?}"),
    }
    assert_eq!(h.jc.pending(), 0);
}

// ----- J4T-05: cancel and kill -----

/// J4T-05: a model-origin job is cancelled with requester client, reason 3
/// `client_cancel`; a repeated request is idempotent (one kind 27).
#[test]
fn j4t_05_model_job_cancel_is_client_cancel() {
    let mut h = H::new(8);
    let job = h.running(1, C1, P1);
    let reply = cancelled(h.send(1, cancel(1, job, P1, false)));
    assert_eq!(reply.requested, Some(true));
    assert_eq!(reply.request_id.as_deref(), Some(&[1; 16][..]));
    let entry = reply.entry.expect("entry");
    assert_eq!(entry.cancel_requested, Some(true));
    assert_eq!(entry.state, Some(pb::JobState::Stopping as i32));
    let c = h.kind_27(job).expect("kind 27");
    assert_eq!(c.requester, journal::JobRequester::Client);
    assert_eq!(c.reason, JobReason::ClientCancel);
    assert!(!c.kill);
    assert_eq!(h.env.slots.borrow().stops, [(job, StopKind::Term)]);
    let reply = cancelled(h.send(1, cancel(2, job, P1, false)));
    assert_eq!(reply.requested, Some(true));
    assert_eq!(h.env.slots.borrow().stops.len(), 1, "no second T");
}

/// J4T-05: `kill` on a model-origin job is `kill_not_permitted` and
/// changes nothing.
#[test]
fn j4t_05_kill_of_a_model_job_is_not_permitted() {
    let mut h = H::new(8);
    let job = h.running(1, C1, P1);
    let reply = cancelled(h.send(1, cancel(1, job, P1, true)));
    assert_eq!(
        reply.error.as_deref(),
        Some(wire::error::KILL_NOT_PERMITTED)
    );
    assert!(h.kind_27(job).is_none());
    assert!(h.env.slots.borrow().stops.is_empty());
}

/// J4T-05: a person-origin job is stopped with requester person, reason 4
/// `person_stop`, and killed with reason 5 `person_kill` (fixture
/// controller: person jobs are not reachable through the journal before P2).
#[test]
fn j4t_05_person_job_stop_and_kill() {
    let mut h = H::new(8);
    let job = h.submit(person(5), C1, P1, "make test");
    h.script(job, [SlotEvent::Spawned]);
    let reply = cancelled(h.send(1, cancel(1, job, P1, false)));
    assert_eq!(reply.requested, Some(true));
    let entry = reply.entry.expect("entry");
    assert_eq!(entry.origin, Some(pb::JobOrigin::Person as i32));
    assert_eq!(entry.input_id.as_deref(), Some(&[5; 16][..]));
    let c = h.kind_27(job).expect("kind 27");
    assert_eq!(
        (c.requester, c.reason, c.kill),
        (journal::JobRequester::Person, JobReason::PersonStop, false)
    );
    let reply = cancelled(h.send(1, cancel(2, job, P1, true)));
    assert_eq!(reply.requested, Some(true));
    assert_eq!(
        h.env.slots.borrow().stops,
        [(job, StopKind::Term), (job, StopKind::Kill)]
    );
}

/// J4T-05: a settled job replies `requested: false` with its settled
/// entry, from the controller's memory or from the journal alone.
#[test]
fn j4t_05_settled_job_replies_requested_false() {
    let mut h = H::new(8);
    let job = h.running(1, C1, P1);
    h.finish(job, 4);
    let reply = cancelled(h.send(1, cancel(1, job, P1, false)));
    assert_eq!(reply.requested, Some(false));
    assert_eq!(state(&reply.entry), Some(SETTLED));
    assert!(h.kind_27(job).is_none());
    // A settled job the controller no longer holds: journal facts only.
    let old = h.running(2, C1, P1);
    h.finish(old, 0);
    let mut fresh = H::new(8);
    fresh.env.journal.jobs = std::mem::take(&mut h.env.journal.jobs);
    let reply = cancelled(fresh.send(1, cancel(3, old, P1, false)));
    assert_eq!(reply.requested, Some(false));
    assert_eq!(state(&reply.entry), Some(SETTLED));
    // An unknown job is `not_found`.
    let reply = cancelled(fresh.send(1, cancel(4, [0x5a; 16], P1, false)));
    assert_eq!(reply.error.as_deref(), Some(wire::error::NOT_FOUND));
}

// ----- J4T-06: retention -----

/// J4T-06: pin and unpin follow the kind 30 rules; a request that changes
/// nothing writes no record and replies `changed: false`; each commit
/// increments the revision and reaches the payload owner.
#[test]
fn j4t_06_pin_and_unpin_rules() {
    let mut h = H::new(8);
    let job = h.running(1, C1, P1);
    let revision = h.env.ctl.revision();
    let reply = retained(h.send(1, retain(1, job, P1, RetentionAction::Pin)));
    assert_eq!(reply.changed, Some(true));
    assert_eq!(reply.entry.as_ref().and_then(|e| e.pinned), Some(true));
    assert!(h.env.ctl.revision() > revision);
    let mut effects = Vec::new();
    h.jc.drain_retained(&mut effects);
    assert_eq!(
        effects,
        [Retained {
            job,
            action: RetentionAction::Pin
        }]
    );
    let r = &h.env.journal.appended[0];
    assert_eq!(r.author, RetentionAuthor::Person { request: [1; 16] });
    assert!(r.note.is_empty() && r.time > 0);
    let reply = retained(h.send(1, retain(2, job, P1, RetentionAction::Pin)));
    assert_eq!(reply.changed, Some(false));
    assert_eq!(h.env.journal.appended.len(), 1, "no record for no change");
    let reply = retained(h.send(1, retain(3, job, P1, RetentionAction::Unpin)));
    assert_eq!(reply.changed, Some(true));
    assert_eq!(reply.entry.as_ref().and_then(|e| e.pinned), Some(false));
    let reply = retained(h.send(1, retain(4, job, P1, RetentionAction::Unpin)));
    assert_eq!(reply.changed, Some(false));
    assert_eq!(h.env.journal.appended.len(), 2);
}

/// J4T-06: delete of a live job is `not_settled`; after kind 28 it commits
/// once; a second delete changes nothing.
#[test]
fn j4t_06_delete_after_settlement_once() {
    let mut h = H::new(8);
    let job = h.running(1, C1, P1);
    let reply = retained(h.send(1, retain(1, job, P1, RetentionAction::Delete)));
    assert_eq!(reply.error.as_deref(), Some(wire::error::NOT_SETTLED));
    assert!(h.env.journal.appended.is_empty());
    h.finish(job, 4);
    let reply = retained(h.send(1, retain(2, job, P1, RetentionAction::Delete)));
    assert_eq!(reply.changed, Some(true));
    assert_eq!(reply.entry.as_ref().and_then(|e| e.deleted), Some(true));
    let reply = retained(h.send(1, retain(3, job, P1, RetentionAction::Delete)));
    assert_eq!(reply.changed, Some(false));
    let mut effects = Vec::new();
    h.jc.drain_retained(&mut effects);
    assert_eq!(effects.len(), 1);
    assert_eq!(effects[0].action, RetentionAction::Delete);
    // The deleted output reads as `deleted` pages.
    h.output(job, PageState::Deleted, b"");
    let reply = page(h.send(2, read(job, 0)));
    assert_eq!(reply.pages[0].state, Some(pb::JobPageState::Deleted as i32));
    assert_eq!(reply.pages[0].total_bytes, Some(4));
}

/// J4T-06: a person note is display-redacted with the unnumbered
/// placeholder before its record; an invalid note is `note_invalid`; the
/// detail lists the note.
#[test]
fn j4t_06_note_redaction_and_validation() {
    let mut h = H::new(8);
    let job = h.running(1, C1, P1);
    let text = format!("deploy with --token {}\nsecond line", secret());
    let reply = retained(h.send(1, note(1, job, P1, &text)));
    assert_eq!(reply.changed, Some(true));
    assert_eq!(reply.entry.as_ref().and_then(|e| e.notes), Some(1));
    let stored = h.env.journal.appended[0].note.clone();
    assert!(!stored.contains(&secret()), "{stored}");
    assert_eq!(stored, "deploy with --token $ASURA_SECRET_0\nsecond line");
    for bad in ["", "bell\u{7}", &"a".repeat(wire::MAX_JOB_NOTE_BYTES + 1)] {
        let reply = retained(h.send(1, note(2, job, P1, bad)));
        assert_eq!(
            reply.error.as_deref(),
            Some(wire::error::NOTE_INVALID),
            "{bad:?}"
        );
    }
    assert_eq!(h.env.journal.appended.len(), 1);
    let reply = detail(h.send(2, inspect(job, Some(P1))));
    assert_eq!(reply.notes.len(), 1);
    assert_eq!(reply.notes[0].author, Some(1));
    assert_eq!(reply.notes[0].text.as_deref(), Some(stored.as_str()));
}

/// J4T-06: 16 notes and 64 retention records per job.
#[test]
fn j4t_06_notes_full_and_retention_full() {
    let mut h = H::new(8);
    let job = h.running(1, C1, P1);
    let summary = &mut h.env.journal.jobs.get_mut(&job).unwrap().listing.retention;
    summary.notes = 16;
    summary.records = 16;
    let reply = retained(h.send(1, note(1, job, P1, "more")));
    assert_eq!(reply.error.as_deref(), Some(wire::error::NOTES_FULL));
    let summary = &mut h.env.journal.jobs.get_mut(&job).unwrap().listing.retention;
    summary.notes = 0;
    summary.records = 64;
    let reply = retained(h.send(1, retain(2, job, P1, RetentionAction::Pin)));
    assert_eq!(reply.error.as_deref(), Some(wire::error::RETENTION_FULL));
    // A request that changes nothing is answered without room.
    let reply = retained(h.send(1, retain(3, job, P1, RetentionAction::Unpin)));
    assert_eq!(reply.changed, Some(false));
    assert!(h.env.journal.appended.is_empty());
}

/// J4T-06: a repeated request ID with the same job and action returns the
/// earlier outcome without a second record.
#[test]
fn j4t_06_repeated_request_returns_the_earlier_outcome() {
    let mut h = H::new(8);
    let job = h.running(1, C1, P1);
    let first = retained(h.send(1, retain(7, job, P1, RetentionAction::Pin)));
    let again = retained(h.send(1, retain(7, job, P1, RetentionAction::Pin)));
    assert_eq!(first, again);
    assert_eq!(again.changed, Some(true));
    assert_eq!(h.env.journal.appended.len(), 1);
}

/// J4T-06: a held journal makes a retain wait for room; the hold's end
/// appends it. A detach while it waits writes nothing.
#[test]
fn j4t_06_held_journal_waits_for_room() {
    let mut h = H::new(8);
    let job = h.running(1, C1, P1);
    h.env.journal.held = true;
    assert!(
        h.send(1, retain(1, job, P1, RetentionAction::Pin))
            .is_none()
    );
    h.settle();
    assert!(h.take(1).is_none() && h.env.journal.appended.is_empty());
    // The append was refused once (held); the handler waits with no timer.
    h.env.held = true;
    h.env.journal.held = false;
    h.settle();
    assert!(h.take(1).is_none());
    h.env.held = false;
    h.settle();
    let reply = retained(h.take(1));
    assert_eq!(reply.changed, Some(true));
    assert_eq!(h.env.journal.appended.len(), 1);
    // Detached while held: no record.
    h.env.held = true;
    assert!(
        h.send(2, retain(2, job, P1, RetentionAction::Unpin))
            .is_none()
    );
    h.jc.retain(&[], &mut h.env.ctx());
    h.env.held = false;
    h.settle();
    assert_eq!(h.env.journal.appended.len(), 1);
    assert!(h.replies.is_empty());
}

/// J4T-06: retains run one at a time, so a second pin of the same job sees
/// the first one's record and changes nothing.
#[test]
fn j4t_06_retains_are_serialized() {
    let mut h = H::new(8);
    let job = h.running(1, C1, P1);
    let e1 = h.envelope(1, retain(1, job, P1, RetentionAction::Pin));
    let e2 = h.envelope(2, retain(2, job, P1, RetentionAction::Pin));
    assert!(h.jc.request(e1, &mut h.env.ctx()).is_none());
    assert!(h.jc.request(e2, &mut h.env.ctx()).is_none());
    h.settle();
    assert_eq!(retained(h.take(1)).changed, Some(true));
    assert_eq!(retained(h.take(2)).changed, Some(false));
    assert_eq!(h.env.journal.appended.len(), 1);
}

// ----- J4T-07: cross-project -----

/// J4T-07: a fence or expected project that differs is `project_mismatch`
/// and has no effect.
#[test]
fn j4t_07_project_mismatch() {
    let mut h = H::new(8);
    let job = h.running(1, C1, P1);
    let reply = detail(h.send(1, inspect(job, Some(P2))));
    assert_eq!(reply.error.as_deref(), Some(wire::error::PROJECT_MISMATCH));
    let mut body = read(job, 0);
    if let Body::JobRead(v) = &mut body {
        v.project_id = Some(P2.to_vec());
    }
    let reply = page(h.send(1, body));
    assert_eq!(reply.error.as_deref(), Some(wire::error::PROJECT_MISMATCH));
    let reply = cancelled(h.send(1, cancel(1, job, P2, false)));
    assert_eq!(reply.error.as_deref(), Some(wire::error::PROJECT_MISMATCH));
    assert!(h.kind_27(job).is_none());
    let reply = retained(h.send(1, retain(2, job, P2, RetentionAction::Pin)));
    assert_eq!(reply.error.as_deref(), Some(wire::error::PROJECT_MISMATCH));
    assert!(h.env.journal.appended.is_empty());
    // With the right expectation, another project's job is cancelled.
    let reply = cancelled(h.send(1, cancel(3, job, P1, false)));
    assert_eq!(reply.requested, Some(true));
    assert_eq!(
        reply.entry.and_then(|e| e.project_id).as_deref(),
        Some(&P1[..])
    );
}

/// J4T-07: the all-projects listing names each entry's project and skips
/// jobs of unregistered projects; an unknown project is checked with a
/// `Project` read.
#[test]
fn j4t_07_all_projects_listing_checks_the_registry() {
    let mut h = H::new(8);
    let a = h.running(1, C1, P1);
    let b = h.running(2, C2, P2);
    let c = h.running(3, C3, P3);
    h.env.registry.known.insert(P2, None);
    h.env.registry.known.insert(P3, Some(false));
    let reply = listing(h.send(1, Body::JobList(list(2, None, None))));
    let jobs: Vec<_> = reply.entries.iter().map(|e| e.job_id.clone()).collect();
    assert_eq!(jobs, [Some(b.to_vec()), Some(a.to_vec())]);
    let projects: Vec<_> = reply.entries.iter().map(|e| e.project_id.clone()).collect();
    assert_eq!(projects, [Some(P2.to_vec()), Some(P1.to_vec())]);
    assert!(h.env.journal.reads.contains(&"Project"));
    assert_eq!(
        reply.execution_enabled, None,
        "no EX11 value for all projects"
    );
    assert_eq!(reply.live_in_service, Some(3));
    let mut learned = Vec::new();
    h.jc.drain_learned(&mut learned);
    assert_eq!(learned.len(), 1);
    assert_eq!(learned[0].project, P2);
    // A job of an unregistered project is `not_found` everywhere.
    let reply = detail(h.send(1, inspect(c, None)));
    assert_eq!(reply.error.as_deref(), Some(wire::error::NOT_FOUND));
    let reply = cancelled(h.send(1, cancel(1, c, P3, false)));
    assert_eq!(reply.error.as_deref(), Some(wire::error::NOT_FOUND));
    assert!(h.kind_27(c).is_none());
    let reply = listing(h.send(1, Body::JobList(list(1, Some(P3), None))));
    assert_eq!(reply.error.as_deref(), Some(wire::error::NOT_FOUND));
}

// ----- Listing, inspect and read -----

/// Listing pages: newest first by kind 26 sequence, `next_before` when
/// more exist, the cursor checked against the journal (`invalid_cursor`),
/// conversation scope and the EX11 value.
#[test]
fn list_pages_cursor_and_scope() {
    let mut h = H::new(8);
    let mut jobs = Vec::new();
    for n in 1..=5 {
        let job = h.running(n, if n % 2 == 0 { C2 } else { C1 }, P1);
        h.finish(job, 1);
        jobs.push(job);
    }
    h.env.registry.execution_off.insert(P1);
    let mut query = list(1, Some(P1), None);
    query.limit = Some(2);
    let first = listing(h.send(1, Body::JobList(query.clone())));
    assert_eq!(first.entries.len(), 2);
    assert_eq!(first.entries[0].job_id.as_deref(), Some(&jobs[4][..]));
    assert_eq!(first.execution_enabled, Some(false));
    assert_eq!(first.live_in_scope, Some(0));
    let next = first.next_before.expect("more");
    assert_eq!(Some(next), first.entries[1].sequence);
    query.before = Some(next);
    let second = listing(h.send(1, Body::JobList(query.clone())));
    assert_eq!(second.entries[0].job_id.as_deref(), Some(&jobs[2][..]));
    query.before = Some(second.next_before.expect("more"));
    let last = listing(h.send(1, Body::JobList(query.clone())));
    assert_eq!(last.entries.len(), 1);
    assert_eq!(last.next_before, None);
    query.before = Some(next + 1);
    let reply = listing(h.send(1, Body::JobList(query)));
    assert_eq!(reply.error.as_deref(), Some(wire::error::INVALID_CURSOR));
    let reply = listing(h.send(1, Body::JobList(list(3, Some(P1), Some(C2)))));
    let ids: Vec<_> = reply.entries.iter().map(|e| e.job_id.clone()).collect();
    assert_eq!(ids, [Some(jobs[3].to_vec()), Some(jobs[1].to_vec())]);
    let entry = &reply.entries[0];
    assert_eq!(entry.command.as_deref(), Some("cargo build --workspace"));
    assert_eq!(entry.outcome, Some(1));
    assert_eq!(entry.stdout_lines, Some(2));
}

/// J4 failure handling: with the journal unavailable, the live list works
/// from memory; inspect and settled reads are `unavailable`.
#[test]
fn live_list_works_without_the_journal() {
    let mut h = H::new(8);
    let live = h.running(1, C1, P1);
    let done = h.running(2, C1, P1);
    h.finish(done, 1);
    h.env.journal.unavailable = true;
    let mut query = list(1, Some(P1), None);
    query.live_only = Some(true);
    let reply = listing(h.send(1, Body::JobList(query)));
    assert_eq!(reply.entries.len(), 1);
    assert_eq!(reply.entries[0].job_id.as_deref(), Some(&live[..]));
    assert_eq!(reply.entries[0].command.as_deref(), Some(""));
    let reply = listing(h.send(1, Body::JobList(list(1, Some(P1), None))));
    assert_eq!(reply.error.as_deref(), Some(wire::error::UNAVAILABLE));
    let reply = detail(h.send(1, inspect(live, None)));
    assert_eq!(reply.error.as_deref(), Some(wire::error::UNAVAILABLE));
    let reply = page(h.send(1, read(done, 0)));
    assert_eq!(reply.error.as_deref(), Some(wire::error::UNAVAILABLE));
    // A live job's pages still read.
    h.output(live, PageState::Live, b"ok");
    h.stored(live, [2, 0]);
    let reply = page(h.send(1, read(live, 0)));
    assert_eq!(reply.pages[0].text.as_deref(), Some("ok"));
}

/// Inspect: the whole command, the absolute cwd, the entry; a queued job
/// shows its block reason and text.
#[test]
fn inspect_command_cwd_and_queued_reason() {
    let mut h = H::new(8);
    let long = format!("{}\nsecond line", "x".repeat(300));
    let job = h.submit(model(1), C1, P1, &long);
    let reply = detail(h.send(1, inspect(job, None)));
    assert_eq!(reply.command.as_deref(), Some(long.as_str()));
    assert_eq!(reply.cwd.as_deref(), Some("/private/tmp/project/src"));
    let entry = reply.entry.expect("entry");
    let excerpt = entry.command.expect("excerpt");
    assert!(excerpt.len() <= 192 && excerpt.ends_with('…'), "{excerpt}");
    assert_eq!(entry.state, Some(pb::JobState::Starting as i32));
    // Fill the agent's slots: the next start is queued with its reason.
    let limits = JobLimits::default();
    let mut queued = None;
    for n in 2..=(limits.live_agent as u8 + 1) {
        queued = Some(h.submit(model(n), C1, P1, "sleep 1"));
    }
    let reply = detail(h.send(1, inspect(queued.unwrap(), None)));
    let entry = reply.entry.expect("entry");
    assert_eq!(entry.state, Some(pb::JobState::Queued as i32));
    assert_eq!(entry.block_reason, Some(5), "agent_slots");
    assert!(
        entry
            .reason_text
            .as_deref()
            .is_some_and(|t| t.starts_with("waiting:")),
        "{entry:?}"
    );
}

/// Reads: an explicit offset, `at_end` (the last page up to the stored
/// bytes), a stale cursor, a job with no process, and the payload store
/// unavailable.
#[test]
fn read_offsets_at_end_and_states() {
    let mut h = H::new(8);
    let job = h.running(1, C1, P1);
    h.output(job, PageState::Complete, b"0123456789");
    h.finish(job, 10);
    let reply = page(h.send(1, read(job, 4)));
    assert_eq!(reply.pages[0].text.as_deref(), Some("456789"));
    assert_eq!(reply.pages[0].next_offset, Some(10));
    let at_end = Body::JobRead(pb::JobRead {
        job_id: Some(job.to_vec()),
        stream: Some(pb::JobStream::Stdout as i32),
        limit: Some(3),
        at_end: Some(true),
        ..Default::default()
    });
    let reply = page(h.send(1, at_end));
    assert_eq!(reply.pages[0].start, Some(7));
    assert_eq!(reply.pages[0].text.as_deref(), Some("789"));
    let reply = page(h.send(1, read(job, 50)));
    assert_eq!(reply.pages[0].start, Some(10));
    assert_eq!(reply.pages[0].text.as_deref(), Some(""));
    // A queued job has no payload: an empty live page without a read.
    let reads = h.env.pages.submitted.borrow().len();
    let queued = h.submit(model(9), C2, P1, "true");
    let reply = page(h.send(1, read(queued, 0)));
    assert_eq!(reply.pages[0].state, Some(pb::JobPageState::Live as i32));
    assert_eq!(h.env.pages.submitted.borrow().len(), reads);
    h.env.pages_ready = false;
    let reply = page(h.send(1, read(job, 0)));
    assert_eq!(reply.error.as_deref(), Some(wire::error::UNAVAILABLE));
    let reply = page(h.send(1, read([0x5a; 16], 0)));
    assert_eq!(reply.error.as_deref(), Some(wire::error::NOT_FOUND));
}

#[test]
fn excerpt_and_unnumbered_placeholders() {
    assert_eq!(command_excerpt("ls -la"), "ls -la");
    assert_eq!(command_excerpt("a\nb"), "a…");
    assert_eq!(command_excerpt("tab\there"), "tab here");
    let long = "é".repeat(200);
    let excerpt = command_excerpt(&long);
    assert!(excerpt.len() <= 192 && excerpt.ends_with('…'));
    assert_eq!(
        unnumbered("$ASURA_SECRET_12 ${ASURA_SECRET_3} ASURA_SECRET_4 $ASURA_SECRET_x"),
        "$ASURA_SECRET_0 ${ASURA_SECRET_0} ASURA_SECRET_4 $ASURA_SECRET_x"
    );
    assert_eq!(priority(&cancel(1, [1; 16], P1, false)), Some(0));
    assert_eq!(priority(&observe([1; 16], 0)), Some(2));
    assert_eq!(priority(&read([1; 16], 0)), Some(1));
}
