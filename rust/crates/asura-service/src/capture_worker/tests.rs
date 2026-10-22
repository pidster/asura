use super::*;

fn shared() -> Arc<Shared> {
    Arc::new(Shared {
        queued_bytes: AtomicUsize::new(0),
        queued_chunks: AtomicUsize::new(0),
        backpressured: AtomicBool::new(false),
        control: Mutex::new(Control::default()),
        changed: Condvar::new(),
    })
}
fn sink() -> (Sink, SinkHandle, Receiver<Input>, Arc<Shared>) {
    let (tx, rx) = sync_channel(QUEUE_CHUNKS);
    let shared = shared();
    let handle = SinkHandle {
        state: Arc::new(Mutex::new(SinkState {
            tx: Some(tx),
            pending: VecDeque::new(),
            pending_bytes: 0,
            accepted: [0; 2],
            discarded: [0; 2],
            ended: [false; 2],
        })),
        shared: shared.clone(),
    };
    (Sink(handle.clone()), handle, rx, shared)
}

/// J1T-15 unit: `room()` is 512 KiB minus queued and waiting bytes, and 0
/// when 8 chunks wait; a full queue keeps bytes waiting on the reactor and
/// marks backpressure; nothing blocks.
#[test]
fn j1t_15_sink_room_counts_queued_and_waiting_bytes() {
    let (mut sink, handle, rx, shared) = sink();
    assert_eq!(sink.room(), QUEUE_BYTES);
    sink.accept(Stream::Stdout, &[b'a'; 4096]);
    assert_eq!(sink.room(), QUEUE_BYTES - 4096);
    for _ in 1..QUEUE_CHUNKS {
        sink.accept(Stream::Stderr, &[b'b'; 10]);
        sink.accept(Stream::Stdout, &[b'c'; 10]);
    }
    // Eight chunks wait: no room, whatever the bytes.
    assert_eq!(shared.queued_chunks.load(Ordering::Acquire), QUEUE_CHUNKS);
    assert_eq!(sink.room(), 0);
    assert!(handle.pending(), "bytes wait on the reactor");
    assert!(shared.backpressured.load(Ordering::Acquire));
    // The worker takes a chunk: the reactor side flushes on its next turn.
    let Input::Chunk(stream, bytes) = rx.recv().unwrap() else {
        panic!("chunk")
    };
    assert_eq!((stream, bytes.len()), (Stream::Stdout, 4096));
    shared.queued_bytes.fetch_sub(4096, Ordering::AcqRel);
    shared.queued_chunks.fetch_sub(1, Ordering::AcqRel);
    handle.flush();
    sink.discard(Stream::Stdout, 7);
    sink.end(Stream::Stdout);
    let (accepted, discarded, ended) = handle.counts();
    assert_eq!(accepted[0], 4096 + 7 * 10);
    assert_eq!(discarded, [7, 0]);
    assert_eq!(ended, [true, false]);
    // A closed worker side drops waiting bytes (zeroizing buffers).
    drop(rx);
    sink.accept(Stream::Stdout, b"late");
    handle.flush();
    assert!(!handle.pending());
}

/// Bytes merge into a waiting chunk of the same stream up to 64 KiB, never
/// across streams, so order within each stream is kept.
#[test]
fn waiting_bytes_merge_per_stream_up_to_one_chunk() {
    let (mut sink, handle, rx, _) = sink();
    for _ in 0..QUEUE_CHUNKS {
        sink.accept(Stream::Stdout, b"x");
    }
    sink.accept(Stream::Stdout, &[b'1'; 30_000]);
    sink.accept(Stream::Stdout, &[b'2'; 30_000]);
    sink.accept(Stream::Stdout, &[b'3'; 10_000]);
    sink.accept(Stream::Stderr, b"e");
    let state = handle.lock();
    let lengths: Vec<(Stream, usize)> = state
        .pending
        .iter()
        .map(|input| match input {
            Input::Chunk(stream, bytes) => (*stream, bytes.len()),
            Input::End(stream, _) => (*stream, 0),
        })
        .collect();
    assert_eq!(
        lengths,
        vec![
            (Stream::Stdout, 60_000),
            (Stream::Stdout, 10_000),
            (Stream::Stderr, 1)
        ]
    );
    drop(state);
    drop(rx);
}

