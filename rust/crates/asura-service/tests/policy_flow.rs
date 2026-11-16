//! Policy admission and the revision lifecycle through the real service
//! (security-policy.md, PL3 validation map): the real conversation owner,
//! policy owner, admission thread, platform solver (the development cvc5
//! package in the Cargo profile directory, or the stalled-solver fixture
//! named by `policy-test-solver`), job controller, guardians under
//! `sandbox-exec` and journal, in private scratch homes under
//! `/private/tmp`. Model calls come from a scripted loopback model. No case
//! reads or writes `~/.asura` or a keychain.
//!
//! Cases (by name): `admit` (PL-I6 at the service, rollback, kind 35
//! sources), `refuse` (PL-I7: AD-7 and a can-error forbid, kind 35
//! `rejected`), `restart` (PL-I5: restore, then a damaged text makes the
//! user scope unavailable), `reprove` (PL-I5: a changed solver digest
//! reproves and reactivates), `stall` (SA-T1, PL-I11), `shutdown`
//! (PL-I13), `flood` (SA-T2 and the next candidate), `revoke` (PL-I8: a
//! narrowing revision revokes a running job; a widening one does not).
#[path = "support/job_fixture.rs"]
mod job_fixture;
#[path = "support/queue_submit.rs"]
mod queue_submit;

use asura_client::PolicyRequest;
use asura_control::pb;
use asura_storage::authority::conversation::{
    self as journal, JobOutcome, JobReason, PolicyRevisionEvent, Record,
};
use job_fixture::{Fixture, Model, Result, Step, call, wait_for, within_100ms};
use std::path::{Path, PathBuf};
use std::time::Duration;

const CHILD: &str = "--policy-child";

/// A model-origin tighten-only forbid of every command whose first tool is
/// `name`, added to the default user text.
fn user_forbid(name: &str) -> String {
    let base = asura_policy::RevisionTexts::builtin()
        .scopes
        .into_iter()
        .find(|s| s.scope == asura_policy::Scope::User)
        .expect("user text")
        .text;
    format!(
        "{base}\n@id(\"no-{name}\") @reason(\"{name}_denied\") @intent(\"true\")\n@explain(\"the user policy denies {name}\")\nforbid (principal is Asura::Job, action == Asura::Action::\"process.exec\", resource)\nwhen {{ principal.origin == Asura::Origin::\"model\" && context has intent && context.intent has command && context.intent.command.first_tool == \"{name}\" }};\n"
    )
}

