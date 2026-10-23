//! Native process qualification; private guardian dispatch is identical to the CLI.
use asura_platform::exec_cmd::{
    ExecCmdError, ExecCmdRequest, ExecCmdResult, Exit, OutputSink, PersonSetup, PreparedExecCmd,
    Profile, StopKind, StopReason, Stream, run_guardian,
};
use asura_platform::secrets::{RefusalCode, SecretVariable, WHOLE_COMMAND, rewrite_command};
use asura_platform::{ProjectIdentity, RuntimeDirectory};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
static CLEAN: AtomicBool = AtomicBool::new(true);
use std::time::{Duration, Instant};
#[path = "support/escapees.rs"]
mod escapees;
fn main() {
    let args: Vec<_> = std::env::args().collect();
    if let [_, flag, id, profile, command] = args.as_slice()
        && flag == "--asura-exec-cmd-guardian"
    {
        std::process::exit(run_guardian(id, profile, command));
    }
    if let [_, flag, id, profile, command, fault] = args.as_slice()
        && flag == "--asura-exec-cmd-guardian"
    {
        std::process::exit(asura_platform::exec_cmd::run_guardian_for_test(
            id, profile, command, fault,
        ));
    }
    if let [_, flag, home] = args.as_slice()
        && flag == "--lease-holder"
    {
        lease_holder(Path::new(home));
    }
    if let [_, flag, pid] = args.as_slice()
        && flag == "--procargs"
    {
        procargs_probe(pid.parse().unwrap());
        return;
    }
    if args.get(1).is_some_and(|s| s == "--escape") {
        escape_child();
        return;
    }
    if let [_, flag, tag, mode] = args.as_slice()
        && flag == "--survivor"
    {
        survivor(tag, mode);
        return;
    }
    if args.get(1).is_some_and(|s| s == "--noop") {
        return;
    }
    qualification();
    println!("exec_cmd process qualification passed");
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
/// Raw output the test keeps: the first 1 MiB of each stream and counts.
/// The platform keeps no ring (J1); this test sink stands in for the
/// service's capture worker.
#[derive(Default)]
struct Bytes {
    kept: [Vec<u8>; 2],
    total: [u64; 2],
    ended: [bool; 2],
    discarded: [u64; 2],
}
#[derive(Clone, Default)]
struct Collected(Arc<Mutex<Bytes>>);
const KEEP: usize = 1024 * 1024;
fn slot(stream: Stream) -> usize {
    match stream {
        Stream::Stdout => 0,
        Stream::Stderr => 1,
    }
}
impl OutputSink for Collected {
    fn room(&self) -> usize {
        KEEP
    }
    fn accept(&mut self, stream: Stream, bytes: &[u8]) {
        let mut b = self.0.lock().unwrap();
        let i = slot(stream);
        b.total[i] += bytes.len() as u64;
        let space = KEEP.saturating_sub(b.kept[i].len());
        b.kept[i].extend_from_slice(&bytes[..bytes.len().min(space)]);
    }
    fn discard(&mut self, stream: Stream, bytes: u64) {
        self.0.lock().unwrap().discarded[slot(stream)] += bytes;
    }
    fn end(&mut self, stream: Stream) {
        self.0.lock().unwrap().ended[slot(stream)] = true;
    }
}
/// A settled job's facts plus the test sink's output.
struct Outcome {
    result: ExecCmdResult,
    stdout: String,
    stderr: String,
    truncated: bool,
    total: [u64; 2],
}
impl std::ops::Deref for Outcome {
    type Target = ExecCmdResult;
    fn deref(&self) -> &ExecCmdResult {
        &self.result
    }
}
impl std::fmt::Debug for Outcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{:?} stdout={:?} stderr={:?}",
            self.result, self.stdout, self.stderr
        )
    }
}
impl Outcome {
    /// Everything the job reported: its result facts and both captured
    /// streams.
    fn render_text(&self) -> String {
        format!("{self:?}")
    }
}
fn prepare(home: &Path, command: &str, timeout: Duration) -> JobGuard {
    prepare_with(home, command, Vec::new(), timeout)
}
fn prepare_with(
    home: &Path,
    command: &str,
    secrets: Vec<SecretVariable>,
    timeout: Duration,
) -> JobGuard {
    let _ = timeout;
    prepare_request(
        home,
        ExecCmdRequest {
            job_id: asura_platform::random_id(),
            profile: Profile::Model1,
            command: command.into(),
            cwd: None,
            secrets,
            secret_command: None,
        },
    )
}
fn prepare_request(home: &Path, request: ExecCmdRequest) -> JobGuard {
    let runtime = RuntimeDirectory::scratch(home, true).unwrap();
    let project = ProjectIdentity::open(
        home.join("project").to_str().unwrap(),
        &runtime.state_boundary(),
    )
    .unwrap();
    let cancel = AtomicBool::new(false);
    let p = PreparedExecCmd::prepare(runtime, &project, request, &cancel)
        .unwrap_or_else(|e| panic!("prepare: {:?}", e.reason));
    let collected = Collected::default();
    JobGuard(
        p.spawn(
            &std::env::current_exe().unwrap(),
            &cancel,
            Box::new(collected.clone()),
        )
        .unwrap_or_else(|e| panic!("spawn: {:?}", e.reason)),
        collected,
    )
}
fn run(home: &Path, command: &str, timeout: Duration, cancel: bool) -> Outcome {
    settle(prepare(home, command, timeout), timeout, cancel)
}
fn settle(mut job: JobGuard, timeout: Duration, cancel: bool) -> Outcome {
    let started = Instant::now();
    let end = started + timeout + Duration::from_secs(4);
    let mut cancelled = false;
    while !job.settled() {
        assert!(Instant::now() < end, "exec_cmd failed to settle");
        if cancel && !cancelled && started.elapsed() > Duration::from_millis(200) {
            job.stop(StopKind::Term);
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
    let bytes = job.1.0.lock().unwrap();
    assert!(bytes.ended[0] && bytes.ended[1], "both streams reached EOF");
    let text = |i: usize| String::from_utf8_lossy(&bytes.kept[i]).into_owned();
    Outcome {
        stdout: text(0),
        stderr: text(1),
        truncated: bytes.total[0] > KEEP as u64 || bytes.total[1] > KEEP as u64,
        total: bytes.total,
        result,
    }
}
fn qualification() {
    let f = Fixture::new();
    // J1T-05 (platform): with no deadline a command that runs 65 s, past the
    // removed 60 s limit, completes with its exit code (J1-Q3). It runs
    // beside the other cases.
    let long_home = f.0.clone();
    let long = std::thread::spawn(move || {
        let started = Instant::now();
        let result = run(
            &long_home,
            "sleep 65; exit 3",
            Duration::from_secs(70),
            false,
        );
        (result.exit_code, result.reason, started.elapsed())
    });
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
        "printf '%s|%s|%s' \"$HOME\" \"$TMPDIR\" \"${ASURA_EXEC_CMD_TEST_SECRET-unset}\"; printf ok > allowed",
        Duration::from_secs(5),
        false,
    );
    assert_eq!(result.exit_code, Some(0));
    assert!(result.stdout.contains("/.asura/tmp/exec-cmd-"));
    assert!(result.stdout.ends_with("|unset"));
    assert_eq!(fs::read(f.0.join("project/allowed")).unwrap(), b"ok");
    let outside = f.0.join("forbidden");
    let command = format!("printf forbidden > '{}'; exit $?", outside.display());
    let result = run(&f.0, &command, Duration::from_secs(5), false);
    assert_ne!(result.exit_code, Some(0));
    assert!(!outside.exists());
    state_isolation(&f.0);
    escapees::escapees(&f.0);
    guardian_refusals(&f.0);
    j2t_05_lease_bytes(&f.0);
    j2t_06_leader_exit_notice(&f.0);
    secret_environment_refusals(&f.0);
    secret_variables(&f.0);
    person_profiles(&f.0);
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
        "printf '%s' $$ > cancel-pid; sleep 30 & wait",
        Duration::from_secs(5),
        true,
    );
    assert_eq!(result.reason, Some(StopReason::Cancelled));
    group_absent(&f.0.join("project/cancel-pid"));
    let result = run(
        &f.0,
        // sh saves redirected descriptors at FD 10, so FD 10 is probed with dup.
        r#"for fd in 3 4 5 6 7 8 9; do if eval 'true <&'"$fd" 2>/dev/null; then exit 99; fi; done; /usr/bin/perl -MPOSIX -e 'for (3..10) { exit 98 if defined POSIX::dup($_) } exit 0'"#,
        Duration::from_secs(5),
        false,
    );
    assert_eq!(result.exit_code, Some(0));
    // A TERM-resistant group ends on cancel: SIGTERM, then SIGKILL.
    let result = run(
        &f.0,
        "trap '' TERM; printf '%s' $$ > resistant-pid; while :; do sleep 1; done",
        Duration::from_secs(5),
        true,
    );
    assert_eq!(result.reason, Some(StopReason::Cancelled));
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
    // J1T-07 (platform): no 64 MiB combined stop; 70,000,000 bytes pass
    // through the sink and the command exits normally (J1-Q2).
    let result = run(
        &f.0,
        "/usr/bin/yes 0123456789 | /usr/bin/head -c 70000000",
        Duration::from_secs(60),
        false,
    );
    assert_eq!(result.exit_code, Some(0), "{result:?}");
    assert_eq!(result.reason, None);
    assert_eq!(result.total[0], 70_000_000);
    println!("J1T-07 platform: 70,000,000 bytes, no output stop");
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
    let (code, reason, elapsed) = long.join().unwrap();
    assert_eq!((code, reason), (Some(3), None));
    assert!(elapsed >= Duration::from_secs(65), "{elapsed:?}");
    println!("J1T-05 platform: a 65 s command completed with exit 3 after {elapsed:?}");
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
/// SI-I6: an absent, invalid or mismatched FD 10 refuses setup; no command runs.
fn guardian_refusals(home: &Path) {
    use std::os::fd::OwnedFd;
    let other = home.join("other-state");
    fs::create_dir(&other).unwrap();
    fs::set_permissions(&other, fs::Permissions::from_mode(0o700)).unwrap();
    let open = |path: &Path| Some(OwnedFd::from(fs::File::open(path).unwrap()));
    // Descriptor faults stop the guardian before its first notice, as for FDs 3-9.
    for (label, state, expected) in [
        ("absent", None, Err(ExecCmdError::Protocol)),
        (
            "not a directory",
            open(&home.join(".asura/secret")),
            Err(ExecCmdError::Protocol),
        ),
        (
            "wrong mode",
            open(&home.join("project")),
            Err(ExecCmdError::Protocol),
        ),
        (
            "not the scratch parent",
            open(&other),
            Ok(Some(ExecCmdError::Denied)),
        ),
    ] {
        let runtime = RuntimeDirectory::scratch(home, true).unwrap();
        let project = ProjectIdentity::open(
            home.join("project").to_str().unwrap(),
            &runtime.state_boundary(),
        )
        .unwrap();
        let cancel = AtomicBool::new(false);
        let mut prepared = PreparedExecCmd::prepare(
            runtime,
            &project,
            ExecCmdRequest {
                job_id: asura_platform::random_id(),
                profile: Profile::Model1,
                command: "printf ran > refused-marker".into(),
                cwd: None,
                secrets: Vec::new(),
                secret_command: None,
            },
            &cancel,
        )
        .unwrap_or_else(|e| panic!("prepare: {:?}", e.reason));
        prepared.replace_state_for_test(state);
        let collected = Collected::default();
        let mut job = JobGuard(
            prepared
                .spawn(
                    &std::env::current_exe().unwrap(),
                    &cancel,
                    Box::new(collected.clone()),
                )
                .unwrap_or_else(|e| panic!("spawn: {:?}", e.reason)),
            collected,
        );
        let end = Instant::now() + Duration::from_secs(5);
        let outcome = loop {
            if job.settled() {
                break Ok(job.result().unwrap());
            }
            assert!(Instant::now() < end, "{label}: refusal failed to settle");
            asura_platform::poll(&job.interests(), Duration::from_millis(20)).unwrap();
            if let Err(error) = job.poll(Instant::now()) {
                break Err(error);
            }
        };
        assert_eq!(outcome.map(|result| result.failure), expected, "{label}");
        assert!(!home.join("project/refused-marker").exists(), "{label}");
        if let Some(mut cleanup) = job.take_cleanup() {
            cleanup
                .run(
                    Instant::now() + Duration::from_secs(2),
                    &AtomicBool::new(false),
                )
                .unwrap();
        }
    }
}
/// Pump a job until `done` or settlement; the limit bounds only this test.
fn pump_job(job: &mut JobGuard, what: &str, mut done: impl FnMut(&JobGuard) -> bool) {
    let until = Instant::now() + Duration::from_secs(15);
    while !done(job) && !job.settled() {
        assert!(Instant::now() < until, "{what}");
        asura_platform::poll(&job.interests(), Duration::from_millis(20)).unwrap();
        job.poll(Instant::now()).unwrap();
    }
}
/// Settle a stopped job, run its scratch cleanup and return its result.
fn finish_stopped(mut job: JobGuard, what: &str) -> ExecCmdResult {
    pump_job(&mut job, what, |_| false);
    let result = job.result().unwrap();
    job.take_cleanup()
        .unwrap()
        .run(
            Instant::now() + Duration::from_secs(2),
            &AtomicBool::new(false),
        )
        .unwrap();
    result
}
/// J2T-05 integration with the real guardian under `sandbox-exec`: an
/// unknown profile token refuses before any effect and any notice; `T`
/// stops a TERM-ignoring group with KILL after the 500 ms grace, reason 2;
/// `K` kills at once, reason 11; `K` escalates `T`; lease EOF still stops
/// the group, reason 4.
fn j2t_05_lease_bytes(home: &Path) {
    // Unknown token: status 1 before any effect; the service sees status
    // EOF with no notice, a protocol fault, and no command ran.
    let runtime = RuntimeDirectory::scratch(home, true).unwrap();
    let project = ProjectIdentity::open(
        home.join("project").to_str().unwrap(),
        &runtime.state_boundary(),
    )
    .unwrap();
    let cancel = AtomicBool::new(false);
    let id = asura_platform::random_id();
    let mut prepared = PreparedExecCmd::prepare(
        runtime,
        &project,
        ExecCmdRequest {
            job_id: id,
            profile: Profile::Model1,
            command: "printf ran > unknown-profile-marker".into(),
            cwd: None,
            secrets: Vec::new(),
            secret_command: None,
        },
        &cancel,
    )
    .unwrap_or_else(|e| panic!("prepare: {:?}", e.reason));
    prepared.profile_token_for_test("model-2");
    let mut job = prepared
        .spawn(
            &std::env::current_exe().unwrap(),
            &cancel,
            Box::new(Collected::default()),
        )
        .unwrap_or_else(|e| panic!("spawn: {:?}", e.reason));
    let until = Instant::now() + Duration::from_secs(5);
    let outcome = loop {
        assert!(Instant::now() < until, "unknown profile failed to end");
        asura_platform::poll(&job.interests(), Duration::from_millis(20)).unwrap();
        if let Err(error) = job.poll(Instant::now()) {
            break error;
        }
    };
    assert_eq!(outcome, ExecCmdError::Protocol);
    assert!(!job.spawned() && job.faulted() && !job.settled());
    assert!(!home.join("project/unknown-profile-marker").exists());
    let hex: String = id.iter().map(|b| format!("{b:02x}")).collect();
    let scratch = home.join(format!(".asura/tmp/exec-cmd-{hex}"));
    assert_eq!(
        fs::read_dir(&scratch).unwrap().count(),
        0,
        "the guardian touched scratch"
    );
    // J2T-25 (J2-A2): once the guardian is reaped and both capture pipes
    // end with no notice, the platform releases the scratch token; the
    // cleanup deletes the scratch.
    let until = Instant::now() + Duration::from_secs(5);
    let mut token = loop {
        if let Some(token) = job.take_cleanup() {
            break token;
        }
        assert!(job.release_pending(), "the token can still be released");
        assert!(Instant::now() < until, "pre-notice token not released");
        asura_platform::poll(&job.interests(), Duration::from_millis(20)).unwrap();
        assert_eq!(job.poll(Instant::now()), Err(ExecCmdError::Protocol));
    };
    assert!(!job.release_pending() && !job.settled() && job.result().is_none());
    token
        .run(
            Instant::now() + Duration::from_secs(2),
            &AtomicBool::new(false),
        )
        .unwrap();
    assert!(!scratch.exists(), "the released token deleted the scratch");
    drop(job);
    println!(
        "J2T-05/J2T-25: unknown profile token refused before any effect or notice; scratch released"
    );

    let resistant = "trap '' TERM; printf '%s' $$ > j2-pid; while :; do sleep 0.05; done";
    // `T`: TERM is ignored, KILL follows the 500 ms grace; reason 2.
    let mut job = prepare(home, resistant, Duration::from_secs(5));
    pump_job(&mut job, "T job running", |_| {
        home.join("project/j2-pid").exists()
    });
    let stopped = Instant::now();
    job.stop(StopKind::Term);
    job.stop(StopKind::Term);
    let result = finish_stopped(job, "T stop settles");
    let elapsed = stopped.elapsed();
    assert_eq!(result.reason, Some(StopReason::Cancelled), "{result:?}");
    assert_eq!(result.signal, Some(9), "{result:?}");
    assert!(result.cleanup_confirmed);
    assert!(elapsed >= Duration::from_millis(500), "{elapsed:?}");
    group_absent(&home.join("project/j2-pid"));
    fs::remove_file(home.join("project/j2-pid")).unwrap();
    println!("J2T-05: T gave KILL after {elapsed:?}, reason 2");

    // `K`: KILL at once; reason 11.
    let mut job = prepare(home, resistant, Duration::from_secs(5));
    pump_job(&mut job, "K job running", |_| {
        home.join("project/j2-pid").exists()
    });
    let stopped = Instant::now();
    job.stop(StopKind::Kill);
    let result = finish_stopped(job, "K stop settles");
    let elapsed = stopped.elapsed();
    assert_eq!(result.reason, Some(StopReason::Killed), "{result:?}");
    assert_eq!(result.signal, Some(9), "{result:?}");
    assert!(elapsed < Duration::from_millis(500), "{elapsed:?}");
    group_absent(&home.join("project/j2-pid"));
    fs::remove_file(home.join("project/j2-pid")).unwrap();
    println!("J2T-05: K gave KILL after {elapsed:?}, reason 11");

    // `K` escalates `T` inside the grace: reason 11 replaces 2.
    let mut job = prepare(home, resistant, Duration::from_secs(5));
    pump_job(&mut job, "T then K job running", |_| {
        home.join("project/j2-pid").exists()
    });
    let stopped = Instant::now();
    job.stop(StopKind::Term);
    pump_job(&mut job, "T read", |_| {
        stopped.elapsed() >= Duration::from_millis(150)
    });
    job.stop(StopKind::Kill);
    let result = finish_stopped(job, "T then K settles");
    let elapsed = stopped.elapsed();
    assert_eq!(result.reason, Some(StopReason::Killed), "{result:?}");
    assert_eq!(result.signal, Some(9), "{result:?}");
    assert!(elapsed < Duration::from_millis(500), "{elapsed:?}");
    group_absent(&home.join("project/j2-pid"));
    fs::remove_file(home.join("project/j2-pid")).unwrap();
    println!("J2T-05: K escalated T after {elapsed:?}, reason 11");

    // Lease EOF with no byte still stops the group; reason 4.
    let mut job = prepare(
        home,
        "printf '%s' $$ > j2-pid; sleep 30 & wait",
        Duration::from_secs(5),
    );
    pump_job(&mut job, "EOF job running", |_| {
        home.join("project/j2-pid").exists()
    });
    job.close_lease();
    let result = finish_stopped(job, "EOF stop settles");
    assert_eq!(result.reason, Some(StopReason::LeaseEnded), "{result:?}");
    assert_eq!(result.signal, Some(15), "{result:?}");
    group_absent(&home.join("project/j2-pid"));
    fs::remove_file(home.join("project/j2-pid")).unwrap();
    println!("J2T-05: lease EOF stopped the group, reason 4");
}
/// J2T-06 integration: the leader exits while a TERM-ignoring member
/// writes. Kind 5 arrives first, output keeps flowing, and kind 2 later
/// repeats its exit facts; a signalled leader reports its signal.
fn j2t_06_leader_exit_notice(home: &Path) {
    let mut job = prepare(
        home,
        "(trap '' TERM; while :; do printf x; sleep 0.02; done) & sleep 0.2; exit 3",
        Duration::from_secs(5),
    );
    pump_job(&mut job, "kind 5", |job| job.leader_exit().is_some());
    assert!(!job.settled(), "kind 5 precedes kind 2");
    assert_eq!(job.leader_exit(), Some(Exit::Exited(3)));
    let at_exit = job.1.0.lock().unwrap().total[0];
    pump_job(&mut job, "member output after kind 5", |job| {
        job.1.0.lock().unwrap().total[0] > at_exit
    });
    assert!(!job.settled(), "the member still writes after kind 5");
    let result = finish_stopped(job, "leader-exit settlement");
    assert_eq!(result.exit_code, Some(3), "{result:?}");
    assert_eq!((result.signal, result.reason), (None, None), "{result:?}");
    assert!(result.cleanup_confirmed);
    println!("J2T-06: kind 5 before kind 2; output flowed after the leader exit");

    let mut job = prepare(home, "kill -KILL $$", Duration::from_secs(5));
    pump_job(&mut job, "signalled kind 5", |job| {
        job.leader_exit().is_some()
    });
    assert_eq!(job.leader_exit(), Some(Exit::Signalled(9)));
    let result = finish_stopped(job, "signalled settlement");
    assert_eq!((result.exit_code, result.signal), (None, Some(9)));
    println!("J2T-06: a signalled leader reported signal 9 in kinds 5 and 2");
}
/// SI-I4 and SI-I5: the Asura home is denied under both spellings; scratch works.
fn state_isolation(home: &Path) {
    let asura = home.join(".asura");
    fs::write(asura.join("secret"), b"state").unwrap();
    fs::copy("/bin/echo", asura.join("tool")).unwrap();
    let firmlinked = Path::new("/System/Volumes/Data").join(asura.strip_prefix("/").unwrap());
    assert_eq!(fs::read(firmlinked.join("secret")).unwrap(), b"state");
    for root in [&asura, &firmlinked] {
        let root = root.display();
        for (label, command) in [
            ("read", format!("/bin/cat '{root}/secret' >/dev/null")),
            ("list", format!("/bin/ls '{root}/run' >/dev/null")),
            ("write", format!("printf x > '{root}/written'")),
            ("exec", format!("'{root}/tool' reached")),
        ] {
            let result = run(
                home,
                &format!("{command}; exit $?"),
                Duration::from_secs(5),
                false,
            );
            assert_ne!(
                result.exit_code,
                Some(0),
                "{label} under {root}: {result:?}"
            );
            assert!(!result.stdout.contains("reached"), "{label} under {root}");
        }
    }
    assert!(!asura.join("written").exists());
    let result = run(
        home,
        "printf '#!/bin/sh\\nprintf scratch-ok\\n' > \"$TMPDIR/tool\" && /bin/chmod 700 \"$TMPDIR/tool\" && \"$TMPDIR/tool\" && /bin/cat \"$TMPDIR/tool\" >/dev/null",
        Duration::from_secs(5),
        false,
    );
    assert_eq!(result.exit_code, Some(0), "{result:?}");
    assert_eq!(result.stdout, "scratch-ok");
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

struct JobGuard(asura_platform::exec_cmd::ExecCmdJob, Collected);
impl std::ops::Deref for JobGuard {
    type Target = asura_platform::exec_cmd::ExecCmdJob;
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
            self.0.stop(StopKind::Term);
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

/// SDU-13 through the real guardian: an environment that bypasses the service
/// check is refused with the setup failure notice, and no command runs.
fn secret_environment_refusals(home: &Path) {
    let mut big = b"ASURA_SECRET_1=".to_vec();
    big.extend_from_slice(&[b'v'; 8193]);
    let mut half = b"ASURA_SECRET_2=".to_vec();
    half.extend_from_slice(&[b'v'; 4097]);
    let mut other_half = b"ASURA_SECRET_3=".to_vec();
    other_half.extend_from_slice(&[b'v'; 4096]);
    // A NUL cannot cross execve: spawn refuses it before any process starts.
    for (label, entries) in [
        ("other name", vec![b"OTHER=value".to_vec()]),
        ("number 0", vec![b"ASURA_SECRET_0=value".to_vec()]),
        ("number 17", vec![b"ASURA_SECRET_17=value".to_vec()]),
        (
            "duplicate",
            vec![
                b"ASURA_SECRET_1=value".to_vec(),
                b"ASURA_SECRET_1=value".to_vec(),
            ],
        ),
        ("over 8192 bytes", vec![big.clone()]),
        ("over 8192 in total", vec![half.clone(), other_half.clone()]),
        ("NUL", vec![b"ASURA_SECRET_1=va\0lue".to_vec()]),
        // SDU-17 through a real guardian: the whole-command variable only
        // alone, non-empty and with the fixed `eval` command.
        (
            "whole command beside numbered",
            vec![
                b"ASURA_SECRET_COMMAND=printf ran > secret-refused-marker".to_vec(),
                b"ASURA_SECRET_1=value".to_vec(),
            ],
        ),
        (
            "whole command with another command",
            vec![b"ASURA_SECRET_COMMAND=printf ran > secret-refused-marker".to_vec()],
        ),
        (
            "whole command empty",
            vec![b"ASURA_SECRET_COMMAND=".to_vec()],
        ),
    ] {
        let runtime = RuntimeDirectory::scratch(home, true).unwrap();
        let project = ProjectIdentity::open(
            home.join("project").to_str().unwrap(),
            &runtime.state_boundary(),
        )
        .unwrap();
        let cancel = AtomicBool::new(false);
        let mut prepared = PreparedExecCmd::prepare(
            runtime,
            &project,
            ExecCmdRequest {
                job_id: asura_platform::random_id(),
                profile: Profile::Model1,
                command: "printf ran > secret-refused-marker".into(),
                cwd: None,
                secrets: Vec::new(),
                secret_command: None,
            },
            &cancel,
        )
        .unwrap_or_else(|e| panic!("prepare: {:?}", e.reason));
        prepared.replace_secret_environment_for_test(entries);
        let collected = Collected::default();
        let job = match prepared.spawn(
            &std::env::current_exe().unwrap(),
            &cancel,
            Box::new(collected.clone()),
        ) {
            Ok(job) => JobGuard(job, collected),
            Err(mut failure) => {
                assert_eq!(label, "NUL");
                assert_eq!(failure.reason, ExecCmdError::Invalid, "{label}");
                failure
                    .cleanup
                    .as_mut()
                    .unwrap()
                    .run(
                        Instant::now() + Duration::from_secs(2),
                        &AtomicBool::new(false),
                    )
                    .unwrap();
                continue;
            }
        };
        let mut job = job;
        let end = Instant::now() + Duration::from_secs(5);
        while !job.settled() {
            assert!(Instant::now() < end, "{label}: refusal failed to settle");
            asura_platform::poll(&job.interests(), Duration::from_millis(20)).unwrap();
            job.poll(Instant::now()).unwrap();
        }
        let result = job.result().unwrap();
        assert_eq!(result.failure, Some(ExecCmdError::Unavailable), "{label}");
        assert_eq!(result.exit_code, None, "{label}");
        assert!(
            !home.join("project/secret-refused-marker").exists(),
            "{label}"
        );
        job.take_cleanup()
            .unwrap()
            .run(
                Instant::now() + Duration::from_secs(2),
                &AtomicBool::new(false),
            )
            .unwrap();
    }
}

/// A fixture secret assembled at run time, so no token is committed.
fn fixture_token() -> String {
    ["ghp_", &"aB3".repeat(12)].concat()
}
fn rewritten(command: &str) -> (String, Vec<SecretVariable>) {
    let rewrite = rewrite_command(command)
        .unwrap_or_else(|r| panic!("{r}"))
        .rewritten()
        .expect("a rewrite");
    assert!(!rewrite.command.contains(&fixture_token()));
    assert_eq!(rewrite.variables.len(), 1);
    (rewrite.command, rewrite.variables)
}
fn holds(bytes: &[u8], needle: &str) -> bool {
    bytes.windows(needle.len()).any(|w| w == needle.as_bytes())
}
/// SDI-06, SDI-07, SDI-09 and the argument-vector check.
fn secret_variables(home: &Path) {
    let token = fixture_token();
    // SDI-06: the rewritten command writes the real value to a project file.
    let (command, secrets) = rewritten(&format!("printf '%s' \"{token}\" > out.txt"));
    assert_eq!(command, "printf '%s' \"${ASURA_SECRET_1}\" > out.txt");
    let result = settle(
        prepare_with(home, &command, secrets, Duration::from_secs(5)),
        Duration::from_secs(5),
        false,
    );
    assert_eq!(result.exit_code, Some(0), "{result:?}");
    assert_eq!(
        fs::read(home.join("project/out.txt")).unwrap(),
        token.as_bytes()
    );
    assert!(!result.render_text().contains(&token));
    fs::remove_file(home.join("project/out.txt")).unwrap();
    println!("SDI-06: out.txt held the value; captured output held no value");

    // SL7 (SDI-12 platform part): a single-quoted value runs through the
    // whole-command variable; `eval` runs the raw text with the real value
    // and no child sees the variable.
    let raw = format!(
        "printf '%s' '{token}' > whole.txt; /usr/bin/printenv; \
         /bin/sh -c 'printf \"child=%s\\n\" \"${{ASURA_SECRET_COMMAND-unset}}\"'"
    );
    let whole = rewrite_command(&raw)
        .unwrap_or_else(|r| panic!("{r}"))
        .whole_command()
        .expect("the whole-command variable");
    assert_eq!(whole.reason.code, RefusalCode::SecretSingleQuoted);
    assert!(!whole.display.contains(&token));
    let result = settle(
        prepare_request(
            home,
            ExecCmdRequest {
                job_id: asura_platform::random_id(),
                profile: Profile::Model1,
                command: WHOLE_COMMAND.into(),
                cwd: None,
                secrets: Vec::new(),
                secret_command: Some(whole.command),
            },
        ),
        Duration::from_secs(5),
        false,
    );
    assert_eq!(result.exit_code, Some(0), "{result:?}");
    assert_eq!(
        fs::read(home.join("project/whole.txt")).unwrap(),
        token.as_bytes()
    );
    assert!(result.stdout.contains("PATH="), "{result:?}");
    assert!(!result.stdout.contains("ASURA_SECRET"), "{result:?}");
    assert!(result.stdout.contains("child=unset"), "{result:?}");
    assert!(!result.render_text().contains(&token));
    fs::remove_file(home.join("project/whole.txt")).unwrap();
    println!("SL7: eval ran the raw command with the value; no child saw the variable");

    // SDI-07: no child sees the variable; record what `set` prints.
    let (command, secrets) = rewritten(&format!("printf '%s' \"{token}\" >/dev/null"));
    let command = format!(
        "{command}; echo ---printenv; /usr/bin/printenv; echo ---env; /usr/bin/env; \
         echo ---child; /bin/sh -c 'printf \"%s\\n\" \"${{ASURA_SECRET_1-unset}}\"'; \
         echo ---set; set"
    );
    let result = settle(
        prepare_with(home, &command, secrets, Duration::from_secs(5)),
        Duration::from_secs(5),
        false,
    );
    assert_eq!(result.exit_code, Some(0), "{result:?}");
    assert!(!result.truncated, "{result:?}");
    let section = |name: &str| {
        let start = result.stdout.find(&format!("---{name}\n")).unwrap() + name.len() + 4;
        let rest = &result.stdout[start..];
        rest[..rest.find("---").unwrap_or(rest.len())].to_owned()
    };
    for name in ["printenv", "env"] {
        let text = section(name);
        assert!(text.contains("PATH="), "{name}: {text}");
        assert!(!text.contains("ASURA_SECRET"), "{name}: {text}");
        assert!(!text.contains(&token), "{name}");
    }
    assert_eq!(section("child"), "unset\n");
    let set = section("set");
    assert!(
        !set.lines().any(|line| line.starts_with("__asura_t=")),
        "the preamble's temporary is unset"
    );
    let line = set
        .lines()
        .find(|line| line.starts_with("ASURA_SECRET_1="))
        .expect("set lists the shell variable");
    let shows_value = line.contains(&token);
    println!(
        "SDI-07: printenv and env hold no secret variable; a child sees it unset; \
         set prints {} (value shown: {shows_value})",
        line.replace(&token, "<value>")
    );

    // SDI-09 and the argument vectors: job A holds a secret while job B probes it.
    let (command, secrets) = rewritten(&format!(
        "printf '%s' \"{token}\" >/dev/null; printf '%s' $$ > held-pid; \
         while [ ! -e release ]; do sleep 0.05; done"
    ));
    let held = prepare_with(home, &command, secrets, Duration::from_secs(20));
    let mut held = held;
    let guardian = held.guardian_pid_for_test();
    let pidfile = home.join("project/held-pid");
    let until = Instant::now() + Duration::from_secs(5);
    let sh = loop {
        if let Ok(text) = fs::read_to_string(&pidfile)
            && let Ok(pid) = text.parse::<i32>()
        {
            break pid;
        }
        assert!(Instant::now() < until, "held job did not start");
        asura_platform::poll(&held.interests(), Duration::from_millis(20)).unwrap();
        held.poll(Instant::now()).unwrap();
    };
    for pid in [guardian, sh] {
        let ps = std::process::Command::new("/bin/ps")
            .args(["-ww", "-o", "args=", "-p", &pid.to_string()])
            .output()
            .unwrap();
        assert!(ps.status.success());
        assert!(!holds(&ps.stdout, &token), "ps args of {pid}");
        let (argv, _) = procargs(pid).expect("same-user procargs read");
        assert!(argv.iter().all(|arg| !holds(arg, &token)), "argv of {pid}");
    }
    let sh_argv = procargs(sh).unwrap().0;
    assert!(
        sh_argv
            .iter()
            .any(|arg| arg.starts_with(b"__asura_t=$ASURA_SECRET_1; unset ASURA_SECRET_1;")),
        "sh received the preamble"
    );
    // The same user, outside any sandbox (the recorded residual risk).
    let (_, env) = procargs(sh).unwrap();
    let outside = env.iter().any(|entry| holds(entry, &token));
    let (_, guardian_env) = procargs(guardian).unwrap();
    let outside_guardian = guardian_env.iter().any(|entry| holds(entry, &token));
    let ps_e = |pid: i32| {
        let ps = std::process::Command::new("/bin/ps")
            .args(["-E", "-ww", "-o", "command=", "-p", &pid.to_string()])
            .output()
            .unwrap();
        assert!(ps.status.success(), "ps -E of {pid}");
        (
            holds(&ps.stdout, &token),
            String::from_utf8_lossy(&ps.stdout).replace(&token, "<value>"),
        )
    };
    let (sh_ps, sh_ps_text) = ps_e(sh);
    let (guardian_ps, guardian_ps_text) = ps_e(guardian);
    println!(
        "SDI-09 same user, unsandboxed: sh procargs envc={} value={outside}, ps -E value={sh_ps}; \
         guardian procargs envc={} value={outside_guardian}, ps -E value={guardian_ps}",
        env.len(),
        guardian_env.len(),
    );
    println!("SDI-09 same-user ps -E of sh: {}", sh_ps_text.trim_end());
    println!(
        "SDI-09 same-user ps -E of guardian: {}",
        guardian_ps_text.trim_end()
    );
    // Owner decision (a): the guardian erases its environment after copying.
    assert!(!outside && !sh_ps, "same-user read of sh found the value");
    assert!(
        !outside_guardian && !guardian_ps,
        "same-user read of the guardian found the value"
    );
    fs::copy(
        std::env::current_exe().unwrap(),
        home.join("project/procargs-helper"),
    )
    .unwrap();
    let probe = run(
        home,
        &format!(
            "/bin/ps -E -ww -o command= -p {sh}; echo \"ps-exit=$?\"; \
             echo ---sh; ./procargs-helper --procargs {sh}; echo \"helper-exit=$?\"; \
             echo ---guardian; ./procargs-helper --procargs {guardian}; echo \"helper-exit=$?\""
        ),
        Duration::from_secs(5),
        false,
    );
    fs::write(home.join("project/release"), b"").unwrap();
    let held = settle(held, Duration::from_secs(20), false);
    assert_eq!(held.exit_code, Some(0), "{held:?}");
    let ps_reads = probe
        .stdout
        .lines()
        .take_while(|line| !line.starts_with("ps-exit="))
        .any(|line| line.contains(&token));
    let helper_reads = |target: &str| {
        probe
            .stdout
            .split("---")
            .find(|part| part.starts_with(target))
            .unwrap()
            .lines()
            .any(|line| line.starts_with("env:") && line.contains(&token))
    };
    println!(
        "SDI-09: sandboxed job B: ps -E of sh read value: {ps_reads}; KERN_PROCARGS2 read \
         value: sh {}, guardian {}",
        helper_reads("sh\n"),
        helper_reads("guardian\n")
    );
    println!(
        "SDI-09 job B output:\n{}\nstderr:\n{}",
        probe.stdout.replace(&token, "<value>"),
        probe.stderr.replace(&token, "<value>")
    );
    assert!(!ps_reads && !helper_reads("sh\n") && !helper_reads("guardian\n"));
    assert!(!probe.render_text().contains(&token), "job B saw the value");
    for name in ["procargs-helper", "release", "held-pid"] {
        fs::remove_file(home.join("project").join(name)).unwrap();
    }
    raw_byte_sweep(home, &token);
}
/// Every file under the test home, except the expected value, is free of it.
fn raw_byte_sweep(root: &Path, token: &str) {
    for entry in fs::read_dir(root).unwrap() {
        let path = entry.unwrap().path();
        let meta = fs::symlink_metadata(&path).unwrap();
        if meta.is_dir() {
            raw_byte_sweep(&path, token);
        } else if meta.is_file() {
            assert!(
                !holds(&fs::read(&path).unwrap_or_default(), token),
                "raw value in {}",
                path.display()
            );
        }
    }
}
/// Argument vector and environment strings of one process.
type ProcArgs = (Vec<Vec<u8>>, Vec<Vec<u8>>);
/// Argument vector and environment of `pid` from KERN_PROCARGS2, or errno.
fn procargs(pid: i32) -> Result<ProcArgs, i32> {
    let mut argmax: libc::c_int = 0;
    let mut size = std::mem::size_of_val(&argmax);
    let mut mib = [libc::CTL_KERN, libc::KERN_ARGMAX];
    // SAFETY: fixed MIB and a complete output location of the stated size.
    if unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            2,
            (&mut argmax as *mut libc::c_int).cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error().raw_os_error().unwrap_or(0));
    }
    let mut buffer = vec![0u8; argmax as usize];
    let mut size = buffer.len();
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
    // SAFETY: the buffer holds `size` writable bytes; sysctl writes at most that.
    if unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            3,
            buffer.as_mut_ptr().cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error().raw_os_error().unwrap_or(0));
    }
    buffer.truncate(size);
    let argc = i32::from_ne_bytes(buffer[..4].try_into().unwrap()) as usize;
    let mut rest = &buffer[4..];
    // Executable path, then NUL padding.
    let skip = rest.iter().position(|b| *b == 0).unwrap_or(rest.len());
    rest = &rest[skip..];
    let skip = rest.iter().position(|b| *b != 0).unwrap_or(rest.len());
    rest = &rest[skip..];
    let mut strings = rest.split(|b| *b == 0);
    let argv: Vec<Vec<u8>> = strings.by_ref().take(argc).map(<[u8]>::to_vec).collect();
    let env = strings
        .take_while(|s| !s.is_empty())
        .map(<[u8]>::to_vec)
        .collect();
    Ok((argv, env))
}
/// Runs inside a sandboxed job: prints what KERN_PROCARGS2 yields for `pid`.
fn procargs_probe(pid: i32) {
    match procargs(pid) {
        Ok((argv, env)) => {
            println!("procargs: readable, argc={} envc={}", argv.len(), env.len());
            for entry in env.iter().filter(|e| e.starts_with(b"ASURA_SECRET_")) {
                println!("env:{}", String::from_utf8_lossy(entry));
            }
        }
        Err(errno) => println!("procargs: errno={errno}"),
    }
}