/// J1T-09 unit: the retry delays are 1, 1, 2, 3, 5, 8, 13 s, at most 60 s.
#[test]
fn j1t_09_backoff_is_fibonacci_capped_at_a_minute() {
    let mut backoff = Backoff::START;
    let mut seen = Vec::new();
    for _ in 0..8 {
        seen.push(backoff.delay().as_secs());
        backoff = backoff.next();
    }
    assert_eq!(seen, [1, 1, 2, 3, 5, 8, 13, 21]);
    let mut capped = Backoff::START;
    let mut gaps = Vec::new();
    for _ in 0..12 {
        gaps.push(capped.delay().as_secs());
        capped = capped.next();
    }
    assert_eq!(gaps, [1, 1, 2, 3, 5, 8, 13, 21, 34, 55, 60, 60]);
    let mut far = Backoff::START;
    for _ in 0..200 {
        far = far.next();
    }
    assert_eq!(
        far.delay().as_secs(),
        60,
        "stays at the cap; never ends work"
    );
}

/// J1T-09 unit: a space event retries at once and restarts the sequence; a
/// cancel or abandonment ends the wait; otherwise the delay elapses.
#[test]
fn j1t_09_space_event_retries_at_once_and_restarts() {
    let shared = shared();
    let mut seen = 0;
    let mut backoff = Backoff(5, 8);
    let notifier = {
        let shared = shared.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            shared.space_event();
        })
    };
    let started = Instant::now();
    assert_eq!(wait_retry(&shared, &mut seen, &mut backoff), Wake::Space);
    assert!(started.elapsed() < Duration::from_secs(2));
    assert_eq!(backoff, Backoff::START);
    notifier.join().unwrap();
    let started = Instant::now();
    assert_eq!(wait_retry(&shared, &mut seen, &mut backoff), Wake::Timer);
    assert!(started.elapsed() >= Duration::from_millis(990));
    assert_eq!(backoff, Backoff(1, 2));
    shared.set(|c| c.cancel = true);
    assert_eq!(wait_retry(&shared, &mut seen, &mut backoff), Wake::Cancel);
    shared.set(|c| c.abandon = true);
    assert_eq!(wait_retry(&shared, &mut seen, &mut backoff), Wake::Abandon);
}

/// Report streams clamp loss span offsets to stored bytes after a fault,
/// and carry digests of stored bytes only.
#[test]
fn report_streams_clamp_spans_to_stored_bytes() {
    let mut stats = StreamStats::default();
    stats.loss_spans.push(LossSpan {
        stored_offset: 500,
        withheld_bytes: 9,
        reason: asura_platform::secrets::LossReason::DetectorError,
    });
    stats.loss_span_count = 1;
    let stream = StreamReport {
        stored: 100,
        digest: Some([1; 32]),
        stats,
        discarded: 3,
        eof: true,
        lines: 4,
    };
    let report = Report {
        state: CaptureState::Incomplete,
        reason: CaptureReason::StorageFault,
        streams: [stream.clone(), stream],
        distinct_numbers: 0,
        numbers: Vec::new(),
        pause_count: 0,
        first_pause_ms: 0,
        paused_ms: 0,
        final_reservation: RESERVATION_STEP,
    };
    let block = report.stream(0);
    assert_eq!(block.spans[0].stored_offset, 100);
    assert_eq!(block.discarded_bytes, 3);
    assert_eq!(block.digest, Some([1; 32]));
    // Kind 28: the same block followed by the line count.
    let job = report.job_stream(0);
    assert_eq!((job.capture, job.lines), (block, 4));
}

