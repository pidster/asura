//! Tool call records (tool-call-records.md) through the real service, the real
//! Swift helper, a scripted loopback Ollama endpoint, the real conversation
//! owner, journal, payload and memory stores, in private scratch homes. Each
//! case ends with a raw-byte sweep for the secrets it used.
//! Cases: TCR-I01..I07, TCR-I09, TCR-E01..E03 (service part) and TCR-P2.
#[path = "support/queue_submit.rs"]
mod queue_submit;
use asura_client::Client;
use asura_control::pb;
use asura_platform::RuntimeDirectory;
use asura_storage::authority::conversation::{self as journal, ToolDecision, ToolFault};
use serde_json::{Value, json};
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
        let home = parent.join(format!("asura-toolcall-{label}-{}", &nonce[..12]));
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
                .arg("--tool-call-child")
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
        let bytes = std::fs::read(self.home.join(".asura/state/control/slot-0.log"))?;
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
        if self.keep || thread::panicking() || std::env::var_os("ASURA_TOOLCALL_KEEP").is_some() {
            eprintln!("tool-call fixture retained: {}", self.home.display());
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

/// One scripted model reply.
#[derive(Clone)]
enum Step {
    /// One tool call with any name and any argument JSON value.
    Call(String, Value),
    /// The final answer.
    Final,
    /// On receipt, create `create` (if any), then answer `then` only once
    /// `release` no longer exists. A test-harness hold, not product code.
    Held {
        create: Option<PathBuf>,
        release: PathBuf,
        then: Box<Step>,
    },
}
fn call(name: &str, arguments: Value) -> Step {
    Step::Call(name.into(), arguments)
}
struct Model {
    endpoint: String,
    stop: Arc<AtomicBool>,
    requests: Arc<Mutex<Vec<Value>>>,
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
                let (log, steps, token) = (log.clone(), steps.clone(), token.clone());
                // Held replies must not block the next request.
                thread::spawn(move || {
                    let _ = serve(&mut socket, &log, &steps, &token);
                });
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
    fn chat_requests(&self) -> usize {
        self.requests.lock().unwrap().len()
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
    log: &Mutex<Vec<Value>>,
    steps: &Mutex<std::collections::VecDeque<Step>>,
    stop: &AtomicBool,
) -> std::io::Result<()> {
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
    let respond = |socket: &mut std::net::TcpStream, value: Value| {
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
            json!({"capabilities": ["completion", "tools"], "model_info": {"fixture.context_length": 32768}}),
        )
    } else if request.starts_with(b"POST /api/chat ") {
        let chat: Value = serde_json::from_slice(body).unwrap_or_default();
        log.lock().unwrap().push(chat);
        let mut step = steps.lock().unwrap().pop_front().unwrap_or(Step::Final);
        while let Step::Held {
            create,
            release,
            then,
        } = step
        {
            if let Some(create) = create {
                let _ = std::fs::write(create, b"");
            }
            let end = Instant::now() + Duration::from_secs(120);
            while release.exists() && Instant::now() < end && !stop.load(Ordering::Acquire) {
                thread::sleep(Duration::from_millis(5));
            }
            step = *then;
        }
        socket.write_all(
            b"HTTP/1.1 200 OK\r\nContent-Type: application/x-ndjson\r\nConnection: close\r\n\r\n",
        )?;
        let response = match step {
            Step::Call(name, arguments) => json!({
                "message": {"role": "assistant", "content": "", "tool_calls": [
                    {"function": {"name": name, "arguments": arguments}}]},
                "done": true, "done_reason": "stop", "prompt_eval_count": 32, "eval_count": 2}),
            _ => json!({
                "message": {"role": "assistant", "content": "COMPLETE"},
                "done": true, "done_reason": "stop", "prompt_eval_count": 48, "eval_count": 3}),
        };
        socket.write_all(format!("{response}\n").as_bytes())
    } else {
        Ok(())
    }
}

// ----------------------------------------------------------------- turns

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
        let event = fixture.attach()?.observe_conversation(operation, 0)?;
        if matches!(event.kind, Some(3..=6)) {
            return Ok(event);
        }
        if Instant::now() >= end {
            return Err(format!("turn did not end: {event:?}").into());
        }
        thread::sleep(Duration::from_millis(50));
    }
}
fn decode(bytes: &[u8], range: std::ops::Range<usize>) -> Result<journal::Record> {
    let frame = &bytes[range];
    Ok(journal::decode_record(
        u16::from_be_bytes([frame[10], frame[11]]),
        &frame[144..frame.len() - 40],
    )
    .map_err(|e| format!("decode: {e:?}"))?)
}
/// One call as the journal holds it: kind 14, and kind 15 when it ran.
struct Recorded {
    call: journal::ToolCall,
    frame: usize,
    result: Option<journal::ToolResult>,
    tool: journal::ToolRecord,
}
fn calls(fixture: &Fixture, operation: [u8; 16]) -> Result<Vec<Recorded>> {
    let (bytes, replay) = fixture.journal()?;
    let op = replay
        .operations
        .get(&operation)
        .ok_or("operation absent")?;
    let mut out = Vec::new();
    for tool in &op.tools {
        let journal::Record::ToolCall(call) = decode(&bytes, tool.intent_frame.clone())? else {
            return Err("kind 14 expected".into());
        };
        let result = match &tool.result_frame {
            Some(frame) => match decode(&bytes, frame.clone())? {
                journal::Record::ToolResult(result) => Some(result),
                _ => return Err("kind 15 expected".into()),
            },
            None => None,
        };
        out.push(Recorded {
            call: *call,
            frame: tool.intent_frame.len(),
            result,
            tool: tool.clone(),
        });
    }
    Ok(out)
}
/// The kinds of every frame of the journal, in order, with their records.
fn frames(bytes: &[u8]) -> Result<Vec<(u16, journal::Record)>> {
    let mut out = Vec::new();
    let mut offset = 0;
    while offset < bytes.len() {
        let length = u32::from_be_bytes(bytes[offset + 12..offset + 16].try_into()?) as usize;
        let kind = u16::from_be_bytes([bytes[offset + 10], bytes[offset + 11]]);
        out.push((kind, decode(bytes, offset..offset + length)?));
        offset += length;
    }
    Ok(out)
}
fn token(seed: &str) -> String {
    ["ghp_", &seed.repeat(12)].concat()
}
fn holds(bytes: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty() && bytes.windows(needle.len()).any(|w| w == needle)
}
fn count(bytes: &[u8], needle: &[u8]) -> usize {
    bytes.windows(needle.len()).filter(|w| *w == needle).count()
}
/// The raw-byte sweep: journal, payloads, memory store, logs and config.
fn sweep(root: &Path, secrets: &[&str]) -> Result<()> {
    for entry in std::fs::read_dir(root)? {
        let path = entry?.path();
        let meta = std::fs::symlink_metadata(&path)?;
        if meta.is_dir() {
            sweep(&path, secrets)?;
        } else if meta.is_file() {
            let bytes = std::fs::read(&path)?;
            for secret in secrets {
                if holds(&bytes, secret.as_bytes()) {
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
fn wait_for(limit: Duration, what: &str, mut check: impl FnMut() -> bool) -> Result<()> {
    let end = Instant::now() + limit;
    while !check() {
        if Instant::now() >= end {
            return Err(format!("timed out waiting for {what}").into());
        }
        thread::sleep(Duration::from_millis(20));
    }
    Ok(())
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
fn arguments(call: &journal::ToolCall) -> Value {
    serde_json::from_str(&call.arguments).unwrap_or_default()
}
const JOURNAL_BYTES: f64 = asura_storage::authority::MAX_JOURNAL as f64;

// ------------------------------------------------------------------ cases

/// TCR-I01 and TCR-E01 (service part) and the E01 part of TCR-P2: a
/// malformed `project` call is recorded and answered with status 3; the turn
/// continues; the model's corrected call runs.
fn malformed_then_valid() -> Result<()> {
    let (fixture, model, project, directory) = setup("malformed")?;
    std::fs::write(directory.join("README.md"), b"E01-CONTENT")?;
    // I01 alone: the turn continues and completes after the rejected call.
    model.script(&[
        call("project", json!({"command": "read_file"})),
        Step::Final,
    ]);
    let operation = submit(&fixture, project, "I01")?;
    let event = finish(&fixture, operation, Duration::from_secs(60))?;
    assert_eq!(event.kind, Some(3), "{event:?}");
    let recorded = calls(&fixture, operation)?;
    assert_eq!(recorded.len(), 1);
    let first = &recorded[0].call;
    assert_eq!(first.decision, ToolDecision::InvalidArguments);
    assert_eq!(first.fault, Some(ToolFault::MissingField));
    assert_eq!(first.name, "project");
    assert_eq!(first.tool_operation, Some(journal::ToolOperation::ReadFile));
    assert_eq!(arguments(first), json!({"command": "read_file"}));
    assert!(
        recorded[0].result.is_none(),
        "a rejected call has no kind 15"
    );
    let messages = model.tool_messages();
    assert_eq!(messages.len(), 1, "{messages:?}");
    assert!(
        messages[0].contains("invalid_arguments: missing_field"),
        "{messages:?}"
    );
    assert!(!messages[0].contains("E01-CONTENT"), "no host read");
    let progress = &event.tools[0];
    assert_eq!(
        (
            progress.state,
            progress.status,
            progress.policy_decision,
            progress.fault
        ),
        (Some(3), Some(3), Some(2), Some(6))
    );
    assert_eq!(
        progress.preview.as_deref(),
        Some("invalid_arguments: missing_field")
    );
    println!(
        "PASS TCR-I01 missing path: kind 14 invalid_arguments/missing_field, no kind 15, status 3 to the model, turn completed"
    );
    // E01: the model corrects itself within the call limit.
    model.script(&[
        call("project", json!({"command": "read_file"})),
        call(
            "project",
            json!({"command": "read_file", "path": "README.md"}),
        ),
        Step::Final,
    ]);
    let operation = submit(&fixture, project, "E01")?;
    let event = finish(&fixture, operation, Duration::from_secs(60))?;
    assert_eq!(event.kind, Some(3), "{event:?}");
    let recorded = calls(&fixture, operation)?;
    assert_eq!(recorded.len(), 2, "two kind 14 records");
    assert_eq!(
        recorded.iter().filter(|r| r.result.is_some()).count(),
        1,
        "one kind 15"
    );
    assert_eq!(recorded[1].call.decision, ToolDecision::Run);
    let result = recorded[1].result.as_ref().ok_or("kind 15")?;
    assert_eq!((result.ordinal, result.status), (2, 1));
    assert!(result.text.contains("E01-CONTENT"));
    let messages = model.tool_messages();
    assert_eq!(messages.len(), 2, "{messages:?}");
    assert!(messages[0].contains("invalid_arguments: missing_field"));
    assert!(messages[1].contains("E01-CONTENT"), "{messages:?}");
    assert_eq!(event.tools.len(), 2);
    assert_eq!(event.tools[1].state, Some(2));
    // TCR-P2 (E01 workload).
    for r in &recorded {
        assert!(r.frame <= 65_050, "kind 14 frame {}", r.frame);
    }
    let total: usize = recorded.iter().map(|r| r.frame).sum();
    println!(
        "TCR-P2 E01: kind 14 frames {:?} bytes ({} per call), {:.6}% of the 8 MiB journal",
        recorded.iter().map(|r| r.frame).collect::<Vec<_>>(),
        total / recorded.len(),
        100.0 * total as f64 / JOURNAL_BYTES
    );
    sweep(&fixture.home, &[])?;
    println!(
        "PASS TCR-E01 service part: rejected call then valid call; two kind 14, one kind 15, the corrected result reached the model"
    );
    Ok(())
}

/// TCR-I02 and TCR-E02 (service part) and the E02 part of TCR-P2.
fn exec_cmd_secret() -> Result<()> {
    let (fixture, model, project, directory) = setup("secret")?;
    let secret = token("Zq7");
    std::fs::write(directory.join(".env"), format!("GITHUB_TOKEN={secret}\n"))?;
    let command = format!("printf '%s' \"{secret}\" > out.txt; cat .env");
    model.script(&[call("exec_cmd", json!({"command": command})), Step::Final]);
    let operation = submit(&fixture, project, "E02")?;
    let event = finish(&fixture, operation, Duration::from_secs(60))?;
    assert_eq!(event.kind, Some(3), "{event:?}");
    // The command ran with the value through its variable.
    assert_eq!(std::fs::read(directory.join("out.txt"))?, secret.as_bytes());
    std::fs::remove_file(directory.join("out.txt"))?;
    std::fs::remove_file(directory.join(".env"))?;
    let recorded = calls(&fixture, operation)?;
    assert_eq!(recorded.len(), 1);
    let record = &recorded[0].call;
    assert_eq!(record.decision, ToolDecision::Run);
    assert_eq!(record.form, journal::ArgumentForm::Canonical);
    let stored = arguments(record);
    let stored_command = stored["command"].as_str().ok_or("command")?;
    assert!(
        stored_command.contains("\"$ASURA_SECRET_1\"")
            || stored_command.contains("${ASURA_SECRET_1}"),
        "{stored_command}"
    );
    assert!(!record.arguments.contains(&secret));
    assert_eq!(record.number_kinds, vec![journal::SecretKind::GithubToken]);
    println!("PASS TCR-I02 kind 14 holds the SL3 placeholder in command; number 1 is github-token");
    let result = recorded[0].result.as_ref().ok_or("kind 15")?;
    assert!(result.text.contains("$ASURA_SECRET_1"), "{}", result.text);
    assert!(!result.text.contains(&secret));
    let progress = &event.tools[0];
    assert_eq!(progress.name.as_deref(), Some("exec_cmd"));
    let shown = format!(
        "{} {}",
        progress.arguments.as_deref().unwrap_or(""),
        progress.preview.as_deref().unwrap_or("")
    );
    assert!(shown.contains("ASURA_SECRET_1"), "{shown}");
    assert!(!shown.contains(&secret));
    for message in model.tool_messages() {
        assert!(!message.contains(&secret));
    }
    assert!(recorded[0].frame <= 65_050);
    println!(
        "TCR-P2 E02: kind 14 frame {} bytes, kind 15 frame {} bytes, {:.6}% of the 8 MiB journal",
        recorded[0].frame,
        recorded[0]
            .tool
            .result_frame
            .as_ref()
            .map_or(0, |f| f.len()),
        100.0 * recorded[0].frame as f64 / JOURNAL_BYTES
    );
    let mut fixture = fixture;
    fixture.stop()?;
    sweep(&fixture.home, &[&secret])?;
    println!(
        "PASS TCR-E02 service part: kind 14, kind 15, activity and model input hold only placeholders; raw-byte sweep clean"
    );
    Ok(())
}

/// TCR-I03: a create_note call with a 16,384-byte body.
fn create_note_body() -> Result<()> {
    // The memory store opens only an `asura-memory-*` scratch root.
    let model = Model::start()?;
    let mut fixture = Fixture::new("create")?;
    let home = PathBuf::from(format!(
        "/private/tmp/asura-memory-toolcall-{:032x}",
        u128::from_ne_bytes(asura_platform::random_id())
    ));
    std::fs::rename(&fixture.home, &home)?;
    fixture.home = home;
    fixture.start()?;
    fixture.wait_ready()?;
    let (project, _) = fixture.project("project")?;
    fixture.model(&model.endpoint)?;
    let marker = format!(
        "TCR-I03-{:032x}",
        u128::from_ne_bytes(asura_platform::random_id())
    );
    let body = format!("{marker} {}", "note text. ".repeat(2000));
    let body = body[..16_384].to_owned();
    assert_eq!(body.len(), 16_384);
    model.script(&[
        call("memory", json!({"command": "create_note", "body": body})),
        Step::Final,
    ]);
    let operation = submit(&fixture, project, "I03")?;
    let event = finish(&fixture, operation, Duration::from_secs(60))?;
    assert_eq!(event.kind, Some(3), "{event:?}");
    let recorded = calls(&fixture, operation)?;
    assert_eq!(recorded.len(), 1);
    let record = &recorded[0].call;
    assert_eq!(record.decision, ToolDecision::Run);
    assert_eq!(arguments(record)["body"].as_str(), Some(body.as_str()));
    let result = recorded[0].result.as_ref().ok_or("kind 15")?;
    assert_eq!(result.status, 1);
    assert!(
        result.text.is_empty(),
        "kind 15 stores no create_note text (TCR-D1)"
    );
    fixture.stop()?;
    let (bytes, replay) = fixture.journal()?;
    let binding = [
        replay.installation_id,
        replay.initialization.graph,
        replay.initialization.request,
    ];
    let expected =
        journal::CreateNoteCall::derive(binding, project, operation, record.generation, 1)
            .map_err(|e| format!("derive: {e:?}"))?;
    assert_eq!(recorded[0].tool.create, Some(expected));
    // The model saw the rendered identity, never stored.
    let hex = |b: &[u8]| b.iter().map(|x| format!("{x:02x}")).collect::<String>();
    let version = hex(&expected.identity.version);
    assert!(model.tool_messages()[0].contains(&format!("version={version}")));
    // No body digest and no second copy anywhere in the journal.
    let digest: [u8; 32] = Sha256::digest(body.as_bytes()).into();
    assert!(!holds(&bytes, &digest), "raw body digest in the journal");
    assert!(
        !holds(&bytes, hex(&digest).as_bytes()),
        "hex body digest in the journal"
    );
    assert_eq!(
        count(&bytes, marker.as_bytes()),
        1,
        "exactly one copy of the body"
    );
    assert!(!holds(&bytes, version.as_bytes()), "derived version stored");
    assert!(
        !holds(&bytes, &expected.identity.version),
        "derived version stored"
    );
    // The note exists in the memory store under the derived version ID.
    use asura_storage::memory;
    let binding = memory::Binding {
        installation_id: memory::Id::new(replay.installation_id)?,
        graph_id: memory::Id::new(replay.initialization.graph)?,
        init_operation_id: memory::Id::new(replay.initialization.request)?,
    };
    let (mut database, ready) = memory::Memory::open(fixture.home.clone(), binding.clone())?;
    fn wait<T>(mut pending: memory::Pending<T>) -> Result<T> {
        let end = Instant::now() + Duration::from_secs(12);
        loop {
            if let Some(result) = pending.poll() {
                return result.map_err(Into::into);
            }
            if Instant::now() >= end {
                return Err("memory deadline".into());
            }
            thread::sleep(Duration::from_millis(2));
        }
    }
    let found = (|| -> Result<memory::Note> {
        wait(ready)?;
        wait(database.get_note(
            binding.clone(),
            memory::Id::new(project)?,
            memory::Id::new(expected.identity.version)?,
        )?)
    })();
    let end = Instant::now() + Duration::from_secs(12);
    while !matches!(database.close(), Ok(true)) && Instant::now() < end {
        thread::sleep(Duration::from_millis(2));
    }
    let note = found?;
    assert_eq!(note.body, body);
    assert_eq!(note.object_id.bytes(), expected.identity.object);
    println!(
        "PASS TCR-I03 16,384-byte body: kind 14 is its only journal copy; the note exists under the derived version; no body digest in the journal"
    );
    Ok(())
}

/// TCR-I04 (restart after a `run` kind 14 before kind 15) and TCR-E03
/// (restart during a running exec_cmd), and TCR-I05 (restart after an
/// invalid call: complete, nothing new written).
fn restarts() -> Result<()> {
    let (mut fixture, model, project, directory) = setup("restart")?;
    let restart = |fixture: &mut Fixture| -> Result<()> {
        fixture.crash()?;
        fixture.start()?;
        fixture.wait_ready()
    };
    let effects = |name: &str| std::fs::read_to_string(directory.join(name)).map_or(0, |s| s.len());
    // I04: committed `run` kind 14, dispatch held at setup, then a crash.
    std::fs::write(fixture.home.join("exec-cmd-hold-setup"), b"")?;
    model.script(&[
        call("exec_cmd", json!({"command": "printf x >> i04.txt"})),
        Step::Final,
    ]);
    let i04 = submit(&fixture, project, "I04")?;
    wait_for(Duration::from_secs(30), "run kind 14", || {
        calls(&fixture, i04).is_ok_and(|c| {
            c.first()
                .is_some_and(|r| r.call.decision == ToolDecision::Run)
        })
    })?;
    restart(&mut fixture)?;
    std::fs::remove_file(fixture.home.join("exec-cmd-hold-setup"))?;
    // E03: a running exec_cmd, then a crash.
    model.script(&[
        call(
            "exec_cmd",
            json!({"command": "printf x >> e03.txt; sleep 60"}),
        ),
        Step::Final,
    ]);
    let e03 = submit(&fixture, project, "E03")?;
    wait_for(Duration::from_secs(30), "running", || {
        effects("e03.txt") == 1
    })?;
    restart(&mut fixture)?;
    // A probe turn proves the service runs new work and nothing reruns.
    model.script(&[
        call("exec_cmd", json!({"command": "printf probe"})),
        Step::Final,
    ]);
    let probe = submit(&fixture, project, "probe")?;
    assert_eq!(
        finish(&fixture, probe, Duration::from_secs(60))?.kind,
        Some(3)
    );
    assert_eq!(effects("i04.txt"), 0, "I04 never ran and never reruns");
    assert_eq!(effects("e03.txt"), 1, "E03 ran once and never reruns");
    let (_, replay) = fixture.journal()?;
    for (label, operation) in [("I04", i04), ("E03", e03)] {
        let op = &replay.operations[&operation];
        let terminal = op.terminal.as_ref().ok_or("terminal")?;
        assert_eq!(terminal.kind, journal::TerminalKind::Interrupted, "{label}");
        assert_eq!(op.tools.len(), 1, "{label}: no new tool record");
        assert!(
            op.tools[0].result_frame.is_none(),
            "{label}: no kind 15 written"
        );
        let event = fixture.attach()?.observe_conversation(operation, 0)?;
        assert_eq!(event.kind, Some(6), "{label} {event:?}");
        let tool = &event.tools[0];
        assert_eq!(tool.state, Some(4), "{label} interrupted");
        assert_eq!(tool.name.as_deref(), Some("exec_cmd"));
        assert!(
            tool.arguments
                .as_deref()
                .unwrap_or("")
                .contains("printf x >>"),
            "{label}"
        );
    }
    println!(
        "PASS TCR-I04 restart after run kind 14 before kind 15: interrupted, no rerun, no new tool record"
    );
    println!(
        "PASS TCR-E03 service part: restart during a running exec_cmd shows the call interrupted with name and arguments; nothing ran again"
    );
    // I05: restart after an invalid call writes nothing but the owner generation.
    model.script(&[
        call("project", json!({"command": "read_file"})),
        Step::Final,
    ]);
    let i05 = submit(&fixture, project, "I05")?;
    assert_eq!(
        finish(&fixture, i05, Duration::from_secs(60))?.kind,
        Some(3)
    );
    let before = fixture.attach()?.observe_conversation(i05, 0)?;
    fixture.stop()?;
    let (bytes_before, _) = fixture.journal()?;
    fixture.start()?;
    fixture.wait_ready()?;
    let after = fixture.attach()?.observe_conversation(i05, 0)?;
    fixture.stop()?;
    let (bytes_after, replay) = fixture.journal()?;
    assert!(
        bytes_after.starts_with(&bytes_before),
        "no record rewritten"
    );
    let appended = frames(&bytes_after[bytes_before.len()..])?;
    assert!(
        !appended.is_empty(),
        "the restart records its owner generation"
    );
    assert!(
        appended.iter().all(|(kind, _)| *kind == 3),
        "only owner generations appended: {:?}",
        appended.iter().map(|(k, _)| *k).collect::<Vec<_>>()
    );
    let op = &replay.operations[&i05];
    assert_eq!(op.tools.len(), 1);
    assert!(op.tools[0].result_frame.is_none());
    let tool = &after.tools[0];
    assert_eq!(
        tool.preview.as_deref(),
        Some("invalid_arguments: missing_field")
    );
    assert_eq!((tool.state, tool.status), (Some(3), Some(3)));
    assert_eq!(
        after
            .tools
            .iter()
            .map(|t| {
                let mut t = t.clone();
                t.elapsed_ms = None;
                t
            })
            .collect::<Vec<_>>(),
        before
            .tools
            .iter()
            .map(|t| {
                let mut t = t.clone();
                t.elapsed_ms = None;
                t
            })
            .collect::<Vec<_>>()
    );
    sweep(&fixture.home, &[])?;
    println!(
        "PASS TCR-I05 restart after invalid_arguments: complete call with rendered text, nothing new but the owner generation"
    );
    Ok(())
}

/// TCR-I06 and TCR-I07: the journal append is held while a call is pending
/// behind another append; controls stay usable; a cancel before the record
/// commits produces kind 7, kind 14 `cancelled`, then the terminal; no time
/// limit fires.
fn held_append_and_cancel() -> Result<()> {
    let (fixture, model, project, _) = setup("held")?;
    let hold = fixture.home.join("journal-hold-appends");
    let release_call = fixture.home.join("release-call");
    std::fs::write(&release_call, b"")?;
    model.script(&[
        Step::Held {
            create: None,
            release: release_call.clone(),
            then: Box::new(call("project", json!({"command": "read_file"}))),
        },
        Step::Final,
        Step::Final,
    ]);
    let operation = submit(&fixture, project, "I06")?;
    wait_for(Duration::from_secs(30), "chat request", || {
        model.chat_requests() >= 1
    })?;
    // Hold appends and occupy the writer with a queue mutation.
    std::fs::write(&hold, b"")?;
    let held_at = Instant::now();
    let request = queue_submit::queue_request(project, None, 0, "held queue mutation");
    let mut submitter = fixture.attach()?;
    let mutation = thread::spawn(move || submitter.queue_conversation_input(request, &|| false));
    thread::sleep(Duration::from_millis(300));
    // The call reaches the service; its kind 14 waits behind the held append.
    std::fs::remove_file(&release_call)?;
    thread::sleep(Duration::from_millis(1_000));
    for _ in 0..5 {
        let mut client = fixture.attach()?;
        within_100ms("Inspect", || Ok(client.inspect()?))?;
        let mut client = fixture.attach()?;
        let event = within_100ms("observe", || Ok(client.observe_conversation(operation, 0)?))?;
        assert!(
            !matches!(event.kind, Some(3..=6)),
            "no time limit ended the turn"
        );
    }
    let (bytes, _) = fixture.journal()?;
    assert!(
        calls(&fixture, operation)?.is_empty(),
        "the call is not yet recorded ({} journal bytes)",
        bytes.len()
    );
    // Cancel before the decision commits.
    let mut canceller = fixture.attach()?;
    let cancel = thread::spawn(move || canceller.cancel_conversation(operation, 1, &|| false));
    thread::sleep(Duration::from_millis(300));
    let mut client = fixture.attach()?;
    within_100ms("Inspect while cancel waits", || Ok(client.inspect()?))?;
    // Past the removed turn and append bounds.
    thread::sleep(Duration::from_millis(5_200).saturating_sub(held_at.elapsed()));
    assert!(!mutation.is_finished(), "held append finished early");
    std::fs::remove_file(&hold)?;
    mutation.join().map_err(|_| "mutation thread")??;
    cancel.join().map_err(|_| "cancel thread")??;
    let event = finish(&fixture, operation, Duration::from_secs(60))?;
    assert_eq!(event.kind, Some(5), "{event:?}");
    let (bytes, replay) = fixture.journal()?;
    let op = &replay.operations[&operation];
    let start = op.accepted_frame.start;
    let ours: Vec<u16> = frames(&bytes[start..])?
        .into_iter()
        .filter_map(|(kind, record)| match &record {
            journal::Record::CancelRequested(p) if p.operation == operation => Some(kind),
            journal::Record::ToolCall(p) if p.operation == operation => Some(kind),
            journal::Record::ToolResult(p) if p.operation == operation => Some(kind),
            journal::Record::TurnTerminal(p) if p.operation == operation => Some(kind),
            _ => None,
        })
        .collect();
    assert_eq!(ours, vec![7, 14, 8], "kind 7, kind 14 cancelled, terminal");
    let recorded = calls(&fixture, operation)?;
    assert_eq!(recorded.len(), 1);
    let record = &recorded[0].call;
    assert_eq!(record.decision, ToolDecision::Cancelled);
    assert_eq!((record.fault, record.name.as_str()), (None, "project"));
    assert_eq!(arguments(record), json!({"command": "read_file"}));
    assert!(recorded[0].result.is_none());
    assert_eq!(event.tools[0].status, Some(6));
    sweep(&fixture.home, &[])?;
    println!(
        "PASS TCR-I06 held append: controls within 100 ms, no time limit, cancelled kind 14 after the append completed"
    );
    println!(
        "PASS TCR-I07 cancel before the decision committed: kind 7, kind 14 cancelled, terminal"
    );
    Ok(())
}

/// TCR-I09 (service part): the Ollama adapter forwards an unregistered name.
fn unregistered_name() -> Result<()> {
    let (fixture, model, project, _) = setup("unregistered")?;
    model.script(&[call("delete_everything", json!({"path": "/"})), Step::Final]);
    let operation = submit(&fixture, project, "I09")?;
    let event = finish(&fixture, operation, Duration::from_secs(60))?;
    assert_eq!(event.kind, Some(4), "the helper ends the turn: {event:?}");
    let recorded = calls(&fixture, operation)?;
    assert_eq!(recorded.len(), 1);
    let record = &recorded[0].call;
    assert_eq!(record.decision, ToolDecision::UnknownTool);
    assert_eq!(record.fault, Some(ToolFault::UnregisteredName));
    assert_eq!(record.source, journal::ToolCallSource::ProviderJson);
    assert_eq!(record.name, "delete_everything");
    assert_eq!(record.tool_operation, None);
    assert!(recorded[0].result.is_none());
    assert_eq!(event.tools[0].status, Some(3));
    assert_eq!(
        model.chat_requests(),
        1,
        "no further model request after the fault"
    );
    sweep(&fixture.home, &[])?;
    println!(
        "PASS TCR-I09 service part: unknown_tool/unregistered_name from provider_json; status 3; the turn failed"
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
    if let [_, flag, job, command] = args.as_slice()
        && flag == "--asura-exec-cmd-guardian"
    {
        std::process::exit(asura_platform::exec_cmd::run_guardian(job, command));
    }
    if args.get(1).map(String::as_str) == Some("--tool-call-child") {
        if let Err(error) = child(Path::new(&args[2])) {
            eprintln!("tool call child failed: {error}");
            std::process::exit(1);
        }
        return;
    }
    let selected: Vec<&str> = args[1..]
        .iter()
        .map(String::as_str)
        .filter(|a| !a.starts_with('-'))
        .collect();
    let cases: [Case; 6] = [
        ("malformed", malformed_then_valid),
        ("secret", exec_cmd_secret),
        ("create", create_note_body),
        ("unregistered", unregistered_name),
        ("held", held_append_and_cancel),
        ("restart", restarts),
    ];
    for (name, case) in cases {
        if !selected.is_empty() && !selected.contains(&name) {
            continue;
        }
        if let Err(error) = case() {
            eprintln!("tool call flow case {name} FAILED: {error}");
            std::process::exit(1);
        }
    }
    println!("tool call flow qualification passed");
}