fn status(fixture: &Fixture) -> Result<pb::PolicyStatus> {
    let update = fixture.attach()?.observe_service(None)?;
    update.policy.ok_or_else(|| "no policy status".into())
}
fn submit(fixture: &Fixture, text: &str) -> Result<[u8; 16]> {
    let reply = fixture.attach()?.policy(
        PolicyRequest::Submit(pb::PolicyCandidateSubmit {
            request_id: Some(asura_platform::random_id().to_vec()),
            scope: Some(2),
            text: Some(text.into()),
        }),
        &|| false,
    )?;
    if let Some(error) = reply.error {
        return Err(format!("submit refused: {error}").into());
    }
    job_fixture::id(reply.candidate_id)
}
fn rollback(fixture: &Fixture, revision: u64) -> Result<[u8; 16]> {
    let reply = fixture.attach()?.policy(
        PolicyRequest::Rollback(pb::PolicyRollback {
            request_id: Some(asura_platform::random_id().to_vec()),
            scope: Some(2),
            revision: Some(revision),
        }),
        &|| false,
    )?;
    if let Some(error) = reply.error {
        return Err(format!("rollback refused: {error}").into());
    }
    job_fixture::id(reply.candidate_id)
}
fn cancel(fixture: &Fixture, id: [u8; 16]) -> Result<pb::PolicyReply> {
    Ok(fixture.attach()?.policy(
        PolicyRequest::Cancel(pb::PolicyCancel {
            request_id: Some(asura_platform::random_id().to_vec()),
            candidate_id: Some(id.to_vec()),
        }),
        &|| false,
    )?)
}
/// The candidate's final state in `PolicyStatus`: 3 activated, 4
/// rejected, 5 cancelled. Harness budget only.
fn result(fixture: &Fixture, id: [u8; 16], limit: Duration) -> Result<pb::PolicyCandidate> {
    let mut found = None;
    wait_for(limit, "the candidate result", || {
        found = status(fixture).ok().and_then(|s| {
            s.candidate.into_iter().chain(s.recent).find(|c| {
                c.candidate_id.as_deref() == Some(&id[..]) && matches!(c.state, Some(3..=5))
            })
        });
        found.is_some()
    })?;
    Ok(found.expect("found"))
}
fn proving(fixture: &Fixture, id: [u8; 16]) -> Result<()> {
    wait_for(Duration::from_secs(30), "the candidate proving", || {
        status(fixture)
            .ok()
            .and_then(|s| s.candidate)
            .is_some_and(|c| c.candidate_id.as_deref() == Some(&id[..]) && c.state == Some(2))
    })
}
fn revisions(fixture: &Fixture) -> Result<Vec<journal::PolicyRevision>> {
    Ok(fixture
        .records()?
        .into_iter()
        .filter_map(|r| match r {
            Record::PolicyRevision(p) => Some(*p),
            _ => None,
        })
        .collect())
}
/// One model `exec_cmd` turn; returns the tool message the model received.
fn exec_turn(fixture: &Fixture, model: &Model, project: [u8; 16], command: &str) -> Result<String> {
    model.script(&[
        call("exec_cmd", serde_json::json!({"command": command})),
        Step::Final,
    ]);
    let operation = fixture.submit(project, "Run it.")?;
    let event = fixture.finish(operation, Duration::from_secs(60))?;
    assert_eq!(event.kind, Some(3), "the model turn completed: {event:?}");
    Ok(model.tool_messages().last().cloned().unwrap_or_default())
}
/// The solver copies in the run directory (`solver-<hex>`).
fn solver_copies(home: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(home.join(".asura/run"))
        .map(|entries| {
            entries
                .flatten()
                .map(|e| e.path())
                .filter(|p| {
                    p.file_name()
                        .is_some_and(|n| n.to_string_lossy().starts_with("solver-"))
                })
                .collect()
        })
        .unwrap_or_default()
}
/// Live processes whose command line names `needle`.
fn processes(needle: &str) -> usize {
    std::process::Command::new("/usr/bin/pgrep")
        .args(["-f", needle])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).lines().count())
        .unwrap_or(0)
}
/// A stalled-solver fixture package in the home, named by
/// `policy-test-solver` for the next service start.
fn use_fixture(fixture: &Fixture, mode: &str) -> Result<PathBuf> {
    let exe = asura_platform::solver::fixture_executable().ok_or("solver fixture not built")?;
    let dir = fixture.home.join(format!("fixture-{mode}"));
    std::fs::create_dir_all(&dir)?;
    let package = asura_platform::solver::fixture_package(&exe, mode, &dir)?;
    std::fs::write(
        fixture.home.join("policy-test-solver"),
        package.to_string_lossy().as_bytes(),
    )?;
    Ok(package)
}
fn started(label: &str) -> Result<(Fixture, Model, [u8; 16], PathBuf)> {
    let model = Model::start()?;
    let mut fixture = Fixture::new(label, CHILD)?;
    fixture.start()?;
    fixture.wait_ready()?;
    let (project, directory) = fixture.project("project")?;
    fixture.model(&model.endpoint)?;
    Ok((fixture, model, project, directory))
}

/// PL-I6 at the service and rollback: the solver is available; a model
/// call journals the built-in revision; a user forbid is admitted with the
/// real cvc5 and activates as revision 2 (source `person`, solver and
/// admission digests); the next model call is denied with scope `user`;
/// rollback to revision 1 activates revision 3 and the command runs again.
fn admit() -> Result<()> {
    let (fixture, model, project, directory) = started("pl-admit")?;
    assert_eq!(status(&fixture)?.solver, Some(2), "solver available");
    let ran = exec_turn(&fixture, &model, project, "printf one > one.txt")?;
    assert!(directory.join("one.txt").exists(), "{ran}");
    let id = submit(&fixture, &user_forbid("printf"))?;
    let done = result(&fixture, id, Duration::from_secs(120))?;
    assert_eq!(done.state, Some(3), "{done:?}");
    assert_eq!(done.revision, Some(2));
    assert!(done.queries.unwrap_or(0) > 0);
    let all = revisions(&fixture)?;
    let activated = all.last().expect("kind 35");
    assert_eq!(activated.event, PolicyRevisionEvent::Activated);
    assert_eq!((activated.seq, activated.previous_seq), (2, 1));
    assert!(activated.solver_digest.is_some() && activated.admission_digest.is_some());
    assert!(
        activated
            .scopes
            .iter()
            .any(|s| s.scope == 2 && s.source == "person")
    );
    let denied = exec_turn(&fixture, &model, project, "printf two > two.txt")?;
    assert!(
        denied.contains(
            "denied: policy: user: printf_denied: the user policy denies printf; do not retry"
        ),
        "{denied}"
    );
    assert!(!directory.join("two.txt").exists());
    // Rollback to the built-in user text: a new revision.
    let id = rollback(&fixture, 1)?;
    let done = result(&fixture, id, Duration::from_secs(120))?;
    assert_eq!((done.state, done.revision), (Some(3), Some(3)), "{done:?}");
    exec_turn(&fixture, &model, project, "printf three > three.txt")?;
    assert!(directory.join("three.txt").exists());
    assert!(
        solver_copies(&fixture.home).is_empty(),
        "solver copies removed"
    );
    println!("PASS PL-I6 service: admitted with cvc5, user denial, rollback as revision 3");
    Ok(())
}

