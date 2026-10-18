//! J1 payload store against private scratch homes: pages (J1T-10), the
//! page-read pool (J1T-11), staging captures (J1T-19) and derived recovery
//! (J1T-22). Ledger arithmetic (J1T-08) is unit-tested in `payload/store.rs`.
use asura_platform::RuntimeDirectory;
use asura_platform::payload_io::gate::{self, Fault, Point};
use asura_platform::secrets::{JobRedaction, Redacted, Stream, detection};
use asura_storage::authority::conversation::{
    ActiveBinding, ArgumentForm, CaptureReason, CaptureSettled, CaptureState, CaptureStream, Cause,
    Composition, Hash, INITIAL_RESERVATION, Id, PendingInit, ProjectRegistered, Record, Replay,
    Request, StartAuthorized, TerminalKind, ToolCall, ToolCallSource, ToolDecision, ToolOperation,
    ToolResult, TurnAccepted, TurnTerminal, encode_frame, exec_cmd_job_id, replay, request_digest,
};
use asura_storage::authority::conversation::{FrameContext, SecretKind};
use asura_storage::payload::{
    DEFAULT_QUOTA, Location, MIB, ObjectKind, Origin, PAGE_STEP, Page, PagePool, PageRequest,
    PageState, PayloadError, PayloadState, PayloadStore, RECOVERY_RESERVE, RecoveryError,
    StreamIndex, Victim, payload_id, read_page, recover,
};
use sha2::{Digest, Sha256};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Instant;

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

// Calls of operation id(10), generation 1, installation id(1).
fn payloads(ordinal: u32) -> [Id; 2] {
    [
        payload_id(id(1), id(10), 1, ordinal, ObjectKind::Stdout),
        payload_id(id(1), id(10), 1, ordinal, ObjectKind::Stderr),
    ]
}
fn call(ordinal: u32) -> Id {
    exec_cmd_job_id(id(10), 1, ordinal)
}
fn admit(store: &PayloadStore, ordinal: u32) -> [Id; 2] {
    let victims = store
        .admit(id(1), id(10), 1, ordinal, call(ordinal))
        .unwrap();
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
    let victims = f.store.admit(id(1), id(10), 1, 4, call(4)).unwrap();
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

// ---------------------------------------------------------------- J1T-11

#[test]
fn j1t_11_reserved_model_worker_queue_limit_and_abandoned_tickets() {
    let f = fixture();
    let [out, _] = settled_call(&f, 1, CaptureState::Complete, b"pool\n", b"", 1, true);
    let pool = PagePool::start(f.store.clone()).unwrap();
    let request = PageRequest {
        payload: out,
        offset: 0,
        limit: 64,
        scope: None,
    };
    pool.hold_for_test(true);
    // The general worker takes a client request and stays held.
    let client = pool.submit(request, Origin::Client).unwrap();
    wait_until(|| pool.queued() == 0);
    assert!(client.poll(Instant::now()).is_none());
    // The reserved worker still takes a model request.
    let model = pool.submit(request, Origin::Model).unwrap();
    wait_until(|| pool.queued() == 0);
    // Client requests never take the reserved worker: they queue.
    let mut queued = Vec::new();
    for n in 1..=8 {
        queued.push(pool.submit(request, Origin::Client).unwrap());
        assert_eq!(pool.queued(), n);
    }
    // The ninth queued request, client or model, is busy.
    assert_eq!(
        pool.submit(request, Origin::Client).err(),
        Some(PageState::Busy)
    );
    assert_eq!(
        pool.submit(request, Origin::Model).err(),
        Some(PageState::Busy)
    );
    // After its step a ticket answers busy and abandons its request: the
    // running one on the general worker and the first queued one.
    let late = Instant::now() + PAGE_STEP;
    assert_eq!(client.poll(late).unwrap().state, PageState::Busy);
    assert_eq!(queued[0].poll(late).unwrap().state, PageState::Busy);
    pool.hold_for_test(false);
    let page = model.wait();
    assert_eq!(
        (page.state, page.text.as_str()),
        (PageState::Complete, "pool\n")
    );
    // The general worker serves the queue in order; the last one answers
    // only after both abandoned jobs were passed.
    for ticket in &queued[1..] {
        assert_eq!(ticket.wait().text, "pool\n");
    }
    // Abandoned jobs were skipped: their slots never received a page.
    for ticket in [&client, &queued[0]] {
        assert!(
            ticket
                .poll(Instant::now())
                .is_none_or(|p| p.state == PageState::Busy)
        );
    }
    pool.close();
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
            reserved_output_tokens: 2048,
            event_cursor: 1,
            composition: Composition::default(),
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
            busy: None,
            form: ArgumentForm::Canonical,
            arguments_bytes: 40,
            arguments: "{\"command\":\"printf \\\"$ASURA_SECRET_1\\\"\",\"cwd\":\".\"}".into(),
            number_kinds: vec![SecretKind::GithubToken],
            unnumbered: 0,
            rule_set_revision: 1,
            rule_set_digest: [21; 32],
            command_cut: false,
            refusal: None,
        })))
    }
    /// Kind 23 then kind 15; returns the kind 23 sequence.
    fn settled(
        &mut self,
        ordinal: u32,
        state: CaptureState,
        streams: [(u64, Option<Hash>); 2],
    ) -> u64 {
        let stream = |(stored, digest): (u64, Option<Hash>)| CaptureStream {
            observed_bytes: stored,
            stored_bytes: stored,
            digest,
            eof: state == CaptureState::Complete,
            ..CaptureStream::default()
        };
        let (reason, exit_kind, exit_value) = match state {
            CaptureState::Complete => (CaptureReason::None, 1, 0),
            CaptureState::Incomplete => (CaptureReason::Quota, 1, 0),
            CaptureState::Unconfirmed => (CaptureReason::SettlementUnconfirmed, 1, 0),
            CaptureState::NotStarted => (CaptureReason::StorageFault, 0, 0),
        };
        let sequence = self.append(Record::CaptureSettled(Box::new(CaptureSettled {
            operation: id(10),
            generation: 1,
            ordinal,
            state,
            reason,
            exit_kind,
            exit_value,
            stdout: stream(streams[0]),
            stderr: stream(streams[1]),
            distinct_numbers: 0,
            numbers: vec![],
            pause_count: 0,
            first_pause_ms: 0,
            paused_ms: 0,
            final_reservation: INITIAL_RESERVATION,
        })));
        self.append(Record::ToolResult(ToolResult {
            operation: id(10),
            generation: 1,
            ordinal,
            status: 1,
            text: "failure=None exit_code=0 signal=none reason=none truncated=false cleanup_confirmed=true\nstdout:\n\nstderr:\n".into(),
            next_offset: None,
            truncated: false,
        }));
        sequence
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
            charged_tokens: 2048,
            text: "reply".into(),
        }));
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

