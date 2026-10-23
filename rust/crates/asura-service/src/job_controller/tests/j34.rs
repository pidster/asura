//! Unit tests of the J3 and J4 controller extensions with the fixture slot
//! and payload port: the unit parts of J3T-04 to J3T-10, J4T-04 and ES-05
//! (tool-job-controller.md, J3 and J4 validation;
//! execution-setting.md, validation). No test starts a thread, a process
//! or a timer; time moves only through the harness clock.
use super::*;
use asura_storage::authority::conversation::{
    JobBlockReason, JobPauseReason, JobRunState, JobScope, OutputStream,
};

fn waits(h: &mut Harness) -> Vec<WaitDone> {
    let mut out = Vec::new();
    h.ctl.drain_waits(&mut out);
    out
}
fn agent_done(agent: AgentKey, job: Id, end: WaitEnding) -> WaitDone {
    WaitDone {
        key: WaitKey::Agent(agent),
        job: Some(job),
        end,
    }
}
fn client_done(key: u64, job: Option<Id>, end: WaitEnding) -> WaitDone {
    WaitDone {
        key: WaitKey::Client(key),
        job,
        end,
    }
}
fn view(h: &Harness, job: Id) -> JobView {
    h.ctl.view(job, h.now).expect("view")
}
fn accepted(h: &mut Harness, n: u8, agent: AgentKey, project: Id) -> Id {
    let Admission::Accepted { job } = h.submit(model(n), agent, project) else {
        panic!("accepted");
    };
    job
}

// ----- J3T-04: yield window -----

/// J3T-04: the window starts when kind 26 commits (J3-E6), not at submit;
/// it ends with `Time` at `yield_seconds` after the commit and no earlier;
/// the reactor deadline names the window end.
#[test]
fn j3t_04_window_starts_at_kind_26_and_ends_on_time() {
    let mut h = Harness::new(limits(4, 2, 8));
    let a = conversation(1);
    let job = accepted(&mut h, 1, a, P1);
    assert_eq!(h.ctl.await_yield(a, job, 10), WaitStart::Parked);
    // Time passes before kind 26 commits: the window has not started.
    h.now += Duration::from_secs(60);
    h.poll();
    assert!(waits(&mut h).is_empty());
    assert_eq!(h.ctl.next_deadline(h.now), None);
    h.commit();
    let start = h.now;
    assert_eq!(
        h.ctl.next_deadline(h.now),
        Some(start + Duration::from_secs(10))
    );
    h.script(job, [SlotEvent::Spawned]);
    h.now = start + Duration::from_millis(9_999);
    h.poll();
    assert!(waits(&mut h).is_empty(), "the window ends only at its time");
    h.now = start + Duration::from_secs(10);
    h.poll();
    assert_eq!(waits(&mut h), [agent_done(a, job, WaitEnding::Time)]);
    // The window ended the wait, not the job.
    assert_eq!(h.state(job), Some(JobState::Running));
    assert!(h.stops().is_empty());
    assert_eq!(h.ctl.next_deadline(h.now), None);
}

/// J3T-04: kind 28 committed within the window ends it with `Settled`;
/// a kind 28 built but not committed does not (J3-E7).
#[test]
fn j3t_04_window_ends_when_kind_28_commits() {
    let mut h = Harness::new(limits(4, 2, 8));
    let a = conversation(1);
    let job = accepted(&mut h, 1, a, P1);
    h.ctl.await_yield(a, job, 10);
    h.commit();
    h.script(job, settle_events(Exit::Exited(0), 0));
    assert_eq!(h.state(job), Some(JobState::Recording));
    assert!(waits(&mut h).is_empty(), "kind 28 not yet committed");
    assert_eq!(view(&h, job).snapshot.state, JobRunState::Finalizing);
    let records = h.commit();
    assert_eq!(kinds(&records), [28]);
    assert_eq!(waits(&mut h), [agent_done(a, job, WaitEnding::Settled)]);
    assert!(view(&h, job).settled());
}

