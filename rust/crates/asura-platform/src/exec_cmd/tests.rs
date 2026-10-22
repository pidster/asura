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
            lease: None,
            status: read,
            status_bytes: Vec::new(),
            status_eof: false,
            notices: 0,
            spawned: false,
            last_kind: 0,
            leader: None,
            unconfirmed: None,
            escapees: 0,
            terminal: None,
            out: Capture::new(out, Stream::Stdout),
            err: Capture::new(err, Stream::Stderr),
            sink: Box::new(TestSink {
                room: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(512 * 1024)),
                ..TestSink::default()
            }),
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
    job.stop(StopKind::Term);
    assert_eq!(job.interests().len(), 3, "drain to EOF after cancel");
}
/// J1T-07 unit: more than 64 MiB of output does not stop the job (J1-Q2);
/// reason 3 `output_limit` is unassigned (J2).
#[test]
fn j1t_07_no_output_limit_producer() {
    let (mut job, _write) = notice_fixture();
    job.out.total = 80 * 1024 * 1024;
    job.err.total = 80 * 1024 * 1024;
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
        if job.status_eof && job.out.eof && job.err.eof {
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
    assert!(
        result
            .render_text(&Tails::default(), "complete")
            .contains("cleanup_confirmed=true capture=complete escapees=3\nstdout:")
    );
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
        assert!(
            result
                .render_text(&Tails::default(), "complete")
                .contains("cleanup_confirmed=false capture=complete escapees=3\n")
        );
    }
    // No escapees: no kind 6, the lease reason stays, no count in the text.
    let (job, polled) = feed(&[spawned, notice(5, 0, 2, false, 9), notice(2, 4, 2, true, 9)]);
    assert_eq!(polled, Ok(()));
    let result = job.result().unwrap();
    assert!(result.cleanup_confirmed);
    assert_eq!(result.escapees, 0);
    assert!(
        !result
            .render_text(&Tails::default(), "complete")
            .contains("escapees")
    );
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

/// J2T-05 unit: the profile token is exact; P1's person tokens are not
/// accepted yet; `model-1` has the 500 ms grace.
#[test]
fn j2t_05_profile_token_parse() {
    assert_eq!(Profile::parse("model-1"), Some(Profile::Model1));
    assert_eq!(Profile::Model1.token(), "model-1");
    assert_eq!(Profile::Model1.grace_ns(), 500_000_000);
    for token in [
        "",
        "model-2",
        "Model-1",
        "model-1 ",
        "model",
        "person-1",
        "person-outside-1",
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