/// A private scratch home with a real payload store (never `~/.asura`).
struct Home {
    path: std::path::PathBuf,
    store: Arc<PayloadStore>,
    installation: Id,
}
impl Home {
    fn new(quota: u64) -> Self {
        use std::os::unix::fs::DirBuilderExt;
        let path = std::path::PathBuf::from(format!(
            "/private/tmp/as-cwj-{:x}",
            u128::from_ne_bytes(asura_platform::random_id())
        ));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .unwrap();
        let runtime = asura_platform::RuntimeDirectory::scratch(&path, true).unwrap();
        let store = PayloadStore::open(&runtime).unwrap();
        store.set_quota(quota).unwrap();
        Self {
            path,
            store: Arc::new(store),
            installation: asura_platform::random_id(),
        }
    }
    /// Admits a job (1 MiB) and starts its capture worker.
    fn start(&self) -> (Id, Handle) {
        let job = asura_platform::random_id();
        let victims = self
            .store
            .admit(self.installation, job, asura_platform::random_id())
            .unwrap();
        let rules = asura_platform::secrets::detection().unwrap();
        let handle = start(Start {
            store: self.store.clone(),
            job,
            victims,
            redaction: JobRedaction::new(rules, &[]),
            wake: None,
            gate_home: Some(self.path.clone()),
        })
        .unwrap();
        (job, handle)
    }
    fn object(&self, id: Id) -> Vec<u8> {
        std::fs::read(self.path.join(".asura/payloads/objects").join(hex(id))).unwrap()
    }
    fn staging(&self, id: Id) -> std::path::PathBuf {
        self.path.join(".asura/payloads/staging").join(hex(id))
    }
}
impl Drop for Home {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}
fn hex(id: Id) -> String {
    id.iter().map(|b| format!("{b:02x}")).collect()
}
fn holds(bytes: &[u8], needle: &str) -> bool {
    bytes.windows(needle.len()).any(|w| w == needle.as_bytes())
}
/// Polls the worker (and flushes the reactor side) until `until` matches a
/// message; returns every message seen. A test guard of 30 s turns a
/// regression into a failure instead of a hung suite.
fn drain_until(handle: &mut Handle, until: impl Fn(&Message) -> bool) -> Vec<Message> {
    let end = Instant::now() + Duration::from_secs(30);
    let mut seen = Vec::new();
    loop {
        handle.sink.flush();
        let messages = handle.drain();
        let done = messages.iter().any(&until);
        seen.extend(messages);
        if done {
            return seen;
        }
        assert!(Instant::now() < end, "worker did not answer: {seen:?}");
        std::thread::sleep(Duration::from_millis(2));
    }
}
fn sealed(messages: Vec<Message>) -> Box<Report> {
    messages
        .into_iter()
        .find_map(|m| match m {
            Message::Sealed(report) => Some(report),
            _ => None,
        })
        .expect("sealed report")
}
fn wait_for(what: &str, ready: impl Fn() -> bool) {
    let end = Instant::now() + Duration::from_secs(30);
    while !ready() {
        assert!(Instant::now() < end, "{what}");
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// J2T-22 unit (TJ-20): line counts are the newline bytes of the stored,
/// redacted bytes, per stream, and equal the installed streams.
#[test]
fn j2t_22_line_counts_equal_the_retained_streams() {
    assert_eq!(newlines(b""), 0);
    assert_eq!(newlines(b"a\nb\r\n\n"), 3);
    assert_eq!(newlines(b"no newline"), 0);
    let home = Home::new(64 * asura_storage::payload::MIB);
    let (job, mut handle) = home.start();
    let mut sink = handle.output_sink();
    let token = ["ghp_", &"x1".repeat(18)].concat();
    sink.accept(Stream::Stdout, b"one\ntwo\n");
    sink.accept(Stream::Stdout, format!("token {token}\npartial").as_bytes());
    sink.accept(Stream::Stderr, b"e1\ne2\ne3\n");
    sink.end(Stream::Stdout);
    sink.end(Stream::Stderr);
    handle.sink.close();
    let report = sealed(drain_until(&mut handle, |m| {
        matches!(m, Message::Sealed(_))
    }));
    assert_eq!(report.state, CaptureState::Complete);
    assert_eq!(report.reason, CaptureReason::None);
    let streams = [report.job_stream(0), report.job_stream(1)];
    assert_eq!([streams[0].lines, streams[1].lines], [3, 3]);
    assert!(streams[0].capture.replacements > 0, "redacted");
    handle.install();
    drain_until(&mut handle, |m| matches!(m, Message::Installed));
    let ids = asura_storage::payload::job_payloads(home.installation, job);
    for (i, id) in ids.into_iter().enumerate() {
        let bytes = home.object(id);
        assert_eq!(bytes.len() as u64, streams[i].capture.stored_bytes);
        assert_eq!(newlines(&bytes), streams[i].lines);
        assert!(!holds(&bytes, &token), "only redacted bytes are stored");
    }
}

/// J2T-22 unit (TJ-20): a write fault keeps the stored prefix from offset 0;
/// the line count covers exactly the stored prefix.
#[test]
fn j2t_22_storage_fault_keeps_the_prefix_and_its_line_count() {
    use asura_platform::payload_io::gate::{Fault, Point, inject};
    let home = Home::new(64 * asura_storage::payload::MIB);
    let (job, mut handle) = home.start();
    let mut sink = handle.output_sink();
    sink.accept(Stream::Stdout, b"kept 1\nkept 2\n");
    let [stdout, _] = asura_storage::payload::job_payloads(home.installation, job);
    let staged = home.staging(stdout);
    wait_for("first chunk staged", || {
        std::fs::metadata(&staged).map_or(0, |m| m.len()) >= 14
    });
    inject(&home.path, Point::Write, Fault::Io, 1);
    sink.accept(Stream::Stdout, b"lost 3\nlost 4\n");
    sink.end(Stream::Stdout);
    sink.end(Stream::Stderr);
    handle.sink.close();
    let messages = drain_until(&mut handle, |m| matches!(m, Message::Sealed(_)));
    assert!(
        messages
            .iter()
            .any(|m| matches!(m, Message::Fault(CaptureReason::StorageFault)))
    );
    let report = sealed(messages);
    assert_eq!(report.state, CaptureState::Incomplete);
    assert_eq!(report.reason, CaptureReason::StorageFault);
    let stream = &report.job_stream(0);
    assert_eq!(stream.capture.stored_bytes, 14);
    assert_eq!(stream.lines, 2);
    assert!(stream.capture.digest.is_some(), "the prefix has its digest");
    let prefix = std::fs::read(&staged).unwrap();
    assert_eq!(prefix, b"kept 1\nkept 2\n");
    assert_eq!(newlines(&prefix), stream.lines);
    handle.abandon();
    drain_until(&mut handle, |m| matches!(m, Message::Abandoned));
}

/// Writes `chunks` lines of 64 KiB to stdout as room allows.
fn flood(sink: &mut Box<dyn OutputSink>, handle: &Handle, chunks: usize) {
    let mut line = vec![b'a'; CHUNK_BYTES - 1];
    line.push(b'\n');
    for _ in 0..chunks {
        wait_for("room", || {
            handle.sink.flush();
            sink.room() >= CHUNK_BYTES
        });
        sink.accept(Stream::Stdout, &line);
    }
}

/// TJ-S1 (J2T-06 unit part): a paused capture retries on the leader's exit;
/// in `Draining` a short never pauses: it faults with its reason (`quota`),
/// which the controller records as kind 28 reason 9.
#[test]
fn draining_short_faults_instead_of_pausing() {
    let mib = asura_storage::payload::MIB;
    // A quota of exactly the 1 MiB initial reservation: growth is short.
    let home = Home::new(mib);
    let (_, mut handle) = home.start();
    let mut sink = handle.output_sink();
    flood(&mut sink, &handle, 17);
    let seen = drain_until(&mut handle, |m| matches!(m, Message::Paused(_)));
    assert!(matches!(
        seen.last(),
        Some(Message::Paused(PauseReason::Quota))
    ));
    handle.leader_exited();
    let seen = drain_until(&mut handle, |m| matches!(m, Message::Fault(_)));
    assert!(seen.iter().any(|m| matches!(m, Message::Resumed)));
    assert!(matches!(
        seen.last(),
        Some(Message::Fault(CaptureReason::Quota))
    ));
    flood(&mut sink, &handle, 2);
    sink.end(Stream::Stdout);
    sink.end(Stream::Stderr);
    handle.sink.close();
    let report = sealed(drain_until(&mut handle, |m| {
        matches!(m, Message::Sealed(_))
    }));
    assert_eq!(report.state, CaptureState::Incomplete);
    assert_eq!(report.reason, CaptureReason::Quota);
    assert_eq!(report.pause_count, 1, "the pause before the leader exited");
    assert_eq!(report.streams[0].stored, mib);
    assert_eq!(report.streams[0].lines, 16);
    assert!(report.streams[0].discarded > 0, "later bytes are counted");
    handle.abandon();

    // Draining before the short: no pause at all.
    let home = Home::new(mib);
    let (_, mut handle) = home.start();
    let mut sink = handle.output_sink();
    handle.leader_exited();
    flood(&mut sink, &handle, 17);
    let seen = drain_until(&mut handle, |m| matches!(m, Message::Fault(_)));
    assert!(!seen.iter().any(|m| matches!(m, Message::Paused(_))));
    sink.end(Stream::Stdout);
    sink.end(Stream::Stderr);
    handle.sink.close();
    let report = sealed(drain_until(&mut handle, |m| {
        matches!(m, Message::Sealed(_))
    }));
    assert_eq!(
        (report.reason, report.pause_count),
        (CaptureReason::Quota, 0)
    );
    handle.abandon();
}

/// TJ-S1: a cancel trigger in `Draining` makes capture discard and count
/// every chunk it has not staged; staged bytes and their lines are kept.
#[test]
fn discard_output_in_draining_counts_later_bytes() {
    let home = Home::new(64 * asura_storage::payload::MIB);
    let (job, mut handle) = home.start();
    let mut sink = handle.output_sink();
    sink.accept(Stream::Stdout, b"kept\n");
    let [stdout, _] = asura_storage::payload::job_payloads(home.installation, job);
    let staged = home.staging(stdout);
    wait_for("first chunk staged", || {
        std::fs::metadata(&staged).map_or(0, |m| m.len()) >= 5
    });
    handle.leader_exited();
    handle.discard_output();
    sink.accept(Stream::Stdout, b"dropped\n");
    sink.end(Stream::Stdout);
    sink.end(Stream::Stderr);
    handle.sink.close();
    let report = sealed(drain_until(&mut handle, |m| {
        matches!(m, Message::Sealed(_))
    }));
    assert_eq!(report.state, CaptureState::Incomplete);
    assert_eq!(report.reason, CaptureReason::DiscardedAfterCancel);
    assert_eq!(report.streams[0].stored, 5);
    assert_eq!(report.streams[0].discarded, 8);
    assert_eq!(report.streams[0].lines, 1);
    handle.abandon();
}

/// J2: when no process ran, the slot asks the sealed worker to discard: it
/// removes its staging files instead of installing them, and exits.
#[test]
fn discard_removes_staging_of_a_job_that_never_ran() {
    let home = Home::new(64 * asura_storage::payload::MIB);
    let (job, mut handle) = home.start();
    let [stdout, stderr] = asura_storage::payload::job_payloads(home.installation, job);
    let mut sink = handle.output_sink();
    wait_for("staging created", || home.staging(stdout).exists());
    // A guardian setup failure: both pipes reach EOF with no byte.
    sink.end(Stream::Stdout);
    sink.end(Stream::Stderr);
    handle.sink.close();
    let report = sealed(drain_until(&mut handle, |m| {
        matches!(m, Message::Sealed(_))
    }));
    assert_eq!(report.state, CaptureState::Complete, "the worker saw EOF");
    handle.discard();
    drain_until(&mut handle, |m| matches!(m, Message::Installed));
    assert!(!home.staging(stdout).exists() && !home.staging(stderr).exists());
    assert!(handle.exited);
}
