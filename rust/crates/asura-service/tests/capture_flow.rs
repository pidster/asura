//! exec_cmd service-level qualification (the J1T cases): the real service,
//! the real Swift helper against a scripted loopback model endpoint, the
//! real conversation owner, guardian, sandbox, capture workers, journal and
//! payload store, all in private scratch homes. Every case ends with the
//! raw-byte sweep.
//!
//! J3 retired the J1 call path (J3-E9): kind 23, the kind 15 tails and the
//! J1 busy codes. Every model exec_cmd call runs as a job (J34-E port, J1T
//! cases after J3-D): the settled facts are read from the job's kind 28 and
//! the result from the call's kind 29, which holds no output text. A job
//! that outlives its yield window settles after the turn ends.
#[path = "support/queue_submit.rs"]
mod queue_submit;
use asura_client::Client;
use asura_control::pb;
use asura_platform::RuntimeDirectory;
use asura_storage::authority::conversation::{self as journal, CaptureReason, JobCaptureState};
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
const BUILD: &str = "asura/conversation-test";

// ---------------------------------------------------------------- fixture

struct Fixture {
    home: PathBuf,
    child: Option<Child>,
    keep: bool,
}
impl Fixture {
    fn new(label: &str) -> Result<Self> {
        Self::at(Path::new("/private/tmp"), label)
    }
    fn at(parent: &Path, label: &str) -> Result<Self> {
        let nonce: String = asura_platform::random_id()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let home = parent.join(format!("asura-capture-{label}-{}", &nonce[..12]));
        std::fs::create_dir(&home)?;
        std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o700))?;
        let home = std::fs::canonicalize(home)?;
        Ok(Self {
            home,
            child: None,
            keep: false,
        })
    }
    fn runtime(&self) -> Result<RuntimeDirectory> {
        Ok(RuntimeDirectory::scratch(&self.home, true)?)
    }
    fn start(&mut self) -> Result<()> {
        assert!(self.child.is_none());
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.home.join("service.log"))?;
        self.child = Some(
            Command::new(std::env::current_exe()?)
                .arg("--capture-child")
                .arg(&self.home)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(log)
                .spawn()?,
        );
        self.attach()?;
        Ok(())
    }
    fn attach(&self) -> Result<Client> {
        let runtime = self.runtime()?;
        let end = Instant::now() + Duration::from_secs(10);
        loop {
            // Test-only harness budget: client waits have no time bound in
            // production (owner, 2026-10-16); a stalled fixture fails here.
            let harness = Instant::now() + Duration::from_secs(180);
            let cancel: asura_client::Cancel =
                std::sync::Arc::new(move || Instant::now() >= harness);
            match Client::attach(&runtime, BUILD, &*cancel) {
                Ok(mut client) => {
                    client.set_cancel(cancel);
                    return Ok(client);
                }
                Err(error) if Instant::now() >= end => return Err(error.into()),
                Err(_) => thread::sleep(Duration::from_millis(10)),
            }
        }
    }
    fn stop(&mut self) -> Result<()> {
        if self.child.is_none() {
            return Ok(());
        }
        let _ = self
            .attach()
            .and_then(|client| client.stop().map_err(Into::into));
        let end = Instant::now() + Duration::from_secs(8);
        while Instant::now() < end {
            if let Some(status) = self.child.as_mut().unwrap().try_wait()? {
                self.child.take();
                if !status.success() {
                    return Err(format!("service exit {status}").into());
                }
                return Ok(());
            }
            thread::sleep(Duration::from_millis(10));
        }
        let child = self.child.as_mut().unwrap();
        child.kill()?;
        child.wait()?;
        self.child.take();
        Err("service failed clean shutdown; killed and reaped".into())
    }
    /// A crash: SIGKILL, no drain.
    fn crash(&mut self) -> Result<()> {
        let mut child = self.child.take().ok_or("no service")?;
        child.kill()?;
        child.wait()?;
        Ok(())
    }
    fn journal(&self) -> Result<(Vec<u8>, journal::Replay)> {
        let bytes = asura_storage::authority::fixture::journal_bytes(&self.home)?;
        let replay = journal::replay(&bytes, &AtomicBool::new(false))
            .map_err(|e| format!("replay: {e:?}"))?;
        Ok((bytes, replay))
    }
    fn wait_ready(&self) -> Result<()> {
        let end = Instant::now() + Duration::from_secs(15);
        loop {
            let status = self.attach()?.inspect_installation()?;
            if status.installation == pb::InstallationState::GraphReady {
                return Ok(());
            }
            if Instant::now() >= end {
                return Err(format!("not ready: {:?}", status.installation).into());
            }
            thread::sleep(Duration::from_millis(20));
        }
    }
    fn project(&self, name: &str) -> Result<([u8; 16], PathBuf)> {
        let directory = self.home.join(name);
        std::fs::create_dir(&directory)?;
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))?;
        let reply = self.attach()?.register_project(
            pb::ProjectRegister {
                request_id: Some(asura_platform::random_id().to_vec()),
                location: Some(directory.to_str().ok_or("path")?.into()),
            },
            &|| false,
        )?;
        Ok((id(reply.project_id)?, directory))
    }
    fn model(&self, endpoint: &str) -> Result<()> {
        for (key, value) in [
            ("providers.ollama.endpoint", endpoint),
            ("model", "ollama:fixture"),
        ] {
            let reply = self.attach()?.config(key, Some(value), &|| false)?;
            assert!(reply.error.is_none(), "{key}: {:?}", reply.error);
        }
        Ok(())
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.stop();
        if let Some(child) = self.child.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
        if self.keep || thread::panicking() || std::env::var_os("ASURA_CAPTURE_KEEP").is_some() {
            eprintln!("capture fixture retained: {}", self.home.display());
        } else {
            let _ = std::fs::remove_dir_all(&self.home);
        }
    }
}
fn id(value: Option<Vec<u8>>) -> Result<[u8; 16]> {
    Ok(value
        .ok_or("missing ID")?
        .try_into()
        .map_err(|_| "wrong ID length")?)
}

// ------------------------------------------------------- scripted model

