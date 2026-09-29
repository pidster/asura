use super::*;
use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
fn request() -> ShellRequest {
    ShellRequest {
        job_id: [7; 16],
        command: "printf ok".into(),
        cwd: None,
        deadline: Instant::now() + Duration::from_secs(5),
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
#[test]
fn capture_is_bounded_sanitized_and_marks_tail_loss() {
    let (read, write) = spawn::pipe(false).unwrap();
    let mut c = Capture::new(read);
    let bytes = [b'x'; 4096];
    for _ in 0..4 {
        // SAFETY: owned pipe, bounded valid input; drain each write before next.
        assert_eq!(
            unsafe { libc::write(write.as_raw_fd(), bytes.as_ptr().cast(), bytes.len()) },
            4096
        );
        c.drain(4096).unwrap();
    }
    let control = b"\x1b[31m\xff\n";
    assert_eq!(
        unsafe { libc::write(write.as_raw_fd(), control.as_ptr().cast(), control.len()) },
        control.len() as isize
    );
    c.drain(4096).unwrap();
    drop(write);
    c.drain(4096).unwrap();
    let (text, truncated) = c.text();
    assert!(truncated);
    assert!(text.len() <= 6144);
    assert!(!text.contains('\x1b'));
    assert!(c.eof);
    assert_eq!(c.total, 16384 + control.len() as u64);
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
    let project = ProjectIdentity::open(project.to_str().unwrap()).unwrap();
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
    let project = ProjectIdentity::open(path.to_str().unwrap()).unwrap();
    let cancel = AtomicBool::new(false);
    let prepared = match PreparedShell::prepare(runtime, &project, request(), &cancel) {
        Ok(v) => v,
        Err(e) => panic!("{:?}", e.reason),
    };
    fs::rename(&path, f.0.join("old-project")).unwrap();
    fs::create_dir(&path).unwrap();
    let mut failure = match prepared.spawn(Path::new("/usr/bin/true"), &cancel) {
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
fn capture_replaces_short_terminal_controls_and_invalid_utf8() {
    let (read, write) = spawn::pipe(false).unwrap();
    let mut capture = Capture::new(read);
    let bytes = b"ok\x1b[31m\xff\0\n\t";
    // SAFETY: bounded bytes written into our owned pipe.
    assert_eq!(
        unsafe { libc::write(write.as_raw_fd(), bytes.as_ptr().cast(), bytes.len()) },
        bytes.len() as isize
    );
    drop(write);
    capture.drain(4096).unwrap();
    let (text, truncated) = capture.text();
    assert_eq!(text, "ok�[31m��\n\t");
    assert!(truncated);
    assert!(capture.eof);
}

#[test]
fn spawn_rejects_replaced_cwd_before_any_process() {
    let f = Fixture::new();
    let runtime = RuntimeDirectory::scratch(&f.0, true).unwrap();
    let path = f.0.join("project");
    fs::create_dir(&path).unwrap();
    fs::create_dir(path.join("sub")).unwrap();
    let project = ProjectIdentity::open(path.to_str().unwrap()).unwrap();
    let cancel = AtomicBool::new(false);
    let mut req = request();
    req.cwd = Some("./sub/".into());
    let prepared = PreparedShell::prepare(runtime, &project, req, &cancel)
        .unwrap_or_else(|e| panic!("{:?}", e.reason));
    fs::rename(path.join("sub"), path.join("old-sub")).unwrap();
    fs::create_dir(path.join("sub")).unwrap();
    let mut failure = match prepared.spawn(Path::new("/usr/bin/true"), &cancel) {
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
            out: Capture::new(out),
            err: Capture::new(err),
            deadline: Instant::now() + Duration::from_secs(5),
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