/// J3T-04: with `yield_seconds` 0 the window ends at spawn, or at once
/// for a job queued with a block reason; a job in setup with no block
/// reason is waited for until it spawns or fails setup.
#[test]
fn j3t_04_zero_seconds_returns_at_spawn_or_when_blocked() {
    let mut h = Harness::new(limits(4, 1, 8));
    let a = conversation(1);
    let first = accepted(&mut h, 1, a, P1);
    h.ctl.await_yield(a, first, 0);
    h.commit();
    assert_eq!(h.state(first), Some(JobState::Starting));
    assert!(waits(&mut h).is_empty(), "setup is waited for");
    h.now += Duration::from_secs(3600);
    h.poll();
    assert!(waits(&mut h).is_empty(), "no time bound during setup");
    h.script(first, [SlotEvent::Spawned]);
    assert_eq!(waits(&mut h), [agent_done(a, first, WaitEnding::Ready)]);

    // Queued behind the agent's one slot: blocked, so it returns at once.
    let second = accepted(&mut h, 2, a, P1);
    h.ctl.await_yield(a, second, 0);
    h.commit();
    assert_eq!(h.reason(second), Some(BlockReason::AgentSlots));
    assert_eq!(waits(&mut h), [agent_done(a, second, WaitEnding::Ready)]);

    // A kind 26 held at the journal quota shows `journal_held`, but the
    // window has not started: the call waits for the commit (J3-E6).
    let c = conversation(3);
    let held = accepted(&mut h, 4, c, P1);
    h.ctl.await_yield(c, held, 0);
    let (id, _) = h.ctl.next_record().expect("kind 26");
    h.ctl.record_done(id, Err(writer::Error::JournalDiskSpace));
    h.poll();
    assert_eq!(h.reason(held), Some(BlockReason::JournalHeld));
    assert!(waits(&mut h).is_empty(), "no window before kind 26 commits");
    h.ctl.journal_hold_ended();
    h.commit();
    assert_eq!(h.state(held), Some(JobState::Starting));
    h.script(held, [SlotEvent::Spawned]);
    assert_eq!(waits(&mut h), [agent_done(c, held, WaitEnding::Ready)]);

    // A setup failure settles the job: the window ends with `Settled`.
    let b = conversation(2);
    let failing = accepted(&mut h, 3, b, P1);
    h.ctl.await_yield(b, failing, 0);
    h.commit();
    h.script(
        failing,
        [
            SlotEvent::SetupFailed(JobReason::SpawnFailed),
            SlotEvent::CaptureSettled(capture(CaptureState::NotStarted, 0, 0)),
        ],
    );
    h.commit();
    assert_eq!(waits(&mut h), [agent_done(b, failing, WaitEnding::Settled)]);
}

/// J3T-04: a revocation does not end the window; the job's kind 28 does.
/// One wait per agent (AE-03); a job the controller never held ends at
/// once as `Withdrawn`.
#[test]
fn j3t_04_revocation_keeps_the_window_until_kind_28() {
    let mut h = Harness::new(limits(4, 2, 8));
    let a = conversation(1);
    let job = accepted(&mut h, 1, a, P1);
    assert_eq!(h.ctl.await_yield(a, job, 30), WaitStart::Parked);
    assert_eq!(h.ctl.await_yield(a, job, 30), WaitStart::Busy);
    assert_eq!(
        h.ctl.await_job(a, job, None, h.now),
        WaitStart::Busy,
        "one agent wait"
    );
    h.commit();
    h.script(job, [SlotEvent::Spawned]);
    h.ctl
        .revoke(Some(P1), OriginFilter::Model, JobReason::ExecutionOff);
    h.commit();
    assert!(waits(&mut h).is_empty(), "revocation alone ends nothing");
    h.script(
        job,
        settle_events(Exit::Signalled(15), 0).into_iter().skip(1),
    );
    h.commit();
    assert_eq!(waits(&mut h), [agent_done(a, job, WaitEnding::Settled)]);
    let b = conversation(2);
    assert_eq!(
        h.ctl.await_yield(b, [9; 16], 10),
        WaitStart::Done(WaitEnding::Withdrawn)
    );
}

// ----- J3T-05 and J3T-06: turn end -----

/// J3T-05: a turn that ends inside the yield window cancels the job with
/// requester model, reason `turn_ended` (TJ-04); the view after the
/// request is the state block of the kind 29 with status 6, and the
/// window produces no second end.
#[test]
fn j3t_05_turn_end_during_yield_cancels_the_job() {
    let mut h = Harness::new(limits(4, 2, 8));
    let a = conversation(1);
    let job = accepted(&mut h, 1, a, P1);
    h.ctl.await_yield(a, job, 10);
    h.commit();
    h.script(job, [SlotEvent::Spawned]);
    assert_eq!(
        h.ctl.end_turn_wait(a),
        Some(TurnWaitEnd::Yield {
            job,
            reply: CancelReply::Accepted(JobState::Stopping),
        })
    );
    assert_eq!(h.stops(), [(job, StopKind::Term)]);
    let v = view(&h, job);
    assert_eq!(v.snapshot.state, JobRunState::Stopping);
    assert_eq!(v.cancel, Some(JobReason::TurnEnded));
    let records = h.commit();
    let c = cancels(&records);
    assert_eq!(
        (c.len(), c[0].requester, c[0].reason),
        (1, JobRequester::Model, JobReason::TurnEnded)
    );
    h.script(
        job,
        settle_events(Exit::Signalled(15), 0).into_iter().skip(1),
    );
    h.commit();
    assert!(waits(&mut h).is_empty(), "the ended window reports nothing");
    assert_eq!(h.ctl.end_turn_wait(a), None);
}

