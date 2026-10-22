//! J4T-11 (worker part): slots, caches, prefix resolution, age text, notice
//! text and status text. Slot threads here are fixtures; the real service
//! path is the J34-E integration and PTY journeys.
use super::*;

fn entry(id: u8, state: pb::JobState) -> pb::JobEntry {
    let mut job = [id; 16];
    job[15] = 1;
    pb::JobEntry {
        job_id: Some(job.to_vec()),
        project_id: Some(vec![1; 16]),
        state: Some(state as i32),
        ..Default::default()
    }
}
fn with_id(hex: &str) -> pb::JobEntry {
    pb::JobEntry {
        job_id: Some(crate::conversation::parse_id(hex).unwrap().to_vec()),
        ..Default::default()
    }
}
fn request(command: Command) -> Request {
    Request {
        epoch: "e".into(),
        command,
        draft: None,
    }
}
fn read() -> Command {
    Command::Read {
        job: [5; 16],
        project: None,
        stream: Stream::Stdout,
        offset: Some(0),
    }
}
fn cancel() -> Command {
    Command::Cancel {
        request_id: [9; 16],
        job: [5; 16],
        project: [1; 16],
        kill: false,
    }
}
fn finished(job: &RequestJob) -> Finished {
    Finished {
        request: job.request.clone(),
        result: Ok(Reply::List(pb::JobListReply::default())),
    }
}
fn wait(handle: &JoinHandle<impl Sized>) {
    let end = Instant::now() + Duration::from_secs(2);
    while !handle.is_finished() {
        assert!(Instant::now() < end, "fixture thread did not settle");
        std::thread::yield_now();
    }
}
fn held(command: Command) -> (Worker, std::sync::mpsc::Sender<()>) {
    let (release, hold) = std::sync::mpsc::channel::<()>();
    let sent = request(command);
    let reply = sent.clone();
    let handle = std::thread::spawn(move || {
        hold.recv().unwrap();
        Finished {
            request: reply,
            result: Ok(Reply::List(pb::JobListReply::default())),
        }
    });
    let mut worker = Worker::default();
    worker.request = Some(RequestJob {
        handle,
        cancel: Arc::new(Cancellation::default()),
        request: sent,
        reported: false,
    });
    (worker, release)
}

#[test]
fn j4t_11_request_slot_holds_one_request_and_cancel_reports_at_once() {
    // CW-01: a held reply stays pending with no timer; the slot is busy.
    let (mut worker, release) = held(read());
    std::thread::sleep(Duration::from_millis(50));
    assert!(worker.poll_request().is_none());
    assert!(worker.request_busy() && worker.running());
    assert_eq!(
        worker.submit(|_| panic!("busy slot sends nothing"), request(read())),
        Err("Job request pending; draft retained")
    );
    // CW-02: the person's cancel reports a read as cancelled at once; the
    // slot stays busy until the thread settles, and the late reply is dropped.
    worker.cancel_request();
    let reported = worker.poll_request().unwrap();
    assert_eq!(reported.result.unwrap_err(), Failure::Cancelled);
    assert!(worker.request_busy());
    assert!(worker.poll_request().is_none());
    release.send(()).unwrap();
    wait(&worker.request.as_ref().unwrap().handle);
    assert!(worker.poll_request().is_none(), "late reply discarded");
    assert!(!worker.request_busy());

    // A stopped mutation is outcome unconfirmed, never a refusal.
    let (mut worker, release) = held(cancel());
    worker.cancel_request();
    assert_eq!(
        worker.poll_request().unwrap().result.unwrap_err(),
        Failure::Unconfirmed
    );
    release.send(()).unwrap();
    wait(&worker.request.as_ref().unwrap().handle);
    assert!(worker.poll_request().is_none());

    // A reply that arrives first is returned once.
    let (mut worker, release) = held(read());
    release.send(()).unwrap();
    wait(&worker.request.as_ref().unwrap().handle);
    let job = worker.request.as_ref().unwrap();
    let expected = finished(job).request;
    let got = worker.poll_request().unwrap();
    assert_eq!(got.request, expected);
    assert!(matches!(got.result, Ok(Reply::List(_))));
    assert!(!worker.request_busy());
}

