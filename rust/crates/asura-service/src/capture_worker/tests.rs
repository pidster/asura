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

/// J1T-09 unit: the retry delays are 1, 1, 2, 3, 5, 8, 13 s with no cap.
#[test]
fn j1t_09_backoff_is_fibonacci_without_a_cap() {
    let mut backoff = Backoff::START;
    let mut seen = Vec::new();
    for _ in 0..8 {
        seen.push(backoff.delay().as_secs());
        backoff = backoff.next();
    }
    assert_eq!(seen, [1, 1, 2, 3, 5, 8, 13, 21]);
    let mut far = Backoff::START;
    for _ in 0..200 {
        far = far.next();
    }
    assert_eq!(
        far.delay().as_secs(),
        u64::MAX,
        "saturates; never ends work"
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
        tail: Vec::new(),
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
}