/// J3T-06: after handle delivery the job survives the turn: a turn end
/// with no wait, or with a parked `job wait`, stops nothing (TJ-03).
#[test]
fn j3t_06_job_survives_the_turn_after_handle_delivery() {
    let mut h = Harness::new(limits(4, 2, 8));
    let a = conversation(1);
    let job = accepted(&mut h, 1, a, P1);
    h.ctl.await_yield(a, job, 1);
    h.commit();
    h.script(job, [SlotEvent::Spawned]);
    h.now += Duration::from_secs(1);
    h.poll();
    assert_eq!(waits(&mut h), [agent_done(a, job, WaitEnding::Time)]);
    // Handle delivered; the turn ends.
    assert_eq!(h.ctl.end_turn_wait(a), None);
    // A later turn's `job wait`, then that turn is cancelled.
    assert_eq!(h.ctl.await_job(a, job, None, h.now), WaitStart::Parked);
    assert_eq!(h.ctl.end_turn_wait(a), Some(TurnWaitEnd::Wait { job }));
    assert!(h.stops().is_empty());
    assert_eq!(h.state(job), Some(JobState::Running));
    assert!(h.commit().is_empty(), "no kind 27");
}

// ----- J3T-07 and J3T-08: state blocks -----

/// J3T-07: a queued job's state block carries its block reason and the
/// live numbers (running, limit, awaiting cleanup), with the brief text
/// (AQ1); `pending_installs` carries the count; a paused job carries its
/// pause reason.
#[test]
fn j3t_07_queued_state_block_reason_and_numbers() {
    let mut h = Harness::new(limits(4, 2, 8));
    let a = conversation(1);
    let one = h.model(1, a, P1);
    let two = h.model(2, a, P1);
    let three = h.model(3, a, P1);
    assert_eq!(h.started(), [one, two]);
    let v = view(&h, three);
    assert_eq!(
        v.snapshot.state,
        JobRunState::Queued(JobBlockReason::AgentSlots)
    );
    assert_eq!(v.snapshot.numbers, [2, 2, 0]);
    assert_eq!(
        v.text.as_deref(),
        Some("waiting: 2 of 2 jobs running for this agent")
    );
    assert_eq!((v.snapshot.elapsed_ms, v.snapshot.stdout_bytes), (0, 0));
    assert_eq!(v.list_entry().numbers, [2, 2, 0]);
    h.script(
        one,
        [
            SlotEvent::Spawned,
            SlotEvent::CleanupUnconfirmed(UnconfirmedReason::CleanupFailed),
        ],
    );
    assert_eq!(view(&h, three).snapshot.numbers, [2, 2, 1]);
    assert_eq!(
        view(&h, one).snapshot.state,
        JobRunState::CleanupUnconfirmed
    );

    // Pending installs: the count only.
    let mut h = Harness::new(limits(4, 2, 8));
    h.payloads.pending = PENDING_INSTALLS;
    let queued = h.model(1, a, P1);
    let v = view(&h, queued);
    assert_eq!(
        v.snapshot.state,
        JobRunState::Queued(JobBlockReason::PendingInstalls)
    );
    assert_eq!(v.snapshot.numbers, [4, 0, 0]);
    assert!(v.snapshot.state.allows_numbers(v.snapshot.numbers));

    // A paused job: `capture_paused` with `disk_space`, then `quota`.
    h.payloads.pending = 0;
    h.poll();
    h.script(
        queued,
        [
            SlotEvent::Spawned,
            SlotEvent::CapturePaused(capture_worker::PauseReason::Enospc),
        ],
    );
    assert_eq!(
        view(&h, queued).snapshot.state,
        JobRunState::CapturePaused(JobPauseReason::DiskSpace)
    );
    h.script(queued, [SlotEvent::CaptureResumed]);
    h.script(
        queued,
        [SlotEvent::CapturePaused(capture_worker::PauseReason::Quota)],
    );
    let v = view(&h, queued);
    assert_eq!(
        v.snapshot.state,
        JobRunState::CapturePaused(JobPauseReason::Quota)
    );
    assert_eq!(v.snapshot.numbers, [0; 3]);
}

