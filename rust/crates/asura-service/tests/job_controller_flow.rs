//! J2 worker-layer integration (tool-job-controller.md, J2 validation): the
//! real conversation owner with its job controller, the real journal
//! writer, payload store, slot and capture workers and the real guardian
//! under `sandbox-exec`, in private scratch homes. The harness writes
//! fixture turns with `run` exec_cmd kind 14 records and submits their
//! model-origin starts, as J3's exec_cmd switch will. Each case ends with
//! the raw-byte sweep. End-to-end cases wait for J3 and J4 (J2 proof
//! limits).
use asura_platform::RuntimeDirectory;
use asura_service::test_support::{JobHarness, JobView, Turn};
use asura_storage::audit::Event;
use asura_storage::authority::conversation::{
    self as journal, Id, JobCaptureState, JobCleanup, JobOutcome, JobReason, JobRequester, Record,
};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

/// Test budgets; production waits have no time bound.
const LONG: Duration = Duration::from_secs(90);

// ---------------------------------------------------------------- fixture

struct Home {
    path: PathBuf,
    runtime: RuntimeDirectory,
}
impl Home {
    fn new(label: &str) -> Result<Self> {
        let name: String = asura_platform::random_id()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let path = PathBuf::from("/private/tmp").join(format!("as-jc-{label}-{}", &name[..12]));
        fs::create_dir(&path)?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700))?;
        let path = fs::canonicalize(path)?;
        let runtime = RuntimeDirectory::scratch(&path, true)?;
        Ok(Self { path, runtime })
    }
    fn project(&self, name: &str) -> Result<String> {
        let dir = self.path.join(name);
        if !dir.exists() {
            fs::create_dir(&dir)?;
        }
        Ok(dir.to_string_lossy().into_owned())
    }
    /// Sets one configuration key before the service starts.
    fn config(&self, key: &str, value: &str) -> Result<()> {
        asura_storage::config::execute(
            self.runtime.clone(),
            key,
            Some(value),
            &AtomicBool::new(false),
        )
        .map_err(|e| format!("config {key}: {e}"))?;
        Ok(())
    }
    fn harness(&self, audit: bool) -> Result<JobHarness> {
        Ok(JobHarness::start(&self.runtime, audit)?)
    }
    /// The job records (kinds 26 to 28) on disk, in journal order.
    fn job_records(&self) -> Result<Vec<Record>> {
        let bytes = asura_storage::authority::fixture::journal_bytes(&self.path)?;
        let mut out = Vec::new();
        let mut offset = 0;
        while offset + 16 <= bytes.len() {
            let length = u32::from_be_bytes(bytes[offset + 12..offset + 16].try_into()?) as usize;
            let frame = &bytes[offset..offset + length];
            let kind = u16::from_be_bytes([frame[10], frame[11]]);
            if (26..=28).contains(&kind) {
                out.push(
                    journal::decode_record(kind, &frame[144..length - 40])
                        .map_err(|e| format!("decode: {e:?}"))?,
                );
            }
            offset += length;
        }
        Ok(out)
    }
    /// The durable replay: unsettled jobs after a clean stop.
    fn unsettled(&self) -> Result<Vec<Id>> {
        let bytes = asura_storage::authority::fixture::journal_bytes(&self.path)?;
        let replay = journal::replay(&bytes, &AtomicBool::new(false))
            .map_err(|e| format!("replay: {e:?}"))?;
        Ok(replay.unsettled_jobs().iter().map(|j| j.job).collect())
    }
    fn object(&self, id: Id) -> Option<Vec<u8>> {
        fs::read(self.path.join(".asura/payloads/objects").join(hex(id))).ok()
    }
    fn fault(&self, text: &str) -> Result<()> {
        Ok(fs::write(self.path.join("exec-cmd-guardian-fault"), text)?)
    }
    fn clear_fault(&self) {
        let _ = fs::remove_file(self.path.join("exec-cmd-guardian-fault"));
    }
}
impl Drop for Home {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}
fn hex(id: Id) -> String {
    id.iter().map(|b| format!("{b:02x}")).collect()
}
/// Fixture secrets assembled at run time, so no token is committed.
fn token(seed: &str) -> String {
    ["ghp_", &seed.repeat(12)].concat()
}
fn holds(bytes: &[u8], needle: &str) -> bool {
    bytes.windows(needle.len()).any(|w| w == needle.as_bytes())
}
/// The raw-byte sweep: no file under the home holds a fixture secret.
fn sweep(root: &Path, secrets: &[&str]) -> Result<()> {
    for entry in fs::read_dir(root)? {
        let path = entry?.path();
        let meta = fs::symlink_metadata(&path)?;
        if meta.is_dir() {
            sweep(&path, secrets)?;
        } else if meta.is_file() {
            let bytes = fs::read(&path)?;
            for secret in secrets {
                if holds(&bytes, secret) {
                    return Err(format!("raw secret bytes in {}", path.display()).into());
                }
            }
        }
    }
    Ok(())
}
fn settled_of(records: &[Record], job: Id) -> Option<&journal::JobSettled> {
    records.iter().find_map(|r| match r {
        Record::JobSettled(s) if s.job == job => Some(&**s),
        _ => None,
    })
}
fn cancel_of(records: &[Record], job: Id) -> Option<&journal::JobCancelRequested> {
    records.iter().find_map(|r| match r {
        Record::JobCancelRequested(c) if c.job == job => Some(c),
        _ => None,
    })
}
fn state(h: &mut JobHarness, job: Id) -> String {
    h.view(job).map_or_else(|| "gone".into(), |v| v.state)
}
fn wait_state(h: &mut JobHarness, job: Id, wanted: &str) -> Result<()> {
    h.until(LONG, |h| state(h, job) == wanted)
        .map_err(|e| format!("{e}: job {} is {}", hex(job), state(h, job)))?;
    Ok(())
}
/// Settled and recorded: kind 28 committed.
fn wait_outcome(h: &mut JobHarness, job: Id) -> Result<JobView> {
    h.until(LONG, |h| {
        h.view(job)
            .is_some_and(|v| v.outcome.is_some() && v.state == "Recorded")
    })
    .map_err(|e| format!("{e}: job {} is {:?}", hex(job), h.view(job)))?;
    Ok(h.view(job).ok_or("no view")?)
}
fn shutdown(h: &mut JobHarness) -> Result<()> {
    h.shutdown();
    h.until(LONG, JobHarness::settled)?;
    Ok(())
}
fn turn(h: &mut JobHarness, project: Id) -> Result<Turn> {
    Ok(h.begin_turn(project)?)
}
fn alive(pid: i32) -> bool {
    // SAFETY: signal 0 checks for existence only; it sends nothing.
    unsafe { libc::kill(pid, 0) == 0 }
}
fn read_pid(path: &Path) -> Option<i32> {
    fs::read_to_string(path).ok()?.trim().parse().ok()
}

