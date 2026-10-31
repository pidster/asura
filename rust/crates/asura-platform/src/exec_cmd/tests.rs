use super::*;
use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
fn request() -> ExecCmdRequest {
    ExecCmdRequest {
        job_id: [7; 16],
        profile: Profile::Model1,
        command: "printf ok".into(),
        cwd: None,
        secrets: Vec::new(),
        secret_command: None,
    }
}
#[test]
fn arguments_reject_effects_outside_contract() {
    let mut r = request();
    assert!(validate(&r).is_ok());
    for cwd in ["../other", "a/../b", "/tmp", "", "x\0y"] {
        r.cwd = Some(cwd.into());
        assert_eq!(validate(&r), Err(ExecCmdError::Invalid));
    }
    for cwd in [".", "./subdir", "a//b/"] {
        r.cwd = Some(cwd.into());
        assert!(validate(&r).is_ok());
    }
    r.cwd = None;
    r.command = "x".repeat(8192);
    assert!(validate(&r).is_ok());
    r.command.push('x');
    assert_eq!(validate(&r), Err(ExecCmdError::Invalid));
    r.command = "x\0y".into();
    assert_eq!(validate(&r), Err(ExecCmdError::Invalid));
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
    assert_eq!(validate(&r), Err(ExecCmdError::Invalid), "8193 bytes");
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
        assert_eq!(validate(&r), Err(ExecCmdError::Invalid));
    }
    r.secrets = vec![
        SecretVariable::for_test(3, b"a b\n$c"),
        SecretVariable::for_test(1, b""),
    ];
    assert!(validate(&r).is_ok(), "numbers need not be contiguous");
    let environment = secret_environment(&r.secrets, None);
    let entries: Vec<&[u8]> = environment.iter().map(|e| e.as_slice()).collect();
    assert_eq!(
        entries,
        [&b"ASURA_SECRET_3=a b\n$c"[..], &b"ASURA_SECRET_1="[..]]
    );
}
/// SDU-17 (request half): `ASURA_SECRET_COMMAND` alone with 1–8192 bytes and
/// the fixed `eval` command is accepted; beside any numbered variable,
/// empty, with a NUL, over 8192 bytes or with another command it refuses.
#[test]
fn sdu_17_whole_command_variable_request_validation() {
    use crate::secrets::rewrite::WHOLE_COMMAND;
    let mut r = request();
    r.command = WHOLE_COMMAND.into();
    r.secret_command = Some(SecretCommand::new(b"echo 'x'"));
    assert!(validate(&r).is_ok());
    let environment = secret_environment(&r.secrets, r.secret_command.as_ref());
    let entries: Vec<&[u8]> = environment.iter().map(|e| e.as_slice()).collect();
    assert_eq!(entries, [&b"ASURA_SECRET_COMMAND=echo 'x'"[..]]);
    r.secret_command = Some(SecretCommand::new(&[b'v'; 8192]));
    assert!(validate(&r).is_ok(), "8192 bytes");
    for (label, value) in [
        ("8193 bytes", vec![b'v'; 8193]),
        ("empty", Vec::new()),
        ("NUL", b"a\0b".to_vec()),
    ] {
        r.secret_command = Some(SecretCommand::new(&value));
        assert_eq!(validate(&r), Err(ExecCmdError::Invalid), "{label}");
    }
    r.secret_command = Some(SecretCommand::new(b"echo 'x'"));
    for n in [1, 16] {
        r.secrets = vec![SecretVariable::for_test(n, b"value")];
        assert_eq!(validate(&r), Err(ExecCmdError::Invalid), "numbered {n}");
    }
    r.secrets.clear();
    r.command = "printf ok".into();
    assert_eq!(validate(&r), Err(ExecCmdError::Invalid), "other command");
}
#[test]
fn notices_reject_identity_reserved_and_invalid_outcomes() {
    let id = [9; 16];
    let n = Notice {
        kind: 2,
        reason: 2,
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
    let mut prepared = match PreparedExecCmd::prepare(runtime, &project, request(), &cancel) {
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
    let prepared = match PreparedExecCmd::prepare(runtime, &project, request(), &cancel) {
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
    assert_eq!(failure.reason, ExecCmdError::Denied);
    failure
        .cleanup
        .as_mut()
        .unwrap()
        .run(Instant::now() + Duration::from_secs(2), &cancel)
        .unwrap();
}
#[test]
fn guardian_normal_invocation_without_private_descriptors_fails_closed() {
    assert_eq!(run_guardian("not-a-job", "model-1", "printf no"), 1);
    let job = "0123456789abcdef0123456789abcdef";
    assert_eq!(run_guardian(job, "model-2", "printf no"), 1);
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
    let prepared = PreparedExecCmd::prepare(runtime, &project, req, &cancel)
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
    assert_eq!(failure.reason, ExecCmdError::Denied);
    failure
        .cleanup
        .as_mut()
        .unwrap()
        .run(Instant::now() + Duration::from_secs(2), &cancel)
        .unwrap();
}

fn notice_fixture() -> (ExecCmdJob, std::os::fd::OwnedFd) {
    let (read, write) = spawn::pipe(true).unwrap();
    let (out, ow) = spawn::pipe(false).unwrap();
    let (err, ew) = spawn::pipe(false).unwrap();
    drop(ow);
    drop(ew);
    (
        ExecCmdJob {
            pid: 0,
            reaped: true,
            exited: true,
            lease: None,
            status: read,
            status_bytes: Vec::new(),
            status_eof: false,
            status_received: false,
            notices: 0,
            spawned: false,
            last_kind: 0,
            leader: None,
            unconfirmed: None,
            escapees: 0,
            terminal: None,
            out: Some(Capture::new(out, Stream::Stdout)),
            err: Some(Capture::new(err, Stream::Stderr)),
            sink: Box::new(TestSink {
                room: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(512 * 1024)),
                ..TestSink::default()
            }),
            stop: None,
            cleanup: None,
            job_id: [7; 16],
            profile: Profile::Model1,
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
    assert_eq!(job.poll(Instant::now()), Err(ExecCmdError::Protocol));
    assert!(!job.settled());
    assert!(job.result().is_none());
}
#[test]
fn settled_setup_failure_is_never_a_successful_exec_cmd_exit() {
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
    pump(&mut job).unwrap();
    let result = job.result().unwrap();
    assert_eq!(result.failure, Some(ExecCmdError::Denied));
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
    job.out.as_mut().unwrap().eof = false;
    job.err.as_mut().unwrap().eof = false;
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
    job.stop(StopKind::Term);
    assert_eq!(job.interests().len(), 3, "drain to EOF after cancel");
}
/// J1T-07 unit: more than 64 MiB of output does not stop the job (J1-Q2);
/// reason 3 `output_limit` is unassigned (J2).
#[test]
fn j1t_07_no_output_limit_producer() {
    let (mut job, _write) = notice_fixture();
    job.out.as_mut().unwrap().total = 80 * 1024 * 1024;
    job.err.as_mut().unwrap().total = 80 * 1024 * 1024;
    let _ = job.poll(Instant::now());
    assert_eq!(job.stop, None, "output never requests a stop");
    let id = [9; 16];
    assert!(Notice::decode(&notice(2, 3, 1, true, 0).encode(id), id).is_err());
}
/// J1T-05/J1T-06 unit: setup and poll check only cancellation; there is
/// no request deadline and no guardian reason 1.
#[test]
fn j1t_05_no_deadline_checks_cancel_only() {
    let cancel = AtomicBool::new(false);
    assert_eq!(check(&cancel), Ok(()));
    cancel.store(true, Ordering::Release);
    assert_eq!(check(&cancel), Err(ExecCmdError::Cancelled));
    let (mut job, _write) = notice_fixture();
    job.lease = None;
    let _ = job.poll(Instant::now() + Duration::from_secs(3600));
    assert_eq!(job.stop, None, "no time-based stop");
    let id = [9; 16];
    assert!(Notice::decode(&notice(2, 1, 1, true, 0).encode(id), id).is_err());
}

fn notice(kind: u8, reason: u8, exit_kind: u8, cleanup: bool, value: u32) -> Notice {
    Notice {
        kind,
        reason,
        exit_kind,
        cleanup,
        value,
    }
}
/// Poll a fixture job until it faults or all three pipes reach EOF. macOS
/// has no `pipe2`, so a process another test forks at the same moment can
/// briefly hold a fixture writer; a fixed number of polls was flaky. Test
/// limit only: 10 s.
fn pump(job: &mut ExecCmdJob) -> Result<(), ExecCmdError> {
    let limit = Instant::now() + Duration::from_secs(10);
    loop {
        job.poll(Instant::now())?;
        if job.status_eof && job.out.as_mut().unwrap().eof && job.err.as_mut().unwrap().eof {
            return Ok(());
        }
        assert!(Instant::now() < limit, "fixture pipes did not reach EOF");
        std::thread::sleep(Duration::from_millis(1));
    }
}
/// Write the notices, end the status pipe and poll until the job settles
/// or faults. Returns the job and the first poll error.
fn feed(notices: &[Notice]) -> (ExecCmdJob, Result<(), ExecCmdError>) {
    let (mut job, write) = notice_fixture();
    for n in notices {
        let bytes = n.encode([7; 16]);
        // SAFETY: fixed bytes into the owned fixture pipe, below PIPE_BUF.
        assert_eq!(
            unsafe { libc::write(write.as_raw_fd(), bytes.as_ptr().cast(), bytes.len()) },
            32
        );
    }
    drop(write);
    let result = pump(&mut job);
    (job, result)
}
/// EC-U2: kind 6 and reasons 12 and 13 round-trip; their invalid shapes,
/// and the person reasons 10 and 11 not yet defined, are protocol faults.
#[test]
fn ec_u2_escapee_notice_shapes_round_trip_and_reject_invalid() {
    let id = [9; 16];
    for valid in [
        notice(6, 0, 0, false, 1),
        notice(6, 0, 0, false, 8000),
        notice(2, 12, 1, true, 0),
        notice(2, 12, 2, true, 9),
        notice(4, 13, 0, false, 0),
        notice(4, 9, 0, false, 0),
        notice(2, 11, 2, true, 9),
        notice(2, 2, 1, true, 0),
        notice(5, 0, 1, false, 255),
        notice(5, 0, 2, false, 127),
    ] {
        let decoded = Notice::decode(&valid.encode(id), id).expect("valid shape");
        assert_eq!(
            (
                decoded.kind,
                decoded.reason,
                decoded.exit_kind,
                decoded.cleanup,
                decoded.value
            ),
            (
                valid.kind,
                valid.reason,
                valid.exit_kind,
                valid.cleanup,
                valid.value
            )
        );
    }
    for (label, invalid) in [
        ("kind 6 zero count", notice(6, 0, 0, false, 0)),
        ("kind 6 reason", notice(6, 12, 0, false, 1)),
        ("kind 6 exit", notice(6, 0, 1, false, 1)),
        ("kind 6 cleanup", notice(6, 0, 0, true, 1)),
        ("kind 2 reason 13", notice(2, 13, 1, true, 0)),
        ("kind 4 reason 12", notice(4, 12, 0, false, 0)),
        ("kind 4 exit", notice(4, 13, 1, false, 0)),
        ("kind 3 reason 12", notice(3, 12, 0, true, 0)),
        ("reason 10", notice(2, 10, 1, true, 0)),
        ("reason 11 on kind 4", notice(4, 11, 0, false, 0)),
        ("reason 14", notice(4, 14, 0, false, 0)),
        ("kind 7", notice(7, 0, 0, false, 0)),
    ] {
        assert!(Notice::decode(&invalid.encode(id), id).is_err(), "{label}");
    }
}
/// EC-U2 order: valid sequences settle with their facts; a late settlement
/// after kind 4 never confirms cleanup.
#[test]
fn ec_u2_escapee_notice_order_and_settlement_facts() {
    let spawned = notice(1, 0, 0, false, 0);
    let exited = notice(5, 0, 1, false, 0);
    let report = notice(6, 0, 0, false, 3);
    let settled = notice(2, 12, 1, true, 0);
    let (job, polled) = feed(&[spawned, exited, report, settled]);
    assert_eq!(polled, Ok(()));
    let result = job.result().expect("settled");
    assert!(result.cleanup_confirmed);
    assert_eq!(result.escapees, 3);
    assert_eq!(result.exit_code, Some(0));
    assert_eq!(result.reason, None, "reason 12 is not a stop reason");
    for (label, reason, expected) in [
        ("escapee", 13, UnconfirmedReason::EscapeeUnterminated),
        ("cleanup", 9, UnconfirmedReason::CleanupFailed),
    ] {
        let (job, polled) = feed(&[
            spawned,
            exited,
            notice(4, reason, 0, false, 0),
            report,
            settled,
        ]);
        assert_eq!(polled, Ok(()), "{label}");
        assert_eq!(job.cleanup_unconfirmed(), Some(expected));
        let result = job.result().expect("late settlement");
        assert!(
            !result.cleanup_confirmed,
            "{label}: late is never confirmed"
        );
        assert_eq!(result.escapees, 3);
    }
    // No escapees: no kind 6, the lease reason stays, no count in the text.
    let (job, polled) = feed(&[spawned, notice(5, 0, 2, false, 9), notice(2, 4, 2, true, 9)]);
    assert_eq!(polled, Ok(()));
    let result = job.result().unwrap();
    assert!(result.cleanup_confirmed);
    assert_eq!(result.escapees, 0);
    // Kind 4 alone never settles.
    let (job, _) = feed(&[spawned, notice(4, 13, 0, false, 0)]);
    assert!(!job.settled());
    assert_eq!(
        job.cleanup_unconfirmed(),
        Some(UnconfirmedReason::EscapeeUnterminated)
    );
}
/// EC-U2 order faults: duplicate kind 6 or kind 4, kind 6 not followed by
/// kind 2 reason 12, reason 12 without kind 6, notices before spawned,
/// after the terminal notice, or beyond five; trailing bytes.
#[test]
fn ec_u2_escapee_notice_order_faults() {
    let spawned = notice(1, 0, 0, false, 0);
    let exited = notice(5, 0, 1, false, 0);
    let report = notice(6, 0, 0, false, 1);
    let unknown = notice(4, 13, 0, false, 0);
    let settled = notice(2, 12, 1, true, 0);
    let plain = notice(2, 0, 1, true, 0);
    for (label, sequence) in [
        (
            "duplicate kind 6",
            vec![spawned, exited, report, report, settled],
        ),
        (
            "kind 4 after kind 6",
            vec![spawned, exited, report, unknown, settled],
        ),
        (
            "duplicate kind 4",
            vec![spawned, unknown, unknown, exited, plain],
        ),
        (
            "plain settle after kind 6",
            vec![spawned, exited, report, plain],
        ),
        ("reason 12 without kind 6", vec![spawned, exited, settled]),
        ("kind 6 before spawned", vec![report, settled]),
        ("kind 6 after settled", vec![spawned, exited, plain, report]),
        (
            "setup failure after spawned",
            vec![spawned, notice(3, 5, 0, true, 0)],
        ),
    ] {
        let (job, polled) = feed(&sequence);
        assert_eq!(polled, Err(ExecCmdError::Protocol), "{label}");
        assert!(!job.settled(), "{label}");
        assert!(job.result().is_none(), "{label}");
    }
    // Trailing partial bytes after a valid settlement.
    let (mut job, write) = notice_fixture();
    let mut bytes = Vec::new();
    for n in [spawned, exited, report, settled] {
        bytes.extend_from_slice(&n.encode([7; 16]));
    }
    bytes.extend_from_slice(b"ASSH");
    // SAFETY: owned fixture pipe, 100 bytes below PIPE_BUF.
    assert_eq!(
        unsafe { libc::write(write.as_raw_fd(), bytes.as_ptr().cast(), bytes.len()) },
        bytes.len() as isize
    );
    drop(write);
    assert_eq!(pump(&mut job), Err(ExecCmdError::Protocol));
    assert!(!job.settled());
}

/// J2T-05 unit: the profile token is exact; P1 adds `person-1` and
/// `person-outside-1` with the 2 s grace (PC8); `model-1` keeps 500 ms.
#[test]
fn j2t_05_profile_token_parse() {
    for (token, profile, grace, sandboxed) in [
        ("model-1", Profile::Model1, 500_000_000, true),
        ("person-1", Profile::Person1, 2_000_000_000, true),
        (
            "person-outside-1",
            Profile::PersonOutside1,
            2_000_000_000,
            false,
        ),
    ] {
        assert_eq!(Profile::parse(token), Some(profile));
        assert_eq!(profile.token(), token);
        assert_eq!(profile.grace_ns(), grace);
        assert_eq!(profile.sandboxed(), sandboxed);
        assert_eq!(profile.person(), profile != Profile::Model1);
    }
    for token in [
        "",
        "model-2",
        "Model-1",
        "model-1 ",
        "model",
        "person",
        "person-2",
        "person-outside",
        "Person-1",
        "person-outside-1\0",
    ] {
        assert_eq!(Profile::parse(token), None, "{token:?}");
    }
}
/// A notice fixture whose lease is a real pipe; returns the job, the
/// status writer and the lease reader.
fn lease_fixture() -> (ExecCmdJob, std::os::fd::OwnedFd, std::os::fd::OwnedFd) {
    let (mut job, status) = notice_fixture();
    let (read, write) = spawn::pipe(true).unwrap();
    spawn::no_sigpipe(write.as_raw_fd()).unwrap();
    job.lease = Some(write);
    (job, status, read)
}
fn lease_bytes(read: &std::os::fd::OwnedFd) -> Vec<u8> {
    let mut buf = [0u8; 16];
    // SAFETY: owned nonblocking fixture pipe and a live output buffer.
    let n = unsafe { libc::read(read.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
    buf[..n.max(0) as usize].to_vec()
}
/// J2T-05 unit, service side: at most one `T` and one `K` reach the lease;
/// a later `T` is ignored; `K` escalates `T`; a failed write closes the
/// lease instead of leaving the job running without a stop.
#[test]
fn j2t_05_stop_writes_at_most_one_t_and_one_k() {
    let (mut job, _status, read) = lease_fixture();
    job.stop(StopKind::Term);
    job.stop(StopKind::Term);
    assert_eq!(lease_bytes(&read), b"T");
    job.stop(StopKind::Kill);
    job.stop(StopKind::Kill);
    job.stop(StopKind::Term);
    assert_eq!(lease_bytes(&read), b"K");
    assert!(job.lease.is_some(), "successful writes keep the lease");
    assert_eq!(job.stop, Some(StopKind::Kill));

    let (mut job, _status, read) = lease_fixture();
    job.stop(StopKind::Kill);
    job.stop(StopKind::Term);
    assert_eq!(lease_bytes(&read), b"K", "no T after K");

    // The reader is gone: EPIPE (no SIGPIPE) closes the lease.
    let (mut job, _status, read) = lease_fixture();
    drop(read);
    job.stop(StopKind::Term);
    assert!(job.lease.is_none(), "a failed write closes the lease");
    assert_eq!(job.stop, Some(StopKind::Term));

    // A full pipe: WouldBlock closes the lease.
    let (mut job, _status, _read) = lease_fixture();
    let fd = job.lease.as_ref().unwrap().as_raw_fd();
    let chunk = [0u8; 4096];
    // SAFETY: nonblocking writes from a live buffer into the owned fixture
    // pipe until it reports full.
    while unsafe { libc::write(fd, chunk.as_ptr().cast(), chunk.len()) } > 0 {}
    job.stop(StopKind::Term);
    assert!(job.lease.is_none(), "WouldBlock closes the lease");

    // A protocol fault closes the lease; no stop request is invented.
    let (mut job, status, _read) = lease_fixture();
    let bytes = notice(5, 0, 1, false, 0).encode([7; 16]);
    // SAFETY: fixed bytes into the owned fixture pipe, below PIPE_BUF.
    assert_eq!(
        unsafe { libc::write(status.as_raw_fd(), bytes.as_ptr().cast(), 32) },
        32
    );
    assert_eq!(job.poll(Instant::now()), Err(ExecCmdError::Protocol));
    assert!(job.faulted() && job.lease.is_none() && job.stop.is_none());
}
/// J2T-06 unit: kind 5 shape: reason 0, exit kind 1 (code 0–255) or 2
/// (signal 1–127), no cleanup bit.
#[test]
fn j2t_06_leader_exit_notice_shape() {
    let id = [9; 16];
    for valid in [
        notice(5, 0, 1, false, 0),
        notice(5, 0, 1, false, 255),
        notice(5, 0, 2, false, 1),
        notice(5, 0, 2, false, 127),
    ] {
        let bytes = valid.encode(id);
        assert_eq!((bytes[4], bytes[5], bytes[7]), (5, 0, 0));
        assert_eq!(bytes[24..28], valid.value.to_be_bytes());
        assert!(Notice::decode(&bytes, id).is_ok(), "{valid:?}");
    }
    for (label, invalid) in [
        ("no exit", notice(5, 0, 0, false, 0)),
        ("reason", notice(5, 2, 1, false, 0)),
        ("cleanup bit", notice(5, 0, 1, true, 0)),
        ("code 256", notice(5, 0, 1, false, 256)),
        ("signal 0", notice(5, 0, 2, false, 0)),
        ("signal 128", notice(5, 0, 2, false, 128)),
        ("exit kind 3", notice(5, 0, 3, false, 1)),
    ] {
        assert!(Notice::decode(&invalid.encode(id), id).is_err(), "{label}");
    }
    // The longest sequences, 1, 5, 4, 6, 2 and 1, 4, 5, 6, 2, fit.
    assert_eq!(usize::from(MAX_NOTICES) * 32, 160);
    const { assert!(160 <= STATUS_BUFFER && STATUS_BUFFER < 512) };
}
/// J2T-06 unit: every valid order of kinds 1, 5, 4, 6 and 2 settles with
/// the kind 5 exit facts; kind 4 makes the settlement late.
#[test]
fn j2t_06_every_valid_notice_order_settles() {
    let spawned = notice(1, 0, 0, false, 0);
    let exited = notice(5, 0, 2, false, 9);
    let unknown = notice(4, 13, 0, false, 0);
    let report = notice(6, 0, 0, false, 2);
    let settle = |reason| notice(2, reason, 2, true, 9);
    for (label, sequence, confirmed, escapees, reason) in [
        ("1 5 2/0", vec![spawned, exited, settle(0)], true, 0, None),
        (
            "1 5 2/2",
            vec![spawned, exited, settle(2)],
            true,
            0,
            Some(StopReason::Cancelled),
        ),
        (
            "1 5 2/4",
            vec![spawned, exited, settle(4)],
            true,
            0,
            Some(StopReason::LeaseEnded),
        ),
        (
            "1 5 2/11",
            vec![spawned, exited, settle(11)],
            true,
            0,
            Some(StopReason::Killed),
        ),
        (
            "1 5 4 2",
            vec![spawned, exited, unknown, settle(11)],
            false,
            0,
            Some(StopReason::Killed),
        ),
        (
            "1 4 5 2",
            vec![spawned, unknown, exited, settle(2)],
            false,
            0,
            Some(StopReason::Cancelled),
        ),
        (
            "1 5 6 2",
            vec![spawned, exited, report, settle(12)],
            true,
            2,
            None,
        ),
        (
            "1 5 4 6 2",
            vec![spawned, exited, unknown, report, settle(12)],
            false,
            2,
            None,
        ),
        (
            "1 4 5 6 2",
            vec![spawned, unknown, exited, report, settle(12)],
            false,
            2,
            None,
        ),
    ] {
        let (job, polled) = feed(&sequence);
        assert_eq!(polled, Ok(()), "{label}");
        assert!(job.spawned(), "{label}");
        assert_eq!(job.leader_exit(), Some(Exit::Signalled(9)), "{label}");
        let result = job.result().unwrap_or_else(|| panic!("{label}: settled"));
        assert_eq!(result.cleanup_confirmed, confirmed, "{label}");
        assert_eq!(result.escapees, escapees, "{label}");
        assert_eq!(result.reason, reason, "{label}");
        assert_eq!(
            (result.signal, result.exit_code),
            (Some(9), None),
            "{label}"
        );
    }
    // The service's own request names the stop when reason 12 replaced it.
    let (mut job, status, _read) = lease_fixture();
    job.stop(StopKind::Kill);
    for n in [spawned, exited, report, settle(12)] {
        let bytes = n.encode([7; 16]);
        // SAFETY: fixed bytes into the owned fixture pipe, below PIPE_BUF.
        assert_eq!(
            unsafe { libc::write(status.as_raw_fd(), bytes.as_ptr().cast(), 32) },
            32
        );
    }
    drop(status);
    pump(&mut job).unwrap();
    assert_eq!(job.result().unwrap().reason, Some(StopReason::Killed));
    // Kind 5 alone: the leader exited, the job is not settled.
    let (mut job, status) = notice_fixture();
    for n in [spawned, notice(5, 0, 1, false, 3)] {
        let bytes = n.encode([7; 16]);
        // SAFETY: as above.
        assert_eq!(
            unsafe { libc::write(status.as_raw_fd(), bytes.as_ptr().cast(), 32) },
            32
        );
    }
    job.poll(Instant::now()).unwrap();
    assert_eq!(job.leader_exit(), Some(Exit::Exited(3)));
    assert!(!job.settled());
}
/// J2T-06 unit: kind 2 without kind 5, unequal exit facts, a second kind
/// 5, kind 5 before kind 1 and kind 6 before kind 5 are protocol faults.
#[test]
fn j2t_06_notice_order_faults() {
    let spawned = notice(1, 0, 0, false, 0);
    let exited = notice(5, 0, 1, false, 4);
    let report = notice(6, 0, 0, false, 1);
    let unknown = notice(4, 9, 0, false, 0);
    for (label, sequence) in [
        (
            "kind 2 without kind 5",
            vec![spawned, notice(2, 0, 1, true, 4)],
        ),
        (
            "kind 2 without kind 5 after kind 4",
            vec![spawned, unknown, notice(2, 2, 1, true, 4)],
        ),
        (
            "unequal exit value",
            vec![spawned, exited, notice(2, 0, 1, true, 5)],
        ),
        (
            "unequal exit kind",
            vec![spawned, exited, notice(2, 0, 2, true, 4)],
        ),
        (
            "second kind 5",
            vec![spawned, exited, exited, notice(2, 0, 1, true, 4)],
        ),
        ("kind 5 before kind 1", vec![exited, spawned]),
        ("kind 5 alone first", vec![exited]),
        (
            "kind 6 before kind 5",
            vec![spawned, report, exited, notice(2, 12, 1, true, 4)],
        ),
        ("kind 5 after kind 6", vec![spawned, exited, report, exited]),
        (
            "kind 5 after kind 2",
            vec![spawned, exited, notice(2, 0, 1, true, 4), exited],
        ),
        (
            "kind 5 after kind 3",
            vec![notice(3, 5, 0, true, 0), exited],
        ),
    ] {
        let (job, polled) = feed(&sequence);
        assert_eq!(polled, Err(ExecCmdError::Protocol), "{label}");
        assert!(job.faulted() && !job.settled(), "{label}");
        assert!(job.result().is_none(), "{label}");
    }
}

/// J2T-25 platform unit (J2-A2): the result carries the guardian's kind 3
/// reason beside the folded failure, and 0 for any other terminal notice.
#[test]
fn j2t_25_result_carries_the_kind_3_reason() {
    for (reason, failure) in [
        (5, ExecCmdError::Unavailable),
        (6, ExecCmdError::Unavailable),
        (7, ExecCmdError::Denied),
        (8, ExecCmdError::Protocol),
        (9, ExecCmdError::CleanupUnconfirmed),
    ] {
        let (job, polled) = feed(&[notice(3, reason, 0, true, 0)]);
        assert_eq!(polled, Ok(()), "reason {reason}");
        let result = job.result().unwrap();
        assert_eq!(result.setup_reason, reason);
        assert_eq!(result.failure, Some(failure), "reason {reason}");
    }
    let (job, polled) = feed(&[
        notice(1, 0, 0, false, 0),
        notice(5, 0, 1, false, 0),
        notice(2, 0, 1, true, 0),
    ]);
    assert_eq!(polled, Ok(()));
    let result = job.result().unwrap();
    assert_eq!((result.setup_reason, result.failure), (0, None));
}

/// A real scratch cleanup token in a private fixture home.
fn scratch_token(f: &Fixture) -> ExecCmdCleanup {
    let runtime = RuntimeDirectory::scratch(&f.0, true).unwrap();
    let project = f.0.join("project");
    fs::create_dir(&project).unwrap();
    let project =
        ProjectIdentity::open(project.to_str().unwrap(), &runtime.state_boundary()).unwrap();
    match PreparedExecCmd::prepare(runtime, &project, request(), &AtomicBool::new(false)) {
        Ok(mut prepared) => prepared.cleanup.take().unwrap(),
        Err(e) => panic!("{:?}", e.reason),
    }
}

/// J2T-25 platform unit (J2-A2): an unsettled job releases its scratch
/// token only when the guardian was reaped after a normal exit, its status
/// pipe reached EOF with no byte, and both capture pipes reached EOF. Each
/// condition false keeps the token; a received status byte or a signal
/// death also ends `release_pending`.
#[test]
fn j2t_25_pre_notice_token_release_needs_every_condition() {
    // Reaped, exited normally, status EOF, no status byte, stdout EOF,
    // stderr EOF.
    let all = [true; 6];
    let mut cases = vec![("all hold", all)];
    for (i, label) in [
        "guardian not reaped",
        "guardian ended by a signal",
        "status pipe open",
        "a status byte arrived",
        "stdout open",
        "stderr open",
    ]
    .into_iter()
    .enumerate()
    {
        let mut conditions = all;
        conditions[i] = false;
        cases.push((label, conditions));
    }
    for (label, [reaped, exited, status_eof, no_byte, out_eof, err_eof]) in cases {
        let f = Fixture::new();
        let (mut job, _status) = notice_fixture();
        job.cleanup = Some(scratch_token(&f));
        job.reaped = reaped;
        job.exited = exited;
        job.status_eof = status_eof;
        job.status_received = !no_byte;
        job.out.as_mut().unwrap().eof = out_eof;
        job.err.as_mut().unwrap().eof = err_eof;
        job.fault = true;
        assert!(!job.settled(), "{label}");
        let released = label == "all hold";
        assert_eq!(
            job.release_pending(),
            !released && no_byte && !(reaped && !exited),
            "{label}"
        );
        let token = job.take_cleanup();
        assert_eq!(token.is_some(), released, "{label}");
        assert_eq!(job.cleanup.is_some(), !released, "{label}: the token stays");
        if let Some(mut token) = token {
            let scratch = spawn::path(token.directory.as_raw_fd()).unwrap();
            token
                .run(
                    Instant::now() + Duration::from_secs(2),
                    &AtomicBool::new(false),
                )
                .unwrap();
            assert!(!Path::new(&scratch).exists());
            assert!(!job.release_pending());
        }
    }
}

/// Poll a faulted fixture job until its three pipes reach EOF; the poll
/// errors are the expected protocol fault. Test limit only: 10 s.
fn pump_faulted(job: &mut ExecCmdJob) {
    let limit = Instant::now() + Duration::from_secs(10);
    while !(job.status_eof && job.out.as_mut().unwrap().eof && job.err.as_mut().unwrap().eof) {
        let _ = job.poll(Instant::now());
        assert!(Instant::now() < limit, "fixture pipes did not reach EOF");
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// J2T-25 platform unit through the pipes: status EOF with no byte is a
/// protocol fault that releases the token once both capture pipes end; an
/// open capture writer keeps it until that pipe ends; a partial notice or
/// a kind 1 before EOF keeps it.
#[test]
fn j2t_25_pre_notice_token_release_through_the_pipes() {
    // No byte, then EOF everywhere: released.
    let f = Fixture::new();
    let (mut job, status) = notice_fixture();
    job.cleanup = Some(scratch_token(&f));
    drop(status);
    pump_faulted(&mut job);
    assert!(job.faulted() && !job.spawned() && !job.settled());
    assert!(job.take_cleanup().is_some(), "no notice, every pipe ended");

    // A held capture writer keeps the token until its pipe ends.
    let f = Fixture::new();
    let (mut job, status) = notice_fixture();
    let (out, out_write) = spawn::pipe(false).unwrap();
    job.out = Some(Capture::new(out, Stream::Stdout));
    job.cleanup = Some(scratch_token(&f));
    drop(status);
    let limit = Instant::now() + Duration::from_secs(10);
    while !(job.status_eof && job.err.as_mut().unwrap().eof) {
        let _ = job.poll(Instant::now());
        assert!(Instant::now() < limit, "fixture pipes did not reach EOF");
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(job.faulted() && !job.out.as_mut().unwrap().eof);
    assert!(job.release_pending());
    assert!(job.take_cleanup().is_none(), "stdout is still open");
    write_all(&out_write, b"late");
    drop(out_write);
    pump_faulted(&mut job);
    assert!(job.take_cleanup().is_some(), "stdout ended");

    // A partial notice or a kind 1: a status byte arrived; the token stays.
    for (label, bytes) in [
        ("partial notice", vec![b'A']),
        ("kind 1", notice(1, 0, 0, false, 0).encode([7; 16]).to_vec()),
    ] {
        let f = Fixture::new();
        let (mut job, status) = notice_fixture();
        job.cleanup = Some(scratch_token(&f));
        write_all(&status, &bytes);
        drop(status);
        pump_faulted(&mut job);
        assert!(job.faulted() && !job.settled(), "{label}");
        assert!(!job.release_pending(), "{label}");
        assert!(job.take_cleanup().is_none(), "{label}");
        assert!(job.cleanup.is_some(), "{label}: the token stays");
    }
}

/// J2T-25 platform unit with a real process: a "guardian" that ends by
/// SIGKILL before any notice, with both capture pipes at EOF, keeps the
/// scratch token, because a signal may end the real guardian between the
/// launch and kind 1 while the command is alive.
#[test]
fn j2t_25_signal_death_before_any_notice_keeps_the_token() {
    let f = Fixture::new();
    let runtime = RuntimeDirectory::scratch(&f.0, true).unwrap();
    let project = f.0.join("project");
    fs::create_dir(&project).unwrap();
    let project =
        ProjectIdentity::open(project.to_str().unwrap(), &runtime.state_boundary()).unwrap();
    let script = f.0.join("killed-guardian");
    fs::write(&script, "#!/bin/sh\nkill -KILL $$\n").unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).unwrap();
    let cancel = AtomicBool::new(false);
    let prepared = match PreparedExecCmd::prepare(runtime, &project, request(), &cancel) {
        Ok(v) => v,
        Err(e) => panic!("{:?}", e.reason),
    };
    let mut job = match prepared.spawn(&script, &cancel, Box::new(TestSink::default())) {
        Ok(job) => job,
        Err(e) => panic!("{:?}", e.reason),
    };
    let limit = Instant::now() + Duration::from_secs(10);
    while !(job.reaped
        && job.status_eof
        && job.out.as_mut().unwrap().eof
        && job.err.as_mut().unwrap().eof)
    {
        let _ = job.poll(Instant::now());
        assert!(Instant::now() < limit, "killed guardian did not end");
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(!job.exited, "ended by a signal");
    assert!(job.faulted() && !job.spawned() && !job.settled());
    assert!(!job.status_received);
    assert!(!job.release_pending());
    assert!(job.take_cleanup().is_none());
    // Test hygiene only: remove the scratch the job keeps.
    job.cleanup
        .take()
        .expect("the token stays")
        .run(Instant::now() + Duration::from_secs(2), &cancel)
        .unwrap();
}

/// A notice fixture for `profile`.
fn feed_profile(profile: Profile, notices: &[Notice]) -> (ExecCmdJob, Result<(), ExecCmdError>) {
    let (mut job, write) = notice_fixture();
    job.profile = profile;
    for n in notices {
        let bytes = n.encode([7; 16]);
        // SAFETY: fixed bytes into the owned fixture pipe, below PIPE_BUF.
        assert_eq!(
            unsafe { libc::write(write.as_raw_fd(), bytes.as_ptr().cast(), bytes.len()) },
            32
        );
    }
    drop(write);
    let result = pump(&mut job);
    (job, result)
}
/// Kind 2 reasons 14 and 15 (escapee-control.md, guardian status notice
/// extensions): valid shapes; after kind 6 only for `person-outside-1`;
/// reason 12 never for it; none of 12, 14 or 15 without kind 6.
#[test]
fn p1_outside_reasons_follow_the_profile() {
    let id = [9; 16];
    for reason in [14, 15] {
        let n = Notice::decode(&notice(2, reason, 1, true, 0).encode(id), id).unwrap();
        assert_eq!(n.reason, reason);
        assert!(Notice::decode(&notice(4, reason, 0, false, 0).encode(id), id).is_err());
        assert!(Notice::decode(&notice(3, reason, 0, true, 0).encode(id), id).is_err());
    }
    assert!(Notice::decode(&notice(2, 16, 1, true, 0).encode(id), id).is_err());
    let spawned = notice(1, 0, 0, false, 0);
    let exited = notice(5, 0, 1, false, 0);
    let report = notice(6, 0, 0, false, 2);
    for reason in [14, 15] {
        let (job, polled) = feed_profile(
            Profile::PersonOutside1,
            &[spawned, exited, report, notice(2, reason, 1, true, 0)],
        );
        assert_eq!(polled, Ok(()), "outside reason {reason}");
        let settled = job.guardian_settled().unwrap();
        assert_eq!(settled.escapees, 2);
        assert_eq!(settled.output_held, reason == 15);
        assert_eq!(settled.exit, Exit::Exited(0));
        assert_eq!(job.result().unwrap().escapees, 2);
    }
    // No survivor: the lease reason, no kind 6.
    let (job, polled) = feed_profile(
        Profile::PersonOutside1,
        &[spawned, exited, notice(2, 0, 1, true, 0)],
    );
    assert_eq!(polled, Ok(()));
    assert!(!job.guardian_settled().unwrap().output_held);
    for (label, profile, sequence) in [
        (
            "reason 12 for person-outside-1",
            Profile::PersonOutside1,
            vec![spawned, exited, report, notice(2, 12, 1, true, 0)],
        ),
        (
            "reason 14 for model-1",
            Profile::Model1,
            vec![spawned, exited, report, notice(2, 14, 1, true, 0)],
        ),
        (
            "reason 15 for person-1",
            Profile::Person1,
            vec![spawned, exited, report, notice(2, 15, 1, true, 0)],
        ),
        (
            "reason 14 without kind 6",
            Profile::PersonOutside1,
            vec![spawned, exited, notice(2, 14, 1, true, 0)],
        ),
        (
            "reason 15 without kind 6",
            Profile::PersonOutside1,
            vec![spawned, exited, notice(2, 15, 1, true, 0)],
        ),
        (
            "plain settle after kind 6",
            Profile::PersonOutside1,
            vec![spawned, exited, report, notice(2, 0, 1, true, 0)],
        ),
    ] {
        let (job, polled) = feed_profile(profile, &sequence);
        assert_eq!(polled, Err(ExecCmdError::Protocol), "{label}");
        assert!(job.guardian_settled().is_none(), "{label}");
        assert!(job.result().is_none(), "{label}");
    }
    // `person-1` keeps reason 12 after kind 6.
    let (job, polled) = feed_profile(
        Profile::Person1,
        &[spawned, exited, report, notice(2, 12, 1, true, 0)],
    );
    assert_eq!(polled, Ok(()));
    assert_eq!(job.result().unwrap().escapees, 2);
}
/// OD2: after kind 2 reason 15 the job keeps capturing until EOF;
/// `close_capture` is refused before the guardian settled, and afterwards
/// closes both readers, which settles the job without EOF.
#[test]
fn p1_survivor_output_drains_until_eof_or_close_capture() {
    let (mut job, write) = notice_fixture();
    job.profile = Profile::PersonOutside1;
    let (out, out_writer) = spawn::pipe(false).unwrap();
    let (err, err_writer) = spawn::pipe(false).unwrap();
    job.out = Some(Capture::new(out, Stream::Stdout));
    job.err = Some(Capture::new(err, Stream::Stderr));
    assert!(!job.close_capture(), "before any notice");
    for n in [
        notice(1, 0, 0, false, 0),
        notice(5, 0, 1, false, 3),
        notice(6, 0, 0, false, 1),
        notice(2, 15, 1, true, 3),
    ] {
        let bytes = n.encode([7; 16]);
        // SAFETY: fixed bytes into the owned fixture pipe.
        assert_eq!(
            unsafe { libc::write(write.as_raw_fd(), bytes.as_ptr().cast(), bytes.len()) },
            32
        );
    }
    drop(write);
    let limit = Instant::now() + Duration::from_secs(10);
    while !job.status_eof {
        job.poll(Instant::now()).unwrap();
        assert!(Instant::now() < limit);
        std::thread::sleep(Duration::from_millis(1));
    }
    // The survivor still writes: capture continues, the job is not settled.
    // SAFETY: a fixed write to the owned fixture writer.
    assert_eq!(
        unsafe { libc::write(out_writer.as_raw_fd(), b"late".as_ptr().cast(), 4) },
        4
    );
    job.poll(Instant::now()).unwrap();
    let settled = job.guardian_settled().unwrap();
    assert!(settled.output_held);
    assert_eq!(settled.exit, Exit::Exited(3));
    assert!(!job.settled(), "capture still open");
    assert_eq!(job.interests().len(), 2, "both capture pipes stay polled");
    assert!(job.close_capture());
    assert!(job.capture_closed());
    assert!(job.settled());
    assert!(job.interests().is_empty());
    let result = job.result().unwrap();
    assert_eq!((result.exit_code, result.escapees), (Some(3), 1));
    assert!(result.cleanup_confirmed);
    drop((out_writer, err_writer));
}

/// Build an FD 11 frame by hand: roles, then (name, value) entries.
fn raw_frame(roles: &[u8], entries: &[(&[u8], &[u8])]) -> Vec<u8> {
    let mut body = vec![roles.len() as u8];
    body.extend_from_slice(roles);
    body.extend_from_slice(&(entries.len() as u16).to_be_bytes());
    for (name, value) in entries {
        body.extend_from_slice(&(name.len() as u16).to_be_bytes());
        body.extend_from_slice(name);
        body.extend_from_slice(&(value.len() as u32).to_be_bytes());
        body.extend_from_slice(value);
    }
    let mut frame = (body.len() as u32).to_be_bytes().to_vec();
    frame.extend_from_slice(&body);
    frame
}
const PERSON_BASE: [(&[u8], &[u8]); 3] = [
    (b"PATH", b"/usr/bin:/bin"),
    (b"HOME", b"/Users/p"),
    (b"TMPDIR", b"/var/folders/x/T/"),
];
/// PC-U50: the FD 11 frame codec in the guardian. Exact parse at each
/// bound; a short frame, trailing bytes, a non-PC3 name or a missing HOME
/// for `person-1`, a role table without the home first, nine roots, and
/// every entry fault are `Invalid` (setup-failed reason 5).
#[test]
fn pc_u50_frame_codec_bounds_and_faults() {
    use person::{RootRole, parse_frame};
    let invalid = |label: &str, frame: &[u8], profile: Profile| {
        assert_eq!(
            parse_frame(frame, profile).err(),
            Some(ExecCmdError::Invalid),
            "{label}"
        );
    };
    // Exact person-1 parse: roles and entries in order.
    let mut entries = PERSON_BASE.to_vec();
    entries.extend([
        (&b"USER"[..], &b"p"[..]),
        (b"LOGNAME", b"p"),
        (b"TERM", b"dumb"),
        (b"GIT_TERMINAL_PROMPT", b"0"),
        (b"LANG", b"en_GB.UTF-8"),
        (b"LC_ALL", b"C"),
        (b"SSH_AUTH_SOCK", b"/tmp/agent.sock"),
    ]);
    let frame = raw_frame(&[1, 2, 3, 4, 4, 4, 4, 4], &entries);
    let parsed = parse_frame(&frame, Profile::Person1).unwrap();
    assert_eq!(
        parsed.roles,
        [
            RootRole::Home,
            RootRole::TempDir,
            RootRole::Developer,
            RootRole::Cache,
            RootRole::Cache,
            RootRole::Cache,
            RootRole::Cache,
            RootRole::Cache
        ]
    );
    let got: Vec<&[u8]> = parsed.entries.iter().map(|e| e.as_slice()).collect();
    assert_eq!(got[0], b"PATH=/usr/bin:/bin");
    assert_eq!(got[9], b"SSH_AUTH_SOCK=/tmp/agent.sock");
    assert_eq!(got.len(), 10);
    // Bounds (ENV1): 128 entries and 40,960 bytes exactly, for both
    // profiles; the largest frame is within MAX_FRAME_BYTES.
    let names: Vec<Vec<u8>> = (0..125).map(|i| format!("V{i:03}").into_bytes()).collect();
    let base_bytes: usize = PERSON_BASE.iter().map(|(n, v)| n.len() + v.len()).sum();
    let name_bytes: usize = names.iter().map(Vec::len).sum::<usize>() + base_bytes;
    let value = vec![b'v'; (40_960 - name_bytes) / 125];
    let mut values: Vec<Vec<u8>> = vec![value; 125];
    let used: usize = name_bytes + values.iter().map(Vec::len).sum::<usize>();
    values[0].extend(std::iter::repeat_n(b'v', 40_960 - used));
    let mut full: Vec<(&[u8], &[u8])> = PERSON_BASE.to_vec();
    full.extend(
        names
            .iter()
            .zip(&values)
            .map(|(n, v)| (n.as_slice(), v.as_slice())),
    );
    let frame = raw_frame(&[], &full);
    assert!(frame.len() <= MAX_FRAME_BYTES);
    assert_eq!(MAX_FRAME_BYTES, 41_743);
    assert_eq!(
        parse_frame(&frame, Profile::PersonOutside1)
            .unwrap()
            .entries
            .len(),
        128
    );
    let mut over_bytes = values.clone();
    over_bytes[1].push(b'v');
    let mut over: Vec<(&[u8], &[u8])> = PERSON_BASE.to_vec();
    over.extend(
        names
            .iter()
            .zip(&over_bytes)
            .map(|(n, v)| (n.as_slice(), v.as_slice())),
    );
    invalid(
        "40,961 bytes",
        &raw_frame(&[], &over),
        Profile::PersonOutside1,
    );
    let mut small: Vec<(&[u8], &[u8])> = PERSON_BASE.to_vec();
    small.extend(names.iter().map(|n| (n.as_slice(), &b""[..])));
    small.push((b"EXTRA", b""));
    invalid(
        "129 entries",
        &raw_frame(&[], &small),
        Profile::PersonOutside1,
    );
    let long_name = vec![b'N'; 64];
    let mut long: Vec<(&[u8], &[u8])> = PERSON_BASE.to_vec();
    long.push((&long_name, b"x"));
    assert!(parse_frame(&raw_frame(&[], &long), Profile::PersonOutside1).is_ok());
    let longer = vec![b'N'; 65];
    long.pop();
    long.push((&longer, b"x"));
    invalid(
        "name 65 bytes",
        &raw_frame(&[], &long),
        Profile::PersonOutside1,
    );
    // Short frames, trailing bytes, a length that disagrees.
    let good = raw_frame(&[1, 2], &PERSON_BASE);
    for cut in [0, 3, 4, 5, good.len() - 1] {
        invalid(&format!("short at {cut}"), &good[..cut], Profile::Person1);
    }
    let mut trailing = good.clone();
    trailing.push(0);
    invalid("trailing byte", &trailing, Profile::Person1);
    // Name rules, the same for both profiles (ENV1): a declared name is
    // accepted; a terminal name with an invalid value is not.
    let mut other = PERSON_BASE.to_vec();
    other.push((b"EDITOR", b"vi"));
    assert!(parse_frame(&raw_frame(&[1, 2], &other), Profile::Person1).is_ok());
    assert!(parse_frame(&raw_frame(&[], &other), Profile::PersonOutside1).is_ok());
    let mut bad_class = PERSON_BASE.to_vec();
    bad_class.push((b"LANG", b"en GB"));
    invalid(
        "LANG class",
        &raw_frame(&[1, 2], &bad_class),
        Profile::Person1,
    );
    invalid(
        "LANG class",
        &raw_frame(&[], &bad_class),
        Profile::PersonOutside1,
    );
    invalid(
        "outside without HOME",
        &raw_frame(
            &[],
            &PERSON_BASE[..1]
                .iter()
                .chain(&PERSON_BASE[2..])
                .copied()
                .collect::<Vec<_>>(),
        ),
        Profile::PersonOutside1,
    );
    invalid(
        "no HOME",
        &raw_frame(
            &[1, 2],
            &PERSON_BASE[..1]
                .iter()
                .chain(&PERSON_BASE[2..])
                .copied()
                .collect::<Vec<_>>(),
        ),
        Profile::Person1,
    );
    invalid(
        "no PATH",
        &raw_frame(&[1, 2], &PERSON_BASE[1..]),
        Profile::Person1,
    );
    invalid(
        "home missing",
        &raw_frame(&[2, 3], &PERSON_BASE),
        Profile::Person1,
    );
    invalid(
        "TMPDIR first",
        &raw_frame(&[2, 1], &PERSON_BASE),
        Profile::Person1,
    );
    invalid(
        "home twice",
        &raw_frame(&[1, 2, 1], &PERSON_BASE),
        Profile::Person1,
    );
    invalid(
        "two developer roots",
        &raw_frame(&[1, 2, 3, 3], &PERSON_BASE),
        Profile::Person1,
    );
    invalid(
        "six caches",
        &raw_frame(&[1, 2, 4, 4, 4, 4, 4, 4], &PERSON_BASE),
        Profile::Person1,
    );
    invalid(
        "role 5",
        &raw_frame(&[1, 2, 5], &PERSON_BASE),
        Profile::Person1,
    );
    invalid(
        "nine roots",
        &raw_frame(&[1, 2, 3, 4, 4, 4, 4, 4, 4], &PERSON_BASE),
        Profile::Person1,
    );
    invalid(
        "outside with roots",
        &raw_frame(&[1, 2], &PERSON_BASE),
        Profile::PersonOutside1,
    );
    invalid("model-1 frame", &raw_frame(&[], &[]), Profile::Model1);
    // Entry faults for either profile.
    for (label, name, value) in [
        ("empty name", &b""[..], &b"x"[..]),
        ("= in name", b"A=B", b"x"),
        ("NUL in name", b"A\0B", b"x"),
        ("NUL in value", b"A", b"x\0y"),
        ("secret name", b"ASURA_SECRET_1", b"x"),
        ("secret command", b"ASURA_SECRET_COMMAND", b"x"),
        ("job marker", b"ASURA_JOB_00", b"1"),
    ] {
        let mut entries = PERSON_BASE.to_vec();
        entries.push((name, value));
        invalid(label, &raw_frame(&[], &entries), Profile::PersonOutside1);
    }
    let mut duplicate = PERSON_BASE.to_vec();
    duplicate.extend([(&b"A"[..], &b"1"[..]), (b"A", b"2")]);
    invalid(
        "duplicate",
        &raw_frame(&[], &duplicate),
        Profile::PersonOutside1,
    );
    // The service-side encoder round-trips through the guardian parser,
    // with the account values set by the setup and a declared variable.
    let dir = std::env::temp_dir();
    let client = vec![
        (b"LANG".to_vec(), b"C".to_vec()),
        (b"DYLD_INSERT_LIBRARIES".to_vec(), b"/x.dylib".to_vec()),
    ];
    let setup = PersonSetup::outside_for_test("p", &dir, &dir, b"/bin", &client).unwrap();
    assert_eq!(
        setup.names(),
        [
            "PATH",
            "HOME",
            "USER",
            "LOGNAME",
            "SHELL",
            "TMPDIR",
            "TERM",
            "GIT_TERMINAL_PROMPT",
            "LANG",
            "DYLD_INSERT_LIBRARIES"
        ]
    );
    let parsed = parse_frame(&setup.frame(), Profile::PersonOutside1).unwrap();
    assert!(parsed.roles.is_empty());
    assert_eq!(
        parsed.entries[9].as_slice(),
        b"DYLD_INSERT_LIBRARIES=/x.dylib"
    );
    // The encoder refuses what the parser refuses, and Asura-set names
    // from a client.
    for bad in [
        vec![(b"ASURA_JOB_1".to_vec(), b"1".to_vec())],
        vec![(b"A=".to_vec(), b"1".to_vec())],
        vec![(b"HOME".to_vec(), b"/elsewhere".to_vec())],
        vec![(b"LANG".to_vec(), b"bad value".to_vec())],
        vec![
            (b"A".to_vec(), b"1".to_vec()),
            (b"A".to_vec(), b"2".to_vec()),
        ],
    ] {
        assert_eq!(
            PersonSetup::outside_for_test("p", &dir, &dir, b"/bin", &bad).err(),
            Some(ExecCmdError::Invalid)
        );
    }
}
/// PC-U50 through a real pipe: the guardian's reader takes exactly one
/// frame and its EOF; a short frame, trailing bytes, an oversized length
/// or lease input end it without a frame.
#[test]
fn pc_u50_frame_reader_over_a_pipe() {
    let read = |bytes: &[u8], stop: bool| {
        let (r, w) = spawn::pipe(true).unwrap();
        // SAFETY: bounded write to the owned fixture pipe, below its buffer.
        assert_eq!(
            unsafe { libc::write(w.as_raw_fd(), bytes.as_ptr().cast(), bytes.len()) },
            bytes.len() as isize
        );
        drop(w);
        let (lease, _lease_writer) = spawn::pipe(true).unwrap();
        person::read_frame(r.as_raw_fd(), lease.as_raw_fd(), &mut || stop).map(|f| f.to_vec())
    };
    let good = raw_frame(&[1, 2], &PERSON_BASE);
    assert_eq!(read(&good, false).unwrap(), good);
    assert_eq!(
        read(&good[..good.len() - 2], false).err(),
        Some(ExecCmdError::Invalid)
    );
    let mut trailing = good.clone();
    trailing.extend_from_slice(b"xx");
    assert_eq!(read(&trailing, false).err(), Some(ExecCmdError::Invalid));
    assert_eq!(
        read(&[0, 1, 0, 0], false).err(),
        Some(ExecCmdError::Invalid),
        "over the bound"
    );
    assert_eq!(read(&[], false).err(), Some(ExecCmdError::Invalid));
    // An open writer that sends nothing: lease input refuses the job.
    let (r, _w) = spawn::pipe(true).unwrap();
    let (lease, _lw) = spawn::pipe(true).unwrap();
    assert_eq!(
        person::read_frame(r.as_raw_fd(), lease.as_raw_fd(), &mut || true).err(),
        Some(ExecCmdError::Cancelled)
    );
}
/// The service-side writer: completion, a closed reader, and cancel.
#[test]
fn p1_frame_writer_ends_on_completion_peer_close_or_cancel() {
    use person::{FrameWrite, write_frame};
    let frame = vec![7u8; 100_000];
    let (r, w) = spawn::pipe(true).unwrap();
    spawn::no_sigpipe(w.as_raw_fd()).unwrap();
    let reader = std::thread::spawn(move || {
        let mut got = Vec::new();
        let mut file = std::fs::File::from(r);
        loop {
            let mut buf = [0u8; 8192];
            match std::io::Read::read(&mut file, &mut buf) {
                Ok(0) => break got,
                Ok(n) => got.extend_from_slice(&buf[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(1))
                }
                Err(e) => panic!("{e}"),
            }
        }
    });
    assert_eq!(
        write_frame(w, &frame, &AtomicBool::new(false)),
        FrameWrite::Complete
    );
    assert_eq!(reader.join().unwrap(), frame);
    let (r, w) = spawn::pipe(true).unwrap();
    spawn::no_sigpipe(w.as_raw_fd()).unwrap();
    drop(r);
    assert_eq!(
        write_frame(w, &frame, &AtomicBool::new(false)),
        FrameWrite::PeerClosed
    );
    let (_r, w) = spawn::pipe(true).unwrap();
    assert_eq!(
        write_frame(w, &frame, &AtomicBool::new(true)),
        FrameWrite::Cancelled
    );
    // A full pipe with no reader progress: cancel ends the paced wait.
    let (_r, w) = spawn::pipe(true).unwrap();
    let cancel = std::sync::Arc::new(AtomicBool::new(false));
    let flag = cancel.clone();
    let writer = std::thread::spawn(move || write_frame(w, &vec![1u8; 1 << 20], &flag));
    std::thread::sleep(Duration::from_millis(250));
    cancel.store(true, Ordering::Release);
    assert_eq!(writer.join().unwrap(), FrameWrite::Cancelled);
}

fn person_paths<'a>(caches: Vec<&'a str>, developer: Option<&'a str>) -> person::PersonPaths<'a> {
    person::PersonPaths {
        project: "/Users/p/src/proj",
        scratch: "/Users/p/.asura/tmp/exec-cmd-00",
        asura: "/Users/p/.asura",
        home: "/Users/p",
        tmpdir: "/private/var/folders/x/T",
        developer,
        caches,
        fixed: vec!["/Library", "/Applications", "/private/etc"],
    }
}
/// The `(allow file-write* ...)` rule of a profile.
fn write_rule(profile: &str) -> &str {
    let start = profile.find("(allow file-write*").expect("write rule");
    let mut depth = 0;
    for (i, c) in profile[start..].char_indices() {
        match c {
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return &profile[start..start + i + 1];
                }
            }
            _ => {}
        }
    }
    unreachable!("unbalanced write rule")
}
/// PC-U25: the person profile from fixture roots. Writes only to the
/// project, the scratch (OD1), `$TMPDIR` and the existing caches, plus
/// `/dev/null`; reads under home; the network rule and the Asura-home
/// socket deny are present; no `mach-lookup`; the Asura deny is after
/// every allow and the scratch re-allow after it; no command text.
#[test]
fn pc_u25_person_profile_from_fixture_roots() {
    let paths = person_paths(vec!["/Users/p/.cargo", "/Users/p/Library/Caches"], None);
    let profile = person::seatbelt(&paths).unwrap();
    assert!(profile.starts_with("(version 1)(deny default)"));
    assert_eq!(
        write_rule(&profile),
        "(allow file-write* (subpath \"/Users/p/src/proj\") (subpath \"/Users/p/.asura/tmp/exec-cmd-00\") (subpath \"/private/var/folders/x/T\") (subpath \"/Users/p/.cargo\") (subpath \"/Users/p/Library/Caches\"))"
    );
    assert_eq!(
        profile.matches("(allow file-write*").count(),
        2,
        "rule plus scratch re-allow"
    );
    assert!(profile.contains("(allow file-read* file-write* (literal \"/dev/null\"))"));
    assert!(!write_rule(&profile).contains("(subpath \"/Users/p\")"));
    let reads = profile
        .find("(allow process-exec file-map-executable file-read*")
        .unwrap();
    let read_rule = &profile[reads..reads + profile[reads..].find("))").unwrap()];
    for root in [
        "/bin",
        "/System",
        "/Library",
        "/Applications",
        "/private/etc",
        "/Users/p",
        "/private/var/folders/x/T",
        "/Users/p/.cargo",
    ] {
        assert!(
            read_rule.contains(&format!("(subpath \"{root}\")")),
            "{root}"
        );
    }
    assert!(
        !read_rule.contains("/opt/homebrew"),
        "absent fixed root omitted"
    );
    assert!(profile.contains("(allow network*)"));
    assert!(profile.contains("(deny network* (remote unix-socket (subpath \"/Users/p/.asura\")))"));
    assert!(!profile.contains("mach-lookup"));
    assert!(profile.contains("(allow sysctl-read)"));
    let deny = profile
        .find("(deny file-read* file-write* process-exec file-map-executable (subpath \"/Users/p/.asura\"))")
        .unwrap();
    let reallow = profile
        .rfind("(allow file-write* (subpath \"/Users/p/.asura/tmp/exec-cmd-00\"))")
        .unwrap();
    assert!(deny > profile.find("(allow network*)").unwrap());
    assert!(deny > reads && reallow > deny);
    for text in ["rm -rf", "echo", "$HOME", "COMMAND"] {
        assert!(!profile.contains(text), "{text}");
    }
    // A quote in a descriptor path is escaped, never closes the literal.
    let mut quoted = person_paths(vec![], None);
    quoted.project = "/Users/p/we\"ird";
    assert!(
        person::seatbelt(&quoted)
            .unwrap()
            .contains("(subpath \"/Users/p/we\\\"ird\")")
    );
}
/// PC-U51: with a scratch, a developer directory outside the fixed roots,
/// and one cache: the scratch write rule is present (OD1), the developer
/// root is read and exec only, the missing caches are omitted, and no
/// command or client text appears.
#[test]
fn pc_u51_person_profile_with_developer_and_missing_cache() {
    let paths = person_paths(vec!["/Users/p/.cargo"], Some("/opt/dev"));
    let profile = person::seatbelt(&paths).unwrap();
    assert!(write_rule(&profile).contains("(subpath \"/Users/p/.asura/tmp/exec-cmd-00\")"));
    assert!(profile.contains("(subpath \"/opt/dev\")"));
    assert!(!write_rule(&profile).contains("/opt/dev"));
    for cache in [".rustup", ".cache", "Library/Caches", ".npm"] {
        assert!(!profile.contains(cache), "{cache}");
    }
    assert!(profile.contains("(literal \"/opt\")"), "ancestor metadata");
    for client in ["en_GB", "SSH_AUTH_SOCK", "agent", "PATH"] {
        assert!(!profile.contains(client), "{client}");
    }
}
/// PC-U52: developer directory selection without running a process.
#[test]
fn pc_u52_developer_directory_selection() {
    use person::{Developer, developer_target};
    let f = Fixture::new();
    let root = f.0.clone();
    let dev = root.join("opt-dev");
    fs::create_dir(&dev).unwrap();
    let missing_clt = root.join("no-clt");
    let link = root.join("xcode_select_link");
    symlink("/Applications/Xcode.app/Contents/Developer", &link).unwrap();
    assert_eq!(developer_target(&link, &missing_clt), Developer::Covered);
    fs::remove_file(&link).unwrap();
    symlink(&dev, &link).unwrap();
    assert_eq!(
        developer_target(&link, &missing_clt),
        Developer::Root(dev.clone())
    );
    fs::remove_file(&link).unwrap();
    // No link: CommandLineTools under /Library is covered, or omitted.
    let clt = Path::new("/Library/Developer/CommandLineTools");
    if clt.is_dir() {
        assert_eq!(developer_target(&link, clt), Developer::Covered);
    }
    assert_eq!(
        developer_target(&link, &dev),
        Developer::Root(dev.clone()),
        "fallback outside the fixed roots"
    );
    assert_eq!(developer_target(&link, &missing_clt), Developer::Omitted);
    symlink("relative/Developer", &link).unwrap();
    assert_eq!(developer_target(&link, &missing_clt), Developer::Omitted);
    fs::remove_file(&link).unwrap();
    symlink(root.join("gone"), &link).unwrap();
    assert_eq!(
        developer_target(&link, &missing_clt),
        Developer::Omitted,
        "missing target"
    );
}
/// PC-U51 at setup: a fixture home with `.cargo`, a symlinked `.npm` and a
/// `Library/Caches`; a developer root inside `/Applications` gets no
/// descriptor, one outside does. The frame carries the PC3 names.
#[test]
fn pc_u51_setup_roots_from_a_fixture_home() {
    let f = Fixture::new();
    let home = f.0.join("home");
    let tmp = f.0.join("tmp");
    let dev = f.0.join("dev");
    for dir in [
        &home,
        &tmp,
        &dev,
        &home.join(".cargo"),
        &home.join("Library/Caches"),
    ] {
        fs::create_dir_all(dir).unwrap();
    }
    symlink(&tmp, home.join(".npm")).unwrap();
    let client = vec![(b"LANG".to_vec(), b"en_GB.UTF-8".to_vec())];
    let setup =
        PersonSetup::sandboxed_for_test("p", &home, &tmp, Some(&dev), b"/usr/bin:/bin", &client)
            .unwrap();
    assert_eq!(
        setup.roles(),
        [
            RootRole::Home,
            RootRole::TempDir,
            RootRole::Developer,
            RootRole::Cache,
            RootRole::Cache
        ]
    );
    assert_eq!(
        setup.names(),
        [
            "PATH",
            "HOME",
            "USER",
            "LOGNAME",
            "SHELL",
            "TMPDIR",
            "TERM",
            "GIT_TERMINAL_PROMPT",
            "LANG"
        ]
    );
    let parsed = person::parse_frame(&setup.frame(), Profile::Person1).unwrap();
    assert_eq!(parsed.roles, setup.roles());
    let covered = PersonSetup::sandboxed_for_test(
        "p",
        &home,
        &tmp,
        Some(Path::new("/Applications")),
        b"/usr/bin",
        &[],
    )
    .unwrap();
    assert!(!covered.roles().contains(&RootRole::Developer));
    // A missing home or TMPDIR is `sandbox_unavailable`.
    let missing = f.0.join("missing");
    for (h, t) in [(&missing, &tmp), (&home, &missing)] {
        assert_eq!(
            PersonSetup::sandboxed_for_test("p", h, t, None, b"/bin", &[]).err(),
            Some(ExecCmdError::Unavailable)
        );
    }
    // SSH_AUTH_SOCK naming a regular file is dropped; the setup succeeds.
    fs::write(f.0.join("not-a-socket"), b"").unwrap();
    let sock = vec![(
        b"SSH_AUTH_SOCK".to_vec(),
        f.0.join("not-a-socket")
            .into_os_string()
            .into_encoded_bytes(),
    )];
    let dropped = PersonSetup::sandboxed_for_test("p", &home, &tmp, None, b"/bin", &sock).unwrap();
    assert!(!dropped.names().contains(&"SSH_AUTH_SOCK".to_owned()));
}
/// PC2 and PC3 at setup: PATH form and client names and values.
#[test]
fn p1_setup_checks_path_and_client_variables() {
    use person::{check_client, check_path};
    assert!(check_path(b"/usr/bin:/bin").is_ok());
    let long = format!("/{}", "a".repeat(4095));
    assert!(check_path(long.as_bytes()).is_ok());
    let too_long = format!("/{}", "a".repeat(4096));
    let many = vec!["/b"; 65].join(":");
    for bad in [
        &b""[..],
        b"bin",
        b"/bin:",
        b"/bin::/usr/bin",
        b"/bin\0",
        too_long.as_bytes(),
        many.as_bytes(),
    ] {
        assert_eq!(check_path(bad), Err(ExecCmdError::Invalid), "{bad:?}");
    }
    let ok = vec![
        (b"LANG".to_vec(), b"en_GB.UTF-8".to_vec()),
        (b"LC_CTYPE".to_vec(), b"de_DE@euro".to_vec()),
        (b"SSH_AUTH_SOCK".to_vec(), b"/tmp/s".to_vec()),
    ];
    assert!(check_client(&ok).is_ok());
    // A declared variable is a valid client entry (ENV1).
    assert!(check_client(&[(b"EDITOR".to_vec(), b"vi".to_vec())]).is_ok());
    for bad in [
        vec![(b"EDITOR".to_vec(), b"a\nb".to_vec())],
        vec![(b"ASURA_X".to_vec(), b"1".to_vec())],
        vec![(b"PATH".to_vec(), b"/bin".to_vec())],
        vec![(b"HOME".to_vec(), b"/x".to_vec())],
        vec![(b"LANG".to_vec(), b"en GB".to_vec())],
        vec![(b"LANG".to_vec(), vec![b'a'; 65])],
        vec![(b"LANG".to_vec(), Vec::new())],
        vec![(b"SSH_AUTH_SOCK".to_vec(), b"relative".to_vec())],
        vec![(
            b"SSH_AUTH_SOCK".to_vec(),
            [b"/".to_vec(), vec![b'a'; 103]].concat(),
        )],
        vec![
            (b"LANG".to_vec(), b"C".to_vec()),
            (b"LANG".to_vec(), b"C".to_vec()),
        ],
    ] {
        assert_eq!(check_client(&bad).err(), Some(ExecCmdError::Invalid));
    }
}
/// `prepare` refuses a person profile without its setup, and
/// `prepare_person` a setup of the other profile or a model request.
#[test]
fn p1_prepare_requires_the_matching_person_setup() {
    let f = Fixture::new();
    fs::create_dir(f.0.join("project")).unwrap();
    let runtime = || RuntimeDirectory::scratch(&f.0, true).unwrap();
    let project = ProjectIdentity::open(
        f.0.join("project").to_str().unwrap(),
        &runtime().state_boundary(),
    )
    .unwrap();
    let cancel = AtomicBool::new(false);
    let with = |profile| ExecCmdRequest {
        profile,
        ..request()
    };
    for profile in [Profile::Person1, Profile::PersonOutside1] {
        let refused = PreparedExecCmd::prepare(runtime(), &project, with(profile), &cancel);
        assert_eq!(refused.err().map(|e| e.reason), Some(ExecCmdError::Invalid));
    }
    let dir = std::env::temp_dir();
    let outside = || PersonSetup::outside_for_test("p", &dir, &dir, b"/bin", &[]).unwrap();
    for profile in [Profile::Model1, Profile::Person1] {
        let refused =
            PreparedExecCmd::prepare_person(runtime(), &project, with(profile), outside(), &cancel);
        assert_eq!(
            refused.err().map(|e| e.reason),
            Some(ExecCmdError::Invalid),
            "{profile:?}"
        );
    }
    let prepared = PreparedExecCmd::prepare_person(
        runtime(),
        &project,
        with(Profile::PersonOutside1),
        outside(),
        &cancel,
    );
    let mut prepared = prepared.unwrap_or_else(|e| panic!("{:?}", e.reason));
    prepared
        .cleanup
        .take()
        .unwrap()
        .run(Instant::now() + Duration::from_secs(2), &cancel)
        .unwrap();
}
/// The `KERN_PROCARGS2` environment match is exact and skips argv.
#[test]
fn p1_environment_marker_match() {
    let record = |argv: &[&str], env: &[&str]| {
        let mut r = (argv.len() as i32).to_ne_bytes().to_vec();
        r.extend_from_slice(b"/bin/x\0\0\0\0");
        for s in argv.iter().chain(&[""][..0]) {
            r.extend_from_slice(s.as_bytes());
            r.push(0);
        }
        for s in env {
            r.extend_from_slice(s.as_bytes());
            r.push(0);
        }
        r.push(0);
        r
    };
    let marker = b"ASURA_JOB_00ff=1";
    use escapee::environment_holds;
    assert!(environment_holds(
        &record(&["x"], &["A=1", "ASURA_JOB_00ff=1"]),
        marker
    ));
    assert!(
        !environment_holds(&record(&["ASURA_JOB_00ff=1"], &["A=1"]), marker),
        "argv is not env"
    );
    assert!(!environment_holds(
        &record(&["x"], &["ASURA_JOB_00ff=10"]),
        marker
    ));
    assert!(
        !environment_holds(&record(&["x"], &["", "ASURA_JOB_00ff=1"]), marker),
        "after the end"
    );
    assert!(!environment_holds(&[1, 0], marker));
    assert!(!environment_holds(&record(&["a", "b"], &[]), marker));
}