/// J3T-08 (controller part): the state block's elapsed time runs from the
/// spawned notice; its byte counts are the stored bytes (watermark) the
/// payload owner reports, never lower than an earlier read; a settled
/// state block holds zeros (its facts come from kind 28).
#[test]
fn j3t_08_elapsed_and_stored_bytes() {
    let mut h = Harness::new(limits(4, 2, 8));
    let job = h.model(1, conversation(1), P1);
    assert_eq!(view(&h, job).snapshot.state, JobRunState::Starting);
    h.script(job, [SlotEvent::Spawned]);
    h.now += Duration::from_millis(12_400);
    h.payloads.stored.insert(job, [20_480, 7]);
    h.poll();
    let v = view(&h, job);
    assert_eq!(
        (
            v.snapshot.state,
            v.snapshot.elapsed_ms,
            v.snapshot.stdout_bytes,
            v.snapshot.stderr_bytes
        ),
        (JobRunState::Running, 12_400, 20_480, 7)
    );
    h.payloads.stored.insert(job, [10, 7]);
    h.poll();
    assert_eq!(view(&h, job).snapshot.stdout_bytes, 20_480, "monotonic");
    h.script(job, settle_events(Exit::Exited(0), 0).into_iter().skip(1));
    h.commit();
    let v = view(&h, job);
    assert_eq!(v.snapshot.state, JobRunState::Settled);
    assert_eq!(
        (
            v.snapshot.elapsed_ms,
            v.snapshot.stdout_bytes,
            v.snapshot.stderr_bytes
        ),
        (0, 0, 0)
    );
    assert_eq!(v.outcome, Some(Outcome::Exited));
    // After the slot is released the remembered job still has a view.
    h.script(job, [SlotEvent::ScratchCleaned]);
    h.idle(job);
    let v = view(&h, job);
    assert!(v.settled());
    assert_eq!((v.project, v.sequence.is_some()), (P1, true));
}

// ----- J3T-09: wait -----

/// J3T-09: `job wait` ends with `already_settled` at once for a settled
/// job, and otherwise with `settled`, `new_output` or `time`; with no
/// `seconds` only events end it; a job the controller never held is
/// `NotLive` (the caller reads the journal).
#[test]
fn j3t_09_wait_end_reasons() {
    let mut h = Harness::new(limits(4, 4, 8));
    let a = conversation(1);
    let job = h.model(1, a, P1);
    h.script(job, [SlotEvent::Spawned]);
    h.payloads.stored.insert(job, [5, 0]);
    h.poll();

    // New output on either stream past the values at the call.
    assert_eq!(h.ctl.await_job(a, job, None, h.now), WaitStart::Parked);
    assert_eq!(h.ctl.next_deadline(h.now), None, "no time without seconds");
    h.now += Duration::from_secs(7200);
    h.poll();
    assert!(waits(&mut h).is_empty(), "only events end it");
    h.payloads.stored.insert(job, [5, 1]);
    h.poll();
    assert_eq!(waits(&mut h), [agent_done(a, job, WaitEnding::NewOutput)]);

    // `seconds` passed since the call.
    assert_eq!(h.ctl.await_job(a, job, Some(3), h.now), WaitStart::Parked);
    assert_eq!(
        h.ctl.next_deadline(h.now),
        Some(h.now + Duration::from_secs(3))
    );
    h.now += Duration::from_secs(3);
    h.poll();
    assert_eq!(waits(&mut h), [agent_done(a, job, WaitEnding::Time)]);

    // Kind 28 committed while parked.
    assert_eq!(h.ctl.await_job(a, job, Some(60), h.now), WaitStart::Parked);
    h.script(job, settle_events(Exit::Exited(0), 0).into_iter().skip(1));
    h.commit();
    assert_eq!(waits(&mut h), [agent_done(a, job, WaitEnding::Settled)]);

    // Settled at the call, before and after the slot is released.
    assert_eq!(
        h.ctl.await_job(a, job, None, h.now),
        WaitStart::Done(WaitEnding::AlreadySettled)
    );
    h.script(job, [SlotEvent::ScratchCleaned]);
    h.idle(job);
    assert_eq!(
        h.ctl.await_job(a, job, None, h.now),
        WaitStart::Done(WaitEnding::AlreadySettled)
    );
    assert_eq!(h.ctl.await_job(a, [9; 16], None, h.now), WaitStart::NotLive);
    assert!(waits(&mut h).is_empty());
}