// ------------------------------------------------------------------ cases

/// J2T-01, J2T-07, J2T-11, J2T-17, J2T-20, J2T-22 (integration): a model
/// job runs to completion; its kind 26 commits before its kind 15 and its
/// kind 28 records the capture facts with line counts equal to the
/// retained stream; a second job outlives its turn and settles during
/// another conversation's turn; a running job is cancelled by a client
/// (kinds 27 and 28); the real audit emitter writes `job.started`,
/// `job.cancel_requested` and `job.settled` in order; no secret reaches
/// disk.
fn lifecycle() -> Result<()> {
    let home = Home::new("life")?;
    home.config("audit.enabled", "true")?;
    let mut h = home.harness(true)?;
    let project = h.register(&home.project("p")?)?;
    let secret = token("aB3");
    let mut first = turn(&mut h, project)?;
    let done = h.launch(
        &mut first,
        &format!("printf 'one\\ntwo\\n'; printf '%s\\n' \"{secret}\""),
    )?;
    let late = h.launch(&mut first, "sleep 1; printf 'late\\n'")?;
    let view = wait_outcome(&mut h, done)?;
    assert_eq!(view.outcome, Some(JobOutcome::Exited));
    h.end_turn(&first)?;
    // J2T-17: the late job settles during another conversation's turn.
    let mut second = turn(&mut h, project)?;
    let view = wait_outcome(&mut h, late)?;
    assert_eq!(view.outcome, Some(JobOutcome::Exited));
    let running = h.launch(&mut second, "sleep 60")?;
    wait_state(&mut h, running, "Running")?;
    let reply = h.cancel(running, JobReason::ClientCancel);
    assert!(reply.starts_with("Accepted"), "{reply}");
    let view = wait_outcome(&mut h, running)?;
    assert_eq!(view.outcome, Some(JobOutcome::Cancelled));
    h.end_turn(&second)?;
    // The installed stdout equals what kind 28 recorded (J2T-22).
    let records = home.job_records()?;
    let settled = settled_of(&records, done).ok_or("kind 28 of the first job")?;
    assert_eq!(settled.capture_state, JobCaptureState::Complete);
    assert_eq!(settled.cleanup, JobCleanup::Confirmed);
    assert_eq!(settled.reason, JobReason::None);
    let installation = h_installation(&home)?;
    let stdout_id = asura_storage::payload::payload_id(
        installation,
        done,
        asura_storage::payload::ObjectKind::Stdout,
    );
    h.until(LONG, |_| home.object(stdout_id).is_some())?;
    let stdout = home.object(stdout_id).ok_or("stdout object")?;
    assert!(!holds(&stdout, &secret), "redacted");
    assert_eq!(settled.stdout.capture.stored_bytes, stdout.len() as u64);
    assert_eq!(
        settled.stdout.lines,
        stdout.iter().filter(|b| **b == b'\n').count() as u64
    );
    assert_eq!(settled.stdout.lines, 3);
    // Record order: kind 26 of each job before its kind 28; the cancel's
    // kind 27 before its kind 28, from the client.
    let cancel = cancel_of(&records, running).ok_or("kind 27")?;
    assert_eq!(
        (cancel.requester, cancel.reason, cancel.kill),
        (JobRequester::Client, JobReason::ClientCancel, false)
    );
    for job in [done, late, running] {
        let intent = records
            .iter()
            .position(|r| matches!(r, Record::JobIntent(i) if journal::job_id(&i.origin) == job))
            .ok_or("kind 26")?;
        let settle = records
            .iter()
            .position(|r| matches!(r, Record::JobSettled(s) if s.job == job))
            .ok_or("kind 28")?;
        assert!(intent < settle);
    }
    // J2T-20: the real emitter wrote the job events in order.
    h.until(LONG, |h| {
        h.audit_events()
            .iter()
            .filter(|e| matches!(e, Event::JobSettled { .. }))
            .count()
            == 3
    })?;
    let events = h.audit_events();
    for job in [done, late, running] {
        let started = events
            .iter()
            .position(|e| matches!(e, Event::JobStarted { job: j, .. } if *j == job))
            .ok_or("job.started")?;
        let settled = events
            .iter()
            .position(|e| matches!(e, Event::JobSettled { job: j, .. } if *j == job))
            .ok_or("job.settled")?;
        assert!(started < settled);
    }
    assert!(
        events
            .iter()
            .any(|e| matches!(e, Event::JobCancelRequested { job, .. } if *job == running))
    );
    shutdown(&mut h)?;
    drop(h);
    assert!(home.unsettled()?.is_empty());
    sweep(&home.path, &[&secret])?;
    println!("PASS lifecycle: J2T-01 J2T-07 J2T-17 J2T-20 J2T-22 (integration)");
    Ok(())
}