#[test]
fn j1t_22_derived_recovery_applies_every_table_row() {
    let f = open(20 * MIB);
    let home = &f.home;
    let mut journal = Journal::started();
    let mut last_use = Vec::new();
    // 1: complete, object installed (plus a stray staging copy).
    journal.admitted(1);
    last_use.push(journal.settled(1, CaptureState::Complete, recorded(1)));
    let [o1, e1] = payloads(1);
    home.put("objects", o1, &body(1));
    home.put("objects", e1, &tail(1));
    home.put("staging", o1, &body(1));
    // 2: complete, object whose length differs from kind 23.
    journal.admitted(2);
    last_use.push(journal.settled(2, CaptureState::Complete, recorded(2)));
    let [o2, e2] = payloads(2);
    home.put("objects", o2, b"payload 2!\n");
    home.put("objects", e2, &tail(2));
    // 3: complete, staging only, digest matches: installed.
    journal.admitted(3);
    last_use.push(journal.settled(3, CaptureState::Complete, recorded(3)));
    let [o3, e3] = payloads(3);
    home.put("staging", o3, &body(3));
    home.put("staging", e3, &tail(3));
    // 4: incomplete, staging only; stdout digest mismatch (same length).
    journal.admitted(4);
    last_use.push(journal.settled(4, CaptureState::Incomplete, recorded(4)));
    let [o4, e4] = payloads(4);
    home.put("staging", o4, b"payload X\n");
    home.put("staging", e4, &tail(4));
    // 5: complete, no files.
    journal.admitted(5);
    last_use.push(journal.settled(5, CaptureState::Complete, recorded(5)));
    let [o5, e5] = payloads(5);
    // 6: unconfirmed, staging prefix.
    let intent6 = journal.admitted(6);
    journal.settled(6, CaptureState::Unconfirmed, [(10, None), (3, None)]);
    let [o6, e6] = payloads(6);
    home.put("staging", o6, &body(6));
    home.put("staging", e6, &tail(6));
    // 7: not_started with stray staging files.
    journal.admitted(7);
    journal.settled(7, CaptureState::NotStarted, [(0, None), (0, None)]);
    let [o7, e7] = payloads(7);
    home.put("staging", o7, b"");
    home.put("staging", e7, b"");
    // 8: admitted kind 22 without kind 23, then a terminal.
    let intent8 = journal.admitted(8);
    journal.failed();
    let [o8, e8] = payloads(8);
    home.put("staging", o8, &body(8));
    home.put("staging", e8, &tail(8));
    // Orphans: no kind 22 names them.
    home.put("staging", [0xAA; 16], b"orphan");
    home.put("objects", [0xBB; 16], b"orphan");
    let replay = journal.replay();
    let report = recover(&f.store, Some(&replay), DEFAULT_QUOTA, &no_cancel()).unwrap();
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
    // Every recovered call is expirable, oldest last use first; the
    // unverified prefixes use their kind 22 sequence.
    assert!(last_use.windows(2).all(|w| w[0] < w[1]) && last_use[4] < intent6 && intent6 < intent8);
    let mut order = Vec::new();
    for ordinal in 100.. {
        f.store
            .set_effective_quota(f.store.used() + MIB - 1, 64 * 1024 * MIB);
        let Ok(victims) = f
            .store
            .admit(id(1), id(99), 1, ordinal, id(1000 + u64::from(ordinal)))
        else {
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
    let mut journal = Journal::started();
    journal.admitted(1);
    journal.settled(1, CaptureState::Complete, recorded(1));
    let [o1, e1] = payloads(1);
    f.home.put("objects", o1, &body(1));
    f.home.put("objects", e1, &tail(1));
    let report = recover(&f.store, Some(&journal.replay()), 64 * MIB, &no_cancel()).unwrap();
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
        let mut journal = Journal::started();
        journal.admitted(1);
        journal.settled(1, CaptureState::Complete, recorded(1));
        let result = recover(
            &f.store,
            Some(&journal.replay()),
            DEFAULT_QUOTA,
            &no_cancel(),
        );
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
    journal.admitted(1);
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
    let victims = f.store.admit(id(1), id(99), 1, 1, id(1000)).unwrap();
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
    let mut journal = Journal::started();
    journal.admitted(1);
    journal.settled(1, CaptureState::Complete, recorded(1));
    let report = recover(
        &restarted,
        Some(&journal.replay()),
        DEFAULT_QUOTA,
        &no_cancel(),
    )
    .unwrap();
    assert_eq!(report.installed, 2);
    assert_eq!(f.home.read("objects", ids[0]).unwrap(), body(1));
    assert!(f.home.read("staging", ids[0]).is_none());
}