// ----- J3T-10: cancel -----

/// J3T-10 (controller part): the model's cancel writes `T` and one kind
/// 27 with requester model and `model_cancel`; a person job is
/// `not_permitted` for the model; a settled job replies with its outcome.
#[test]
fn j3t_10_model_cancel() {
    let mut h = Harness::new(limits(4, 4, 8));
    let job = h.model(1, conversation(1), P1);
    h.script(job, [SlotEvent::Spawned]);
    assert_eq!(
        h.ctl
            .cancel(job, Requester::Model, JobReason::ModelCancel, false),
        CancelReply::Accepted(JobState::Stopping)
    );
    assert_eq!(
        h.ctl
            .cancel(job, Requester::Model, JobReason::ModelCancel, false),
        CancelReply::Accepted(JobState::Stopping),
        "idempotent"
    );
    assert_eq!(h.stops(), [(job, StopKind::Term)]);
    let records = h.commit();
    let c = cancels(&records);
    assert_eq!(
        (c.len(), c[0].requester, c[0].reason),
        (1, JobRequester::Model, JobReason::ModelCancel)
    );
    h.script(
        job,
        settle_events(Exit::Signalled(15), 0).into_iter().skip(1),
    );
    h.commit();
    assert_eq!(
        h.ctl
            .cancel(job, Requester::Model, JobReason::ModelCancel, false),
        CancelReply::AlreadySettled(Outcome::Cancelled)
    );

    let Admission::Accepted { job: person_job } = h.submit(person(5), conversation(1), P1) else {
        panic!("accepted");
    };
    h.commit();
    assert_eq!(
        h.ctl
            .cancel(person_job, Requester::Model, JobReason::ModelCancel, false),
        CancelReply::NotPermitted
    );
}

// ----- J4T-04: parked client requests -----

/// J4T-04: a parked observe ends on new bytes past the client's offset,
/// on a state change, or at settlement, and at once when one holds
/// already; with `at_end` and no stream, on either stream's new bytes.
/// No timer is involved.
#[test]
fn j4t_04_observe_ends_on_bytes_state_and_settlement() {
    let mut h = Harness::new(limits(4, 4, 8));
    let job = h.model(1, conversation(1), P1);
    h.script(job, [SlotEvent::Spawned]);
    h.payloads.stored.insert(job, [100, 0]);
    h.poll();
    let running = Some(JobRunState::Running);

    // At once: the client holds fewer bytes than are stored.
    assert_eq!(
        h.ctl
            .observe(1, job, Some((OutputStream::Stdout, 50)), running),
        WaitStart::Done(WaitEnding::NewOutput)
    );
    // At once: the state differs from the client's.
    assert_eq!(
        h.ctl.observe(
            1,
            job,
            Some((OutputStream::Stdout, 100)),
            Some(JobRunState::Starting)
        ),
        WaitStart::Done(WaitEnding::StateChanged)
    );
    // Parked on stdout at 100; stderr bytes do not end it.
    assert_eq!(
        h.ctl
            .observe(1, job, Some((OutputStream::Stdout, 100)), running),
        WaitStart::Parked
    );
    assert_eq!(h.ctl.next_deadline(h.now), None, "no hold time");
    h.payloads.stored.insert(job, [100, 9]);
    h.now += Duration::from_secs(86_400);
    h.poll();
    assert!(waits(&mut h).is_empty());
    h.payloads.stored.insert(job, [101, 9]);
    h.poll();
    assert_eq!(
        waits(&mut h),
        [client_done(1, Some(job), WaitEnding::NewOutput)]
    );

    // `at_end` with no stream: either stream past its present bytes.
    assert_eq!(h.ctl.observe(2, job, None, running), WaitStart::Parked);
    h.payloads.stored.insert(job, [101, 10]);
    h.poll();
    assert_eq!(
        waits(&mut h),
        [client_done(2, Some(job), WaitEnding::NewOutput)]
    );

    // A state change.
    assert_eq!(h.ctl.observe(3, job, None, running), WaitStart::Parked);
    h.script(job, [SlotEvent::LeaderExited(Exit::Exited(0))]);
    assert_eq!(
        waits(&mut h),
        [client_done(3, Some(job), WaitEnding::StateChanged)]
    );

    // Settlement, then at once after it.
    let draining = Some(JobRunState::Draining);
    assert_eq!(h.ctl.observe(4, job, None, draining), WaitStart::Parked);
    h.script(job, settle_events(Exit::Exited(0), 0).into_iter().skip(2));
    let finalizing = Some(JobRunState::Finalizing);
    // The state passed through finalizing: the observe ended on it.
    assert_eq!(
        waits(&mut h),
        [client_done(4, Some(job), WaitEnding::StateChanged)]
    );
    assert_eq!(h.ctl.observe(5, job, None, finalizing), WaitStart::Parked);
    h.commit();
    assert_eq!(
        waits(&mut h),
        [client_done(5, Some(job), WaitEnding::Settled)]
    );
    assert_eq!(
        h.ctl.observe(6, job, None, None),
        WaitStart::Done(WaitEnding::Settled)
    );
    assert_eq!(h.ctl.observe(6, [9; 16], None, None), WaitStart::NotLive);
}

