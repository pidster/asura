//! The J4 control requests through the real service and client over the
//! control socket, with real guardians and a scripted loopback model
//! (tool-job-controller.md, J4 validation, integration layer). Cases:
//! J4T-04, J4T-05, J4T-06, J4T-07, J4T-08, J4T-09, J4T-13 and ES-12.
#[path = "support/job_fixture.rs"]
mod job_fixture;
#[path = "support/queue_submit.rs"]
mod queue_submit;

use asura_control::pb;
use asura_storage::authority::conversation::{self as journal, JobReason, JobRequester, Record};
use job_fixture::{Fixture, Model, Result, Step, call, wait_for, within_100ms};
use serde_json::json;
use std::path::Path;
use std::time::{Duration, Instant};

const CHILD: &str = "--job-control-child";

fn never() -> bool {
    false
}
fn list(project: [u8; 16]) -> pb::JobList {
    pb::JobList {
        scope: Some(pb::JobScope::Project as i32),
        project_id: Some(project.to_vec()),
        conversation_id: None,
        before: None,
        limit: Some(8),
        live_only: None,
        after_revision: None,
    }
}
fn job_id(entry: &pb::JobEntry) -> Result<[u8; 16]> {
    job_fixture::id(entry.job_id.clone())
}

/// J4T-04, J4T-05, J4T-07, J4T-09 and ES-12: a model job outlives its turn;
/// the client lists it with its command, reads a page (the page reply wakes
/// the reactor), parks an observe that ends on new output and a list that
/// ends on the revision; a wrong expected project is `project_mismatch`; the
/// client cancel stops the job with one kind 27 (client, client_cancel).
fn follow_and_cancel() -> Result<()> {
    let model = Model::start()?;
    let mut fixture = Fixture::new("follow", CHILD)?;
    fixture.start()?;
    fixture.wait_ready()?;
    let (project, _) = fixture.project("project")?;
    let (other, _) = fixture.project("other")?;
    fixture.model(&model.endpoint)?;
    model.script(&[
        call(
            "exec_cmd",
            json!({"command": "printf 'first\\n'; sleep 2; printf 'second\\n'; sleep 60", "yield_seconds": 0}),
        ),
        Step::Final,
    ]);
    let operation = fixture.submit(project, "Start a long job.")?;
    let event = fixture.finish(operation, Duration::from_secs(60))?;
    // J4T-09: the activity names the job; EX12 counts it.
    let job = job_fixture::id(event.tools[0].job_id.clone())?;
    assert_eq!(event.outlived_jobs, Some(1), "{event:?}");
    assert_eq!(event.outlived_job_ids, vec![job.to_vec()]);
    let mut client = fixture.attach()?;
    assert!(
        client.supports(pb::Capability::Jobs),
        "CAPABILITY_JOBS (EN1)"
    );
    let reply = client.jobs(list(project), &never)?;
    assert_eq!(reply.entries.len(), 1, "{reply:?}");
    let entry = &reply.entries[0];
    assert_eq!(job_id(entry)?, job);
    assert!(
        entry
            .command
            .as_deref()
            .unwrap_or("")
            .starts_with("printf 'first"),
        "{entry:?}"
    );
    assert_eq!(reply.live_in_scope, Some(1));
    assert_eq!(reply.execution_enabled, Some(true));
    let revision = reply.revision.ok_or("revision")?;
    // A page read: the reply arrives with no other reactor event.
    wait_for(Duration::from_secs(10), "first output", || {
        client
            .jobs(list(project), &never)
            .is_ok_and(|r| r.entries[0].stdout_bytes.unwrap_or(0) >= 6)
    })?;
    let page = within_100ms("JobRead with a page wake", || {
        Ok(client.read_job(
            pb::JobRead {
                job_id: Some(job.to_vec()),
                project_id: Some(project.to_vec()),
                stream: Some(pb::JobStream::Stdout as i32),
                offset: Some(0),
                limit: Some(8192),
                at_end: None,
            },
            &never,
        )?)
    })?;
    let stdout = &page.pages[0];
    assert_eq!(stdout.text.as_deref(), Some("first\n"), "{page:?}");
    assert_eq!(stdout.state, Some(pb::JobPageState::Live as i32));
    // J4T-04: an observe parked at the stored bytes ends on new output.
    let started = Instant::now();
    let observed = client.observe_job(
        pb::JobObserve {
            job_id: Some(job.to_vec()),
            project_id: None,
            stream: Some(pb::JobStream::Stdout as i32),
            offset: Some(6),
            at_end: None,
            after_state: None,
        },
        &never,
    )?;
    let waited = started.elapsed();
    assert!(
        observed.pages[0]
            .text
            .as_deref()
            .unwrap_or("")
            .contains("second"),
        "{observed:?}"
    );
    // J4T-07: a wrong expected project is refused before any effect.
    let refused = client.cancel_job(
        pb::JobCancel {
            request_id: Some(asura_platform::random_id().to_vec()),
            job_id: Some(job.to_vec()),
            expected_project_id: Some(other.to_vec()),
            kill: None,
        },
        &never,
    )?;
    assert_eq!(refused.error.as_deref(), Some("project_mismatch"));
    // ES-12 and J4T-04: a list parked at the revision returns on the cancel.
    let runtime = fixture.runtime()?;
    let parked = std::thread::spawn({
        let request = pb::JobList {
            after_revision: Some(revision.max(1)),
            limit: Some(1),
            ..list(project)
        };
        move || -> std::result::Result<pb::JobListReply, String> {
            let cancel: asura_client::Cancel = std::sync::Arc::new(|| false);
            let mut client = asura_client::Client::attach(&runtime, job_fixture::BUILD, &*cancel)
                .map_err(|e| e.to_string())?;
            client.jobs(request, &never).map_err(|e| e.to_string())
        }
    });
    std::thread::sleep(Duration::from_millis(300));
    let cancelled = client.cancel_job(
        pb::JobCancel {
            request_id: Some(asura_platform::random_id().to_vec()),
            job_id: Some(job.to_vec()),
            expected_project_id: Some(project.to_vec()),
            kill: None,
        },
        &never,
    )?;
    assert_eq!(cancelled.requested, Some(true), "{cancelled:?}");
    let parked = parked.join().map_err(|_| "parked list panicked")??;
    assert!(parked.revision.unwrap_or(0) != revision, "{parked:?}");
    // J4T-05: one kind 27 from the client; the job settles cancelled.
    wait_for(Duration::from_secs(20), "kind 28", || {
        fixture.records().is_ok_and(|r| {
            r.iter()
                .any(|r| matches!(r, Record::JobSettled(s) if s.job == job))
        })
    })?;
    let records = fixture.records()?;
    let cancels: Vec<_> = records
        .iter()
        .filter_map(|r| match r {
            Record::JobCancelRequested(c) if c.job == job => Some(*c),
            _ => None,
        })
        .collect();
    assert_eq!(cancels.len(), 1);
    assert_eq!(
        (cancels[0].requester, cancels[0].reason),
        (JobRequester::Client, JobReason::ClientCancel)
    );
    // The settled job's pages and detail still read (the viewer's
    // requests after a cancel).
    let page = client.read_job(
        pb::JobRead {
            job_id: Some(job.to_vec()),
            project_id: None,
            stream: Some(pb::JobStream::Stdout as i32),
            offset: None,
            limit: Some(8192),
            at_end: Some(true),
        },
        &never,
    )?;
    assert!(page.error.is_none(), "{page:?}");
    assert!(
        page.pages[0]
            .text
            .as_deref()
            .unwrap_or("")
            .contains("second"),
        "{page:?}"
    );
    let detail = client.inspect_job(job, None, &never)?;
    assert_eq!(
        detail.entry.as_ref().and_then(|e| e.reason),
        Some(JobReason::ClientCancel as u32),
        "{detail:?}"
    );
    fixture.stop()?;
    println!(
        "PASS J4T-04/05/07/09 ES-12 integration: listed with its command, page read with a page wake, observe ended on new output after {waited:?}, project_mismatch refused, parked list returned on the cancel, one client kind 27"
    );
    Ok(())
}

