//! Owner unit tests of the person command path (person-command-mode.md,
//! PC-U26, PC-U27, PC-U54, PC-U56, PC-U57, and the P3-E form rules). The
//! real conversation owner, journal writer, payload store and job
//! controller run in a private scratch home; the controller drives a
//! scripted slot, so no process starts. No case makes a Keychain call: the
//! dispatch read outcomes are given to the owner's item step directly.
use super::*;
use crate::job_controller::{CaptureOutcome, SlotEvent, SlotFactory, SlotPort, SlotStartError};
use crate::test_support::JobHarness;
use asura_platform::exec_cmd::{Exit, StopKind};
use asura_storage::authority::conversation::{
    CaptureReason, CaptureState, CaptureStream, JobCleanup, JobStream,
};
use std::cell::RefCell;
use std::os::unix::fs::DirBuilderExt;
use std::rc::Rc;
use std::time::Duration;

// ----- Scripted slot -----

#[derive(Default)]
struct Log {
    /// Started jobs: profile and whether a person setup came with them.
    started: Vec<(Id, Profile, bool)>,
    scripts: BTreeMap<Id, VecDeque<SlotEvent>>,
    idle: BTreeSet<Id>,
}
type Slots = Rc<RefCell<Log>>;
struct Slot {
    log: Slots,
    job: Option<Id>,
}
impl Drop for H {
    fn drop(&mut self) {
        if let Some(mut h) = self.h.take() {
            h.owner().close_secret_store();
            h.owner().close_writer_for_test();
        }
        if let Some(target) = self.keychain.take() {
            let _ = asura_platform::keychain::scratch::delete(&target);
        }
    }
}
impl SlotPort for Slot {
    fn start(&mut self, start: crate::job_controller::SlotStart) -> Result<(), SlotStartError> {
        self.job = Some(start.job);
        self.log
            .borrow_mut()
            .started
            .push((start.job, start.profile, start.person.is_some()));
        Ok(())
    }
    fn stop(&mut self, _kind: StopKind) {}
    fn discard_capture(&mut self) {}
    fn close_capture(&mut self) {}
    fn interests(&self, _out: &mut Vec<asura_platform::PollInterest>) {}
    fn next_deadline(&self, _now: Instant) -> Option<Instant> {
        None
    }
    fn poll(&mut self, _now: Instant, out: &mut Vec<SlotEvent>) {
        let Some(job) = self.job else {
            return;
        };
        if let Some(script) = self.log.borrow_mut().scripts.get_mut(&job) {
            while out.len() < crate::job_controller::SLOT_EVENTS {
                let Some(event) = script.pop_front() else {
                    break;
                };
                out.push(event);
            }
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
fn empty_stream() -> JobStream {
    JobStream {
        capture: CaptureStream {
            digest: Some(Sha256::digest(b"").into()),
            eof: true,
            ..Default::default()
        },
        lines: 0,
    }
}
fn complete() -> SlotEvent {
    SlotEvent::CaptureSettled(Box::new(CaptureOutcome {
        state: CaptureState::Complete,
        reason: CaptureReason::None,
        stdout: empty_stream(),
        stderr: empty_stream(),
        distinct_numbers: 0,
        numbers: Vec::new(),
        pause_count: 0,
        first_pause_ms: 0,
        paused_ms: 0,
        final_reservation: 1024 * 1024,
        capture: None,
    }))
}
use sha2::Digest as _;
use sha2::Sha256;

// ----- Harness -----

/// A `!` (or `!!`) submit body with a fresh request ID.
fn submit_body(project: Id, conversation: Option<(Id, u64)>, command: &str, outside: bool) -> Body {
    Body::ConversationQueueCommandSubmit(pb::ConversationQueueCommandSubmit {
        request_id: Some(asura_platform::random_id().to_vec()),
        project_id: Some(project.to_vec()),
        conversation_id: conversation.map(|(c, _)| c.to_vec()),
        expected_generation: Some(conversation.map_or(0, |(_, g)| g)),
        new_conversation: Some(conversation.is_none()),
        command: Some(command.into()),
        cwd: Some(".".into()),
        search_path: (!outside).then(|| "/usr/bin:/bin".into()),
        environment: Vec::new(),
        outside_sandbox: outside.then_some(true),
        launch_environment: Vec::new(),
    })
}

/// The scratch home, removed last.
struct Dir(std::path::PathBuf);
impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
struct H {
    h: Option<JobHarness>,
    log: Slots,
    /// The harness runs the real audit worker.
    audit: bool,
    /// The scratch keychain of a `!!` case; deleted after the store's
    /// worker stopped (testing rule 2).
    keychain: Option<asura_platform::keychain::Target>,
    path: Dir,
    runtime: asura_platform::RuntimeDirectory,
    counter: u64,
    _serial: std::sync::MutexGuard<'static, ()>,
}
impl H {
    fn new() -> Self {
        Self::start(false, false)
    }
    /// A home with a scratch keychain for the secret store.
    fn with_keychain() -> Self {
        Self::start(true, false)
    }
    /// [`Self::with_keychain`] with the real audit worker.
    fn audited() -> Self {
        Self::start(true, true)
    }
    fn start(keychain: bool, audit: bool) -> Self {
        let serial = crate::TEST_WRITER.lock().unwrap_or_else(|e| e.into_inner());
        let path = std::path::PathBuf::from(format!(
            "/private/tmp/asura-pcu-{:x}",
            u128::from_ne_bytes(asura_platform::random_id())
        ));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .unwrap();
        let runtime = asura_platform::RuntimeDirectory::scratch(&path, true).unwrap();
        let keychain = keychain.then(|| {
            let file = path.join("scratch.keychain");
            let password: String = asura_platform::random_id()
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect();
            let target = asura_platform::keychain::scratch::create(&file, &password)
                .expect("scratch keychain");
            std::fs::write(
                path.join("secret-store-keychain"),
                format!("{}\n{password}\n", file.display()),
            )
            .unwrap();
            target
        });
        if audit {
            // The audit log is a start-time setting, written first.
            asura_storage::config::execute(
                runtime.clone(),
                "audit.enabled",
                Some("true"),
                &std::sync::atomic::AtomicBool::new(false),
            )
            .expect("audit.enabled");
        }
        let log = Slots::default();
        let h = JobHarness::start_scripted(&runtime, factory(log.clone()), audit).expect("harness");
        Self {
            h: Some(h),
            log,
            audit,
            keychain,
            path: Dir(path),
            runtime,
            counter: 0,
            _serial: serial,
        }
    }
    fn j(&mut self) -> &mut JobHarness {
        self.h.as_mut().expect("harness")
    }
    /// The same home again, as after a restart.
    fn restart(&mut self) {
        if let Some(mut old) = self.h.take() {
            old.owner().close_secret_store();
            old.owner().close_writer_for_test();
        }
        let log = Slots::default();
        self.h = Some(
            JobHarness::start_scripted(&self.runtime, factory(log.clone()), self.audit)
                .expect("harness"),
        );
        self.log = log;
        self.counter = 0;
    }
    fn project(&mut self) -> Id {
        let location = self.path.0.join("project");
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&location)
            .unwrap();
        self.j()
            .register(location.to_str().unwrap())
            .expect("project")
    }
    fn send(&mut self, body: Body) -> pb::Envelope {
        self.counter += 1;
        let request = pb::Envelope {
            service_epoch: Some(vec![1; 16]),
            attachment_id: Some(vec![2; 16]),
            request_counter: Some(self.counter),
            body: Some(body),
        };
        asura_control::validate_semantics(&request, asura_control::Direction::ClientToServer)
            .expect("valid request");
        let counter = self.counter;
        self.j().request(request);
        let reply = self.j().reply(counter).expect("reply");
        asura_control::validate_semantics(&reply, asura_control::Direction::ServerToClient)
            .unwrap_or_else(|e| panic!("invalid reply {reply:?}: {e:?}"));
        reply
    }
    /// A `!` (or `!!`) submit; returns the accepted input.
    fn command(
        &mut self,
        project: Id,
        conversation: Option<(Id, u64)>,
        command: &str,
        outside: bool,
    ) -> Id {
        let body = submit_body(project, conversation, command, outside);
        self.accepted(body)
    }
    /// Sends a submit that must be accepted; returns its input.
    fn accepted(&mut self, body: Body) -> Id {
        let reply = self.send(body);
        match reply.body {
            Some(Body::ConversationQueueReply(value)) => value
                .accepted_input_id
                .clone()
                .and_then(|v| v.try_into().ok())
                .unwrap_or_else(|| panic!("refused: {value:?}")),
            other => panic!("not accepted: {other:?}"),
        }
    }
    fn message(&mut self, project: Id, conversation: Id, generation: u64) -> Id {
        let reply = self.send(Body::ConversationQueueSubmit(pb::ConversationQueueSubmit {
            request_id: Some(asura_platform::random_id().to_vec()),
            project_id: Some(project.to_vec()),
            conversation_id: Some(conversation.to_vec()),
            expected_generation: Some(generation),
            new_conversation: Some(false),
            prompt: Some("after the command".into()),
        }));
        match reply.body {
            Some(Body::ConversationQueueReply(value)) => {
                value.accepted_input_id.unwrap().try_into().unwrap()
            }
            other => panic!("message not queued: {other:?}"),
        }
    }
    fn entries(&mut self, project: Id) -> Vec<pb::ConversationQueueEntry> {
        match self
            .send(Body::ConversationQueueList(pb::ConversationQueueList {
                project_id: Some(project.to_vec()),
                input_id: None,
                after_revision: None,
            }))
            .body
        {
            Some(Body::ConversationQueueReply(value)) => value.entries,
            other => panic!("no list: {other:?}"),
        }
    }
    fn entry(&mut self, project: Id, input: Id) -> pb::ConversationQueueEntry {
        self.entries(project)
            .into_iter()
            .find(|e| e.input_id.as_deref() == Some(&input[..]))
            .expect("entry")
    }
    fn conversation_of(&mut self, input: Id) -> Id {
        self.j().owner().replay.as_ref().unwrap().inputs[&input].conversation
    }
    fn started(&self, job: Id) -> Option<(Profile, bool)> {
        self.log
            .borrow()
            .started
            .iter()
            .find(|(j, ..)| *j == job)
            .map(|(_, p, person)| (*p, *person))
    }
    fn script(&mut self, job: Id, events: impl IntoIterator<Item = SlotEvent>) {
        self.log
            .borrow_mut()
            .scripts
            .entry(job)
            .or_default()
            .extend(events);
    }
    fn wait(&mut self, what: &str, mut done: impl FnMut(&mut Self) -> bool) {
        let end = Instant::now() + Duration::from_secs(30);
        while !done(self) {
            assert!(Instant::now() < end, "timed out waiting for {what}");
            self.j().step();
        }
    }
    fn steps(&mut self, n: usize) {
        for _ in 0..n {
            self.j().step();
        }
    }
    /// The job's run: spawned and running.
    fn run(&mut self, input: Id) -> Id {
        let job = Origin::Person { input }.job_id();
        self.wait("slot start", |h| h.started(job).is_some());
        self.script(job, [SlotEvent::Spawned]);
        self.wait("kind 26", |h| {
            h.j()
                .owner()
                .replay
                .as_ref()
                .is_some_and(|r| r.jobs.contains_key(&job))
        });
        job
    }
    /// Ends a running job: exit 0, capture complete, slot idle.
    fn settle(&mut self, job: Id, output_held: bool, escapees: u32) {
        let exit = Exit::Exited(0);
        if !output_held {
            self.script(
                job,
                [
                    SlotEvent::LeaderExited(exit),
                    SlotEvent::ProcessSettled {
                        exit,
                        cleanup: JobCleanup::Confirmed,
                        escapees,
                        elapsed_ms: 5,
                        output_held: false,
                    },
                ],
            );
        }
        self.script(job, [complete(), SlotEvent::ScratchCleaned]);
        self.log.borrow_mut().idle.insert(job);
    }
}

// ----- Cases -----

/// PC-U26: a lane with a running person job, then a queued message: the
/// message waits until the job settles, then dispatches.
#[test]
fn pc_u26_message_waits_for_the_person_job() {
    let mut h = H::new();
    let project = h.project();
    let input = h.command(project, None, "make check", false);
    let job = h.run(input);
    assert_eq!(h.started(job), Some((Profile::Person1, true)));
    let conversation = h.conversation_of(input);
    let message = h.message(project, conversation, 0);
    h.steps(20);
    let entry = h.entry(project, message);
    assert_eq!(entry.state, Some(1), "the message waits: {entry:?}");
    let owner = h.j().owner();
    assert!(
        owner
            .replay
            .as_ref()
            .unwrap()
            .ready_input_with(owner.persons.released())
            .is_none(),
        "no message is ready while the person job runs"
    );
    h.settle(job, false, 0);
    h.wait("the message leaves Queued", |h| {
        h.entry(project, message).state != Some(1)
    });
    assert_eq!(h.entry(project, input).state, Some(4), "Complete");
}

/// PC-U27: a conversation that is idle but holds a live model job, while
/// another conversation's turn is active: its command at the lane head
/// dispatches; the model job and the other turn continue.
#[test]
fn pc_u27_command_dispatches_beside_a_model_job_and_another_turn() {
    let mut h = H::new();
    let project = h.project();
    let mut first = h.j().begin_turn(project).expect("turn");
    let model = h.j().launch(&mut first, "sleep 60").expect("model job");
    h.script(model, [SlotEvent::Spawned]);
    h.j().end_turn(&first).expect("end turn");
    let other = h.j().begin_turn(project).expect("other turn");
    let input = h.command(
        project,
        Some((first.conversation, first.generation)),
        "git status",
        false,
    );
    let job = h.run(input);
    assert_eq!(h.started(job), Some((Profile::Person1, true)));
    let status = h.j().view(model).expect("model job");
    assert_eq!(status.state, "Running", "the model job continues");
    let replay = h.j().owner().replay.clone().unwrap();
    assert_eq!(
        replay.active_operation,
        Some(other.operation),
        "the other turn continues"
    );
}

/// PC-U54: the overlay: `ShellExited` of a `!!` job marks its entry
/// Complete while the job drains, and a message queued behind it
/// dispatches. After a restart without kind 28, recovery writes
/// `interrupted` and the entry is Failed.
#[test]
fn pc_u54_shell_exited_releases_the_lane_until_restart() {
    let mut h = H::with_keychain();
    let project = h.project();
    let input = h.command(project, None, "brew upgrade", true);
    let job = h.run(input);
    assert_eq!(h.started(job), Some((Profile::PersonOutside1, true)));
    let conversation = h.conversation_of(input);
    let message = h.message(project, conversation, 0);
    h.steps(10);
    assert_eq!(h.entry(project, message).state, Some(1));
    let exit = Exit::Exited(0);
    h.script(
        job,
        [
            SlotEvent::LeaderExited(exit),
            SlotEvent::ProcessSettled {
                exit,
                cleanup: JobCleanup::Confirmed,
                escapees: 1,
                elapsed_ms: 5,
                output_held: true,
            },
        ],
    );
    h.wait("released", |h| {
        h.j().owner().persons.released().contains(&input)
    });
    assert_eq!(h.entry(project, input).state, Some(4), "overlay Complete");
    assert_eq!(
        h.j().view(job).unwrap().state,
        "Draining",
        "the job stays live"
    );
    // The owner's lane is released: it dispatches the message, which waited
    // until now (state 1 above); see `pc_u54_message_turn_after_release`.
    h.wait("the message dispatch", |h| {
        h.entry(project, message).state != Some(1)
    });
    // Restart without kind 28: recovery writes `interrupted`; Failed.
    h.restart();
    h.wait("recovery", |h| {
        h.j()
            .owner()
            .replay
            .as_ref()
            .is_some_and(|r| !r.jobs.contains_key(&job))
    });
    assert_eq!(
        h.entry(project, input).state,
        Some(5),
        "Failed after recovery"
    );
}

/// PC-U56: waiting reasons at the lane head: a model turn of its
/// conversation; the controller's block reason with its live numbers; a
/// Keychain wait (locked, unavailable, busy); an earlier held input. The
/// locked recheck is paced 1, 1, 2, 3, 5 ... s up to 60 s on a fixture
/// clock, and the person's next action (Send now) rechecks at once.
#[test]
fn pc_u56_waiting_reasons_and_paced_recheck() {
    let mut h = H::new();
    let project = h.project();
    // A model turn running in the conversation.
    let turn = h.j().begin_turn(project).expect("turn");
    let input = h.command(
        project,
        Some((turn.conversation, turn.generation)),
        "ls",
        false,
    );
    h.steps(5);
    let entry = h.entry(project, input);
    assert_eq!(
        (entry.waiting_reason, entry.waiting_text.as_deref()),
        (
            Some(pb::QueueWaitReason::ModelTurn as i32),
            Some("waiting: model turn running")
        )
    );
    // A Keychain wait at the lane head, by each kind.
    let record = Box::new(PersonCommandQueued {
        request: [0xaa; 16],
        digest: [0; 32],
        input,
        project,
        conversation: turn.conversation,
        expected_generation: turn.generation,
        new_conversation: false,
        outside_sandbox: true,
        command: "ls".into(),
        display_command: String::new(),
        cwd: ".".into(),
        search_path: String::new(),
        environment: Vec::new(),
        order_revision: 1,
        number_kinds: Vec::new(),
        unnumbered: 0,
        command_cut: false,
        whole_command: false,
        rule_set_revision: 1,
        rule_set_digest: [1; 32],
        fallback: None,
    });
    let t0 = Instant::now();
    for (failure, wait, code, text) in [
        (
            StoreFailure::Locked,
            KeychainWait::Locked,
            pb::QueueWaitReason::KeychainLocked,
            "waiting: Keychain locked",
        ),
        (
            StoreFailure::Unavailable {
                status: -25291,
                reason: None,
            },
            KeychainWait::Unavailable,
            pb::QueueWaitReason::KeychainUnavailable,
            "waiting: Keychain unavailable",
        ),
    ] {
        let owner = h.j().owner();
        let dispatch = owner.person_item_read(record.clone(), GetReply::Failed(failure), None, t0);
        let Dispatch::Waiting(_, got, pacing) = &dispatch else {
            panic!("not waiting");
        };
        assert_eq!(*got, wait);
        assert_eq!(
            pacing.at(),
            t0 + Duration::from_secs(1),
            "first recheck 1 s"
        );
        owner.persons.dispatches.insert(input, dispatch);
        let reasons = owner.waiting_reasons();
        assert_eq!(reasons.get(&input), Some(&(code as i32, text.to_owned())));
    }
    h.j().owner().persons.dispatches.insert(
        input,
        Dispatch::Waiting(record.clone(), KeychainWait::Busy, Pacing::start(t0)),
    );
    assert_eq!(
        h.j().owner().waiting_reasons().get(&input),
        Some(&(
            pb::QueueWaitReason::KeychainBusy as i32,
            "waiting: Keychain requests queued".to_owned()
        ))
    );
    // The paced recheck on a fixture clock: each miss takes the next delay.
    let mut now = t0;
    let mut pacing = paced(None, now);
    let mut delays = Vec::new();
    for _ in 0..12 {
        delays.push((pacing.at() - now).as_secs());
        now = pacing.at();
        pacing = paced(Some(pacing), now);
    }
    assert_eq!(delays, [1, 1, 2, 3, 5, 8, 13, 21, 34, 55, 60, 60]);
    // Send now (the person's next action) rechecks at once.
    let owner = h.j().owner();
    owner.persons.recheck_now(&BTreeSet::from([input]));
    assert!(matches!(
        owner.persons.dispatches.get(&input),
        Some(Dispatch::Ready(_))
    ));
    owner.persons.dispatches.remove(&input);
    // An earlier held input: the next command waits behind it.
    let second = h.command(
        project,
        Some((turn.conversation, turn.generation)),
        "pwd",
        false,
    );
    let _ = h.j().owner().command_hold(input, CANCELLED);
    h.wait("held", |h| h.entry(project, input).state == Some(2));
    h.j().end_turn(&turn).expect("end turn");
    h.steps(5);
    let entry = h.entry(project, second);
    assert_eq!(
        (entry.waiting_reason, entry.waiting_text.as_deref()),
        (
            Some(pb::QueueWaitReason::HeldHead as i32),
            Some("waiting: an earlier input is held")
        )
    );
}

/// PC-U56: a controller block reason reaches the lane head with its live
/// numbers: two live model jobs fill the agent's slots.
#[test]
fn pc_u56_controller_block_reason() {
    let mut h = H::new();
    let project = h.project();
    let mut turn = h.j().begin_turn(project).expect("turn");
    for _ in 0..2 {
        let model = h.j().launch(&mut turn, "sleep 60").expect("model job");
        h.script(model, [SlotEvent::Spawned]);
    }
    h.j().end_turn(&turn).expect("end turn");
    let input = h.command(
        project,
        Some((turn.conversation, turn.generation)),
        "ls",
        false,
    );
    h.wait("blocked", |h| {
        h.entry(project, input).waiting_reason == Some(pb::QueueWaitReason::AgentSlots as i32)
    });
    let entry = h.entry(project, input);
    assert_eq!(
        entry.waiting_text.as_deref(),
        Some("waiting: 2 of 2 jobs running for this agent")
    );
    assert_eq!(entry.state, Some(1));
}

/// PC-U57: the dispatch read outcomes: `missing` holds with
/// `secrets_lost`; `mismatch` holds with `secrets_lost` and deletes the
/// item; `denied` holds with `secret_store_denied`; a changed rule-set
/// revision holds with `secret_rules_changed`. Nothing runs.
#[test]
fn pc_u57_dispatch_read_outcomes_hold() {
    let mut h = H::new();
    let project = h.project();
    let turn = h.j().begin_turn(project).expect("turn");
    let mut inputs = Vec::new();
    for _ in 0..4 {
        inputs.push(h.command(
            project,
            Some((turn.conversation, turn.generation)),
            "true",
            false,
        ));
    }
    let record = |input: Id| {
        Box::new(PersonCommandQueued {
            request: [0xbb; 16],
            digest: [0; 32],
            input,
            project,
            conversation: turn.conversation,
            expected_generation: turn.generation,
            new_conversation: false,
            outside_sandbox: true,
            command: "true".into(),
            display_command: String::new(),
            cwd: ".".into(),
            search_path: String::new(),
            environment: Vec::new(),
            order_revision: 1,
            number_kinds: Vec::new(),
            unnumbered: 0,
            command_cut: false,
            whole_command: false,
            rule_set_revision: 1,
            rule_set_digest: [1; 32],
            fallback: None,
        })
    };
    h.wait("rule set", |h| {
        matches!(h.j().owner().rules.ready(), Some(Ok(_)))
    });
    let now = Instant::now();
    let owner = h.j().owner();
    let missing = owner.person_item_read(record(inputs[0]), GetReply::Missing, None, now);
    let mismatch = owner.person_item_read(record(inputs[1]), GetReply::Mismatch, None, now);
    let denied = owner.person_item_read(
        record(inputs[2]),
        GetReply::Failed(StoreFailure::Denied),
        None,
        now,
    );
    let changed = owner.person_start(
        record(inputs[3]),
        Some(Secrets {
            launch_environment: Some(Vec::new()),
            ..Default::default()
        }),
        now,
    );
    for dispatch in [&missing, &mismatch, &denied, &changed] {
        assert!(matches!(dispatch, Dispatch::Holding));
    }
    assert_eq!(
        owner.persons.deleted,
        vec![inputs[1]],
        "the mismatched item is deleted"
    );
    for (input, dispatch) in inputs.iter().zip([missing, mismatch, denied, changed]) {
        owner.persons.dispatches.insert(*input, dispatch);
    }
    h.wait("held", |h| {
        inputs
            .clone()
            .iter()
            .all(|input| h.entry(project, *input).state == Some(2))
    });
    let reasons: Vec<Option<String>> = inputs
        .iter()
        .map(|input| h.entry(project, *input).hold_reason)
        .collect();
    assert_eq!(
        reasons,
        [
            Some(SECRETS_LOST.into()),
            Some(SECRETS_LOST.into()),
            Some(SECRET_STORE_DENIED.into()),
            Some(SECRET_RULES_CHANGED.into()),
        ]
    );
    assert!(h.log.borrow().started.is_empty(), "nothing runs");
}

/// The PC2 PATH sanitizer (PC-U19, service part): empty and relative
/// entries are dropped, order kept; NUL, oversize, more than 64 entries or
/// nothing left refuses.
#[test]
fn path_sanitizer() {
    assert_eq!(
        sanitize_path(Some("/usr/bin::bin:./x:/bin")).as_deref(),
        Some("/usr/bin:/bin")
    );
    assert_eq!(sanitize_path(Some("relative:")), None);
    assert_eq!(sanitize_path(Some("/usr/bin\0")), None);
    assert_eq!(sanitize_path(Some(&"/a".repeat(2049))), None);
    let many: Vec<String> = (0..65).map(|i| format!("/p{i}")).collect();
    assert_eq!(sanitize_path(Some(&many.join(":"))), None);
    assert!(sanitize_path(Some(&many[..64].join(":"))).is_some());
    assert_eq!(sanitize_path(None), None);
}

/// The audit names: every `QueueRefusal` maps to its member, and every
/// writer error to a named member (exhaustive matches); refusal replies
/// carry the typed refusal and the |OSStatus| only for
/// `secret_store_unavailable`.
#[test]
fn refusal_names_are_exhaustive() {
    for value in 1..=13 {
        let refusal = pb::QueueRefusal::try_from(value).unwrap();
        let named = queue_refusal(refusal).expect("named");
        assert_eq!(typed_refusal(named), Some(refusal));
    }
    assert_eq!(queue_refusal(pb::QueueRefusal::Unspecified), None);
    use writer::Error as E;
    for error in [
        E::Busy,
        E::Unavailable,
        E::NotInitialized,
        E::AlreadyInitialized,
        E::RepairRequired,
        E::OutcomeUnconfirmed,
        E::Cancelled,
        E::Conflict,
        E::Invalid,
        E::StaleProject,
        E::OverlapsAsuraState,
        E::ModelUnavailable,
        E::Limit,
        E::JournalQuota,
        E::JournalDiskSpace,
    ] {
        let refusal = error_refusal(error);
        assert!(typed_refusal(refusal).is_none(), "{error:?} is a token");
        if !matches!(error, E::AlreadyInitialized | E::ModelUnavailable) {
            assert_eq!(error_token(refusal), storage_error(error), "{error:?}");
        }
    }
}

/// The rewrite's stored form round-trips: the raw command is restored from
/// the stored rewrite and values, and re-planned to the same variables; a
/// value that does not belong to the record is a mismatch.
#[test]
fn stored_rewrite_replans() {
    let rules = asura_platform::secrets::detection().unwrap();
    let token = ["ghp_", &"aB3".repeat(12)].concat();
    let raw = format!("curl -H \"Authorization: Bearer {token}\" x && echo {token}");
    let Plan::Rewritten(rewrite) = secrets::rewrite_exec_cmd_call_with(rules, &raw, ".").unwrap()
    else {
        panic!("rewrite");
    };
    let planned = planned(Plan::Rewritten(rewrite));
    let record = PersonCommandQueued {
        request: [1; 16],
        digest: [0; 32],
        input: [2; 16],
        project: [3; 16],
        conversation: [4; 16],
        expected_generation: 0,
        new_conversation: true,
        outside_sandbox: false,
        command: planned.command.clone(),
        display_command: String::new(),
        cwd: ".".into(),
        search_path: "/usr/bin".into(),
        environment: Vec::new(),
        order_revision: 1,
        number_kinds: planned.number_kinds.clone(),
        unnumbered: planned.unnumbered,
        command_cut: false,
        whole_command: false,
        rule_set_revision: rules.revision(),
        rule_set_digest: rules.digest(),
        fallback: None,
    };
    assert!(!record.command.contains(&token));
    let restored = restore(&record.command, &planned.secrets.variables).unwrap();
    assert_eq!(restored.as_str(), raw);
    let variables = replan_rewrite(rules, &record, &planned.secrets.variables).unwrap();
    assert_eq!(variables.len(), 1);
    assert_eq!(variables[0].value(), token.as_bytes());
    let mut other = planned.secrets.variables.clone();
    other[0].value = Zeroizing::new(b"ghp_other".to_vec());
    assert!(replan_rewrite(rules, &record, &other).is_none());
}

/// The display-only copy is cut at 8,192 bytes, never inside a placeholder.
#[test]
fn display_cut_keeps_placeholders_whole() {
    let short = "echo '$ASURA_SECRET_1'";
    assert_eq!(cut_display(short), (short.to_owned(), false));
    let mut long = "x".repeat(8_185);
    long.push_str(" $ASURA_SECRET_12 tail");
    let (cut, was_cut) = cut_display(&long);
    assert!(was_cut);
    assert!(cut.len() <= 8_192);
    assert!(!cut.contains("$ASURA_SECRET_1"), "{}", &cut[8_180..]);
    assert!(cut.ends_with(' '));
}

/// PC-U54, message part (P3-E2): after `ShellExited` the message's
/// preparation is not refused: the journal writer's `PrepareInput` takes
/// an input that replay's `input_ready` accepts (the `!!` job's kind 26
/// is enough), so the message is not held `request_conflict`.
#[test]
fn pc_u54_message_turn_after_release() {
    let mut h = H::with_keychain();
    let project = h.project();
    let input = h.command(project, None, "brew upgrade", true);
    let job = h.run(input);
    let conversation = h.conversation_of(input);
    let message = h.message(project, conversation, 0);
    let exit = Exit::Exited(0);
    h.script(
        job,
        [
            SlotEvent::LeaderExited(exit),
            SlotEvent::ProcessSettled {
                exit,
                cleanup: JobCleanup::Confirmed,
                escapees: 1,
                elapsed_ms: 5,
                output_held: true,
            },
        ],
    );
    h.wait("the message leaves Queued", |h| {
        h.entry(project, message).state != Some(1)
    });
    let entry = h.entry(project, message);
    // The writer prepared the turn; the harness has no model package, so
    // the turn's admission holds it there, past the preparation.
    assert_eq!(
        entry.hold_reason.as_deref(),
        Some("model_package_unavailable"),
        "{entry:?}"
    );
}

/// Secret store ordering rule 6 (P3-E2): an exact `!!` retry whose kind 31
/// is older than the replay checkpoint, after its job settled and its item
/// was deleted, finds its request in the journal before any Keychain call:
/// it gets the original input, reads the (missing) item once, and writes
/// and deletes no item. The audit holds one `secret_store.put` for it.
#[test]
fn p3e2_exact_retry_after_the_checkpoint_writes_no_item() {
    let mut h = H::audited();
    let project = h.project();
    let body = submit_body(project, None, "brew upgrade", true);
    let input = h.accepted(body.clone());
    let job = h.run(input);
    h.settle(job, false, 0);
    h.wait("the item delete", |h| {
        h.j().owner().persons.deleted.contains(&input) && h.j().owner().persons.deletes.is_empty()
    });
    let Body::ConversationQueueCommandSubmit(submit) = &body else {
        unreachable!()
    };
    let request: Id = submit.request_id.clone().unwrap().try_into().unwrap();
    // Rolls: the request leaves the replay state for an index run.
    let turn = h.j().begin_turn(project).expect("turn");
    let text = "y".repeat(journal::MAX_TEXT);
    for k in 0..160u64 {
        h.j()
            .append(
                journal::Record::ResponseText(journal::ResponseText {
                    operation: turn.operation,
                    generation: turn.generation,
                    offset: k * journal::MAX_TEXT as u64,
                    text: text.clone(),
                }),
                None,
            )
            .expect("response text");
    }
    h.j().end_turn(&turn).expect("end turn");
    {
        let replay = h.j().owner().replay.clone().unwrap();
        assert!(
            replay.base > 0 && !replay.requests.contains_key(&request),
            "the kind 31 is older than the checkpoint"
        );
    }
    let deleted = h.j().owner().persons.deleted.len();
    let items = h.j().owner().persons.items.clone();
    // The exact retry.
    assert_eq!(h.accepted(body), input, "the original input");
    h.wait("the retry's read is audited", |h| {
        h.j().audit_events().iter().any(|e| {
            matches!(e, Event::SecretStoreGet { input: i, reply: StoreReply::Missing, .. } if *i == input)
        })
    });
    h.steps(10);
    let owner = h.j().owner();
    assert_eq!(owner.persons.deleted.len(), deleted, "no item to delete");
    assert_eq!(owner.persons.items, items, "no item stored");
    let events = h.j().audit_events();
    let puts: Vec<_> = events
        .iter()
        .filter(|e| matches!(e, Event::SecretStorePut { .. }))
        .collect();
    assert_eq!(puts.len(), 1, "{puts:?}");
    assert!(matches!(
        puts[0],
        Event::SecretStorePut { input: i, reply: StoreReply::Stored, status: None } if *i == input
    ));
    for (what, found) in [
        (
            "dispatch read",
            events.iter().any(|e| {
                matches!(e, Event::SecretStoreGet { input: i, reply: StoreReply::Read, .. } if *i == input)
            }),
        ),
        (
            "delete",
            events.iter().any(|e| {
                matches!(e, Event::SecretStoreDelete { input: i, reply: StoreReply::Deleted, .. } if *i == input)
            }),
        ),
        (
            "sweep",
            events.iter().any(|e| {
                matches!(e, Event::SecretStoreSweep { reply: StoreReply::Swept, .. })
            }),
        ),
    ] {
        assert!(found, "{what}: {events:?}");
    }
}

/// P3-E2 queue rule: a project holds one pending new conversation at a
/// time (PR1). While a person command waits (Held after a restart) in a
/// generation-0 conversation, a new-conversation submit is refused
/// `conversation_busy` before any Keychain call, for `!` and `!!`; an
/// exact retry of the waiting command still returns its input.
#[test]
fn p3e2_new_conversation_waits_for_the_pending_one() {
    let mut h = H::with_keychain();
    let project = h.project();
    let body = submit_body(project, None, "make check", false);
    let input = h.accepted(body.clone());
    // Restart before its kind 26: the command is Held at generation 0.
    h.restart();
    h.wait("replay", |h| {
        h.j()
            .owner()
            .replay
            .as_ref()
            .is_some_and(|r| r.input_status(input) == Some(InputStatus::Held))
    });
    let items = h.j().owner().persons.items.clone();
    for outside in [false, true] {
        let reply = h.send(submit_body(project, None, "ls", outside));
        match reply.body {
            Some(Body::Error(e)) => {
                assert_eq!(e.message.as_deref(), Some("conversation_busy"), "{e:?}")
            }
            other => panic!("not refused conversation_busy: {other:?}"),
        }
    }
    assert_eq!(h.j().owner().persons.items, items, "no item was put");
    assert_eq!(h.accepted(body), input, "the exact retry");
}