#[test]
fn j4t_11_cancellation_disconnects_a_pending_transport() {
    use std::io::Read;
    let (client, mut peer) = UnixStream::pair().unwrap();
    peer.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
    let cancel = Cancellation {
        stopped: AtomicBool::new(false),
        socket: Mutex::new(Some(client)),
    };
    cancel.cancel();
    assert!(cancel.stopped());
    assert_eq!(peer.read(&mut [0; 1]).unwrap(), 0);
}

fn scope(epoch: &str) -> Scope {
    Scope {
        project: [1; 16],
        epoch: epoch.into(),
    }
}
fn status_job(scope: Scope, update: Option<StatusUpdate>) -> StatusJob {
    let handle = std::thread::spawn(|| {});
    wait(&handle);
    StatusJob {
        scope,
        cancel: Arc::new(Cancellation::default()),
        latest: Arc::new(Mutex::new(update)),
        handle,
    }
}

#[test]
fn j4t_11_status_slot_parks_per_scope_paces_retry_and_remembers_no_capability() {
    let now = Instant::now();
    // An unread reply keeps the finished slot until it is polled.
    let mut worker = Worker::default();
    worker.status = Some(status_job(
        scope("e"),
        Some(StatusUpdate {
            scope: scope("e"),
            result: Ok(pb::JobListReply {
                live_in_scope: Some(2),
                revision: Some(4),
                ..Default::default()
            }),
        }),
    ));
    worker
        .synchronize_status(|_| panic!("no attach"), Some(scope("e")), now)
        .unwrap();
    assert!(worker.running());
    let update = worker.poll_status().unwrap();
    assert_eq!(update.result.unwrap().live_in_scope, Some(2));
    // Reaped: the same scope waits for the retry pacing; nothing attaches.
    worker
        .synchronize_status(|_| panic!("paced"), Some(scope("e")), now)
        .unwrap();
    assert!(worker.status.is_none());
    assert_eq!(worker.next_deadline(), Some(now + STATUS_RETRY));
    worker
        .synchronize_status(|_| panic!("paced"), Some(scope("e")), now)
        .unwrap();
    assert!(worker.status.is_none());

    // A scope change cancels the parked request; no scope clears pacing.
    let mut worker = Worker::default();
    worker.status = Some(status_job(scope("e"), None));
    worker
        .synchronize_status(|_| panic!("no scope"), None, now)
        .unwrap();
    assert!(worker.status.is_none());
    assert_eq!(worker.next_deadline(), None);

    // No CAPABILITY_JOBS: remembered for the epoch; no further attach.
    let mut worker = Worker::default();
    worker.status = Some(status_job(
        scope("old"),
        Some(StatusUpdate {
            scope: scope("old"),
            result: Err(Failure::Unsupported),
        }),
    ));
    assert_eq!(
        worker.poll_status().unwrap().result.unwrap_err(),
        Failure::Unsupported
    );
    worker
        .synchronize_status(|_| panic!("unsupported epoch"), Some(scope("old")), now)
        .unwrap();
    worker
        .synchronize_status(
            |_| panic!("unsupported epoch"),
            Some(scope("old")),
            now + Duration::from_secs(5),
        )
        .unwrap();
    assert!(worker.status.is_none());
    assert_eq!(worker.next_deadline(), None);
    assert_eq!(worker.unsupported.as_deref(), Some("old"));
}

fn follow(session: u64) -> Follow {
    Follow {
        session,
        epoch: "e".into(),
        job: [5; 16],
        project: None,
        stream: Stream::Stdout,
        offset: 0,
        after_state: Some(pb::JobState::Running as i32),
    }
}