/// One scripted model reply: an exec_cmd call, or the final answer.
#[derive(Clone)]
enum Step {
    ExecCmd(String),
    Final,
}
struct Model {
    endpoint: String,
    stop: Arc<AtomicBool>,
    requests: Arc<Mutex<Vec<serde_json::Value>>>,
    script: Arc<Mutex<std::collections::VecDeque<Step>>>,
    thread: Option<thread::JoinHandle<()>>,
}
impl Model {
    fn start() -> Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        listener.set_nonblocking(true)?;
        let endpoint = format!("http://{}", listener.local_addr()?);
        let stop = Arc::new(AtomicBool::new(false));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let script = Arc::new(Mutex::new(std::collections::VecDeque::new()));
        let (token, log, steps) = (stop.clone(), requests.clone(), script.clone());
        let thread = thread::spawn(move || {
            while !token.load(Ordering::Acquire) {
                let (mut socket, _) = match listener.accept() {
                    Ok(value) => value,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                    Err(_) => return,
                };
                let _ = socket.set_nonblocking(false);
                let _ = socket.set_read_timeout(Some(Duration::from_secs(5)));
                let _ = serve(&mut socket, &log, &steps);
            }
        });
        Ok(Self {
            endpoint,
            stop,
            requests,
            script,
            thread: Some(thread),
        })
    }
    fn script(&self, steps: &[Step]) {
        let mut script = self.script.lock().unwrap();
        script.clear();
        script.extend(steps.iter().cloned());
        self.requests.lock().unwrap().clear();
    }
    /// Every `tool` message content the helper sent back to the model.
    fn tool_messages(&self) -> Vec<String> {
        let requests = self.requests.lock().unwrap();
        let Some(last) = requests.last() else {
            return Vec::new();
        };
        last["messages"]
            .as_array()
            .map(|messages| {
                messages
                    .iter()
                    .filter(|m| m["role"] == "tool")
                    .filter_map(|m| m["content"].as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default()
    }
}
impl Drop for Model {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
fn serve(
    socket: &mut std::net::TcpStream,
    log: &Mutex<Vec<serde_json::Value>>,
    steps: &Mutex<std::collections::VecDeque<Step>>,
) -> std::io::Result<()> {
    let mut request = Vec::new();
    let (header_end, length) = loop {
        let mut bytes = [0; 8192];
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
    let respond = |socket: &mut std::net::TcpStream, value: serde_json::Value| {
        let payload = value.to_string();
        write!(
            socket,
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            payload.len(),
            payload
        )
    };
    if request.starts_with(b"POST /api/show ") {
        respond(
            socket,
            serde_json::json!({"capabilities": ["completion", "tools"], "model_info": {"fixture.context_length": 16384}}),
        )
    } else if request.starts_with(b"POST /api/chat ") {
        let chat: serde_json::Value = serde_json::from_slice(body).unwrap_or_default();
        log.lock().unwrap().push(chat);
        let step = steps.lock().unwrap().pop_front().unwrap_or(Step::Final);
        socket.write_all(
            b"HTTP/1.1 200 OK\r\nContent-Type: application/x-ndjson\r\nConnection: close\r\n\r\n",
        )?;
        let response = match step {
            Step::ExecCmd(command) => serde_json::json!({
                "message": {"role": "assistant", "content": "", "tool_calls": [
                    {"function": {"name": "exec_cmd", "arguments": {"command": command, "cwd": "."}}}]},
                "done": true, "done_reason": "stop", "prompt_eval_count": 32, "eval_count": 2}),
            Step::Final => serde_json::json!({
                "message": {"role": "assistant", "content": "COMPLETE"},
                "done": true, "done_reason": "stop", "prompt_eval_count": 48, "eval_count": 3}),
        };
        socket.write_all(format!("{response}\n").as_bytes())
    } else {
        Ok(())
    }
}

// ----------------------------------------------------------------- turns

/// Queues a new-conversation input and waits for its turn.
fn submit(fixture: &Fixture, project: [u8; 16], prompt: &str) -> Result<[u8; 16]> {
    let accepted = queue_submit::submit_queued(&|| fixture.attach(), project, None, 0, prompt)?;
    id(accepted.operation_id)
}
fn finish(
    fixture: &Fixture,
    operation: [u8; 16],
    limit: Duration,
) -> Result<pb::ConversationEvent> {
    let end = Instant::now() + limit;
    loop {
        let event = fixture.attach()?.observe_conversation(operation, 0, 0)?;
        if matches!(event.kind, Some(3..=6)) {
            return Ok(event);
        }
        if Instant::now() >= end {
            return Err("turn did not end".into());
        }
        thread::sleep(Duration::from_millis(50));
    }
}
fn cancel(fixture: &Fixture, operation: [u8; 16]) -> Result<Duration> {
    let started = Instant::now();
    fixture
        .attach()?
        .cancel_conversation(operation, 1, &|| false)?;
    Ok(started.elapsed())
}
/// Kind 14 decision of an exec_cmd call, as these cases read it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Disposition {
    Admitted,
    Refused,
    Other(journal::ToolDecision),
}
/// One exec_cmd call as the journal states it: the kind 14 record (the only
/// copy of the command) plus identities recomputed with the storage
/// derivations (TCR-D23); nothing here is read from a retired kind 22.
struct Intent {
    disposition: Disposition,
    command: String,
    variables: Vec<journal::SecretKind>,
    refusal: Option<journal::ExecCmdRefusal>,
    stdout_payload: [u8; 16],
}
/// The records of one operation's exec_cmd calls, decoded from the journal:
/// kind 14, the job's kind 28 and the call's kind 29.
struct Call {
    intent: Intent,
    record: journal::ToolCall,
    settled: Option<journal::JobSettled>,
    result: Option<journal::JobResult>,
}
/// Every record of the journal, in order.
fn records(bytes: &[u8]) -> Result<Vec<journal::Record>> {
    let mut out = Vec::new();
    let mut offset = 0;
    while offset < bytes.len() {
        let length = u32::from_be_bytes(bytes[offset + 12..offset + 16].try_into()?) as usize;
        let frame = &bytes[offset..offset + length];
        out.push(
            journal::decode_record(
                u16::from_be_bytes([frame[10], frame[11]]),
                &frame[144..frame.len() - 40],
            )
            .map_err(|e| format!("decode: {e:?}"))?,
        );
        offset += length;
    }
    Ok(out)
}
fn exec_cmd_calls(fixture: &Fixture, operation: [u8; 16]) -> Result<Vec<Call>> {
    let (bytes, replay) = fixture.journal()?;
    let op = replay
        .operations
        .get(&operation)
        .ok_or("operation absent")?;
    let decode =
        |range: asura_storage::authority::conversation::Locator| -> Result<journal::Record> {
            let frame = &bytes[range.range()];
            Ok(journal::decode_record(
                u16::from_be_bytes([frame[10], frame[11]]),
                &frame[144..frame.len() - 40],
            )
            .map_err(|e| format!("decode: {e:?}"))?)
        };
    let all = records(&bytes)?;
    let mut out = Vec::new();
    for tool in &op.tools {
        let journal::Record::ToolCall(record) = decode(tool.intent_frame)? else {
            return Err("kind 14 expected".into());
        };
        let job = journal::exec_cmd_job_id(operation, record.generation, record.ordinal);
        let settled = all.iter().find_map(|r| match r {
            journal::Record::JobSettled(s) if s.job == job => Some((**s).clone()),
            _ => None,
        });
        let result = match &tool.result_frame {
            Some(frame) => match decode(*frame)? {
                journal::Record::JobResult(result) => Some(*result),
                _ => return Err("kind 29 expected".into()),
            },
            None => None,
        };
        let arguments: serde_json::Value =
            serde_json::from_str(&record.arguments).unwrap_or_default();
        let intent = Intent {
            disposition: match record.decision {
                journal::ToolDecision::Run => Disposition::Admitted,
                journal::ToolDecision::Refused => Disposition::Refused,
                other => Disposition::Other(other),
            },
            command: arguments["command"].as_str().unwrap_or_default().to_owned(),
            variables: record.number_kinds.clone(),
            refusal: record.refusal,
            stdout_payload: asura_storage::payload::payload_id(
                replay.installation_id,
                journal::exec_cmd_job_id(operation, record.generation, record.ordinal),
                asura_storage::payload::ObjectKind::Stdout,
            ),
        };
        if let Some(call) = tool.exec_cmd.as_deref() {
            assert_eq!(
                call.stdout_payload, intent.stdout_payload,
                "derived payload ID"
            );
        }
        out.push(Call {
            intent,
            record: *record,
            settled,
            result,
        });
    }
    Ok(out)
}
/// The first exec_cmd call of `operation` once its job's kind 28 committed:
/// a job may settle after its turn ended (J3 yield window).
fn settled_call(fixture: &Fixture, operation: [u8; 16], limit: Duration) -> Result<Call> {
    let end = Instant::now() + limit;
    loop {
        let mut calls = exec_cmd_calls(fixture, operation)?;
        if calls.first().is_some_and(|c| c.settled.is_some()) {
            return Ok(calls.remove(0));
        }
        if Instant::now() >= end {
            return Err("job did not settle".into());
        }
        thread::sleep(Duration::from_millis(50));
    }
}
fn hex(id: [u8; 16]) -> String {
    id.iter().map(|b| format!("{b:02x}")).collect()
}
fn object(fixture: &Fixture, id: [u8; 16]) -> PathBuf {
    fixture.home.join(".asura/payloads/objects").join(hex(id))
}
fn staging(fixture: &Fixture, id: [u8; 16]) -> PathBuf {
    fixture.home.join(".asura/payloads/staging").join(hex(id))
}
fn wait_for(limit: Duration, what: &str, mut check: impl FnMut() -> bool) -> Result<()> {
    let end = Instant::now() + limit;
    while !check() {
        if Instant::now() >= end {
            return Err(format!("{what} did not happen").into());
        }
        thread::sleep(Duration::from_millis(20));
    }
    Ok(())
}
fn token(seed: &str) -> String {
    ["ghp_", &seed.repeat(12)].concat()
}
fn holds(bytes: &[u8], needle: &str) -> bool {
    bytes.windows(needle.len()).any(|w| w == needle.as_bytes())
}
/// The raw-byte sweep: staging, payloads, journal, audit, logs and config.
fn sweep(root: &Path, secrets: &[&str]) -> Result<()> {
    for entry in std::fs::read_dir(root)? {
        let path = entry?.path();
        let meta = std::fs::symlink_metadata(&path)?;
        if meta.is_dir() {
            sweep(&path, secrets)?;
        } else if meta.is_file() {
            let bytes = std::fs::read(&path)?;
            for secret in secrets {
                if holds(&bytes, secret) {
                    return Err(format!("raw secret bytes in {}", path.display()).into());
                }
            }
        }
    }
    Ok(())
}
fn within_100ms<T>(label: &str, call: impl FnOnce() -> Result<T>) -> Result<T> {
    let started = Instant::now();
    let value = call()?;
    let elapsed = started.elapsed();
    if elapsed >= Duration::from_millis(100) {
        return Err(format!("{label} took {elapsed:?}").into());
    }
    Ok(value)
}
fn setup(label: &str) -> Result<(Fixture, Model, [u8; 16], PathBuf)> {
    let model = Model::start()?;
    let mut fixture = Fixture::new(label)?;
    fixture.start()?;
    fixture.wait_ready()?;
    let (project, directory) = fixture.project("project")?;
    fixture.model(&model.endpoint)?;
    Ok((fixture, model, project, directory))
}

// ------------------------------------------------------------------ cases

/// J1T-24 (SDE-01 exec_cmd part, transport level), J1T-14 (SDI-03), J1T-03
/// integration and the refused forms: one turn of four exec_cmd calls.
fn secrets_turn() -> Result<()> {
    let (fixture, model, project, directory) = setup("secrets")?;
    let first = token("Qr5");
    let second = token("Tk8");
    std::fs::write(
        directory.join(".env"),
        format!("GITHUB_TOKEN={first}\nPLAIN=value\n"),
    )?;
    model.script(&[
        Step::ExecCmd("cat .env".into()),
        Step::ExecCmd(format!(
            "printf '%s' \"{second}\" > out.txt; sleep 2; printf done"
        )),
        Step::ExecCmd(format!("echo '{first}'")),
        Step::Final,
    ]);
    let operation = submit(&fixture, project, "Run the fixture commands.")?;
    // SDI-03: while the second call sleeps, no process argument list holds the value.
    let mut ps_checks = 0;
    let end = Instant::now() + Duration::from_secs(30);
    while Instant::now() < end && !directory.join("out.txt").exists() {
        thread::sleep(Duration::from_millis(20));
    }
    for _ in 0..10 {
        let ps = Command::new("/bin/ps")
            .args(["-axww", "-o", "args="])
            .output()?;
        assert!(!holds(&ps.stdout, &second), "ps showed the value");
        ps_checks += 1;
        thread::sleep(Duration::from_millis(100));
    }
    let event = finish(&fixture, operation, Duration::from_secs(60))?;
    assert_eq!(event.kind, Some(3), "{event:?}");
    // The TUI's activity source: rewritten arguments; a job result has no
    // output preview (PC12), and the activity names the job (J4T-09).
    assert!(event.tools.iter().all(|t| t.job_id.is_some()));
    assert!(
        event.tools[1]
            .arguments
            .as_deref()
            .unwrap_or("")
            .contains("ASURA_SECRET_1")
    );
    let calls = exec_cmd_calls(&fixture, operation)?;
    assert_eq!(calls.len(), 3);
    // 1. cat .env: placeholders in the stored output. Results carry no
    // output text (PC12); the J1 tails are retired.
    let cat = &calls[0];
    assert_eq!(cat.intent.disposition, Disposition::Admitted);
    let settled = cat.settled.as_ref().ok_or("kind 28")?;
    assert_eq!(settled.capture_state, JobCaptureState::Complete);
    cat.result.as_ref().ok_or("kind 29")?;
    wait_for(Duration::from_secs(5), "install", || {
        object(&fixture, cat.intent.stdout_payload).exists()
    })?;
    let stored = std::fs::read(object(&fixture, cat.intent.stdout_payload))?;
    let digest: [u8; 32] = Sha256::digest(&stored).into();
    assert_eq!(settled.stdout.capture.digest, Some(digest));
    assert!(holds(&stored, "GITHUB_TOKEN=$ASURA_SECRET_1"));
    // 2. command secret: kind 14 holds the rewrite; the command used the value.
    let rewrite = &calls[1];
    assert!(
        rewrite.intent.command.contains("ASURA_SECRET_1"),
        "{}",
        rewrite.intent.command
    );
    assert!(!rewrite.intent.command.contains(&second));
    assert_eq!(
        rewrite.intent.variables,
        vec![journal::SecretKind::GithubToken]
    );
    assert_eq!(std::fs::read(directory.join("out.txt"))?, second.as_bytes());
    std::fs::remove_file(directory.join("out.txt"))?;
    // 3. single quotes (SL7): runs from the whole-command variable; kind 14
    // holds the display text, bit 1 and the fallback reason.
    let quoted = &calls[2];
    assert_eq!(quoted.intent.disposition, Disposition::Admitted);
    assert!(quoted.record.whole_command);
    assert_eq!(
        quoted.intent.refusal.map(|r| r.code),
        Some(journal::RefusalCode::SecretSingleQuoted)
    );
    assert_eq!(quoted.intent.command, "echo '$ASURA_SECRET_1'");
    assert_eq!(quoted.record.form, journal::ArgumentForm::Canonical);
    assert_eq!(
        quoted.settled.as_ref().map(|s| s.capture_state),
        Some(JobCaptureState::Complete)
    );
    wait_for(Duration::from_secs(5), "install", || {
        object(&fixture, quoted.intent.stdout_payload).exists()
    })?;
    let echoed = std::fs::read(object(&fixture, quoted.intent.stdout_payload))?;
    assert_eq!(echoed, b"$ASURA_SECRET_1\n");
    // What the model saw: three handle deliveries, no output text.
    let messages = model.tool_messages();
    assert_eq!(messages.len(), 3, "{messages:?}");
    for message in &messages {
        assert!(!message.contains(&first) && !message.contains(&second));
        assert!(message.starts_with("job: "), "{message}");
        assert!(
            !message.contains("GITHUB_TOKEN"),
            "no output text: {message}"
        );
    }
    assert!(
        messages[2]
            .contains("command filtered: run from a secret variable: echo '$ASURA_SECRET_1'"),
        "SL7 line: {}",
        messages[2]
    );
    // The service stays available.
    model.script(&[Step::ExecCmd("printf after".into()), Step::Final]);
    let next = submit(&fixture, project, "Again.")?;
    assert_eq!(
        finish(&fixture, next, Duration::from_secs(30))?.kind,
        Some(3)
    );
    drop(model);
    let mut fixture = fixture;
    fixture.stop()?;
    // The fixture's own input is the only expected copy.
    std::fs::remove_file(directory.join(".env"))?;
    sweep(&fixture.home, &[&first, &second])?;
    println!(
        "PASS J1T-24/J1T-14 cat .env placeholders to the model, rewritten command in kind 14 and {ps_checks} ps checks, single-quoted value run from a secret variable, raw-byte sweep clean"
    );
    Ok(())
}

/// SDI-12 (SL7): real service, helper, guardian and sandbox. A model call
/// with a single-quoted fixture token runs through `eval` from the
/// whole-command variable and uses the real value; kind 14 has decision
/// `run`, field 20 bit 1 and no value; while it runs no process argument
/// vector holds the value; a raw-byte sweep of the journal, payloads, audit,
/// logs and config finds no value.
fn whole_command_turn() -> Result<()> {
    let (fixture, model, project, directory) = setup("whole")?;
    let value = token("Wv4");
    model.script(&[
        Step::ExecCmd(format!(
            "printf '%s' '{value}' > whole.txt; sleep 2; printf '%s' '{value}'"
        )),
        Step::Final,
    ]);
    let operation = submit(&fixture, project, "Run the single-quoted fixture.")?;
    let end = Instant::now() + Duration::from_secs(30);
    while Instant::now() < end && !directory.join("whole.txt").exists() {
        thread::sleep(Duration::from_millis(20));
    }
    // While `sleep 2` runs: no argument vector holds the value; the shell's
    // holds the preamble and the fixed `eval` text.
    let mut ps_checks = 0;
    let mut saw_eval = false;
    for _ in 0..10 {
        let ps = Command::new("/bin/ps")
            .args(["-axww", "-o", "args="])
            .output()?;
        assert!(!holds(&ps.stdout, &value), "ps showed the value");
        saw_eval |= holds(&ps.stdout, "eval \"$ASURA_SECRET_COMMAND\"");
        ps_checks += 1;
        thread::sleep(Duration::from_millis(100));
    }
    assert!(saw_eval, "the shell ran the fixed eval text");
    let event = finish(&fixture, operation, Duration::from_secs(60))?;
    assert_eq!(event.kind, Some(3), "{event:?}");
    // The activity source marks the call and shows the display text.
    assert_eq!(event.tools[0].command_filtered, Some(true));
    assert!(
        event.tools[0]
            .arguments
            .as_deref()
            .unwrap_or("")
            .contains("printf '%s' '$ASURA_SECRET_1' > whole.txt")
    );
    assert_eq!(
        std::fs::read(directory.join("whole.txt"))?,
        value.as_bytes()
    );
    std::fs::remove_file(directory.join("whole.txt"))?;
    let calls = exec_cmd_calls(&fixture, operation)?;
    assert_eq!(calls.len(), 1);
    let call = &calls[0];
    assert_eq!(call.intent.disposition, Disposition::Admitted);
    assert!(call.record.whole_command && !call.record.command_cut);
    assert_eq!(
        call.intent.refusal.map(|r| r.code),
        Some(journal::RefusalCode::SecretSingleQuoted)
    );
    assert_eq!(
        call.intent.command,
        "printf '%s' '$ASURA_SECRET_1' > whole.txt; sleep 2; printf '%s' '$ASURA_SECRET_1'"
    );
    assert_eq!(
        call.intent.variables,
        vec![journal::SecretKind::GithubToken]
    );
    // Replay derives the fixed command and one variable (SL7 amendment).
    let (_, replay) = fixture.journal()?;
    let derived = replay.operations[&operation].tools[0]
        .exec_cmd
        .as_deref()
        .ok_or("derived exec_cmd")?;
    assert_eq!(derived.fixed_command, Some(journal::WHOLE_COMMAND));
    assert_eq!(derived.secret_variables, 1);
    let settled = call.settled.as_ref().ok_or("kind 28")?;
    assert_eq!(settled.capture_state, JobCaptureState::Complete);
    assert_eq!((settled.exit_kind, settled.exit_value), (1, 0));
    call.result.as_ref().ok_or("kind 29")?;
    // The printed value is the command's literal, replaced by its number.
    wait_for(Duration::from_secs(5), "install", || {
        object(&fixture, call.intent.stdout_payload).exists()
    })?;
    assert_eq!(
        std::fs::read(object(&fixture, call.intent.stdout_payload))?,
        b"$ASURA_SECRET_1"
    );
    let messages = model.tool_messages();
    assert_eq!(messages.len(), 1, "{messages:?}");
    assert!(!messages[0].contains(&value));
    drop(model);
    let mut fixture = fixture;
    fixture.stop()?;
    sweep(&fixture.home, &[&value])?;
    println!(
        "PASS SDI-12 single-quoted value ran through eval from ASURA_SECRET_COMMAND, kind 14 run with bit 1 and no value, {ps_checks} ps checks, raw-byte sweep clean"
    );
    Ok(())
}

/// J1T-05 integration: no 60 s limit; turn cancel stops a command. J3: the
/// call returns `running` at its 10 s yield window and the job settles
/// later with no turn running (J3T-04, TJ-02).
fn timeout_removed() -> Result<()> {
    let (fixture, model, project, directory) = setup("timeout")?;
    model.script(&[Step::ExecCmd("sleep 65; exit 7".into()), Step::Final]);
    let started = Instant::now();
    let operation = submit(&fixture, project, "Long.")?;
    // Controls respond while the command runs.
    thread::sleep(Duration::from_secs(5));
    within_100ms("inspect during a long command", || {
        Ok(fixture.attach()?.inspect()?)
    })?;
    let event = finish(&fixture, operation, Duration::from_secs(120))?;
    assert_eq!(event.kind, Some(3));
    let returned = started.elapsed();
    assert!(
        returned < Duration::from_secs(60),
        "the turn ended at the yield window: {returned:?}"
    );
    // EX12: the job outlived its turn.
    assert_eq!(event.outlived_jobs, Some(1), "{event:?}");
    let messages = model.tool_messages();
    assert!(messages[0].contains("state: running"), "{}", messages[0]);
    let call = settled_call(&fixture, operation, Duration::from_secs(120))?;
    let result = call.result.as_ref().ok_or("kind 29")?;
    assert_eq!(result.status, 1);
    let settled = call.settled.as_ref().ok_or("kind 28")?;
    assert_eq!((settled.exit_kind, settled.exit_value), (1, 7));
    assert!(started.elapsed() >= Duration::from_secs(65));
    model.script(&[
        Step::ExecCmd("printf '%s' $$ > pid; sleep 300".into()),
        Step::Final,
    ]);
    let operation = submit(&fixture, project, "Cancel me.")?;
    wait_for(Duration::from_secs(20), "command start", || {
        directory.join("pid").exists()
    })?;
    let latency = cancel(&fixture, operation)?;
    let event = finish(&fixture, operation, Duration::from_secs(20))?;
    assert_eq!(event.kind, Some(5), "{event:?}");
    let calls = exec_cmd_calls(&fixture, operation)?;
    assert_eq!(calls[0].result.as_ref().map(|r| r.status), Some(6));
    let pid: i32 = std::fs::read_to_string(directory.join("pid"))?.parse()?;
    // SAFETY: signal 0 only probes the (now absent) process group.
    assert!(unsafe { libc::kill(-pid, 0) } != 0, "group stopped");
    println!(
        "PASS J1T-05 a 65 s command completed with exit 7 in {:?}; turn cancel ({latency:?}) stopped a command",
        started.elapsed()
    );
    Ok(())
}

/// J1T-06 integration: setup held over 2 s still spawns; cancel ends a
/// held setup with `not_started`.
fn setup_budget_removed() -> Result<()> {
    let (fixture, model, project, directory) = setup("setup")?;
    let hold = fixture.home.join("exec-cmd-hold-setup");
    std::fs::write(&hold, b"")?;
    model.script(&[Step::ExecCmd("printf ran > marker".into()), Step::Final]);
    let operation = submit(&fixture, project, "Held setup.")?;
    thread::sleep(Duration::from_millis(2600));
    assert!(!directory.join("marker").exists(), "setup is held");
    std::fs::remove_file(&hold)?;
    assert_eq!(
        finish(&fixture, operation, Duration::from_secs(30))?.kind,
        Some(3)
    );
    assert!(
        directory.join("marker").exists(),
        "spawned after a 2.6 s setup"
    );
    std::fs::write(&hold, b"")?;
    model.script(&[Step::ExecCmd("printf ran > marker2".into()), Step::Final]);
    let operation = submit(&fixture, project, "Held setup cancelled.")?;
    thread::sleep(Duration::from_millis(1500));
    cancel(&fixture, operation)?;
    assert_eq!(
        finish(&fixture, operation, Duration::from_secs(20))?.kind,
        Some(5)
    );
    std::fs::remove_file(&hold)?;
    let calls = exec_cmd_calls(&fixture, operation)?;
    let settled = calls[0].settled.as_ref().ok_or("kind 28")?;
    assert_eq!(settled.capture_state, JobCaptureState::NotStarted);
    assert_eq!(calls[0].result.as_ref().map(|r| r.status), Some(6));
    assert!(!directory.join("marker2").exists());
    assert!(!staging(&fixture, calls[0].intent.stdout_payload).exists());
    println!("PASS J1T-06 setup held 2.6 s still spawned; cancel during setup settled not_started");
    Ok(())
}

/// J1T-07 integration: 80 MiB of output is retained whole under 1 GiB.
fn no_output_stop() -> Result<()> {
    let (fixture, model, project, _) = setup("big")?;
    let size = 80 * 1024 * 1024;
    model.script(&[
        Step::ExecCmd(format!(
            "/usr/bin/yes 0123456789abcdef | /usr/bin/head -c {size}"
        )),
        Step::Final,
    ]);
    let operation = submit(&fixture, project, "Big.")?;
    assert_eq!(
        finish(&fixture, operation, Duration::from_secs(600))?.kind,
        Some(3)
    );
    let calls = vec![settled_call(&fixture, operation, Duration::from_secs(600))?];
    let settled = calls[0].settled.as_ref().ok_or("kind 28")?;
    assert_eq!(settled.capture_state, JobCaptureState::Complete);
    assert_eq!(settled.stdout.capture.stored_bytes, size);
    assert!(settled.final_reservation >= size);
    wait_for(Duration::from_secs(20), "install", || {
        object(&fixture, calls[0].intent.stdout_payload).exists()
    })?;
    assert_eq!(
        std::fs::metadata(object(&fixture, calls[0].intent.stdout_payload))?.len(),
        size
    );
    println!(
        "PASS J1T-07 80 MiB retained whole; final reservation {} MiB",
        settled.final_reservation >> 20
    );
    Ok(())
}

/// J1T-21 integration and J1T-15 controls: a stalled scanner only
/// backpressures; cancel gives `unconfirmed` at 2.5 s. (The J1 `pending_full`
/// busy code is retired: J3 queues a start behind `pending_installs`.)
fn overload() -> Result<()> {
    let (fixture, model, project, _) = setup("overload")?;
    let gate = fixture.home.join("capture-hold-scan");
    std::fs::write(&gate, b"")?;
    let flood = "while :; do printf '%0200d\\n' 1; done";
    for round in 0..4 {
        model.script(&[Step::ExecCmd(flood.into()), Step::Final]);
        let operation = submit(&fixture, project, "Flood.")?;
        thread::sleep(Duration::from_millis(if round == 0 { 3000 } else { 800 }));
        let event = fixture.attach()?.observe_conversation(operation, 0, 0)?;
        assert!(!matches!(event.kind, Some(3..=6)), "running, not cancelled");
        within_100ms("inspect during a stalled capture", || {
            Ok(fixture.attach()?.inspect()?)
        })?;
        let cancel_started = Instant::now();
        within_100ms("cancel during a stalled capture", || {
            cancel(&fixture, operation)
        })?;
        assert_eq!(
            finish(&fixture, operation, Duration::from_secs(20))?.kind,
            Some(5)
        );
        // The cancel ended the yield window: kind 29 status 6 before the
        // terminal; the job settles at the L3 bound after it (TJ-04).
        let calls = vec![settled_call(&fixture, operation, Duration::from_secs(20))?];
        let settled_after = cancel_started.elapsed();
        assert_eq!(calls[0].result.as_ref().map(|r| r.status), Some(6));
        let settled = calls[0].settled.as_ref().ok_or("kind 28")?;
        assert_eq!(settled.capture_state, JobCaptureState::Unconfirmed);
        assert_eq!(settled.capture_reason, CaptureReason::SettlementUnconfirmed);
        assert!(
            settled_after >= Duration::from_millis(2400),
            "{settled_after:?}"
        );
        if round == 0 {
            println!("J1T-21: unconfirmed settled {settled_after:?} after the cancel");
        }
    }
    std::fs::remove_file(&gate)?;
    println!("PASS J1T-21 stalled capture only backpressures; unconfirmed at L3");
    Ok(())
}
/// J1T-22 (RG-05): crashes at each point; recovery installs once and never
/// reruns a command.
fn crash_points() -> Result<()> {
    let model = Model::start()?;
    let mut fixture = Fixture::new("crash")?;
    fixture.start()?;
    fixture.wait_ready()?;
    let (project, directory) = fixture.project("project")?;
    fixture.model(&model.endpoint)?;
    let restart = |fixture: &mut Fixture| -> Result<()> {
        fixture.crash()?;
        fixture.start()?;
        fixture.wait_ready()
    };
    let effects = |name: &str| -> usize {
        std::fs::read_to_string(directory.join(name)).map_or(0, |s| s.len())
    };
    // (a) After kind 14, before spawn.
    std::fs::write(fixture.home.join("exec-cmd-hold-setup"), b"")?;
    model.script(&[Step::ExecCmd("printf x >> a.txt".into()), Step::Final]);
    let a = submit(&fixture, project, "a")?;
    wait_for(Duration::from_secs(20), "kind 14", || {
        exec_cmd_calls(&fixture, a).is_ok_and(|c| !c.is_empty())
    })?;
    restart(&mut fixture)?;
    std::fs::remove_file(fixture.home.join("exec-cmd-hold-setup"))?;
    assert_eq!(effects("a.txt"), 0, "never ran, never reruns");
    // (b) While running.
    model.script(&[
        Step::ExecCmd("printf x >> b.txt; printf partial; sleep 60".into()),
        Step::Final,
    ]);
    let b = submit(&fixture, project, "b")?;
    wait_for(Duration::from_secs(20), "running", || effects("b.txt") == 1)?;
    thread::sleep(Duration::from_millis(300));
    restart(&mut fixture)?;
    // (c) After sealing, before kind 28 commits.
    model.script(&[
        Step::ExecCmd("printf x >> c.txt; printf sealed".into()),
        Step::Final,
    ]);
    let c = submit(&fixture, project, "c")?;
    wait_for(Duration::from_secs(20), "kind 14", || {
        exec_cmd_calls(&fixture, c).is_ok_and(|found| !found.is_empty())
    })?;
    std::fs::write(fixture.home.join("journal-hold-appends"), b"")?;
    wait_for(Duration::from_secs(20), "command ran", || {
        effects("c.txt") == 1
    })?;
    thread::sleep(Duration::from_millis(500));
    fixture.crash()?;
    std::fs::remove_file(fixture.home.join("journal-hold-appends"))?;
    fixture.start()?;
    fixture.wait_ready()?;
    // (d) After kind 28, install fails: recovery installs after the digest check.
    std::fs::write(fixture.home.join("payload-fault-install"), b"eio")?;
    model.script(&[
        Step::ExecCmd("printf x >> d.txt; printf installed-later".into()),
        Step::Final,
    ]);
    let d = submit(&fixture, project, "d")?;
    assert_eq!(finish(&fixture, d, Duration::from_secs(30))?.kind, Some(3));
    std::fs::remove_file(fixture.home.join("payload-fault-install"))?;
    let d_calls = exec_cmd_calls(&fixture, d)?;
    assert!(staging(&fixture, d_calls[0].intent.stdout_payload).exists());
    restart(&mut fixture)?;
    // (e) After kind 29.
    model.script(&[
        Step::ExecCmd("printf x >> e.txt; printf done".into()),
        Step::Final,
    ]);
    let e = submit(&fixture, project, "e")?;
    assert_eq!(finish(&fixture, e, Duration::from_secs(30))?.kind, Some(3));
    restart(&mut fixture)?;
    // Recovery is derived: wait until the store is ready (an admitted call).
    model.script(&[Step::ExecCmd("printf ready".into()), Step::Final]);
    let probe = submit(&fixture, project, "probe")?;
    finish(&fixture, probe, Duration::from_secs(30))?;
    for (name, count) in [
        ("a.txt", 0),
        ("b.txt", 1),
        ("c.txt", 1),
        ("d.txt", 1),
        ("e.txt", 1),
    ] {
        assert_eq!(effects(name), count, "{name}: no rerun");
    }
    let (_, replay) = fixture.journal()?;
    for (label, operation) in [("a", a), ("b", b), ("c", c)] {
        let op = &replay.operations[&operation];
        let terminal = op.terminal.as_ref().ok_or("terminal")?;
        assert_eq!(terminal.kind, journal::TerminalKind::Interrupted, "{label}");
        op.tools[0].exec_cmd.as_ref().ok_or("exec_cmd")?;
        // J2 recovery is explicit: an unsettled job gets kind 28
        // `interrupted` at the next start.
        let call = &exec_cmd_calls(&fixture, operation)?[0];
        assert!(
            call.settled
                .as_ref()
                .is_none_or(|s| s.outcome == journal::JobOutcome::Interrupted),
            "{label}: interrupted at recovery"
        );
    }
    // (a) never staged; (b) and (c) keep unverified prefixes.
    let ca = exec_cmd_calls(&fixture, a)?;
    assert!(!object(&fixture, ca[0].intent.stdout_payload).exists());
    for operation in [b, c] {
        let call = &exec_cmd_calls(&fixture, operation)?[0];
        assert!(staging(&fixture, call.intent.stdout_payload).exists());
        assert!(!object(&fixture, call.intent.stdout_payload).exists());
    }
    // (d) installed by recovery after the digest check.
    let call = &exec_cmd_calls(&fixture, d)?[0];
    assert!(object(&fixture, call.intent.stdout_payload).exists());
    assert!(!staging(&fixture, call.intent.stdout_payload).exists());
    let stored = std::fs::read(object(&fixture, call.intent.stdout_payload))?;
    assert_eq!(stored, b"installed-later");
    // (e) intact.
    let call = &exec_cmd_calls(&fixture, e)?[0];
    assert_eq!(
        std::fs::read(object(&fixture, call.intent.stdout_payload))?,
        b"done"
    );
    println!(
        "PASS J1T-22 crash before spawn, while running, after sealing, after kind 28 with install pending, after kind 29: recovery installs once, no rerun"
    );
    Ok(())
}

/// Native model (the system model, opt-in): J1T-25 (a command with a secret
/// is recorded and run as its rewrite) and J1T-05 native (a command longer
/// than 60 s completes). The prompt is the person's own input and is the
/// only journal frame allowed to hold the value.
fn native() -> Result<()> {
    let mut fixture = Fixture::new("native")?;
    fixture.start()?;
    fixture.wait_ready()?;
    let (project, directory) = fixture.project("project")?;
    let secret = token("Nv4");
    let command = format!("printf '%s' \"{secret}\" | wc -c");
    let operation = submit(
        &fixture,
        project,
        &format!(
            "Call exec_cmd exactly once with command `{command}`, cwd `.`. Then reply only with the number it printed. Do not use other tools."
        ),
    )?;
    let event = finish(&fixture, operation, Duration::from_secs(180))?;
    assert_eq!(event.kind, Some(3), "{event:?}");
    let calls = exec_cmd_calls(&fixture, operation)?;
    let call = calls.first().ok_or("no exec_cmd call")?;
    assert_eq!(call.intent.disposition, Disposition::Admitted);
    assert!(
        call.intent.command.contains("ASURA_SECRET_1"),
        "{}",
        call.intent.command
    );
    assert!(!call.intent.command.contains(&secret));
    let result = call.result.as_ref().ok_or("kind 29")?;
    // The model chooses the exact bytes it sends; the record and the run are
    // what this proves. The command succeeded through its secret variable.
    assert_eq!(result.status, 1, "{result:?}");
    assert!(
        call.intent.variables.len() == 1,
        "{:?}",
        call.intent.variables
    );
    assert!(
        event.tools[0]
            .arguments
            .as_deref()
            .unwrap_or("")
            .contains("ASURA_SECRET_1")
    );
    let started = Instant::now();
    let long = submit(
        &fixture,
        project,
        "Call exec_cmd exactly once with command `sleep 65; echo slept-65`, cwd `.`. Then reply only DONE. Do not use other tools.",
    )?;
    let event = finish(&fixture, long, Duration::from_secs(300))?;
    assert_eq!(event.kind, Some(3), "{event:?}");
    let calls = exec_cmd_calls(&fixture, long)?;
    let result = calls[0].result.as_ref().ok_or("kind 29")?;
    assert_eq!(result.status, 1);
    let settled = calls[0].settled.as_ref().ok_or("kind 28")?;
    assert_eq!((settled.exit_kind, settled.exit_value), (1, 0));
    assert!(started.elapsed() >= Duration::from_secs(65));
    fixture.stop()?;
    // Sweep everything except the journal; in the journal, only the turn's
    // own accepted prompt frame holds the value.
    let (bytes, replay) = fixture.journal()?;
    let prompt = replay.operations[&operation].accepted_frame;
    let mut start = 0;
    while let Some(found) = bytes[start..]
        .windows(secret.len())
        .position(|w| w == secret.as_bytes())
    {
        let at = start + found;
        assert!(
            prompt.range().contains(&at),
            "value outside the prompt frame at {at}"
        );
        start = at + 1;
    }
    for entry in ["payloads", "logs", "tmp"] {
        let path = fixture.home.join(".asura").join(entry);
        if path.exists() {
            sweep(&path, &[&secret])?;
        }
    }
    sweep(&directory, &[&secret])?;
    println!(
        "PASS J1T-25/J1T-05 native: rewritten command recorded and run; a 65 s command completed in {:?}",
        started.elapsed()
    );
    Ok(())
}

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

/// EC-E3 (EX-V9) and EC-E1 at the service level: the real service, helper,
/// guardian and sandbox run a model exec_cmd call whose command starts
/// `setsid`, double-fork and pipe-holding escapees. Each is detected and
/// killed, the call settles, its result confirms cleanup only after all
/// three are gone and shows the count, and no escapee writes the project
/// afterwards. Then a late settlement (unverifiable scans, test-support
/// fault) settles with cleanup_confirmed=false, releases the scratch and
/// the slot, and the next call runs.
fn escapees() -> Result<()> {
    let (fixture, model, project, directory) = setup("escapees")?;
    std::fs::write(
        directory.join("escapee.pl"),
        include_str!("../../asura-platform/tests/fixtures/escapee.pl"),
    )?;
    let command = "/usr/bin/perl escapee.pl setsid s; /usr/bin/perl escapee.pl doublefork d; \
        /usr/bin/perl escapee.pl pipehold p; \
        while [ ! -e s.pid ] || [ ! -e d.pid ] || [ ! -e p.pid ]; do sleep 0.05; done; \
        sleep 0.3; exit 0";
    model.script(&[Step::ExecCmd(command.into()), Step::Final]);
    let operation = submit(&fixture, project, "Escape.")?;
    let event = finish(&fixture, operation, Duration::from_secs(60))?;
    assert_eq!(event.kind, Some(3), "{event:?}");
    let calls = exec_cmd_calls(&fixture, operation)?;
    let result = calls[0].result.as_ref().ok_or("kind 29")?;
    assert_eq!(result.status, 1, "{result:?}");
    let settled = calls[0].settled.as_ref().ok_or("kind 28")?;
    assert_eq!((settled.exit_kind, settled.exit_value), (1, 0));
    assert_eq!(settled.cleanup, journal::JobCleanup::Confirmed);
    assert_eq!(settled.escapees, 3);
    assert_eq!(settled.capture_state, JobCaptureState::Complete);
    assert!(
        settled.stdout.capture.eof && settled.stderr.capture.eof,
        "capture EOF"
    );
    for tag in ["s", "d", "p"] {
        let pid: i32 = std::fs::read_to_string(directory.join(format!("{tag}.pid")))?
            .trim()
            .parse()?;
        assert!(
            started(pid).is_none(),
            "{tag} escapee alive after settlement"
        );
        let beat = directory.join(format!("{tag}.beat"));
        let before = std::fs::metadata(&beat)?.len();
        thread::sleep(Duration::from_millis(300));
        assert_eq!(
            std::fs::metadata(&beat)?.len(),
            before,
            "{tag} escapee wrote the project after settlement"
        );
    }
    // Late settlement through the service: scans unverifiable five times.
    std::fs::write(
        fixture.home.join("exec-cmd-guardian-fault"),
        "unverifiable:5",
    )?;
    model.script(&[Step::ExecCmd("printf late-output".into()), Step::Final]);
    let operation = submit(&fixture, project, "Late.")?;
    let event = finish(&fixture, operation, Duration::from_secs(60))?;
    assert_eq!(event.kind, Some(3), "{event:?}");
    std::fs::remove_file(fixture.home.join("exec-cmd-guardian-fault"))?;
    let calls = exec_cmd_calls(&fixture, operation)?;
    let result = calls[0].result.as_ref().ok_or("kind 29")?;
    assert_eq!(result.status, 1, "{result:?}");
    let settled = calls[0].settled.as_ref().ok_or("kind 28")?;
    assert_eq!(settled.cleanup, journal::JobCleanup::Late);
    assert_eq!(settled.capture_state, JobCaptureState::Complete);
    // The slot and scratch were released: the next call runs and confirms.
    model.script(&[Step::ExecCmd("printf again".into()), Step::Final]);
    let operation = submit(&fixture, project, "Again.")?;
    assert_eq!(
        finish(&fixture, operation, Duration::from_secs(60))?.kind,
        Some(3)
    );
    let calls = exec_cmd_calls(&fixture, operation)?;
    let settled = calls[0].settled.as_ref().ok_or("kind 28")?;
    assert_eq!(settled.cleanup, journal::JobCleanup::Confirmed);
    assert_eq!(settled.capture_state, JobCaptureState::Complete);
    let scratch: Vec<_> = std::fs::read_dir(fixture.home.join(".asura/tmp"))?
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().starts_with("exec-cmd-"))
        .collect();
    assert!(scratch.is_empty(), "scratch left: {scratch:?}");
    println!(
        "PASS EC-E3/EC-E1 setsid, double-fork and pipe-holding escapees killed before cleanup_confirmed (escapees=3); late settlement released the slot with cleanup_confirmed=false"
    );
    Ok(())
}

fn child(home: &Path) -> Result<()> {
    asura_platform::journal_gate::install_file_gate(home);
    tracing_subscriber::fmt()
        .with_ansi(false)
        .with_target(false)
        .with_max_level(tracing::Level::DEBUG)
        .with_writer(std::io::stderr)
        .init();
    asura_service::run(RuntimeDirectory::scratch(home, true)?, BUILD, None)?;
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
    if let [_, flag, job, profile, command, fault] = args.as_slice()
        && flag == "--asura-exec-cmd-guardian"
    {
        std::process::exit(asura_platform::exec_cmd::run_guardian_for_test(
            job, profile, command, fault,
        ));
    }
    if args.get(1).map(String::as_str) == Some("--capture-child") {
        if let Err(error) = child(Path::new(&args[2])) {
            eprintln!("capture child failed: {error}");
            std::process::exit(1);
        }
        return;
    }
    let selected: Vec<&str> = args[1..]
        .iter()
        .map(String::as_str)
        .filter(|a| !a.starts_with('-'))
        .collect();
    let cases: [Case; 8] = [
        ("secrets", secrets_turn),
        ("whole", whole_command_turn),
        ("setup", setup_budget_removed),
        ("overload", overload),
        ("crash", crash_points),
        ("big", no_output_stop),
        ("timeout", timeout_removed),
        ("escapees", escapees),
    ];
    if selected.contains(&"native") {
        if let Err(error) = native() {
            eprintln!("capture flow case native FAILED: {error}");
            std::process::exit(1);
        }
        return;
    }
    for (name, case) in cases {
        if !selected.is_empty() && !selected.contains(&name) {
            continue;
        }
        if let Err(error) = case() {
            eprintln!("capture flow case {name} FAILED: {error}");
            std::process::exit(1);
        }
    }
    println!("capture flow qualification passed");
}