/// J4T-06 and J4T-13: pin and unpin; a person note with a detected secret
/// stored as `$ASURA_SECRET_0` and a typed placeholder kept; a model note;
/// JobInspect lists both notes with their authors; delete of the settled
/// output removes its files and pages report `deleted`.
fn retention() -> Result<()> {
    let model = Model::start()?;
    let mut fixture = Fixture::new("retain", CHILD)?;
    fixture.start()?;
    fixture.wait_ready()?;
    let (project, _) = fixture.project("project")?;
    fixture.model(&model.endpoint)?;
    model.script(&[
        call("exec_cmd", json!({"command": "printf 'kept output\\n'"})),
        call(
            "job",
            json!({"command": "note", "job": "{job}", "text": "model says hi"}),
        ),
        Step::Final,
    ]);
    let operation = fixture.submit(project, "Note it.")?;
    let event = fixture.finish(operation, Duration::from_secs(60))?;
    assert_eq!(event.kind, Some(3), "{event:?}");
    let job = job_fixture::id(event.tools[0].job_id.clone())?;
    let mut client = fixture.attach()?;
    let retain = |client: &mut asura_client::Client,
                  action: pb::JobRetainAction,
                  note: Option<&str>|
     -> Result<pb::JobRetainReply> {
        Ok(client.retain_job(
            pb::JobRetain {
                request_id: Some(asura_platform::random_id().to_vec()),
                job_id: Some(job.to_vec()),
                expected_project_id: Some(project.to_vec()),
                action: Some(action as i32),
                note: note.map(str::to_owned),
                confirmed: (action == pb::JobRetainAction::Delete).then_some(true),
            },
            &never,
        )?)
    };
    let pinned = retain(&mut client, pb::JobRetainAction::Pin, None)?;
    assert_eq!(pinned.changed, Some(true), "{pinned:?}");
    let again = retain(&mut client, pb::JobRetainAction::Pin, None)?;
    assert_eq!(
        again.changed,
        Some(false),
        "an unchanged pin writes nothing"
    );
    let unpinned = retain(&mut client, pb::JobRetainAction::Unpin, None)?;
    assert_eq!(unpinned.changed, Some(true));
    let token = ["ghp_", &"Np8".repeat(12)].concat();
    let note = format!("token {token} and $ASURA_SECRET_3 stay safe");
    let noted = retain(&mut client, pb::JobRetainAction::Note, Some(&note))?;
    assert_eq!(noted.changed, Some(true), "{noted:?}");
    let detail = client.inspect_job(job, Some(project), &never)?;
    let notes: Vec<_> = detail
        .notes
        .iter()
        .map(|n| (n.author, n.text.clone().unwrap_or_default()))
        .collect();
    assert_eq!(notes.len(), 2, "{notes:?}");
    assert_eq!(
        notes[0],
        (Some(2), "model says hi".to_owned()),
        "the model note first"
    );
    assert_eq!(notes[1].0, Some(1));
    assert!(notes[1].1.contains("$ASURA_SECRET_0"), "{:?}", notes[1]);
    assert!(
        notes[1].1.contains("$ASURA_SECRET_3"),
        "a typed placeholder stays"
    );
    assert!(!notes[1].1.contains(&token));
    // Delete: the summary and notes stay; pages report `deleted`.
    let deleted = retain(&mut client, pb::JobRetainAction::Delete, None)?;
    assert_eq!(deleted.changed, Some(true), "{deleted:?}");
    let (_, replay) = fixture.journal()?;
    let [stdout, _] = asura_storage::payload::job_payloads(replay.installation_id, job);
    let object = fixture
        .home
        .join(".asura/payloads/objects")
        .join(job_fixture::hex(&stdout));
    wait_for(Duration::from_secs(10), "the payload deletion", || {
        !object.exists()
    })?;
    let page = client.read_job(
        pb::JobRead {
            job_id: Some(job.to_vec()),
            project_id: None,
            stream: Some(pb::JobStream::Stdout as i32),
            offset: Some(0),
            limit: Some(64),
            at_end: None,
        },
        &never,
    )?;
    assert_eq!(
        page.pages[0].state,
        Some(pb::JobPageState::Deleted as i32),
        "{page:?}"
    );
    let detail = client.inspect_job(job, None, &never)?;
    assert_eq!(detail.notes.len(), 2, "notes stay after delete");
    let kinds: Vec<_> = fixture
        .records()?
        .into_iter()
        .filter_map(|r| match r {
            Record::JobRetention(r) if r.job == job => Some(r.action),
            _ => None,
        })
        .collect();
    use journal::RetentionAction as A;
    assert_eq!(kinds, vec![A::Note, A::Pin, A::Unpin, A::Note, A::Delete]);
    let journal_bytes = asura_storage::authority::fixture::journal_bytes(&fixture.home)?;
    assert!(
        !job_fixture::holds(&journal_bytes, token.as_bytes()),
        "no secret in kind 30"
    );
    fixture.stop()?;
    println!(
        "PASS J4T-06/13 integration: pin, unchanged pin, unpin, a redacted person note with a typed placeholder kept, the model note, both notes in JobInspect by author, delete removed the files and pages report deleted, the notes stay"
    );
    Ok(())
}

