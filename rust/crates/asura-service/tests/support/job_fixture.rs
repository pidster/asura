//! The real service with a scripted loopback Ollama endpoint, in a private
//! scratch home, for the J3 and J4 integration cases (job_tool_flow,
//! job_control_flow). The scripted model can name the job of the previous
//! handle delivery: `{job}` in a call's argument strings becomes the last
//! `job: <handle>` line of the tool results it was sent.
#![allow(dead_code)] // each test file uses part of the fixture
use asura_client::Client;
use asura_control::pb;
use asura_platform::RuntimeDirectory;
use asura_storage::authority::conversation as journal;
use serde_json::{Value, json};
use std::io::{Read, Write};
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
pub const BUILD: &str = "asura/conversation-test";

pub struct Fixture {
    pub home: PathBuf,
    child: Option<Child>,
    flag: &'static str,
}
impl Fixture {
    /// A scratch home under `/private/tmp`; `flag` is the test binary's
    /// child-service argument.
    pub fn new(label: &str, flag: &'static str) -> Result<Self> {
        let nonce: String = asura_platform::random_id()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let home = Path::new("/private/tmp").join(format!("asura-job-{label}-{}", &nonce[..12]));
        std::fs::create_dir(&home)?;
        std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o700))?;
        let home = std::fs::canonicalize(home)?;
        Ok(Self {
            home,
            child: None,
            flag,
        })
    }
    pub fn runtime(&self) -> Result<RuntimeDirectory> {
        Ok(RuntimeDirectory::scratch(&self.home, true)?)
    }
    pub fn start(&mut self) -> Result<()> {
        assert!(self.child.is_none());
        let log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.home.join("service.log"))?;
        self.child = Some(
            Command::new(std::env::current_exe()?)
                .arg(self.flag)
                .arg(&self.home)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(log)
                .spawn()?,
        );
        self.attach()?;
        Ok(())
    }
    pub fn attach(&self) -> Result<Client> {
        let runtime = self.runtime()?;
        let end = Instant::now() + Duration::from_secs(10);
        loop {
            // Test-only harness budget: client waits have no time bound in
            // production (owner, 2026-10-16); a stalled fixture fails here.
            let harness = Instant::now() + Duration::from_secs(180);
            let cancel: asura_client::Cancel = Arc::new(move || Instant::now() >= harness);
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
    pub fn stop(&mut self) -> Result<()> {
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
    pub fn journal(&self) -> Result<(Vec<u8>, journal::Replay)> {
        let bytes = asura_storage::authority::fixture::journal_bytes(&self.home)?;
        let replay = journal::replay(&bytes, &AtomicBool::new(false))
            .map_err(|e| format!("replay: {e:?}"))?;
        Ok((bytes, replay))
    }
    pub fn wait_ready(&self) -> Result<()> {
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
    pub fn project(&self, name: &str) -> Result<([u8; 16], PathBuf)> {
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
    pub fn config(&self, key: &str, value: &str) -> Result<pb::ConfigReply> {
        Ok(self.attach()?.config(key, Some(value), &|| false)?)
    }
    pub fn model(&self, endpoint: &str) -> Result<()> {
        for (key, value) in [
            ("providers.ollama.endpoint", endpoint),
            ("model", "ollama:fixture"),
        ] {
            let reply = self.config(key, value)?;
            assert!(reply.error.is_none(), "{key}: {:?}", reply.error);
        }
        Ok(())
    }
    /// The journal records of the home, decoded in order.
    pub fn records(&self) -> Result<Vec<journal::Record>> {
        let (bytes, _) = self.journal()?;
        records(&bytes)
    }
    pub fn submit(&self, project: [u8; 16], prompt: &str) -> Result<[u8; 16]> {
        let accepted =
            super::queue_submit::submit_queued(&|| self.attach(), project, None, 0, prompt)?;
        id(accepted.operation_id)
    }
    /// The next turn of the conversation that `previous` belongs to (job
    /// commands reach only the jobs of their own conversation, TJ-12).
    pub fn follow(&self, project: [u8; 16], previous: [u8; 16], prompt: &str) -> Result<[u8; 16]> {
        let (_, replay) = self.journal()?;
        let op = replay
            .operations
            .get(&previous)
            .ok_or("previous operation")?;
        let accepted = super::queue_submit::submit_queued(
            &|| self.attach(),
            project,
            Some(op.conversation.to_vec()),
            op.generation,
            prompt,
        )?;
        id(accepted.operation_id)
    }
    /// The terminal event of `operation`; harness budget `limit`.
    pub fn finish(&self, operation: [u8; 16], limit: Duration) -> Result<pb::ConversationEvent> {
        let end = Instant::now() + limit;
        loop {
            let event = self.attach()?.observe_conversation(operation, 0, 0)?;
            if matches!(event.kind, Some(3..=6)) {
                return Ok(event);
            }
            if Instant::now() >= end {
                return Err(format!("turn did not end: {event:?}").into());
            }
            thread::sleep(Duration::from_millis(50));
        }
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.stop();
        if let Some(child) = self.child.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
        if thread::panicking() || std::env::var_os("ASURA_JOB_KEEP").is_some() {
            eprintln!("job fixture retained: {}", self.home.display());
        } else {
            let _ = std::fs::remove_dir_all(&self.home);
        }
    }
}
pub fn id(value: Option<Vec<u8>>) -> Result<[u8; 16]> {
    Ok(value
        .ok_or("missing ID")?
        .try_into()
        .map_err(|_| "wrong ID length")?)
}
pub fn hex(id: &[u8]) -> String {
    id.iter().map(|b| format!("{b:02x}")).collect()
}
/// Every record of the journal bytes, in order.
pub fn records(bytes: &[u8]) -> Result<Vec<journal::Record>> {
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
pub fn wait_for(limit: Duration, what: &str, mut check: impl FnMut() -> bool) -> Result<()> {
    let end = Instant::now() + limit;
    while !check() {
        if Instant::now() >= end {
            return Err(format!("timed out waiting for {what}").into());
        }
        thread::sleep(Duration::from_millis(20));
    }
    Ok(())
}
pub fn within_100ms<T>(label: &str, call: impl FnOnce() -> Result<T>) -> Result<T> {
    let started = Instant::now();
    let value = call()?;
    let elapsed = started.elapsed();
    if elapsed >= Duration::from_millis(100) {
        return Err(format!("{label} took {elapsed:?}").into());
    }
    Ok(value)
}
pub fn holds(bytes: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty() && bytes.windows(needle.len()).any(|w| w == needle)
}
/// The raw-byte sweep of a home: journal, payloads, audit, logs and config.
pub fn sweep(root: &Path, needles: &[&[u8]]) -> Result<()> {
    for entry in std::fs::read_dir(root)? {
        let path = entry?.path();
        let meta = std::fs::symlink_metadata(&path)?;
        if meta.is_dir() {
            sweep(&path, needles)?;
        } else if meta.is_file() {
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            // The project's own files and the payload objects are the
            // output's home; the sweep covers every other file.
            if path
                .components()
                .any(|c| c.as_os_str() == "payloads" || c.as_os_str() == "project")
            {
                continue;
            }
            let bytes = std::fs::read(&path)?;
            for needle in needles {
                if holds(&bytes, needle) {
                    return Err(format!("{} holds the swept bytes ({name})", path.display()).into());
                }
            }
        }
    }
    Ok(())
}

// ------------------------------------------------------- scripted model

/// One scripted model reply.
#[derive(Clone)]
pub enum Step {
    /// One tool call; `{job}` in any argument string becomes the handle of
    /// the last handle delivery the model received.
    Call(String, Value),
    /// The final answer.
    Final,
    /// On receipt, create this file (a journal gate file of the service
    /// home), then reply with the inner step. A test-harness hold.
    Create(PathBuf, Box<Step>),
}
pub fn call(name: &str, arguments: Value) -> Step {
    Step::Call(name.into(), arguments)
}
pub struct Model {
    pub endpoint: String,
    stop: Arc<AtomicBool>,
    requests: Arc<Mutex<Vec<Value>>>,
    script: Arc<Mutex<std::collections::VecDeque<Step>>>,
    thread: Option<thread::JoinHandle<()>>,
}
impl Model {
    pub fn start() -> Result<Self> {
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
                let (log, steps) = (log.clone(), steps.clone());
                thread::spawn(move || {
                    let _ = serve(&mut socket, &log, &steps);
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
    pub fn script(&self, steps: &[Step]) {
        let mut script = self.script.lock().unwrap();
        script.clear();
        script.extend(steps.iter().cloned());
        self.requests.lock().unwrap().clear();
    }
    /// Every `tool` message content of the last request, in order.
    pub fn tool_messages(&self) -> Vec<String> {
        let requests = self.requests.lock().unwrap();
        requests
            .last()
            .map(tool_messages)
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
fn tool_messages(request: &Value) -> Vec<String> {
    request["messages"]
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
/// The handle of the last `job: <handle>` line in the request's results.
fn last_handle(request: &Value) -> Option<String> {
    tool_messages(request).iter().rev().find_map(|text| {
        text.lines()
            .find_map(|line| line.strip_prefix("job: "))
            .map(str::to_owned)
    })
}
fn substitute(value: &Value, handle: &str) -> Value {
    match value {
        Value::String(text) => Value::String(text.replace("{job}", handle)),
        Value::Object(members) => Value::Object(
            members
                .iter()
                .map(|(k, v)| (k.clone(), substitute(v, handle)))
                .collect(),
        ),
        other => other.clone(),
    }
}
fn serve(
    socket: &mut std::net::TcpStream,
    log: &Mutex<Vec<Value>>,
    steps: &Mutex<std::collections::VecDeque<Step>>,
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
    if request.starts_with(b"POST /api/show ") {
        let payload = json!({"capabilities": ["completion", "tools"],
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
    let chat: Value = serde_json::from_slice(body).unwrap_or_default();
    let handle = last_handle(&chat).unwrap_or_default();
    log.lock().unwrap().push(chat);
    let mut step = steps.lock().unwrap().pop_front().unwrap_or(Step::Final);
    while let Step::Create(path, then) = step {
        let _ = std::fs::write(path, b"");
        step = *then;
    }
    socket.write_all(
        b"HTTP/1.1 200 OK\r\nContent-Type: application/x-ndjson\r\nConnection: close\r\n\r\n",
    )?;
    let response = match step {
        Step::Call(name, arguments) => json!({
            "message": {"role": "assistant", "content": "", "tool_calls": [
                {"function": {"name": name, "arguments": substitute(&arguments, &handle)}}]},
            "done": true, "done_reason": "stop", "prompt_eval_count": 32, "eval_count": 2}),
        Step::Final | Step::Create(..) => json!({
            "message": {"role": "assistant", "content": "COMPLETE"},
            "done": true, "done_reason": "stop", "prompt_eval_count": 48, "eval_count": 3}),
    };
    socket.write_all(format!("{response}\n").as_bytes())
}

/// The child service of a test binary.
pub fn child(home: &Path) -> Result<()> {
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