/// J4T-04: a parked listing ends when the revision differs; detach drops
/// a parked request with no reply; more than `CLIENT_WAITS` parked client
/// requests are `Busy`; shutdown answers the rest with `Closed`.
#[test]
fn j4t_04_list_revision_detach_bound_and_shutdown() {
    let mut h = Harness::new(limits(4, 4, 8));
    let revision = h.ctl.revision();
    assert_eq!(
        h.ctl.park_list(1, revision - 1),
        WaitStart::Done(WaitEnding::Revision(revision))
    );
    assert_eq!(h.ctl.park_list(1, revision), WaitStart::Parked);
    h.now += Duration::from_secs(86_400);
    h.poll();
    assert!(waits(&mut h).is_empty(), "no hold time");
    let job = h.model(1, conversation(1), P1);
    let done = waits(&mut h);
    assert!(matches!(
        done.as_slice(),
        [WaitDone {
            key: WaitKey::Client(1),
            job: None,
            end: WaitEnding::Revision(r),
        }] if *r > revision
    ));

    // The owner's retention and execution changes bump it too.
    let revision = h.ctl.revision();
    h.ctl.park_list(2, revision);
    h.ctl.bump_revision();
    h.poll();
    assert_eq!(
        waits(&mut h),
        [client_done(2, None, WaitEnding::Revision(revision + 1))]
    );

    // Detach: removed without a reply.
    let revision = h.ctl.revision();
    h.ctl.park_list(3, revision);
    assert!(h.ctl.cancel_wait(WaitKey::Client(3)));
    assert!(!h.ctl.cancel_wait(WaitKey::Client(3)));
    h.ctl.bump_revision();
    h.poll();
    assert!(waits(&mut h).is_empty());

    // The client bound, then shutdown.
    let revision = h.ctl.revision();
    for key in 0..CLIENT_WAITS as u64 {
        assert_eq!(h.ctl.park_list(100 + key, revision), WaitStart::Parked);
    }
    assert_eq!(h.ctl.park_list(999, revision), WaitStart::Busy);
    assert_eq!(
        h.ctl.park_list(100, revision),
        WaitStart::Parked,
        "a repeated key replaces its request"
    );
    h.script(job, [SlotEvent::Spawned]);
    assert_eq!(
        h.ctl.await_job(conversation(1), job, None, h.now),
        WaitStart::Parked
    );
    let mut parked = waits(&mut h);
    parked.clear();
    // Make the revision stable for the shutdown check.
    let revision = h.ctl.revision();
    for key in 0..CLIENT_WAITS as u64 {
        h.ctl.park_list(100 + key, revision);
    }
    h.ctl.close();
    let done = waits(&mut h);
    assert_eq!(done.len(), CLIENT_WAITS);
    assert!(done.iter().all(|d| d.end == WaitEnding::Closed));
    // The agent's wait ends with its job, not with shutdown.
    h.script(
        job,
        settle_events(Exit::Signalled(15), 0).into_iter().skip(1),
    );
    h.commit();
    assert_eq!(
        waits(&mut h),
        [agent_done(conversation(1), job, WaitEnding::Settled)]
    );
}

// ----- J4 listings: live views, counts and revision -----