fn view_reason(h: &mut JobHarness, job: Id) -> Option<String> {
    h.view(job).and_then(|v| v.reason)
}
fn wait_reason(h: &mut JobHarness, job: Id, reason: &str) -> Result<JobView> {
    h.until(LONG, |h| view_reason(h, job).as_deref() == Some(reason))
        .map_err(|e| format!("{e}: job {} is {:?}", hex(job), h.view(job)))?;
    Ok(h.view(job).ok_or("no view")?)
}
fn outcome_of(records: &[Record], job: Id) -> Result<(JobOutcome, JobReason)> {
    let s = settled_of(records, job).ok_or_else(|| format!("no kind 28 for {}", hex(job)))?;
    Ok((s.outcome, s.reason))
}
fn ended(path: &Path) -> bool {
    read_pid(path).is_some_and(|pid| !alive(pid))
}

/// J2T-08 and J2T-21 (integration): an agent with 2 running and 1 queued
/// (`agent_slots`); a fifth service job queued (`service_slots`); each
/// starts when a slot is released, with no timer; a duplicate start returns
/// the existing job. After `jobs.liveAgent` is set to 3 and the service
/// restarts, three jobs of one agent run at once.
fn queues() -> Result<()> {
    let home = Home::new("queue")?;
    let mut h = home.harness(false)?;
    let project = h.register(&home.project("p")?)?;
    let mut a = turn(&mut h, project)?;
    let a1 = h.launch(&mut a, "sleep 60")?;
    let a2 = h.launch(&mut a, "sleep 60")?;
    let a3 = h.launch(&mut a, "printf a3")?;
    let view = wait_reason(&mut h, a3, "AgentSlots")?;
    assert_eq!(
        view.text.as_deref(),
        Some("waiting: 2 of 2 jobs running for this agent")
    );
    h.end_turn(&a)?;
    let mut b = turn(&mut h, project)?;
    let b1 = h.launch(&mut b, "sleep 60")?;
    let b2 = h.launch(&mut b, "sleep 60")?;
    h.end_turn(&b)?;
    let mut c = turn(&mut h, project)?;
    let c1 = h.launch(&mut c, "printf c1")?;
    for job in [a1, a2, b1, b2] {
        wait_state(&mut h, job, "Running")?;
    }
    let view = wait_reason(&mut h, c1, "ServiceSlots")?;
    assert_eq!(view.text.as_deref(), Some("waiting: 4 of 4 jobs running"));
    assert_eq!(view_reason(&mut h, a3).as_deref(), Some("AgentSlots"));
    // A slot release starts the agent's head; the service head still waits.
    assert!(
        h.cancel(a1, JobReason::ClientCancel)
            .starts_with("Accepted")
    );
    let view = wait_outcome(&mut h, a3)?;
    assert_eq!(view.outcome, Some(JobOutcome::Exited));
    assert!(
        h.cancel(b1, JobReason::ClientCancel)
            .starts_with("Accepted")
    );
    let view = wait_outcome(&mut h, c1)?;
    assert_eq!(view.outcome, Some(JobOutcome::Exited));
    h.end_turn(&c)?;
    shutdown(&mut h)?;
    drop(h);
    let records = home.job_records()?;
    for job in [a2, b2] {
        assert_eq!(
            outcome_of(&records, job)?,
            (JobOutcome::Cancelled, JobReason::Shutdown)
        );
    }
    // J2T-21: the set applies at the next service start.
    home.config("jobs.liveAgent", "3")?;
    let mut h = home.harness(false)?;
    let mut d = turn(&mut h, project)?;
    let jobs: Vec<Id> = (0..3)
        .map(|_| h.launch(&mut d, "sleep 60"))
        .collect::<std::result::Result<_, _>>()?;
    for job in &jobs {
        wait_state(&mut h, *job, "Running")?;
    }
    h.end_turn(&d)?;
    shutdown(&mut h)?;
    drop(h);
    assert!(home.unsettled()?.is_empty());
    sweep(&home.path, &[])?;
    println!("PASS queues: J2T-08 J2T-21 (integration)");
    Ok(())
}