#[test]
fn j4t_11_observe_slot_follows_one_session_and_never_restarts_an_ended_one() {
    let (release, hold) = std::sync::mpsc::channel::<()>();
    let cancel = Arc::new(Cancellation::default());
    let mut worker = Worker::default();
    worker.follow = Some(FollowJob {
        follow: follow(1),
        cancel: cancel.clone(),
        latest: Arc::new(Mutex::new(Some(FollowUpdate {
            session: 1,
            result: Ok(pb::JobPage::default()),
        }))),
        handle: std::thread::spawn(move || {
            let _ = hold.recv();
        }),
    });
    // The same session keeps its observe in flight.
    worker
        .synchronize_follow(|_| panic!("in flight"), Some(follow(1)))
        .unwrap();
    assert!(!cancel.stopped());
    assert_eq!(worker.poll_follow().unwrap().session, 1);
    // Closing the viewer (no session) cancels the wait; the job is untouched.
    worker
        .synchronize_follow(|_| panic!("closed"), None)
        .unwrap();
    assert!(cancel.stopped());
    assert!(
        worker.poll_follow().is_none(),
        "a stopped session publishes nothing"
    );
    release.send(()).unwrap();
    wait(&worker.follow.as_ref().unwrap().handle);
    worker
        .synchronize_follow(|_| panic!("closed"), None)
        .unwrap();
    assert!(!worker.running());

    // A session whose observe ended is reaped and not started again.
    let handle = std::thread::spawn(|| {});
    wait(&handle);
    let mut worker = Worker::default();
    worker.follow = Some(FollowJob {
        follow: follow(2),
        cancel: Arc::new(Cancellation::default()),
        latest: Arc::new(Mutex::new(None)),
        handle,
    });
    worker
        .synchronize_follow(|_| panic!("ended session restarted"), Some(follow(2)))
        .unwrap();
    assert!(worker.follow.is_none());
    worker
        .synchronize_follow(|_| panic!("ended session restarted"), Some(follow(2)))
        .unwrap();
    assert!(!worker.running());
}

#[test]
fn j4t_11_follow_reads_the_end_when_more_than_a_page_behind() {
    let page = |next: u64, total: u64| pb::JobPage {
        pages: vec![pb::JobStreamPage {
            next_offset: Some(next),
            total_bytes: Some(total),
            ..Default::default()
        }],
        ..Default::default()
    };
    assert!(!behind(&page(8192, 8192)));
    assert!(!behind(&page(8192, 16_384)));
    assert!(behind(&page(8192, 16_385)));
    assert!(!behind(&pb::JobPage::default()));
}

#[test]
fn j4t_11_prefix_resolution_accepts_full_handles_and_unique_prefixes() {
    let a = "abcdef0123456789abcdef0123456789";
    let b = "abcdef9999999999abcdef0123456789";
    let c = "123456aaaaaaaaaaaaaaaaaaaaaaaaaa";
    let cache = vec![with_id(a), with_id(b), with_id(c)];
    let id = |hex| crate::conversation::parse_id(hex).unwrap();
    // The full handle needs no cached entry.
    assert_eq!(
        resolve("00000000000000000000000000000077", &cache),
        Resolution::Job(id("00000000000000000000000000000077"))
    );
    assert_eq!(resolve(&a.to_uppercase(), &cache), Resolution::Job(id(a)));
    // A unique prefix of at least 6 digits among cached entries.
    assert_eq!(resolve("123456", &cache), Resolution::Job(id(c)));
    assert_eq!(resolve("abcdef0", &cache), Resolution::Job(id(a)));
    assert_eq!(
        resolve("abcdef", &cache),
        Resolution::Ambiguous(vec![a.into(), b.into()])
    );
    assert_eq!(resolve("fedcba", &cache), Resolution::NoMatch);
    for bad in [
        "12345",
        "zzzzzz",
        "",
        "abcdef0123456789abcdef01234567890",
        "00000000000000000000000000000000",
    ] {
        assert_eq!(resolve(bad, &cache), Resolution::Malformed, "{bad}");
    }
}

#[test]
fn j4t_11_cache_keeps_64_newest_entries_without_duplicates() {
    let mut cache = Vec::new();
    for n in 0..70u8 {
        remember(&mut cache, &entry(n, pb::JobState::Running));
    }
    assert_eq!(cache.len(), MAX_CACHED_ENTRIES);
    assert_eq!(
        entry_id(&cache[0]),
        entry_id(&entry(69, pb::JobState::Running))
    );
    // A newer copy replaces the older one and moves to the front.
    let mut settled = entry(30, pb::JobState::Settled);
    settled.outcome = Some(1);
    remember(&mut cache, &settled);
    assert_eq!(cache.len(), MAX_CACHED_ENTRIES);
    assert_eq!(cache[0].state, Some(pb::JobState::Settled as i32));
    assert_eq!(
        cache
            .iter()
            .filter(|e| entry_id(e) == entry_id(&settled))
            .count(),
        1
    );
    // An entry without an ID is not remembered.
    remember(&mut cache, &pb::JobEntry::default());
    assert_eq!(cache.len(), MAX_CACHED_ENTRIES);
}