/// J4 (controller part of J4T-03 and J4T-07): live jobs of a scope newest
/// first by kind 26 sequence, with live counts per scope and for the
/// service; settled jobs leave the live set; the revision increments on
/// admission, state changes and settlement.
#[test]
fn j4_live_views_counts_and_revision() {
    let mut h = Harness::new(limits(8, 4, 8));
    let r0 = h.ctl.revision();
    assert!(r0 >= 1);
    let a = h.model(1, conversation(1), P1);
    let b = h.model(2, conversation(2), P1);
    let c = h.model(3, conversation(3), P2);
    let r1 = h.ctl.revision();
    assert!(r1 > r0);
    let ids = |views: Vec<JobView>| views.into_iter().map(|v| v.job).collect::<Vec<_>>();
    assert_eq!(ids(h.ctl.live_views(None, h.now)), [c, b, a]);
    assert_eq!(
        ids(h.ctl.live_views(Some(JobScope::Project(P1)), h.now)),
        [b, a]
    );
    assert_eq!(
        ids(h
            .ctl
            .live_views(Some(JobScope::Conversation([2; 16])), h.now)),
        [b]
    );
    assert_eq!(h.ctl.live_count(None), 3);
    assert_eq!(h.ctl.live_count(Some(JobScope::AllProjects)), 3);
    assert_eq!(h.ctl.live_count(Some(JobScope::Project(P2))), 1);
    let v = view(&h, a);
    assert!(v.sequence.is_some() && v.intent_ms > 0);
    // A start whose kind 26 is not committed is not listed.
    h.submit(model(4), conversation(1), P1);
    assert_eq!(h.ctl.live_count(None), 3);
    h.commit();
    assert_eq!(h.ctl.live_count(None), 4);
    h.script(a, settle_events(Exit::Exited(0), 0));
    let r2 = h.ctl.revision();
    assert!(r2 > r1);
    h.commit();
    assert!(h.ctl.revision() > r2, "settlement");
    assert_eq!(h.ctl.live_count(Some(JobScope::Project(P1))), 2);
    assert!(!ids(h.ctl.live_views(None, h.now)).contains(&a));
}

// ----- ES-05: execution setting revocation and the grant check -----

/// ES-05: revoking a project's model jobs with `execution_off` spares
/// person jobs and other projects; a queued model start settles
/// `not_started` with kinds 27 and 28, a running one gets `T`; one kind
/// 27 each, requester service. A global change reaches every project.
#[test]
fn es_05_revocation_by_project_and_origin() {
    let mut h = Harness::new(limits(8, 2, 8));
    let a1 = h.model(1, conversation(1), P1);
    let a2 = h.model(2, conversation(1), P1);
    let queued = h.model(3, conversation(1), P1);
    let Admission::Accepted { job: person_job } = h.submit(person(5), conversation(2), P1) else {
        panic!("accepted");
    };
    h.commit();
    let b = h.model(4, conversation(3), P2);
    for job in [a1, a2, person_job, b] {
        h.script(job, [SlotEvent::Spawned]);
    }
    assert_eq!(h.reason(queued), Some(BlockReason::AgentSlots));

    h.ctl
        .revoke(Some(P1), OriginFilter::Model, JobReason::ExecutionOff);
    assert_eq!(h.stops(), [(a1, StopKind::Term), (a2, StopKind::Term)]);
    let records = h.commit();
    let c = cancels(&records);
    assert_eq!(c.len(), 3);
    assert!(c.iter().all(|c| c.requester == JobRequester::Service
        && c.reason == JobReason::ExecutionOff
        && [a1, a2, queued].contains(&c.job)));
    let s = settled(&records);
    assert_eq!(
        (s.len(), s[0].job, s[0].outcome, s[0].reason),
        (1, queued, Outcome::NotStarted, JobReason::ExecutionOff)
    );
    assert_eq!(h.state(person_job), Some(JobState::Running));
    assert_eq!(h.state(b), Some(JobState::Running));
    // The running jobs settle with the reason.
    h.script(
        a1,
        settle_events(Exit::Signalled(15), 0).into_iter().skip(1),
    );
    let records = h.commit();
    let s = settled(&records)[0];
    assert_eq!(
        (s.outcome, s.reason),
        (Outcome::Cancelled, JobReason::ExecutionOff)
    );

    // Global off: every project's model jobs; the person job continues.
    h.ctl
        .revoke(None, OriginFilter::Model, JobReason::ExecutionOff);
    assert!(h.stops().contains(&(b, StopKind::Term)));
    assert_eq!(h.state(person_job), Some(JobState::Running));
    // A non-service reason revokes nothing.
    let mut h2 = Harness::new(limits(8, 2, 8));
    let x = h2.model(1, conversation(1), P1);
    h2.script(x, [SlotEvent::Spawned]);
    h2.ctl
        .revoke(None, OriginFilter::Model, JobReason::ModelCancel);
    assert_eq!(h2.state(x), Some(JobState::Running));
}

