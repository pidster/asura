//! The J3 job path through the real service, the real Swift helper, a
//! scripted loopback model, the real conversation owner, job controller,
//! guardians under `sandbox-exec`, journal and payload store, in private
//! scratch homes (tool-job-controller.md, J3 validation, integration
//! layer). Cases: J3T-04 to J3T-14, J3T-17, J3T-20, ES-04, ES-05, ES-07,
//! ES-10, ES-11 and the identity detectors through real turns.
#[path = "support/queue_submit.rs"]
mod queue_submit;
#[path = "support/job_fixture.rs"]
mod job_fixture;

use asura_storage::authority::conversation::{
    self as journal, JobOutcome, JobReason, JobResultOutcome, Record, ToolDecision,
    ToolDecisionReason, ToolOperation,
};
use job_fixture::{Fixture, Model, Result, Step, call, holds, sweep, wait_for};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const CHILD: &str = "--job-tool-child";

fn setup(label: &str, config: &[(&str, &str)]) -> Result<(Fixture, Model, [u8; 16], PathBuf)> {
    let model = Model::start()?;
    let mut fixture = Fixture::new(label, CHILD)?;
    fixture.start()?;
    fixture.wait_ready()?;
    let (project, directory) = fixture.project("project")?;
    fixture.model(&model.endpoint)?;
    if !config.is_empty() {
        // `jobs.*` is read at service start.
        for (key, value) in config {
            let reply = fixture.config(key, value)?;
            assert!(reply.error.is_none(), "{key}: {:?}", reply.error);
        }
        fixture.stop()?;
        fixture.start()?;
        fixture.wait_ready()?;
    }
    Ok((fixture, model, project, directory))
}

