//! Controller unit tests with the fixture slot and payload port (J2T-07,
//! J2T-08, J2T-24 and the unit parts of J2T-09 to J2T-15 and J2T-20).
use super::fixture::{self, AuditLog, FixturePayloads, Scratch, Slots, capture};
use super::*;
use asura_storage::authority::conversation::{FrameContext, Locator, Record, encode_frame};

const P1: Id = [0x11; 16];
const P2: Id = [0x22; 16];

fn limits(live_service: u32, live_agent: u32, queued_agent: u32) -> JobLimits {
    JobLimits {
        live_service,
        live_agent: live_agent.min(live_service),
        queued_agent,
        ..JobLimits::default()
    }
}
fn conversation(n: u8) -> AgentKey {
    AgentKey {
        conversation: [n; 16],
        agent: 0,
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
/// Every record the controller builds must encode under the journal codec
/// rules (the record-alone rules of kinds 26 to 28).
fn assert_encodes(record: &Record) {
    let context = FrameContext {
        installation_id: [1; 16],
        transition_id: [2; 16],
        sequence: 9,
        prior_digest: [0; 32],
        expected_revision: 0,
        owner_generation: 3,
    };
    encode_frame(&context, record).unwrap_or_else(|e| panic!("{record:?}: {e:?}"));
}

struct Harness {
    ctl: JobController,
    slots: Slots,
    payloads: FixturePayloads,
    audit: AuditLog,
    runtime: RuntimeDirectory,
    now: Instant,
    sequence: u64,
    journal: Vec<Record>,
    _scratch: Scratch,
}
impl Harness {
    fn new(limits: JobLimits) -> Self {
        let mut h = Self::unrecovered(limits);
        h.ctl.recover(Vec::new());
        h.poll();
        h
    }
    fn unrecovered(limits: JobLimits) -> Self {
        let scratch = Scratch::new();
        let runtime = scratch.runtime();
        let slots = Slots::default();
        let audit = AuditLog::default();
        let ctl = JobController::with_audit(
            limits,
            Some(Box::new(audit.clone())),
            runtime.clone(),
            fixture::factory(slots.clone()),
        );
        Self {
            ctl,
            slots,
            payloads: FixturePayloads::new(&runtime),
            audit,
            runtime,
            now: Instant::now(),
            sequence: 100,
            journal: Vec::new(),
            _scratch: scratch,
        }
    }
    fn request(&self, origin: Origin, agent: AgentKey, project: Id, digest: u8) -> StartRequest {
        StartRequest {
            origin,
            agent,
            scope: tools::ExecutionScope {
                project,
                path: "/private/tmp/project".into(),
                device: 1,
                inode: 2,
                boundary: self.runtime.state_boundary(),
            },
            profile: match origin {
                Origin::Model { .. } => Profile::Model1,
                Origin::Person { .. } => Profile::Person1,
            },
            command: "printf secret-command-text".into(),
            cwd: ".".into(),
            secrets: Vec::new(),
            secret_command: None,
            redaction: JobRedaction::new(asura_platform::secrets::detection().expect("rules"), &[]),
            duplicate_digest: [digest; 32],
            stored_command: "printf secret-command-text".into(),
            person: None,
        }
    }
    fn submit(&mut self, origin: Origin, agent: AgentKey, project: Id) -> Admission {
        let request = self.request(origin, agent, project, 1);
        self.ctl.submit(request, self.now)
    }
    /// Submits and commits a model start; returns its job ID.
    fn model(&mut self, n: u8, agent: AgentKey, project: Id) -> Id {
        let Admission::Accepted { job } = self.submit(model(n), agent, project) else {
            panic!("model start refused");
        };
        self.commit();
        job
    }
    fn poll(&mut self) {
        self.ctl.poll(self.now, &mut self.payloads);
    }
    /// Appends every record the owner would take, committing each, then
    /// polls. Returns the records appended by this call.
    fn commit(&mut self) -> Vec<Record> {
        let mut out = Vec::new();
        for _ in 0..1000 {
            self.poll();
            let Some((job, record)) = self.ctl.next_record() else {
                break;
            };
            assert_encodes(&record);
            self.sequence += 1;
            out.push(record.clone());
            self.journal.push(record);
            self.ctl.record_done(job, Ok(self.sequence));
        }
        self.poll();
        out
    }
    fn script(&mut self, job: Id, events: impl IntoIterator<Item = SlotEvent>) {
        self.slots
            .borrow_mut()
            .scripts
            .entry(job)
            .or_default()
            .extend(events);
        self.poll();
    }
    fn idle(&mut self, job: Id) {
        self.slots.borrow_mut().idle.push(job);
        self.poll();
    }
    fn state(&self, job: Id) -> Option<JobState> {
        self.ctl.status(job).map(|s| s.state)
    }
    fn reason(&self, job: Id) -> Option<BlockReason> {
        self.ctl.status(job).and_then(|s| s.reason)
    }
    fn text(&self, job: Id) -> Option<String> {
        self.ctl.status(job).and_then(|s| s.text)
    }
    fn started(&self) -> Vec<Id> {
        self.slots.borrow().started.clone()
    }
    fn stops(&self) -> Vec<(Id, StopKind)> {
        self.slots.borrow().stops.clone()
    }
    /// Runs a started job to a normal settlement and releases its slot.
    fn finish(&mut self, job: Id, code: u8) -> Vec<Record> {
        self.script(job, settle_events(Exit::Exited(code), 0));
        let records = self.commit();
        self.script(job, [SlotEvent::ScratchCleaned]);
        self.idle(job);
        records
    }
    fn audit_kinds(&self) -> Vec<&'static str> {
        self.audit
            .0
            .borrow()
            .iter()
            .map(|e| match e {
                Event::JobQueued { .. } => "job.queued",
                Event::JobStarted { .. } => "job.started",
                Event::JobCancelRequested { .. } => "job.cancel_requested",
                Event::JobSettled { .. } => "job.settled",
                _ => "other",
            })
            .collect()
    }
}
fn settle_events(exit: Exit, escapees: u32) -> Vec<SlotEvent> {
    vec![
        SlotEvent::Spawned,
        SlotEvent::LeaderExited(exit),
        SlotEvent::ProcessSettled {
            exit,
            cleanup: JobCleanup::Confirmed,
            escapees,
            elapsed_ms: 42,
            output_held: false,
        },
        SlotEvent::CaptureSettled(capture(CaptureState::Complete, 12, 3)),
    ]
}
fn settled(records: &[Record]) -> Vec<&JobSettled> {
    records
        .iter()
        .filter_map(|r| match r {
            Record::JobSettled(s) => Some(&**s),
            _ => None,
        })
        .collect()
}
fn cancels(records: &[Record]) -> Vec<&JobCancelRequested> {
    records
        .iter()
        .filter_map(|r| match r {
            Record::JobCancelRequested(c) => Some(c),
            _ => None,
        })
        .collect()
}
fn kinds(records: &[Record]) -> Vec<u16> {
    records.iter().map(Record::kind).collect()
}

// ----- J2T-07: lifecycle (TJ-S1) -----

/// J2T-07: every transition of a model job from `Queued` to `Recorded`
/// with fixture events; the requested state differs from the settled state;
/// one kind 27; the outcome is `cancelled` for a trigger before leader exit.
#[test]
fn j2t_07_lifecycle_cancel_while_running() {
    let mut h = Harness::new(limits(4, 2, 8));
    let Admission::Accepted { job } = h.submit(model(1), conversation(1), P1) else {
        panic!("accepted");
    };
    assert_eq!(h.state(job), Some(JobState::Queued));
    h.poll();
    assert!(h.started().is_empty(), "no start before kind 26 commits");
    let records = h.commit();
    assert_eq!(kinds(&records), [26]);
    assert_eq!(h.started(), [job]);
    assert_eq!(h.state(job), Some(JobState::Starting));
    h.script(job, [SlotEvent::Spawned]);
    assert_eq!(h.state(job), Some(JobState::Running));
    // The request: `Stopping` with `T` written; nothing is settled yet.
    assert_eq!(
        h.ctl
            .cancel(job, Requester::Client, JobReason::ClientCancel, false),
        CancelReply::Accepted(JobState::Stopping)
    );
    assert_eq!(h.stops(), [(job, StopKind::Term)]);
    assert_eq!(h.ctl.status(job).unwrap().outcome, None);
    // A second request writes no second kind 27 and no second `T`.
    assert_eq!(
        h.ctl
            .cancel(job, Requester::Model, JobReason::ModelCancel, false),
        CancelReply::Accepted(JobState::Stopping)
    );
    h.script(job, [SlotEvent::LeaderExited(Exit::Signalled(15))]);
    assert_eq!(h.state(job), Some(JobState::Draining));
    h.script(
        job,
        [SlotEvent::ProcessSettled {
            exit: Exit::Signalled(15),
            cleanup: JobCleanup::Confirmed,
            escapees: 0,
            elapsed_ms: 7,
            output_held: false,
        }],
    );
    assert_eq!(h.state(job), Some(JobState::Finalizing));
    h.script(
        job,
        [SlotEvent::CaptureSettled(capture(
            CaptureState::Incomplete,
            5,
            1,
        ))],
    );
    assert_eq!(h.state(job), Some(JobState::Recording));
    let records = h.commit();
    assert_eq!(kinds(&records), [27, 28]);
    assert_eq!(cancels(&records)[0].reason, JobReason::ClientCancel);
    let s = settled(&records)[0];
    assert_eq!(
        (s.outcome, s.reason, s.exit_kind, s.exit_value, s.cleanup),
        (
            Outcome::Cancelled,
            JobReason::ClientCancel,
            2,
            15,
            JobCleanup::Confirmed
        )
    );
    assert_eq!(s.stdout.lines, 1);
    assert_eq!(h.state(job), Some(JobState::Recorded));
    assert_eq!(h.payloads.committed.len(), 1, "install after kind 28 (R2)");
    assert_eq!(h.payloads.committed[0].0, job);
    // The slot stays held until scratch cleanup ends and the slot is idle.
    h.script(job, [SlotEvent::ScratchCleaned]);
    assert!(h.slots.borrow().dropped.is_empty());
    h.idle(job);
    assert_eq!(h.slots.borrow().dropped, [job]);
    assert_eq!(
        h.ctl
            .cancel(job, Requester::Client, JobReason::ClientCancel, false),
        CancelReply::AlreadySettled(Outcome::Cancelled)
    );
    assert!(h.ctl.settled());
}

/// J2T-07: a normal exit records outcome `exited`, reason 0, the stream
/// facts with their line counts, and cleanup `confirmed`.
#[test]
fn j2t_07_normal_exit() {
    let mut h = Harness::new(limits(4, 2, 8));
    let job = h.model(1, conversation(1), P1);
    let records = h.finish(job, 3);
    let s = settled(&records)[0];
    assert_eq!(
        (s.outcome, s.reason, s.exit_kind, s.exit_value),
        (Outcome::Exited, JobReason::None, 1, 3)
    );
    assert_eq!(s.capture_state, JobCaptureState::Complete);
    assert_eq!((s.stdout.capture.stored_bytes, s.stdout.lines), (12, 3));
    assert_eq!(s.elapsed_ms, 42);
    assert_eq!(h.ctl.status(job).unwrap().state, JobState::Recorded);
}

/// J2T-07: cancel of a queued job removes its entry and records kinds 27
/// and 28 `not_started` with the cancel reason; it never reaches a slot.
#[test]
fn j2t_07_cancel_of_queued_job_is_not_started() {
    let mut h = Harness::new(limits(1, 1, 8));
    let running = h.model(1, conversation(1), P1);
    let queued = h.model(2, conversation(1), P1);
    assert_eq!(h.reason(queued), Some(BlockReason::AgentSlots));
    assert_eq!(
        h.ctl
            .cancel(queued, Requester::Model, JobReason::TurnEnded, false),
        CancelReply::Accepted(JobState::Recording)
    );
    let records = h.commit();
    assert_eq!(kinds(&records), [27, 28]);
    let s = settled(&records)[0];
    assert_eq!(
        (s.outcome, s.reason, s.capture_state),
        (
            Outcome::NotStarted,
            JobReason::TurnEnded,
            JobCaptureState::NotStarted
        )
    );
    assert_eq!(h.started(), [running]);
    assert!(h.payloads.released.is_empty(), "no reservation was taken");
}

/// J2T-07: a cancel before spawn sets the slot's cancel; setup then fails
/// (`not_started` with the cancel reason), or the late returned job spawns
/// and is stopped at once (`cancelled`).
#[test]
fn j2t_07_cancel_before_spawn() {
    let mut h = Harness::new(limits(4, 2, 8));
    let job = h.model(1, conversation(1), P1);
    assert_eq!(
        h.ctl
            .cancel(job, Requester::Client, JobReason::ClientCancel, false),
        CancelReply::Accepted(JobState::Starting)
    );
    assert_eq!(h.stops(), [(job, StopKind::Term)]);
    h.script(
        job,
        [
            SlotEvent::SetupFailed(JobReason::SetupFailed),
            SlotEvent::CaptureSettled(capture(CaptureState::NotStarted, 0, 0)),
        ],
    );
    let records = h.commit();
    assert_eq!(kinds(&records), [27, 28]);
    let s = settled(&records)[0];
    assert_eq!(
        (s.outcome, s.reason),
        (Outcome::NotStarted, JobReason::ClientCancel)
    );

    let late = h.model(2, conversation(1), P1);
    h.ctl
        .cancel(late, Requester::Client, JobReason::ClientCancel, false);
    h.script(late, [SlotEvent::Spawned]);
    assert_eq!(h.state(late), Some(JobState::Stopping));
    h.script(
        late,
        settle_events(Exit::Signalled(15), 0).into_iter().skip(1),
    );
    let records = h.commit();
    let s = settled(&records)[0];
    assert_eq!(
        (s.outcome, s.reason),
        (Outcome::Cancelled, JobReason::ClientCancel)
    );
}

/// J2T-07: a setup failure with no cancel settles `not_started` with the
/// slot's reason; a slot start error settles with `capture_unavailable`
/// and returns the reservation with its victims.
#[test]
fn j2t_07_setup_failure_and_start_error() {
    let mut h = Harness::new(limits(4, 2, 8));
    let job = h.model(1, conversation(1), P1);
    h.script(
        job,
        [
            SlotEvent::SetupFailed(JobReason::CwdUnavailable),
            SlotEvent::CaptureSettled(capture(CaptureState::NotStarted, 0, 0)),
        ],
    );
    let records = h.commit();
    assert_eq!(kinds(&records), [28]);
    assert_eq!(settled(&records)[0].reason, JobReason::CwdUnavailable);

    h.payloads.victims = vec![asura_storage::payload::Victim {
        call: [9; 16],
        payloads: [[8; 16], [7; 16]],
    }];
    h.slots.borrow_mut().start_error = Some(SlotStartError::CaptureUnavailable);
    let Admission::Accepted { job: failed } = h.submit(model(2), conversation(1), P1) else {
        panic!("accepted");
    };
    let records = h.commit();
    assert_eq!(kinds(&records), [26, 28]);
    assert_eq!(
        settled(&records)
            .iter()
            .map(|s| (s.job, s.reason))
            .collect::<Vec<_>>(),
        [(failed, JobReason::CaptureUnavailable)]
    );
    assert!(h.payloads.released.contains(&(failed, 1)));
}

/// J2T-07 and J2T-24: a person stop then a person kill writes one kind 27
/// (`person_stop`), `T` then `K`, and settles with `person_kill`.
#[test]
fn j2t_07_person_kill_after_person_stop() {
    let mut h = Harness::new(limits(4, 2, 8));
    let Admission::Accepted { job } = h.submit(person(1), conversation(1), P1) else {
        panic!("accepted");
    };
    h.commit();
    h.script(job, [SlotEvent::Spawned]);
    assert_eq!(
        h.ctl
            .cancel(job, Requester::Person, JobReason::PersonStop, false),
        CancelReply::Accepted(JobState::Stopping)
    );
    assert_eq!(
        h.ctl
            .cancel(job, Requester::Person, JobReason::PersonKill, true),
        CancelReply::Accepted(JobState::Stopping)
    );
    // A second kill writes no second `K`.
    h.ctl
        .cancel(job, Requester::Person, JobReason::PersonKill, true);
    assert_eq!(h.stops(), [(job, StopKind::Term), (job, StopKind::Kill)]);
    h.script(
        job,
        settle_events(Exit::Signalled(9), 0).into_iter().skip(1),
    );
    let records = h.commit();
    assert_eq!(kinds(&records), [27, 28]);
    assert_eq!(cancels(&records)[0].reason, JobReason::PersonStop);
    assert!(!cancels(&records)[0].kill);
    let s = settled(&records)[0];
    assert_eq!(
        (s.outcome, s.reason),
        (Outcome::Cancelled, JobReason::PersonKill)
    );
}

// ----- J2T-24: person origin at the controller -----

/// J2T-24: a person waiter has no record until its slot and reservation
/// are granted; then kind 26 has the person layout; the slot starts only
/// after it commits; a model cancel is `not_permitted`.
#[test]
fn j2t_24_person_waiter_has_no_record_until_grant() {
    let mut h = Harness::new(limits(1, 1, 8));
    let running = h.model(1, conversation(1), P1);
    let Admission::Accepted { job } = h.submit(person(5), conversation(1), P1) else {
        panic!("accepted");
    };
    assert!(h.commit().is_empty(), "a waiter writes no record");
    assert_eq!(h.reason(job), Some(BlockReason::AgentSlots));
    assert_eq!(
        h.ctl
            .cancel(job, Requester::Model, JobReason::ModelCancel, false),
        CancelReply::NotPermitted
    );
    h.finish(running, 0);
    // Granted: slot and reservation first, then kind 26; no slot start yet.
    assert_eq!(h.state(job), Some(JobState::Starting));
    assert_eq!(h.started(), [running]);
    assert!(h.payloads.admitted.contains(&job));
    let (id, record) = h.ctl.next_record().expect("kind 26");
    assert_eq!(id, job);
    let Record::JobIntent(intent) = &record else {
        panic!("kind 26");
    };
    assert_eq!(intent.origin, JobOrigin::Person { input: [5; 16] });
    assert_eq!(intent.agent, 0);
    assert_encodes(&record);
    h.poll();
    assert_eq!(h.started(), [running], "no start before kind 26 commits");
    h.ctl.record_done(job, Ok(500));
    h.poll();
    assert_eq!(h.started(), [running, job]);
}

/// J2T-24: a held person kind 26 releases its slot and reservation and the
/// waiter waits again at its queue head with `journal_held`.
#[test]
fn j2t_24_person_kind_26_held_releases_the_slot() {
    let mut h = Harness::new(limits(4, 2, 8));
    let Admission::Accepted { job } = h.submit(person(5), conversation(1), P1) else {
        panic!("accepted");
    };
    h.poll();
    let (id, _) = h.ctl.next_record().expect("kind 26");
    h.ctl.record_done(id, Err(writer::Error::JournalQuota));
    h.poll();
    assert_eq!(h.state(job), Some(JobState::Queued));
    assert_eq!(h.reason(job), Some(BlockReason::JournalHeld));
    assert_eq!(h.payloads.released, [(job, 0)]);
    assert!(h.ctl.next_record().is_none(), "held: no resubmission yet");
    h.ctl.journal_hold_ended();
    let records = h.commit();
    assert_eq!(kinds(&records), [26]);
    assert_eq!(h.started(), [job]);
}

/// J2T-24: a person waiter cancelled before its grant leaves with no
/// record (`Withdrawn`).
#[test]
fn j2t_24_person_waiter_cancel_writes_nothing() {
    let mut h = Harness::new(limits(1, 1, 8));
    h.model(1, conversation(1), P1);
    let Admission::Accepted { job } = h.submit(person(5), conversation(1), P1) else {
        panic!("accepted");
    };
    assert_eq!(
        h.ctl
            .cancel(job, Requester::Person, JobReason::PersonStop, false),
        CancelReply::Accepted(JobState::Withdrawn)
    );
    assert!(h.commit().is_empty());
    assert_eq!(h.ctl.status(job), None);
}

// ----- Outbox order -----

/// Kind 26 commits before its kinds 27 and 28; closure records go before
/// new kind 26 records. J3T-21 (TJ-S1, J34-K2): a model start cancelled
/// before its kind 26 is submitted, or while it is in flight, is never
/// withdrawn: the cancel replies `Accepted(Recording)` and kinds 26, 27
/// and 28 (`not_started`, the trigger's reason) commit in that order.
#[test]
fn outbox_orders_intent_before_closures() {
    let mut h = Harness::new(limits(1, 1, 8));
    // Cancel before submission: kind 26 stays queued, 27 and 28 follow.
    let Admission::Accepted { job } = h.submit(model(1), conversation(1), P1) else {
        panic!("accepted");
    };
    assert_eq!(
        h.ctl
            .cancel(job, Requester::Model, JobReason::TurnEnded, false),
        CancelReply::Accepted(JobState::Recording)
    );
    let records = h.commit();
    assert_eq!(kinds(&records), [26, 27, 28]);
    assert_eq!(cancels(&records)[0].job, job);
    let s = settled(&records)[0];
    assert_eq!(
        (s.job, s.outcome, s.reason),
        (job, Outcome::NotStarted, JobReason::TurnEnded)
    );
    assert_eq!(h.state(job), Some(JobState::Recorded));
    assert!(h.started().is_empty(), "a cancelled start never starts");

    // Cancel while kind 26 is in flight: 27 and 28 wait for its commit.
    let Admission::Accepted { job } = h.submit(model(2), conversation(1), P1) else {
        panic!("accepted");
    };
    let (id, record) = h.ctl.next_record().expect("kind 26");
    assert_eq!((id, record.kind()), (job, 26));
    assert_eq!(
        h.ctl
            .cancel(job, Requester::Model, JobReason::TurnEnded, false),
        CancelReply::Accepted(JobState::Recording)
    );
    assert!(h.ctl.next_record().is_none(), "one record in flight");
    h.ctl.record_done(job, Ok(1));
    let records = h.commit();
    assert_eq!(kinds(&records), [27, 28]);
    let s = settled(&records)[0];
    assert_eq!(
        (s.outcome, s.reason),
        (Outcome::NotStarted, JobReason::TurnEnded)
    );

    // The closure (kind 27 of a running job) goes before a new kind 26.
    let mut h = Harness::new(limits(1, 1, 8));
    let running = h.model(1, conversation(1), P1);
    h.script(running, [SlotEvent::Spawned]);
    h.submit(model(2), conversation(1), P1);
    h.ctl
        .cancel(running, Requester::Client, JobReason::ClientCancel, false);
    let (_, first) = h.ctl.next_record().expect("record");
    assert_eq!(first.kind(), 27);
    h.ctl.record_done(running, Ok(1));
    let (_, second) = h.ctl.next_record().expect("record");
    assert_eq!(second.kind(), 26);
}

/// J3T-21: a model kind 26 held at the quota and cancelled during the hold
/// stays queued; kinds 26, 27 and 28 commit after `journal_hold_ended`.
#[test]
fn j3t_21_held_model_start_cancelled_records_after_the_hold() {
    let mut h = Harness::new(limits(4, 2, 8));
    let Admission::Accepted { job } = h.submit(model(1), conversation(1), P1) else {
        panic!("accepted");
    };
    let (id, _) = h.ctl.next_record().expect("kind 26");
    h.ctl.record_done(id, Err(writer::Error::JournalQuota));
    h.poll();
    assert_eq!(h.reason(job), Some(BlockReason::JournalHeld));
    assert_eq!(
        h.ctl
            .cancel(job, Requester::Model, JobReason::ModelCancel, false),
        CancelReply::Accepted(JobState::Recording)
    );
    assert!(h.commit().is_empty(), "held: nothing commits");
    h.ctl.journal_hold_ended();
    let records = h.commit();
    assert_eq!(kinds(&records), [26, 27, 28]);
    let s = settled(&records)[0];
    assert_eq!(
        (s.job, s.outcome, s.reason),
        (job, Outcome::NotStarted, JobReason::ModelCancel)
    );
    assert!(h.started().is_empty());
}

/// J3T-21: a model start cancelled while its kind 26 is in flight, whose
/// reply is then a quota hold, keeps its kind 26 queued; kinds 26, 27 and
/// 28 commit after the hold.
#[test]
fn j3t_21_in_flight_model_start_cancelled_then_held() {
    let mut h = Harness::new(limits(4, 2, 8));
    let Admission::Accepted { job } = h.submit(model(1), conversation(1), P1) else {
        panic!("accepted");
    };
    let (id, _) = h.ctl.next_record().expect("kind 26");
    assert_eq!(
        h.ctl
            .cancel(job, Requester::Model, JobReason::TurnEnded, false),
        CancelReply::Accepted(JobState::Recording)
    );
    h.ctl.record_done(id, Err(writer::Error::JournalDiskSpace));
    assert!(h.commit().is_empty(), "held: nothing commits");
    assert!(h.ctl.status(job).is_some(), "not withdrawn");
    h.ctl.journal_hold_ended();
    let records = h.commit();
    assert_eq!(kinds(&records), [26, 27, 28]);
    assert_eq!(settled(&records)[0].reason, JobReason::TurnEnded);
}

/// J3T-21: `revoke` with `execution_off` of a queued model start whose kind
/// 26 is still in the outbox gives kinds 26, 27 and 28, requester service.
#[test]
fn j3t_21_revoked_model_start_with_kind_26_in_the_outbox() {
    let mut h = Harness::new(limits(4, 2, 8));
    let Admission::Accepted { job } = h.submit(model(1), conversation(1), P1) else {
        panic!("accepted");
    };
    h.ctl
        .revoke(Some(P1), OriginFilter::Model, JobReason::ExecutionOff);
    let records = h.commit();
    assert_eq!(kinds(&records), [26, 27, 28]);
    let c = cancels(&records)[0];
    assert_eq!(
        (c.job, c.requester, c.reason),
        (job, JobRequester::Service, JobReason::ExecutionOff)
    );
    let s = settled(&records)[0];
    assert_eq!(
        (s.outcome, s.reason),
        (Outcome::NotStarted, JobReason::ExecutionOff)
    );
    assert!(h.started().is_empty());
}

/// J3T-21: while the journal is unavailable a model start whose kind 26
/// never committed is withdrawn: `Accepted(Withdrawn)` and nothing written.
#[test]
fn j3t_21_journal_unavailable_withdraws_a_model_start() {
    let mut h = Harness::new(limits(4, 2, 8));
    let Admission::Accepted { job } = h.submit(model(1), conversation(1), P1) else {
        panic!("accepted");
    };
    h.ctl.journal_unavailable();
    assert_eq!(
        h.ctl
            .cancel(job, Requester::Model, JobReason::TurnEnded, false),
        CancelReply::Accepted(JobState::Withdrawn)
    );
    assert!(h.commit().is_empty());
    assert_eq!(h.ctl.status(job), None);
}

/// J3T-21: a person job granted its slot is still withdrawn before its kind
/// 26 is submitted, and when its kind 26 is held after a cancel in flight.
#[test]
fn j3t_21_person_job_before_kind_26_is_withdrawn() {
    let mut h = Harness::new(limits(4, 2, 8));
    let Admission::Accepted { job } = h.submit(person(5), conversation(1), P1) else {
        panic!("accepted");
    };
    h.poll();
    assert_eq!(h.state(job), Some(JobState::Starting));
    assert_eq!(
        h.ctl
            .cancel(job, Requester::Person, JobReason::PersonStop, false),
        CancelReply::Accepted(JobState::Withdrawn)
    );
    assert!(h.commit().is_empty());

    let Admission::Accepted { job } = h.submit(person(6), conversation(1), P1) else {
        panic!("accepted");
    };
    h.poll();
    let (id, record) = h.ctl.next_record().expect("kind 26");
    assert_eq!((id, record.kind()), (job, 26));
    h.ctl
        .cancel(job, Requester::Person, JobReason::PersonStop, false);
    h.ctl.record_done(id, Err(writer::Error::JournalQuota));
    h.ctl.journal_hold_ended();
    assert!(h.commit().is_empty(), "held person job: no record");
    assert_eq!(h.ctl.status(job), None);
    assert!(h.started().is_empty());
}

/// A model kind 26 held at the journal quota waits with `journal_held`
/// while kinds 27 and 28 still commit; it is resubmitted after the hold.
#[test]
fn held_kind_26_waits_while_closures_commit() {
    let mut h = Harness::new(limits(4, 2, 8));
    let running = h.model(1, conversation(1), P1);
    h.script(running, [SlotEvent::Spawned]);
    let Admission::Accepted { job } = h.submit(model(2), conversation(1), P1) else {
        panic!("accepted");
    };
    let (id, _) = h.ctl.next_record().expect("kind 26");
    h.ctl.record_done(id, Err(writer::Error::JournalDiskSpace));
    h.poll();
    assert_eq!(h.reason(job), Some(BlockReason::JournalHeld));
    h.ctl
        .cancel(running, Requester::Client, JobReason::ClientCancel, false);
    let records = h.commit();
    assert_eq!(kinds(&records), [27], "closures commit while held");
    h.ctl.journal_hold_ended();
    assert_eq!(kinds(&h.commit()), [26]);
    assert_eq!(h.started(), [running, job]);
}

// ----- J2T-08: agent queues and start conditions -----

/// J2T-08: FIFO within an agent; rotation across agents beginning after
/// the agent served last; each start waits for a released slot, with no
/// timer.
#[test]
fn j2t_08_fifo_and_rotation() {
    let mut h = Harness::new(limits(1, 1, 8));
    let a1 = h.model(1, conversation(1), P1);
    let a2 = h.model(2, conversation(1), P1);
    let b1 = h.model(3, conversation(2), P1);
    let b2 = h.model(4, conversation(2), P1);
    assert_eq!(h.started(), [a1]);
    assert_eq!(h.reason(a2), Some(BlockReason::AgentSlots));
    assert_eq!(h.reason(b1), Some(BlockReason::ServiceSlots));
    assert_eq!(h.reason(b2), Some(BlockReason::ServiceSlots));
    // Nothing starts without a wake event.
    h.now += Duration::from_secs(3600);
    h.poll();
    assert_eq!(h.started(), [a1]);
    h.finish(a1, 0);
    assert_eq!(h.started(), [a1, b1]);
    h.finish(b1, 0);
    assert_eq!(h.started(), [a1, b1, a2]);
    h.finish(a2, 0);
    assert_eq!(h.started(), [a1, b1, a2, b2]);
}

/// J2T-08 and J2T-18 (unit): rotation fairness: with one slot and three
/// busy agents, each agent is served in turn.
#[test]
fn j2t_08_rotation_fairness() {
    let mut h = Harness::new(limits(1, 1, 8));
    let mut ids = Vec::new();
    for n in 0..9u8 {
        ids.push(h.model(n + 1, conversation(n % 3 + 1), P1));
    }
    let mut served = Vec::new();
    for _ in 0..9 {
        let last = *h.started().last().unwrap();
        served.push(ids.iter().position(|j| *j == last).unwrap() % 3);
        h.finish(last, 0);
    }
    assert_eq!(served, [0, 1, 2, 0, 1, 2, 0, 1, 2]);
}

/// J2T-08: each blocking reason in condition order: the first failing
/// condition is the one shown, with its brief text.
#[test]
fn j2t_08_block_reasons_in_order() {
    let mut h = Harness::unrecovered(limits(1, 1, 8));
    h.payloads.ready = false;
    h.payloads.pending = 4;
    h.payloads.short = Some(Short::Quota);
    let Admission::Accepted { job } = h.submit(model(1), conversation(1), P1) else {
        panic!("accepted");
    };
    h.commit();
    assert_eq!(h.reason(job), Some(BlockReason::Recovering));
    assert_eq!(
        h.text(job).as_deref(),
        Some("waiting: recovering jobs after restart")
    );
    h.ctl.recover(Vec::new());
    h.poll();
    assert_eq!(h.reason(job), Some(BlockReason::PayloadUnavailable));
    assert_eq!(
        h.text(job).as_deref(),
        Some("waiting: output store is recovering")
    );
    h.payloads.ready = true;
    h.poll();
    assert_eq!(h.reason(job), Some(BlockReason::PendingInstalls));
    assert_eq!(
        h.text(job).as_deref(),
        Some("waiting: 4 finished jobs still saving output")
    );
    h.payloads.pending = 3;
    h.poll();
    assert_eq!(h.reason(job), Some(BlockReason::Quota));
    assert_eq!(h.text(job).as_deref(), Some("waiting: output quota full"));
    h.payloads.short = Some(Short::DiskSpace);
    h.payloads.epoch += 1;
    h.poll();
    assert_eq!(h.reason(job), Some(BlockReason::DiskSpace));
    assert_eq!(h.text(job).as_deref(), Some("waiting: disk space low"));
    h.payloads.short = None;
    h.payloads.epoch += 1;
    h.poll();
    assert_eq!(h.started(), [job]);

    // Agent and service slots, with the awaiting-cleanup count.
    let mut h = Harness::new(limits(2, 1, 8));
    let a = h.model(1, conversation(1), P1);
    let a2 = h.model(2, conversation(1), P1);
    assert_eq!(
        h.text(a2).as_deref(),
        Some("waiting: 1 of 1 jobs running for this agent")
    );
    let b = h.model(3, conversation(2), P1);
    let c = h.model(4, conversation(3), P1);
    assert_eq!(h.started(), [a, b]);
    assert_eq!(h.reason(c), Some(BlockReason::ServiceSlots));
    assert_eq!(h.text(c).as_deref(), Some("waiting: 2 of 2 jobs running"));
    h.script(
        a,
        [
            SlotEvent::Spawned,
            SlotEvent::CleanupUnconfirmed(UnconfirmedReason::CleanupFailed),
        ],
    );
    assert_eq!(
        h.text(c).as_deref(),
        Some("waiting: 2 of 2 jobs running (1 awaiting cleanup)")
    );
    assert_eq!(
        h.text(a2).as_deref(),
        Some("waiting: 1 of 1 jobs running for this agent (1 awaiting cleanup)")
    );

    // Journal reasons.
    h.ctl.journal_unavailable();
    h.poll();
    assert_eq!(h.reason(c), Some(BlockReason::JournalUnavailable));
    assert_eq!(h.text(c).as_deref(), Some("waiting: journal unavailable"));
}

/// J2T-08: `queue_full`, `jobs_full` and `closed` are refused before any
/// record; a duplicate start returns the existing job at every bound; a
/// changed argument digest conflicts.
#[test]
fn j2t_08_busy_bounds_and_duplicates() {
    let mut h = Harness::new(limits(1, 1, 2));
    let a = h.model(1, conversation(1), P1);
    h.model(2, conversation(1), P1);
    h.model(3, conversation(1), P1);
    assert_eq!(
        h.submit(model(4), conversation(1), P1),
        Admission::Busy(BusyReason::QueueFull)
    );
    assert!(h.ctl.next_record().is_none(), "no record for busy");
    assert_eq!(
        h.submit(model(2), conversation(1), P1),
        Admission::Existing {
            job: model(2).job_id()
        }
    );
    let changed = h.request(model(2), conversation(1), P1, 9);
    assert_eq!(h.ctl.submit(changed, h.now), Admission::Conflict);
    // The running job counts toward the agent queue bound only while queued.
    assert_eq!(h.started(), [a]);

    // 64 unsettled jobs.
    let mut h = Harness::new(limits(1, 1, 16));
    for n in 0..64u8 {
        let agent = conversation(n / 16 + 1);
        assert!(matches!(
            h.submit(model(n + 1), agent, P1),
            Admission::Accepted { .. }
        ));
    }
    h.commit();
    assert_eq!(
        h.submit(model(100), conversation(9), P1),
        Admission::Busy(BusyReason::JobsFull)
    );
    assert_eq!(
        h.submit(model(1), conversation(1), P1),
        Admission::Existing {
            job: model(1).job_id()
        }
    );

    // Closed: queued starts settle; their identities are still known.
    h.ctl.close();
    assert_eq!(settled(&h.commit()).len(), 63);
    assert_eq!(
        h.submit(model(101), conversation(9), P1),
        Admission::Busy(BusyReason::Closed)
    );
    assert_eq!(
        h.submit(model(1), conversation(1), P1),
        Admission::Existing {
            job: model(1).job_id()
        }
    );
}

/// J2T-08: person waiters and model entries share the agent and service
/// counts in arrival order; no slot is reserved for either origin (AQ1).
#[test]
fn j2t_08_person_and_model_share_counts() {
    let mut h = Harness::new(limits(4, 2, 8));
    let m = h.model(1, conversation(1), P1);
    let Admission::Accepted { job: p } = h.submit(person(1), conversation(1), P1) else {
        panic!("accepted");
    };
    h.commit();
    let m2 = h.model(2, conversation(1), P1);
    assert_eq!(h.started(), [m, p]);
    assert_eq!(h.reason(m2), Some(BlockReason::AgentSlots));
    assert_eq!(
        h.text(m2).as_deref(),
        Some("waiting: 2 of 2 jobs running for this agent")
    );
}

/// J2T-08: the effective agent limit is the smaller of the two counts, and
/// the defaults are the J2a-2 values.
#[test]
fn j2t_08_limits_from_configuration() {
    let defaults = JobLimits::default();
    assert_eq!(
        (
            defaults.live_service,
            defaults.live_agent,
            defaults.queued_agent
        ),
        (4, 2, 8)
    );
    let mut h = Harness::new(limits(1, 1, 8));
    let a = h.model(1, conversation(1), P1);
    let b = h.model(2, conversation(2), P1);
    assert_eq!(h.started(), [a]);
    assert_eq!(h.reason(b), Some(BlockReason::ServiceSlots));
}

/// The `disk_space` recheck is paced 1, 1, 2, 3, 5, 8 s and so on, at most
/// 60 s apart; it ends nothing and stops once the block ends.
#[test]
fn disk_space_recheck_is_paced() {
    let mut h = Harness::new(limits(4, 2, 8));
    h.payloads.short = Some(Short::DiskSpace);
    let job = h.model(1, conversation(1), P1);
    assert_eq!(h.reason(job), Some(BlockReason::DiskSpace));
    let mut delays = Vec::new();
    for _ in 0..12 {
        let at = h.ctl.next_deadline(h.now).expect("recheck");
        delays.push((at - h.now).as_secs());
        h.now = at;
        h.poll();
    }
    assert_eq!(delays, [1, 1, 2, 3, 5, 8, 13, 21, 34, 55, 60, 60]);
    assert_eq!(h.state(job), Some(JobState::Queued));
    h.payloads.short = None;
    h.now = h.ctl.next_deadline(h.now).unwrap();
    h.poll();
    assert_eq!(h.started(), [job]);
    assert_eq!(h.ctl.next_deadline(h.now), None, "no other timer");
}

// ----- J2T-09 to J2T-11 -----

/// J2T-09: `CapturePaused` holds only its own slot; another job starts
/// and runs. Quota in `Draining` is a cancel trigger: capture discards and
/// the job records `quota` with an exit outcome.
#[test]
fn j2t_09_capture_pause_per_job() {
    let mut h = Harness::new(limits(2, 2, 8));
    let a = h.model(1, conversation(1), P1);
    h.script(
        a,
        [
            SlotEvent::Spawned,
            SlotEvent::CapturePaused(capture_worker::PauseReason::Enospc),
        ],
    );
    assert_eq!(h.state(a), Some(JobState::CapturePaused));
    let b = h.model(2, conversation(1), P1);
    assert_eq!(h.started(), [a, b]);
    h.script(b, [SlotEvent::Spawned]);
    assert_eq!(h.state(b), Some(JobState::Running));
    h.script(a, [SlotEvent::CaptureResumed]);
    assert_eq!(h.state(a), Some(JobState::Running));
    h.script(
        a,
        [
            SlotEvent::LeaderExited(Exit::Exited(0)),
            SlotEvent::CapturePaused(capture_worker::PauseReason::Quota),
        ],
    );
    assert_eq!(h.slots.borrow().discards, [a]);
    h.script(a, settle_events(Exit::Exited(0), 0).into_iter().skip(2));
    let records = h.commit();
    assert_eq!(kinds(&records), [27, 28]);
    assert_eq!(cancels(&records)[0].reason, JobReason::Quota);
    let s = settled(&records)[0];
    assert_eq!((s.outcome, s.reason), (Outcome::Exited, JobReason::Quota));
}

/// J2T-10: kind 4 enters `CleanupUnconfirmed`: no record, the slot stays
/// counted and the owner stays available; a late kind 2 settles with
/// cleanup `late` and the kind 4 reason, and the slot is released after
/// scratch cleanup.
#[test]
fn j2t_10_cleanup_unconfirmed_holds_the_slot() {
    let mut h = Harness::new(limits(1, 1, 8));
    let a = h.model(1, conversation(1), P1);
    h.script(
        a,
        [
            SlotEvent::Spawned,
            SlotEvent::LeaderExited(Exit::Exited(1)),
            SlotEvent::CleanupUnconfirmed(UnconfirmedReason::EscapeeUnterminated),
        ],
    );
    assert_eq!(h.state(a), Some(JobState::CleanupUnconfirmed));
    let b = h.model(2, conversation(2), P1);
    assert!(h.commit().is_empty(), "no settlement while unconfirmed");
    assert_eq!(
        h.text(b).as_deref(),
        Some("waiting: 1 of 1 jobs running (1 awaiting cleanup)")
    );
    // A cancel request is accepted and recorded; the state does not change.
    assert_eq!(
        h.ctl
            .cancel(a, Requester::Client, JobReason::ClientCancel, false),
        CancelReply::Accepted(JobState::CleanupUnconfirmed)
    );
    assert_eq!(kinds(&h.commit()), [27]);
    h.script(
        a,
        [
            SlotEvent::ProcessSettled {
                exit: Exit::Exited(1),
                cleanup: JobCleanup::Late,
                escapees: 1,
                elapsed_ms: 9,
                output_held: false,
            },
            SlotEvent::CaptureSettled(capture(CaptureState::Incomplete, 3, 0)),
        ],
    );
    let records = h.commit();
    let s = settled(&records)[0];
    assert_eq!(
        (
            s.outcome,
            s.reason,
            s.cleanup,
            s.unconfirmed_reason,
            s.escapees
        ),
        (
            Outcome::Exited,
            JobReason::ClientCancel,
            JobCleanup::Late,
            13,
            1
        )
    );
    assert_eq!(h.started(), [a]);
    h.script(a, [SlotEvent::ScratchCleaned]);
    h.idle(a);
    assert_eq!(h.started(), [a, b]);
}

/// J2T-10: retained scratch keeps the slot until restart.
#[test]
fn j2t_10_retained_scratch_keeps_the_slot() {
    let mut h = Harness::new(limits(1, 1, 8));
    let a = h.model(1, conversation(1), P1);
    h.script(a, settle_events(Exit::Exited(0), 0));
    h.commit();
    h.script(a, [SlotEvent::ScratchRetained]);
    h.idle(a);
    let b = h.model(2, conversation(2), P1);
    assert_eq!(h.started(), [a]);
    assert_eq!(h.reason(b), Some(BlockReason::ServiceSlots));
}

/// J2T-11: the kind 6 escapee count reaches kind 28 and `job.settled`.
#[test]
fn j2t_11_escapee_count() {
    let mut h = Harness::new(limits(4, 2, 8));
    let a = h.model(1, conversation(1), P1);
    h.script(a, settle_events(Exit::Exited(0), 3));
    let records = h.commit();
    let s = settled(&records)[0];
    assert_eq!((s.escapees, s.cleanup), (3, JobCleanup::Confirmed));
    let audit = h.audit.0.borrow();
    let Some(Event::JobSettled { escapees, .. }) =
        audit.iter().find(|e| matches!(e, Event::JobSettled { .. }))
    else {
        panic!("job.settled");
    };
    assert_eq!(*escapees, 3);
}

// ----- J2T-12 to J2T-15 -----

fn unsettled(n: u8, sequence: u64, cancel: bool) -> UnsettledJob {
    let origin = model(n).journal();
    let job = journal::job_id(&origin);
    UnsettledJob {
        job,
        origin,
        agent: 0,
        conversation: [n; 16],
        project: P1,
        owner_generation: 1,
        time: 0,
        intent: Locator {
            sequence,
            segment: 1,
            offset: 0,
            length: 225,
        },
        cancel: cancel.then_some(journal::JobCancel {
            requester: JobRequester::Client,
            reason: JobReason::ClientCancel,
            kill: false,
            frame: Locator::NONE,
        }),
        payloads: [[0; 16]; 2],
    }
}

/// J2T-12: one `interrupted` kind 28 (reason `restart`) per unsettled job,
/// oldest first; admission waits with `recovering` until all commit.
#[test]
fn j2t_12_recovery_writes_interrupted() {
    let mut h = Harness::unrecovered(limits(4, 2, 8));
    let Admission::Accepted { job } = h.submit(model(9), conversation(9), P1) else {
        panic!("accepted");
    };
    h.poll();
    let first = unsettled(1, 10, false);
    let second = unsettled(2, 20, true);
    h.ctl.recover(vec![second.clone(), first.clone()]);
    assert_eq!(h.state(first.job), Some(JobState::Interrupted));
    assert_eq!(
        h.ctl
            .cancel(first.job, Requester::Client, JobReason::ClientCancel, false),
        CancelReply::AlreadySettled(Outcome::Interrupted)
    );
    // The two interrupted settlements go before the new kind 26.
    let (a, r1) = h.ctl.next_record().unwrap();
    assert_eq!(a, first.job);
    h.ctl.record_done(a, Ok(1));
    h.poll();
    assert_eq!(h.reason(job), Some(BlockReason::Recovering));
    let (b, r2) = h.ctl.next_record().unwrap();
    assert_eq!(b, second.job);
    for r in [&r1, &r2] {
        let Record::JobSettled(s) = r else {
            panic!("kind 28");
        };
        assert_eq!(
            (s.outcome, s.reason, s.capture_state),
            (
                Outcome::Interrupted,
                JobReason::Restart,
                JobCaptureState::Interrupted
            )
        );
        assert_encodes(r);
    }
    h.ctl.record_done(b, Ok(2));
    h.commit();
    assert_eq!(h.started(), [job], "admission opens after recovery");
    assert_eq!(
        h.audit_kinds()[..3],
        ["job.queued", "job.settled", "job.settled"]
    );
}

/// J2T-13: journal outage stops every live job, drops the outbox and starts
/// nothing; a settled capture is abandoned, not installed.
#[test]
fn j2t_13_journal_outage() {
    let mut h = Harness::new(limits(1, 1, 8));
    let a = h.model(1, conversation(1), P1);
    h.script(a, [SlotEvent::Spawned]);
    let b = h.model(2, conversation(1), P1);
    h.ctl
        .cancel(a, Requester::Client, JobReason::ClientCancel, false);
    h.ctl.journal_unavailable();
    assert_eq!(h.stops(), [(a, StopKind::Term)]);
    assert!(h.ctl.next_record().is_none(), "outbox dropped");
    h.script(a, settle_events(Exit::Exited(0), 0).into_iter().skip(1));
    assert_eq!(h.payloads.abandoned, [(a, false)]);
    assert!(h.payloads.committed.is_empty());
    h.script(a, [SlotEvent::ScratchCleaned]);
    h.idle(a);
    assert_eq!(h.started(), [a], "nothing starts");
    assert_eq!(h.reason(b), Some(BlockReason::JournalUnavailable));
    assert!(h.ctl.next_record().is_none());
    assert!(h.ctl.settled());
}

/// J2T-14: shutdown settles queued starts `not_started` with `shutdown`,
/// sends `T` to live jobs, removes person waiters, refuses new submissions
/// with `closed`, and is settled once the closure records commit.
#[test]
fn j2t_14_shutdown() {
    let mut h = Harness::new(limits(1, 1, 8));
    let live = h.model(1, conversation(1), P1);
    h.script(live, [SlotEvent::Spawned]);
    let queued = h.model(2, conversation(1), P1);
    let Admission::Accepted { job: waiter } = h.submit(person(3), conversation(1), P1) else {
        panic!("accepted");
    };
    h.ctl.close();
    assert_eq!(h.stops(), [(live, StopKind::Term)]);
    assert_eq!(h.ctl.status(waiter), None, "waiter removed");
    assert_eq!(
        h.submit(model(4), conversation(1), P1),
        Admission::Busy(BusyReason::Closed)
    );
    assert!(!h.ctl.settled());
    let records = h.commit();
    assert_eq!(kinds(&records), [27, 27, 28]);
    let s = settled(&records)[0];
    assert_eq!(
        (s.job, s.outcome, s.reason),
        (queued, Outcome::NotStarted, JobReason::Shutdown)
    );
    assert!(
        cancels(&records)
            .iter()
            .all(|c| c.reason == JobReason::Shutdown)
    );
    h.script(
        live,
        settle_events(Exit::Signalled(15), 0).into_iter().skip(1),
    );
    let records = h.commit();
    assert_eq!(settled(&records)[0].outcome, Outcome::Cancelled);
    h.script(live, [SlotEvent::ScratchCleaned]);
    h.idle(live);
    assert!(h.ctl.settled());
    assert_eq!(h.started(), [live]);
}

/// J2T-15: revocation reaches only the project's jobs: queued ones settle
/// `not_started`, live ones get `T`, both with `identity_changed`.
#[test]
fn j2t_15_revocation_by_project() {
    let mut h = Harness::new(limits(2, 1, 8));
    let p1_live = h.model(1, conversation(1), P1);
    h.script(p1_live, [SlotEvent::Spawned]);
    let p1_queued = h.model(2, conversation(1), P1);
    let p2_live = h.model(3, conversation(2), P2);
    h.script(p2_live, [SlotEvent::Spawned]);
    h.ctl.revoke_project(P1, JobReason::IdentityChanged);
    assert_eq!(h.stops(), [(p1_live, StopKind::Term)]);
    let records = h.commit();
    assert!(
        cancels(&records)
            .iter()
            .all(|c| c.reason == JobReason::IdentityChanged && c.requester == JobRequester::Service)
    );
    assert_eq!(cancels(&records).len(), 2);
    assert_eq!(settled(&records)[0].job, p1_queued);
    assert_eq!(h.state(p2_live), Some(JobState::Running));
    // Revocation needs a service reason.
    h.ctl.revoke_project(P2, JobReason::ClientCancel);
    assert_eq!(h.state(p2_live), Some(JobState::Running));
}

/// J2T-15: the slot's pre-spawn identity recheck settles its own job
/// `not_started` with `project_changed` and revokes the project's other
/// jobs with `identity_changed`; another project's job continues.
#[test]
fn j2t_15_slot_identity_recheck() {
    let mut h = Harness::new(limits(4, 2, 8));
    let other = h.model(1, conversation(1), P1);
    h.script(other, [SlotEvent::Spawned]);
    let elsewhere = h.model(2, conversation(2), P2);
    h.script(elsewhere, [SlotEvent::Spawned]);
    let checking = h.model(3, conversation(1), P1);
    h.script(
        checking,
        [
            SlotEvent::IdentityChanged,
            SlotEvent::CaptureSettled(capture(CaptureState::NotStarted, 0, 0)),
        ],
    );
    let records = h.commit();
    let s = settled(&records)[0];
    assert_eq!(
        (s.job, s.outcome, s.reason),
        (checking, Outcome::NotStarted, JobReason::ProjectChanged)
    );
    assert_eq!(h.stops(), [(other, StopKind::Term)]);
    assert_eq!(cancels(&records)[0].reason, JobReason::IdentityChanged);
    assert_eq!(h.state(elsewhere), Some(JobState::Running));
}

/// A capture fault other than ENOSPC is a cancel trigger with
/// `capture_fault`; the prefix is kept.
#[test]
fn capture_fault_is_a_cancel_trigger() {
    let mut h = Harness::new(limits(4, 2, 8));
    let a = h.model(1, conversation(1), P1);
    h.script(
        a,
        [
            SlotEvent::Spawned,
            SlotEvent::CaptureFault(CaptureReason::Enospc),
        ],
    );
    assert_eq!(h.state(a), Some(JobState::Running), "ENOSPC is no fault");
    h.script(a, [SlotEvent::CaptureFault(CaptureReason::StorageFault)]);
    assert_eq!(h.stops(), [(a, StopKind::Term)]);
    h.script(a, settle_events(Exit::Signalled(15), 0).into_iter().skip(1));
    let records = h.commit();
    assert_eq!(cancels(&records)[0].requester, JobRequester::Service);
    let s = settled(&records)[0];
    assert_eq!(
        (s.outcome, s.reason),
        (Outcome::Cancelled, JobReason::CaptureFault)
    );
}

// ----- J2T-20: audit events -----

/// J2T-20: the four job events with their fields; one `job.queued` per job
/// however its reason changes; no event holds the command text.
#[test]
fn j2t_20_audit_events() {
    let mut h = Harness::new(limits(1, 1, 8));
    let a = h.model(1, conversation(1), P1);
    let b = h.model(2, conversation(2), P1);
    h.payloads.short = Some(Short::Quota);
    h.script(a, [SlotEvent::Spawned]);
    h.ctl
        .cancel(a, Requester::Client, JobReason::ClientCancel, false);
    h.script(a, settle_events(Exit::Exited(0), 0).into_iter().skip(1));
    h.commit();
    h.script(a, [SlotEvent::ScratchCleaned]);
    h.idle(a);
    assert_eq!(h.reason(b), Some(BlockReason::Quota), "reason changed");
    let audit = h.audit.0.borrow();
    for event in audit.iter() {
        event.validate().expect("valid audit event");
        let text = serde_json::to_string(event).unwrap();
        assert!(!text.contains("secret-command-text"), "{text}");
        assert!(!text.contains("/private/tmp/project"), "{text}");
    }
    let queued: Vec<_> = audit
        .iter()
        .filter(|e| matches!(e, Event::JobQueued { .. }))
        .collect();
    assert_eq!(queued.len(), 1, "one job.queued per job");
    assert_eq!(
        *queued[0],
        Event::JobQueued {
            project: P1,
            conversation: [2; 16],
            job: b,
            origin: record::JobOrigin::Model,
            agent: 0,
            reason: record::JobBlockReason::ServiceSlots,
            outside_sandbox: false,
        }
    );
    let started = audit
        .iter()
        .find(|e| matches!(e, Event::JobStarted { .. }))
        .unwrap();
    assert_eq!(
        *started,
        Event::JobStarted {
            project: P1,
            conversation: [1; 16],
            job: a,
            origin: record::JobOrigin::Model,
            agent: 0,
            profile: record::JobProfile::Model1,
            operation: Some([1; 16]),
            generation: Some(1),
            ordinal: Some(1),
            input: None,
            outside_sandbox: false,
        }
    );
    assert!(audit.contains(&Event::JobCancelRequested {
        project: P1,
        conversation: [1; 16],
        job: a,
        requester: record::JobRequester::Client,
        reason: record::JobReason::ClientCancel,
        kill: false,
        outside_sandbox: false,
    }));
    let Some(Event::JobSettled {
        outcome,
        reason,
        exit_kind,
        cleanup,
        capture_state,
        stdout_bytes,
        elapsed_ms,
        ..
    }) = audit.iter().find(|e| matches!(e, Event::JobSettled { .. }))
    else {
        panic!("job.settled");
    };
    assert_eq!(
        (
            *outcome,
            *reason,
            *exit_kind,
            *cleanup,
            *capture_state,
            *stdout_bytes,
            *elapsed_ms
        ),
        (
            record::JobOutcome::Cancelled,
            record::JobReason::ClientCancel,
            record::JobExitKind::Exited,
            record::JobCleanup::Confirmed,
            record::JobCaptureState::Complete,
            12,
            42
        )
    );
}

/// The person `job.started` carries `input` and no model reference.
#[test]
fn j2t_20_person_started_event() {
    let mut h = Harness::new(limits(4, 2, 8));
    let Admission::Accepted { job } = h.submit(person(6), conversation(1), P1) else {
        panic!("accepted");
    };
    h.commit();
    h.script(job, [SlotEvent::Spawned]);
    let audit = h.audit.0.borrow();
    let started = audit
        .iter()
        .find(|e| matches!(e, Event::JobStarted { .. }))
        .unwrap();
    started.validate().unwrap();
    let Event::JobStarted {
        origin,
        operation,
        input,
        ..
    } = started
    else {
        unreachable!()
    };
    assert_eq!(
        (*origin, *operation, *input),
        (record::JobOrigin::Person, None, Some([6; 16]))
    );
}

/// Controller events: at most 64 per drain, and undrained state events of
/// one job coalesce.
#[test]
fn events_are_bounded_and_coalesce() {
    let mut h = Harness::new(limits(1, 1, 16));
    for n in 0..40u8 {
        h.submit(model(n + 1), conversation(n % 4 + 1), P1);
    }
    h.commit();
    let mut out = Vec::new();
    h.ctl.drain_events(&mut out);
    assert_eq!(out.len(), EVENTS_PER_DRAIN);
    let mut rest = Vec::new();
    h.ctl.drain_events(&mut rest);
    assert!(!rest.is_empty() && rest.len() <= EVENTS_PER_DRAIN);
    let states = out
        .iter()
        .chain(&rest)
        .filter(|e| matches!(e, JobEvent::State(..)))
        .count();
    assert!(states <= 40, "one undrained state event per job");
}

/// A service requester may cancel with a service reason only; the
/// controller adds no poll interest of its own, and J2 never closes a
/// capture early (that is P3-T).
#[test]
fn service_cancel_and_reactor_hooks() {
    let mut h = Harness::new(limits(4, 2, 8));
    let a = h.model(1, conversation(1), P1);
    h.script(a, [SlotEvent::Spawned]);
    assert_eq!(
        h.ctl
            .cancel(a, Requester::Service, JobReason::ClientCancel, false),
        CancelReply::NotPermitted
    );
    assert_eq!(
        h.ctl
            .cancel(a, Requester::Client, JobReason::PersonKill, true),
        CancelReply::NotPermitted
    );
    assert_eq!(
        h.ctl
            .cancel(a, Requester::Service, JobReason::Shutdown, false),
        CancelReply::Accepted(JobState::Stopping)
    );
    assert_eq!(
        h.ctl
            .cancel([3; 16], Requester::Client, JobReason::ClientCancel, false),
        CancelReply::NotFound
    );
    let mut interests = Vec::new();
    h.ctl.interests(&mut interests);
    assert!(interests.is_empty());
    assert!(h.slots.borrow().closes.is_empty());
}

// J3 and J4 controller extensions (J34-K).
mod j34;

/// Person command packet P3-K (person-command-mode.md): the person
/// profiles at admission, the `!!` switch check at slot grant (reason 20),
/// `ShellExited` and the released `Draining` state (OD2, PQ3), and
/// `close_capture` on cancel. PC-U23, the controller part of PC-U43 and
/// PC-U53.
mod person {
    use super::*;
    use std::cell::Cell;
    use std::rc::Rc;

    fn outside_request(h: &Harness, n: u8, agent: AgentKey) -> StartRequest {
        let mut request = h.request(person(n), agent, P1, 1);
        request.profile = Profile::PersonOutside1;
        request
    }
    /// Submits a `!!` job; returns its ID.
    fn submit_outside(h: &mut Harness, n: u8, agent: AgentKey) -> Id {
        let request = outside_request(h, n, agent);
        let Admission::Accepted { job } = h.ctl.submit(request, h.now) else {
            panic!("`!!` start refused");
        };
        job
    }
    /// A switch the test flips: the configuration owner's current value.
    fn switch(h: &mut Harness, on: bool) -> Rc<Cell<bool>> {
        let value = Rc::new(Cell::new(on));
        let read = value.clone();
        h.ctl.set_outside_switch(Box::new(move || read.get()));
        value
    }
    fn events(h: &mut Harness) -> Vec<JobEvent> {
        let mut out = Vec::new();
        h.ctl.drain_events(&mut out);
        out
    }
    fn shell_exits(events: &[JobEvent]) -> Vec<(Id, Exit, u32)> {
        events
            .iter()
            .filter_map(|e| match *e {
                JobEvent::ShellExited(job, exit, survivors) => Some((job, exit, survivors)),
                _ => None,
            })
            .collect()
    }
    /// Kind 5, kind 6 with `survivors`, then kind 2 (reason 15 when
    /// `held`, else reason 14), as the `!!` slot reports them.
    fn shell_exit(exit: Exit, survivors: u32, held: bool) -> Vec<SlotEvent> {
        vec![
            SlotEvent::Spawned,
            SlotEvent::LeaderExited(exit),
            SlotEvent::ProcessSettled {
                exit,
                cleanup: JobCleanup::Confirmed,
                escapees: survivors,
                elapsed_ms: 42,
                output_held: held,
            },
        ]
    }
    fn closed_capture() -> Box<CaptureOutcome> {
        let mut c = capture(CaptureState::Incomplete, 30, 5);
        c.reason = CaptureReason::ClosedAfterCancel;
        c
    }
    fn closes(h: &Harness) -> Vec<Id> {
        h.slots.borrow().closes.clone()
    }
    fn discards(h: &Harness) -> Vec<Id> {
        h.slots.borrow().discards.clone()
    }
    /// Every audit event is valid, and those of `job` carry the `!!` flag.
    fn assert_outside_audit(h: &Harness, job: Id) {
        for event in h.audit.0.borrow().iter() {
            event.validate().expect("valid audit event");
            let (id, outside) = match event {
                Event::JobQueued {
                    job,
                    outside_sandbox,
                    ..
                }
                | Event::JobStarted {
                    job,
                    outside_sandbox,
                    ..
                }
                | Event::JobCancelRequested {
                    job,
                    outside_sandbox,
                    ..
                }
                | Event::JobSettled {
                    job,
                    outside_sandbox,
                    ..
                } => (*job, *outside_sandbox),
                _ => continue,
            };
            assert_eq!(outside, id == job, "outside_sandbox of {id:?}");
        }
    }

    /// The profile is fixed at admission and paired with the origin:
    /// `model-1` for a model start, `person-1` or `person-outside-1` for a
    /// person start; any other pairing is a conflict and admits nothing.
    #[test]
    fn profile_is_paired_with_origin_at_admission() {
        let mut h = Harness::new(limits(4, 2, 8));
        let mut request = h.request(person(1), conversation(1), P1, 1);
        request.profile = Profile::Model1;
        assert_eq!(h.ctl.submit(request, h.now), Admission::Conflict);
        let mut request = h.request(model(2), conversation(2), P1, 1);
        request.profile = Profile::Person1;
        assert_eq!(h.ctl.submit(request, h.now), Admission::Conflict);
        let mut request = h.request(model(3), conversation(3), P1, 1);
        request.profile = Profile::PersonOutside1;
        assert_eq!(h.ctl.submit(request, h.now), Admission::Conflict);
        assert!(h.ctl.status(person(1).job_id()).is_none());
        // `!` and `!!` starts are admitted with their own profiles, and the
        // started event names them.
        let bang = h.submit(person(4), conversation(4), P1);
        let Admission::Accepted { job: bang } = bang else {
            panic!("`!` accepted");
        };
        let outside = submit_outside(&mut h, 5, conversation(5));
        h.commit();
        h.script(bang, [SlotEvent::Spawned]);
        h.script(outside, [SlotEvent::Spawned]);
        let audit = h.audit.0.borrow();
        let profiles: Vec<_> = audit
            .iter()
            .filter_map(|e| match e {
                Event::JobStarted {
                    job,
                    profile,
                    outside_sandbox,
                    ..
                } => Some((*job, *profile, *outside_sandbox)),
                _ => None,
            })
            .collect();
        assert_eq!(
            profiles,
            [
                (bang, record::JobProfile::Person1, false),
                (outside, record::JobProfile::PersonOutside1, true)
            ]
        );
        for event in audit.iter() {
            event.validate().expect("valid audit event");
        }
    }

    /// PC-U23: a person command at the head of its agent queue, blocked
    /// for a slot and then by the quota, stays queued with each reason and
    /// its brief text, writes no record, and starts when space frees.
    #[test]
    fn pc_u23_person_waiter_blocked_by_slot_then_quota() {
        let mut h = Harness::new(limits(1, 1, 8));
        let running = h.model(1, conversation(1), P1);
        let job = submit_outside(&mut h, 5, conversation(1));
        assert!(h.commit().is_empty(), "a waiter writes no record");
        assert_eq!(h.state(job), Some(JobState::Queued));
        assert_eq!(h.reason(job), Some(BlockReason::AgentSlots));
        assert_eq!(
            h.text(job).as_deref(),
            Some("waiting: 1 of 1 jobs running for this agent")
        );
        let queued = |events: Vec<JobEvent>| -> Vec<BlockReason> {
            events
                .into_iter()
                .filter_map(|e| match e {
                    JobEvent::Queued(id, reason) if id == job => Some(reason),
                    _ => None,
                })
                .collect()
        };
        assert_eq!(queued(events(&mut h)), [BlockReason::AgentSlots]);
        // The slot frees; now the 1 MiB reservation is short.
        h.payloads.short = Some(Short::Quota);
        h.finish(running, 0);
        assert_eq!(h.state(job), Some(JobState::Queued));
        assert_eq!(h.reason(job), Some(BlockReason::Quota));
        assert_eq!(h.text(job).as_deref(), Some("waiting: output quota full"));
        assert!(!h.payloads.admitted.contains(&job));
        // Each reason reaches the owner as a `Queued` event.
        assert_eq!(queued(events(&mut h)), [BlockReason::Quota]);
        // Space frees: retried on the space epoch, no timer.
        h.payloads.short = None;
        h.payloads.epoch += 1;
        let records = h.commit();
        assert_eq!(kinds(&records), [26]);
        assert_eq!(h.started(), [running, job]);
        assert_eq!(h.state(job), Some(JobState::Starting));
        assert_eq!(
            h.audit_kinds()
                .iter()
                .filter(|k| **k == "job.queued")
                .count(),
            1
        );
        assert_outside_audit(&h, job);
    }

    /// PC-U43 (controller part): the switch on at submit and off at slot
    /// grant: kind 26 then kind 28 `not_started` reason 20, no setup, the
    /// slot and reservation released, and `job.settled` with the flag.
    #[test]
    fn pc_u43_switch_off_at_slot_grant() {
        let mut h = Harness::new(limits(1, 1, 8));
        let allow = switch(&mut h, true);
        let running = h.model(1, conversation(1), P1);
        let job = submit_outside(&mut h, 5, conversation(1));
        let behind = h.model(2, conversation(1), P1);
        h.poll();
        assert_eq!(h.reason(job), Some(BlockReason::AgentSlots));
        allow.set(false);
        h.finish(running, 0);
        let records = h.commit();
        assert_eq!(kinds(&records), [26, 28]);
        let Record::JobIntent(intent) = &records[0] else {
            panic!("kind 26");
        };
        assert_eq!(intent.origin, JobOrigin::Person { input: [5; 16] });
        let s = settled(&records)[0];
        assert_eq!(
            (s.job, s.outcome, s.reason, s.capture_state, s.escapees),
            (
                job,
                Outcome::NotStarted,
                JobReason::OutsideSandboxOff,
                JobCaptureState::NotStarted,
                0
            )
        );
        assert!(cancels(&records).is_empty(), "reason 20 is not a cancel");
        // No setup: the slot never started; the reservation returned after
        // kind 28 committed, and the slot went to the next head.
        assert!(!h.started().contains(&job));
        assert!(h.payloads.released.iter().any(|(id, _)| *id == job));
        assert_eq!(h.state(job), Some(JobState::Recorded));
        assert_eq!(h.started(), [running, behind], "slot released");
        assert!(
            events(&mut h).contains(&JobEvent::Settled(job, Outcome::NotStarted)),
            "the owner deletes the secret item on this settlement"
        );
        let audit = h.audit.0.borrow();
        assert!(
            !audit
                .iter()
                .any(|e| matches!(e, Event::JobStarted { job: id, .. } if *id == job))
        );
        let settled_event = audit
            .iter()
            .find(|e| matches!(e, Event::JobSettled { job: id, .. } if *id == job))
            .expect("job.settled");
        let Event::JobSettled {
            reason,
            outside_sandbox,
            ..
        } = settled_event
        else {
            unreachable!()
        };
        assert_eq!(
            (*reason, *outside_sandbox),
            (record::JobReason::OutsideSandboxOff, true)
        );
        drop(audit);
        assert_outside_audit(&h, job);
    }

    /// PC-U43 (controller part): the switch governs `!!` only, and only at
    /// slot grant: a `!` job starts while it is off, and turning it off
    /// after a `!!` spawn leaves that job running.
    #[test]
    fn pc_u43_switch_off_after_spawn_and_for_bang() {
        let mut h = Harness::new(limits(4, 2, 8));
        let allow = switch(&mut h, true);
        let outside = submit_outside(&mut h, 5, conversation(1));
        h.commit();
        h.script(outside, [SlotEvent::Spawned]);
        assert_eq!(h.state(outside), Some(JobState::Running));
        allow.set(false);
        h.poll();
        assert_eq!(h.state(outside), Some(JobState::Running));
        assert!(h.stops().is_empty(), "a running `!!` job is not stopped");
        let Admission::Accepted { job: bang } = h.submit(person(6), conversation(2), P1) else {
            panic!("accepted");
        };
        h.commit();
        assert_eq!(h.started(), [outside, bang], "`!` ignores the switch");
        let records = h.finish(outside, 0);
        let s = settled(&records)[0];
        assert_eq!((s.outcome, s.reason), (Outcome::Exited, JobReason::None));
    }

    /// PC-U53: a `!!` slot reports kind 5, kind 6 count 2 and kind 2
    /// reason 15, then capture EOF. `ShellExited` once; the job stays
    /// `Draining` with its slot held and named in a blocked start's text;
    /// EOF settles `exited` with capture `complete` and escapees 2.
    #[test]
    fn pc_u53_survivor_holds_output_then_eof() {
        let mut h = Harness::new(limits(1, 1, 8));
        let job = submit_outside(&mut h, 5, conversation(1));
        h.commit();
        events(&mut h);
        h.script(job, shell_exit(Exit::Exited(0), 2, true));
        let drained = events(&mut h);
        assert_eq!(shell_exits(&drained), [(job, Exit::Exited(0), 2)]);
        assert!(!drained.iter().any(|e| matches!(e, JobEvent::Settled(..))));
        assert_eq!(h.state(job), Some(JobState::Draining));
        assert!(h.stops().is_empty(), "PQ3, EC-D1: nothing is signalled");
        let v = h.ctl.view(job, h.now).expect("view");
        assert_eq!(
            (v.profile, v.shell_exited, v.left_running),
            (Profile::PersonOutside1, true, Some(2))
        );
        assert_eq!(v.shell_exit, Some((1, 0)), "the kind 5 exit is kept");
        assert_eq!(h.ctl.live_count(None), 1);
        // The slot stays held: a later start waits and names it.
        let next = h.model(2, conversation(1), P1);
        assert_eq!(h.reason(next), Some(BlockReason::AgentSlots));
        assert_eq!(
            h.text(next).as_deref(),
            Some("waiting: 1 of 1 jobs running for this agent (1 shell exited, output open)")
        );
        // More slot polls: no second `ShellExited`.
        h.poll();
        assert!(shell_exits(&events(&mut h)).is_empty());
        // Survivor EOF: the capture seals complete.
        h.script(
            job,
            [SlotEvent::CaptureSettled(capture(
                CaptureState::Complete,
                40,
                5,
            ))],
        );
        let records = h.commit();
        let s = settled(&records)[0];
        assert_eq!(
            (
                s.outcome,
                s.reason,
                s.exit_value,
                s.capture_state,
                s.capture_reason,
                s.escapees,
                s.cleanup
            ),
            (
                Outcome::Exited,
                JobReason::None,
                0,
                JobCaptureState::Complete,
                CaptureReason::None,
                2,
                JobCleanup::Confirmed
            )
        );
        assert!(cancels(&records).is_empty());
        let v = h.ctl.view(job, h.now).expect("view");
        assert_eq!((v.shell_exited, v.left_running), (false, Some(2)));
        assert_eq!(v.shell_exit, None, "settled: the exit is kind 28's");
        h.script(job, [SlotEvent::ScratchCleaned]);
        h.idle(job);
        assert_eq!(h.started(), [job, next], "slot released at settlement");
        assert_outside_audit(&h, job);
        let audit = h.audit.0.borrow();
        assert!(audit.iter().any(|e| matches!(
            e,
            Event::JobSettled { job: id, escapees: 2, outside_sandbox: true, .. } if *id == job
        )));
    }

    /// PC-U53: a person cancel of a released `!!` job writes kind 27,
    /// calls `close_capture` (no lease byte, no discard), and settles
    /// `exited` with the trigger's reason, capture `incomplete` reason 7.
    /// A kill after the stop writes nothing more.
    #[test]
    fn pc_u53_person_cancel_closes_capture() {
        let mut h = Harness::new(limits(4, 2, 8));
        let job = submit_outside(&mut h, 5, conversation(1));
        h.commit();
        h.script(job, shell_exit(Exit::Exited(3), 2, true));
        assert_eq!(shell_exits(&events(&mut h)).len(), 1);
        let v = h.ctl.view(job, h.now).expect("view");
        assert_eq!((v.shell_exited, v.shell_exit), (true, Some((1, 3))));
        assert_eq!(
            h.ctl
                .cancel(job, Requester::Person, JobReason::PersonStop, false),
            CancelReply::Accepted(JobState::Draining)
        );
        assert_eq!(closes(&h), [job]);
        assert!(discards(&h).is_empty());
        assert!(h.stops().is_empty(), "the guardian has exited");
        assert_eq!(
            h.ctl
                .cancel(job, Requester::Person, JobReason::PersonKill, true),
            CancelReply::Accepted(JobState::Draining)
        );
        assert_eq!(closes(&h), [job], "closed once");
        assert!(h.stops().is_empty());
        let records = h.commit();
        let c = cancels(&records);
        assert_eq!(
            (c.len(), c[0].requester, c[0].reason, c[0].kill),
            (1, JobRequester::Person, JobReason::PersonStop, false)
        );
        h.script(job, [SlotEvent::CaptureSettled(closed_capture())]);
        let records = h.commit();
        let s = settled(&records)[0];
        assert_eq!(
            (
                s.outcome,
                s.reason,
                s.exit_value,
                s.capture_state,
                s.capture_reason,
                s.escapees
            ),
            (
                Outcome::Exited,
                JobReason::PersonKill,
                3,
                JobCaptureState::Incomplete,
                CaptureReason::ClosedAfterCancel,
                2
            )
        );
        assert!(shell_exits(&events(&mut h)).is_empty());
        assert_outside_audit(&h, job);
    }

    /// PC-U53: kind 2 reason 14 (no survivor holds output) emits no
    /// `ShellExited`; the job finalizes and settles on EOF as any job.
    #[test]
    fn pc_u53_reason_14_never_releases() {
        let mut h = Harness::new(limits(4, 2, 8));
        let job = submit_outside(&mut h, 5, conversation(1));
        h.commit();
        h.script(job, shell_exit(Exit::Exited(0), 2, false));
        assert!(shell_exits(&events(&mut h)).is_empty());
        assert_eq!(h.state(job), Some(JobState::Finalizing));
        let v = h.ctl.view(job, h.now).expect("view");
        assert_eq!((v.shell_exited, v.left_running), (false, None));
        assert_eq!(v.shell_exit, None, "not released");
        h.script(
            job,
            [SlotEvent::CaptureSettled(capture(
                CaptureState::Complete,
                4,
                1,
            ))],
        );
        let records = h.commit();
        let s = settled(&records)[0];
        assert_eq!(
            (s.outcome, s.escapees, s.capture_state),
            (Outcome::Exited, 2, JobCaptureState::Complete)
        );
        assert!(shell_exits(&events(&mut h)).is_empty());
    }

    /// Reason 15 releases only a `!!` job whose capture is still open: a
    /// `!` job never gets `ShellExited` (its slot never reports a held
    /// output, and the controller ignores one), and a capture that sealed
    /// before kind 2 settles at once.
    #[test]
    fn shell_exited_only_for_open_outside_capture() {
        let mut h = Harness::new(limits(4, 2, 8));
        let Admission::Accepted { job: bang } = h.submit(person(4), conversation(1), P1) else {
            panic!("accepted");
        };
        let outside = submit_outside(&mut h, 5, conversation(2));
        h.commit();
        h.script(bang, shell_exit(Exit::Exited(0), 0, true));
        assert_eq!(h.state(bang), Some(JobState::Finalizing));
        let mut early = shell_exit(Exit::Exited(0), 1, true);
        early.insert(
            2,
            SlotEvent::CaptureSettled(capture(CaptureState::Complete, 4, 1)),
        );
        h.script(outside, early);
        assert!(shell_exits(&events(&mut h)).is_empty());
        assert_eq!(h.state(outside), Some(JobState::Recording));
    }

    /// A trigger before the guardian's kind 2 (in `Draining` after kind 5):
    /// capture discards, and the reason 15 that follows closes it, so the
    /// survivors cannot hold the job open. It settles `exited` with the
    /// trigger's reason and capture reason 7.
    #[test]
    fn trigger_between_shell_exit_and_reason_15_closes_capture() {
        let mut h = Harness::new(limits(4, 2, 8));
        let job = submit_outside(&mut h, 5, conversation(1));
        h.commit();
        h.script(
            job,
            [SlotEvent::Spawned, SlotEvent::LeaderExited(Exit::Exited(0))],
        );
        assert_eq!(h.state(job), Some(JobState::Draining));
        let v = h.ctl.view(job, h.now).expect("view");
        assert_eq!(
            (v.shell_exited, v.shell_exit),
            (false, None),
            "draining before release: no shell exit shown"
        );
        h.ctl
            .cancel(job, Requester::Person, JobReason::PersonStop, false);
        assert_eq!(discards(&h), [job]);
        h.script(
            job,
            [SlotEvent::ProcessSettled {
                exit: Exit::Exited(0),
                cleanup: JobCleanup::Confirmed,
                escapees: 1,
                elapsed_ms: 9,
                output_held: true,
            }],
        );
        assert_eq!(shell_exits(&events(&mut h)).len(), 1);
        assert_eq!(closes(&h), [job]);
        h.script(job, [SlotEvent::CaptureSettled(closed_capture())]);
        let records = h.commit();
        let s = settled(&records)[0];
        assert_eq!(
            (s.outcome, s.reason, s.capture_reason),
            (
                Outcome::Exited,
                JobReason::PersonStop,
                CaptureReason::ClosedAfterCancel
            )
        );
    }

    /// A stop before the shell exits settles `cancelled`; when survivors
    /// then hold the output, the capture closes and kind 28 records reason
    /// 5 `discarded_after_cancel`, because replay allows reason 7 only
    /// with `exited` or `signalled`. A `!` job whose slot reported reason
    /// 7 records reason 5 too (PR3: reason 7 needs `person-outside-1`).
    #[test]
    fn capture_reason_7_only_where_replay_allows_it() {
        let mut h = Harness::new(limits(4, 2, 8));
        let outside = submit_outside(&mut h, 5, conversation(1));
        let Admission::Accepted { job: bang } = h.submit(person(6), conversation(2), P1) else {
            panic!("accepted");
        };
        h.commit();
        h.script(outside, [SlotEvent::Spawned]);
        h.ctl
            .cancel(outside, Requester::Person, JobReason::PersonStop, false);
        assert_eq!(h.stops(), [(outside, StopKind::Term)]);
        let mut events_ = shell_exit(Exit::Signalled(15), 1, true);
        events_.remove(0);
        h.script(outside, events_);
        assert_eq!(closes(&h), [outside]);
        h.script(outside, [SlotEvent::CaptureSettled(closed_capture())]);
        h.script(bang, settle_events(Exit::Exited(0), 0).into_iter().take(3));
        h.ctl
            .cancel(bang, Requester::Person, JobReason::PersonStop, false);
        h.script(bang, [SlotEvent::CaptureSettled(closed_capture())]);
        let records = h.commit();
        let by_job = |id: Id| {
            settled(&records)
                .into_iter()
                .find(|s| s.job == id)
                .unwrap()
                .clone()
        };
        let o = by_job(outside);
        assert_eq!(
            (o.outcome, o.reason, o.capture_reason),
            (
                Outcome::Cancelled,
                JobReason::PersonStop,
                CaptureReason::DiscardedAfterCancel
            )
        );
        let b = by_job(bang);
        assert_eq!(b.capture_reason, CaptureReason::DiscardedAfterCancel);
    }

    /// Recovery: an interrupted `!!` job's `job.settled` carries the flag,
    /// from the input's kind 31 (`recover_profiles`).
    #[test]
    fn recovered_outside_job_audit_carries_the_flag() {
        let mut h = Harness::unrecovered(limits(4, 2, 8));
        let mut u = unsettled(1, 10, false);
        u.origin = JobOrigin::Person { input: [1; 16] };
        u.job = journal::job_id(&u.origin);
        let mut bang = unsettled(2, 20, false);
        bang.origin = JobOrigin::Person { input: [2; 16] };
        bang.job = journal::job_id(&bang.origin);
        h.ctl
            .recover_profiles(vec![u.clone(), bang.clone()], &|input| input == [1; 16]);
        let records = h.commit();
        assert_eq!(kinds(&records), [28, 28]);
        assert_outside_audit(&h, u.job);
    }
}