/// PL-I7 at the service: an AD-7 forbid of one person address and port,
/// and a forbid that can error, are refused `proof_failed` with a
/// counterexample; each appends kind 35 `rejected`; the active revision
/// stays and decisions continue.
fn refuse() -> Result<()> {
    let (fixture, model, project, directory) = started("pl-refuse")?;
    let base = user_forbid("printf");
    for (label, extra) in [
        (
            "ad7",
            "@id(\"no-net\") @reason(\"no_net\")\nforbid (principal is Asura::Job, action == Asura::Action::\"network.connect\", resource) when { principal.origin == Asura::Origin::\"person_typed\" && context.ip == ip(\"192.0.2.1\") && context.port == 443 };",
        ),
        (
            "error",
            "@id(\"overflow\") @reason(\"overflow\")\nforbid (principal is Asura::Job, action == Asura::Action::\"network.connect\", resource) when { context.port + 9223372036854775807 > 0 };",
        ),
    ] {
        let id = submit(&fixture, &format!("{base}\n{extra}\n"))?;
        let done = result(&fixture, id, Duration::from_secs(120))?;
        assert_eq!(done.state, Some(4), "{label}: {done:?}");
        assert_eq!(done.reason.as_deref(), Some("proof_failed"), "{label}");
        assert!(
            !done.counterexamples.is_empty(),
            "{label}: a counterexample"
        );
    }
    let all = revisions(&fixture)?;
    assert_eq!(
        all.iter()
            .filter(|r| r.event == PolicyRevisionEvent::Rejected)
            .count(),
        2
    );
    exec_turn(&fixture, &model, project, "printf ok > ok.txt")?;
    assert!(
        directory.join("ok.txt").exists(),
        "the built-in revision decides"
    );
    println!("PASS PL-I7 service: AD-7 and can-error forbids refused with counterexamples");
    Ok(())
}

/// PL-I5: an admitted revision is restored at restart with no new kind 35
/// and keeps deciding; a damaged stored text makes the user scope
/// unavailable, so a model call is refused `policy_unavailable`.
fn restart() -> Result<()> {
    let (mut fixture, model, project, directory) = started("pl-restart")?;
    let id = submit(&fixture, &user_forbid("printf"))?;
    assert_eq!(
        result(&fixture, id, Duration::from_secs(120))?.state,
        Some(3)
    );
    let before = revisions(&fixture)?.len();
    fixture.stop()?;
    fixture.start()?;
    fixture.wait_ready()?;
    let denied = exec_turn(&fixture, &model, project, "printf x > x.txt")?;
    assert!(denied.contains("user: printf_denied"), "{denied}");
    assert_eq!(
        revisions(&fixture)?.len(),
        before,
        "restored without kind 35"
    );
    assert_eq!(status(&fixture)?.user, Some(1));
    // Damage the stored text.
    fixture.stop()?;
    let admitted = fixture.home.join(".asura/policy/admitted");
    for entry in std::fs::read_dir(&admitted)? {
        std::fs::write(entry?.path(), b"// damaged\n")?;
    }
    fixture.start()?;
    fixture.wait_ready()?;
    wait_for(Duration::from_secs(30), "user scope unavailable", || {
        status(&fixture).is_ok_and(|s| s.user == Some(2))
    })?;
    let refused = exec_turn(&fixture, &model, project, "printf y > y.txt")?;
    assert!(refused.contains("denied: policy"), "{refused}");
    assert!(!directory.join("y.txt").exists());
    println!("PASS PL-I5 restart: restored; a damaged text makes the user scope unavailable");
    Ok(())
}

