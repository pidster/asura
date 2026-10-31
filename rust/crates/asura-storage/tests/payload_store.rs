//! Payload store against private scratch homes: pages (J1T-10) and the
//! delivered-text bound (J3T-08), the page-read pool without a step bound
//! (J2T-16), staging captures (J1T-19), derived job recovery (J1T-22 rows,
//! now from kind 28 facts), job-keyed payloads and job recovery (J2T-04),
//! the person pin hold and delete (J4T-06, storage) and the ledger's stored
//! bytes of a real capture (J3T-22, storage). Ledger arithmetic
//! (J1T-08) is unit-tested in `payload/store.rs`.
use asura_platform::RuntimeDirectory;
use asura_platform::payload_io::gate::{self, Fault, Point};
use asura_platform::secrets::{JobRedaction, Redacted, Stream, detection};
use asura_storage::authority::conversation::{
    ActiveBinding, ArgumentForm, CaptureState, Cause, Composition, Hash, Id, JobIntent, JobOrigin,
    JobResult, JobResultBody, JobResultOutcome, JobRunState, JobSnapshot, PendingInit,
    ProjectRegistered, Record, Replay, Request, StartAuthorized, TerminalKind, ToolCall,
    ToolCallSource, ToolDecision, ToolOperation, TurnAccepted, TurnTerminal, encode_frame,
    exec_cmd_job_id, job_id, replay, request_digest,
};
use asura_storage::authority::conversation::{FrameContext, SecretKind};
use asura_storage::authority::writer::{PayloadFact, Settlement};
use asura_storage::payload::{
    DEFAULT_QUOTA, DeleteError, FACT_BATCH, Known, Location, MIB, ObjectKind, Origin, PAGE_LIMIT,
    PAGE_QUEUE, Page, PagePool, PageReply, PageRequest, PageState, PayloadError, PayloadState,
    PayloadStore, RECOVERY_RESERVE, RecoveryError, RecoveryReport, SpaceListener, StreamIndex,
    TEXT_LIMIT, Victim, job_payloads, payload_id, read_page, recover, recover_known,
    recover_with_facts,
};
use sha2::{Digest, Sha256};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

fn id(n: u64) -> Id {
    let mut id = [0; 16];
    id[8..].copy_from_slice(&n.to_be_bytes());
    id
}
fn sha(bytes: &[u8]) -> Hash {
    Sha256::digest(bytes).into()
}
fn hex(id: Id) -> String {
    id.iter().map(|b| format!("{b:02x}")).collect()
}

