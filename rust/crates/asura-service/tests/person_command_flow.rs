//! Person commands (`!` and `!!`) through the real service
//! (person-command-mode.md, packet P3-E; secret-store.md, SL5): the real
//! conversation owner, job controller, slot and capture workers, guardians
//! under `sandbox-exec` (or none for `!!`), journal, payload store and the
//! secret store worker over a scratch keychain file, in private scratch
//! homes. No case reads or writes `~/.asura` or the login keychain: each
//! home names its own scratch keychain in `secret-store-keychain`, and a
//! `!` command's account home and `$TMPDIR` are fixture directories
//! (`exec-cmd-person-account`). A `!!` command gets a fixture launch
//! environment whose HOME is the scratch home.
//!
//! Cases (by name): `basic` (PC-I1, PC-I17), `refusals` (PC-U43 submit,
//! PC-I31, refusals), `secrets` (SS-I1, PC-I28, SS-I11), `restart`
//! (SS-I2, PC-I11, PC-I32, SDI-10), `orphan` (SS-I3), `locked` (SS-I4,
//! SS-I5), `lost` (SS-I6, SS-I7), `outside` (PC-I23, PC-I30), `stop`
//! (PC-I7, PC-I8, PC-I24), `survivors` (PC-I33, PC-I34, PC-I40, EC-I9,
//! EC-I10), `interrupt` (PC-I10, PC-I26, PC-I35), `gate` (SS-I8, SS-I9,
//! in-process service), `full` (SS-I10), `profile` (PC-I2 to PC-I5,
//! PC-I14, PC-I29, PC-I36), `observer` (field 8), `long` (PC-I9, PC-I25).
#[path = "support/job_fixture.rs"]
mod job_fixture;
#[path = "support/queue_submit.rs"]
mod queue_submit;

use asura_client::CommandSubmit;
use asura_control::pb;
use asura_platform::keychain::{self, Keychain, Target};
use asura_storage::authority::conversation::{
    self as journal, CaptureReason, JobCaptureState, JobOrigin, JobOutcome, JobReason, JobSettled,
    PersonCommandQueued, Record,
};
use job_fixture::{Fixture, Result, holds, id, wait_for};
use sha2::{Digest, Sha256};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const CHILD: &str = "--person-command-child";
const PATH: &str = "/usr/bin:/bin";

// ------------------------------------------------------------ the scratch env