#[test]
fn j4t_11_age_text_uses_intent_or_settlement_time() {
    let now = 10_000_000_000;
    let mut job = entry(1, pb::JobState::Running);
    assert_eq!(age(&job, now), "—");
    job.intent_unix_ms = Some(now - 12_400);
    assert_eq!(age(&job, now), "12 s");
    job.intent_unix_ms = Some(now - 4 * 60_000 - 59_000);
    assert_eq!(age(&job, now), "4 min");
    job.intent_unix_ms = Some(now - 3 * 3_600_000);
    assert_eq!(age(&job, now), "3 h");
    job.intent_unix_ms = Some(now - 2 * 86_400_000 - 1);
    assert_eq!(age(&job, now), "2 d");
    // A settled job ages from its settlement; 0 shows a dash.
    job.state = Some(pb::JobState::Settled as i32);
    assert_eq!(age(&job, now), "—");
    job.settled_unix_ms = Some(now - 5_000);
    assert_eq!(age(&job, now), "5 s");
    // A clock behind the record shows 0, never a negative age.
    assert_eq!(ago(now + 5_000, now), "0 s");
}

#[test]
fn j4t_11_state_and_result_text() {
    use pb::JobState as S;
    let mut queued = entry(1, S::Queued);
    queued.block_reason = Some(2);
    queued.reason_text = Some("waiting: 2 of 2 jobs running for this agent".into());
    // J34E-06 shows this text in /jobs.
    assert_eq!(
        state_text(&queued),
        "queued (waiting: 2 of 2 jobs running for this agent)"
    );
    let mut paused = entry(1, S::CapturePaused);
    paused.block_reason = Some(2);
    assert_eq!(state_text(&paused), "paused (quota)");
    paused.block_reason = Some(3);
    assert_eq!(state_text(&paused), "paused (enospc)");
    let mut running = entry(1, S::Running);
    assert_eq!(state_text(&running), "running");
    running.cancel_requested = Some(true);
    assert_eq!(state_text(&running), "running · cancel requested");
    for (state, text) in [
        (S::Starting, "starting"),
        (S::Stopping, "stopping"),
        (S::Draining, "draining"),
        (S::CleanupUnconfirmed, "cleanup unconfirmed"),
        (S::Finalizing, "finalizing"),
    ] {
        assert_eq!(state_text(&entry(1, state)), text);
    }
    let settled = |outcome, reason, kind, value| pb::JobEntry {
        outcome: Some(outcome),
        reason: Some(reason),
        exit_kind: Some(kind),
        exit_value: Some(value),
        cancel_requested: Some(true),
        ..entry(1, S::Settled)
    };
    assert_eq!(state_text(&settled(1, 0, 1, 0)), "exit 0");
    assert_eq!(state_text(&settled(2, 0, 2, 9)), "signal 9");
    assert_eq!(
        state_text(&settled(1, 19, 1, 143)),
        "exit 143 · execution_off"
    );
    assert_eq!(
        state_text(&settled(3, 3, 2, 15)),
        "cancelled · signal 15 · client_cancel"
    );
    assert_eq!(
        state_text(&settled(4, 19, 0, 0)),
        "not started · execution_off"
    );
    assert_eq!(state_text(&settled(5, 11, 0, 0)), "interrupted");
    assert_eq!(state_text(&pb::JobEntry::default()), "unknown");
    for code in 0..=20 {
        assert_ne!(reason_name(code), "unknown", "{code}");
    }
    assert_eq!(reason_name(21), "unknown");
}