/// The kind 14 and kind 29 records of one operation, in order.
fn calls(fixture: &Fixture, operation: [u8; 16]) -> Result<Vec<(journal::ToolCall, Option<journal::JobResult>)>> {
    let records = fixture.records()?;
    let mut out: Vec<(journal::ToolCall, Option<journal::JobResult>)> = Vec::new();
    for record in records {
        match record {
            Record::ToolCall(call) if call.operation == operation => out.push((*call, None)),
            Record::JobResult(result) if result.operation == operation => {
                if let Some(entry) = out.iter_mut().find(|(c, _)| c.ordinal == result.ordinal) {
                    entry.1 = Some(*result);
                }
            }
            _ => {}
        }
    }
    Ok(out)
}
fn settled(fixture: &Fixture, job: [u8; 16]) -> Result<Option<journal::JobSettled>> {
    Ok(fixture.records()?.into_iter().find_map(|r| match r {
        Record::JobSettled(s) if s.job == job => Some(*s),
        _ => None,
    }))
}
fn wait_settled(fixture: &Fixture, job: [u8; 16], limit: Duration) -> Result<journal::JobSettled> {
    let end = Instant::now() + limit;
    loop {
        if let Some(s) = settled(fixture, job)? {
            return Ok(s);
        }
        if Instant::now() >= end {
            return Err("job did not settle".into());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}
fn job_of(operation: [u8; 16], generation: u64, ordinal: u32) -> [u8; 16] {
    journal::exec_cmd_job_id(operation, generation, ordinal)
}

/// J3T-04, J3T-06, J3T-08, J3T-09, J3T-11, J3T-13, J3T-17 and J3T-20: a short
/// command settles inside its window; a long one returns `running` at its
/// window and keeps running after the turn; the model reads both streams,
/// waits for new output, lists and notes; no output text is recorded.
fn reads_and_waits() -> Result<()> {
    let (fixture, model, project, directory) = setup("reads", &[])?;
    // The output is built at run time, so the command text (kind 14) does
    // not hold it.
    let marker = "OUTXPUT-7f3a";
    model.script(&[
        call(
            "exec_cmd",
            json!({"command": "printf 'OUT%sPUT-%s\\nline2\\n' X 7f3a; printf 'warn' >&2"}),
        ),
        call("job", json!({"command": "read", "job": "{job}", "stream": "stdout"})),
        call("job", json!({"command": "read", "job": "{job}", "stream": "stderr"})),
        call(
            "exec_cmd",
            json!({"command": "sleep 1; printf tick; sleep 2; printf tock", "yield_seconds": 0}),
        ),
        call("job", json!({"command": "wait", "job": "{job}"})),
        call("job", json!({"command": "status", "job": "{job}"})),
        call("job", json!({"command": "note", "job": "{job}", "text": "watch this job"})),
        call("job", json!({"command": "list"})),
        Step::Final,
    ]);
    let operation = fixture.submit(project, "Run and read.")?;
    let event = fixture.finish(operation, Duration::from_secs(90))?;
    assert_eq!(event.kind, Some(3), "{event:?}");
    let messages = model.tool_messages();
    assert_eq!(messages.len(), 8, "{messages:?}");
    // J3T-04: settled inside the window; no output text in the delivery.
    assert!(messages[0].starts_with("job: "), "{}", messages[0]);
    assert!(messages[0].contains("state: settled") && messages[0].contains("result: exit 0"));
    assert!(!messages[0].contains(marker), "PC12: no output text in the delivery");
    // J3T-08: pages of both streams from 0; the delivered text is the output.
    assert!(messages[1].contains("stream: stdout") && messages[1].contains(marker), "{}", messages[1]);
    assert!(messages[1].contains("next offset: end of stream"), "{}", messages[1]);
    assert!(messages[2].contains("stream: stderr") && messages[2].contains("warn"), "{}", messages[2]);
    // J3T-04 (`yield_seconds` 0): handle delivery as soon as it spawned.
    assert!(
        messages[3].contains("state: running") || messages[3].contains("state: starting"),
        "{}",
        messages[3]
    );
    // J3T-09 and J3T-22: the wait ends on new output with no other event.
    assert!(
        messages[4].contains("wait ended: new output") || messages[4].contains("wait ended: settled"),
        "{}",
        messages[4]
    );
    assert!(messages[5].contains("command: sleep 1; printf tick"), "{}", messages[5]);
    // J3T-13: the note is recorded on the job.
    assert!(messages[6].starts_with("note: recorded on job "), "{}", messages[6]);
    // J3T-11: the list shows both jobs, newest first.
    assert!(messages[7].starts_with("jobs: 2 shown, newest first"), "{}", messages[7]);
    let rows: Vec<&str> = messages[7].lines().skip(1).collect();
    assert!(rows[0].contains("sleep 1; printf tick"), "{rows:?}");
    assert!(rows[1].contains("printf 'OUT%sPUT"), "{rows:?}");
    // Records: kind 14 `run` for each call; kind 29 for each; the note's
    // kind 30 before its kind 29 (JR16).
    let recorded = calls(&fixture, operation)?;
    assert_eq!(recorded.len(), 8);
    let codes: Vec<_> = recorded.iter().map(|(c, _)| c.tool_operation).collect();
    use ToolOperation as T;
    assert_eq!(
        codes,
        [T::ExecCmd, T::JobRead, T::JobRead, T::ExecCmd, T::JobWait, T::JobStatus, T::JobNote, T::JobList]
            .map(Some)
    );
    for (call, result) in &recorded {
        assert_eq!(call.decision, ToolDecision::Run);
        let result = result.as_ref().ok_or("kind 29")?;
        assert_eq!(result.outcome, JobResultOutcome::Ok, "{result:?}");
    }
    let records = fixture.records()?;
    let note = records
        .iter()
        .position(|r| matches!(r, Record::JobRetention(_)))
        .ok_or("kind 30")?;
    let note_result = records
        .iter()
        .position(|r| matches!(r, Record::JobResult(r) if r.ordinal == 7))
        .ok_or("kind 29 of the note")?;
    assert!(note < note_result, "JR16: kind 30 before its kind 29");
    // J3T-06: the long job keeps running after the turn and settles.
    let long = job_of(operation, recorded[3].0.generation, 4);
    let facts = wait_settled(&fixture, long, Duration::from_secs(30))?;
    assert_eq!(facts.outcome, JobOutcome::Exited);
    // J3T-20: one tool.finished per job call, after its kind 29, by value.
    let mut fixture = fixture;
    fixture.stop()?;
    let audit = audit_text(&fixture.home)?;
    for name in ["job_read", "job_wait", "job_status", "job_note", "job_list", "exec_cmd"] {
        assert!(audit.contains(&format!("\"tool\":\"{name}\"")), "{name} not audited");
    }
    // J3T-17 and J3T-20: no output text in the journal or the audit log.
    let journal_bytes = asura_storage::authority::fixture::journal_bytes(&fixture.home)?;
    assert!(!holds(&journal_bytes, marker.as_bytes()), "J3T-17: output in the journal");
    assert!(!holds(audit.as_bytes(), marker.as_bytes()));
    assert!(!holds(audit.as_bytes(), b"watch this job"), "no note text in the audit");
    sweep(&fixture.home, &[marker.as_bytes()])?;
    drop(directory);
    println!(
        "PASS J3T-04/06/08/09/11/13/17/20 integration: settled and running deliveries without output, pages of both streams, wait on new output, status, note before its kind 29, list newest first, the long job settled after its turn, tool.finished per job call, raw-byte sweep clean"
    );
    Ok(())
}

fn audit_text(home: &Path) -> Result<String> {
    let mut text = String::new();
    let logs = home.join(".asura/logs/audit");
    if let Ok(entries) = std::fs::read_dir(&logs) {
        for entry in entries {
            let path = entry?.path();
            if path.is_file() {
                text.push_str(&String::from_utf8_lossy(&std::fs::read(path)?));
            }
        }
    }
    if text.is_empty() {
        for entry in walk(&home.join(".asura"))? {
            if entry.to_string_lossy().contains("audit") && entry.is_file() {
                text.push_str(&String::from_utf8_lossy(&std::fs::read(entry)?));
            }
        }
    }
    Ok(text)
}
fn walk(root: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir(root) {
        for entry in entries {
            let path = entry?.path();
            if path.is_dir() {
                out.extend(walk(&path)?);
            } else {
                out.push(path);
            }
        }
    }
    Ok(out)
}

/// J3T-07, J3T-10, J3T-12 and J3T-14: with 2 live jobs per agent and a
/// queue of 1, the third start returns `queued (waiting: 2 of 2 jobs
/// running for this agent)`, the fourth is `busy` with `queue_full`; the
/// model cancels a job (TERM, one kind 27, `model_cancel`); an unknown
/// handle is `not_found`.
fn queue_and_cancel() -> Result<()> {
    let (fixture, model, project, directory) = setup(
        "queue",
        &[("jobs.liveAgent", "2"), ("jobs.queuedAgent", "1")],
    )?;
    let sleeper = json!({"command": "sleep 30", "yield_seconds": 0});
    model.script(&[
        call("exec_cmd", sleeper.clone()),
        call("exec_cmd", sleeper.clone()),
        call("exec_cmd", json!({"command": "printf third > third.txt", "yield_seconds": 0})),
        call("exec_cmd", json!({"command": "printf fourth > fourth.txt", "yield_seconds": 0})),
        call("job", json!({"command": "status", "job": "0123456789abcdef0123456789abcdef"})),
        Step::Final,
    ]);
    let operation = fixture.submit(project, "Queue.")?;
    let event = fixture.finish(operation, Duration::from_secs(60))?;
    assert_eq!(event.kind, Some(3), "{event:?}");
    let messages = model.tool_messages();
    assert_eq!(messages.len(), 5, "{messages:?}");
    assert!(
        messages[2].contains("state: queued (waiting: 2 of 2 jobs running for this agent)"),
        "J3T-07: {}",
        messages[2]
    );
    // The helper prefixes a failed call's text with its status.
    assert!(
        messages[3].contains("exec_cmd_busy: queue_full: "),
        "J3T-14: {}",
        messages[3]
    );
    assert!(
        messages[4].contains("job_not_found: no job with this handle in this conversation"),
        "J3T-12: {}",
        messages[4]
    );
    let recorded = calls(&fixture, operation)?;
    assert_eq!(recorded[3].0.decision, ToolDecision::Busy);
    assert_eq!(recorded[3].0.reason, Some(ToolDecisionReason::QueueFull));
    assert!(recorded[3].1.is_none(), "busy has no kind 29");
    assert!(!directory.join("fourth.txt").exists());
    // J3T-10: a later turn cancels the first sleeper by its handle.
    let first = job_of(operation, recorded[0].0.generation, 1);
    let handle = job_fixture::hex(&first);
    model.script(&[
        call("job", json!({"command": "cancel", "job": handle})),
        call("job", json!({"command": "status", "job": handle})),
        Step::Final,
    ]);
    let second = fixture.follow(project, operation, "Cancel it.")?;
    let event = fixture.finish(second, Duration::from_secs(60))?;
    assert_eq!(event.kind, Some(3), "{event:?}");
    let messages = model.tool_messages();
    assert!(messages[0].contains("cancel: requested"), "{}", messages[0]);
    let facts = wait_settled(&fixture, first, Duration::from_secs(20))?;
    assert_eq!(facts.outcome, JobOutcome::Cancelled);
    assert_eq!(facts.reason, JobReason::ModelCancel);
    let cancels = fixture
        .records()?
        .into_iter()
        .filter(|r| matches!(r, Record::JobCancelRequested(c) if c.job == first))
        .count();
    assert_eq!(cancels, 1, "one kind 27");
    // The queued third starts once a slot frees.
    wait_for(Duration::from_secs(20), "the queued job ran", || {
        directory.join("third.txt").exists()
    })?;
    println!(
        "PASS J3T-07/10/12/14 integration: third start queued with agent_slots, fourth busy queue_full, unknown handle not_found, model cancel stopped a job with one kind 27, the queued job then ran"
    );
    Ok(())
}

/// ES-04, ES-05, ES-07, ES-10 and ES-11 (integration): turning the project
/// off revokes its running model job with `execution_off` (also in its
/// yield window, which then reports `cancelled`), the next exec_cmd is
/// `denied` with reason `execution_off`, and on again runs.
fn execution_setting() -> Result<()> {
    let (fixture, model, project, directory) = setup("exec", &[])?;
    let key = format!("execution.projects.{}", job_fixture::hex(&project));
    model.script(&[
        call("exec_cmd", json!({"command": "sleep 60", "yield_seconds": 30})),
        Step::Final,
    ]);
    let operation = fixture.submit(project, "Long.")?;
    // The job runs inside its yield window.
    let job = job_of(operation, 1, 1);
    wait_for(Duration::from_secs(20), "kind 26", || {
        fixture
            .records()
            .is_ok_and(|r| r.iter().any(|r| matches!(r, Record::JobIntent(_))))
    })?;
    std::thread::sleep(Duration::from_millis(500));
    let reply = fixture.config(&key, "off")?;
    assert!(reply.error.is_none(), "{:?}", reply.error);
    let facts = wait_settled(&fixture, job, Duration::from_secs(20))?;
    assert_eq!(facts.reason, JobReason::ExecutionOff, "ES-05");
    let event = fixture.finish(operation, Duration::from_secs(40))?;
    assert_eq!(event.kind, Some(3), "{event:?}");
    let messages = model.tool_messages();
    assert!(messages[0].contains("state: settled"), "ES-10: {}", messages[0]);
    assert!(messages[0].contains("the person turned execution off"), "{}", messages[0]);
    // ES-04: refused, nothing reaches the controller.
    model.script(&[
        call("exec_cmd", json!({"command": "printf ran > refused.txt"})),
        Step::Final,
    ]);
    let refused = fixture.submit(project, "Again.")?;
    fixture.finish(refused, Duration::from_secs(30))?;
    let messages = model.tool_messages();
    assert!(
        messages[0].contains(
            "denied: execution_off: the person turned command execution off for this project; do not retry"
        ),
        "ES-04: {}",
        messages[0]
    );
    let recorded = calls(&fixture, refused)?;
    assert_eq!(recorded[0].0.decision, ToolDecision::Denied);
    assert_eq!(recorded[0].0.reason, Some(ToolDecisionReason::ExecutionOff));
    assert!(!directory.join("refused.txt").exists());
    // ES-07: on again runs.
    let reply = fixture.config(&key, "inherit")?;
    assert!(reply.error.is_none(), "{:?}", reply.error);
    model.script(&[
        call("exec_cmd", json!({"command": "printf ran > again.txt"})),
        Step::Final,
    ]);
    let again = fixture.submit(project, "On again.")?;
    fixture.finish(again, Duration::from_secs(30))?;
    assert!(directory.join("again.txt").exists());
    // ES-11: execution.changed for the off and the inherit, with the
    // revoked count; the job events carry execution_off.
    let mut fixture = fixture;
    fixture.stop()?;
    let audit = audit_text(&fixture.home)?;
    assert!(audit.contains("\"kind\":\"execution.changed\""), "{audit}");
    assert!(audit.contains("\"revoked_projects\":1"), "{audit}");
    assert!(audit.contains("\"reason\":\"execution_off\""), "{audit}");
    println!(
        "PASS ES-04/05/07/10/11 integration: project off revoked the running model job with execution_off, the next exec_cmd was denied execution_off and ran nothing, inherit ran again, execution.changed audited"
    );
    Ok(())
}

/// The identity detectors through real turns (J2-E10): a project replaced
/// under its path fails the next turn's admission and revokes its jobs; a
/// `job` call's recheck on a replaced project revokes and answers
/// `not_found`.
fn identity() -> Result<()> {
    let (fixture, model, project, directory) = setup("identity", &[])?;
    model.script(&[
        call("exec_cmd", json!({"command": "sleep 60", "yield_seconds": 0})),
        // Replace the project directory while the turn runs, then call
        // `job status`: the recheck sees another inode.
        call("exec_cmd", json!({"command": "sleep 1", "yield_seconds": 0})),
        call("job", json!({"command": "status", "job": "{job}"})),
        Step::Final,
    ]);
    // Hold the second model reply until the directory is replaced.
    let operation = fixture.submit(project, "Identity.")?;
    let job = job_of(operation, 1, 1);
    wait_for(Duration::from_secs(20), "the first job runs", || {
        fixture
            .records()
            .is_ok_and(|r| r.iter().any(|r| matches!(r, Record::JobIntent(_))))
    })?;
    let moved = directory.with_file_name("project-old");
    std::fs::rename(&directory, &moved)?;
    std::fs::create_dir(&directory)?;
    let event = fixture.finish(operation, Duration::from_secs(60))?;
    let facts = wait_settled(&fixture, job, Duration::from_secs(20))?;
    assert_eq!(facts.reason, JobReason::IdentityChanged, "{facts:?} {event:?}");
    let messages = model.tool_messages();
    assert!(
        messages.iter().any(|m| m.contains("job_not_found")),
        "the recheck answered not_found: {messages:?}"
    );
    println!(
        "PASS identity detectors through a real turn: a replaced project revoked its running job with identity_changed and the job call got not_found"
    );
    Ok(())
}

type Case = (&'static str, fn() -> Result<()>);

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if let [_, flag, job, profile, command] = args.as_slice()
        && flag == "--asura-exec-cmd-guardian"
    {
        std::process::exit(asura_platform::exec_cmd::run_guardian(
            job, profile, command,
        ));
    }
    if args.get(1).map(String::as_str) == Some(CHILD) {
        if let Err(error) = job_fixture::child(Path::new(&args[2])) {
            eprintln!("job tool child failed: {error}");
            std::process::exit(1);
        }
        return;
    }
    let selected: Vec<&str> = args[1..]
        .iter()
        .map(String::as_str)
        .filter(|a| !a.starts_with('-'))
        .collect();
    let cases: [Case; 4] = [
        ("reads", reads_and_waits),
        ("queue", queue_and_cancel),
        ("execution", execution_setting),
        ("identity", identity),
    ];
    for (name, case) in cases {
        if !selected.is_empty() && !selected.contains(&name) {
            continue;
        }
        if let Err(error) = case() {
            eprintln!("job tool flow case {name} FAILED: {error}");
            std::process::exit(1);
        }
    }
    println!("job tool flow qualification passed");
}