/// J4T-08 (EX12): a turn cancelled with two live jobs carries
/// `outlived_jobs` 2 and both IDs, oldest first; after a service restart,
/// when both jobs have settled after the terminal, the terminal read still
/// derives 2 from the records.
fn outlived() -> Result<()> {
    let model = Model::start()?;
    let mut fixture = Fixture::new("outlived", CHILD)?;
    fixture.start()?;
    fixture.wait_ready()?;
    let (project, _) = fixture.project("project")?;
    fixture.model(&model.endpoint)?;
    let long = json!({"command": "sleep 600", "yield_seconds": 0});
    model.script(&[
        call("exec_cmd", long.clone()),
        call("exec_cmd", long),
        // Only events end this wait: the turn cancel ends it.
        call("job", json!({"command": "wait", "job": "{job}"})),
        Step::Final,
    ]);
    let operation = fixture.submit(project, "Start two jobs and wait.")?;
    wait_for(Duration::from_secs(60), "the job wait call", || {
        fixture.records().is_ok_and(|records| {
            records
                .iter()
                .filter(|r| matches!(r, Record::ToolCall(c) if c.operation == operation))
                .count()
                == 3
        })
    })?;
    fixture
        .attach()?
        .cancel_conversation(operation, 1, &never)?;
    let event = fixture.finish(operation, Duration::from_secs(60))?;
    assert_eq!(event.kind, Some(5), "cancelled: {event:?}");
    let jobs: Vec<Vec<u8>> = event.tools[..2]
        .iter()
        .map(|t| t.job_id.clone().ok_or("job_id"))
        .collect::<std::result::Result<_, _>>()?;
    assert_eq!(event.outlived_jobs, Some(2), "{event:?}");
    assert_eq!(event.outlived_job_ids, jobs);
    // Both jobs are still live: no kind 28 yet.
    assert!(!fixture
        .records()?
        .iter()
        .any(|r| matches!(r, Record::JobSettled(_))));
    // The restart settles both after the terminal; the read derives the
    // same count from the records.
    fixture.stop()?;
    fixture.start()?;
    fixture.wait_ready()?;
    wait_for(Duration::from_secs(60), "both kind 28 after the terminal", || {
        fixture.records().is_ok_and(|records| {
            records
                .iter()
                .filter(|r| matches!(r, Record::JobSettled(s) if jobs.contains(&s.job.to_vec())))
                .count()
                == 2
        })
    })?;
    let event = fixture.finish(operation, Duration::from_secs(60))?;
    assert_eq!(event.kind, Some(5), "{event:?}");
    assert_eq!(event.outlived_jobs, Some(2), "after restart: {event:?}");
    assert_eq!(event.outlived_job_ids, jobs);
    fixture.stop()?;
    println!(
        "PASS J4T-08 integration: a turn cancelled with two live jobs gives outlived_jobs 2 with both IDs, and the same after a service restart"
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
            eprintln!("job control child failed: {error}");
            std::process::exit(1);
        }
        return;
    }
    let selected: Vec<&str> = args[1..]
        .iter()
        .map(String::as_str)
        .filter(|a| !a.starts_with('-'))
        .collect();
    let cases: [Case; 3] = [
        ("follow", follow_and_cancel),
        ("retain", retention),
        ("outlived", outlived),
    ];
    for (name, case) in cases {
        if !selected.is_empty() && !selected.contains(&name) {
            continue;
        }
        if let Err(error) = case() {
            eprintln!("job control flow case {name} FAILED: {error}");
            std::process::exit(1);
        }
    }
    println!("job control flow qualification passed");
}