/// A private scratch home (mode 0700, canonical), removed on drop.
struct Home(PathBuf);
impl Home {
    fn new() -> Self {
        let path = PathBuf::from("/private/tmp")
            .join(format!("as-ps-{}", hex(asura_platform::random_id())));
        fs::create_dir(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        Self(fs::canonicalize(path).unwrap())
    }
    fn file(&self, directory: &str, id: Id) -> PathBuf {
        self.0.join(".asura/payloads").join(directory).join(hex(id))
    }
    /// Writes a payload file directly, mode 0600, as an earlier run left it.
    fn put(&self, directory: &str, id: Id, bytes: &[u8]) {
        let path = self.file(directory, id);
        fs::write(&path, bytes).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    }
    fn read(&self, directory: &str, id: Id) -> Option<Vec<u8>> {
        fs::read(self.file(directory, id)).ok()
    }
}
impl Drop for Home {
    fn drop(&mut self) {
        gate::set_free_bytes(&self.0, None);
        let _ = fs::remove_dir_all(&self.0);
    }
}

struct Fixture {
    store: Arc<PayloadStore>,
    home: Home,
}
/// A store whose volume reports `free` bytes; the quota is still 0.
fn open(free: u64) -> Fixture {
    let home = Home::new();
    gate::set_free_bytes(&home.0, Some(free));
    let runtime = RuntimeDirectory::scratch(&home.0, true).unwrap();
    let store = Arc::new(PayloadStore::open(&runtime).unwrap());
    assert_eq!(store.quota(), 0);
    Fixture { store, home }
}
fn fixture() -> Fixture {
    let fixture = open(64 * 1024 * MIB);
    assert_eq!(
        fixture.store.set_quota(DEFAULT_QUOTA).unwrap(),
        DEFAULT_QUOTA
    );
    fixture
}

/// Real scanner output for `bytes` (no secrets, so it is unchanged).
fn redacted(bytes: &[u8]) -> Redacted {
    let mut scanner = JobRedaction::new(detection().unwrap(), &[]);
    let released = scanner.scan(Stream::Stdout, bytes);
    let tail = scanner.finish(Stream::Stdout);
    let out = if released.is_empty() { tail } else { released };
    assert_eq!(out.as_bytes(), bytes);
    out
}

// Calls of operation id(10), generation 1, installation id(1), keyed by
// their model-origin job IDs.
fn payloads(ordinal: u32) -> [Id; 2] {
    [
        payload_id(id(1), call(ordinal), ObjectKind::Stdout),
        payload_id(id(1), call(ordinal), ObjectKind::Stderr),
    ]
}
fn call(ordinal: u32) -> Id {
    exec_cmd_job_id(id(10), 1, ordinal)
}
fn admit(store: &PayloadStore, ordinal: u32) -> [Id; 2] {
    let victims = store.admit(id(1), call(ordinal), id(10)).unwrap();
    assert!(victims.is_empty());
    payloads(ordinal)
}
fn page(store: &PayloadStore, payload: Id, offset: u64, limit: usize) -> Page {
    read_page(
        store,
        PageRequest {
            payload,
            offset,
            limit,
            scope: None,
            lines: asura_storage::payload::Lines::Any,
        },
    )
}
fn wait_until(mut condition: impl FnMut() -> bool) {
    while !condition() {
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
}

// ---------------------------------------------------------------- J1T-19

#[test]
fn j1t_19_capture_offsets_resume_hashing_and_stream_identity() {
    let f = fixture();
    let [out, err] = admit(&f.store, 1);
    let mut capture = f.store.create_capture(call(1)).unwrap();
    assert_eq!(capture.call(), call(1));
    assert!(Arc::ptr_eq(capture.store(), &f.store));
    assert_eq!(
        f.store.payload_state(out),
        Some((PayloadState::Live, Location::Staging, 0))
    );
    // Staging files are created exclusively.
    assert!(matches!(
        f.store.create_capture(call(1)),
        Err(PayloadError::Exists)
    ));
    let line = redacted(b"hello world\n");
    let (written, result) = capture.append(StreamIndex::Stdout, &line, 0);
    result.unwrap();
    assert_eq!(written, 12);
    // A resumed append writes from `from` and reports only its own bytes.
    let (written, result) = capture.append(StreamIndex::Stdout, &line, 6);
    result.unwrap();
    assert_eq!(written, 6);
    for from in [12, 99] {
        let (written, result) = capture.append(StreamIndex::Stdout, &line, from);
        result.unwrap();
        assert_eq!(written, 0);
    }
    let warning = redacted(b"warn\n");
    let (written, result) = capture.append(StreamIndex::Stderr, &warning, 0);
    result.unwrap();
    assert_eq!(written, 5);
    assert_eq!(capture.stored(StreamIndex::Stdout), 18);
    assert_eq!(capture.stored(StreamIndex::Stderr), 5);
    assert_eq!(capture.total(), 23);
    capture.sync().unwrap();
    // Stream identity: each stream has its own staging file named by its ID.
    assert_eq!(
        f.home.read("staging", out).unwrap(),
        b"hello world\nworld\n"
    );
    assert_eq!(f.home.read("staging", err).unwrap(), b"warn\n");
    assert_ne!(out, err);
    let sealed = capture.seal();
    assert_eq!(
        sealed,
        [(18, sha(b"hello world\nworld\n")), (5, sha(b"warn\n"))]
    );
}

#[test]
fn j1t_19_hash_covers_exactly_the_written_bytes_after_injected_faults() {
    let f = fixture();
    let [out, err] = admit(&f.store, 1);
    let mut capture = f.store.create_capture(call(1)).unwrap();
    let first = redacted(b"first\n");
    let second = redacted(b"second\n");
    capture.append(StreamIndex::Stdout, &first, 0).1.unwrap();
    gate::inject(&f.home.0, Point::Write, Fault::NoSpace, 1);
    let (written, result) = capture.append(StreamIndex::Stdout, &second, 0);
    assert_eq!(written, 0);
    assert!(matches!(result, Err(PayloadError::NoSpace)));
    assert_eq!(capture.stored(StreamIndex::Stdout), 6);
    // The retry continues from the returned position.
    let (written, result) = capture.append(StreamIndex::Stdout, &second, written);
    result.unwrap();
    assert_eq!(written, 7);
    gate::inject(&f.home.0, Point::Write, Fault::Io, 1);
    let (written, result) = capture.append(StreamIndex::Stderr, &second, 0);
    assert_eq!(written, 0);
    assert!(matches!(result, Err(PayloadError::Io(_))));
    gate::inject(&f.home.0, Point::Sync, Fault::Io, 1);
    assert!(matches!(capture.sync(), Err(PayloadError::Io(_))));
    capture.sync().unwrap();
    assert_eq!(f.home.read("staging", out).unwrap(), b"first\nsecond\n");
    assert_eq!(f.home.read("staging", err).unwrap(), b"");
    assert_eq!(
        capture.seal(),
        [(13, sha(b"first\nsecond\n")), (0, sha(b""))]
    );
}

#[test]
fn j1t_19_watermark_is_raised_only_by_a_completed_write() {
    let f = fixture();
    let [out, _] = admit(&f.store, 1);
    let mut capture = f.store.create_capture(call(1)).unwrap();
    let empty = page(&f.store, out, 0, 64);
    assert_eq!(
        (
            empty.state,
            empty.total,
            empty.watermark,
            empty.text.as_str()
        ),
        (PageState::Live, 0, Some(0), "")
    );
    capture
        .append(StreamIndex::Stdout, &redacted(b"abc\n"), 0)
        .1
        .unwrap();
    let live = page(&f.store, out, 0, 64);
    assert_eq!(live.state, PageState::Live);
    assert_eq!(live.text, "abc\n");
    assert_eq!(
        (live.watermark, live.total, live.next_offset),
        (Some(4), 4, 4)
    );
    assert_eq!(live.digest, None);
    // A failed write leaves the watermark where it was.
    gate::inject(&f.home.0, Point::Write, Fault::NoSpace, 1);
    let more = redacted(b"defg\n");
    assert_eq!(capture.append(StreamIndex::Stdout, &more, 0).0, 0);
    assert_eq!(page(&f.store, out, 0, 64).watermark, Some(4));
    // Past the watermark is a stale cursor even though the file may grow.
    let stale = page(&f.store, out, 5, 64);
    assert_eq!(
        (stale.state, stale.next_offset),
        (PageState::StaleCursor, 4)
    );
    capture.append(StreamIndex::Stdout, &more, 0).1.unwrap();
    let live = page(&f.store, out, 4, 64);
    assert_eq!((live.text.as_str(), live.watermark), ("defg\n", Some(9)));
}

// ---------------------------------------------------------------- J1T-10

/// Captures `stdout` and `stderr` for one call, seals, settles and installs it.
fn settled_call(
    f: &Fixture,
    ordinal: u32,
    state: CaptureState,
    stdout: &[u8],
    stderr: &[u8],
    last_use: u64,
    install: bool,
) -> [Id; 2] {
    let ids = admit(&f.store, ordinal);
    let mut capture = f.store.create_capture(call(ordinal)).unwrap();
    for (stream, bytes) in [(StreamIndex::Stdout, stdout), (StreamIndex::Stderr, stderr)] {
        if !bytes.is_empty() {
            capture.append(stream, &redacted(bytes), 0).1.unwrap();
        }
    }
    capture.sync().unwrap();
    let [(a, da), (b, db)] = capture.seal();
    let digests = state != CaptureState::Unconfirmed;
    f.store.settle(
        call(ordinal),
        state,
        [(a, digests.then_some(da)), (b, digests.then_some(db))],
        last_use,
        false,
    );
    if install {
        f.store.install(call(ordinal)).unwrap();
    }
    ids
}

#[test]
fn j1t_10_page_states_offsets_and_scope() {
    let f = fixture();
    let [out1, err1] = settled_call(&f, 1, CaptureState::Complete, b"out\n", b"err\n", 50, true);
    let complete = page(&f.store, out1, 0, 64);
    assert_eq!(complete.state, PageState::Complete);
    assert_eq!(complete.text, "out\n");
    assert_eq!(complete.digest, Some(sha(b"out\n")));
    assert_eq!((complete.total, complete.watermark), (4, None));
    assert_eq!(page(&f.store, err1, 0, 64).text, "err\n");
    assert_eq!(
        f.store.payload_state(out1),
        Some((PayloadState::Complete, Location::Objects, 4))
    );
    assert!(f.home.read("staging", out1).is_none());
    assert_eq!(f.home.read("objects", out1).unwrap(), b"out\n");
    // Offset equal to the total: an empty page; larger: stale cursor.
    let end = page(&f.store, out1, 4, 64);
    assert_eq!(
        (end.state, end.start, end.next_offset, end.text.as_str()),
        (PageState::Complete, 4, 4, "")
    );
    let stale = page(&f.store, out1, 5, 64);
    assert_eq!(
        (stale.state, stale.next_offset),
        (PageState::StaleCursor, 4)
    );
    // Scope: the owning operation reads; another is denied.
    let scoped = |scope| {
        read_page(
            &f.store,
            PageRequest {
                payload: out1,
                offset: 0,
                limit: 64,
                scope: Some(scope),
                lines: asura_storage::payload::Lines::Any,
            },
        )
    };
    assert_eq!(scoped(id(10)).state, PageState::Complete);
    let denied = scoped(id(11));
    assert_eq!(
        (denied.state, denied.text.as_str()),
        (PageState::Denied, "")
    );
    // Incomplete, installed.
    let [out2, _] = settled_call(&f, 2, CaptureState::Incomplete, b"part\n", b"", 60, true);
    let incomplete = page(&f.store, out2, 0, 64);
    assert_eq!(
        (incomplete.state, incomplete.text.as_str()),
        (PageState::Incomplete, "part\n")
    );
    // Unconfirmed: never installed; pages read the staging prefix.
    let [out3, _] = settled_call(
        &f,
        3,
        CaptureState::Unconfirmed,
        b"prefix\n",
        b"",
        70,
        false,
    );
    let unverified = page(&f.store, out3, 0, 64);
    assert_eq!(unverified.state, PageState::UnverifiedPrefix);
    assert_eq!(
        (unverified.text.as_str(), unverified.digest),
        ("prefix\n", None)
    );
    // Expired: interim expiry deletes the oldest call in this run.
    f.store
        .set_effective_quota(f.store.used() + MIB - 1, 64 * 1024 * MIB);
    let victims = f.store.admit(id(1), call(4), id(10)).unwrap();
    assert_eq!(
        victims,
        [Victim {
            call: call(1),
            payloads: [out1, err1]
        }]
    );
    f.store.delete_victim(victims[0]).unwrap();
    for payload in [out1, err1] {
        let expired = page(&f.store, payload, 0, 64);
        assert_eq!(
            (expired.state, expired.text.as_str()),
            (PageState::Expired, "")
        );
        assert!(f.home.read("objects", payload).is_none());
        assert_eq!(f.store.payload_state(payload), None);
    }
    // Missing: no catalog entry.
    assert_eq!(page(&f.store, [0x55; 16], 0, 64).state, PageState::Missing);
    // Missing: catalog entry without a file.
    fs::remove_file(f.home.file("objects", out2)).unwrap();
    assert_eq!(page(&f.store, out2, 0, 64).state, PageState::Missing);
}

#[test]
fn j1t_10_offsets_snap_to_scalar_boundaries_and_invalid_bytes_are_replaced() {
    let f = fixture();
    // a(1) é(2) €(3) 😀(4) b(1) \n(1); then invalid bytes.
    let text = "aé€😀b\n".as_bytes();
    let [out, err] = settled_call(
        &f,
        1,
        CaptureState::Complete,
        text,
        b"x\xffy\xc3\n",
        1,
        true,
    );
    let at = |offset, limit| {
        let p = page(&f.store, out, offset, limit);
        (p.start, p.text, p.next_offset)
    };
    // Starts inside 2-, 3- and 4-byte scalars move forward.
    assert_eq!(at(2, 64), (3, "€😀b\n".into(), 12));
    assert_eq!(at(4, 64), (6, "😀b\n".into(), 12));
    assert_eq!(at(5, 64), (6, "😀b\n".into(), 12));
    for offset in 7..=9 {
        assert_eq!(at(offset, 64), (10, "b\n".into(), 12));
    }
    // Ends inside scalars trim back.
    assert_eq!(at(0, 2), (0, "a".into(), 1));
    assert_eq!(at(0, 5), (0, "aé".into(), 3));
    assert_eq!(at(0, 9), (0, "aé€".into(), 6));
    // A page that would be empty keeps its first whole scalar.
    assert_eq!(at(6, 1), (6, "😀".into(), 10));
    // Following next_offset reassembles the text exactly.
    let mut offset = 0;
    let mut joined = String::new();
    while offset < 12 {
        let p = page(&f.store, out, offset, 3);
        assert_eq!(p.start, offset);
        joined.push_str(&p.text);
        offset = p.next_offset;
    }
    assert_eq!(joined, "aé€😀b\n");
    // Invalid UTF-8 becomes U+FFFD at read; the stored bytes are unchanged.
    let invalid = page(&f.store, err, 0, 64);
    assert_eq!(invalid.text, "x\u{FFFD}y\u{FFFD}\n");
    assert_eq!(invalid.next_offset, 5);
    assert_eq!(f.home.read("objects", err).unwrap(), b"x\xffy\xc3\n");
}

#[test]
fn j1t_10_live_page_at_the_watermark_never_splits_a_scalar() {
    let f = fixture();
    let [out, _] = admit(&f.store, 1);
    let mut capture = f.store.create_capture(call(1)).unwrap();
    // The pipe delivered half of 😀; the rest follows later.
    capture
        .append(StreamIndex::Stdout, &redacted(b"a\xF0\x9F"), 0)
        .1
        .unwrap();
    let first = page(&f.store, out, 0, 64);
    assert_eq!(first.state, PageState::Live);
    assert_eq!((first.text.as_str(), first.next_offset), ("a", 1));
    assert_eq!(first.watermark, Some(3));
    let pending = page(&f.store, out, 1, 64);
    assert_eq!((pending.text.as_str(), pending.next_offset), ("", 1));
    capture
        .append(StreamIndex::Stdout, &redacted(b"\x98\x80\n"), 0)
        .1
        .unwrap();
    let second = page(&f.store, out, first.next_offset, 64);
    assert_eq!((second.text.as_str(), second.next_offset), ("😀\n", 6));
}

// ---------------------------------------------------------------- J2T-16

fn pool_fixture() -> (Fixture, PagePool, PageRequest) {
    let f = fixture();
    let [out, _] = settled_call(&f, 1, CaptureState::Complete, b"pool\n", b"", 1, true);
    let pool = PagePool::start(f.store.clone(), 1, None).unwrap();
    let request = PageRequest {
        payload: out,
        offset: 0,
        limit: 64,
        scope: None,
        lines: asura_storage::payload::Lines::Any,
    };
    (f, pool, request)
}
fn complete(reply: PageReply) -> String {
    let PageReply::Page(page) = reply else {
        panic!("cancelled");
    };
    assert_eq!(page.state, PageState::Complete);
    page.text
}

/// J34-E page wake (J4 control handler; J4T-04): a worker notifies the
/// reactor wake after it stores a reply, so the reply is ready when the
/// wake arrives, with no other event.
#[test]
fn j34e_page_reply_wakes_the_reactor_after_it_is_stored() {
    use std::io::Read;
    use std::os::fd::{AsRawFd, BorrowedFd};
    let f = fixture();
    let [out, _] = settled_call(&f, 1, CaptureState::Complete, b"wake\n", b"", 1, true);
    let (reader, sender) = asura_platform::events::wake_pair().unwrap();
    let pool = PagePool::start(f.store.clone(), 1, Some(sender)).unwrap();
    // SAFETY: `reader` owns the descriptor and outlives this borrow; the
    // clone is a new descriptor of the same nonblocking socket.
    let borrowed = unsafe { BorrowedFd::borrow_raw(reader.as_raw_fd()) };
    let mut wake = std::os::unix::net::UnixStream::from(borrowed.try_clone_to_owned().unwrap());
    let mut byte = [0u8; 1];
    assert!(wake.read(&mut byte).is_err(), "no wake before a read");
    let ticket = pool
        .submit(
            PageRequest {
                payload: out,
                offset: 0,
                limit: 64,
                scope: None,
                lines: asura_storage::payload::Lines::Any,
            },
            Origin::Client,
        )
        .unwrap();
    wait_until(|| matches!(wake.read(&mut byte), Ok(1)));
    assert_eq!(
        complete(ticket.poll().expect("stored before the wake")),
        "wake\n"
    );
}

/// `jobs.modelReservedPageReads` 0: no worker is reserved, so two client
/// reads run at once; with 1 the second waits for the general worker.
#[test]
fn j34e_model_reserved_page_reads_zero_frees_both_workers() {
    let f = fixture();
    let [out, _] = settled_call(&f, 1, CaptureState::Complete, b"both\n", b"", 1, true);
    let request = PageRequest {
        payload: out,
        offset: 0,
        limit: 64,
        scope: None,
        lines: asura_storage::payload::Lines::Any,
    };
    let pool = PagePool::start(f.store.clone(), 0, None).unwrap();
    pool.hold_for_test(true);
    let first = pool.submit(request, Origin::Client).unwrap();
    let second = pool.submit(request, Origin::Client).unwrap();
    wait_until(|| pool.running() == 2 && pool.queued() == 0);
    pool.hold_for_test(false);
    assert_eq!(complete(first.wait()), "both\n");
    assert_eq!(complete(second.wait()), "both\n");
    // A value above 1 still leaves one general worker.
    let pool = PagePool::start(f.store.clone(), 5, None).unwrap();
    pool.hold_for_test(true);
    let first = pool.submit(request, Origin::Client).unwrap();
    let second = pool.submit(request, Origin::Client).unwrap();
    wait_until(|| pool.running() == 1);
    assert_eq!(pool.queued(), 1);
    pool.hold_for_test(false);
    assert_eq!(complete(first.wait()), "both\n");
    assert_eq!(complete(second.wait()), "both\n");
}

/// J2T-16 (WB1): a gated read held 5 s returns its page after release, with
/// no `busy`; the worker stays held meanwhile and no replacement starts.
#[test]
fn j2t_16_gated_read_waits_for_its_reply_with_no_step() {
    let (_f, pool, request) = pool_fixture();
    pool.hold_for_test(true);
    let client = pool.submit(request, Origin::Client).unwrap();
    wait_until(|| pool.queued() == 0);
    // A second client request waits behind the held general worker: no
    // replacement worker takes it, and the reserved worker never does.
    let behind = pool.submit(request, Origin::Client).unwrap();
    let held = Instant::now();
    while held.elapsed() < Duration::from_secs(5) {
        assert!(client.poll().is_none(), "no busy and no page while held");
        assert!(behind.poll().is_none());
        assert_eq!((pool.running(), pool.queued()), (1, 1));
        std::thread::sleep(Duration::from_millis(250));
    }
    pool.hold_for_test(false);
    assert_eq!(complete(client.wait()), "pool\n");
    assert_eq!(complete(behind.wait()), "pool\n");
    wait_until(|| pool.running() == 0);
}

/// J2T-16: the caller's cancel withdraws a queued read at once and frees its
/// queue place; the ninth queued request is `busy`.
#[test]
fn j2t_16_cancel_withdraws_a_queued_read_and_the_ninth_is_busy() {
    let (_f, pool, request) = pool_fixture();
    pool.hold_for_test(true);
    let running = pool.submit(request, Origin::Client).unwrap();
    wait_until(|| pool.queued() == 0);
    let mut queued: Vec<_> = (1..=PAGE_QUEUE)
        .map(|n| {
            let ticket = pool.submit(request, Origin::Client).unwrap();
            assert_eq!(pool.queued(), n);
            ticket
        })
        .collect();
    for origin in [Origin::Client, Origin::Model] {
        assert_eq!(pool.submit(request, origin).err(), Some(PageState::Busy));
    }
    // Cancel: the queue place is free at once, before any worker moves.
    queued.remove(3).cancel();
    assert_eq!(pool.queued(), PAGE_QUEUE - 1);
    // Detach (drop) of a queued ticket withdraws it too.
    drop(queued.remove(0));
    assert_eq!(pool.queued(), PAGE_QUEUE - 2);
    let refill = pool.submit(request, Origin::Client).unwrap();
    assert_eq!(pool.queued(), PAGE_QUEUE - 1);
    pool.hold_for_test(false);
    assert_eq!(complete(running.wait()), "pool\n");
    for ticket in queued {
        assert_eq!(complete(ticket.wait()), "pool\n");
    }
    assert_eq!(complete(refill.wait()), "pool\n");
}

/// J2T-16: detaching a running read discards its reply; the worker stays
/// held until the read returns and then serves the queue.
#[test]
fn j2t_16_detach_discards_a_running_reply_and_the_worker_stays_held() {
    let (_f, pool, request) = pool_fixture();
    pool.hold_for_test(true);
    let running = pool.submit(request, Origin::Client).unwrap();
    wait_until(|| pool.queued() == 0);
    let next = pool.submit(request, Origin::Client).unwrap();
    drop(running);
    // The general worker is still held by the detached read.
    assert_eq!((pool.running(), pool.queued()), (1, 1));
    std::thread::sleep(Duration::from_millis(50));
    assert!(next.poll().is_none());
    assert_eq!(pool.running(), 1);
    pool.hold_for_test(false);
    assert_eq!(complete(next.wait()), "pool\n");
    wait_until(|| pool.running() == 0);
}

/// J2T-16: one worker is reserved for model requests; client requests never
/// take it. A reply is returned once.
#[test]
fn j2t_16_reserved_model_worker() {
    let (_f, pool, request) = pool_fixture();
    pool.hold_for_test(true);
    let client = pool.submit(request, Origin::Client).unwrap();
    wait_until(|| pool.queued() == 0);
    // The reserved worker still takes a model request.
    let model = pool.submit(request, Origin::Model).unwrap();
    wait_until(|| pool.queued() == 0);
    assert_eq!(pool.running(), 2);
    // Client requests queue while both workers are held.
    let queued = pool.submit(request, Origin::Client).unwrap();
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(pool.queued(), 1);
    pool.hold_for_test(false);
    wait_until(|| pool.running() == 0 && pool.queued() == 0);
    for ticket in [&client, &model, &queued] {
        assert_eq!(complete(ticket.poll().unwrap()), "pool\n");
        assert!(ticket.poll().is_none(), "a reply is returned once");
    }
}

/// J2T-16: pool shutdown ends every waiting ticket, queued or running, with
/// `Cancelled`; later requests are `busy`.
#[test]
fn j2t_16_shutdown_cancels_every_waiting_ticket() {
    let (_f, pool, request) = pool_fixture();
    pool.hold_for_test(true);
    let running = pool.submit(request, Origin::Client).unwrap();
    wait_until(|| pool.queued() == 0);
    let queued = pool.submit(request, Origin::Client).unwrap();
    let waiter = std::thread::spawn(move || queued.wait());
    pool.close();
    assert_eq!(running.wait(), PageReply::Cancelled);
    assert_eq!(waiter.join().unwrap(), PageReply::Cancelled);
    assert_eq!(pool.queued(), 0);
    assert_eq!(
        pool.submit(request, Origin::Model).err(),
        Some(PageState::Busy)
    );
}

// ---------------------------------------------------------------- J1T-22

/// An in-memory format-1 journal (installation id(1), operation id(10)).
struct Journal {
    bytes: Vec<u8>,
    sequence: u64,
    digest: Hash,
}
impl Journal {
    fn append(&mut self, record: Record) -> u64 {
        let frame = encode_frame(
            &FrameContext {
                installation_id: id(1),
                transition_id: id(10_000 + self.sequence),
                sequence: self.sequence + 1,
                prior_digest: self.digest,
                expected_revision: self.sequence,
                owner_generation: 1,
            },
            &record,
        )
        .unwrap();
        self.digest
            .copy_from_slice(&frame[frame.len() - 40..frame.len() - 8]);
        self.bytes.extend(frame);
        self.sequence += 1;
        self.sequence
    }
    fn started() -> Self {
        let mut j = Self {
            bytes: Vec::new(),
            sequence: 0,
            digest: [0; 32],
        };
        j.append(Record::PendingInit(PendingInit {
            request: id(2),
            digest: request_digest(Request::Initialize { mode: 1 }).unwrap(),
            mode: 1,
            configuration_revision: 1,
            configuration_digest: [3; 32],
            graph: id(4),
        }));
        j.append(Record::ActiveBinding(ActiveBinding {
            request: id(2),
            generation: 1,
            graph: id(4),
            configuration_digest: [3; 32],
        }));
        j.append(Record::ProjectRegistered(ProjectRegistered {
            request: id(5),
            digest: request_digest(Request::Register {
                location: "/workspace",
            })
            .unwrap(),
            project: id(6),
            location: "/workspace".into(),
            device: 1,
            inode: 2,
            registry_revision: 1,
            visibility: 1,
        }));
        j.append(Record::TurnAccepted(Box::new(TurnAccepted {
            request: id(7),
            digest: request_digest(Request::Submit {
                project: id(6),
                conversation: None,
                expected_generation: 0,
                prompt: "hello",
            })
            .unwrap(),
            project: id(6),
            original_conversation: None,
            expected_generation: 0,
            conversation: id(8),
            generation: 1,
            task: id(9),
            operation: id(10),
            model: "system".into(),
            configuration_digest: [3; 32],
            instructions_digest: [11; 32],
            input_digest: [12; 32],
            prior_operations: vec![],
            prompt: "hello".into(),
            event_cursor: 1,
            composition: Composition::default(),
            thinking_level: 0,
            thinking_request: 0,
        })));
        j.append(Record::StartAuthorized(StartAuthorized {
            operation: id(10),
            generation: 1,
            helper: id(13),
            input_digest: [12; 32],
        }));
        j
    }
    /// A `run` exec_cmd kind 14; returns its sequence. Replay derives the
    /// job and payload IDs from it (TCR-D23).
    fn admitted(&mut self, ordinal: u32) -> u64 {
        self.append(Record::ToolCall(Box::new(ToolCall {
            operation: id(10),
            generation: 1,
            ordinal,
            source: ToolCallSource::GeneratedContent,
            name: "exec_cmd".into(),
            name_bytes: 8,
            tool_operation: Some(ToolOperation::ExecCmd),
            decision: ToolDecision::Run,
            fault: None,
            reason: None,
            form: ArgumentForm::Canonical,
            arguments_bytes: 40,
            arguments: "{\"command\":\"printf \\\"$ASURA_SECRET_1\\\"\",\"cwd\":\".\"}".into(),
            number_kinds: vec![SecretKind::GithubToken],
            unnumbered: 0,
            rule_set_revision: 1,
            rule_set_digest: [21; 32],
            command_cut: false,
            whole_command: false,
            refusal: None,
        })))
    }
    fn failed(&mut self) {
        self.append(Record::TurnTerminal(TurnTerminal {
            operation: id(10),
            generation: 1,
            kind: TerminalKind::Failed,
            cause: Cause::ProviderFailure,
            final_cursor: u64::MAX,
            usage_known: false,
            output_tokens: 0,
            text_offset: 0,
            text: "reply".into(),
        }));
    }
    /// An exec_cmd job call: kind 14 `run`, kind 26 and kind 29 (handle
    /// delivery, JR11 and JR13). Returns the job ID and the kind 26
    /// sequence.
    fn job(&mut self, ordinal: u32) -> (Id, u64) {
        self.admitted(ordinal);
        let origin = JobOrigin::Model {
            operation: id(10),
            generation: 1,
            ordinal,
        };
        let intent = self.append(Record::JobIntent(JobIntent {
            origin,
            agent: 0,
            time: 1_760_000_000_000,
        }));
        self.append(Record::JobResult(Box::new(JobResult {
            operation: id(10),
            generation: 1,
            ordinal,
            status: JobResultOutcome::Ok.status(),
            outcome: JobResultOutcome::Ok,
            job: [0; 16],
            view: Vec::new(),
            body: JobResultBody::State(JobSnapshot {
                state: JobRunState::Running,
                numbers: [0; 3],
                elapsed_ms: 7,
                stdout_bytes: 5,
                stderr_bytes: 0,
            }),
        })));
        (job_id(&origin), intent)
    }
    fn replay(&self) -> Replay {
        replay(&self.bytes, &AtomicBool::new(false)).unwrap()
    }
}
fn body(ordinal: u32) -> Vec<u8> {
    format!("payload {ordinal}\n").into_bytes()
}
fn tail(ordinal: u32) -> Vec<u8> {
    format!("e{ordinal}\n").into_bytes()
}
fn recorded(ordinal: u32) -> [(u64, Option<Hash>); 2] {
    [
        (body(ordinal).len() as u64, Some(sha(&body(ordinal)))),
        (tail(ordinal).len() as u64, Some(sha(&tail(ordinal)))),
    ]
}
fn no_cancel() -> AtomicBool {
    AtomicBool::new(false)
}
/// The recovery facts of one settled or unsettled job of operation id(10),
/// as the journal's `PayloadFacts` answers them for its first payload.
fn fact(job: Id, intent: u64, settled: Option<Settlement>) -> PayloadFact {
    let payloads = job_payloads(id(1), job);
    PayloadFact {
        payload: payloads[0],
        call: job,
        operation: id(10),
        payloads,
        intent_sequence: intent,
        settled,
        active: false,
        pinned: false,
        deleted: false,
    }
}
/// Recovery from the given job facts, one per job.
fn recover_facts(
    store: &PayloadStore,
    facts: &[PayloadFact],
    quota: u64,
) -> Result<RecoveryReport, RecoveryError> {
    recover_known(
        store,
        facts.iter().map(Known::from).collect(),
        quota,
        &no_cancel(),
    )
}
/// Kind 28 capture facts of a job whose streams hold `body` and `tail`.
fn settled(state: CaptureState, sequence: u64, ordinal: u32) -> Option<Settlement> {
    Some((state, sequence, recorded(ordinal)))
}

/// J1T-22 rows for jobs (TJ-Q5): each job's payload state derives from its
/// kind 26 and kind 28 facts and the files on disk.
#[test]
fn j1t_22_derived_recovery_applies_every_table_row() {
    let f = open(20 * MIB);
    let home = &f.home;
    let mut facts = Vec::new();
    // Job n: kind 26 at 10 n, kind 28 at 10 n + 5.
    let mut last_use = Vec::new();
    let mut row = |n: u32, state: Option<CaptureState>, streams| {
        let sequence = u64::from(n) * 10 + 5;
        facts.push(fact(
            call(n),
            u64::from(n) * 10,
            state.map(|state| (state, sequence, streams)),
        ));
        last_use.push(sequence);
        payloads(n)
    };
    // 1: complete, object installed (plus a stray staging copy).
    let [o1, e1] = row(1, Some(CaptureState::Complete), recorded(1));
    home.put("objects", o1, &body(1));
    home.put("objects", e1, &tail(1));
    home.put("staging", o1, &body(1));
    // 2: complete, object whose length differs from kind 28.
    let [o2, e2] = row(2, Some(CaptureState::Complete), recorded(2));
    home.put("objects", o2, b"payload 2!\n");
    home.put("objects", e2, &tail(2));
    // 3: complete, staging only, digest matches: installed.
    let [o3, e3] = row(3, Some(CaptureState::Complete), recorded(3));
    home.put("staging", o3, &body(3));
    home.put("staging", e3, &tail(3));
    // 4: incomplete, staging only; stdout digest mismatch (same length).
    let [o4, e4] = row(4, Some(CaptureState::Incomplete), recorded(4));
    home.put("staging", o4, b"payload X\n");
    home.put("staging", e4, &tail(4));
    // 5: complete, no files.
    let [o5, e5] = row(5, Some(CaptureState::Complete), recorded(5));
    // 6: unconfirmed, staging prefix; its last use is its kind 26.
    let [o6, e6] = row(6, Some(CaptureState::Unconfirmed), [(10, None), (3, None)]);
    home.put("staging", o6, &body(6));
    home.put("staging", e6, &tail(6));
    // 7: not_started with stray staging files.
    let [o7, e7] = row(7, Some(CaptureState::NotStarted), [(0, None), (0, None)]);
    home.put("staging", o7, b"");
    home.put("staging", e7, b"");
    // 8: kind 26 without kind 28 (or an `interrupted` one).
    let [o8, e8] = row(8, None, [(0, None), (0, None)]);
    home.put("staging", o8, &body(8));
    home.put("staging", e8, &tail(8));
    // Orphans: no job derives them.
    home.put("staging", [0xAA; 16], b"orphan");
    home.put("objects", [0xBB; 16], b"orphan");
    let (intent6, intent8) = (60, 80);
    let report = recover_facts(&f.store, &facts, DEFAULT_QUOTA).unwrap();
    // Bytes on disk afterwards, by call: 13 + 14 + 13 + 13 + 0 + 13 + 13.
    assert_eq!(report.installed, 3);
    assert_eq!(report.corrupt, 2);
    assert_eq!(report.missing, 2);
    assert_eq!(report.unverified, 4);
    assert_eq!(report.orphans_deleted, 2);
    assert_eq!(report.strays_deleted, 3);
    assert_eq!(report.retained, 79);
    // The ledger counts the bytes on disk; the quota is bounded by disk.
    assert_eq!(f.store.used(), 79);
    assert_eq!(f.store.quota(), 79 + 20 * MIB - RECOVERY_RESERVE);
    let state = |payload| f.store.payload_state(payload).unwrap();
    // Row: object installed, length checked.
    assert_eq!(state(o1), (PayloadState::Complete, Location::Objects, 10));
    assert!(home.read("staging", o1).is_none());
    assert_eq!(page(&f.store, o1, 0, 64).text, "payload 1\n");
    assert_eq!(page(&f.store, o1, 0, 64).digest, Some(sha(&body(1))));
    // Row: object length mismatch is corrupt; the file stays.
    assert_eq!(state(o2), (PayloadState::Corrupt, Location::Objects, 10));
    assert_eq!(state(e2), (PayloadState::Complete, Location::Objects, 3));
    assert_eq!(page(&f.store, o2, 0, 64).state, PageState::Corrupt);
    assert!(home.read("objects", o2).is_some());
    // Row: staging verified and installed.
    for (payload, total) in [(o3, 10), (e3, 3)] {
        assert_eq!(
            state(payload),
            (PayloadState::Complete, Location::Objects, total)
        );
        assert!(home.read("staging", payload).is_none());
        assert!(home.read("objects", payload).is_some());
    }
    assert_eq!(page(&f.store, o3, 0, 64).text, "payload 3\n");
    // Row: staging digest mismatch is corrupt; the file stays in staging.
    assert_eq!(state(o4), (PayloadState::Corrupt, Location::Staging, 10));
    assert_eq!(home.read("staging", o4).unwrap(), b"payload X\n");
    assert_eq!(page(&f.store, o4, 0, 64).state, PageState::Corrupt);
    assert_eq!(state(e4), (PayloadState::Incomplete, Location::Objects, 3));
    assert_eq!(page(&f.store, e4, 0, 64).state, PageState::Incomplete);
    // Row: no file is missing.
    for payload in [o5, e5] {
        assert_eq!(state(payload).0, PayloadState::Missing);
        assert_eq!(page(&f.store, payload, 0, 64).state, PageState::Missing);
    }
    // Rows: unconfirmed and interrupted are unverified prefixes in staging.
    for (payload, text) in [(o6, "payload 6\n"), (o8, "payload 8\n")] {
        assert_eq!(
            state(payload),
            (PayloadState::UnverifiedPrefix, Location::Staging, 10)
        );
        let p = page(&f.store, payload, 0, 64);
        assert_eq!(
            (p.state, p.text.as_str(), p.digest),
            (PageState::UnverifiedPrefix, text, None)
        );
    }
    assert_eq!(state(e6).0, PayloadState::UnverifiedPrefix);
    assert_eq!(state(e8).0, PayloadState::UnverifiedPrefix);
    // Row: not_started strays are deleted and the call is not in the ledger.
    for payload in [o7, e7] {
        assert!(home.read("staging", payload).is_none());
        assert_eq!(f.store.payload_state(payload), None);
    }
    // Row: orphans are deleted.
    assert!(home.read("staging", [0xAA; 16]).is_none());
    assert!(home.read("objects", [0xBB; 16]).is_none());
    // Every recovered job is expirable, oldest last use first; the
    // unverified prefixes use their kind 26 sequence.
    assert!(last_use.windows(2).all(|w| w[0] < w[1]) && last_use[4] < intent6 && intent6 < intent8);
    let mut order = Vec::new();
    for ordinal in 100u32.. {
        f.store
            .set_effective_quota(f.store.used() + MIB - 1, 64 * 1024 * MIB);
        let Ok(victims) = f.store.admit(id(1), id(1000 + u64::from(ordinal)), id(99)) else {
            break;
        };
        for victim in victims {
            order.push((1..=8).find(|n| call(*n) == victim.call).unwrap());
            f.store.delete_victim(victim).unwrap();
        }
        f.store.release(id(1000 + u64::from(ordinal)));
    }
    assert_eq!(order, [1, 2, 3, 4, 5, 6, 8]);
    assert_eq!(f.store.used(), 0);
    for directory in ["staging", "objects"] {
        let left = fs::read_dir(home.0.join(".asura/payloads").join(directory))
            .unwrap()
            .count();
        assert_eq!(left, 0, "{directory} is empty after expiry");
    }
}

#[test]
fn j1t_22_configured_quota_bounds_the_effective_quota() {
    let f = open(64 * 1024 * MIB);
    let [o1, e1] = payloads(1);
    f.home.put("objects", o1, &body(1));
    f.home.put("objects", e1, &tail(1));
    let facts = [fact(call(1), 10, settled(CaptureState::Complete, 15, 1))];
    let report = recover_facts(&f.store, &facts, 64 * MIB).unwrap();
    assert_eq!(report.retained, 13);
    assert_eq!((f.store.used(), f.store.quota()), (13, 64 * MIB));
}

#[test]
fn j1t_22_without_replay_every_file_is_an_orphan() {
    let f = open(64 * 1024 * MIB);
    let [o1, _] = payloads(1);
    f.home.put("objects", o1, &body(1));
    f.home.put("staging", [0xAA; 16], b"x");
    let report = recover(&f.store, None, DEFAULT_QUOTA, &no_cancel()).unwrap();
    assert_eq!(report.orphans_deleted, 2);
    assert_eq!(report.retained, 0);
    assert!(f.home.read("objects", o1).is_none());
    assert_eq!(f.store.used(), 0);
}

#[test]
fn j1t_22_unsafe_entries_make_recovery_fail() {
    let cases: [fn(&Home); 4] = [
        |home| {
            std::os::unix::fs::symlink("/etc/hosts", home.file("objects", [0xEE; 16])).unwrap();
        },
        |home| {
            fs::write(home.0.join(".asura/payloads/staging/foreign"), b"x").unwrap();
        },
        |home| {
            let path = home.file("staging", [0xEE; 16]);
            fs::write(&path, b"x").unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        },
        |home| {
            fs::create_dir(home.file("objects", [0xEE; 16])).unwrap();
        },
    ];
    for (index, make_unsafe) in cases.into_iter().enumerate() {
        let f = open(64 * 1024 * MIB);
        let [o1, e1] = payloads(1);
        f.home.put("objects", o1, &body(1));
        f.home.put("objects", e1, &tail(1));
        make_unsafe(&f.home);
        let facts = [fact(call(1), 10, settled(CaptureState::Complete, 15, 1))];
        let result = recover_facts(&f.store, &facts, DEFAULT_QUOTA);
        assert!(
            matches!(result, Err(RecoveryError::Payload(PayloadError::Unsafe))),
            "case {index}: {result:?}"
        );
        // Nothing was registered or deleted.
        assert_eq!(f.store.used(), 0);
        assert_eq!(f.store.payload_state(o1), None);
        assert!(f.home.read("objects", o1).is_some());
    }
}

#[test]
fn j1t_22_cancel_stops_recovery_before_any_change() {
    let f = open(64 * 1024 * MIB);
    f.home.put("staging", [0xAA; 16], b"orphan");
    let [o3, _] = payloads(3);
    f.home.put("staging", o3, &body(3));
    let result = recover(&f.store, None, DEFAULT_QUOTA, &AtomicBool::new(true));
    assert!(matches!(result, Err(RecoveryError::Cancelled)));
    assert!(f.home.read("staging", [0xAA; 16]).is_some());
    assert!(f.home.read("staging", o3).is_some());
    assert_eq!((f.store.used(), f.store.quota()), (0, 0));
}

#[test]
fn j1t_22_interrupted_prefix_without_terminal_is_expirable() {
    let f = open(64 * 1024 * MIB);
    let mut journal = Journal::started();
    // A job left unsettled, with no terminal, as a crash leaves it.
    journal.job(1);
    let [o1, e1] = payloads(1);
    f.home.put("staging", o1, &body(1));
    let report = recover(
        &f.store,
        Some(&journal.replay()),
        DEFAULT_QUOTA,
        &no_cancel(),
    )
    .unwrap();
    assert_eq!((report.unverified, report.retained), (1, 10));
    assert_eq!(
        f.store.payload_state(o1),
        Some((PayloadState::UnverifiedPrefix, Location::Staging, 10))
    );
    assert_eq!(
        f.store.payload_state(e1),
        Some((PayloadState::Missing, Location::None, 0))
    );
    f.store
        .set_effective_quota(f.store.used() + MIB - 1, 64 * 1024 * MIB);
    let victims = f.store.admit(id(1), id(1000), id(99)).unwrap();
    assert_eq!(victims.len(), 1);
    assert_eq!(victims[0].call, call(1));
    f.store.delete_victim(victims[0]).unwrap();
    assert!(f.home.read("staging", o1).is_none());
    assert_eq!(page(&f.store, o1, 0, 64).state, PageState::Expired);
}

/// Install after an injected install fault leaves staging for recovery.
#[test]
fn j1t_22_install_fault_leaves_staging_that_recovery_installs() {
    let f = fixture();
    let ids = admit(&f.store, 1);
    let mut capture = f.store.create_capture(call(1)).unwrap();
    capture
        .append(StreamIndex::Stdout, &redacted(&body(1)), 0)
        .1
        .unwrap();
    capture
        .append(StreamIndex::Stderr, &redacted(&tail(1)), 0)
        .1
        .unwrap();
    capture.sync().unwrap();
    let [(a, da), (b, db)] = capture.seal();
    f.store.settle(
        call(1),
        CaptureState::Complete,
        [(a, Some(da)), (b, Some(db))],
        7,
        true,
    );
    gate::inject(&f.home.0, Point::Install, Fault::Io, 1);
    assert!(matches!(f.store.install(call(1)), Err(PayloadError::Io(_))));
    assert!(f.home.read("staging", ids[0]).is_some());
    // A restart: a fresh store over the same directories.
    let runtime = RuntimeDirectory::scratch(&f.home.0, false).unwrap();
    let restarted = PayloadStore::open(&runtime).unwrap();
    let facts = [fact(call(1), 10, settled(CaptureState::Complete, 15, 1))];
    let report = recover_facts(&restarted, &facts, DEFAULT_QUOTA).unwrap();
    assert_eq!(report.installed, 2);
    assert_eq!(f.home.read("objects", ids[0]).unwrap(), body(1));
    assert!(f.home.read("staging", ids[0]).is_none());
}

// ---------------------------------------------------------------- J2T-04

/// The retired J1 derivation: operation, generation and ordinal instead of
/// the job ID. Payload files that J1 wrote under it no longer match.
fn retired_payload_id(installation: Id, ordinal: u32, kind: u8) -> Id {
    let mut hash = Sha256::new();
    hash.update(b"asura.payload\0");
    hash.update(installation);
    hash.update(id(10));
    hash.update(1u64.to_be_bytes());
    hash.update(ordinal.to_be_bytes());
    hash.update([kind]);
    let mut out = [0; 16];
    out.copy_from_slice(&hash.finalize()[..16]);
    out
}

/// J2T-04 (integration): payload files of the retired J1 derivation are
/// orphans and recovery deletes them; the same call's files under its
/// job-keyed IDs are recovered.
#[test]
fn j2t_04_j1_orphans_of_the_old_derivation_are_deleted_at_recovery() {
    let f = open(64 * 1024 * MIB);
    let [o1, e1] = payloads(1);
    assert_eq!([o1, e1], job_payloads(id(1), call(1)));
    let old = [
        retired_payload_id(id(1), 1, 1),
        retired_payload_id(id(1), 1, 2),
    ];
    assert!(old.iter().all(|id| ![o1, e1].contains(id)));
    f.home.put("objects", old[0], &body(1));
    f.home.put("objects", old[1], &tail(1));
    f.home.put("staging", old[0], &body(1));
    f.home.put("objects", o1, &body(1));
    f.home.put("objects", e1, &tail(1));
    let facts = [fact(call(1), 10, settled(CaptureState::Complete, 15, 1))];
    let report = recover_facts(&f.store, &facts, DEFAULT_QUOTA).unwrap();
    assert_eq!(report.orphans_deleted, 3);
    assert_eq!(report.retained, 13);
    for payload in old {
        assert!(f.home.read("objects", payload).is_none());
        assert!(f.home.read("staging", payload).is_none());
        assert_eq!(f.store.payload_state(payload), None);
    }
    assert_eq!(
        f.store.payload_state(o1),
        Some((PayloadState::Complete, Location::Objects, 10))
    );
    assert_eq!(page(&f.store, o1, 0, 64).text, "payload 1\n");
}

/// J2T-04 (integration): a live job's capture stages under the job's
/// payload IDs, for the model and the person origin alike.
#[test]
fn j2t_04_job_capture_stages_under_job_keyed_ids() {
    let f = fixture();
    for origin in [
        JobOrigin::Model {
            operation: id(10),
            generation: 1,
            ordinal: 9,
        },
        JobOrigin::Person { input: id(77) },
    ] {
        let job = job_id(&origin);
        assert!(f.store.admit(id(1), job, id(10)).unwrap().is_empty());
        let [out, err] = job_payloads(id(1), job);
        assert_eq!(out, payload_id(id(1), job, ObjectKind::Stdout));
        assert_eq!(err, payload_id(id(1), job, ObjectKind::Stderr));
        let mut capture = f.store.create_capture(job).unwrap();
        assert_eq!(capture.call(), job);
        capture
            .append(StreamIndex::Stdout, &redacted(b"job\n"), 0)
            .1
            .unwrap();
        capture.sync().unwrap();
        assert_eq!(f.home.read("staging", out).unwrap(), b"job\n");
        assert_eq!(f.home.read("staging", err).unwrap(), b"");
        let [(a, da), (b, db)] = capture.seal();
        f.store.settle(
            job,
            CaptureState::Complete,
            [(a, Some(da)), (b, Some(db))],
            5,
            false,
        );
        f.store.install(job).unwrap();
        assert_eq!(f.home.read("objects", out).unwrap(), b"job\n");
        assert_eq!(page(&f.store, out, 0, 64).state, PageState::Complete);
    }
}

/// One `PayloadFacts` answer of the job form per payload, as the journal
/// writer builds it: `call` is the job ID, `intent_sequence` the kind 26
/// sequence and `settled` the kind 28 capture facts (none when unsettled or
/// `interrupted`); an unsettled job is `active`.
fn job_fact(job: Id, intent: u64, settled: Option<Settlement>, active: bool) -> Vec<PayloadFact> {
    let first = PayloadFact {
        active,
        ..fact(job, intent, settled)
    };
    first
        .payloads
        .iter()
        .map(|payload| PayloadFact {
            payload: *payload,
            ..first.clone()
        })
        .collect()
}
/// A facts answer for `recover_with_facts` from a fixed list.
fn answer(
    facts: &[PayloadFact],
) -> impl FnMut(Vec<Id>) -> Result<Vec<PayloadFact>, RecoveryError> + '_ {
    move |batch: Vec<Id>| {
        assert!(!batch.is_empty() && batch.len() <= FACT_BATCH);
        Ok(facts
            .iter()
            .filter(|fact| batch.contains(&fact.payload))
            .cloned()
            .collect())
    }
}

/// J2T-04 and TJ-Q5 (integration): job payload recovery from `PayloadFacts`
/// maps each job's state from kinds 26 and 28: `complete` or `incomplete`
/// verifies and installs from staging; `unconfirmed`, no kind 28 and an
/// `interrupted` kind 28 keep an unverified prefix, eligible for interim
/// expiry by kind 26 sequence; `not_started` deletes stray staging files;
/// files no job derives are orphans. Facts are asked in batches of at most
/// 128 payload IDs.
#[test]
fn j2t_04_job_recovery_from_payload_facts() {
    let f = open(64 * 1024 * MIB);
    let home = &f.home;
    let job = |n: u32| {
        job_id(&JobOrigin::Model {
            operation: id(10),
            generation: 1,
            ordinal: n,
        })
    };
    let ids = |n: u32| job_payloads(id(1), job(n));
    let mut facts = Vec::new();
    // Job 1: complete, staging only, digests match: installed.
    let [o1, e1] = ids(1);
    home.put("staging", o1, &body(1));
    home.put("staging", e1, &tail(1));
    facts.extend(job_fact(
        job(1),
        10,
        Some((CaptureState::Complete, 40, recorded(1))),
        false,
    ));
    // Job 2: incomplete, object installed.
    let [o2, e2] = ids(2);
    home.put("objects", o2, &body(2));
    home.put("objects", e2, &tail(2));
    facts.extend(job_fact(
        job(2),
        11,
        Some((CaptureState::Incomplete, 41, recorded(2))),
        false,
    ));
    // Job 3: unconfirmed prefix.
    let [o3, _] = ids(3);
    home.put("staging", o3, &body(3));
    facts.extend(job_fact(
        job(3),
        12,
        Some((CaptureState::Unconfirmed, 42, [(10, None), (0, None)])),
        false,
    ));
    // Job 4: not_started with stray staging files.
    let [o4, e4] = ids(4);
    home.put("staging", o4, b"");
    home.put("staging", e4, b"");
    facts.extend(job_fact(
        job(4),
        13,
        Some((CaptureState::NotStarted, 43, [(0, None), (0, None)])),
        false,
    ));
    // Job 5: unsettled (no kind 28), active.
    let [o5, e5] = ids(5);
    home.put("staging", o5, &body(5));
    home.put("staging", e5, &tail(5));
    facts.extend(job_fact(job(5), 14, None, true));
    // Job 6: an `interrupted` kind 28 carries no capture facts.
    let [o6, _] = ids(6);
    home.put("staging", o6, &body(6));
    facts.extend(job_fact(job(6), 15, None, false));
    // Orphans: no job derives them; 300 of them to exercise batching.
    for n in 0..300u64 {
        home.put("staging", id(0xF000_0000 + n), b"orphan");
    }
    let mut asked = Vec::new();
    let mut facts_of = answer(&facts);
    let mut answer = |batch: Vec<Id>| -> Result<Vec<PayloadFact>, RecoveryError> {
        asked.push(batch.len());
        facts_of(batch)
    };
    let report = recover_with_facts(&f.store, &mut answer, DEFAULT_QUOTA, &no_cancel()).unwrap();
    assert_eq!(asked.iter().sum::<usize>(), 300 + 10);
    assert!(asked.len() >= 3);
    assert_eq!(report.installed, 2);
    assert_eq!(report.corrupt, 0);
    assert_eq!(report.missing, 0);
    assert_eq!(report.unverified, 4);
    assert_eq!(report.orphans_deleted, 300);
    assert_eq!(report.strays_deleted, 2);
    // 13 + 13 + 10 + 13 + 10.
    assert_eq!(report.retained, 59);
    assert_eq!(f.store.used(), 59);
    let state = |payload| f.store.payload_state(payload).unwrap();
    for (payload, total) in [(o1, 10), (e1, 3)] {
        assert_eq!(
            state(payload),
            (PayloadState::Complete, Location::Objects, total)
        );
        assert!(home.read("staging", payload).is_none());
    }
    assert_eq!(page(&f.store, o1, 0, 64).digest, Some(sha(&body(1))));
    assert_eq!(state(o2), (PayloadState::Incomplete, Location::Objects, 10));
    for payload in [o3, o5, e5, o6] {
        assert_eq!(state(payload).0, PayloadState::UnverifiedPrefix);
        assert_eq!(
            page(&f.store, payload, 0, 64).state,
            PageState::UnverifiedPrefix
        );
    }
    for payload in [o4, e4] {
        assert!(home.read("staging", payload).is_none());
        assert_eq!(f.store.payload_state(payload), None);
    }
    // Interim expiry: last use is kind 28 when settled with capture facts,
    // else kind 26: jobs 3 to 6 (12..15) before jobs 1 and 2 (40, 41).
    let mut order = Vec::new();
    for n in 0u64.. {
        f.store
            .set_effective_quota(f.store.used() + MIB - 1, 64 * 1024 * MIB);
        let Ok(victims) = f.store.admit(id(1), id(5000 + n), id(99)) else {
            break;
        };
        for victim in victims {
            order.push((1..=6).find(|k| job(*k) == victim.call).unwrap());
            f.store.delete_victim(victim).unwrap();
        }
        f.store.release(id(5000 + n));
    }
    assert_eq!(order, [3, 5, 6, 1, 2]);
    assert_eq!(f.store.used(), 0);
}

/// J2T-04 (integration): a replay's unsettled job is known to recovery: its
/// staging prefix is kept as an unverified prefix with the kind 26 sequence
/// as last use, never deleted as an orphan; a job with no files has two
/// missing payloads.
#[test]
fn j2t_04_unsettled_job_from_replay_keeps_its_prefix() {
    let f = open(64 * 1024 * MIB);
    let mut journal = Journal::started();
    let (first, first_intent) = journal.job(1);
    let (job, intent) = journal.job(2);
    // The jobs outlive their turn.
    journal.failed();
    assert_eq!(job, call(2), "an exec_cmd call's job is its call's");
    let [o2, e2] = job_payloads(id(1), job);
    assert_eq!([o2, e2], payloads(2));
    f.home.put("staging", o2, &body(2));
    let replay = journal.replay();
    assert_eq!(replay.jobs[&job].intent.sequence, intent);
    assert_eq!(replay.jobs[&first].intent.sequence, first_intent);
    let known = asura_storage::payload::known_calls(&replay);
    assert_eq!(known.len(), 2, "both unsettled jobs");
    let report = recover(&f.store, Some(&replay), DEFAULT_QUOTA, &no_cancel()).unwrap();
    assert_eq!(report.orphans_deleted, 0);
    assert_eq!(report.unverified, 1);
    assert_eq!(
        f.store.payload_state(o2),
        Some((PayloadState::UnverifiedPrefix, Location::Staging, 10))
    );
    assert_eq!(
        f.store.payload_state(e2),
        Some((PayloadState::Missing, Location::None, 0))
    );
    f.store
        .set_effective_quota(f.store.used() + MIB - 1, 64 * 1024 * MIB);
    for payload in job_payloads(id(1), first) {
        assert_eq!(
            f.store.payload_state(payload),
            Some((PayloadState::Missing, Location::None, 0))
        );
    }
    // Oldest kind 26 first: the empty job 1, then job 2's prefix.
    let victims = f.store.admit(id(1), id(6000), id(99)).unwrap();
    assert_eq!(
        victims.iter().map(|v| v.call).collect::<Vec<_>>(),
        [first, job]
    );
}

// ---------------------------------------------------------------- J3T-08

/// J3T-08 (storage): the delivered text of a page never exceeds 8,192
/// bytes. Output that is four fifths invalid bytes (each 3 bytes of U+FFFD)
/// makes the reader stop before 8,192 source bytes; following
/// `next_offset` reads every byte once and reassembles the lossy text.
#[test]
fn j3t_08_page_text_never_exceeds_the_bound() {
    let f = open(64 * 1024 * MIB);
    let source: Vec<u8> = (0..3 * PAGE_LIMIT)
        .map(|n| if n % 5 == 0 { b'a' } else { 0xFF })
        .collect();
    let [o1, e1] = payloads(1);
    f.home.put("objects", o1, &source);
    f.home.put("objects", e1, b"");
    let facts = [fact(
        call(1),
        10,
        Some((
            CaptureState::Complete,
            15,
            [
                (source.len() as u64, Some(sha(&source))),
                (0, Some(sha(b""))),
            ],
        )),
    )];
    recover_facts(&f.store, &facts, DEFAULT_QUOTA).unwrap();
    let first = page(&f.store, o1, 0, PAGE_LIMIT);
    assert_eq!(first.state, PageState::Complete);
    assert!(first.text.len() <= TEXT_LIMIT);
    assert!(
        first.next_offset < PAGE_LIMIT as u64,
        "the reader stopped early"
    );
    let mut offset = 0;
    let mut pages = 0;
    let mut joined = String::new();
    while offset < source.len() as u64 {
        let p = page(&f.store, o1, offset, PAGE_LIMIT);
        assert!(p.text.len() <= TEXT_LIMIT && p.next_offset > offset);
        assert_eq!(p.start, offset);
        joined.push_str(&p.text);
        offset = p.next_offset;
        pages += 1;
    }
    assert_eq!(joined, String::from_utf8_lossy(&source));
    assert!(pages > 3);
}

// ---------------------------------------------------------------- J4T-06

/// Counts space events, as the J2 queue-head wake and a paused capture's
/// retry receive them.
#[derive(Default)]
struct Wakes(std::sync::atomic::AtomicU32);
impl SpaceListener for Wakes {
    fn space_event(&self) {
        self.0.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
    }
}
impl Wakes {
    fn count(&self) -> u32 {
        self.0.load(std::sync::atomic::Ordering::Acquire)
    }
}
fn listen(store: &PayloadStore) -> Arc<Wakes> {
    let wakes = Arc::new(Wakes::default());
    let listener: Arc<dyn SpaceListener> = wakes.clone();
    store.listen(&listener);
    wakes
}

/// J4T-06 (storage): a pinned job survives interim expiry under quota
/// pressure; its files and pages stay. Unpinned, it is a victim again.
#[test]
fn j4t_06_pin_survives_interim_expiry_under_quota_pressure() {
    let f = fixture();
    let [o1, e1] = settled_call(&f, 1, CaptureState::Complete, b"keep\n", b"", 1, true);
    let [o2, _] = settled_call(&f, 2, CaptureState::Complete, b"drop\n", b"", 2, true);
    f.store.set_pinned(call(1), true);
    f.store
        .set_effective_quota(f.store.used() + MIB - 1, 64 * 1024 * MIB);
    // Job 1 is the oldest; the pin makes job 2 the victim.
    let victims = f.store.admit(id(1), call(3), id(10)).unwrap();
    assert_eq!(
        victims.iter().map(|v| v.call).collect::<Vec<_>>(),
        [call(2)]
    );
    f.store.delete_victim(victims[0]).unwrap();
    assert_eq!(page(&f.store, o2, 0, 64).state, PageState::Expired);
    f.store.release(call(3));
    // Under quota pressure only the pinned job is left: nothing is chosen.
    f.store
        .set_effective_quota(f.store.used() + MIB - 1, 64 * 1024 * MIB);
    assert!(f.store.admit(id(1), call(4), id(10)).is_err());
    assert_eq!(f.home.read("objects", o1).unwrap(), b"keep\n");
    assert!(f.home.read("objects", e1).is_some());
    assert_eq!(page(&f.store, o1, 0, 64).text, "keep\n");
    let wakes = listen(&f.store);
    f.store.set_pinned(call(1), false);
    assert_eq!(wakes.count(), 1, "unpin wakes waiters");
    let victims = f.store.admit(id(1), call(4), id(10)).unwrap();
    assert_eq!(
        victims.iter().map(|v| v.call).collect::<Vec<_>>(),
        [call(1)]
    );
}

/// J4T-06 (storage): a person delete removes both stream files, frees their
/// quota and wakes a `quota` head; pages answer `deleted`. A live job is
/// refused and keeps its files. Deleting again is `Ok`.
#[test]
fn j4t_06_delete_frees_quota_and_wakes_a_quota_head() {
    let f = fixture();
    let [o1, e1] = settled_call(&f, 1, CaptureState::Complete, b"out\n", b"err\n", 1, true);
    // A staging copy is deleted too.
    f.home.put("staging", e1, b"err\n");
    // A live job.
    admit(&f.store, 3);
    let mut capture = f.store.create_capture(call(3)).unwrap();
    // The pin keeps expiry from freeing the space: only the delete can.
    f.store.set_pinned(call(1), true);
    let wakes = listen(&f.store);
    f.store
        .set_effective_quota(f.store.used() + MIB - 1, 64 * 1024 * MIB);
    assert!(
        f.store.admit(id(1), call(2), id(10)).is_err(),
        "a quota head"
    );
    // A live job's output cannot be deleted.
    capture
        .append(StreamIndex::Stdout, &redacted(b"live\n"), 0)
        .1
        .unwrap();
    assert!(matches!(
        f.store.delete_job(id(1), call(3)),
        Err(DeleteError::NotSettled)
    ));
    assert_eq!(f.home.read("staging", payloads(3)[0]).unwrap(), b"live\n");
    assert_eq!(page(&f.store, payloads(3)[0], 0, 64).state, PageState::Live);
    let used = f.store.used();
    let before = wakes.count();
    f.store.delete_job(id(1), call(1)).unwrap();
    assert_eq!(f.store.used(), used - 8, "the 8 retained bytes are free");
    assert!(wakes.count() > before, "freed bytes wake the quota head");
    for payload in [o1, e1] {
        assert!(f.home.read("objects", payload).is_none());
        assert!(f.home.read("staging", payload).is_none());
        for offset in [0, 2, 99] {
            let p = page(&f.store, payload, offset, 64);
            assert_eq!(
                (p.state, p.text.as_str(), p.next_offset),
                (PageState::Deleted, "", offset)
            );
        }
    }
    // The woken head is admitted now.
    assert!(f.store.admit(id(1), call(2), id(10)).unwrap().is_empty());
    f.store.delete_job(id(1), call(1)).unwrap();
    assert_eq!(page(&f.store, o1, 0, 64).state, PageState::Deleted);
}

/// J4T-06 (storage): a crash after the kind 30 `delete` committed and before
/// the files were gone; recovery sees the delete in `PayloadFacts` and
/// completes it: files deleted, nothing retained, pages `deleted`. A pinned
/// job keeps its pin hold across the restart.
#[test]
fn j4t_06_crash_after_delete_commit_completes_at_recovery() {
    let f = open(64 * 1024 * MIB);
    let [o1, e1] = payloads(1);
    f.home.put("objects", o1, &body(1));
    f.home.put("objects", e1, &tail(1));
    f.home.put("staging", e1, &tail(1));
    let [o2, e2] = payloads(2);
    f.home.put("objects", o2, &body(2));
    f.home.put("objects", e2, &tail(2));
    let [o3, e3] = payloads(3);
    f.home.put("objects", o3, &body(3));
    f.home.put("objects", e3, &tail(3));
    let mut facts = Vec::new();
    for (n, pinned, deleted) in [(1, true, true), (2, true, false), (3, false, false)] {
        for mut fact in job_fact(
            call(n),
            u64::from(n) * 10,
            settled(CaptureState::Complete, u64::from(n) * 10 + 5, n),
            false,
        ) {
            fact.pinned = pinned;
            fact.deleted = deleted;
            facts.push(fact);
        }
    }
    let report =
        recover_with_facts(&f.store, &mut answer(&facts), DEFAULT_QUOTA, &no_cancel()).unwrap();
    assert_eq!(report.deleted, 3);
    assert_eq!(report.orphans_deleted, 0);
    assert_eq!(report.retained, 26);
    assert_eq!(f.store.used(), 26);
    for payload in [o1, e1] {
        assert!(f.home.read("objects", payload).is_none());
        assert!(f.home.read("staging", payload).is_none());
        assert_eq!(f.store.payload_state(payload), None);
        assert_eq!(page(&f.store, payload, 0, 64).state, PageState::Deleted);
    }
    assert!(f.store.pinned(call(2)) && !f.store.pinned(call(3)));
    assert_eq!(page(&f.store, o2, 0, 64).text, "payload 2\n");
    // Under quota pressure job 3 expires; the pinned job 2 never does.
    let mut order = Vec::new();
    for n in 0u64.. {
        f.store
            .set_effective_quota(f.store.used() + MIB - 1, 64 * 1024 * MIB);
        let Ok(victims) = f.store.admit(id(1), id(7000 + n), id(99)) else {
            break;
        };
        for victim in victims {
            order.push(victim.call);
            f.store.delete_victim(victim).unwrap();
        }
        f.store.release(id(7000 + n));
    }
    assert_eq!(order, [call(3)]);
    assert_eq!(f.home.read("objects", o2).unwrap(), body(2));
}

// ---------------------------------------------------------------- J3T-22

fn file_len(home: &Home, directory: &str, payload: Id) -> u64 {
    fs::metadata(home.file(directory, payload)).unwrap().len()
}

/// J3T-22 (storage): during a real capture, `Ledger::stored` equals the
/// staging file sizes after each append of either stream; after settlement
/// and install it is the recorded total; an unknown job is `None`.
#[test]
fn j3t_22_stored_bytes_equal_the_staging_file_sizes_after_each_append() {
    let f = fixture();
    assert_eq!(f.store.stored(call(1)), None);
    let [out, err] = admit(&f.store, 1);
    assert_eq!(f.store.stored(call(1)), Some([0, 0]));
    let mut capture = f.store.create_capture(call(1)).unwrap();
    assert_eq!(f.store.stored(call(1)), Some([0, 0]));
    let chunks: [(StreamIndex, &[u8]); 5] = [
        (StreamIndex::Stdout, b"one\n"),
        (StreamIndex::Stderr, b"warning: two\n"),
        (StreamIndex::Stdout, b"three three\n"),
        (StreamIndex::Stdout, b""),
        (StreamIndex::Stderr, b"four\n"),
    ];
    for (stream, bytes) in chunks {
        let (written, result) = capture.append(stream, &redacted(bytes), 0);
        result.unwrap();
        assert_eq!(written, bytes.len());
        let sizes = [
            file_len(&f.home, "staging", out),
            file_len(&f.home, "staging", err),
        ];
        assert_eq!(f.store.stored(call(1)), Some(sizes));
        assert_eq!(
            sizes,
            [
                capture.stored(StreamIndex::Stdout),
                capture.stored(StreamIndex::Stderr)
            ]
        );
    }
    assert_eq!(f.store.stored(call(1)), Some([16, 18]));
    let [(a, da), (b, db)] = capture.seal();
    f.store.settle(
        call(1),
        CaptureState::Complete,
        [(a, Some(da)), (b, Some(db))],
        5,
        false,
    );
    assert_eq!(f.store.stored(call(1)), Some([16, 18]));
    f.store.install(call(1)).unwrap();
    assert_eq!(
        f.store.stored(call(1)),
        Some([
            file_len(&f.home, "objects", out),
            file_len(&f.home, "objects", err)
        ])
    );
}
