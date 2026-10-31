//! The execution path guard through the real service
//! (execution-path-guard.md, packet EPG-W): the real conversation owner,
//! job controller, slot threads, path guard worker, guardians under
//! `sandbox-exec` (none for `!!`), journal and the secret store worker over
//! a scratch keychain file, in private scratch homes. Model jobs come from
//! a scripted loopback model. No case reads or writes `~/.asura` or the
//! login keychain: each home names its own scratch keychain in
//! `secret-store-keychain`, a `!` command's account home and `$TMPDIR` are
//! fixture directories, and a `!!` command's HOME is the scratch home.
//!
//! Cases (by name): `model_change` (EPG-I2, PX-V9), `approval` (EPG-I3,
//! PX-V11, and the `/paths` replies), `restart` (EPG-I4), `stall`
//! (EPG-I5), `overflow` (EPG-I6), `hold` (EPG-I7).
#[path = "support/job_fixture.rs"]
mod job_fixture;
#[path = "support/queue_submit.rs"]
mod queue_submit;

use asura_client::CommandSubmit;
use asura_control::pb;
use asura_platform::keychain::{self, Target};
use asura_storage::authority::conversation::{
    self as journal, ExecutionPathRecord, JobOrigin, JobOutcome, JobSettled, PathAction,
    PathEntryKind, Record,
};
use job_fixture::{Fixture, Model, Result, Step, call, id, wait_for, within_100ms};
use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const CHILD: &str = "--path-guard-child";
const PATH: &str = "/usr/bin:/bin";

// ------------------------------------------------------------ the scratch env

/// A scratch home with its service, scratch keychain and fixture account.
/// Drop stops the service before it deletes the scratch keychain, then the
/// fixture removes the home.
struct Env {
    fixture: Fixture,
    target: Option<Target>,
    keychain: PathBuf,
}
impl Env {
    fn new(label: &str) -> Result<Self> {
        let fixture = Fixture::new(label, CHILD)?;
        let home = fixture.home.clone();
        let keychain = home.join("scratch.keychain");
        let password: String = asura_platform::random_id()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let target = keychain::scratch::create(&keychain, &password)
            .map_err(|e| format!("scratch keychain: {e:?}"))?;
        write_private(
            &home.join("secret-store-keychain"),
            &format!("{}\n{password}\n", keychain.display()),
        )?;
        let account = home.join("account");
        let tmpdir = home.join("account-tmp");
        for dir in [&account, &tmpdir] {
            std::fs::create_dir(dir)?;
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
        }
        write_private(
            &home.join("exec-cmd-person-account"),
            &format!("{}\n{}\n", account.display(), tmpdir.display()),
        )?;
        Ok(Self {
            fixture,
            target: Some(target),
            keychain,
        })
    }
    fn home(&self) -> &Path {
        &self.fixture.home
    }
    fn start(&mut self) -> Result<()> {
        self.fixture.start()?;
        self.fixture.wait_ready()
    }
    fn restart(&mut self) -> Result<()> {
        self.fixture.stop()?;
        self.start()
    }
}
impl Drop for Env {
    fn drop(&mut self) {
        let _ = self.fixture.stop();
        if let Some(target) = self.target.take()
            && self.keychain.exists()
        {
            let _ = keychain::scratch::delete(&target);
        }
    }
}
fn write_private(path: &Path, text: &str) -> Result<()> {
    std::fs::write(path, text)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}

// ------------------------------------------------------------ submits