/// A scratch home with its service, scratch keychain and fixture account.
/// Drop stops the service before it deletes the scratch keychain (testing
/// rule 2), then the fixture removes the home.
struct Env {
    fixture: Fixture,
    target: Option<Target>,
    keychain: PathBuf,
    /// The `!` command's fixture account home and `$TMPDIR`.
    account: PathBuf,
    tmpdir: PathBuf,
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
            account,
            tmpdir,
        })
    }
    fn home(&self) -> &Path {
        &self.fixture.home
    }
    fn start(&mut self) -> Result<()> {
        self.fixture.start()?;
        self.fixture.wait_ready()
    }
    /// Starts with the audit log enabled (a startup setting).
    fn start_audited(&mut self) -> Result<()> {
        self.start()?;
        let reply = self.fixture.config("audit.enabled", "true")?;
        assert!(reply.error.is_none(), "{reply:?}");
        self.fixture.stop()?;
        self.start()
    }
    fn target(&self) -> &Target {
        self.target.as_ref().expect("scratch keychain")
    }
    /// The installation's items in the scratch keychain, read by this test
    /// binary (the service is the same executable).
    fn items(&self) -> Result<Vec<[u8; 16]>> {
        let installation = self.installation()?;
        let listing = Keychain::open(self.target())
            .and_then(|k| k.list(&installation))
            .map_err(|e| format!("list: {e:?}"))?;
        Ok(listing.inputs)
    }
    fn installation(&self) -> Result<[u8; 16]> {
        Ok(self.fixture.journal()?.1.installation_id)
    }
    fn project(&self, name: &str) -> Result<([u8; 16], PathBuf)> {
        self.fixture.project(name)
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

/// A `!` submit (PATH and no client variables), or a `!!` submit with
/// `launch` as its launch environment.
fn request(
    project: [u8; 16],
    conversation: Option<[u8; 16]>,
    command: &str,
    launch: Option<&[(&str, &str)]>,
) -> pb::ConversationQueueCommandSubmit {
    pb::ConversationQueueCommandSubmit {
        request_id: Some(asura_platform::random_id().to_vec()),
        project_id: Some(project.to_vec()),
        conversation_id: conversation.map(|c| c.to_vec()),
        expected_generation: Some(0),
        new_conversation: Some(conversation.is_none()),
        command: Some(command.into()),
        cwd: Some(".".into()),
        search_path: launch.is_none().then(|| PATH.into()),
        environment: Vec::new(),
        outside_sandbox: launch.is_some().then_some(true),
        launch_environment: launch
            .unwrap_or_default()
            .iter()
            .map(|(name, value)| pb::LaunchVariable {
                name: Some(name.as_bytes().to_vec()),
                value: Some(value.as_bytes().to_vec()),
            })
            .collect(),
    }
}
/// A fixture launch environment: PATH, the scratch HOME and `extra`.
fn launch<'a>(env: &'a Env, extra: &[(&'a str, &'a str)]) -> Vec<(&'a str, String)> {
    let mut out = vec![
        ("PATH", PATH.to_string()),
        ("HOME", env.home().display().to_string()),
    ];
    out.extend(extra.iter().map(|(n, v)| (*n, (*v).to_string())));
    out
}
fn outside(
    project: [u8; 16],
    conversation: Option<[u8; 16]>,
    command: &str,
    environment: &[(&str, String)],
) -> pb::ConversationQueueCommandSubmit {
    let pairs: Vec<(&str, &str)> = environment.iter().map(|(n, v)| (*n, v.as_str())).collect();
    request(project, conversation, command, Some(&pairs))
}
/// Sends a submit; retries `conversation_busy` only (the serialized ticket).
fn send(env: &Env, request: &pb::ConversationQueueCommandSubmit) -> Result<CommandSubmit> {
    let end = Instant::now() + Duration::from_secs(20);
    loop {
        match env
            .fixture
            .attach()?
            .queue_person_command(request.clone(), &|| false)
        {
            Ok(reply) => return Ok(reply),
            Err(asura_client::Error::Remote(_, message))
                if message == "conversation_busy" && Instant::now() < end =>
            {
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(error) => return Err(error.into()),
        }
    }
}
/// Sends a submit that must be accepted; returns (input, conversation).
fn queued(env: &Env, request: &pb::ConversationQueueCommandSubmit) -> Result<([u8; 16], [u8; 16])> {
    match send(env, request)? {
        CommandSubmit::Accepted(reply) => {
            let input = id(reply.accepted_input_id.clone())?;
            let entry = reply
                .entries
                .iter()
                .find(|e| e.input_id.as_deref() == Some(&input[..]))
                .ok_or("accepted entry")?;
            Ok((input, id(entry.conversation_id.clone())?))
        }
        other => Err(format!("refused: {other:?}").into()),
    }
}
fn refused(env: &Env, request: &pb::ConversationQueueCommandSubmit) -> Result<pb::QueueRefusal> {
    match send(env, request)? {
        CommandSubmit::Refused { refusal, .. } => Ok(refusal),
        other => Err(format!("accepted: {other:?}").into()),
    }
}
fn job_of(input: [u8; 16]) -> [u8; 16] {
    journal::job_id(&JobOrigin::Person { input })
}

// ------------------------------------------------------------ journal reads

fn records(env: &Env) -> Result<Vec<Record>> {
    env.fixture.records()
}
fn settled(env: &Env, input: [u8; 16]) -> Result<Option<JobSettled>> {
    let job = job_of(input);
    Ok(records(env)?.into_iter().find_map(|r| match r {
        Record::JobSettled(s) if s.job == job => Some(*s),
        _ => None,
    }))
}
fn wait_settled(env: &Env, input: [u8; 16], limit: Duration) -> Result<JobSettled> {
    let mut found = None;
    wait_for(limit, "kind 28", || {
        found = settled(env, input).ok().flatten();
        found.is_some()
    })?;
    Ok(found.expect("settled"))
}
fn intent_committed(env: &Env, input: [u8; 16]) -> Result<bool> {
    let job = job_of(input);
    Ok(records(env)?
        .iter()
        .any(|r| matches!(r, Record::JobIntent(i) if journal::job_id(&i.origin) == job)))
}
fn kind_31(env: &Env, input: [u8; 16]) -> Result<PersonCommandQueued> {
    records(env)?
        .into_iter()
        .find_map(|r| match r {
            Record::PersonCommandQueued(p) if p.input == input => Some(*p),
            _ => None,
        })
        .ok_or_else(|| "no kind 31".into())
}
/// The stored stdout of a settled job (installed after kind 28, R2).
fn stdout(env: &Env, input: [u8; 16]) -> Result<Vec<u8>> {
    let installation = env.installation()?;
    let [payload, _] = asura_storage::payload::job_payloads(installation, job_of(input));
    let path = env
        .home()
        .join(".asura/payloads/objects")
        .join(job_fixture::hex(&payload));
    wait_for(Duration::from_secs(20), "stdout installed", || {
        path.exists()
    })?;
    Ok(std::fs::read(path)?)
}
/// The queue entry of `input` as the service lists it.
fn entry(env: &Env, project: [u8; 16], input: [u8; 16]) -> Result<pb::ConversationQueueEntry> {
    let reply = env.fixture.attach()?.conversation_queue(project, None)?;
    reply
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
fn audit_text(home: &Path) -> Result<String> {
    let mut text = String::new();
    for path in walk(&home.join(".asura"))? {
        if path.to_string_lossy().contains("audit") && path.is_file() {
            text.push_str(&String::from_utf8_lossy(&std::fs::read(path)?));
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
/// Fixture secrets assembled at run time, so no token is committed.
fn token(seed: &str) -> String {
    ["ghp_", &seed.repeat(12)].concat()
}
fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
/// A `!` blocker that runs until `release` exists in the project.
const BLOCKER: &str = "while [ ! -e release ]; do /bin/sleep 0.1; done";

// ------------------------------------------------------------------ cases

/// PC-I1 and PC-I17: `echo hi` queues kind 31, then a person-origin kind 26
/// and kind 28 exit 0 with stdout `hi\n`, one line, and the digest of the
/// stream. An exact retry of the request ID returns the same input and
/// queues nothing; a changed retry conflicts. The audit log records
/// `person_command.queued` with no command text.
fn basic() -> Result<()> {
    let mut env = Env::new("pc-basic")?;
    env.start_audited()?;
    let (project, _) = env.project("project")?;
    let submit = request(project, None, "echo hi", None);
    let (input, _) = queued(&env, &submit)?;
    let settled = wait_settled(&env, input, Duration::from_secs(60))?;
    assert_eq!(settled.outcome, JobOutcome::Exited, "{settled:?}");
    assert_eq!(settled.exit_value, 0);
    assert_eq!(settled.stdout.lines, 1);
    assert_eq!(settled.stdout.capture.stored_bytes, 3);
    let digest: [u8; 32] = Sha256::digest(b"hi\n").into();
    assert_eq!(settled.stdout.capture.digest, Some(digest));
    assert_eq!(stdout(&env, input)?, b"hi\n");
    // Record order: kind 31, then kind 26 with person origin, then kind 28.
    let all = records(&env)?;
    let position = |f: &dyn Fn(&Record) -> bool| all.iter().position(|r| f(r));
    let q = position(&|r| matches!(r, Record::PersonCommandQueued(p) if p.input == input))
        .ok_or("kind 31")?;
    let i =
        position(&|r| matches!(r, Record::JobIntent(i) if i.origin == JobOrigin::Person { input }))
            .ok_or("kind 26")?;
    let s = position(&|r| matches!(r, Record::JobSettled(s) if s.job == job_of(input)))
        .ok_or("kind 28")?;
    assert!(q < i && i < s, "order {q} {i} {s}");
    // PC-I17: the exact retry returns the original input; nothing new.
    let (again, _) = queued(&env, &submit)?;
    assert_eq!(again, input);
    let count = records(&env)?
        .iter()
        .filter(|r| matches!(r, Record::PersonCommandQueued(_)))
        .count();
    assert_eq!(count, 1, "one input for one request ID");
    let mut changed = submit.clone();
    changed.command = Some("echo other".into());
    match send(&env, &changed) {
        Err(error) => assert!(error.to_string().contains("request_conflict"), "{error}"),
        Ok(other) => return Err(format!("changed retry accepted: {other:?}").into()),
    }
    let audit = audit_text(env.home())?;
    assert!(audit.contains("person_command.queued"), "{audit}");
    assert!(audit.contains("person_command.refused"), "{audit}");
    assert!(!audit.contains("echo hi"), "no command text in audit");
    println!(
        "PASS PC-I1 PC-I17: kind 31, person kind 26 and kind 28 exit 0 with hi; retry and conflict"
    );
    Ok(())
}

/// PC-U43 (submit part), PC-I31 and refusals: the cwd and PATH form
/// refusals; `personCommands.allowOutsideSandbox` false refuses a `!!`
/// submit while a `!` submit runs; true again lets `!!` run.
fn refusals() -> Result<()> {
    let mut env = Env::new("pc-refuse")?;
    env.start()?;
    let (project, _) = env.project("project")?;
    let mut bad = request(project, None, "true", None);
    bad.cwd = Some("../outside".into());
    assert_eq!(refused(&env, &bad)?, pb::QueueRefusal::CwdInvalid);
    let mut bad = request(project, None, "true", None);
    bad.search_path = Some("relative:bin".into());
    assert_eq!(refused(&env, &bad)?, pb::QueueRefusal::PathInvalid);
    let bad = request(project, None, "echo $ASURA_SECRET_1", None);
    assert_eq!(
        refused(&env, &bad)?,
        pb::QueueRefusal::PlaceholderNotAvailable
    );
    let reply = env
        .fixture
        .config("personCommands.allowOutsideSandbox", "false")?;
    assert!(reply.error.is_none(), "{reply:?}");
    let environment = launch(&env, &[]);
    let off = outside(project, None, "true", &environment);
    assert_eq!(refused(&env, &off)?, pb::QueueRefusal::OutsideSandboxOff);
    let (input, _) = queued(&env, &request(project, None, "echo sandboxed", None))?;
    let settled = wait_settled(&env, input, Duration::from_secs(60))?;
    assert_eq!(settled.outcome, JobOutcome::Exited);
    assert!(
        !records(&env)?
            .iter()
            .any(|r| matches!(r, Record::PersonCommandQueued(p) if p.outside_sandbox)),
        "nothing journaled for a refused `!!`"
    );
    assert!(env.items()?.is_empty(), "nothing stored for a refused `!!`");
    let reply = env
        .fixture
        .config("personCommands.allowOutsideSandbox", "true")?;
    assert!(reply.error.is_none(), "{reply:?}");
    let (input, _) = queued(&env, &outside(project, None, "echo outside", &environment))?;
    let settled = wait_settled(&env, input, Duration::from_secs(60))?;
    assert_eq!(settled.outcome, JobOutcome::Exited);
    assert_eq!(stdout(&env, input)?, b"outside\n");
    println!(
        "PASS PC-I31 refusals: cwd, PATH and placeholder refusals; switch off refuses !!, ! runs, on again runs !!"
    );
    Ok(())
}

/// SS-I1, PC-I28 and SS-I11: a `!` command with a rewritten secret, one
/// with a whole-command secret and a `!!` command whose launch environment
/// holds a token, queued behind a blocker: one item each, and no journal,
/// audit or log byte holds a value. Released, each runs with its real
/// value (the printed digest matches), the retained output holds
/// placeholders, and every item is deleted after settlement. An exact `!!`
/// retry after settlement returns the original input, stores no item and
/// runs nothing again.
fn secrets() -> Result<()> {
    let mut env = Env::new("pc-secret")?;
    env.start_audited()?;
    let (project, directory) = env.project("project")?;
    let (blocker, conversation) = queued(&env, &request(project, None, BLOCKER, None))?;
    wait_for(Duration::from_secs(30), "blocker running", || {
        intent_committed(&env, blocker).unwrap_or(false)
    })?;
    let rewritten = token("aB3");
    let whole = token("cD5");
    let launched = token("eF7");
    let first = request(
        project,
        Some(conversation),
        &format!(
            "printf '%s\\n' \"{rewritten}\"; printf %s \"{rewritten}\" | /usr/bin/shasum -a 256"
        ),
        None,
    );
    let second = request(
        project,
        Some(conversation),
        &format!("printf %s '{whole}' | /usr/bin/shasum -a 256; echo '{whole}'"),
        None,
    );
    let environment = launch(&env, &[("FIXTURE_TOKEN", &launched)]);
    let third = outside(
        project,
        Some(conversation),
        "printf %s \"$FIXTURE_TOKEN\" | /usr/bin/shasum -a 256; printf '%s\\n' \"$FIXTURE_TOKEN\"",
        &environment,
    );
    let (one, _) = queued(&env, &first)?;
    let (two, _) = queued(&env, &second)?;
    let (three, _) = queued(&env, &third)?;
    let mut items = env.items()?;
    items.sort_unstable();
    let mut expected = vec![one, two, three];
    expected.sort_unstable();
    assert_eq!(items, expected, "one item per command with values");
    let q = kind_31(&env, one)?;
    assert!(
        q.command.contains("ASURA_SECRET_1") && !q.whole_command,
        "{q:?}"
    );
    assert_eq!(q.number_kinds.len(), 1);
    let q = kind_31(&env, two)?;
    assert!(
        q.whole_command && q.command == journal::WHOLE_COMMAND,
        "{q:?}"
    );
    assert!(q.display_command.contains("$ASURA_SECRET_1"), "{q:?}");
    let q = kind_31(&env, three)?;
    assert!(q.outside_sandbox && q.search_path.is_empty() && q.environment.is_empty());
    let needles = [rewritten.as_bytes(), whole.as_bytes(), launched.as_bytes()];
    job_fixture::sweep(env.home(), &needles)?;
    std::fs::write(directory.join("release"), b"")?;
    for (input, value) in [(one, &rewritten), (two, &whole), (three, &launched)] {
        let settled = wait_settled(&env, input, Duration::from_secs(60))?;
        assert_eq!(settled.outcome, JobOutcome::Exited, "{settled:?}");
        let output = String::from_utf8(stdout(&env, input)?)?;
        assert!(
            output.contains(&sha256_hex(value.as_bytes())),
            "the real value ran: {output}"
        );
        assert!(output.contains("$ASURA_SECRET_"), "placeholder: {output}");
        assert!(!output.contains(value.as_str()), "no value retained");
    }
    wait_for(Duration::from_secs(20), "items deleted", || {
        env.items().is_ok_and(|i| i.is_empty())
    })?;
    // SS-I11: the exact `!!` retry after settlement.
    let before = records(&env)?.len();
    let (again, _) = queued(&env, &third)?;
    assert_eq!(again, three);
    assert!(env.items()?.is_empty(), "the retry stored no item");
    std::thread::sleep(Duration::from_millis(500));
    let after = records(&env)?;
    assert!(
        !after[before..]
            .iter()
            .any(|r| matches!(r, Record::JobIntent(_) | Record::PersonCommandQueued(_))),
        "nothing runs again"
    );
    job_fixture::sweep(env.home(), &needles)?;
    let audit = audit_text(env.home())?;
    assert!(audit.contains("\"secret_item\":true"), "{audit}");
    println!(
        "PASS SS-I1 PC-I28 SS-I11: items before kind 31, real values ran, placeholders kept, items deleted, retry ran nothing"
    );
    Ok(())
}

/// SS-I2, PC-I11, PC-I32 and SDI-10: a queued `!!` command, a queued `!`
/// command with a secret and a queued `!` command without one, behind a
/// running blocker; the service is killed; after the restart each is held
/// and its item kept. Resume runs each once with its stored values, read
/// at dispatch; the items are deleted after settlement. A further queued
/// `!!` command whose item is deleted outside Asura is held at dispatch
/// with `secrets_lost`, and Drop succeeds.
fn restart() -> Result<()> {
    let mut env = Env::new("pc-restart")?;
    env.start()?;
    let (project, directory) = env.project("project")?;
    let (blocker, conversation) = queued(&env, &request(project, None, BLOCKER, None))?;
    wait_for(Duration::from_secs(30), "blocker running", || {
        intent_committed(&env, blocker).unwrap_or(false)
    })?;
    let secret = token("gH9");
    let launched = token("jK2");
    let environment = launch(&env, &[("FIXTURE_TOKEN", &launched)]);
    let (a, _) = queued(
        &env,
        &outside(
            project,
            Some(conversation),
            "printf %s \"$FIXTURE_TOKEN\" | /usr/bin/shasum -a 256",
            &environment,
        ),
    )?;
    let (b, _) = queued(
        &env,
        &request(
            project,
            Some(conversation),
            &format!("printf %s \"{secret}\" | /usr/bin/shasum -a 256"),
            None,
        ),
    )?;
    let (c, _) = queued(
        &env,
        &request(project, Some(conversation), "echo plain", None),
    )?;
    let (d, _) = queued(
        &env,
        &outside(project, Some(conversation), "echo lost", &environment),
    )?;
    let mut items = env.items()?;
    items.sort_unstable();
    let mut expected = vec![a, b, d];
    expected.sort_unstable();
    assert_eq!(items, expected);
    env.fixture.crash()?;
    std::fs::write(directory.join("release"), b"")?;
    env.start()?;
    // The blocker's job is interrupted; the queued inputs are held.
    let settled = wait_settled(&env, blocker, Duration::from_secs(30))?;
    assert_eq!(settled.outcome, JobOutcome::Interrupted);
    for input in [a, b, c, d] {
        let entry = entry(&env, project, input)?;
        assert_eq!(entry.state, Some(2), "held after restart: {entry:?}");
    }
    // The sweep keeps the items of held inputs.
    std::thread::sleep(Duration::from_millis(500));
    let mut items = env.items()?;
    items.sort_unstable();
    assert_eq!(items, expected, "held inputs keep their items");
    // One item deleted outside Asura (SS-I6 form of PC-I32).
    let installation = env.installation()?;
    Keychain::open(env.target())
        .and_then(|k| {
            k.delete(&keychain::ItemKey {
                installation,
                input: d,
            })
        })
        .map_err(|e| format!("delete: {e:?}"))?;
    for input in [a, b, c] {
        decide(&env, input, RESUME)?;
        let settled = wait_settled(&env, input, Duration::from_secs(60))?;
        assert_eq!(settled.outcome, JobOutcome::Exited, "{settled:?}");
    }
    let out = String::from_utf8(stdout(&env, a)?)?;
    assert!(out.contains(&sha256_hex(launched.as_bytes())), "{out}");
    let out = String::from_utf8(stdout(&env, b)?)?;
    assert!(out.contains(&sha256_hex(secret.as_bytes())), "{out}");
    assert_eq!(stdout(&env, c)?, b"plain\n");
    decide(&env, d, RESUME)?;
    wait_for(Duration::from_secs(30), "secrets_lost", || {
        entry(&env, project, d)
            .is_ok_and(|e| e.state == Some(2) && e.hold_reason.as_deref() == Some("secrets_lost"))
    })?;
    assert!(!intent_committed(&env, d)?, "nothing ran");
    decide(&env, d, DROP)?;
    wait_for(Duration::from_secs(20), "items deleted", || {
        env.items().is_ok_and(|i| i.is_empty())
    })?;
    job_fixture::sweep(env.home(), &[secret.as_bytes(), launched.as_bytes()])?;
    println!(
        "PASS SS-I2 PC-I11 PC-I32 SDI-10: held after restart, each resumed ran once with its stored values; a deleted item held secrets_lost and dropped"
    );
    Ok(())
}

/// SS-I3: the service is killed between the item put and the kind 31
/// append (the append is held by the test-support file
/// `person-command-hold-append`): the item is an orphan, and the restart
/// sweep deletes it.
fn orphan() -> Result<()> {
    let mut env = Env::new("pc-orphan")?;
    write_private(&env.home().join("person-command-hold-append"), "")?;
    env.start()?;
    let (project, _) = env.project("project")?;
    let environment = launch(&env, &[]);
    let submit = outside(project, None, "echo orphan", &environment);
    let runtime = env.fixture.runtime()?;
    // The reply never comes while the append is held: send it on a thread.
    let sender = std::thread::spawn(move || {
        let cancel: asura_client::Cancel = std::sync::Arc::new(|| false);
        let mut client = asura_client::Client::attach(&runtime, job_fixture::BUILD, &*cancel)
            .map_err(|e| e.to_string())?;
        client
            .queue_person_command(submit, &|| false)
            .map(|_| ())
            .map_err(|e| e.to_string())
    });
    wait_for(Duration::from_secs(30), "item stored", || {
        env.items().is_ok_and(|i| i.len() == 1)
    })?;
    assert!(
        !records(&env)?
            .iter()
            .any(|r| matches!(r, Record::PersonCommandQueued(_))),
        "no kind 31 yet"
    );
    env.fixture.crash()?;
    let _ = sender.join();
    std::fs::remove_file(env.home().join("person-command-hold-append"))?;
    env.start()?;
    wait_for(Duration::from_secs(30), "orphan swept", || {
        env.items().is_ok_and(|i| i.is_empty())
    })?;
    println!("PASS SS-I3: killed between put and append; the restart sweep deleted the orphan");
    Ok(())
}

/// SS-I4 and SS-I5: a submit with the scratch keychain locked is refused
/// with `secret_store_locked` and nothing is journaled; unlocked, the
/// submit runs. A command queued behind a blocker whose keychain is
/// locked at dispatch waits at the lane head with `keychain_locked`, and
/// runs once the keychain is unlocked (the paced recheck).
fn locked() -> Result<()> {
    let mut env = Env::new("pc-locked")?;
    env.start()?;
    let (project, directory) = env.project("project")?;
    let environment = launch(&env, &[]);
    keychain::scratch::lock(env.target()).map_err(|e| format!("lock: {e:?}"))?;
    let submit = outside(project, None, "echo unlocked", &environment);
    assert_eq!(refused(&env, &submit)?, pb::QueueRefusal::SecretStoreLocked);
    assert!(
        !records(&env)?
            .iter()
            .any(|r| matches!(r, Record::PersonCommandQueued(_))),
        "nothing journaled"
    );
    keychain::scratch::unlock(env.target()).map_err(|e| format!("unlock: {e:?}"))?;
    let (input, _) = queued(&env, &submit)?;
    assert_eq!(
        wait_settled(&env, input, Duration::from_secs(60))?.outcome,
        JobOutcome::Exited
    );
    // SS-I5: locked at dispatch.
    let (blocker, conversation) = queued(&env, &request(project, None, BLOCKER, None))?;
    wait_for(Duration::from_secs(30), "blocker running", || {
        intent_committed(&env, blocker).unwrap_or(false)
    })?;
    let (waiting, _) = queued(
        &env,
        &outside(project, Some(conversation), "echo after", &environment),
    )?;
    keychain::scratch::lock(env.target()).map_err(|e| format!("lock: {e:?}"))?;
    std::fs::write(directory.join("release"), b"")?;
    wait_for(Duration::from_secs(30), "keychain_locked", || {
        entry(&env, project, waiting).is_ok_and(|e| {
            e.waiting_reason == Some(pb::QueueWaitReason::KeychainLocked as i32)
                && e.waiting_text.as_deref() == Some("waiting: Keychain locked")
        })
    })?;
    assert!(!intent_committed(&env, waiting)?);
    keychain::scratch::unlock(env.target()).map_err(|e| format!("unlock: {e:?}"))?;
    let settled = wait_settled(&env, waiting, Duration::from_secs(90))?;
    assert_eq!(settled.outcome, JobOutcome::Exited);
    println!(
        "PASS SS-I4 SS-I5: locked submit refused with nothing journaled; locked at dispatch waited with keychain_locked and ran after unlock"
    );
    Ok(())
}

/// SS-I6 and SS-I7: an item deleted outside Asura holds the input at
/// dispatch with `secrets_lost`; an item written by another binary
/// identity (the `secret_store_fixture` binary) holds it with
/// `secret_store_denied`, with no dialog. Drop succeeds for both.
fn lost() -> Result<()> {
    let mut env = Env::new("pc-lost")?;
    env.start()?;
    let (project, directory) = env.project("project")?;
    let environment = launch(&env, &[]);
    let (blocker, conversation) = queued(&env, &request(project, None, BLOCKER, None))?;
    wait_for(Duration::from_secs(30), "blocker running", || {
        intent_committed(&env, blocker).unwrap_or(false)
    })?;
    let (missing, _) = queued(
        &env,
        &outside(project, Some(conversation), "echo missing", &environment),
    )?;
    let (denied, _) = queued(
        &env,
        &outside(project, Some(conversation), "echo denied", &environment),
    )?;
    env.fixture.crash()?;
    std::fs::write(directory.join("release"), b"")?;
    let installation = env.installation()?;
    let keychain = Keychain::open(env.target()).map_err(|e| format!("open: {e:?}"))?;
    for input in [missing, denied] {
        keychain
            .delete(&keychain::ItemKey {
                installation,
                input,
            })
            .map_err(|e| format!("delete: {e:?}"))?;
    }
    // Another binary identity writes the second input's item.
    let password = std::fs::read_to_string(env.home().join("secret-store-keychain"))?
        .lines()
        .nth(1)
        .ok_or("password")?
        .to_owned();
    let status = std::process::Command::new(env!("CARGO_BIN_EXE_secret_store_fixture"))
        .arg(&env.keychain)
        .arg(&password)
        .arg(keychain::service_name(&installation))
        .arg(format!("input.{}", job_fixture::hex(&denied)))
        .status()?;
    assert!(status.success(), "fixture item written");
    env.start()?;
    decide(&env, missing, RESUME)?;
    wait_for(Duration::from_secs(30), "secrets_lost", || {
        entry(&env, project, missing)
            .is_ok_and(|e| e.hold_reason.as_deref() == Some("secrets_lost"))
    })?;
    // A held input blocks its lane: drop it before the next one runs.
    assert!(!intent_committed(&env, missing)?);
    decide(&env, missing, DROP)?;
    decide(&env, denied, RESUME)?;
    let mut last = None;
    let waited = wait_for(Duration::from_secs(30), "secret_store_denied", || {
        last = entry(&env, project, denied).ok();
        last.as_ref()
            .is_some_and(|e| e.hold_reason.as_deref() == Some("secret_store_denied"))
    });
    if let Err(error) = waited {
        return Err(format!("{error}: {last:?}").into());
    }
    assert!(!intent_committed(&env, denied)?);
    decide(&env, denied, DROP)?;
    for input in [missing, denied] {
        assert_eq!(entry(&env, project, input)?.state, Some(6), "dropped");
    }
    println!(
        "PASS SS-I6 SS-I7: a deleted item held secrets_lost; another binary's item held secret_store_denied with no dialog; both dropped"
    );
    Ok(())
}

/// PC-I23 and PC-I30: a `!!` command writes a file outside every person
/// profile root and the same write from `!` is denied; the `!!` command
/// sees the marker variable, the fixture HOME and PATH unchanged, stdin at
/// EOF and no TTY. It also writes the scratch home's `config.yaml` and
/// connects to the scratch control socket (EXD-1); no test touches the
/// account's `~/.asura`.
fn outside_profile() -> Result<()> {
    let mut env = Env::new("pc-outside")?;
    env.start()?;
    let (project, _) = env.project("project")?;
    let target = env.home().join("outside-target");
    std::fs::create_dir(&target)?;
    let environment = launch(&env, &[("FIXTURE_MARKER", "marker-value")]);
    let command = format!(
        "touch '{0}/made-outside' && printf '%s|%s|%s\\n' \"$FIXTURE_MARKER\" \"$HOME\" \"$PATH\"; if [ -t 0 ]; then echo tty; else echo notty; fi; cat; echo stdin-eof",
        target.display()
    );
    let (input, _) = queued(&env, &outside(project, None, &command, &environment))?;
    let settled = wait_settled(&env, input, Duration::from_secs(60))?;
    assert_eq!(
        (settled.outcome, settled.exit_value),
        (JobOutcome::Exited, 0)
    );
    assert!(
        target.join("made-outside").exists(),
        "the !! write succeeded"
    );
    let output = String::from_utf8(stdout(&env, input)?)?;
    assert_eq!(
        output,
        format!(
            "marker-value|{}|{PATH}\nnotty\nstdin-eof\n",
            env.home().display()
        )
    );
    let denied = format!("touch '{}/made-inside'", target.display());
    let (sandboxed, _) = queued(&env, &request(project, None, &denied, None))?;
    let settled = wait_settled(&env, sandboxed, Duration::from_secs(60))?;
    assert_eq!(settled.outcome, JobOutcome::Exited);
    assert_ne!(settled.exit_value, 0, "the ! write is denied");
    assert!(!target.join("made-inside").exists());
    // PC-I30: the scratch home's config and control socket.
    let config = env.home().join(".asura/config.yaml");
    let socket = env.home().join(".asura/run/control.sock");
    let command = format!(
        "printf '# touched by !!\\n' >> '{}' && /usr/bin/perl -MIO::Socket::UNIX -e 'IO::Socket::UNIX->new(Peer => $ARGV[0]) or exit 3' '{}'",
        config.display(),
        socket.display()
    );
    let (exposure, _) = queued(&env, &outside(project, None, &command, &environment))?;
    let settled = wait_settled(&env, exposure, Duration::from_secs(60))?;
    assert_eq!(
        (settled.outcome, settled.exit_value),
        (JobOutcome::Exited, 0)
    );
    assert!(std::fs::read_to_string(&config)?.contains("# touched by !!"));
    println!(
        "PASS PC-I23 PC-I30: !! wrote outside the roots with the launch environment, no TTY, stdin EOF; ! was denied; !! reached the scratch config and socket"
    );
    Ok(())
}

/// PC-I7, PC-I8 and PC-I24: a command ignoring TERM; a person stop gets
/// KILL 2 s after TERM (`person_stop`); a stop then a kill sends KILL at
/// once (`person_kill`); the same for `!!`.
fn stop() -> Result<()> {
    let mut env = Env::new("pc-stop")?;
    env.start()?;
    let (project, _) = env.project("project")?;
    let environment = launch(&env, &[]);
    let ignoring = "trap '' TERM; echo ready; while :; do /bin/sleep 0.1; done";
    for outside_mode in [false, true] {
        for kill in [false, true] {
            let submit = if outside_mode {
                outside(project, None, ignoring, &environment)
            } else {
                request(project, None, ignoring, None)
            };
            let (input, _) = queued(&env, &submit)?;
            let job = job_of(input);
            wait_for(Duration::from_secs(30), "running", || {
                intent_committed(&env, input).unwrap_or(false)
            })?;
            std::thread::sleep(Duration::from_millis(500));
            let started = Instant::now();
            let mut client = env.fixture.attach()?;
            let reply = client.cancel_job(
                pb::JobCancel {
                    request_id: Some(asura_platform::random_id().to_vec()),
                    job_id: Some(job.to_vec()),
                    expected_project_id: Some(project.to_vec()),
                    kill: None,
                },
                &|| false,
            )?;
            assert_eq!(reply.requested, Some(true), "{reply:?}");
            if kill {
                let reply = client.cancel_job(
                    pb::JobCancel {
                        request_id: Some(asura_platform::random_id().to_vec()),
                        job_id: Some(job.to_vec()),
                        expected_project_id: Some(project.to_vec()),
                        kill: Some(true),
                    },
                    &|| false,
                )?;
                assert!(reply.error.is_none(), "{reply:?}");
            }
            let settled = wait_settled(&env, input, Duration::from_secs(30))?;
            let elapsed = started.elapsed();
            assert_eq!(settled.outcome, JobOutcome::Cancelled, "{settled:?}");
            if kill {
                assert_eq!(settled.reason, JobReason::PersonKill);
                assert!(
                    elapsed < Duration::from_millis(1900),
                    "KILL at once: {elapsed:?}"
                );
            } else {
                assert_eq!(settled.reason, JobReason::PersonStop);
                assert!(
                    elapsed >= Duration::from_millis(1900),
                    "KILL 2 s after TERM, not before: {elapsed:?}"
                );
            }
        }
    }
    println!(
        "PASS PC-I7 PC-I8 PC-I24: person stop killed after the 2 s grace, stop then kill at once, for ! and !!"
    );
    Ok(())
}

/// PC-I33, PC-I34, PC-I40, EC-I9 and EC-I10 through the service: a `!!`
/// survivor in a new session keeps stdout; the shell exits; the lane
/// advances while the job drains (a command queued behind it runs); all
/// five ticks are retained and the job settles `exited` 0 with capture
/// `complete` and escapees 1. A survivor writing forever is closed by a
/// person cancel: kind 27, capture `incomplete` reason 7, kind 28 `exited`
/// 0 with `person_stop`; the survivor is alive and gets EPIPE. An in-group
/// background process of `!!` is not stopped at shell exit; under `!` it
/// is. An env-scrubbed survivor (EC-I10) is not attributed.
fn survivors() -> Result<()> {
    let mut env = Env::new("pc-surv")?;
    env.start()?;
    let (project, _) = env.project("project")?;
    let environment = launch(&env, &[]);
    let exe = std::env::current_exe()?;
    let marks = env.home().join("marks");
    std::fs::create_dir(&marks)?;
    let survivor = |tag: &str, mode: &str| {
        format!(
            "'{exe}' --survivor '{marks}/{tag}' {mode} & while [ ! -e '{marks}/{tag}.pid' ]; do /bin/sleep 0.05; done; exit 0",
            exe = exe.display(),
            marks = marks.display()
        )
    };
    // PC-I33.
    let (held, conversation) = queued(
        &env,
        &outside(project, None, &survivor("hold", "hold"), &environment),
    )?;
    let (behind, _) = queued(
        &env,
        &request(project, Some(conversation), "echo behind", None),
    )?;
    let behind_done = wait_settled(&env, behind, Duration::from_secs(60))?;
    assert_eq!(behind_done.outcome, JobOutcome::Exited);
    assert!(
        settled(&env, held)?.is_none(),
        "the lane advanced while the !! job drained (OD2)"
    );
    let settled_held = wait_settled(&env, held, Duration::from_secs(60))?;
    assert_eq!(
        (
            settled_held.outcome,
            settled_held.exit_value,
            settled_held.escapees
        ),
        (JobOutcome::Exited, 0, 1)
    );
    assert_eq!(settled_held.capture_state, JobCaptureState::Complete);
    assert_eq!(
        stdout(&env, held)?,
        b"tick 0\ntick 1\ntick 2\ntick 3\ntick 4\n"
    );
    assert_eq!(
        read_mark(&marks.join("hold.done"))?,
        "ended",
        "never signalled"
    );
    // PC-I34.
    let (forever, _) = queued(
        &env,
        &outside(project, None, &survivor("forever", "forever"), &environment),
    )?;
    let job = job_of(forever);
    wait_for(Duration::from_secs(30), "released", || {
        env.fixture
            .attach()
            .and_then(|mut c| Ok(c.inspect_job(job, None, &|| false)?))
            .is_ok_and(|d| d.entry.is_some_and(|e| e.shell_exited == Some(true)))
    })?;
    let reply = env.fixture.attach()?.cancel_job(
        pb::JobCancel {
            request_id: Some(asura_platform::random_id().to_vec()),
            job_id: Some(job.to_vec()),
            expected_project_id: Some(project.to_vec()),
            kill: None,
        },
        &|| false,
    )?;
    assert_eq!(reply.requested, Some(true), "{reply:?}");
    let closed = wait_settled(&env, forever, Duration::from_secs(30))?;
    assert_eq!(
        (closed.outcome, closed.exit_value, closed.reason),
        (JobOutcome::Exited, 0, JobReason::PersonStop)
    );
    assert_eq!(closed.capture_state, JobCaptureState::Incomplete);
    assert_eq!(closed.capture_reason, CaptureReason::ClosedAfterCancel);
    assert!(
        records(&env)?
            .iter()
            .any(|r| matches!(r, Record::JobCancelRequested(c) if c.job == job)),
        "kind 27"
    );
    assert_eq!(read_mark(&marks.join("forever.epipe"))?, "EPIPE");
    // PC-I40: an in-group background process.
    let group = "/bin/sh -c 'nohup /bin/sh -c \"for i in 1 2 3 4 5; do echo n \\$i; /bin/sleep 1; done\" &'";
    let (grouped, _) = queued(&env, &outside(project, None, group, &environment))?;
    let settled_group = wait_settled(&env, grouped, Duration::from_secs(60))?;
    // The background shell and, at the scan, its current `sleep` child are
    // in the owned group: at least one survivor, never signalled (PQ3).
    assert!(settled_group.escapees >= 1, "{settled_group:?}");
    assert_eq!(settled_group.capture_state, JobCaptureState::Complete);
    let out = String::from_utf8(stdout(&env, grouped)?)?;
    assert_eq!(out, "n 1\nn 2\nn 3\nn 4\nn 5\n", "all five lines");
    let (sandboxed, _) = queued(&env, &request(project, None, group, None))?;
    let settled_sandboxed = wait_settled(&env, sandboxed, Duration::from_secs(60))?;
    assert_eq!(settled_sandboxed.escapees, 0);
    let out = String::from_utf8(stdout(&env, sandboxed)?)?;
    assert!(out.lines().count() <= 1, "at most the first line: {out}");
    // EC-I9 and EC-I10: a survivor that closed its output is counted
    // (reason 14); an env-scrubbed one is not attributed.
    let (counted, _) = queued(
        &env,
        &outside(project, None, &survivor("closed", "closed"), &environment),
    )?;
    let s = wait_settled(&env, counted, Duration::from_secs(60))?;
    assert_eq!(s.escapees, 1, "EC-I9: {s:?}");
    let scrubbed = format!(
        "/usr/bin/env -i '{exe}' --survivor '{marks}/scrub' closed & while [ ! -e '{marks}/scrub.pid' ]; do /bin/sleep 0.05; done; exit 0",
        exe = exe.display(),
        marks = marks.display()
    );
    let (unattributed, _) = queued(&env, &outside(project, None, &scrubbed, &environment))?;
    let s = wait_settled(&env, unattributed, Duration::from_secs(60))?;
    assert_eq!(s.escapees, 0, "EC-I10: {s:?}");
    println!(
        "PASS PC-I33 PC-I34 PC-I40 EC-I9 EC-I10 through the service: release, EOF settlement, close after cancel, in-group survivor, attribution"
    );
    Ok(())
}
fn read_mark(path: &Path) -> Result<String> {
    let mut text = None;
    wait_for(Duration::from_secs(30), "mark", || {
        text = std::fs::read_to_string(path).ok();
        text.is_some()
    })?;
    Ok(text.unwrap())
}

/// The survivor fixture (this test binary): `setsid`, then `hold` prints
/// five ticks a second apart and ends; `forever` writes until EPIPE;
/// `closed` closes its output and lives 3 s. It records its pid at
/// `<base>.pid` and how it ended; never longer than 60 s.
fn survivor_main(base: &str, mode: &str) {
    use std::io::Write;
    // SAFETY: the test-owned child leaves its session; nothing else changes.
    unsafe {
        libc::setsid();
    }
    let record = |suffix: &str, text: &str| {
        let tmp = format!("{base}.{suffix}.tmp");
        let _ = std::fs::write(&tmp, text);
        let _ = std::fs::rename(&tmp, format!("{base}.{suffix}"));
    };
    if mode == "closed" {
        // SAFETY: the child's own standard descriptors only.
        unsafe {
            let null = libc::open(c"/dev/null".as_ptr(), libc::O_RDWR);
            for fd in 0..=2 {
                libc::dup2(null, fd);
            }
            libc::close(null);
        }
    }
    let tmp = format!("{base}.pid.tmp");
    let _ = std::fs::write(&tmp, std::process::id().to_string());
    let _ = std::fs::rename(&tmp, format!("{base}.pid"));
    if mode == "closed" {
        std::thread::sleep(Duration::from_secs(3));
        record("done", "ended");
        return;
    }
    let end = Instant::now() + Duration::from_secs(60);
    let limit = if mode == "hold" { 5 } else { u32::MAX };
    let mut out = std::io::stdout();
    for i in 0..limit {
        if let Err(error) = out
            .write_all(format!("tick {i}\n").as_bytes())
            .and_then(|()| out.flush())
        {
            record(
                "epipe",
                if error.raw_os_error() == Some(libc::EPIPE) {
                    "EPIPE"
                } else {
                    "error"
                },
            );
            return;
        }
        if Instant::now() >= end {
            break;
        }
        std::thread::sleep(if mode == "hold" {
            Duration::from_secs(1)
        } else {
            Duration::from_millis(50)
        });
    }
    record("done", "ended");
}

// ------------------------------------------------------------ scripted model

/// One scripted model reply.
#[derive(Clone)]
enum Step {
    Call(&'static str, serde_json::Value),
    Final,
}
/// A loopback Ollama endpoint that keeps every chat request body.
struct Model {
    endpoint: String,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    chats: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    script: std::sync::Arc<std::sync::Mutex<std::collections::VecDeque<Step>>>,
    thread: Option<std::thread::JoinHandle<()>>,
}
impl Model {
    fn start() -> Result<Self> {
        use std::sync::atomic::Ordering;
        let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        listener.set_nonblocking(true)?;
        let endpoint = format!("http://{}", listener.local_addr()?);
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let chats = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let script = std::sync::Arc::new(std::sync::Mutex::new(std::collections::VecDeque::new()));
        let (token, log, steps) = (stop.clone(), chats.clone(), script.clone());
        let thread = std::thread::spawn(move || {
            while !token.load(Ordering::Acquire) {
                let (mut socket, _) = match listener.accept() {
                    Ok(value) => value,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                    Err(_) => return,
                };
                let _ = socket.set_nonblocking(false);
                let _ = socket.set_read_timeout(Some(Duration::from_secs(5)));
                let (log, steps) = (log.clone(), steps.clone());
                std::thread::spawn(move || {
                    let _ = serve(&mut socket, &log, &steps);
                });
            }
        });
        Ok(Self {
            endpoint,
            stop,
            chats,
            script,
            thread: Some(thread),
        })
    }
    fn script(&self, steps: &[Step]) {
        let mut script = self.script.lock().unwrap();
        script.clear();
        script.extend(steps.iter().cloned());
    }
    fn chats(&self) -> Vec<String> {
        self.chats.lock().unwrap().clone()
    }
}
impl Drop for Model {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
fn serve(
    socket: &mut std::net::TcpStream,
    log: &std::sync::Mutex<Vec<String>>,
    steps: &std::sync::Mutex<std::collections::VecDeque<Step>>,
) -> std::io::Result<()> {
    use std::io::{Read, Write};
    let mut request = Vec::new();
    let (header_end, length) = loop {
        let mut bytes = [0; 65536];
        let count = socket.read(&mut bytes)?;
        if count == 0 {
            return Ok(());
        }
        request.extend_from_slice(&bytes[..count]);
        if let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
            let header = String::from_utf8_lossy(&request[..end]);
            let length = header
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .and_then(|v| v.trim().parse::<usize>().ok())
                })
                .unwrap_or(0);
            if request.len() >= end + 4 + length {
                break (end, length);
            }
        }
    };
    let body = &request[header_end + 4..header_end + 4 + length];
    if request.starts_with(b"POST /api/show ") {
        let payload = serde_json::json!({"capabilities": ["completion", "tools"],
            "model_info": {"fixture.context_length": 32768}})
        .to_string();
        return write!(
            socket,
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            payload.len(),
            payload
        );
    }
    if !request.starts_with(b"POST /api/chat ") {
        return Ok(());
    }
    log.lock()
        .unwrap()
        .push(String::from_utf8_lossy(body).into_owned());
    let step = steps.lock().unwrap().pop_front().unwrap_or(Step::Final);
    socket.write_all(
        b"HTTP/1.1 200 OK\r\nContent-Type: application/x-ndjson\r\nConnection: close\r\n\r\n",
    )?;
    let response = match step {
        Step::Call(name, arguments) => serde_json::json!({
            "message": {"role": "assistant", "content": "", "tool_calls": [
                {"function": {"name": name, "arguments": arguments}}]},
            "done": true, "done_reason": "stop", "prompt_eval_count": 32, "eval_count": 2}),
        Step::Final => serde_json::json!({
            "message": {"role": "assistant", "content": "COMPLETE"},
            "done": true, "done_reason": "stop", "prompt_eval_count": 48, "eval_count": 3}),
    };
    socket.write_all(format!("{response}\n").as_bytes())
}
fn call(name: &'static str, arguments: serde_json::Value) -> Step {
    Step::Call(name, arguments)
}
/// The conversation and generation of a model operation.
fn turn_of(env: &Env, operation: [u8; 16]) -> Result<([u8; 16], u64)> {
    let (_, replay) = env.fixture.journal()?;
    let op = replay.operations.get(&operation).ok_or("operation")?;
    Ok((op.conversation, op.generation))
}
fn at_generation(
    mut request: pb::ConversationQueueCommandSubmit,
    generation: u64,
) -> pb::ConversationQueueCommandSubmit {
    request.expected_generation = Some(generation);
    request
}
fn position(records: &[Record], f: impl Fn(&Record) -> bool) -> Option<usize> {
    records.iter().position(f)
}

/// PC-I12, PC-I27, PC-I13, PC-I21 and PC-I39 with a scripted model: a `!`
/// and a `!!` command submitted while a model turn runs wait with
/// `waiting: model turn running` and run after the turn's terminal record;
/// the next turn's input holds both person entries (the `!!` one with the
/// outside label). A released `!!` job is listed, inspected and read by the
/// model, whose cancel is `not_permitted` while the job keeps running.
/// Turning execution off for the project stops a model job with
/// `execution_off` while a `!` job of the project runs on and settles.
fn model() -> Result<()> {
    let model = Model::start()?;
    let mut env = Env::new("pc-model")?;
    env.start()?;
    let (project, _) = env.project("project")?;
    env.fixture.model(&model.endpoint)?;
    model.script(&[
        call(
            "exec_cmd",
            serde_json::json!({"command": "sleep 4", "yield_seconds": 4}),
        ),
        Step::Final,
    ]);
    let operation = env.fixture.submit(project, "Wait for it.")?;
    let (conversation, generation) = turn_of(&env, operation)?;
    let environment = launch(&env, &[]);
    let (sandboxed, _) = queued(
        &env,
        &at_generation(
            request(project, Some(conversation), "echo sandboxed", None),
            generation,
        ),
    )?;
    let (outside_input, _) = queued(
        &env,
        &at_generation(
            outside(project, Some(conversation), "echo outside", &environment),
            generation,
        ),
    )?;
    let waiting = entry(&env, project, sandboxed)?;
    assert_eq!(
        waiting.waiting_reason,
        Some(pb::QueueWaitReason::ModelTurn as i32),
        "PC-I12: {waiting:?}"
    );
    let event = env.fixture.finish(operation, Duration::from_secs(60))?;
    assert_eq!(event.kind, Some(3), "{event:?}");
    for input in [sandboxed, outside_input] {
        assert_eq!(
            wait_settled(&env, input, Duration::from_secs(60))?.outcome,
            JobOutcome::Exited
        );
    }
    let all = records(&env)?;
    let terminal = position(
        &all,
        |r| matches!(r, Record::TurnTerminal(t) if t.operation == operation),
    )
    .ok_or("kind 8")?;
    for input in [sandboxed, outside_input] {
        let intent = position(
            &all,
            |r| matches!(r, Record::JobIntent(i) if i.origin == JobOrigin::Person { input }),
        )
        .ok_or("kind 26")?;
        assert!(
            intent > terminal,
            "PC-I12, PC-I27: runs after the terminal commit"
        );
    }
    // PC-I13: the next turn's input holds both person entries.
    model.script(&[Step::Final]);
    let next = env.fixture.follow(project, operation, "What did I run?")?;
    env.fixture.finish(next, Duration::from_secs(60))?;
    let chat = model.chats().last().cloned().ok_or("chat")?;
    assert!(
        chat.contains("The person ran a command themselves"),
        "{chat}"
    );
    assert!(
        chat.contains("The person ran a command, outside the sandbox, themselves"),
        "{chat}"
    );
    assert!(chat.contains("command: echo sandboxed"), "{chat}");
    // PC-I21: a released `!!` job; the model lists, inspects and reads it;
    // its cancel is not permitted and the job keeps running.
    let exe = std::env::current_exe()?;
    let marks = env.home().join("marks");
    std::fs::create_dir(&marks)?;
    let (_, generation) = turn_of(&env, next)?;
    let forever = format!(
        "'{exe}' --survivor '{marks}/f' forever & while [ ! -e '{marks}/f.pid' ]; do /bin/sleep 0.05; done; exit 0",
        exe = exe.display(),
        marks = marks.display()
    );
    let (released, _) = queued(
        &env,
        &at_generation(
            outside(project, Some(conversation), &forever, &environment),
            generation,
        ),
    )?;
    let job = job_of(released);
    wait_for(Duration::from_secs(30), "released", || {
        entry(&env, project, released).is_ok_and(|e| e.state == Some(4))
    })?;
    let handle = job_fixture::hex(&job);
    model.script(&[
        call("job", serde_json::json!({"command": "list"})),
        call(
            "job",
            serde_json::json!({"command": "status", "job": handle}),
        ),
        call(
            "job",
            serde_json::json!({"command": "read", "job": handle, "stream": "stdout"}),
        ),
        call(
            "job",
            serde_json::json!({"command": "wait", "job": handle, "seconds": 1}),
        ),
        call(
            "job",
            serde_json::json!({"command": "cancel", "job": handle}),
        ),
        Step::Final,
    ]);
    let third = env.fixture.follow(project, next, "Look at my job.")?;
    env.fixture.finish(third, Duration::from_secs(60))?;
    let chat = model.chats().last().cloned().ok_or("chat")?;
    assert!(
        chat.contains(&handle),
        "list and status name the job: {chat}"
    );
    assert!(chat.contains("tick"), "read returns the output: {chat}");
    assert!(chat.contains("not_permitted"), "{chat}");
    assert!(settled(&env, released)?.is_none(), "the job keeps running");
    let reply = env.fixture.attach()?.cancel_job(
        pb::JobCancel {
            request_id: Some(asura_platform::random_id().to_vec()),
            job_id: Some(job.to_vec()),
            expected_project_id: Some(project.to_vec()),
            kill: None,
        },
        &|| false,
    )?;
    assert_eq!(reply.requested, Some(true));
    wait_settled(&env, released, Duration::from_secs(30))?;
    // PC-I39: execution off stops the model job; the `!` job runs on.
    model.script(&[
        call(
            "exec_cmd",
            serde_json::json!({"command": "sleep 60", "yield_seconds": 1}),
        ),
        Step::Final,
    ]);
    let fourth = env.fixture.follow(project, third, "Start a long job.")?;
    env.fixture.finish(fourth, Duration::from_secs(60))?;
    let model_job = records(&env)?
        .into_iter()
        .find_map(|r| match r {
            Record::JobIntent(i) => match i.origin {
                JobOrigin::Model { operation, .. } if operation == fourth => {
                    Some(journal::job_id(&i.origin))
                }
                _ => None,
            },
            _ => None,
        })
        .ok_or("model job")?;
    let (person, _) = queued(
        &env,
        &request(project, None, "/bin/sleep 3; echo person done", None),
    )?;
    wait_for(Duration::from_secs(30), "person running", || {
        intent_committed(&env, person).unwrap_or(false)
    })?;
    let key = format!("execution.projects.{}", job_fixture::hex(&project));
    let reply = env.fixture.config(&key, "off")?;
    assert!(reply.error.is_none(), "{reply:?}");
    let stopped = records(&env)?;
    let _ = stopped;
    wait_for(Duration::from_secs(30), "model job stopped", || {
        records(&env).is_ok_and(|r| {
            r.iter().any(|r| {
                matches!(r, Record::JobSettled(s) if s.job == model_job && s.reason == JobReason::ExecutionOff)
            })
        })
    })?;
    let person_done = wait_settled(&env, person, Duration::from_secs(30))?;
    assert_eq!(
        (
            person_done.outcome,
            person_done.exit_value,
            person_done.reason
        ),
        (JobOutcome::Exited, 0, JobReason::None)
    );
    assert_eq!(stdout(&env, person)?, b"person done\n");
    println!(
        "PASS PC-I12 PC-I27 PC-I13 PC-I21 PC-I39: waited for the turn, notice entries, model job commands on a person job, execution off spared the person job"
    );
    Ok(())
}

/// PC-I10, PC-I26 and PC-I35: the service is killed while a `!` job, a
/// `!!` job and a released `!!` job run. The guardians stop the owned
/// groups on lease EOF; the next start records `interrupted` for each and
/// relaunches nothing; the released job's survivor is not signalled (its
/// next write fails with EPIPE).
fn interrupt() -> Result<()> {
    let mut env = Env::new("pc-intr")?;
    env.start()?;
    let (project, directory) = env.project("project")?;
    let environment = launch(&env, &[]);
    let marks = env.home().join("marks");
    std::fs::create_dir(&marks)?;
    let sleeper = |name: &str| {
        format!(
            "/bin/sleep 600 & echo $! > '{}/{name}.pid'; wait",
            marks.display()
        )
    };
    let (sandboxed, _) = queued(&env, &request(project, None, &sleeper("a"), None))?;
    let (unsandboxed, _) = queued(&env, &outside(project, None, &sleeper("b"), &environment))?;
    let exe = std::env::current_exe()?;
    let forever = format!(
        "'{exe}' --survivor '{marks}/f' forever & while [ ! -e '{marks}/f.pid' ]; do /bin/sleep 0.05; done; exit 0",
        exe = exe.display(),
        marks = marks.display()
    );
    let (released, _) = queued(&env, &outside(project, None, &forever, &environment))?;
    wait_for(Duration::from_secs(30), "running", || {
        [sandboxed, unsandboxed, released]
            .iter()
            .all(|i| intent_committed(&env, *i).unwrap_or(false))
            && marks.join("a.pid").exists()
            && marks.join("b.pid").exists()
    })?;
    wait_for(Duration::from_secs(30), "released", || {
        entry(&env, project, released).is_ok_and(|e| e.state == Some(4))
    })?;
    let pids: Vec<i32> = ["a", "b"]
        .iter()
        .map(|n| {
            std::fs::read_to_string(marks.join(format!("{n}.pid")))
                .unwrap()
                .trim()
                .parse()
                .unwrap()
        })
        .collect();
    env.fixture.crash()?;
    for pid in &pids {
        wait_for(
            Duration::from_secs(10),
            "group stopped on lease EOF",
            || {
                // SAFETY: signal 0 checks for the process only.
                unsafe { libc::kill(*pid, 0) != 0 }
            },
        )?;
    }
    assert_eq!(read_mark(&marks.join("f.epipe"))?, "EPIPE", "not signalled");
    let before = records(&env)?
        .iter()
        .filter(|r| matches!(r, Record::JobIntent(_)))
        .count();
    env.start()?;
    for input in [sandboxed, unsandboxed, released] {
        let s = wait_settled(&env, input, Duration::from_secs(30))?;
        assert_eq!(s.outcome, JobOutcome::Interrupted, "{s:?}");
    }
    std::thread::sleep(Duration::from_millis(500));
    let after = records(&env)?
        .iter()
        .filter(|r| matches!(r, Record::JobIntent(_)))
        .count();
    assert_eq!(before, after, "no relaunch");
    let _ = directory;
    println!(
        "PASS PC-I10 PC-I26 PC-I35: lease EOF stopped the groups, restart recorded interrupted, nothing relaunched, the survivor was not signalled"
    );
    Ok(())
}

/// PC-I2 to PC-I5, PC-I14, PC-I15, PC-I29 and PC-I36 through the service,
/// with a fixture account home and `$TMPDIR`: writes to the project,
/// `$TMPDIR` and `~/.cargo` succeed; writes to `~/.zshrc`, `~/.ssh/x` and
/// `~/other` are denied; reads of `~/.ssh/known_hosts` and `~/.gitconfig`
/// succeed; a loopback HTTP fixture answers; a symlinked cwd is not
/// started with `cwd_unavailable`; with `sandbox-exec` made unavailable a
/// `!` command is not started with `sandbox_unavailable` and a `!!`
/// command runs; a new-session escapee of a `!` command is frozen and
/// killed with cleanup confirmed.
fn profile() -> Result<()> {
    let mut env = Env::new("pc-prof")?;
    env.start()?;
    let (project, directory) = env.project("project")?;
    let account = env.account.clone();
    for dir in [".cargo", ".ssh"] {
        std::fs::create_dir(account.join(dir))?;
    }
    std::fs::write(account.join(".ssh/known_hosts"), b"fixture-host\n")?;
    std::fs::write(account.join(".gitconfig"), b"[user]\n")?;
    let run = |command: &str| -> Result<(JobSettled, [u8; 16])> {
        let (input, _) = queued(&env, &request(project, None, command, None))?;
        Ok((wait_settled(&env, input, Duration::from_secs(60))?, input))
    };
    // PC-I2.
    let (s, _) =
        run("touch ./w-project && touch \"$TMPDIR/w-tmp\" && touch \"$HOME/.cargo/w-cache\"")?;
    assert_eq!((s.outcome, s.exit_value), (JobOutcome::Exited, 0), "{s:?}");
    assert!(directory.join("w-project").exists());
    assert!(env.tmpdir.join("w-tmp").exists());
    assert!(account.join(".cargo/w-cache").exists());
    // PC-I3.
    for target in ["$HOME/.zshrc", "$HOME/.ssh/x", "$HOME/other"] {
        let (s, _) = run(&format!("touch \"{target}\""))?;
        assert_ne!(s.exit_value, 0, "{target} is denied");
    }
    assert!(!account.join(".zshrc").exists() && !account.join(".ssh/x").exists());
    // PC-I4.
    let (s, input) = run("cat \"$HOME/.ssh/known_hosts\" \"$HOME/.gitconfig\"")?;
    assert_eq!(s.exit_value, 0);
    assert_eq!(stdout(&env, input)?, b"fixture-host\n[user]\n");
    // PC-I5: a loopback HTTP fixture.
    let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
    let port = listener.local_addr()?.port();
    let server = std::thread::spawn(move || {
        use std::io::{Read, Write};
        if let Ok((mut socket, _)) = listener.accept() {
            let mut bytes = [0; 1024];
            let _ = socket.read(&mut bytes);
            let _ = socket.write_all(
                b"HTTP/1.1 200 OK\r\nContent-Length: 8\r\nConnection: close\r\n\r\nloopback",
            );
        }
    });
    let (s, input) = run(&format!("/usr/bin/curl -s http://localhost:{port}/"))?;
    let _ = server.join();
    assert_eq!(s.exit_value, 0, "{s:?}");
    assert_eq!(stdout(&env, input)?, b"loopback");
    // PC-I14: a symlinked launch subdirectory.
    std::fs::create_dir(directory.join("real"))?;
    std::os::unix::fs::symlink(directory.join("real"), directory.join("link"))?;
    let mut linked = request(project, None, "touch ran", None);
    linked.cwd = Some("link".into());
    let (input, _) = queued(&env, &linked)?;
    let s = wait_settled(&env, input, Duration::from_secs(60))?;
    assert_eq!(
        (s.outcome, s.reason),
        (JobOutcome::NotStarted, JobReason::CwdUnavailable)
    );
    assert!(!directory.join("real/ran").exists(), "nothing runs");
    // PC-I36: a new-session escapee writing to `$TMPDIR` is attributed,
    // frozen and killed; cleanup confirmed.
    let fixture =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../asura-platform/tests/fixtures/escapee.pl");
    let (s, _) = run(&format!(
        "/usr/bin/perl '{}' single \"$TMPDIR/escapee\"; /bin/sleep 0.5",
        fixture.display()
    ))?;
    assert!(s.escapees >= 1, "PC-I36: {s:?}");
    assert_eq!(s.cleanup, journal::JobCleanup::Confirmed);
    // PC-I15 and PC-I29: `sandbox-exec` unavailable (test seam).
    write_private(
        &env.home().join("exec-cmd-guardian-fault"),
        "no-sandbox-exec\n",
    )?;
    let (s, _) = run("echo sandboxed")?;
    assert_eq!(
        (s.outcome, s.reason),
        (JobOutcome::NotStarted, JobReason::SandboxUnavailable),
        "PC-I15"
    );
    let environment = launch(&env, &[]);
    let (input, _) = queued(&env, &outside(project, None, "echo outside", &environment))?;
    let s = wait_settled(&env, input, Duration::from_secs(60))?;
    assert_eq!((s.outcome, s.exit_value), (JobOutcome::Exited, 0), "PC-I29");
    std::fs::remove_file(env.home().join("exec-cmd-guardian-fault"))?;
    println!("PASS PC-I2 PC-I3 PC-I4 PC-I5 PC-I14 PC-I15 PC-I29 PC-I36 through the service");
    Ok(())
}

/// SS-I10 and `ServiceObservation` field 8: with 256 items under this
/// installation's service (unrecognized accounts, which are counted and
/// never deleted), the observer reports the counts and a submit with a
/// value is refused with `secret_store_full`.
fn full() -> Result<()> {
    let mut env = Env::new("pc-full")?;
    env.start()?;
    let (project, _) = env.project("project")?;
    let installation = env.installation()?;
    env.fixture.stop()?;
    for n in 0..256 {
        let added = keychain::scratch::add_account(
            env.target(),
            &installation,
            &format!("other.{n}"),
            keychain::scratch::FIXTURE_DATA,
        )
        .map_err(|e| format!("add: {e:?}"))?;
        assert_eq!(added, keychain::Added::Stored);
    }
    env.start()?;
    let mut status = None;
    wait_for(Duration::from_secs(30), "observer field 8", || {
        status = env
            .fixture
            .attach()
            .and_then(|mut c| Ok(c.observe_service(None)?))
            .ok()
            .and_then(|u| u.secret_store);
        status.as_ref().is_some_and(|s| s.item_count == Some(256))
    })?;
    let status = status.unwrap();
    assert_eq!(status.unrecognized_count, Some(256));
    assert_eq!(status.stranded_count, Some(0));
    assert_eq!(status.sweep_pending, Some(false));
    let environment = launch(&env, &[]);
    let submit = outside(project, None, "echo full", &environment);
    assert_eq!(refused(&env, &submit)?, pb::QueueRefusal::SecretStoreFull);
    // A command without values never touches the Keychain and runs.
    let (input, _) = queued(&env, &request(project, None, "echo free", None))?;
    assert_eq!(
        wait_settled(&env, input, Duration::from_secs(60))?.outcome,
        JobOutcome::Exited
    );
    println!(
        "PASS SS-I10 field 8: 256 items counted, a submit with values refused secret_store_full, a secret-free command ran"
    );
    Ok(())
}

/// PC-I9 and PC-I25: a `!` and a `!!` command each run 90 s with no
/// deadline stop and finish normally.
fn long() -> Result<()> {
    let mut env = Env::new("pc-long")?;
    env.start()?;
    let (project, _) = env.project("project")?;
    let environment = launch(&env, &[]);
    let (a, _) = queued(
        &env,
        &request(project, None, "/bin/sleep 90; echo done", None),
    )?;
    let (b, _) = queued(
        &env,
        &outside(project, None, "/bin/sleep 90; echo done", &environment),
    )?;
    for input in [a, b] {
        let s = wait_settled(&env, input, Duration::from_secs(150))?;
        assert_eq!((s.outcome, s.exit_value), (JobOutcome::Exited, 0), "{s:?}");
        assert!(s.elapsed_ms >= 90_000, "{s:?}");
        assert_eq!(stdout(&env, input)?, b"done\n");
    }
    println!("PASS PC-I9 PC-I25: 90 s commands ran with no deadline and finished");
    Ok(())
}

/// PC-I16 and PC-I18: with a person job's setup held (test seam) and the
/// journal's appends held, status, the queue list and a cancel stay
/// responsive; a submitting client that disconnects leaves its running and
/// queued commands running; a second client lists and cancels them.
fn stall() -> Result<()> {
    let mut env = Env::new("pc-stall")?;
    env.start()?;
    let (project, _) = env.project("project")?;
    let hold = env.home().join("exec-cmd-hold-setup");
    std::fs::write(&hold, b"")?;
    let (held, conversation) = queued(&env, &request(project, None, "echo held", None))?;
    wait_for(Duration::from_secs(30), "kind 26", || {
        intent_committed(&env, held).unwrap_or(false)
    })?;
    let (queued_behind, _) = queued(
        &env,
        &request(project, Some(conversation), "echo behind", None),
    )?;
    for _ in 0..5 {
        job_fixture::within_100ms("inspect", || Ok(env.fixture.attach()?.inspect()?))?;
        job_fixture::within_100ms("queue list", || {
            Ok(env.fixture.attach()?.conversation_queue(project, None)?)
        })?;
    }
    // The journal stalls: appends are held; status stays responsive.
    std::fs::write(env.home().join("journal-hold-appends"), b"")?;
    for _ in 0..5 {
        job_fixture::within_100ms("inspect while the journal stalls", || {
            Ok(env.fixture.attach()?.inspect()?)
        })?;
    }
    std::fs::remove_file(env.home().join("journal-hold-appends"))?;
    // PC-I18: a second client cancels the held job; the queued one runs.
    let reply = env.fixture.attach()?.cancel_job(
        pb::JobCancel {
            request_id: Some(asura_platform::random_id().to_vec()),
            job_id: Some(job_of(held).to_vec()),
            expected_project_id: Some(project.to_vec()),
            kill: None,
        },
        &|| false,
    )?;
    assert_eq!(reply.requested, Some(true), "{reply:?}");
    std::fs::remove_file(&hold)?;
    let s = wait_settled(&env, held, Duration::from_secs(30))?;
    assert_eq!(s.outcome, JobOutcome::NotStarted, "nothing ran: {s:?}");
    let s = wait_settled(&env, queued_behind, Duration::from_secs(30))?;
    assert_eq!(s.outcome, JobOutcome::Exited);
    let listed = env.fixture.attach()?.jobs(
        pb::JobList {
            scope: Some(1),
            project_id: Some(project.to_vec()),
            ..Default::default()
        },
        &|| false,
    )?;
    assert!(
        listed
            .entries
            .iter()
            .any(|e| e.job_id.as_deref() == Some(&job_of(queued_behind)[..])),
        "{listed:?}"
    );
    println!(
        "PASS PC-I16 PC-I18: stalled setup and journal left status and lists responsive; another client cancelled and listed"
    );
    Ok(())
}

/// PC-I19: a command writes 100 MiB across both streams within the quota:
/// both streams are retained whole, byte and line counts are exact, and no
/// journal record holds output bytes.
fn volume() -> Result<()> {
    let mut env = Env::new("pc-volume")?;
    env.start()?;
    let (project, _) = env.project("project")?;
    let command = "/usr/bin/yes stdout-line | /usr/bin/head -c 52428800; /usr/bin/yes stderr-line | /usr/bin/head -c 52428800 >&2";
    let (input, _) = queued(&env, &request(project, None, command, None))?;
    let s = wait_settled(&env, input, Duration::from_secs(300))?;
    assert_eq!((s.outcome, s.exit_value), (JobOutcome::Exited, 0), "{s:?}");
    assert_eq!(s.stdout.capture.stored_bytes, 52_428_800);
    assert_eq!(s.stderr.capture.stored_bytes, 52_428_800);
    assert_eq!(s.stdout.lines, 52_428_800 / 12);
    assert_eq!(s.stderr.lines, 52_428_800 / 12);
    let (bytes, _) = env.fixture.journal()?;
    assert!(!holds(&bytes, b"stdout-line"), "no output in the journal");
    println!("PASS PC-I19: 100 MiB retained whole with exact counts; no output in the journal");
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
    if let [_, flag, base, mode] = args.as_slice()
        && flag == "--survivor"
    {
        survivor_main(base, mode);
        return;
    }
    if args.get(1).map(String::as_str) == Some(CHILD) {
        if let Err(error) = child(Path::new(&args[2])) {
            eprintln!("person command child failed: {error}");
            std::process::exit(1);
        }
        return;
    }
    let selected: Vec<&str> = args[1..]
        .iter()
        .map(String::as_str)
        .filter(|a| !a.starts_with('-'))
        .collect();
    let cases: Vec<Case> = vec![
        ("basic", basic),
        ("refusals", refusals),
        ("secrets", secrets),
        ("restart", restart),
        ("orphan", orphan),
        ("locked", locked),
        ("lost", lost),
        ("outside", outside_profile),
        ("stop", stop),
        ("survivors", survivors),
        ("model", model),
        ("interrupt", interrupt),
        ("profile", profile),
        ("full", full),
        ("long", long),
        ("stall", stall),
        ("volume", volume),
    ];
    for (name, case) in cases {
        if !selected.is_empty() && !selected.contains(&name) {
            continue;
        }
        if let Err(error) = case() {
            eprintln!("person command flow case {name} FAILED: {error}");
            std::process::exit(1);
        }
    }
    println!("person command flow qualification passed");
}

/// The child service: the shared fixture child with this binary as the
/// guardian.
fn child(home: &Path) -> Result<()> {
    job_fixture::child(home)
}
