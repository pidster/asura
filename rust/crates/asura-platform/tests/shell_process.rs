//! Native process qualification; private guardian dispatch is identical to the CLI.
use asura_platform::shell::{PreparedShell, ShellRequest, ShellResult, StopReason, run_guardian};
use asura_platform::{ProjectIdentity, RuntimeDirectory};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
static CLEAN: AtomicBool = AtomicBool::new(true);
use std::time::{Duration, Instant};
fn main() {
    let args: Vec<_> = std::env::args().collect();
    if let [_, flag, id, deadline, command] = args.as_slice()
        && flag == "--asura-shell-guardian"
    {
        std::process::exit(run_guardian(id, deadline, command));
    }
    if let [_, flag, home] = args.as_slice()
        && flag == "--lease-holder"
    {
        lease_holder(Path::new(home));
    }
    if args.get(1).is_some_and(|s| s == "--escape") {
        escape_child();
        return;
    }
    qualification();
    println!("shell process qualification passed");
}
struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let name = format!(
            "as-sh-p-{}-{}",
            std::process::id(),
            asura_platform::random_id()
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        );
        let path = std::path::PathBuf::from("/private/tmp").join(name);
        fs::create_dir(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        let path = fs::canonicalize(path).unwrap();
        fs::create_dir(path.join("project")).unwrap();
        Self(path)
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        if CLEAN.load(Ordering::Acquire) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
}
fn prepare(home: &Path, command: &str, timeout: Duration) -> JobGuard {
    let runtime = RuntimeDirectory::scratch(home, true).unwrap();
    let project = ProjectIdentity::open(home.join("project").to_str().unwrap()).unwrap();
    let cancel = AtomicBool::new(false);
    let p = PreparedShell::prepare(
        runtime,
        &project,
        ShellRequest {
            job_id: asura_platform::random_id(),
            command: command.into(),
            cwd: None,
            deadline: Instant::now() + timeout,
        },
        &cancel,
    )
    .unwrap_or_else(|e| panic!("prepare: {:?}", e.reason));
    JobGuard(
        p.spawn(&std::env::current_exe().unwrap(), &cancel)
            .unwrap_or_else(|e| panic!("spawn: {:?}", e.reason)),
    )
}
fn run(home: &Path, command: &str, timeout: Duration, cancel: bool) -> ShellResult {
    let mut job = prepare(home, command, timeout);
    let started = Instant::now();
    let end = started + timeout + Duration::from_secs(4);
    let mut cancelled = false;
    while !job.settled() {
        assert!(Instant::now() < end, "shell failed to settle");
        if cancel && !cancelled && started.elapsed() > Duration::from_millis(200) {
            job.stop(StopReason::Cancelled);
            cancelled = true;
        }
        asura_platform::poll(&job.interests(), Duration::from_millis(20)).unwrap();
        job.poll(Instant::now()).unwrap();
    }
    let result = job.result().unwrap();
    assert!(result.cleanup_confirmed);
    assert!(result.failure.is_none(), "{:?}", result);
    job.take_cleanup()
        .unwrap()
        .run(
            Instant::now() + Duration::from_secs(2),
            &AtomicBool::new(false),
        )
        .unwrap();
    result
}
fn qualification() {
    let f = Fixture::new();
    let result = run(
        &f.0,
        "printf stdout; printf stderr >&2; read value || printf eof; exit 7",
        Duration::from_secs(5),
        false,
    );
    assert_eq!(result.exit_code, Some(7), "{result:?}");
    assert_eq!(result.stdout, "stdouteof");
    assert_eq!(result.stderr, "stderr");
    let result = run(
        &f.0,
        "printf '%s|%s|%s' \"$HOME\" \"$TMPDIR\" \"${ASURA_SHELL_TEST_SECRET-unset}\"; printf ok > allowed",
        Duration::from_secs(5),
        false,
    );
    assert_eq!(result.exit_code, Some(0));
    assert!(result.stdout.contains("/.asura/tmp/shell-"));
    assert!(result.stdout.ends_with("|unset"));
    assert_eq!(fs::read(f.0.join("project/allowed")).unwrap(), b"ok");
    let outside = f.0.join("forbidden");
    let command = format!("printf forbidden > '{}'; exit $?", outside.display());
    let result = run(&f.0, &command, Duration::from_secs(5), false);
    assert_ne!(result.exit_code, Some(0));
    assert!(!outside.exists());
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let address = listener.local_addr().unwrap();
    let probe = std::net::TcpStream::connect(address).unwrap();
    let accept_deadline = Instant::now() + Duration::from_secs(1);
    loop {
        match listener.accept() {
            Ok(_) => break,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(
                    Instant::now() < accept_deadline,
                    "baseline listener was unreachable"
                );
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(error) => panic!("baseline accept: {error}"),
        }
    }
    drop(probe);
    let result = run(
        &f.0,
        &format!("/usr/bin/curl --max-time 1 --silent --show-error http://{address}"),
        Duration::from_secs(5),
        false,
    );
    assert_ne!(result.exit_code, Some(0));
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock,
        "sandboxed child reached loopback listener"
    );
    let result = run(
        &f.0,
        "printf '%s' $$ > timeout-pid; sleep 30 & wait",
        Duration::from_millis(600),
        false,
    );
    assert_eq!(result.reason, Some(StopReason::Deadline));
    group_absent(&f.0.join("project/timeout-pid"));
    let result = run(
        &f.0,
        "printf '%s' $$ > cancel-pid; sleep 30 & wait",
        Duration::from_secs(5),
        true,
    );
    assert_eq!(result.reason, Some(StopReason::Cancelled));
    group_absent(&f.0.join("project/cancel-pid"));
    let result = run(
        &f.0,
        r#"for fd in 3 4 5 6 7 8 9; do if eval 'true <&'"$fd" 2>/dev/null; then exit 99; fi; done; exit 0"#,
        Duration::from_secs(5),
        false,
    );
    assert_eq!(result.exit_code, Some(0));
    let result = run(
        &f.0,
        "trap '' TERM; printf '%s' $$ > resistant-pid; while :; do sleep 1; done",
        Duration::from_millis(600),
        false,
    );
    assert_eq!(result.reason, Some(StopReason::Deadline));
    assert_eq!(result.signal, Some(9));
    group_absent(&f.0.join("project/resistant-pid"));
    let result = run(
        &f.0,
        "printf '%s' $$ > grandchildren-pid; sleep 30 & exit 0",
        Duration::from_secs(5),
        false,
    );
    assert_eq!(result.exit_code, Some(0));
    group_absent(&f.0.join("project/grandchildren-pid"));
    let result = run(&f.0, "/usr/bin/yes output", Duration::from_secs(15), false);
    assert_eq!(result.reason, Some(StopReason::OutputLimit));
    assert!(result.truncated);
    assert!(result.render_text().len() <= 16384);
    fs::copy(
        std::env::current_exe().unwrap(),
        f.0.join("project/escape-helper"),
    )
    .unwrap();
    let result = run(
        &f.0,
        "./escape-helper --escape & sleep 0.1; exit 0",
        Duration::from_secs(5),
        false,
    );
    assert_eq!(result.exit_code, Some(0));
    let escaped = f.0.join("project/escaped-pid");
    let pid = fs::read_to_string(&escaped)
        .unwrap()
        .parse::<i32>()
        .unwrap();
    let until = Instant::now() + Duration::from_secs(3);
    while unsafe { libc::kill(pid, 0) } == 0 {
        if Instant::now() >= until {
            CLEAN.store(false, Ordering::Release);
            panic!("test-owned escaped child exceeded fixed lifetime")
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    // Fresh service-like process dies without dropping Job. Lease EOF must stop its group.
    let child = std::process::Command::new(std::env::current_exe().unwrap())
        .arg("--lease-holder")
        .arg(&f.0)
        .spawn()
        .unwrap();
    let mut holder = LeaseGuard {
        child,
        home: f.0.clone(),
    };
    assert!(holder.child.wait().unwrap().success());
    let pidfile = f.0.join("project/lease-pid");
    let until = Instant::now() + Duration::from_secs(4);
    loop {
        if let Ok(s) = fs::read_to_string(&pidfile) {
            let pid = s.parse::<i32>().unwrap();
            if !group_exists(pid) {
                break;
            }
        }
        assert!(
            Instant::now() < until,
            "lease EOF failed to settle owned group"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}
fn lease_holder(home: &Path) -> ! {
    let mut job = prepare(
        home,
        "printf '%s' $$ > lease-pid; sleep 30 & wait",
        Duration::from_secs(5),
    );
    fs::write(
        home.join("guardian-pid"),
        job.guardian_pid_for_test().to_string(),
    )
    .unwrap();
    let until = Instant::now() + Duration::from_secs(2);
    while !home.join("project/lease-pid").exists() {
        assert!(Instant::now() < until);
        job.poll(Instant::now()).unwrap();
        std::thread::sleep(Duration::from_millis(10));
    }
    // Exit bypasses Rust drops, as a service process crash would.
    std::process::exit(0)
}
fn group_exists(pid: i32) -> bool {
    assert!(pid > 1); // SAFETY: signal zero is a read-only existence probe, never cleanup by stale PID.
    unsafe { libc::kill(-pid, 0) == 0 }
}
fn group_absent(path: &Path) {
    let pid = fs::read_to_string(path).unwrap().parse::<i32>().unwrap();
    assert!(!group_exists(pid), "owned process group remains");
}

struct JobGuard(asura_platform::shell::ShellJob);
impl std::ops::Deref for JobGuard {
    type Target = asura_platform::shell::ShellJob;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
impl std::ops::DerefMut for JobGuard {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}
impl Drop for JobGuard {
    fn drop(&mut self) {
        if !self.0.settled() {
            self.0.stop(StopReason::Cancelled);
            let until = Instant::now() + Duration::from_secs(7);
            while !self.0.settled() && Instant::now() < until {
                let _ = asura_platform::poll(&self.0.interests(), Duration::from_millis(20));
                let _ = self.0.poll(Instant::now());
            }
        }
        if self.0.settled() {
            if let Some(mut cleanup) = self.0.take_cleanup()
                && cleanup
                    .run(
                        Instant::now() + Duration::from_secs(2),
                        &AtomicBool::new(false),
                    )
                    .is_err()
            {
                CLEAN.store(false, Ordering::Release);
            }
        } else {
            CLEAN.store(false, Ordering::Release);
        }
    }
}

struct LeaseGuard {
    child: std::process::Child,
    home: PathBuf,
}
impl Drop for LeaseGuard {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
        let until = Instant::now() + Duration::from_secs(7);
        while Instant::now() < until {
            let ids = [
                self.home.join("guardian-pid"),
                self.home.join("project/lease-pid"),
            ];
            let mut absent = true;
            for path in ids {
                match fs::read_to_string(path)
                    .ok()
                    .and_then(|s| s.parse::<i32>().ok())
                {
                    Some(pid) => {
                        if unsafe { libc::kill(pid, 0) } == 0 {
                            absent = false;
                        }
                    }
                    None => absent = false,
                }
            }
            if absent {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        CLEAN.store(false, Ordering::Release);
    }
}
fn escape_child() {
    // This test-owned child has an independent hard lifetime; no cleanup by stale PID.
    unsafe {
        assert_eq!(
            libc::setsid(),
            std::process::id() as i32,
            "escape fixture did not create a session"
        );
        assert_eq!(libc::getpgrp(), std::process::id() as i32);
        libc::close(0);
        libc::close(1);
        libc::close(2);
    }
    fs::write("escaped-pid", std::process::id().to_string()).unwrap();
    std::thread::sleep(Duration::from_millis(800));
}
