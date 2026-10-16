use super::*;
use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
fn request() -> ShellRequest {
    ShellRequest {
        job_id: [7; 16],
        command: "printf ok".into(),
        cwd: None,
        deadline: None,
        secrets: Vec::new(),
    }
}
#[test]
fn arguments_reject_effects_outside_contract() {
    let mut r = request();
    assert!(validate(&r).is_ok());
    for cwd in ["../other", "a/../b", "/tmp", "", "x\0y"] {
        r.cwd = Some(cwd.into());
        assert_eq!(validate(&r), Err(ShellError::Invalid));
    }
    for cwd in [".", "./subdir", "a//b/"] {
        r.cwd = Some(cwd.into());
        assert!(validate(&r).is_ok());
    }
    r.cwd = None;
    r.command = "x".repeat(8192);
    assert!(validate(&r).is_ok());
    r.command.push('x');
    assert_eq!(validate(&r), Err(ShellError::Invalid));
    r.command = "x\0y".into();
    assert_eq!(validate(&r), Err(ShellError::Invalid));
}
/// The service-side half of SDU-13: the request's secret variables are
/// checked before any descriptor or process work.
#[test]
fn secret_variables_reject_numbers_duplicates_nul_and_size() {
    let mut r = request();
    r.secrets = (1..=16)
        .map(|n| SecretVariable::for_test(n, &[b'v'; 512]))
        .collect();
    assert!(validate(&r).is_ok(), "16 values of 8192 bytes in total");
    r.secrets[15] = SecretVariable::for_test(16, &[b'v'; 513]);
    assert_eq!(validate(&r), Err(ShellError::Invalid), "8193 bytes");
    for bad in [
        vec![SecretVariable::for_test(0, b"value")],
        vec![SecretVariable::for_test(17, b"value")],
        vec![
            SecretVariable::for_test(1, b"value"),
            SecretVariable::for_test(1, b"other"),
        ],
        vec![SecretVariable::for_test(1, b"va\0lue")],
        vec![SecretVariable::for_test(1, &[b'v'; 8193])],
    ] {
        r.secrets = bad;
        assert_eq!(validate(&r), Err(ShellError::Invalid));
    }
    r.secrets = vec![
        SecretVariable::for_test(3, b"a b\n$c"),
        SecretVariable::for_test(1, b""),
    ];
    assert!(validate(&r).is_ok(), "numbers need not be contiguous");
    let environment = secret_environment(&r.secrets);
    let entries: Vec<&[u8]> = environment.iter().map(|e| e.as_slice()).collect();
    assert_eq!(
        entries,
        [&b"ASURA_SECRET_3=a b\n$c"[..], &b"ASURA_SECRET_1="[..]]
    );
}
#[test]
fn notices_reject_identity_reserved_and_invalid_outcomes() {
    let id = [9; 16];
    let n = Notice {
        kind: 2,
        reason: 1,
        exit_kind: 2,
        cleanup: true,
        value: 15,
    };
    let valid = n.encode(id);
    assert!(Notice::decode(&valid, id).is_ok());
    for index in [0, 7, 8, 28] {
        let mut b = valid;
        b[index] ^= 0xff;
        assert!(Notice::decode(&b, id).is_err());
    }
    let mut b = valid;
    b[6] = 0;
    assert!(Notice::decode(&b, id).is_err());
    let mut b = valid;
    b[4] = 1;
    assert!(Notice::decode(&b, id).is_err());
    let bad = Notice {
        kind: 3,
        reason: 0,
        exit_kind: 0,
        cleanup: true,
        value: 0,
    }
    .encode(id);
    assert!(Notice::decode(&bad, id).is_err());
    assert!(Notice::decode(&valid[..31], id).is_err());
}
/// A sink with a settable room that records what the reactor side gave it.
#[derive(Default)]
struct TestSink {
    room: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    accepted: [Vec<u8>; 2],
    discarded: [u64; 2],
    ended: [bool; 2],
}
fn index(stream: Stream) -> usize {
    match stream {
        Stream::Stdout => 0,
        Stream::Stderr => 1,
    }
}
impl OutputSink for TestSink {
    fn room(&self) -> usize {
        self.room.load(Ordering::Acquire)
    }
    fn accept(&mut self, stream: Stream, bytes: &[u8]) {
        assert!(bytes.len() <= self.room(), "accept beyond room");
        self.room.fetch_sub(bytes.len(), Ordering::AcqRel);
        self.accepted[index(stream)].extend_from_slice(bytes);
    }
    fn discard(&mut self, stream: Stream, bytes: u64) {
        self.discarded[index(stream)] += bytes;
    }
    fn end(&mut self, stream: Stream) {
        assert!(!self.ended[index(stream)], "end twice");
        self.ended[index(stream)] = true;
    }
}
fn write_all(fd: &std::os::fd::OwnedFd, bytes: &[u8]) {
    // SAFETY: owned pipe and bounded valid input below the pipe buffer.
    assert_eq!(
        unsafe { libc::write(fd.as_raw_fd(), bytes.as_ptr().cast(), bytes.len()) },
        bytes.len() as isize
    );
}
/// J1T-15 unit: reads never exceed the sink's room, a sink with less room
/// than one read buffer stops reads, and no byte is lost by backpressure.
#[test]
fn j1t_15_drain_respects_sink_room_and_stops_below_one_read_buffer() {
    let (read, write) = spawn::pipe(false).unwrap();
    let mut c = Capture::new(read, Stream::Stdout);
    let mut sink = TestSink::default();
    sink.room.store(6000, Ordering::Release);
    write_all(&write, &[b'x'; 10_000]);
    assert!(c.wants_read(sink.room(), false));
    c.drain(DRAIN_BUDGET, &mut sink, false).unwrap();
    // One full read of 4096; the remaining 1904 bytes of room are below a
    // read buffer, so reading stops (the interest is omitted too).
    assert_eq!(sink.accepted[0].len(), 4096);
    assert!(!c.wants_read(sink.room(), false));
    c.drain(DRAIN_BUDGET, &mut sink, false).unwrap();
    assert_eq!(sink.accepted[0].len(), 4096, "no read below one buffer");
    sink.room.store(64 * 1024, Ordering::Release);
    c.drain(DRAIN_BUDGET, &mut sink, false).unwrap();
    assert_eq!(
        sink.accepted[0].len(),
        4096 + 5904,
        "budget-limited dispatch"
    );
    drop(write);
    c.drain(DRAIN_BUDGET, &mut sink, false).unwrap();
    assert!(c.eof && sink.ended[0]);
    assert_eq!(sink.accepted[0], vec![b'x'; 10_000]);
    assert_eq!(sink.discarded, [0, 0]);
    assert_eq!(c.total, 10_000);
}
/// J1T-15 unit, discard after cancel (owner, 2026-10-15): with a cancel
/// trigger the pipe is read to EOF; bytes the sink cannot take are counted
/// as discarded, and every later byte of that stream is discarded too.
#[test]
fn j1t_15_after_cancel_full_sink_discards_and_never_rejoins_the_stream() {
    let (read, write) = spawn::pipe(false).unwrap();
    let mut c = Capture::new(read, Stream::Stderr);
    let mut sink = TestSink::default();
    sink.room.store(100, Ordering::Release);
    write_all(&write, &[b'a'; 300]);
    assert!(c.wants_read(sink.room(), true), "draining ignores room");
    assert!(!c.wants_read(sink.room(), false));
    c.drain(DRAIN_BUDGET, &mut sink, true).unwrap();
    assert_eq!(sink.accepted[1], vec![b'a'; 100]);
    assert_eq!(sink.discarded[1], 200);
    sink.room.store(64 * 1024, Ordering::Release);
    write_all(&write, &[b'b'; 50]);
    c.drain(DRAIN_BUDGET, &mut sink, true).unwrap();
    assert_eq!(sink.accepted[1].len(), 100, "no gap joined to a later tail");
    assert_eq!(sink.discarded[1], 250);
    drop(write);
    c.drain(DRAIN_BUDGET, &mut sink, true).unwrap();
    assert!(c.eof && sink.ended[1]);
}
#[test]
fn rendered_tail_replaces_short_terminal_controls_and_invalid_utf8() {
    let (text, truncated) = render_tail(b"ok\x1b[31m\xff\0\n\t");
    assert_eq!(text, "ok\u{fffd}[31m\u{fffd}\u{fffd}\n\t");
    assert!(truncated);
    let mut stream = vec![b'x'; 16384];
    stream.extend_from_slice(b"\x1b[31m\xff\n");
    let (text, truncated) = render_tail(&stream);
    assert!(truncated);
    assert!(text.len() <= 6144);
    assert!(text.ends_with("x\u{fffd}[31m\u{fffd}\n"));
    assert!(!text.contains('\x1b'));
}
struct Fixture(std::path::PathBuf);
impl Fixture {
    fn new() -> Self {
        let p = std::path::PathBuf::from("/private/tmp").join(format!(
            "as-sh-u-{}-{}",
            std::process::id(),
            crate::random_id()
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        ));
        fs::create_dir(&p).unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(0o700)).unwrap();
        Self(fs::canonicalize(p).unwrap())
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
#[test]
fn preparation_pins_cwd_and_cleanup_never_follows_symlink() {
    let f = Fixture::new();
    let runtime = RuntimeDirectory::scratch(&f.0, true).unwrap();
    let project = f.0.join("project");
    fs::create_dir(&project).unwrap();
    let project =
        ProjectIdentity::open(project.to_str().unwrap(), &runtime.state_boundary()).unwrap();
    let cancel = AtomicBool::new(false);
    let mut prepared = match PreparedShell::prepare(runtime, &project, request(), &cancel) {
        Ok(v) => v,
        Err(e) => panic!("{:?}", e.reason),
    };
    let mut cleanup = prepared.cleanup.take().unwrap();
    let scratch = spawn::path(cleanup.directory.as_raw_fd()).unwrap();
    let outside = f.0.join("keep");
    fs::write(&outside, b"keep").unwrap();
    symlink(&outside, Path::new(&scratch).join("link")).unwrap();
    fs::create_dir(Path::new(&scratch).join("nested")).unwrap();
    fs::write(Path::new(&scratch).join("nested/file"), b"x").unwrap();
    cleanup
        .run(Instant::now() + Duration::from_secs(2), &cancel)
        .unwrap();
    assert!(outside.exists());
    assert!(!Path::new(&scratch).exists());
}
#[test]
fn spawn_rejects_replaced_project_and_retains_scratch() {
    let f = Fixture::new();
    let runtime = RuntimeDirectory::scratch(&f.0, true).unwrap();
    let path = f.0.join("project");
    fs::create_dir(&path).unwrap();
    let project = ProjectIdentity::open(path.to_str().unwrap(), &runtime.state_boundary()).unwrap();
    let cancel = AtomicBool::new(false);
    let prepared = match PreparedShell::prepare(runtime, &project, request(), &cancel) {
        Ok(v) => v,
        Err(e) => panic!("{:?}", e.reason),
    };
    fs::rename(&path, f.0.join("old-project")).unwrap();
    fs::create_dir(&path).unwrap();
    let mut failure = match prepared.spawn(
        Path::new("/usr/bin/true"),
        &cancel,
        Box::new(TestSink::default()),
    ) {
        Err(e) => e,
        Ok(_) => panic!("replaced project spawned"),
    };
    assert_eq!(failure.reason, ShellError::Denied);
    failure
        .cleanup
        .as_mut()
        .unwrap()
        .run(Instant::now() + Duration::from_secs(2), &cancel)
        .unwrap();
}
#[test]
fn guardian_normal_invocation_without_private_descriptors_fails_closed() {
    assert_eq!(run_guardian("not-a-job", "0", "printf no"), 1);
}

#[test]
fn spawn_rejects_replaced_cwd_before_any_process() {
    let f = Fixture::new();
    let runtime = RuntimeDirectory::scratch(&f.0, true).unwrap();
    let path = f.0.join("project");
    fs::create_dir(&path).unwrap();
    fs::create_dir(path.join("sub")).unwrap();
    let project = ProjectIdentity::open(path.to_str().unwrap(), &runtime.state_boundary()).unwrap();
    let cancel = AtomicBool::new(false);
    let mut req = request();
    req.cwd = Some("./sub/".into());
    let prepared = PreparedShell::prepare(runtime, &project, req, &cancel)
        .unwrap_or_else(|e| panic!("{:?}", e.reason));
    fs::rename(path.join("sub"), path.join("old-sub")).unwrap();
    fs::create_dir(path.join("sub")).unwrap();
    let mut failure = match prepared.spawn(
        Path::new("/usr/bin/true"),
        &cancel,
        Box::new(TestSink::default()),
    ) {
        Err(e) => e,
        Ok(_) => panic!("replaced cwd spawned"),
    };
    assert_eq!(failure.reason, ShellError::Denied);
    failure
        .cleanup
        .as_mut()
        .unwrap()
        .run(Instant::now() + Duration::from_secs(2), &cancel)
        .unwrap();
}

fn notice_fixture() -> (ShellJob, std::os::fd::OwnedFd) {
    let (read, write) = spawn::pipe(true).unwrap();
    let (out, ow) = spawn::pipe(false).unwrap();
    let (err, ew) = spawn::pipe(false).unwrap();
    drop(ow);
    drop(ew);
    (
        ShellJob {
            pid: 0,
            reaped: true,
            lease: None,
            status: read,
            status_bytes: Vec::new(),
            status_eof: false,
            notices: 0,
            spawned: false,
            terminal: None,
            out: Capture::new(out, Stream::Stdout),
            err: Capture::new(err, Stream::Stderr),
            sink: Box::new(TestSink {
                room: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(512 * 1024)),
                ..TestSink::default()
            }),
            deadline: None,
            stop: None,
            cleanup: None,
            job_id: [7; 16],
            alternate: false,
            fault: false,
        },
        write,
    )
}
#[test]
fn duplicate_unknown_notice_cannot_prove_settlement() {
    let (mut job, write) = notice_fixture();
    for kind in [1, 4, 4] {
        let n = Notice {
            kind,
            reason: if kind == 4 { 9 } else { 0 },
            exit_kind: 0,
            cleanup: false,
            value: 0,
        }
        .encode([7; 16]);
        // SAFETY: fixed bytes into owned empty fixture pipe.
        assert_eq!(
            unsafe { libc::write(write.as_raw_fd(), n.as_ptr().cast(), n.len()) },
            32
        );
    }
    assert_eq!(job.poll(Instant::now()), Err(ShellError::Protocol));
    assert!(!job.settled());
    assert!(job.result().is_none());
}
#[test]
fn settled_setup_failure_is_never_a_successful_shell_exit() {
    let (mut job, write) = notice_fixture();
    let n = Notice {
        kind: 3,
        reason: 7,
        exit_kind: 0,
        cleanup: true,
        value: 0,
    }
    .encode([7; 16]);
    assert_eq!(
        unsafe { libc::write(write.as_raw_fd(), n.as_ptr().cast(), n.len()) },
        32
    );
    drop(write);
    job.poll(Instant::now()).unwrap();
    job.poll(Instant::now()).unwrap();
    let result = job.result().unwrap();
    assert_eq!(result.failure, Some(ShellError::Denied));
    assert_eq!(result.exit_code, None);
    assert_eq!(result.signal, None);
}