// ---------------------------------------------------------------------------
// P1: person profiles (person-command-mode.md, escapee-control.md).

/// A test-owned survivor for the `!!` cases. Modes: `closed` (new session,
/// output closed), `hold` (new session, keeps the capture pipes and writes
/// one line), `group` (stays in the shell's process group, keeps the pipes
/// and writes one line). It writes `<tag>.pid` in its working directory
/// and ends on `<tag>.release` or after its fixed 20 s lifetime. It never
/// signals any process.
fn survivor(tag: &str, mode: &str) {
    // SAFETY: test-owned child; setsid and descriptor changes affect only it.
    unsafe {
        if mode != "group" {
            assert!(libc::setsid() > 0, "survivor setsid");
        }
        if mode == "closed" {
            let null = libc::open(c"/dev/null".as_ptr(), libc::O_RDWR);
            for fd in 0..=2 {
                libc::dup2(null, fd);
            }
            libc::close(null);
        }
    }
    if mode != "closed" {
        use std::io::Write;
        let mut out = std::io::stdout();
        let _ = writeln!(out, "held {tag}");
        let _ = out.flush();
    }
    let pid = std::process::id().to_string();
    fs::write(format!("{tag}.pid.tmp"), pid).unwrap();
    fs::rename(format!("{tag}.pid.tmp"), format!("{tag}.pid")).unwrap();
    let end = Instant::now() + Duration::from_secs(20);
    while Instant::now() < end && !Path::new(&format!("{tag}.release")).exists() {
        std::thread::sleep(Duration::from_millis(50));
    }
}
/// A live, non-zombie process.
fn alive(pid: i32) -> bool {
    // SAFETY: plain-data output; zero is a valid bit pattern.
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as i32;
    // SAFETY: complete output buffer of the stated size; read-only query.
    let got = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            (&mut info as *mut libc::proc_bsdinfo).cast(),
            size,
        )
    };
    got == size && info.pbi_status != libc::SZOMB
}
/// End a test-owned survivor through its release file and wait for it.
fn release_survivor(project: &Path, tag: &str) {
    let pid: i32 = fs::read_to_string(project.join(format!("{tag}.pid")))
        .unwrap()
        .parse()
        .unwrap();
    fs::write(project.join(format!("{tag}.release")), b"").unwrap();
    let until = Instant::now() + Duration::from_secs(3);
    while alive(pid) {
        if Instant::now() >= until {
            CLEAN.store(false, Ordering::Release);
            panic!("{tag} survivor did not end on release");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}
/// The fixture person home: the test home itself, so the Asura home lies
/// inside it as it does for a real account.
struct PersonHome {
    home: PathBuf,
    tmpdir: PathBuf,
}
impl PersonHome {
    fn new(home: &Path) -> Self {
        let tmpdir = home.join("person-tmp");
        for dir in [
            tmpdir.clone(),
            home.join(".cargo"),
            home.join(".ssh"),
            home.join("Library/Caches"),
        ] {
            fs::create_dir_all(&dir).unwrap();
        }
        fs::set_permissions(home.join(".ssh"), fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(
            home.join(".ssh/known_hosts"),
            b"fixture-host ssh-ed25519 AAAA\n",
        )
        .unwrap();
        fs::write(home.join(".gitconfig"), b"[user]\n\tname = Fixture\n").unwrap();
        fs::write(home.join(".zshrc"), b"# fixture\n").unwrap();
        fs::write(home.join(".asura/person-secret"), b"state").unwrap();
        Self {
            home: home.to_path_buf(),
            tmpdir,
        }
    }
    fn sandboxed(&self, client: &[(Vec<u8>, Vec<u8>)]) -> PersonSetup {
        self.sandboxed_with(None, client)
    }
    fn sandboxed_with(
        &self,
        developer: Option<&Path>,
        client: &[(Vec<u8>, Vec<u8>)],
    ) -> PersonSetup {
        PersonSetup::sandboxed_for_test(
            "fixture",
            &self.home,
            &self.tmpdir,
            developer,
            b"/usr/bin:/bin:/usr/sbin:/sbin",
            client,
        )
        .unwrap()
    }
    /// A `!!` launch environment with a scratch HOME and TMPDIR.
    fn outside(&self, extra: &[(&str, &str)]) -> PersonSetup {
        let mut launch = vec![
            (b"PATH".to_vec(), b"/usr/bin:/bin:/usr/sbin:/sbin".to_vec()),
            (
                b"HOME".to_vec(),
                self.home.as_os_str().as_encoded_bytes().to_vec(),
            ),
            (
                b"TMPDIR".to_vec(),
                self.tmpdir.as_os_str().as_encoded_bytes().to_vec(),
            ),
            (b"LANG".to_vec(), b"C".to_vec()),
        ];
        launch.extend(
            extra
                .iter()
                .map(|(n, v)| (n.as_bytes().to_vec(), v.as_bytes().to_vec())),
        );
        PersonSetup::outside(&launch).unwrap()
    }
}
/// Prepare and spawn a person job with optional guardian fault and frame.
fn person_job(
    home: &Path,
    profile: Profile,
    command: &str,
    setup: PersonSetup,
    fault: Option<&str>,
    frame: Option<Vec<u8>>,
) -> JobGuard {
    person_job_with_id(
        home,
        asura_platform::random_id(),
        profile,
        command,
        setup,
        fault,
        frame,
    )
}
fn person_job_with_id(
    home: &Path,
    job_id: [u8; 16],
    profile: Profile,
    command: &str,
    setup: PersonSetup,
    fault: Option<&str>,
    frame: Option<Vec<u8>>,
) -> JobGuard {
    let runtime = RuntimeDirectory::scratch(home, true).unwrap();
    let project = ProjectIdentity::open(
        home.join("project").to_str().unwrap(),
        &runtime.state_boundary(),
    )
    .unwrap();
    let cancel = AtomicBool::new(false);
    let request = ExecCmdRequest {
        job_id,
        profile,
        command: command.into(),
        cwd: None,
        secrets: Vec::new(),
        secret_command: None,
    };
    let mut prepared = PreparedExecCmd::prepare_person(runtime, &project, request, setup, &cancel)
        .unwrap_or_else(|e| panic!("prepare: {:?}", e.reason));
    if let Some(fault) = fault {
        prepared.guardian_fault_for_test(fault);
    }
    if let Some(frame) = frame {
        prepared.replace_frame_for_test(frame);
    }
    let collected = Collected::default();
    JobGuard(
        prepared
            .spawn(
                &std::env::current_exe().unwrap(),
                &cancel,
                Box::new(collected.clone()),
            )
            .unwrap_or_else(|e| panic!("spawn: {:?}", e.reason)),
        collected,
    )
}
/// Run a person job to settlement and collect its outcome.
fn person_run(home: &Path, profile: Profile, command: &str, setup: PersonSetup) -> Outcome {
    settle(
        person_job(home, profile, command, setup, None, None),
        Duration::from_secs(20),
        false,
    )
}
fn person_profiles(home: &Path) {
    let person = PersonHome::new(home);
    pc_i2_to_i4_person_profile_files(home, &person);
    pc_i5_person_profile_network(home, &person);
    pc_i22_ssh_agent_socket(home, &person);
    pc_i29_sandbox_exec_unavailable(home, &person);
    pc_u50_guardian_refuses_bad_frames(home, &person);
    p1_person_escapee_marker(home, &person);
    p1_person_grace(home, &person);
    ec_i9_ec_i10_outside_survivors(home, &person);
    od2_outside_survivor_holds_output(home, &person);
    pc_i38_dyld_launch_variable(home, &person);
    if std::env::var_os("ASURA_TEST_NETWORK").is_some() {
        pc_i37_mach_probe_steps(home, &person);
    } else {
        println!("PC-I37 skipped: set ASURA_TEST_NETWORK=1 to reach github.com and crates.io");
    }
}
/// PC-I2, PC-I3 and PC-I4 with the real guardian and `sandbox-exec`.
fn pc_i2_to_i4_person_profile_files(home: &Path, person: &PersonHome) {
    // PC-I2: project, TMPDIR and the ~/.cargo cache are writable.
    let result = person_run(
        home,
        Profile::Person1,
        "printf a > pc-i2-project && printf b > \"$TMPDIR/pc-i2\" && printf c > \"$HOME/.cargo/pc-i2\" && printf ok",
        person.sandboxed(&[]),
    );
    assert_eq!(result.exit_code, Some(0), "{result:?}");
    assert_eq!(fs::read(home.join("project/pc-i2-project")).unwrap(), b"a");
    assert_eq!(fs::read(person.tmpdir.join("pc-i2")).unwrap(), b"b");
    assert_eq!(fs::read(home.join(".cargo/pc-i2")).unwrap(), b"c");
    println!("PC-I2: project, TMPDIR and ~/.cargo writes succeeded");
    // PC-I3: dotfiles, ~/.ssh and other home paths are not writable.
    for (label, target) in [
        ("~/.zshrc", "$HOME/.zshrc"),
        ("~/.ssh/x", "$HOME/.ssh/x"),
        ("~/other", "$HOME/other"),
        ("~/.asura", "$HOME/.asura/written"),
        ("job scratch of another path", "$HOME/.asura/tmp/other"),
    ] {
        let result = person_run(
            home,
            Profile::Person1,
            &format!("printf x > \"{target}\"; exit $?"),
            person.sandboxed(&[]),
        );
        assert_ne!(result.exit_code, Some(0), "{label}: {result:?}");
        assert!(
            result.stderr.contains("Operation not permitted"),
            "{label}: {result:?}"
        );
    }
    assert_eq!(fs::read(home.join(".zshrc")).unwrap(), b"# fixture\n");
    for path in [".ssh/x", "other", ".asura/written", ".asura/tmp/other"] {
        assert!(!home.join(path).exists(), "{path}");
    }
    println!("PC-I3: ~/.zshrc, ~/.ssh/x, ~/other and ~/.asura writes were denied by the OS");
    // PC-I4: home reads succeed; the Asura home stays unreadable.
    let result = person_run(
        home,
        Profile::Person1,
        "/bin/cat \"$HOME/.ssh/known_hosts\" \"$HOME/.gitconfig\"",
        person.sandboxed(&[]),
    );
    assert_eq!(result.exit_code, Some(0), "{result:?}");
    assert_eq!(
        result.stdout,
        "fixture-host ssh-ed25519 AAAA\n[user]\n\tname = Fixture\n"
    );
    let result = person_run(
        home,
        Profile::Person1,
        "/bin/cat \"$HOME/.asura/person-secret\"; exit $?",
        person.sandboxed(&[]),
    );
    assert_ne!(result.exit_code, Some(0), "{result:?}");
    assert!(!result.stdout.contains("state"));
    // The environment is the FD 11 set: person PATH, home, TMPDIR, fixed
    // values; nothing from the guardian or the test process.
    let result = person_run(
        home,
        Profile::Person1,
        "/usr/bin/env",
        person.sandboxed(&[(b"LANG".to_vec(), b"en_GB.UTF-8".to_vec())]),
    );
    assert_eq!(result.exit_code, Some(0), "{result:?}");
    let mut names: Vec<&str> = result
        .stdout
        .lines()
        .map(|line| line.split('=').next().unwrap())
        .collect();
    names.sort();
    assert_eq!(
        names,
        [
            "GIT_TERMINAL_PROMPT",
            "HOME",
            "LANG",
            "LOGNAME",
            "PATH",
            "PWD",
            "SHLVL",
            "TERM",
            "TMPDIR",
            "USER",
            "_"
        ],
        "{result:?}"
    );
    assert!(
        result
            .stdout
            .contains(&format!("HOME={}\n", home.display()))
    );
    assert!(result.stdout.contains("TERM=dumb\n"));
    println!("PC-I4: ~/.ssh/known_hosts and ~/.gitconfig reads succeeded; ~/.asura stayed denied");
}
/// PC-I5: a loopback HTTP fixture and a name lookup succeed under the
/// person profile; the Asura control socket stays unreachable (PX3), while
/// a `!!` command reaches it.
fn pc_i5_person_profile_network(home: &Path, person: &PersonHome) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        use std::io::{Read, Write};
        let (mut stream, _) = listener.accept().unwrap();
        let mut request = [0u8; 1024];
        let _ = stream.read(&mut request);
        stream
            .write_all(b"HTTP/1.0 200 OK\r\nContent-Length: 9\r\n\r\nperson-ok")
            .unwrap();
    });
    let result = person_run(
        home,
        Profile::Person1,
        &format!("/usr/bin/curl --silent --show-error http://{address}/"),
        person.sandboxed(&[]),
    );
    server.join().unwrap();
    assert_eq!(result.exit_code, Some(0), "{result:?}");
    assert_eq!(result.stdout, "person-ok");
    let result = person_run(
        home,
        Profile::Person1,
        "/usr/bin/perl -MSocket -e 'my ($e, @r) = Socket::getaddrinfo(\"localhost\", \"80\"); exit($e ? 1 : 0)'",
        person.sandboxed(&[]),
    );
    assert_eq!(result.exit_code, Some(0), "{result:?}");
    // PX3: a Unix socket under the Asura home refuses the person profile.
    let socket = home.join(".asura/run/pc-i5.sock");
    let control = std::os::unix::net::UnixListener::bind(&socket).unwrap();
    control.set_nonblocking(true).unwrap();
    let connect = format!(
        "/usr/bin/perl -MIO::Socket::UNIX -e 'IO::Socket::UNIX->new(Type => SOCK_STREAM, Peer => shift) or exit 3; exit 0' '{}'; exit $?",
        socket.display()
    );
    let result = person_run(home, Profile::Person1, &connect, person.sandboxed(&[]));
    assert_eq!(result.exit_code, Some(3), "{result:?}");
    assert_eq!(
        control.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock,
        "person profile reached the Asura socket"
    );
    let result = person_run(home, Profile::PersonOutside1, &connect, person.outside(&[]));
    assert_eq!(result.exit_code, Some(0), "{result:?}");
    assert!(
        control.accept().is_ok(),
        "!! reaches the control socket (EXD-1)"
    );
    drop(control);
    fs::remove_file(&socket).unwrap();
    println!("PC-I5: loopback HTTP and lookup succeeded; the Asura socket was denied to ! only");
}
/// PC-I22: a fixture SSH agent answers through the sandbox; a variable
/// naming a regular file is dropped and the command still runs.
fn pc_i22_ssh_agent_socket(home: &Path, person: &PersonHome) {
    let socket = home.join("pc-i22.sock");
    let key = home.join("pc-i22-key");
    let status = std::process::Command::new("/usr/bin/ssh-keygen")
        .args([
            "-q",
            "-t",
            "ed25519",
            "-N",
            "",
            "-C",
            "pc-i22-fixture",
            "-f",
        ])
        .arg(&key)
        .status()
        .unwrap();
    assert!(status.success());
    let mut agent = std::process::Command::new("/usr/bin/ssh-agent")
        .arg("-D")
        .arg("-a")
        .arg(&socket)
        .stdout(std::process::Stdio::null())
        .env_clear()
        .spawn()
        .unwrap();
    let until = Instant::now() + Duration::from_secs(5);
    while !socket.exists() {
        assert!(Instant::now() < until, "fixture agent socket");
        std::thread::sleep(Duration::from_millis(20));
    }
    let added = std::process::Command::new("/usr/bin/ssh-add")
        .arg(&key)
        .env_clear()
        .env("SSH_AUTH_SOCK", &socket)
        .stderr(std::process::Stdio::null())
        .status()
        .unwrap();
    assert!(added.success());
    let sock = |path: &Path| {
        vec![(
            b"SSH_AUTH_SOCK".to_vec(),
            path.as_os_str().as_encoded_bytes().to_vec(),
        )]
    };
    let result = person_run(
        home,
        Profile::Person1,
        "/usr/bin/ssh-add -l",
        person.sandboxed(&sock(&socket)),
    );
    let _ = agent.kill();
    let _ = agent.wait();
    assert_eq!(result.exit_code, Some(0), "{result:?}");
    assert!(result.stdout.contains("pc-i22-fixture"), "{result:?}");
    let file = home.join("pc-i22-not-a-socket");
    fs::write(&file, b"").unwrap();
    let result = person_run(
        home,
        Profile::Person1,
        "printf '%s' \"${SSH_AUTH_SOCK-unset}\"",
        person.sandboxed(&sock(&file)),
    );
    assert_eq!(result.exit_code, Some(0), "{result:?}");
    assert_eq!(result.stdout, "unset");
    println!("PC-I22: the fixture agent answered through the sandbox; a regular file was dropped");
}
/// PC-I29: with `sandbox-exec` unavailable, `!!` still runs and `!` is not
/// started with `sandbox_unavailable` (setup reason 6).
fn pc_i29_sandbox_exec_unavailable(home: &Path, person: &PersonHome) {
    let job = person_job(
        home,
        Profile::PersonOutside1,
        "printf ran > pc-i29-outside",
        person.outside(&[]),
        Some("no-sandbox-exec"),
        None,
    );
    let result = settle(job, Duration::from_secs(10), false);
    assert_eq!(result.exit_code, Some(0), "{result:?}");
    assert!(home.join("project/pc-i29-outside").exists());
    let mut job = person_job(
        home,
        Profile::Person1,
        "printf ran > pc-i29-person",
        person.sandboxed(&[]),
        Some("no-sandbox-exec"),
        None,
    );
    pump_job(&mut job, "PC-I29 refusal", |_| false);
    let result = job.result().unwrap();
    assert_eq!(result.failure, Some(ExecCmdError::Unavailable));
    assert_eq!(result.setup_reason, 6, "sandbox_unavailable");
    assert!(!home.join("project/pc-i29-person").exists());
    println!("PC-I29: !! ran without sandbox-exec; ! was refused with reason 6");
}
/// PC-U50 through the real guardian: each invalid frame is setup-failed
/// reason 5 and no command runs.
fn pc_u50_guardian_refuses_bad_frames(home: &Path, person: &PersonHome) {
    let frame = |roles: &[u8], entries: &[(&str, &str)]| {
        let mut body = vec![roles.len() as u8];
        body.extend_from_slice(roles);
        body.extend_from_slice(&(entries.len() as u16).to_be_bytes());
        for (name, value) in entries {
            body.extend_from_slice(&(name.len() as u16).to_be_bytes());
            body.extend_from_slice(name.as_bytes());
            body.extend_from_slice(&(value.len() as u32).to_be_bytes());
            body.extend_from_slice(value.as_bytes());
        }
        let mut frame = (body.len() as u32).to_be_bytes().to_vec();
        frame.extend_from_slice(&body);
        frame
    };
    let home_text = home.to_str().unwrap();
    let base = [
        ("PATH", "/usr/bin:/bin"),
        ("HOME", home_text),
        ("TMPDIR", "/tmp"),
    ];
    let good = frame(&[1, 2, 4, 4], &base);
    let mut trailing = good.clone();
    trailing.push(b'x');
    let mut other = base.to_vec();
    other.push(("EDITOR", "vi"));
    for (label, profile, bytes) in [
        ("short", Profile::Person1, good[..good.len() - 3].to_vec()),
        ("trailing", Profile::Person1, trailing),
        (
            "non-PC3 name",
            Profile::Person1,
            frame(&[1, 2, 4, 4], &other),
        ),
        (
            "no HOME",
            Profile::Person1,
            frame(&[1, 2, 4, 4], &[base[0], base[2]]),
        ),
        (
            "home not first",
            Profile::Person1,
            frame(&[2, 1, 4, 4], &base),
        ),
        (
            "roots for !!",
            Profile::PersonOutside1,
            frame(&[1, 2], &base),
        ),
        (
            "job marker name",
            Profile::PersonOutside1,
            frame(&[], &[("ASURA_JOB_1", "1")]),
        ),
        ("empty", Profile::PersonOutside1, Vec::new()),
    ] {
        let setup = match profile {
            Profile::Person1 => person.sandboxed(&[]),
            _ => person.outside(&[]),
        };
        let mut job = person_job(
            home,
            profile,
            "printf ran > pc-u50-ran",
            setup,
            None,
            Some(bytes),
        );
        pump_job(&mut job, label, |_| false);
        let result = job.result().unwrap();
        assert_eq!(result.setup_reason, 5, "{label}: {result:?}");
        assert!(result.failure.is_some(), "{label}");
        assert!(!home.join("project/pc-u50-ran").exists(), "{label}");
    }
    println!("PC-U50 (guardian): every invalid frame was setup-failed reason 5; nothing ran");
}
/// OD1 and EX5 for `!`: the job scratch is writable, so the sandbox
/// marker attributes a `setsid` escapee, which is frozen and killed.
fn p1_person_escapee_marker(home: &Path, person: &PersonHome) {
    let project = home.join("project");
    fs::write(
        project.join("p1-escapee.pl"),
        include_str!("fixtures/escapee.pl"),
    )
    .unwrap();
    let result = person_run(
        home,
        Profile::Person1,
        "/usr/bin/perl p1-escapee.pl setsid p1e; while [ ! -e p1e.pid ]; do /bin/sleep 0.05; done; exit 0",
        person.sandboxed(&[]),
    );
    assert_eq!(result.exit_code, Some(0), "{result:?}");
    assert_eq!(result.escapees, 1, "{result:?}");
    let pid: i32 = fs::read_to_string(project.join("p1e.pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert!(!alive(pid), "the ! escapee was killed");
    // The scratch is writable and starts empty (OD1); HOME stays the
    // person's. The command names the path; it is not listed for it.
    let id = asura_platform::random_id();
    let hex: String = id.iter().map(|b| format!("{b:02x}")).collect();
    let scratch = home.join(format!(".asura/tmp/exec-cmd-{hex}"));
    let command = format!(
        "d='{}'; if [ -z \"$(/bin/ls -A \"$d\")\" ] && printf x > \"$d/probe\" && /bin/cat \"$d/probe\" >/dev/null; then printf scratch-ok; fi; printf '|%s' \"$HOME\"",
        scratch.display()
    );
    let job = person_job_with_id(
        home,
        id,
        Profile::Person1,
        &command,
        person.sandboxed(&[]),
        None,
        None,
    );
    let result = settle(job, Duration::from_secs(10), false);
    assert!(
        !scratch.exists(),
        "the scratch is deleted when the job ends"
    );
    assert_eq!(result.exit_code, Some(0), "{result:?}");
    assert_eq!(result.stdout, format!("scratch-ok|{}", home.display()));
    println!(
        "OD1: the ! job scratch was writable and empty; the setsid escapee was killed (count 1)"
    );
}
/// PC8: `T` stops a TERM-ignoring person group with KILL after the 2 s
/// grace, for both person profiles.
fn p1_person_grace(home: &Path, person: &PersonHome) {
    for (profile, setup) in [
        (Profile::Person1, person.sandboxed(&[])),
        (Profile::PersonOutside1, person.outside(&[])),
    ] {
        let tag = profile.token();
        let mut job = person_job(
            home,
            profile,
            &format!("trap '' TERM; printf '%s' $$ > grace-{tag}; while :; do /bin/sleep 1; done"),
            setup,
            None,
            None,
        );
        let pidfile = home.join(format!("project/grace-{tag}"));
        pump_job(&mut job, "grace job started", |_| pidfile.exists());
        let stopped = Instant::now();
        job.stop(StopKind::Term);
        let result = finish_stopped(job, "grace job settled");
        let elapsed = stopped.elapsed();
        assert_eq!(result.reason, Some(StopReason::Cancelled), "{tag}");
        assert_eq!(result.signal, Some(9), "{tag}: KILL after the grace");
        assert!(elapsed >= Duration::from_millis(1900), "{tag}: {elapsed:?}");
        group_absent(&pidfile);
        println!("PC8 {tag}: TERM ignored, KILL after {elapsed:?}");
    }
}
/// Pump an outside job until the guardian settled.
fn guardian_settled(job: &mut JobGuard, what: &str) -> asura_platform::exec_cmd::GuardianSettled {
    let until = Instant::now() + Duration::from_secs(15);
    loop {
        if let Some(settled) = job.guardian_settled() {
            return settled;
        }
        assert!(Instant::now() < until, "{what}");
        asura_platform::poll(&job.interests(), Duration::from_millis(20)).unwrap();
        job.poll(Instant::now()).unwrap();
    }
}
/// EC-I9 and EC-I10: a `!!` escapee that kept its environment and closed
/// its output is found by the env-marker scan, counted (kind 6 count 1,
/// kind 2 reason 14), not signalled and still running after settlement.
/// One with a scrubbed environment and a new session is not attributable
/// (EXD-1): the owned group still settles, with no count.
fn ec_i9_ec_i10_outside_survivors(home: &Path, person: &PersonHome) {
    let exe = std::env::current_exe().unwrap();
    let project = home.join("project");
    let command = format!(
        "'{}' --survivor ec9 closed & while [ ! -e ec9.pid ]; do /bin/sleep 0.05; done; exit 0",
        exe.display()
    );
    let mut job = person_job(
        home,
        Profile::PersonOutside1,
        &command,
        person.outside(&[]),
        None,
        None,
    );
    let settled = guardian_settled(&mut job, "EC-I9 guardian settled");
    assert_eq!(settled.escapees, 1, "{settled:?}");
    assert!(!settled.output_held, "reason 14: no capture pipe held");
    assert_eq!(settled.exit, Exit::Exited(0));
    let result = finish_stopped(job, "EC-I9 settled");
    assert_eq!((result.exit_code, result.escapees), (Some(0), 1));
    assert!(result.cleanup_confirmed);
    let pid: i32 = fs::read_to_string(project.join("ec9.pid"))
        .unwrap()
        .parse()
        .unwrap();
    assert!(alive(pid), "EC-D1: the survivor is left running");
    release_survivor(&project, "ec9");
    println!("EC-I9: env-marked survivor counted (kind 6 = 1, reason 14) and left running");
    let command = format!(
        "/usr/bin/env -i '{}' --survivor ec10 closed & while [ ! -e ec10.pid ]; do /bin/sleep 0.05; done; exit 0",
        exe.display()
    );
    let mut job = person_job(
        home,
        Profile::PersonOutside1,
        &command,
        person.outside(&[]),
        None,
        None,
    );
    let settled = guardian_settled(&mut job, "EC-I10 guardian settled");
    assert_eq!(settled.escapees, 0, "scrubbed survivor is not attributable");
    let result = finish_stopped(job, "EC-I10 settled");
    assert_eq!((result.exit_code, result.escapees), (Some(0), 0));
    let pid: i32 = fs::read_to_string(project.join("ec10.pid"))
        .unwrap()
        .parse()
        .unwrap();
    assert!(alive(pid), "never signalled");
    release_survivor(&project, "ec10");
    println!("EC-I10: env-scrubbed new-session survivor not attributed; the group settled");
}
/// OD2 and PQ3: a `!!` survivor holding the output keeps capture open
/// after the guardian's kind 2 reason 15; the job settles at the
/// survivor's EOF, or at `close_capture`, which leaves it running. An
/// in-group background process is not stopped at shell exit.
fn od2_outside_survivor_holds_output(home: &Path, person: &PersonHome) {
    let exe = std::env::current_exe().unwrap();
    let project = home.join("project");
    for (tag, mode) in [("od2a", "hold"), ("od2b", "group")] {
        let command = format!(
            "'{}' --survivor {tag} {mode} & while [ ! -e {tag}.pid ]; do /bin/sleep 0.05; done; exit 0",
            exe.display()
        );
        let mut job = person_job(
            home,
            Profile::PersonOutside1,
            &command,
            person.outside(&[]),
            None,
            None,
        );
        let settled = guardian_settled(&mut job, "OD2 guardian settled");
        assert_eq!(settled.escapees, 1, "{tag}: {settled:?}");
        assert!(settled.output_held, "{tag}: reason 15");
        let pid: i32 = fs::read_to_string(project.join(format!("{tag}.pid")))
            .unwrap()
            .parse()
            .unwrap();
        assert!(alive(pid), "{tag}: PQ3, not stopped at shell exit");
        // The survivor's line arrives after the guardian settled.
        let line = format!("held {tag}\n");
        pump_job(&mut job, "OD2 survivor output", |job| {
            job.1.0.lock().unwrap().kept[0].ends_with(line.as_bytes())
        });
        assert!(!job.settled(), "{tag}: capture continues");
        if mode == "hold" {
            // The survivor ends: EOF settles the job with its output.
            release_survivor(&project, tag);
            let result = finish_stopped(job, "OD2 EOF settlement");
            assert_eq!((result.exit_code, result.escapees), (Some(0), 1));
        } else {
            // A cancel closes capture; the survivor is not killed.
            job.stop(StopKind::Term);
            assert!(job.close_capture());
            assert!(job.settled());
            let result = job.result().unwrap();
            assert_eq!((result.exit_code, result.escapees), (Some(0), 1));
            job.take_cleanup()
                .unwrap()
                .run(
                    Instant::now() + Duration::from_secs(2),
                    &AtomicBool::new(false),
                )
                .unwrap();
            assert!(alive(pid), "{tag}: EC-D1, left running after close_capture");
            release_survivor(&project, tag);
        }
        println!("OD2 {tag}: kind 2 reason 15, capture continued after the shell exited");
    }
}
/// PC-I38: a `!!` launch environment with `DYLD_INSERT_LIBRARIES` naming a
/// fixture library. The guardian never loads it; the variable is given
/// only to the `/bin/sh` spawn. A control run of the same unsigned test
/// binary with the variable in its own environment loads it.
fn pc_i38_dyld_launch_variable(home: &Path, person: &PersonHome) {
    let markers = home.join("pc-i38");
    fs::create_dir(&markers).unwrap();
    let source = home.join("pc-i38.c");
    fs::write(&source, include_str!("fixtures/dyld_marker.c")).unwrap();
    let library = home.join("pc-i38.dylib");
    let built = std::process::Command::new("/usr/bin/cc")
        .args(["-dynamiclib", "-o"])
        .arg(&library)
        .arg(format!("-DMARKER_DIR=\"{}\"", markers.display()))
        .arg(&source)
        .status()
        .unwrap();
    assert!(built.success(), "fixture library build");
    let library_text = library.to_str().unwrap();
    // Control: the variable in a process's own environment loads it.
    let control = std::process::Command::new(std::env::current_exe().unwrap())
        .arg("--noop")
        .env("DYLD_INSERT_LIBRARIES", library_text)
        .spawn()
        .unwrap();
    let control_pid = control.id();
    assert!(control.wait_with_output().unwrap().status.success());
    assert!(
        markers.join(format!("loaded.{control_pid}")).exists(),
        "control run loaded the fixture"
    );
    let mut job = person_job(
        home,
        Profile::PersonOutside1,
        "printf '%s' \"$PC_I38_SEEN\"",
        person.outside(&[
            ("DYLD_INSERT_LIBRARIES", library_text),
            ("PC_I38_SEEN", "launch-env"),
        ]),
        None,
        None,
    );
    let guardian = job.guardian_pid_for_test();
    pump_job(&mut job, "PC-I38 settled", |_| false);
    let result = job.result().unwrap();
    assert_eq!(result.exit_code, Some(0), "{result:?}");
    let bytes = job.1.0.lock().unwrap();
    assert_eq!(String::from_utf8_lossy(&bytes.kept[0]), "launch-env");
    drop(bytes);
    assert!(
        !markers.join(format!("loaded.{guardian}")).exists(),
        "the guardian loaded the launch environment's library"
    );
    let loaded: Vec<_> = fs::read_dir(&markers)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    assert_eq!(
        loaded,
        [format!("loaded.{control_pid}")],
        "only the control loaded it"
    );
    if let Some(mut cleanup) = job.take_cleanup() {
        cleanup
            .run(
                Instant::now() + Duration::from_secs(2),
                &AtomicBool::new(false),
            )
            .unwrap();
    }
    println!("PC-I38: the guardian (pid {guardian}) never loaded the launch environment's library");
}
/// PC-I37: the PC-P1 probe steps under the person profile with the final
/// (empty) `mach-lookup` list. Needs the network; opt in with
/// `ASURA_TEST_NETWORK=1`.
fn pc_i37_mach_probe_steps(home: &Path, person: &PersonHome) {
    let cargo = std::process::Command::new("rustup")
        .args(["which", "cargo"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| PathBuf::from(String::from_utf8(o.stdout).unwrap().trim()));
    let repo = home.join("project/pc-i37-repo");
    fs::create_dir(&repo).unwrap();
    for args in [
        &["init", "-q", "."][..],
        &[
            "-c",
            "user.email=f@x",
            "-c",
            "user.name=f",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "i",
        ],
    ] {
        assert!(
            std::process::Command::new("/usr/bin/git")
                .args(args)
                .current_dir(&repo)
                .status()
                .unwrap()
                .success()
        );
    }
    let toolchain = cargo
        .as_ref()
        .and_then(|c| c.parent()?.parent().map(Path::to_path_buf));
    let mut steps = vec![
        ("git ls-remote", "/usr/bin/git ls-remote https://github.com/git/git HEAD".to_owned()),
        ("git status", "/usr/bin/git -C pc-i37-repo status --short".into()),
        ("curl", "/usr/bin/curl -sI https://example.com".into()),
        ("xcrun", "/usr/bin/xcrun --find clang".into()),
        ("git --version", "/usr/bin/git --version".into()),
        (
            "lookup",
            "/usr/bin/perl -MSocket -e 'my ($e, @r) = Socket::getaddrinfo(\"example.com\", \"443\"); exit($e ? 1 : 0)'".into(),
        ),
    ];
    if let Some(cargo) = &cargo {
        steps.push((
            "cargo search",
            format!(
                "CARGO_HOME=\"$TMPDIR/pc-i37-cargo\" '{}' search serde --limit 1",
                cargo.display()
            ),
        ));
    }
    for (label, command) in steps {
        // The toolchain is read and exec only, through the developer root.
        let result = person_run(
            home,
            Profile::Person1,
            &command,
            person.sandboxed_with(toolchain.as_deref(), &[]),
        );
        assert_eq!(result.exit_code, Some(0), "PC-I37 {label}: {result:?}");
        println!("PC-I37 {label}: passed");
    }
}