/// PL-I5 reproving: a solver with another digest at restart reproves the
/// stored text and activates it as a new revision with that digest.
fn reprove() -> Result<()> {
    let (mut fixture, model, project, _) = started("pl-reprove")?;
    let id = submit(&fixture, &user_forbid("printf"))?;
    assert_eq!(
        result(&fixture, id, Duration::from_secs(120))?.state,
        Some(3)
    );
    let first = revisions(&fixture)?.last().cloned().expect("kind 35");
    fixture.stop()?;
    use_fixture(&fixture, "answer-unsat")?;
    fixture.start()?;
    fixture.wait_ready()?;
    wait_for(Duration::from_secs(60), "reproved revision", || {
        revisions(&fixture).is_ok_and(|all| all.last().is_some_and(|r| r.seq > first.seq))
    })?;
    let second = revisions(&fixture)?.last().cloned().expect("kind 35");
    assert_eq!(second.event, PolicyRevisionEvent::Activated);
    assert_ne!(second.solver_digest, first.solver_digest);
    let denied = exec_turn(&fixture, &model, project, "printf z > z.txt")?;
    assert!(denied.contains("user: printf_denied"), "{denied}");
    println!("PASS PL-I5 reprove: a changed solver digest reproved the text as a new revision");
    Ok(())
}

/// SA-T1 and PL-I11: a solver that reads and never answers: the candidate
/// waits proving, a model call is decided meanwhile, status answers within
/// 100 ms, and cancel kills and reaps the solver and removes its copy.
fn stall() -> Result<()> {
    let model = Model::start()?;
    let mut fixture = Fixture::new("pl-stall", CHILD)?;
    let package = use_fixture(&fixture, "silent")?;
    fixture.start()?;
    fixture.wait_ready()?;
    let (project, directory) = fixture.project("project")?;
    fixture.model(&model.endpoint)?;
    let id = submit(&fixture, &user_forbid("printf"))?;
    proving(&fixture, id)?;
    wait_for(Duration::from_secs(30), "the solver copy", || {
        !solver_copies(&fixture.home).is_empty()
    })?;
    within_100ms("status while proving", || status(&fixture))?;
    exec_turn(&fixture, &model, project, "printf live > live.txt")?;
    assert!(directory.join("live.txt").exists(), "decisions continue");
    let reply = cancel(&fixture, id)?;
    assert!(reply.error.is_none(), "{reply:?}");
    let done = result(&fixture, id, Duration::from_secs(30))?;
    assert_eq!(done.state, Some(5), "{done:?}");
    wait_for(Duration::from_secs(30), "copy removed", || {
        solver_copies(&fixture.home).is_empty()
    })?;
    assert_eq!(processes(&package.to_string_lossy()), 0, "fixture reaped");
    assert!(
        revisions(&fixture)?
            .iter()
            .all(|r| r.event != PolicyRevisionEvent::Rejected),
        "a cancel appends nothing"
    );
    println!("PASS SA-T1/PL-I11: stalled solver; decisions continue; cancel kills and reaps it");
    Ok(())
}

/// PL-I13: shutdown during admission settles with no solver process and no
/// copy left.
fn shutdown() -> Result<()> {
    let mut fixture = Fixture::new("pl-shutdown", CHILD)?;
    let package = use_fixture(&fixture, "silent")?;
    fixture.start()?;
    fixture.wait_ready()?;
    let id = submit(&fixture, &user_forbid("printf"))?;
    proving(&fixture, id)?;
    fixture.stop()?;
    assert!(solver_copies(&fixture.home).is_empty(), "no copy left");
    assert_eq!(processes(&package.to_string_lossy()), 0, "no orphan solver");
    println!("PASS PL-I13: shutdown during admission left no solver and no copy");
    Ok(())
}

/// SA-T2 at the service: a solver that writes 1 MiB before reading: the
/// candidate is refused `solver_failed`; the admission thread takes the
/// next candidate.
fn flood() -> Result<()> {
    let mut fixture = Fixture::new("pl-flood", CHILD)?;
    use_fixture(&fixture, "flood-stdout")?;
    fixture.start()?;
    fixture.wait_ready()?;
    let first = submit(&fixture, &user_forbid("printf"))?;
    let second = submit(&fixture, &user_forbid("sleep"))?;
    let done = result(&fixture, first, Duration::from_secs(60))?;
    assert_eq!(done.reason.as_deref(), Some("solver_failed"), "{done:?}");
    let next = result(&fixture, second, Duration::from_secs(60))?;
    assert_eq!(
        next.state,
        Some(4),
        "the next candidate was admitted in turn"
    );
    assert!(solver_copies(&fixture.home).is_empty());
    println!("PASS SA-T2 service: flooding solver refused; the next candidate ran");
    Ok(())
}