/// J1T-15 unit: interests omit a stream's read while the sink has less
/// than one read buffer of room, keep the status pipe, and return to both
/// pipes after a cancel trigger.
#[test]
fn j1t_15_interests_follow_sink_room_and_cancel() {
    let (job, _write) = notice_fixture();
    let mut job = job;
    job.out.eof = false;
    job.err.eof = false;
    let room = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(READ_BUFFER));
    job.sink = Box::new(TestSink {
        room: room.clone(),
        ..TestSink::default()
    });
    assert_eq!(job.interests().len(), 3);
    room.store(READ_BUFFER - 1, Ordering::Release);
    let interests = job.interests();
    assert_eq!(interests.len(), 1);
    assert_eq!(interests[0].fd, job.status.as_raw_fd());
    job.stop(StopReason::Cancelled);
    assert_eq!(job.interests().len(), 3, "drain to EOF after cancel");
}
/// J1T-07 unit: nothing produces `OutputLimit`; more than 64 MiB of output
/// does not stop the job (J1-Q2).
#[test]
fn j1t_07_no_output_limit_producer() {
    let (mut job, _write) = notice_fixture();
    job.out.total = 80 * 1024 * 1024;
    job.err.total = 80 * 1024 * 1024;
    let _ = job.poll(Instant::now());
    assert_ne!(job.stop, Some(StopReason::OutputLimit));
    assert!(job.stop.is_none() || job.fault);
}
/// J1T-05/J1T-06 unit: with no deadline, setup and poll check only
/// cancellation; a deadline remains only for qualification.
#[test]
fn j1t_05_no_deadline_checks_cancel_only() {
    let cancel = AtomicBool::new(false);
    assert_eq!(check(None, &cancel), Ok(()));
    let past = Instant::now() - Duration::from_secs(1);
    assert_eq!(check(Some(past), &cancel), Err(ShellError::Deadline));
    cancel.store(true, Ordering::Release);
    assert_eq!(check(None, &cancel), Err(ShellError::Cancelled));
    let (mut job, _write) = notice_fixture();
    job.lease = None;
    let _ = job.poll(Instant::now() + Duration::from_secs(3600));
    assert_ne!(job.stop, Some(StopReason::Deadline));
}