/// ES-05 (grant check): a model-origin head whose project has execution
/// off at slot grant settles `not_started` with `execution_off` (kinds 27
/// and 28) and takes no slot or reservation; a person job of the project
/// and another project's model job still start.
#[test]
fn es_05_execution_checked_at_slot_grant() {
    let mut h = Harness::new(limits(8, 4, 8));
    let off = std::rc::Rc::new(std::cell::Cell::new(false));
    let flag = off.clone();
    h.ctl
        .set_execution(Box::new(move |project| !(flag.get() && project == P1)));
    let first = h.model(1, conversation(1), P1);
    assert_eq!(h.started(), [first]);
    off.set(true);
    let Admission::Accepted { job } = h.submit(model(2), conversation(1), P1) else {
        panic!("accepted");
    };
    let records = h.commit();
    assert_eq!(kinds(&records), [26, 27, 28]);
    let s = settled(&records)[0];
    assert_eq!(
        (s.job, s.outcome, s.reason),
        (job, Outcome::NotStarted, JobReason::ExecutionOff)
    );
    assert_eq!(cancels(&records)[0].requester, JobRequester::Service);
    assert_eq!(h.started(), [first]);
    assert!(!h.payloads.admitted.contains(&job), "no reservation");
    let other = h.model(3, conversation(2), P2);
    let Admission::Accepted { job: person_job } = h.submit(person(6), conversation(3), P1) else {
        panic!("accepted");
    };
    h.commit();
    assert_eq!(h.started(), [first, other, person_job]);
}

// ----- TJ-Q9: disk-space recheck -----

/// TJ-Q9 (J34-K unit): each paced recheck that finds the head still
/// blocked by `disk_space` asks the payload owner to measure free space
/// again, once per recheck; a head blocked by `quota` or any other reason
/// never asks, and neither does a poll between rechecks. A changed space
/// epoch re-evaluates the head at once.
#[test]
fn tj_q9_refresh_space_only_while_blocked_by_disk_space() {
    let mut h = Harness::new(limits(4, 2, 8));
    h.payloads.short = Some(Short::DiskSpace);
    let job = h.model(1, conversation(1), P1);
    assert_eq!(h.reason(job), Some(BlockReason::DiskSpace));
    assert_eq!(h.payloads.refreshes, 0, "not before the first recheck");
    for n in 1..=3 {
        let at = h.ctl.next_deadline(h.now).expect("recheck");
        h.now = at - Duration::from_millis(1);
        h.poll();
        assert_eq!(h.payloads.refreshes, n - 1, "not between rechecks");
        h.now = at;
        h.poll();
        assert_eq!(h.payloads.refreshes, n, "once per recheck");
    }
    // The measurement found space: the epoch changes and the head starts
    // with no recheck.
    h.payloads.short = None;
    h.payloads.epoch += 1;
    h.poll();
    assert_eq!(h.started(), [job]);
    assert_eq!(h.payloads.refreshes, 3);
    assert_eq!(h.ctl.next_deadline(h.now), None);

    // `quota` never asks, and has no paced recheck.
    let mut h = Harness::new(limits(4, 2, 8));
    h.payloads.short = Some(Short::Quota);
    let job = h.model(1, conversation(1), P1);
    assert_eq!(h.reason(job), Some(BlockReason::Quota));
    assert_eq!(h.ctl.next_deadline(h.now), None);
    h.now += Duration::from_secs(600);
    h.poll();
    assert_eq!(h.payloads.refreshes, 0);

    // A head that turns from `disk_space` to `quota` stops asking.
    let mut h = Harness::new(limits(4, 2, 8));
    h.payloads.short = Some(Short::DiskSpace);
    let job = h.model(1, conversation(1), P1);
    h.payloads.short = Some(Short::Quota);
    h.now = h.ctl.next_deadline(h.now).expect("recheck");
    h.poll();
    assert_eq!(h.reason(job), Some(BlockReason::Quota));
    assert_eq!(h.payloads.refreshes, 0);
}

/// The pacing type that the owner's limits-read retry reuses (TJ-F5):
/// 1, 1, 2, 3, 5, 8 s and so on, at most 60 s.
#[test]
fn pacing_steps() {
    let t = Instant::now();
    let mut p = Pacing::start(t);
    let mut steps = vec![(p.at() - t).as_secs()];
    for _ in 0..11 {
        let now = p.at();
        p.advance(now);
        steps.push((p.at() - now).as_secs());
    }
    assert_eq!(steps, [1, 1, 2, 3, 5, 8, 13, 21, 34, 55, 60, 60]);
}
