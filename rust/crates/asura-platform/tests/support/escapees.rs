//! EX5 escapee control against the real guardian, `sandbox-exec` and real
//! escapee processes (escapee-control.md EC-I1–EC-I8, EC-I11, EC-V-OLD and
//! EC-D2). Every fixture escapee is a test-owned Perl process with a fixed
//! 20 s lifetime and a release file, so a failure never leaves a daemon.
use super::*;
use asura_platform::exec_cmd::{ExecCmdJob, UnconfirmedReason, escapee_scan_for_test};

/// The escapee fixture. Modes: `setsid` (new session, parent exits, ppid
/// 1), `doublefork` (new session, then a second fork whose parent exits),
/// `pipehold` (new session, keeps the capture pipes) and `forkloop` (each
/// generation forks a child that starts a new session, and exits).
/// Files in the project: `<tag>.pid`, `<tag>.ppid`, `<tag>.beat` (one byte
/// per 50 ms) and `<tag>.pids` (forkloop). `<tag>.release` ends it.
const FIXTURE: &str = include_str!("../fixtures/escapee.pl");

/// The (start seconds, start microseconds) of a live, non-zombie process.
fn started(pid: i32) -> Option<(u64, u64)> {
    // SAFETY: plain-data output; zero is a valid bit pattern.
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = std::mem::size_of::<libc::proc_bsdinfo>() as i32;
    // SAFETY: a complete output buffer of the stated size; read-only query.
    let got = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            (&mut info as *mut libc::proc_bsdinfo).cast(),
            size,
        )
    };
    (got == size && info.pbi_status != libc::SZOMB)
        .then_some((info.pbi_start_tvsec, info.pbi_start_tvusec))
}
/// One fixture escapee, pinned by its start time.
struct Escapee {
    pid: i32,
    start: (u64, u64),
    beat: PathBuf,
}
impl Escapee {
    fn alive(&self) -> bool {
        started(self.pid) == Some(self.start)
    }
    fn beats(&self) -> u64 {
        fs::metadata(&self.beat).map(|m| m.len()).unwrap_or(0)
    }
}
fn job(home: &Path, command: &str, fault: Option<&str>) -> (JobGuard, [u8; 16]) {
    let id = asura_platform::random_id();
    let runtime = RuntimeDirectory::scratch(home, true).unwrap();
    let project = ProjectIdentity::open(
        home.join("project").to_str().unwrap(),
        &runtime.state_boundary(),
    )
    .unwrap();
    let cancel = AtomicBool::new(false);
    let request = ExecCmdRequest {
        job_id: id,
        profile: Profile::Model1,
        command: command.into(),
        cwd: None,
        secrets: Vec::new(),
        secret_command: None,
    };
    let mut prepared = PreparedExecCmd::prepare(runtime, &project, request, &cancel)
        .unwrap_or_else(|e| panic!("prepare: {:?}", e.reason));
    if let Some(fault) = fault {
        prepared.guardian_fault_for_test(fault);
    }
    let collected = Collected::default();
    let spawned = prepared
        .spawn(
            &std::env::current_exe().unwrap(),
            &cancel,
            Box::new(collected.clone()),
        )
        .unwrap_or_else(|e| panic!("spawn: {:?}", e.reason));
    (JobGuard(spawned, collected), id)
}
/// Total CPU time of a live process in milliseconds, from `ps -o time=`
/// (`[h:]m:ss.cc`).
fn cpu_ms(pid: i32) -> u64 {
    let out = std::process::Command::new("/bin/ps")
        .args(["-o", "time=", "-p", &pid.to_string()])
        .output()
        .unwrap();
    let text = String::from_utf8(out.stdout).unwrap();
    let mut total = 0.0f64;
    for part in text.trim().split(':') {
        total = total * 60.0
            + part
                .parse::<f64>()
                .unwrap_or_else(|_| panic!("ps time {text:?}"));
    }
    (total * 1000.0).round() as u64
}
fn pump(job: &mut ExecCmdJob) {
    asura_platform::poll(&job.interests(), Duration::from_millis(20)).unwrap();
    job.poll(Instant::now()).unwrap();
}
/// Pump every unsettled job until `done`; the limit bounds only this test
/// harness, never the product.
fn pump_until(
    jobs: &mut [&mut JobGuard],
    what: &str,
    mut done: impl FnMut(&[&mut JobGuard]) -> bool,
) {
    let until = Instant::now() + Duration::from_secs(15);
    while !done(jobs) {
        assert!(Instant::now() < until, "{what}");
        for job in jobs.iter_mut() {
            if !job.settled() {
                pump(job);
            }
        }
    }
}
/// Pump until the escapee `tag` recorded its PID, then pin it.
fn escapee(project: &Path, tag: &str, jobs: &mut [&mut JobGuard]) -> Escapee {
    let path = project.join(format!("{tag}.pid"));
    pump_until(jobs, &format!("{tag} escapee started"), |_| path.exists());
    let pid: i32 = fs::read_to_string(&path).unwrap().trim().parse().unwrap();
    let start = started(pid).unwrap_or_else(|| panic!("{tag} escapee is alive"));
    Escapee {
        pid,
        start,
        beat: project.join(format!("{tag}.beat")),
    }
}
fn ppid(project: &Path, tag: &str) -> i32 {
    fs::read_to_string(project.join(format!("{tag}.ppid")))
        .unwrap()
        .trim()
        .parse()
        .unwrap()
}
/// Pump until settled; then the scratch is removed and both streams ended.
fn finish(mut job: JobGuard) -> (ExecCmdResult, [bool; 2], Option<UnconfirmedReason>) {
    pump_until(&mut [&mut job], "job settled", |jobs| jobs[0].settled());
    let result = job.result().unwrap();
    job.take_cleanup()
        .unwrap()
        .run(
            Instant::now() + Duration::from_secs(2),
            &AtomicBool::new(false),
        )
        .unwrap();
    let ended = job.1.0.lock().unwrap().ended;
    (result, ended, job.cleanup_unconfirmed())
}
/// No escapee writes the project after settlement: the heartbeat stops.
fn assert_stopped(escapee: &Escapee, label: &str) {
    assert!(!escapee.alive(), "{label}: escapee alive after settlement");
    let before = escapee.beats();
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(
        escapee.beats(),
        before,
        "{label}: project write after settlement"
    );
}
/// Remove a fixture escapee that a fault case left running: the release
/// file ends it; the harness then waits for its exit.
fn release(project: &Path, tag: &str, escapee: &Escapee) {
    fs::write(project.join(format!("{tag}.release")), b"").unwrap();
    let until = Instant::now() + Duration::from_secs(3);
    while escapee.alive() {
        if Instant::now() >= until {
            CLEAN.store(false, Ordering::Release);
            panic!("{tag} fixture escapee did not end on release");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

pub(super) fn escapees(home: &Path) {
    let project = home.join("project");
    fs::write(project.join("escapee.pl"), FIXTURE).unwrap();
    all_kinds(home, &project);
    forking(home, &project);
    nested_sandbox(home, &project);
    concurrent(home, &project);
    old_group_only_check(home, &project);
    undying_pipe_holder(home, &project);
    unverifiable_scan(home);
}

/// EC-I1, EC-I2, EC-I3 and EX-V9 at the platform layer: `setsid`,
/// double-fork and pipe-holding escapees under one job are detected and
/// killed, the capture pipes reach EOF, and settlement confirms cleanup
/// only after all three are gone. EC-I11: one real scan from outside the
/// guardian finds the job's escapees within the 100 ms poll.
fn all_kinds(home: &Path, project: &Path) {
    let command = "printf '%s' $$ > kinds.leader; /usr/bin/perl escapee.pl setsid s1; /usr/bin/perl escapee.pl doublefork d1; \
        /usr/bin/perl escapee.pl pipehold p1; while [ ! -e kinds.go ]; do sleep 0.05; done; exit 0";
    let (mut job, id) = job(home, command, None);
    let setsid = escapee(project, "s1", &mut [&mut job]);
    let double = escapee(project, "d1", &mut [&mut job]);
    let holder = escapee(project, "p1", &mut [&mut job]);
    assert_eq!(ppid(project, "s1"), 1, "setsid escapee reparented");
    assert_eq!(ppid(project, "d1"), 1, "double-fork escapee reparented");
    let leader: i32 = fs::read_to_string(project.join("kinds.leader"))
        .unwrap()
        .parse()
        .unwrap();
    for e in [&setsid, &double, &holder] {
        // SAFETY: read-only query of a live test-owned process.
        let pgid = unsafe { libc::getpgid(e.pid) };
        assert!(pgid > 0 && pgid != leader, "escapee left the owned group");
    }
    // EC-I11: the real scan with this job's marker and decoy.
    let asura = home.join(".asura");
    let scratch = asura.join(format!(
        "tmp/exec-cmd-{}",
        id.iter().map(|b| format!("{b:02x}")).collect::<String>()
    ));
    let started_scan = Instant::now();
    let found = escapee_scan_for_test(scratch.to_str().unwrap(), asura.to_str().unwrap()).unwrap();
    let cost = started_scan.elapsed();
    for e in [&setsid, &double, &holder] {
        assert!(found.contains(&e.pid), "scan attributes escapee {}", e.pid);
    }
    assert!(cost < Duration::from_millis(100), "scan took {cost:?}");
    println!(
        "EC-I11: one UID scan attributed {} processes in {cost:?}",
        found.len()
    );
    // Before the leader exits, the pipe holder keeps the capture open.
    fs::write(project.join("kinds.go"), b"").unwrap();
    let (result, ended, unconfirmed) = finish(job);
    assert!(result.cleanup_confirmed, "{result:?}");
    assert_eq!(unconfirmed, None, "never reported unconfirmed");
    assert_eq!(result.escapees, 3, "{result:?}");
    assert_eq!(result.exit_code, Some(0));
    assert_eq!(ended, [true, true], "capture pipes reached EOF");
    assert!(
        result
            .render_text(&Tails::default(), "complete")
            .contains("cleanup_confirmed=true capture=complete escapees=3\n")
    );
    for (label, e) in [
        ("setsid", &setsid),
        ("double-fork", &double),
        ("pipe holder", &holder),
    ] {
        assert_stopped(e, label);
    }
    println!(
        "EC-I1 EC-I2 EC-I3: setsid, double-fork and pipe-holding escapees killed before cleanup_confirmed"
    );
}

/// EC-I5: a PID-hopping escapee (each generation starts a new session in a
/// child and exits) is frozen and killed; nothing hops on after settlement.
fn forking(home: &Path, project: &Path) {
    let command = "/usr/bin/perl escapee.pl forkloop f1; \
        while [ $(wc -l < f1.pids 2>/dev/null || echo 0) -lt 5 ]; do sleep 0.05; done; exit 0";
    let (job, _) = job(home, command, None);
    let (result, ended, unconfirmed) = finish(job);
    assert!(result.cleanup_confirmed, "{result:?}");
    assert_eq!(unconfirmed, None);
    assert!(result.escapees >= 1, "{result:?}");
    assert_eq!(ended, [true, true]);
    let pids = project.join("f1.pids");
    let before = fs::read_to_string(&pids).unwrap();
    std::thread::sleep(Duration::from_millis(300));
    let after = fs::read_to_string(&pids).unwrap();
    assert_eq!(before, after, "a hopping escapee survived the sweep");
    let generations = after.lines().count();
    assert!(generations >= 5, "{generations} generations");
    println!(
        "EC-I5: forking escapee stopped after {generations} generations; {} killed",
        result.escapees
    );
}

/// EC-I7: a nested `sandbox-exec` is refused inside the job profile; an
/// escapee started afterwards is still attributed and killed.
fn nested_sandbox(home: &Path, project: &Path) {
    let command = "/usr/bin/sandbox-exec -p '(version 1)(allow default)' /usr/bin/true \
        2>nested.err; printf '%s' $? > nested.status; /usr/bin/perl escapee.pl setsid n1; \
        while [ ! -e n1.pid ]; do sleep 0.05; done; exit 0";
    let (mut job, _) = job(home, command, None);
    let nested = escapee(project, "n1", &mut [&mut job]);
    let (result, _, _) = finish(job);
    assert_ne!(
        fs::read_to_string(project.join("nested.status")).unwrap(),
        "0"
    );
    let message = fs::read_to_string(project.join("nested.err")).unwrap();
    assert!(
        message.contains("sandbox_apply: Operation not permitted"),
        "{message}"
    );
    assert!(result.cleanup_confirmed);
    assert_eq!(result.escapees, 1, "{result:?}");
    assert_stopped(&nested, "nested");
    println!("EC-I7: nested sandbox-exec refused; the escapee stayed attributed and was killed");
}

/// EC-I6: two concurrent jobs, each with an escapee. Job A settles while
/// B's escapee keeps running; each job kills only its own.
fn concurrent(home: &Path, project: &Path) {
    let command = |tag: &str| {
        format!(
            "/usr/bin/perl escapee.pl setsid {tag}; while [ ! -e {tag}.go ]; do sleep 0.05; done; exit 0"
        )
    };
    let (mut a, _) = job(home, &command("ca"), None);
    let (mut b, _) = job(home, &command("cb"), None);
    let escapee_a = escapee(project, "ca", &mut [&mut a, &mut b]);
    let escapee_b = escapee(project, "cb", &mut [&mut a, &mut b]);
    fs::write(project.join("ca.go"), b"").unwrap();
    pump_until(&mut [&mut a, &mut b], "job A settled", |jobs| {
        jobs[0].settled()
    });
    assert!(!b.settled());
    assert!(escapee_b.alive(), "job A killed job B's escapee");
    let before = escapee_b.beats();
    let (result_a, _, _) = finish(a);
    assert!(result_a.cleanup_confirmed);
    assert_eq!(result_a.escapees, 1, "{result_a:?}");
    assert_stopped(&escapee_a, "job A");
    pump_until(&mut [&mut b], "job B escapee still writes", |_| {
        escapee_b.beats() > before
    });
    assert!(escapee_b.alive());
    fs::write(project.join("cb.go"), b"").unwrap();
    let (result_b, _, _) = finish(b);
    assert!(result_b.cleanup_confirmed);
    assert_eq!(result_b.escapees, 1, "{result_b:?}");
    assert_stopped(&escapee_b, "job B");
    println!("EC-I6: two concurrent jobs each killed only their own escapee");
}

/// EC-V-OLD and EC-I4: with the old group-only check (`group-only` fault)
/// the guardian confirms cleanup while a `setsid` escapee lives and writes
/// the project. The real gate never does: see `all_kinds` and
/// `undying_pipe_holder`.
fn old_group_only_check(home: &Path, project: &Path) {
    let command =
        "/usr/bin/perl escapee.pl setsid o1; while [ ! -e o1.pid ]; do sleep 0.05; done; exit 0";
    let (mut job, _) = job(home, command, Some("group-only"));
    let survivor = escapee(project, "o1", &mut [&mut job]);
    let (result, _, _) = finish(job);
    assert!(result.cleanup_confirmed, "old check confirms: {result:?}");
    assert_eq!(result.escapees, 0);
    assert!(survivor.alive(), "the old check left the escapee running");
    let before = survivor.beats();
    let until = Instant::now() + Duration::from_secs(3);
    while survivor.beats() == before {
        assert!(Instant::now() < until, "escapee wrote after settlement");
        std::thread::sleep(Duration::from_millis(20));
    }
    release(project, "o1", &survivor);
    println!(
        "EC-V-OLD: the group-only check confirmed cleanup while the escapee wrote the project"
    );
}

/// EC-I8, EC-I3 and EC-I4 with the real gate: a pipe-holding escapee that
/// survives SIGKILL (`undyable` fault) keeps the capture pipe open and the
/// job unsettled; the guardian reports cleanup_unconfirmed reason 13 and
/// never settled while it lives. When it is gone, a paced sweep records a
/// late settlement that does not confirm cleanup, and EOF arrives.
fn undying_pipe_holder(home: &Path, project: &Path) {
    let command =
        "/usr/bin/perl escapee.pl pipehold u1; while [ ! -e u1.pid ]; do sleep 0.05; done; exit 0";
    let (mut job, _) = job(home, command, Some("undyable"));
    let holder = escapee(project, "u1", &mut [&mut job]);
    pump_until(&mut [&mut job], "cleanup_unconfirmed", |jobs| {
        jobs[0].cleanup_unconfirmed().is_some()
    });
    assert_eq!(
        job.cleanup_unconfirmed(),
        Some(UnconfirmedReason::EscapeeUnterminated)
    );
    for _ in 0..10 {
        pump(&mut job);
    }
    // J2T-05: lease EOF after the leader-exit stop only ends lease
    // polling; the hung-up lease never spins the guardian's paced loop.
    job.close_lease();
    let guardian = job.guardian_pid_for_test();
    let before = cpu_ms(guardian);
    let window = Instant::now();
    while window.elapsed() < Duration::from_secs(1) {
        pump(&mut job);
    }
    let used = cpu_ms(guardian) - before;
    assert!(
        used < 250,
        "guardian used {used} ms of CPU in 1 s after EOF"
    );
    println!("J2T-05: {used} ms guardian CPU in 1 s after lease EOF; no spin");
    assert!(holder.alive(), "the fault keeps the holder alive");
    assert!(!job.settled(), "never settled while an escapee lives");
    assert_eq!(
        job.1.0.lock().unwrap().ended,
        [false, false],
        "no capture EOF while the holder keeps the pipe"
    );
    // The holder is frozen: it no longer writes the project.
    let before = holder.beats();
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(holder.beats(), before, "a frozen escapee wrote the project");
    // The test removes its own fixture: re-check its identity, then kill.
    assert!(holder.alive());
    // SAFETY: the PID's start time was just re-verified; test-owned process.
    assert_eq!(unsafe { libc::kill(holder.pid, libc::SIGKILL) }, 0);
    let (result, ended, unconfirmed) = finish(job);
    assert!(!result.cleanup_confirmed, "late settlement: {result:?}");
    assert_eq!(unconfirmed, Some(UnconfirmedReason::EscapeeUnterminated));
    assert_eq!(result.escapees, 1);
    assert_eq!(result.exit_code, Some(0));
    assert_eq!(ended, [true, true], "EOF after the holder is gone");
    assert!(!holder.alive());
    println!(
        "EC-I8 EC-I3: undying pipe holder kept the job unconfirmed; late settlement after it ended"
    );
}

/// EC-D2 integration: unverifiable scans report cleanup_unconfirmed even
/// though the group and pipes settled; a later verified scan settles late
/// without confirming cleanup, and the scratch is released.
fn unverifiable_scan(home: &Path) {
    let (job, _) = job(home, "printf done", Some("unverifiable:5"));
    let (result, ended, unconfirmed) = finish(job);
    assert!(!result.cleanup_confirmed, "{result:?}");
    assert_eq!(unconfirmed, Some(UnconfirmedReason::EscapeeUnterminated));
    assert_eq!(result.escapees, 0);
    assert_eq!(result.exit_code, Some(0));
    assert_eq!(ended, [true, true]);
    println!(
        "EC-D2: unverifiable scans kept cleanup unconfirmed; late settlement released scratch"
    );
}