#[test]
fn j4t_11_notice_status_exit_and_error_text() {
    // EX12 (J4-E6).
    assert_eq!(outlived_notice(0), None);
    assert_eq!(
        outlived_notice(1).unwrap(),
        "↳ 1 job kept running after this turn · /jobs to view or cancel"
    );
    assert_eq!(
        outlived_notice(2).unwrap(),
        "↳ 2 jobs kept running after this turn · /jobs to view or cancel"
    );
    // Status row (J4 TUI, ES-12).
    assert_eq!(status_text(0, Some(true)), "");
    assert_eq!(status_text(0, None), "");
    assert_eq!(status_text(3, Some(true)), "jobs 3");
    assert_eq!(status_text(0, Some(false)), "exec off");
    assert_eq!(status_text(2, Some(false)), "jobs 2 · exec off");
    // Exit confirmation.
    assert_eq!(exit_line(0), None);
    assert_eq!(
        exit_line(1).unwrap(),
        "1 job is still running in all projects; it stops when the service stops."
    );
    assert_eq!(
        exit_line(4).unwrap(),
        "4 jobs are still running in all projects; they stop when the service stops."
    );
    // Every wire token has its own text; unknown text is shown literally.
    use limits::error as e;
    let tokens = [
        e::NOT_FOUND,
        e::PROJECT_MISMATCH,
        e::KILL_NOT_PERMITTED,
        e::NOT_SETTLED,
        e::NOTES_FULL,
        e::RETENTION_FULL,
        e::NOTE_INVALID,
        e::BUSY,
        e::UNAVAILABLE,
        e::INVALID_CURSOR,
    ];
    for token in tokens {
        assert_ne!(error_text(token), token, "{token}");
    }
    assert_eq!(error_text("Draining\u{7}"), "Draining ");
    assert_eq!(
        error_text(e::NOT_SETTLED),
        "a running job's output cannot be deleted"
    );
}

#[test]
fn j4t_11_marks_bytes_page_states_and_handles() {
    let mut job = entry(1, pb::JobState::Running);
    assert_eq!(origin_mark(&job), "");
    job.origin = Some(pb::JobOrigin::Person as i32);
    assert_eq!(origin_mark(&job), "!");
    job.outside_sandbox = Some(true);
    assert_eq!(origin_mark(&job), "!!");
    assert_eq!(bytes(0), "0 B");
    assert_eq!(bytes(1536), "1.5 KiB");
    assert_eq!(bytes(3 * 1_048_576), "3.0 MiB");
    for (code, name) in [
        (1, "live"),
        (2, "complete"),
        (3, "incomplete"),
        (4, "unverified prefix"),
        (5, "expired"),
        (6, "missing"),
        (7, "corrupt"),
        (8, "deleted"),
    ] {
        assert_eq!(page_state(Some(code)), name);
    }
    assert_eq!(page_state(None), "unknown");
    assert_eq!(short(&[0xab; 16]), "abababab");
    assert_eq!(handle(&[0xab; 16]).len(), 32);
}

#[test]
fn j4t_11_requests_carry_the_designed_fields() {
    let list = list_request(false, Some([1; 16]), Some(9));
    assert_eq!(list.scope, Some(pb::JobScope::Project as i32));
    assert_eq!(list.project_id, Some(vec![1; 16]));
    assert_eq!((list.before, list.limit), (Some(9), Some(8)));
    let all = list_request(true, Some([1; 16]), None);
    assert_eq!(all.scope, Some(pb::JobScope::AllProjects as i32));
    assert_eq!(all.project_id, None, "all projects names no project");
    let at = read_request([5; 16], Some([1; 16]), Stream::Stderr, Some(8192));
    assert_eq!((at.offset, at.at_end), (Some(8192), None));
    assert_eq!(at.stream, Some(pb::JobStream::Stderr as i32));
    let end = read_request([5; 16], None, Stream::Stdout, None);
    assert_eq!(
        (end.offset, end.at_end, end.project_id),
        (None, Some(true), None)
    );
    assert_eq!(end.limit, Some(limits::MAX_JOB_PAGE_BYTES));
    assert!(cancel().mutation());
    assert!(!read().mutation());
    assert_eq!(Retain::Note("x".into()).action(), pb::JobRetainAction::Note);
    assert_eq!(Retain::Delete.action(), pb::JobRetainAction::Delete);
    assert_eq!(Stream::Stdout.other(), Stream::Stderr);
}