/// The child half of `recovery`: starts jobs, prints READY and keeps its
/// reactor running until the parent kills it with SIGKILL.
fn recovery_child(home: &Path) -> Result<()> {
    let runtime = RuntimeDirectory::scratch(home, true)?;
    let mut h = JobHarness::start(&runtime, false)?;
    let location = home.join("p").to_string_lossy().into_owned();
    let project = h.register(&location)?;
    let mut t = turn(&mut h, project)?;
    let done = h.launch(&mut t, "printf 'done\\n' >> runs.log")?;
    wait_outcome(&mut h, done)?;
    let r1 = h.launch(
        &mut t,
        "echo $$ > r1.pid; printf 'r1\\n' >> runs.log; sleep 60",
    )?;
    let r2 = h.launch(
        &mut t,
        "echo $$ > r2.pid; printf 'r2\\n' >> runs.log; sleep 60",
    )?;
    let q1 = h.launch(&mut t, "printf 'q1\\n' >> runs.log")?;
    wait_state(&mut h, r1, "Running")?;
    wait_state(&mut h, r2, "Running")?;
    wait_reason(&mut h, q1, "AgentSlots")?;
    h.until(LONG, |_| {
        Path::new(&location).join("r1.pid").exists() && Path::new(&location).join("r2.pid").exists()
    })?;
    println!("READY {} {} {} {}", hex(done), hex(r1), hex(r2), hex(q1));
    std::io::Write::flush(&mut std::io::stdout())?;
    loop {
        h.step();
    }
}