/// A `!` or `!!` submit with PATH and no terminal variables (ENV1: the
/// same allow-list environment for both modes).
fn request(
    _env: &Env,
    project: [u8; 16],
    command: &str,
    outside: bool,
) -> pb::ConversationQueueCommandSubmit {
    pb::ConversationQueueCommandSubmit {
        request_id: Some(asura_platform::random_id().to_vec()),
        project_id: Some(project.to_vec()),
        conversation_id: None,
        expected_generation: Some(0),
        new_conversation: Some(true),
        command: Some(command.into()),
        cwd: Some(".".into()),
        search_path: Some(PATH.into()),
        environment: Vec::new(),
        outside_sandbox: outside.then_some(true),
    }
}
/// Queues a command; retries `conversation_busy` only. Returns its input.
fn queued(env: &Env, request: &pb::ConversationQueueCommandSubmit) -> Result<[u8; 16]> {
    let end = Instant::now() + Duration::from_secs(20);
    let sent = loop {
        match env
            .fixture
            .attach()?
            .queue_person_command(request.clone(), &|| false)
        {
            Ok(reply) => break reply,
            Err(asura_client::Error::Remote(_, message))
                if message == "conversation_busy" && Instant::now() < end =>
            {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(error) => return Err(error.into()),
        }
    };
    match sent {
        CommandSubmit::Accepted(reply) => id(reply.accepted_input_id),
        other => Err(format!("refused: {other:?}").into()),
    }
}
fn job_of(input: [u8; 16]) -> [u8; 16] {
    journal::job_id(&JobOrigin::Person { input })
}
fn settled(env: &Env, job: [u8; 16]) -> Result<Option<JobSettled>> {
    Ok(env.fixture.records()?.into_iter().find_map(|r| match r {
        Record::JobSettled(s) if s.job == job => Some(*s),
        _ => None,
    }))
}
fn wait_settled(env: &Env, input: [u8; 16]) -> Result<JobSettled> {
    let mut found = None;
    wait_for(Duration::from_secs(60), "kind 28", || {
        found = settled(env, job_of(input)).ok().flatten();
        found.is_some()
    })?;
    Ok(found.expect("settled"))
}
fn intent_committed(env: &Env, input: [u8; 16]) -> Result<bool> {
    let job = job_of(input);
    Ok(env
        .fixture
        .records()?
        .iter()
        .any(|r| matches!(r, Record::JobIntent(i) if journal::job_id(&i.origin) == job)))
}
fn entry(env: &Env, project: [u8; 16], input: [u8; 16]) -> Result<pb::ConversationQueueEntry> {
    env.fixture
        .attach()?
        .conversation_queue(project, None)?
        .entries
        .into_iter()
        .find(|e| e.input_id.as_deref() == Some(&input[..]))
        .ok_or_else(|| "no queue entry".into())
}
fn decide(env: &Env, input: [u8; 16], action: u32) -> Result<pb::ConversationQueueReply> {
    Ok(env.fixture.attach()?.decide_conversation_input(
        pb::ConversationQueueDecision {
            request_id: Some(asura_platform::random_id().to_vec()),
            input_id: Some(input.to_vec()),
            action: Some(action),
            ..Default::default()
        },
        &|| false,
    )?)
}
const RESUME: u32 = 1;
const DROP: u32 = 2;
const HOLD: u32 = 4;

// ------------------------------------------------------------ kind 32

fn path_records(env: &Env) -> Result<Vec<ExecutionPathRecord>> {
    Ok(env
        .fixture
        .records()?
        .into_iter()
        .filter_map(|r| match r {
            Record::ExecutionPath(p) => Some(*p),
            _ => None,
        })
        .collect())
}
/// The kind 32 of one reference, if any.
fn path_record(env: &Env, reference: [u8; 16]) -> Result<Option<ExecutionPathRecord>> {
    Ok(path_records(env)?
        .into_iter()
        .find(|p| p.reference == reference))
}
fn wait_record(env: &Env, reference: [u8; 16], what: &str) -> Result<ExecutionPathRecord> {
    let mut found = None;
    wait_for(Duration::from_secs(60), what, || {
        found = path_record(env, reference).ok().flatten();
        found.is_some()
    })?;
    Ok(found.expect("kind 32"))
}
/// (kind, path) of each entry.
fn entries(record: &ExecutionPathRecord) -> Vec<(PathEntryKind, &str)> {
    record
        .entries
        .iter()
        .map(|e| (e.kind, e.path.as_str()))
        .collect()
}
fn warnings(entry: &pb::ConversationQueueEntry) -> Vec<(u32, &str)> {
    entry
        .path_warnings
        .iter()
        .map(|w| (w.kind.unwrap_or(0), w.path.as_deref().unwrap_or("")))
        .collect()
}

// ------------------------------------------------------------ model jobs

/// One model turn that runs `command` as a model exec_cmd job, then waits
/// for its kind 32 `model_changed` (when `changed`) and the turn's end.
fn model_turn(
    env: &Env,
    model: &Model,
    project: [u8; 16],
    command: &str,
    changed: bool,
) -> Result<Option<ExecutionPathRecord>> {
    model_turn_after(env, model, project, None, command, changed).map(|(record, _)| record)
}
/// [`model_turn`] in the conversation of `previous` when given: a project
/// has at most one pending input in a new conversation, so a turn beside a
/// queued or held command in a new conversation follows an earlier turn.
fn model_turn_after(
    env: &Env,
    model: &Model,
    project: [u8; 16],
    previous: Option<[u8; 16]>,
    command: &str,
    changed: bool,
) -> Result<(Option<ExecutionPathRecord>, [u8; 16])> {
    let before = path_records(env)?.len();
    model.script(&[
        call("exec_cmd", serde_json::json!({"command": command})),
        Step::Final,
    ]);
    let operation = match previous {
        Some(previous) => env.fixture.follow(project, previous, "Edit the build.")?,
        None => env.fixture.submit(project, "Edit the build.")?,
    };
    let event = env.fixture.finish(operation, Duration::from_secs(60))?;
    assert_eq!(event.kind, Some(3), "the model turn completed: {event:?}");
    if !changed {
        return Ok((None, operation));
    }
    let mut found = None;
    wait_for(Duration::from_secs(60), "kind 32 model_changed", || {
        found = path_records(env).ok().and_then(|all| {
            all.into_iter()
                .skip(before)
                .find(|p| p.action == PathAction::ModelChanged)
        });
        found.is_some()
    })?;
    Ok((found, operation))
}

// ------------------------------------------------------------ `/paths`

/// A raw control attachment for the path guard messages, which the
/// client library does not carry yet (EPG-T).
struct Raw {
    stream: std::os::unix::net::UnixStream,
    codec: asura_control::ControlCodec,
    epoch: Vec<u8>,
    attachment: Vec<u8>,
    counter: u64,
}
impl Raw {
    fn attach(env: &Env) -> Result<Self> {
        let runtime = env.fixture.runtime()?;
        let stream = runtime.connect()?.stream().try_clone()?;
        stream.set_nonblocking(false)?;
        // Test-only harness budget; a stalled fixture fails here.
        stream.set_read_timeout(Some(Duration::from_secs(60)))?;
        let mut raw = Self {
            stream,
            codec: asura_control::ControlCodec::new(),
            epoch: Vec::new(),
            attachment: Vec::new(),
            counter: 0,
        };
        raw.write(&pb::Envelope {
            body: Some(pb::envelope::Body::Hello(pb::Hello {
                client_build: Some(job_fixture::BUILD.into()),
            })),
            ..Default::default()
        })?;
        let hello = raw.read()?;
        raw.epoch = hello.service_epoch.ok_or("epoch")?;
        raw.attachment = hello.attachment_id.ok_or("attachment")?;
        Ok(raw)
    }
    fn write(&mut self, envelope: &pb::Envelope) -> Result<()> {
        let frame = asura_control::encode_frame(envelope).map_err(|e| format!("{e:?}"))?;
        self.stream.write_all(&frame)?;
        Ok(())
    }
    fn read(&mut self) -> Result<pb::Envelope> {
        let mut buffer = [0u8; 8192];
        loop {
            if let Some(frame) = self.codec.next_frame().map_err(|e| format!("{e:?}"))? {
                return match frame {
                    asura_control::Frame::Message(envelope) => {
                        asura_control::validate_semantics(
                            &envelope,
                            asura_control::Direction::ServerToClient,
                        )
                        .map_err(|e| format!("reply semantics: {e:?}"))?;
                        Ok(*envelope)
                    }
                    asura_control::Frame::VersionRejected => Err("version rejected".into()),
                };
            }
            let n = self.stream.read(&mut buffer)?;
            if n == 0 {
                return Err("connection closed".into());
            }
            self.codec
                .push(&buffer[..n])
                .map_err(|e| format!("{e:?}"))?;
        }
    }
    fn request(&mut self, body: pb::envelope::Body) -> Result<pb::PathGuardReply> {
        self.counter += 1;
        self.write(&pb::Envelope {
            service_epoch: Some(self.epoch.clone()),
            attachment_id: Some(self.attachment.clone()),
            request_counter: Some(self.counter),
            body: Some(body),
        })?;
        match self.read()?.body {
            Some(pb::envelope::Body::PathGuardReply(reply)) => Ok(reply),
            other => Err(format!("reply: {other:?}").into()),
        }
    }
}
fn list(env: &Env, project: [u8; 16]) -> Result<pb::PathGuardReply> {
    Raw::attach(env)?.request(pb::envelope::Body::PathGuardList(pb::PathGuardList {
        project_id: Some(project.to_vec()),
    }))
}
fn approve(
    env: &Env,
    project: [u8; 16],
    request_id: [u8; 16],
    path: Option<&str>,
) -> Result<pb::PathGuardReply> {
    Raw::attach(env)?.request(pb::envelope::Body::PathGuardApprove(pb::PathGuardApprove {
        request_id: Some(request_id.to_vec()),
        project_id: Some(project.to_vec()),
        path: path.map(str::to_owned),
        all: path.is_none().then_some(true),
    }))
}
fn revoke(env: &Env, project: [u8; 16], path: &str) -> Result<pb::PathGuardReply> {
    Raw::attach(env)?.request(pb::envelope::Body::PathGuardRevoke(pb::PathGuardRevoke {
        request_id: Some(asura_platform::random_id().to_vec()),
        project_id: Some(project.to_vec()),
        path: Some(path.into()),
    }))
}
fn pending(reply: &pb::PathGuardReply) -> Vec<(u32, &str)> {
    reply
        .pending
        .iter()
        .map(|w| (w.kind.unwrap_or(0), w.path.as_deref().unwrap_or("")))
        .collect()
}

const MODEL_CHANGED: u32 = 1;
const NOT_APPROVED: u32 = 2;
const CHANGED_SINCE_APPROVAL: u32 = 3;
const TOO_LARGE: u32 = 4;
const OVERFLOW: u32 = 5;

// ------------------------------------------------------------ cases

/// EPG-I2 (PX-V9): a real model job edits `build.rs` and `package.json`;
/// the first `!` command's entry and acknowledgement list both; the
/// second lists none.
fn model_change() -> Result<()> {
    let model = Model::start()?;
    let mut env = Env::new("pg-model")?;
    env.start()?;
    let (project, directory) = env.fixture.project("project")?;
    std::fs::write(directory.join("build.rs"), b"fn main() {}\n")?;
    env.fixture.model(&model.endpoint)?;
    let change = model_turn(
        &env,
        &model,
        project,
        "printf '// edited\\n' >> build.rs; printf '{}' > package.json; printf x > notes.txt",
        true,
    )?
    .expect("change");
    assert_eq!(
        entries(&change),
        [
            (PathEntryKind::ModelChanged, "build.rs"),
            (PathEntryKind::ModelChanged, "package.json"),
        ],
        "only build and test paths: {change:?}"
    );
    assert_eq!((change.project, change.overflow), (project, 0));
    // The first command: both, on the entry and in its acknowledgement,
    // which precedes its kind 26.
    let first = queued(&env, &request(&env, project, "sleep 1; echo first", false))?;
    let ack = wait_record(&env, first, "kind 32 acknowledged")?;
    assert_eq!(ack.action, PathAction::Acknowledged);
    assert_eq!(
        entries(&ack),
        [
            (PathEntryKind::ModelChanged, "build.rs"),
            (PathEntryKind::ModelChanged, "package.json"),
        ]
    );
    let shown = entry(&env, project, first)?;
    assert_eq!(
        warnings(&shown),
        [(MODEL_CHANGED, "build.rs"), (MODEL_CHANGED, "package.json")],
        "{shown:?}"
    );
    let all = env.fixture.records()?;
    let ack_at = all
        .iter()
        .position(|r| matches!(r, Record::ExecutionPath(p) if p.reference == first))
        .ok_or("ack")?;
    let intent_at = all
        .iter()
        .position(
            |r| matches!(r, Record::JobIntent(i) if i.origin == JobOrigin::Person { input: first }),
        )
        .ok_or("kind 26")?;
    assert!(ack_at < intent_at, "the acknowledgement precedes kind 26");
    let s = wait_settled(&env, first)?;
    assert_eq!(s.outcome, JobOutcome::Exited, "the command ran: {s:?}");
    // The second command: none.
    let second = queued(&env, &request(&env, project, "echo second", false))?;
    let ack = wait_record(&env, second, "second acknowledgement")?;
    assert!(ack.entries.is_empty(), "{ack:?}");
    assert_eq!(ack.through, change_sequence(&env, &change)?);
    assert_eq!(wait_settled(&env, second)?.outcome, JobOutcome::Exited);
    assert!(warnings(&entry(&env, project, second)?).is_empty());
    // A model job that changes no build or test path writes no kind 32.
    let before = path_records(&env)?.len();
    model_turn(&env, &model, project, "printf y > notes.txt", false)?;
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(
        path_records(&env)?
            .iter()
            .skip(before)
            .filter(|p| p.action == PathAction::ModelChanged)
            .count(),
        0
    );
    println!("PASS EPG-I2: the first command listed build.rs and package.json, the second none");
    Ok(())
}

/// The frame sequence of a committed kind 32 record.
fn change_sequence(env: &Env, record: &ExecutionPathRecord) -> Result<u64> {
    let bytes = asura_storage::authority::fixture::journal_bytes(&env.fixture.home)?;
    let mut offset = 0;
    while offset < bytes.len() {
        let length = u32::from_be_bytes(bytes[offset + 12..offset + 16].try_into()?) as usize;
        let frame = &bytes[offset..offset + length];
        let sequence = u64::from_be_bytes(frame[16..24].try_into()?);
        if let Ok(Record::ExecutionPath(p)) = journal::decode_record(
            u16::from_be_bytes([frame[10], frame[11]]),
            &frame[144..frame.len() - 40],
        ) && *p == *record
        {
            return Ok(sequence);
        }
        offset += length;
    }
    Err("record not found".into())
}

/// EPG-I3 (PX-V11), and the `/paths` replies: approve a hook; change it
/// with `!`; the next command warns `changed_since_approval`.
fn approval() -> Result<()> {
    let mut env = Env::new("pg-approve")?;
    env.start()?;
    let (project, directory) = env.fixture.project("project")?;
    let hooks = directory.join(".git/hooks");
    std::fs::create_dir_all(&hooks)?;
    std::fs::write(hooks.join("pre-commit"), b"#!/bin/sh\nexit 0\n")?;
    std::fs::write(directory.join(".envrc"), b"export A=1\n")?;
    std::fs::write(hooks.join("big"), vec![b'x'; 1024 * 1024 + 1])?;
    // The list: unapproved files and the large one.
    let listed = list(&env, project)?;
    assert_eq!(
        pending(&listed),
        [
            (NOT_APPROVED, ".envrc"),
            (NOT_APPROVED, ".git/hooks/pre-commit"),
            (TOO_LARGE, ".git/hooks/big"),
        ],
        "{listed:?}"
    );
    assert!(listed.approved.is_empty() && listed.error.is_none());
    // Request errors.
    let one = |path: &str| approve(&env, project, asura_platform::random_id(), Some(path));
    assert_eq!(one("src/main.rs")?.error.as_deref(), Some("not_implicit"));
    assert_eq!(one("/etc/passwd")?.error.as_deref(), Some("path_invalid"));
    assert_eq!(one("a/../.envrc")?.error.as_deref(), Some("path_invalid"));
    assert_eq!(
        one(".git/hooks/absent")?.error.as_deref(),
        Some("not_found")
    );
    assert_eq!(one(".git/hooks/big")?.error.as_deref(), Some("too_large"));
    assert_eq!(
        revoke(&env, project, ".envrc")?.error.as_deref(),
        Some("not_found")
    );
    assert_eq!(
        list(&env, [9; 16])?.error.as_deref(),
        Some("not_found"),
        "an unknown project"
    );
    // Approve the hook; an exact retry gives the same reply and record.
    let request_id = asura_platform::random_id();
    let approved = approve(&env, project, request_id, Some(".git/hooks/pre-commit"))?;
    assert_eq!(approved.error, None, "{approved:?}");
    assert_eq!(approved.approved, [".git/hooks/pre-commit"]);
    assert_eq!(
        pending(&approved),
        [(NOT_APPROVED, ".envrc"), (TOO_LARGE, ".git/hooks/big")]
    );
    let retried = approve(&env, project, request_id, Some(".git/hooks/pre-commit"))?;
    assert_eq!(retried, approved);
    let record = path_record(&env, request_id)?.ok_or("kind 32 approved")?;
    assert_eq!(record.action, PathAction::Approved);
    assert_ne!(record.digest, [0; 32]);
    assert_eq!(
        path_records(&env)?
            .iter()
            .filter(|p| p.action == PathAction::Approved)
            .count(),
        1,
        "the retry appended nothing"
    );
    // A command with the hook unchanged: no hook warning.
    let first = queued(&env, &request(&env, project, "echo first", false))?;
    let ack = wait_record(&env, first, "first acknowledgement")?;
    assert_eq!(
        entries(&ack),
        [
            (PathEntryKind::NotApproved, ".envrc"),
            (PathEntryKind::TooLarge, ".git/hooks/big"),
        ]
    );
    wait_settled(&env, first)?;
    // PX-V11: change it with `!`.
    let change = queued(
        &env,
        &request(
            &env,
            project,
            "printf 'echo hi\\n' >> .git/hooks/pre-commit",
            false,
        ),
    )?;
    let s = wait_settled(&env, change)?;
    assert_eq!((s.outcome, s.exit_value), (JobOutcome::Exited, 0), "{s:?}");
    let next = queued(&env, &request(&env, project, "echo next", false))?;
    let ack = wait_record(&env, next, "next acknowledgement")?;
    assert_eq!(
        entries(&ack),
        [
            (PathEntryKind::NotApproved, ".envrc"),
            (PathEntryKind::ChangedSinceApproval, ".git/hooks/pre-commit"),
            (PathEntryKind::TooLarge, ".git/hooks/big"),
        ],
        "PX-V11"
    );
    assert_eq!(
        warnings(&entry(&env, project, next)?)[1],
        (CHANGED_SINCE_APPROVAL, ".git/hooks/pre-commit")
    );
    wait_settled(&env, next)?;
    // Kinds 2-3 repeat before each command until approved.
    let again = queued(&env, &request(&env, project, "echo again", false))?;
    assert_eq!(wait_record(&env, again, "again")?.entries.len(), 3);
    wait_settled(&env, again)?;
    // `approve all` approves the changed hook and `.envrc`, not the large
    // file; its retry appends nothing.
    let all_id = asura_platform::random_id();
    let all = approve(&env, project, all_id, None)?;
    assert_eq!(all.error, None, "{all:?}");
    assert_eq!(all.approved, [".envrc", ".git/hooks/pre-commit"]);
    assert_eq!(pending(&all), [(TOO_LARGE, ".git/hooks/big")]);
    let count = path_records(&env)?.len();
    assert_eq!(approve(&env, project, all_id, None)?, all);
    assert_eq!(path_records(&env)?.len(), count);
    // Revoke: the file is unapproved again.
    let revoked = revoke(&env, project, ".envrc")?;
    assert_eq!(revoked.approved, [".git/hooks/pre-commit"]);
    assert_eq!(
        pending(&revoked),
        [(NOT_APPROVED, ".envrc"), (TOO_LARGE, ".git/hooks/big")]
    );
    // A missing approved file is changed since approval.
    std::fs::remove_file(hooks.join("pre-commit"))?;
    assert_eq!(
        pending(&list(&env, project)?),
        [
            (NOT_APPROVED, ".envrc"),
            (CHANGED_SINCE_APPROVAL, ".git/hooks/pre-commit"),
            (TOO_LARGE, ".git/hooks/big"),
        ]
    );
    // The bound: 256 approved paths, then `limit`.
    std::fs::remove_file(hooks.join("big"))?;
    std::fs::write(hooks.join("pre-commit"), b"#!/bin/sh\n")?;
    for i in 0..260 {
        std::fs::write(hooks.join(format!("h{i:03}")), b"x")?;
    }
    let all = approve(&env, project, asura_platform::random_id(), None)?;
    assert_eq!(all.error, None, "{all:?}");
    assert_eq!(all.approved.len(), 256, "the walk keeps 256 implicit paths");
    assert_eq!(pending(&all).last(), Some(&(OVERFLOW, ".")));
    let over = one(".git/hooks/h259")?;
    assert_eq!(over.error.as_deref(), Some("limit"), "{over:?}");
    println!(
        "PASS EPG-I3: approved hook changed by ! warned changed_since_approval; list, approve, approve all, revoke, retries and errors"
    );
    Ok(())
}

/// EPG-I4: a restart between a model change and the person command keeps
/// the warning.
fn restart() -> Result<()> {
    let model = Model::start()?;
    let mut env = Env::new("pg-restart")?;
    env.start()?;
    let (project, _) = env.fixture.project("project")?;
    env.fixture.model(&model.endpoint)?;
    model_turn(
        &env,
        &model,
        project,
        "printf 'fn main(){}' > build.rs",
        true,
    )?;
    env.restart()?;
    let input = queued(&env, &request(&env, project, "echo after", false))?;
    let ack = wait_record(&env, input, "acknowledgement after restart")?;
    assert_eq!(entries(&ack), [(PathEntryKind::ModelChanged, "build.rs")]);
    assert_eq!(wait_settled(&env, input)?.outcome, JobOutcome::Exited);
    println!("PASS EPG-I4: the model change replayed and warned after a restart");
    Ok(())
}

/// EPG-I5: a stalled filesystem during a check: the entry waits with
/// `path_check`; status, lists, a Drop and a model turn in another lane
/// respond within 100 ms (the model turn completes).
fn stall() -> Result<()> {
    let model = Model::start()?;
    let mut env = Env::new("pg-stall")?;
    env.start()?;
    let (project, _) = env
        .fixture
        .project("project")
        .map_err(|e| format!("project: {e}"))?;
    env.fixture
        .model(&model.endpoint)
        .map_err(|e| format!("model: {e}"))?;
    // A conversation for the other lane.
    model.script(&[Step::Final]);
    let first = env
        .fixture
        .submit(project, "First lane.")
        .map_err(|e| format!("first: {e}"))?;
    env.fixture.finish(first, Duration::from_secs(60))?;
    let gate = env.home().join("path-guard-hold");
    std::fs::write(&gate, b"")?;
    let waiting = queued(&env, &request(&env, project, "echo waiting", false))
        .map_err(|e| format!("queued: {e}"))?;
    wait_for(Duration::from_secs(30), "path_check", || {
        entry(&env, project, waiting)
            .is_ok_and(|e| e.waiting_reason == Some(pb::QueueWaitReason::PathCheck as i32))
    })?;
    let shown = entry(&env, project, waiting).map_err(|e| format!("entry: {e}"))?;
    assert_eq!(shown.waiting_text.as_deref(), Some("waiting: path check"));
    assert_eq!(shown.state, Some(1));
    for _ in 0..5 {
        within_100ms("inspect", || Ok(env.fixture.attach()?.inspect()?))?;
        within_100ms("queue list", || {
            Ok(env.fixture.attach()?.conversation_queue(project, None)?)
        })?;
    }
    // Another lane: a model turn runs and completes while the check waits.
    model.script(&[Step::Final]);
    let operation = env
        .fixture
        .follow(project, first, "Another lane.")
        .map_err(|e| format!("model submit: {e}"))?;
    let event = env.fixture.finish(operation, Duration::from_secs(60))?;
    assert_eq!(event.kind, Some(3), "{event:?}");
    // The person holds the waiting entry (the check is cancelled), then
    // drops it.
    let held =
        within_100ms("hold", || decide(&env, waiting, HOLD)).map_err(|e| format!("hold: {e}"))?;
    let shown = held
        .entries
        .iter()
        .find(|e| e.input_id.as_deref() == Some(&waiting[..]))
        .ok_or("held entry")?;
    assert_eq!(shown.state, Some(2), "{shown:?}");
    within_100ms("drop", || decide(&env, waiting, DROP)).map_err(|e| format!("drop: {e}"))?;
    assert!(!intent_committed(&env, waiting)?);
    assert!(path_record(&env, waiting)?.is_none());
    // The next command waits behind the stalled worker, never refused,
    // and runs when the filesystem answers.
    let behind = queued(&env, &request(&env, project, "echo behind", false))
        .map_err(|e| format!("behind: {e}"))?;
    wait_for(Duration::from_secs(30), "path_check", || {
        entry(&env, project, behind)
            .is_ok_and(|e| e.waiting_reason == Some(pb::QueueWaitReason::PathCheck as i32))
    })?;
    std::fs::remove_file(&gate)?;
    assert_eq!(wait_settled(&env, behind)?.outcome, JobOutcome::Exited);
    assert!(
        !intent_committed(&env, waiting)?,
        "nothing ran for the dropped entry"
    );
    println!(
        "PASS EPG-I5: the stalled check waited with path_check; status, lists, Hold, Drop and a model turn stayed responsive"
    );
    Ok(())
}

/// EPG-I6: 1,100 test files: the snapshot passes its bound; the first
/// 1,024 are still compared; the warning includes `overflow`.
fn overflow() -> Result<()> {
    let model = Model::start()?;
    let mut env = Env::new("pg-overflow")?;
    env.start()?;
    let (project, directory) = env.fixture.project("project")?;
    std::fs::create_dir(directory.join("tests"))?;
    for i in 0..1_100 {
        std::fs::write(directory.join(format!("tests/t{i:04}.rs")), b"")?;
    }
    env.fixture.model(&model.endpoint)?;
    let change = model_turn(
        &env,
        &model,
        project,
        "printf x >> tests/t0005.rs; printf x >> tests/t1050.rs",
        true,
    )?
    .expect("change");
    assert_eq!(
        entries(&change),
        [(PathEntryKind::ModelChanged, "tests/t0005.rs")]
    );
    assert_eq!(change.overflow, 1, "the bound, not t1050: {change:?}");
    let input = queued(&env, &request(&env, project, "echo after", false))?;
    let ack = wait_record(&env, input, "acknowledgement")?;
    assert_eq!(
        entries(&ack),
        [
            (PathEntryKind::ModelChanged, "tests/t0005.rs"),
            (PathEntryKind::Overflow, "."),
        ]
    );
    assert_eq!(
        warnings(&entry(&env, project, input)?),
        [(MODEL_CHANGED, "tests/t0005.rs"), (OVERFLOW, ".")]
    );
    wait_settled(&env, input)?;
    println!("PASS EPG-I6: 1,100 test files: the first 1,024 compared, overflow warned");
    Ok(())
}

/// EPG-I7: a `!!` command after a model change is held `paths_changed`
/// with no kind 32 and no kind 26; a model change while it is held holds
/// it again with the new list; Resume then appends kind 32 and runs it;
/// Drop drops a held one with no kind 26; a restart keeps one held; a `!!`
/// command with only a `not_approved` warning runs without a hold.
fn hold() -> Result<()> {
    let model = Model::start()?;
    let mut env = Env::new("pg-hold")?;
    env.start()?;
    let (project, directory) = env.fixture.project("project")?;
    std::fs::create_dir_all(directory.join(".git"))?;
    std::fs::write(directory.join(".git/config"), b"[core]\n")?;
    env.fixture.model(&model.endpoint)?;
    let (_, first) = model_turn_after(
        &env,
        &model,
        project,
        None,
        "printf 'all:\\n' > Makefile",
        true,
    )?;
    let held = queued(&env, &request(&env, project, "echo held", true))?;
    let held_entry = |env: &Env| -> Result<pb::ConversationQueueEntry> {
        let mut found = None;
        wait_for(Duration::from_secs(30), "held paths_changed", || {
            found = entry(env, project, held)
                .ok()
                .filter(|e| e.state == Some(2));
            found.is_some()
        })?;
        Ok(found.expect("held"))
    };
    let shown = held_entry(&env)?;
    assert_eq!(
        shown.hold_reason.as_deref(),
        Some("paths_changed"),
        "{shown:?}"
    );
    assert_eq!(
        warnings(&shown),
        [(MODEL_CHANGED, "Makefile"), (NOT_APPROVED, ".git/config")]
    );
    assert!(path_record(&env, held)?.is_none(), "no kind 32 while held");
    assert!(!intent_committed(&env, held)?, "no kind 26 while held");
    // A model change while it is held: Resume holds it again, with the new
    // list.
    let (_, second) = model_turn_after(
        &env,
        &model,
        project,
        Some(first),
        "printf 'x:\\n' > a.mk",
        true,
    )?;
    decide(&env, held, RESUME)?;
    wait_for(Duration::from_secs(30), "held again", || {
        entry(&env, project, held).is_ok_and(|e| {
            e.state == Some(2)
                && warnings(&e).contains(&(MODEL_CHANGED, "a.mk"))
                && e.hold_reason.as_deref() == Some("paths_changed")
        })
    })?;
    assert!(path_record(&env, held)?.is_none() && !intent_committed(&env, held)?);
    // Resume with nothing newer: kind 32, then kind 26, and it runs.
    decide(&env, held, RESUME)?;
    let ack = wait_record(&env, held, "acknowledgement after Resume")?;
    assert_eq!(
        entries(&ack),
        [
            (PathEntryKind::ModelChanged, "Makefile"),
            (PathEntryKind::ModelChanged, "a.mk"),
            (PathEntryKind::NotApproved, ".git/config"),
        ]
    );
    let s = wait_settled(&env, held)?;
    assert_eq!((s.outcome, s.exit_value), (JobOutcome::Exited, 0), "{s:?}");
    // Drop: a held one never runs.
    model_turn_after(
        &env,
        &model,
        project,
        Some(second),
        "printf 'y:\\n' >> Makefile",
        true,
    )?;
    let dropped = queued(&env, &request(&env, project, "echo dropped", true))?;
    wait_for(Duration::from_secs(30), "held", || {
        entry(&env, project, dropped).is_ok_and(|e| e.state == Some(2))
    })?;
    decide(&env, dropped, DROP)?;
    // Restart keeps a held one held.
    let kept = queued(&env, &request(&env, project, "echo kept", true))?;
    wait_for(Duration::from_secs(30), "held", || {
        entry(&env, project, kept).is_ok_and(|e| e.state == Some(2))
    })?;
    env.restart()?;
    std::thread::sleep(Duration::from_millis(500));
    assert_eq!(
        entry(&env, project, kept)?.state,
        Some(2),
        "held after restart"
    );
    assert!(!intent_committed(&env, dropped)? && !intent_committed(&env, kept)?);
    assert_eq!(entry(&env, project, dropped)?.state, Some(6), "dropped");
    // After the restart the hold shows its warnings again on Resume, and a
    // second Resume runs it.
    decide(&env, kept, RESUME)?;
    wait_for(Duration::from_secs(30), "held again after restart", || {
        entry(&env, project, kept).is_ok_and(|e| {
            e.state == Some(2) && warnings(&e).contains(&(MODEL_CHANGED, "Makefile"))
        })
    })?;
    decide(&env, kept, RESUME)?;
    assert_eq!(wait_settled(&env, kept)?.outcome, JobOutcome::Exited);
    // Only `not_approved`: no hold.
    let last = queued(&env, &request(&env, project, "echo last", true))?;
    let ack = wait_record(&env, last, "last acknowledgement")?;
    assert_eq!(entries(&ack), [(PathEntryKind::NotApproved, ".git/config")]);
    assert_eq!(wait_settled(&env, last)?.outcome, JobOutcome::Exited);
    println!(
        "PASS EPG-I7: !! held paths_changed, held again on a newer change, ran on Resume, dropped, kept held across a restart; not_approved alone did not hold"
    );
    Ok(())
}

type Case = fn() -> Result<()>;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if let [_, flag, job, profile, command] = args.as_slice()
        && flag == "--asura-exec-cmd-guardian"
    {
        std::process::exit(asura_platform::exec_cmd::run_guardian(
            job, profile, command,
        ));
    }
    if args.get(1).map(String::as_str) == Some("--asura-exec-cmd-guardian") {
        std::process::exit(2);
    }
    if args.get(1).map(String::as_str) == Some(CHILD) {
        if let Err(error) = job_fixture::child(Path::new(&args[2])) {
            eprintln!("path guard child failed: {error}");
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
        ("model_change", model_change),
        ("approval", approval),
        ("restart", restart),
        ("stall", stall),
        ("overflow", overflow),
        ("hold", hold),
    ];
    for (name, case) in cases {
        if !selected.is_empty() && !selected.contains(&name) {
            continue;
        }
        if let Err(error) = case() {
            eprintln!("path guard flow case {name} FAILED: {error}");
            std::process::exit(1);
        }
    }
    println!("path guard flow qualification passed");
}