/// PL-I8: a running model job decided under revision 1 is re-evaluated
/// when a narrowing revision activates and is revoked `policy_revoked`;
/// a widening revision does not revoke the next running job.
fn revoke() -> Result<()> {
    let (fixture, model, project, _) = started("pl-revoke")?;
    exec_turn(&fixture, &model, project, "sleep 120")?;
    let job_of = |fixture: &Fixture| -> Option<(journal::Id, Option<JobOutcome>, JobReason)> {
        let records = fixture.records().ok()?;
        let job = records.iter().rev().find_map(|r| match r {
            Record::JobIntent(i) => Some(journal::job_id(&i.origin)),
            _ => None,
        })?;
        let settled = records.iter().find_map(|r| match r {
            Record::JobSettled(s) if s.job == job => Some((s.outcome, s.reason)),
            _ => None,
        });
        Some((
            job,
            settled.map(|s| s.0),
            settled.map_or(JobReason::None, |s| s.1),
        ))
    };
    wait_for(Duration::from_secs(30), "the job runs", || {
        job_of(&fixture).is_some()
    })?;
    let id = submit(&fixture, &user_forbid("sleep"))?;
    assert_eq!(
        result(&fixture, id, Duration::from_secs(120))?.state,
        Some(3)
    );
    wait_for(Duration::from_secs(60), "the job revoked", || {
        job_of(&fixture).is_some_and(|(_, outcome, reason)| {
            outcome.is_some() && reason == JobReason::PolicyRevoked
        })
    })?;
    assert!(fixture.records()?.iter().any(|r| matches!(r,
        Record::PolicyDecision(d) if matches!(d.subject, journal::PolicySubject::JobRecheck { .. })
            && d.outcome == journal::PolicyOutcome::Deny)));
    // A widening change (back to the default text) revokes nothing.
    exec_turn(&fixture, &model, project, "printf start; sleep 120")?;
    let running = job_of(&fixture).expect("job").0;
    let default = asura_policy::RevisionTexts::builtin()
        .scopes
        .into_iter()
        .find(|s| s.scope == asura_policy::Scope::User)
        .expect("user text")
        .text;
    let id = submit(&fixture, &default)?;
    assert_eq!(
        result(&fixture, id, Duration::from_secs(120))?.state,
        Some(3)
    );
    std::thread::sleep(Duration::from_secs(2));
    let now = job_of(&fixture).expect("job");
    assert_eq!((now.0, now.1), (running, None), "the running job continues");
    println!("PASS PL-I8: a narrowing revision revoked the running job; a widening one did not");
    Ok(())
}

type Case = fn() -> Result<()>;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if let [_, flag, job, profile, constraints, command] = args.as_slice()
        && flag == "--asura-exec-cmd-guardian"
    {
        std::process::exit(asura_platform::exec_cmd::run_guardian(
            job,
            profile,
            constraints,
            command,
        ));
    }
    if args.get(1).map(String::as_str) == Some("--asura-exec-cmd-guardian") {
        std::process::exit(2);
    }
    if args.get(1).map(String::as_str) == Some(CHILD) {
        if let Err(error) = job_fixture::child(Path::new(&args[2])) {
            eprintln!("policy child failed: {error}");
            std::process::exit(1);
        }
        return;
    }
    let selected: Vec<&str> = args[1..]
        .iter()
        .map(String::as_str)
        .filter(|a| !a.starts_with('-'))
        .collect();
    let cases: Vec<(&str, Case)> = vec![
        ("admit", admit),
        ("refuse", refuse),
        ("restart", restart),
        ("reprove", reprove),
        ("stall", stall),
        ("shutdown", shutdown),
        ("flood", flood),
        ("revoke", revoke),
    ];
    for (name, case) in cases {
        if !selected.is_empty() && !selected.contains(&name) {
            continue;
        }
        if let Err(error) = case() {
            eprintln!("policy flow case {name} FAILED: {error}");
            std::process::exit(1);
        }
    }
    println!("policy flow qualification passed");
}