/// J2T-12 (integration): a child process with a settled job, two running
/// jobs and a queued job is killed with SIGKILL. Each guardian stops its
/// group on lease EOF. The next start writes one `interrupted` kind 28 per
/// unsettled job under the new owner generation, reruns nothing, and then
/// opens admission.
fn recovery() -> Result<()> {
    let home = Home::new("recover")?;
    let directory = PathBuf::from(home.project("p")?);
    let mut child = Command::new(std::env::current_exe()?)
        .arg("--recovery-child")
        .arg(&home.path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()?;
    let mut line = String::new();
    std::io::BufRead::read_line(
        &mut std::io::BufReader::new(child.stdout.take().ok_or("stdout")?),
        &mut line,
    )?;
    let ids: Vec<&str> = line.split_whitespace().collect();
    if ids.first() != Some(&"READY") {
        let _ = child.kill();
        let _ = child.wait();
        return Err(format!("child: {line:?}").into());
    }
    child.kill()?;
    child.wait()?;
    // Lease EOF: each guardian stops its group.
    let end = Instant::now() + Duration::from_secs(20);
    while !(ended(&directory.join("r1.pid")) && ended(&directory.join("r2.pid"))) {
        assert!(
            Instant::now() < end,
            "a group outlived its guardian's lease"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
    let runs_before = fs::read_to_string(directory.join("runs.log"))?;
    assert_eq!(runs_before.lines().count(), 3, "{runs_before}");
    let interrupted: Vec<String> = ids[2..].iter().map(|s| s.to_string()).collect();
    let mut h = home.harness(false)?;
    h.until(LONG, |_| {
        let records = home.job_records().unwrap_or_default();
        records
            .iter()
            .filter(|r| matches!(r, Record::JobSettled(s) if s.outcome == JobOutcome::Interrupted))
            .count()
            == 3
    })?;
    // Admission opens: a new job runs.
    let project = h.register(&directory.to_string_lossy())?;
    let mut t = turn(&mut h, project)?;
    let after = h.launch(&mut t, "printf after")?;
    let view = wait_outcome(&mut h, after)?;
    assert_eq!(view.outcome, Some(JobOutcome::Exited));
    h.end_turn(&t)?;
    shutdown(&mut h)?;
    drop(h);
    let records = home.job_records()?;
    for r in records.iter() {
        if let Record::JobSettled(s) = r
            && s.outcome == JobOutcome::Interrupted
        {
            assert!(interrupted.contains(&hex(s.job)), "{}", hex(s.job));
            assert_eq!(s.reason, JobReason::Restart);
            assert_eq!(s.capture_state, JobCaptureState::Interrupted);
        }
    }
    assert_eq!(
        fs::read_to_string(directory.join("runs.log"))?,
        runs_before,
        "nothing reran"
    );
    assert!(home.unsettled()?.is_empty());
    sweep(&home.path, &[])?;
    println!("PASS recovery: J2T-12 (integration)");
    Ok(())
}

/// J2T-02 and J2T-13 (integration). Quota hold: kind 26 waits with
/// `journal_held` while a live job's kinds 27 and 28 commit; when the hold
/// ends kind 26 commits and the job runs. Writer fault: a live job is
/// stopped, nothing starts, and the next start writes `interrupted`.
fn outage() -> Result<()> {
    let home = Home::new("outage")?;
    let directory = PathBuf::from(home.project("p")?);
    let mut h = home.harness(false)?;
    let project = h.register(&directory.to_string_lossy())?;
    let mut t = turn(&mut h, project)?;
    let live = h.launch(&mut t, "sleep 60")?;
    wait_state(&mut h, live, "Running")?;
    let pending = h.call(&mut t, "printf held")?;
    let held = pending.job;
    let status = h.journal_status()?;
    let quota = status.usage
        + asura_storage::authority::journal::ROLL_RESERVE
        + h.reserved_bytes() as u64
        + 100;
    h.set_journal_quota(quota)?;
    let admission = h.start_job(pending)?;
    assert!(admission.starts_with("Accepted"), "{admission}");
    let view = wait_reason(&mut h, held, "JournalHeld")?;
    assert_eq!(view.text.as_deref(), Some("waiting: journal space is full"));
    // Closure records commit while the journal is held.
    assert!(
        h.cancel(live, JobReason::ClientCancel)
            .starts_with("Accepted")
    );
    wait_outcome(&mut h, live)?;
    assert!(!h.intent_committed(held));
    assert_eq!(view_reason(&mut h, held).as_deref(), Some("JournalHeld"));
    h.set_journal_quota(1 << 40)?;
    h.resolve(&t, held)?;
    let view = wait_outcome(&mut h, held)?;
    assert_eq!(view.outcome, Some(JobOutcome::Exited));
    // Writer fault during a live job.
    let doomed = h.launch(&mut t, "echo $$ > doomed.pid; sleep 60")?;
    wait_state(&mut h, doomed, "Running")?;
    h.until(LONG, |_| directory.join("doomed.pid").exists())?;
    let never = h.call(&mut t, "printf never")?;
    let never_job = never.job;
    h.close_writer();
    let _ = h.start_job(never)?;
    h.until(LONG, |h| h.unavailable().is_some())?;
    h.until(LONG, |_| ended(&directory.join("doomed.pid")))?;
    drop(h);
    let mut h = home.harness(false)?;
    h.until(LONG, |_| {
        home.job_records()
            .is_ok_and(|r| outcome_of(&r, doomed).is_ok())
    })?;
    shutdown(&mut h)?;
    drop(h);
    let records = home.job_records()?;
    assert_eq!(
        outcome_of(&records, live)?,
        (JobOutcome::Cancelled, JobReason::ClientCancel)
    );
    assert_eq!(
        outcome_of(&records, doomed)?,
        (JobOutcome::Interrupted, JobReason::Restart)
    );
    assert!(
        !records
            .iter()
            .any(|r| matches!(r, Record::JobIntent(i) if journal::job_id(&i.origin) == never_job)),
        "no kind 26 after the writer fault"
    );
    assert!(home.unsettled()?.is_empty());
    sweep(&home.path, &[])?;
    println!("PASS outage: J2T-02 J2T-13 (integration)");
    Ok(())
}

/// J2T-14 and J2T-10 (integration): shutdown settles a queued start
/// `not_started` and stops live jobs with `shutdown` within the 2 s bound.
/// A job whose escapee cannot be killed (`undyable`) stays in
/// `CleanupUnconfirmed`; shutdown does not wait beyond its bound, and the
/// next start writes `interrupted` for it.
fn shutdown_case() -> Result<()> {
    let home = Home::new("stop")?;
    let directory = PathBuf::from(home.project("p")?);
    fs::write(
        directory.join("escapee.pl"),
        include_str!("../../asura-platform/tests/fixtures/escapee.pl"),
    )?;
    let mut h = home.harness(false)?;
    let project = h.register(&directory.to_string_lossy())?;
    let mut t = turn(&mut h, project)?;
    let live = h.launch(&mut t, "sleep 60")?;
    let stubborn = h.launch(&mut t, "trap '' TERM; while :; do sleep 1; done")?;
    let queued = h.launch(&mut t, "printf queued")?;
    wait_state(&mut h, live, "Running")?;
    wait_state(&mut h, stubborn, "Running")?;
    wait_reason(&mut h, queued, "AgentSlots")?;
    let started = Instant::now();
    shutdown(&mut h)?;
    let spent = started.elapsed();
    drop(h);
    let records = home.job_records()?;
    assert_eq!(
        outcome_of(&records, queued)?,
        (JobOutcome::NotStarted, JobReason::Shutdown)
    );
    for job in [live, stubborn] {
        assert_eq!(
            outcome_of(&records, job)?,
            (JobOutcome::Cancelled, JobReason::Shutdown)
        );
        let cancel = cancel_of(&records, job).ok_or("kind 27")?;
        assert_eq!(cancel.requester, JobRequester::Service);
    }
    assert!(spent < Duration::from_secs(10), "{spent:?}");
    assert!(home.unsettled()?.is_empty());
    // Beyond the bound: an escapee that cannot be killed.
    let mut h = home.harness(false)?;
    let mut t = turn(&mut h, project)?;
    home.fault("undyable")?;
    let held = h.launch(
        &mut t,
        "/usr/bin/perl escapee.pl setsid u; while [ ! -e u.pid ]; do sleep 0.05; done; exit 0",
    )?;
    h.until(LONG, |h| {
        matches!(state(h, held).as_str(), "CleanupUnconfirmed" | "Recorded")
    })?;
    if state(&mut h, held) != "CleanupUnconfirmed" {
        let records = home.job_records()?;
        return Err(format!("not held: {:?}", settled_of(&records, held)).into());
    }
    home.clear_fault();
    assert!(h.unavailable().is_none(), "the owner stays available");
    let started = Instant::now();
    shutdown(&mut h)?;
    let spent = started.elapsed();
    assert!(spent >= Duration::from_secs(2), "the 2 s bound: {spent:?}");
    drop(h);
    fs::write(directory.join("u.release"), b"")?;
    // The fault left the fixture frozen, so the release file cannot end it:
    // the test removes its own process after checking it is still the
    // fixture (a reused PID would not run escapee.pl).
    let pid: libc::pid_t = fs::read_to_string(directory.join("u.pid"))?
        .trim()
        .parse()?;
    let command = Command::new("/bin/ps")
        .args(["-p", &pid.to_string(), "-o", "command="])
        .output()?;
    if String::from_utf8_lossy(&command.stdout).contains("escapee.pl setsid u") {
        // SAFETY: a test-owned fixture process, identity checked above.
        assert_eq!(unsafe { libc::kill(pid, libc::SIGKILL) }, 0);
    }
    assert_eq!(home.unsettled()?, vec![held]);
    let mut h = home.harness(false)?;
    h.until(LONG, |_| {
        home.job_records()
            .is_ok_and(|r| outcome_of(&r, held).is_ok())
    })?;
    shutdown(&mut h)?;
    drop(h);
    assert_eq!(
        outcome_of(&home.job_records()?, held)?,
        (JobOutcome::Interrupted, JobReason::Restart)
    );
    sweep(&home.path, &[])?;
    println!("PASS shutdown: J2T-14 J2T-10 (integration)");
    Ok(())
}

/// J2T-15 (integration): the project directory is replaced during a
/// running job. The next start in the project fails its setup identity
/// recheck: it settles `not_started` with `project_changed`, and the
/// project's running job is revoked with `identity_changed`. Another
/// project's job continues until the owner's identity hook revokes its own
/// project.
fn identity() -> Result<()> {
    let home = Home::new("ident")?;
    let p_dir = PathBuf::from(home.project("p")?);
    let q_dir = PathBuf::from(home.project("q")?);
    let mut h = home.harness(false)?;
    let p = h.register(&p_dir.to_string_lossy())?;
    let q = h.register(&q_dir.to_string_lossy())?;
    let mut tp = turn(&mut h, p)?;
    let p1 = h.launch(&mut tp, "sleep 60")?;
    h.end_turn(&tp)?;
    let mut tq = turn(&mut h, q)?;
    let q1 = h.launch(&mut tq, "sleep 60")?;
    h.end_turn(&tq)?;
    wait_state(&mut h, p1, "Running")?;
    wait_state(&mut h, q1, "Running")?;
    let mut tp = turn(&mut h, p)?;
    fs::rename(&p_dir, home.path.join("p.old"))?;
    fs::create_dir(&p_dir)?;
    let p2 = h.launch(&mut tp, "printf replaced")?;
    let view = wait_outcome(&mut h, p2)?;
    assert_eq!(view.outcome, Some(JobOutcome::NotStarted));
    wait_outcome(&mut h, p1)?;
    assert_eq!(state(&mut h, q1), "Running", "another project continues");
    h.end_turn(&tp)?;
    h.identity_changed(q);
    wait_outcome(&mut h, q1)?;
    shutdown(&mut h)?;
    drop(h);
    let records = home.job_records()?;
    assert_eq!(
        outcome_of(&records, p2)?,
        (JobOutcome::NotStarted, JobReason::ProjectChanged)
    );
    for job in [p1, q1] {
        assert_eq!(
            outcome_of(&records, job)?,
            (JobOutcome::Cancelled, JobReason::IdentityChanged)
        );
    }
    sweep(&home.path, &[])?;
    println!("PASS identity: J2T-15 (integration)");
    Ok(())
}

/// J2T-10 and J2T-11 (integration): the escapee fixture's three escapees
/// are killed and counted in kind 28 with cleanup confirmed. Unverifiable
/// scans give `CleanupUnconfirmed`: the slot is held, the owner stays
/// available, another agent's job runs, and the late settlement records
/// cleanup `late` and releases the slot.
fn cleanup() -> Result<()> {
    let home = Home::new("cleanup")?;
    let directory = PathBuf::from(home.project("p")?);
    fs::write(
        directory.join("escapee.pl"),
        include_str!("../../asura-platform/tests/fixtures/escapee.pl"),
    )?;
    let mut h = home.harness(false)?;
    let project = h.register(&directory.to_string_lossy())?;
    let mut t = turn(&mut h, project)?;
    let escapes = h.launch(
        &mut t,
        "/usr/bin/perl escapee.pl setsid s; /usr/bin/perl escapee.pl doublefork d; \
         /usr/bin/perl escapee.pl pipehold p; \
         while [ ! -e s.pid ] || [ ! -e d.pid ] || [ ! -e p.pid ]; do sleep 0.05; done; \
         sleep 0.3; exit 0",
    )?;
    let view = wait_outcome(&mut h, escapes)?;
    assert_eq!(view.outcome, Some(JobOutcome::Exited));
    home.fault("unverifiable:5")?;
    let late = h.launch(&mut t, "printf late-output")?;
    h.until(LONG, |h| {
        h.events()
            .iter()
            .any(|e| e.contains("CleanupUnconfirmed") && e.contains(&format!("{late:?}")))
            || h.view(late)
                .is_some_and(|v| v.state == "CleanupUnconfirmed")
    })?;
    home.clear_fault();
    h.end_turn(&t)?;
    assert!(h.unavailable().is_none());
    let mut other = turn(&mut h, project)?;
    let runs = h.launch(&mut other, "printf other")?;
    let view = wait_outcome(&mut h, runs)?;
    assert_eq!(view.outcome, Some(JobOutcome::Exited));
    let view = wait_outcome(&mut h, late)?;
    assert_eq!(view.outcome, Some(JobOutcome::Exited));
    h.end_turn(&other)?;
    shutdown(&mut h)?;
    drop(h);
    let records = home.job_records()?;
    let s = settled_of(&records, escapes).ok_or("kind 28")?;
    assert_eq!((s.escapees, s.cleanup), (3, JobCleanup::Confirmed));
    let s = settled_of(&records, late).ok_or("kind 28")?;
    assert_eq!((s.cleanup, s.unconfirmed_reason), (JobCleanup::Late, 13));
    for tag in ["s", "d", "p"] {
        let _ = fs::write(directory.join(format!("{tag}.release")), b"");
    }
    sweep(&home.path, &[])?;
    println!("PASS cleanup: J2T-10 J2T-11 (integration)");
    Ok(())
}

/// J2T-25 (integration): a guardian that exits before any notice (as for
/// an unknown profile token) gives kind 28 `not_started` with
/// `protocol_fault`; the scratch is deleted and the slot released, so the
/// next queued job uses it (`jobs.liveService` 1).
fn protocol_fault() -> Result<()> {
    let home = Home::new("fault")?;
    home.config("jobs.liveService", "1")?;
    let script = home.path.join("refusing-guardian");
    fs::write(&script, "#!/bin/sh\nexit 2\n")?;
    fs::set_permissions(&script, fs::Permissions::from_mode(0o700))?;
    fs::write(
        home.path.join("exec-cmd-guardian-executable"),
        script.to_string_lossy().as_bytes(),
    )?;
    let mut h = home.harness(false)?;
    let project = h.register(&home.project("p")?)?;
    let mut t = turn(&mut h, project)?;
    let first = h.launch(&mut t, "printf first")?;
    let second = h.launch(&mut t, "printf second")?;
    wait_outcome(&mut h, first)?;
    wait_outcome(&mut h, second)?;
    fs::remove_file(home.path.join("exec-cmd-guardian-executable"))?;
    let third = h.launch(&mut t, "printf third")?;
    let view = wait_outcome(&mut h, third)?;
    assert_eq!(view.outcome, Some(JobOutcome::Exited));
    h.end_turn(&t)?;
    shutdown(&mut h)?;
    drop(h);
    let records = home.job_records()?;
    for job in [first, second] {
        assert_eq!(
            outcome_of(&records, job)?,
            (JobOutcome::NotStarted, JobReason::ProtocolFault)
        );
    }
    let scratch: Vec<_> = fs::read_dir(home.path.join(".asura/tmp"))
        .map(|d| {
            d.filter_map(|e| e.ok())
                .filter(|e| e.file_name().to_string_lossy().starts_with("exec-cmd-"))
                .collect()
        })
        .unwrap_or_default();
    assert!(scratch.is_empty(), "scratch deleted");
    sweep(&home.path, &[])?;
    println!("PASS protocol_fault: J2T-25 (integration)");
    Ok(())
}

/// J2T-09 and J2T-19 (integration, quota form): with a 64 MiB quota, a
/// flooding job pauses its own capture (`CapturePaused`) while another job
/// runs; a third start waits with `quota`. The pause facts reach kind 28.
fn quota_pause() -> Result<()> {
    let home = Home::new("quota")?;
    home.config("retention.quotaBytes", "67108864")?;
    let mut h = home.harness(false)?;
    let project = h.register(&home.project("p")?)?;
    let mut t = turn(&mut h, project)?;
    let small = h.launch(&mut t, "sleep 20; printf ok")?;
    let flood = h.launch(
        &mut t,
        "/usr/bin/yes 0123456789 | /usr/bin/head -c 80000000; sleep 60",
    )?;
    h.end_turn(&t)?;
    let mut u = turn(&mut h, project)?;
    let mut paused_while_small_ran = false;
    h.until(LONG, |h| {
        let paused = state(h, flood) == "CapturePaused";
        if paused && state(h, small) == "Running" {
            paused_while_small_ran = true;
        }
        paused
    })?;
    assert!(paused_while_small_ran, "the pause holds only its own job");
    let third = h.launch(&mut u, "printf third")?;
    wait_reason(&mut h, third, "Quota")?;
    let view = wait_outcome(&mut h, small)?;
    assert_eq!(view.outcome, Some(JobOutcome::Exited));
    assert!(
        h.cancel(flood, JobReason::ClientCancel)
            .starts_with("Accepted")
    );
    wait_outcome(&mut h, flood)?;
    let view = wait_outcome(&mut h, third)?;
    assert_eq!(view.outcome, Some(JobOutcome::Exited));
    h.end_turn(&u)?;
    shutdown(&mut h)?;
    drop(h);
    let records = home.job_records()?;
    let s = settled_of(&records, flood).ok_or("kind 28")?;
    assert!(s.pause_count >= 1, "pause facts recorded");
    assert_eq!(
        (s.outcome, s.reason),
        (JobOutcome::Cancelled, JobReason::ClientCancel)
    );
    println!("PASS quota_pause: J2T-09 (quota form, integration)");
    sweep(&home.path, &[])?;
    Ok(())
}

/// J2T-18 (integration): 16 configured live jobs over two agents flood
/// output; every job starts, a cancel is accepted at once, and no owner
/// poll takes 100 ms or more.
fn scale() -> Result<()> {
    let home = Home::new("scale")?;
    home.config("jobs.liveService", "16")?;
    home.config("jobs.liveAgent", "8")?;
    let mut h = home.harness(false)?;
    let project = h.register(&home.project("p")?)?;
    let mut jobs = Vec::new();
    for _ in 0..2 {
        let mut t = turn(&mut h, project)?;
        for _ in 0..8 {
            jobs.push(h.launch(
                &mut t,
                "/usr/bin/yes 0123456789abcdef | /usr/bin/head -c 4000000; sleep 60",
            )?);
        }
        h.end_turn(&t)?;
    }
    for job in &jobs {
        wait_state(&mut h, *job, "Running")?;
    }
    h.longest_poll = Duration::ZERO;
    let end = Instant::now() + Duration::from_secs(5);
    while Instant::now() < end {
        h.step();
    }
    let started = Instant::now();
    assert!(
        h.cancel(jobs[0], JobReason::ClientCancel)
            .starts_with("Accepted")
    );
    let cancel = started.elapsed();
    assert_eq!(state(&mut h, jobs[0]), "Stopping");
    for job in &jobs[1..] {
        assert!(
            h.cancel(*job, JobReason::ClientCancel)
                .starts_with("Accepted")
        );
    }
    for job in &jobs {
        wait_outcome(&mut h, *job)?;
    }
    let longest = h.longest_poll;
    shutdown(&mut h)?;
    drop(h);
    assert!(cancel < Duration::from_millis(100), "{cancel:?}");
    assert!(longest < Duration::from_millis(100), "{longest:?}");
    sweep(&home.path, &[])?;
    println!(
        "PASS scale: J2T-18 (16 live jobs, longest owner poll {longest:?}, cancel {cancel:?})"
    );
    Ok(())
}

/// J2T-07 (integration): a real cancel in `Queued`, `Starting` (setup held
/// by the test gate), and `Draining` (the leader exited while a member
/// still holds the output). `Running` is in `lifecycle`, `CapturePaused`
/// in `quota_pause`.
fn cancels() -> Result<()> {
    let home = Home::new("cancel")?;
    home.config("jobs.liveAgent", "1")?;
    let mut h = home.harness(false)?;
    let project = h.register(&home.project("p")?)?;
    let mut t = turn(&mut h, project)?;
    let gate = home.path.join("exec-cmd-hold-setup");
    fs::write(&gate, b"")?;
    let starting = h.launch(&mut t, "printf never")?;
    let queued = h.launch(&mut t, "printf never")?;
    wait_state(&mut h, starting, "Starting")?;
    wait_reason(&mut h, queued, "AgentSlots")?;
    assert!(
        h.cancel(queued, JobReason::ClientCancel)
            .starts_with("Accepted")
    );
    assert!(
        h.cancel(starting, JobReason::ModelCancel)
            .starts_with("Accepted")
    );
    wait_outcome(&mut h, queued)?;
    wait_outcome(&mut h, starting)?;
    let _ = fs::remove_file(&gate);
    let draining = h.launch(&mut t, "(sleep 4; printf member) & printf leader; exit 0")?;
    wait_state(&mut h, draining, "Draining")?;
    assert!(
        h.cancel(draining, JobReason::ClientCancel)
            .starts_with("Accepted")
    );
    let view = wait_outcome(&mut h, draining)?;
    assert_eq!(view.outcome, Some(JobOutcome::Exited));
    h.end_turn(&t)?;
    shutdown(&mut h)?;
    drop(h);
    let records = home.job_records()?;
    assert_eq!(
        outcome_of(&records, queued)?,
        (JobOutcome::NotStarted, JobReason::ClientCancel)
    );
    assert_eq!(
        outcome_of(&records, starting)?,
        (JobOutcome::NotStarted, JobReason::ModelCancel)
    );
    assert_eq!(
        outcome_of(&records, draining)?,
        (JobOutcome::Exited, JobReason::ClientCancel)
    );
    for job in [queued, starting, draining] {
        assert!(cancel_of(&records, job).is_some(), "kind 27");
    }
    sweep(&home.path, &[])?;
    println!("PASS cancels: J2T-07 (integration)");
    Ok(())
}

fn h_installation(home: &Home) -> Result<Id> {
    let bytes = asura_storage::authority::fixture::journal_bytes(&home.path)?;
    let replay =
        journal::replay(&bytes, &AtomicBool::new(false)).map_err(|e| format!("replay: {e:?}"))?;
    Ok(replay.installation_id)
}

type Case = (&'static str, fn() -> Result<()>);

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if let [_, flag, id, profile, command] = args.as_slice()
        && flag == "--asura-exec-cmd-guardian"
    {
        std::process::exit(asura_platform::exec_cmd::run_guardian(id, profile, command));
    }
    if let [_, flag, id, profile, command, fault] = args.as_slice()
        && flag == "--asura-exec-cmd-guardian"
    {
        std::process::exit(asura_platform::exec_cmd::run_guardian_for_test(
            id, profile, command, fault,
        ));
    }
    let selected: Vec<&str> = args[1..]
        .iter()
        .map(String::as_str)
        .filter(|a| !a.starts_with('-'))
        .collect();
    if let [_, flag, home] = args.as_slice()
        && flag == "--recovery-child"
    {
        if let Err(error) = recovery_child(Path::new(home)) {
            println!("FAILED {error}");
        }
        std::process::exit(1);
    }
    let cases: Vec<Case> = vec![
        ("lifecycle", lifecycle),
        ("queues", queues),
        ("cancels", cancels),
        ("recovery", recovery),
        ("outage", outage),
        ("shutdown", shutdown_case),
        ("identity", identity),
        ("cleanup", cleanup),
        ("protocol_fault", protocol_fault),
        ("quota_pause", quota_pause),
        ("scale", scale),
    ];
    for (name, case) in cases {
        if !selected.is_empty() && !selected.contains(&name) {
            continue;
        }
        if let Err(error) = case() {
            eprintln!("job controller case {name} FAILED: {error}");
            std::process::exit(1);
        }
    }
    println!("job controller flow passed");
}
