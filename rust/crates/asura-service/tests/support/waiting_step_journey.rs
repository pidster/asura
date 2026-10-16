//! WS-01..04 integration: the real service and Swift helper against a private
//! loopback provider that holds `/api/show` (Hello) or `/api/chat` (output)
//! until the test releases it. The waiting step is visible, controls stay
//! responsive, and a hold or cancel ends each stalled wait.
use super::*;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

/// Which request the server holds open without replying.
const HOLD_SHOW: usize = 1;
const HOLD_CHAT: usize = 2;

struct Server {
    endpoint: String,
    stop: Arc<AtomicBool>,
    hold: Arc<AtomicUsize>,
    /// Held requests whose peer closed its connection (helper stopped).
    released: Arc<AtomicUsize>,
    thread: Option<thread::JoinHandle<()>>,
}
impl Server {
    fn start() -> Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        listener.set_nonblocking(true)?;
        let endpoint = format!("http://{}", listener.local_addr()?);
        let stop = Arc::new(AtomicBool::new(false));
        let hold = Arc::new(AtomicUsize::new(0));
        let released = Arc::new(AtomicUsize::new(0));
        let (token, mode, closed) = (stop.clone(), hold.clone(), released.clone());
        let task = thread::spawn(move || {
            // Test fixture bounds: at most 8 connections, 64 KiB requests,
            // a 120 s server lifetime and 50 ms stop checks.
            let end = Instant::now() + Duration::from_secs(120);
            let mut workers = Vec::new();
            while !token.load(Ordering::Acquire) && Instant::now() < end && workers.len() < 8 {
                match listener.accept() {
                    Ok((socket, _)) => {
                        let (token, mode, closed) = (token.clone(), mode.clone(), closed.clone());
                        workers.push(thread::spawn(move || {
                            let _ = serve(socket, &token, &mode, &closed, end);
                        }));
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(_) => break,
                }
            }
            for worker in workers {
                let _ = worker.join();
            }
        });
        Ok(Self {
            endpoint,
            stop,
            hold,
            released,
            thread: Some(task),
        })
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(task) = self.thread.take() {
            let _ = task.join();
        }
    }
}
fn serve(
    mut socket: TcpStream,
    stop: &AtomicBool,
    hold: &AtomicUsize,
    released: &AtomicUsize,
    end: Instant,
) -> std::io::Result<()> {
    socket.set_nonblocking(false)?;
    socket.set_read_timeout(Some(Duration::from_secs(2)))?;
    socket.set_write_timeout(Some(Duration::from_secs(2)))?;
    let mut request = Vec::new();
    loop {
        let mut bytes = [0; 4096];
        let count = socket.read(&mut bytes)?;
        if count == 0 {
            return Ok(());
        }
        request.extend_from_slice(&bytes[..count]);
        if request.len() > 65536 {
            return Err(std::io::ErrorKind::InvalidData.into());
        }
        if let Some(header_end) = request.windows(4).position(|part| part == b"\r\n\r\n") {
            let header = String::from_utf8_lossy(&request[..header_end]);
            let length = header
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .and_then(|value| value.trim().parse::<usize>().ok())
                })
                .unwrap_or(0);
            if request.len() >= header_end + 4 + length {
                break;
            }
        }
    }
    let show = request.starts_with(b"POST /api/show ");
    let chat = request.starts_with(b"POST /api/chat ");
    let held = (show && hold.load(Ordering::Acquire) == HOLD_SHOW)
        || (chat && hold.load(Ordering::Acquire) == HOLD_CHAT);
    if held {
        // Hold without replying until the helper closes the connection or
        // the test stops the server.
        socket.set_read_timeout(Some(Duration::from_millis(50)))?;
        let mut byte = [0; 1];
        while !stop.load(Ordering::Acquire) && Instant::now() < end {
            match socket.read(&mut byte) {
                Ok(0) => {
                    released.fetch_add(1, Ordering::AcqRel);
                    return Ok(());
                }
                Ok(_) => {}
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) => {}
                Err(_) => {
                    released.fetch_add(1, Ordering::AcqRel);
                    return Ok(());
                }
            }
        }
        return Ok(());
    }
    if show {
        let body =
            r#"{"capabilities":["completion"],"model_info":{"fixture.context_length":8192}}"#;
        write!(
            socket,
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        )?;
    } else if chat {
        socket.write_all(
            b"HTTP/1.1 200 OK\r\nContent-Type: application/x-ndjson\r\nConnection: close\r\n\r\n",
        )?;
        socket.write_all(b"{\"message\":{\"role\":\"assistant\",\"content\":\"COMPLETE\"},\"done\":true,\"done_reason\":\"stop\",\"prompt_eval_count\":1,\"eval_count\":1}\n")?;
    }
    Ok(())
}

fn new_input(project: [u8; 16], prompt: &str) -> pb::ConversationQueueSubmit {
    pb::ConversationQueueSubmit {
        request_id: Some(asura_platform::random_id().to_vec()),
        project_id: Some(project.to_vec()),
        conversation_id: None,
        expected_generation: Some(0),
        new_conversation: Some(true),
        prompt: Some(prompt.into()),
    }
}
/// Polls the exact entry until `accept` holds; a test-only harness budget.
fn entry_until(
    fixture: &Fixture,
    project: [u8; 16],
    input: [u8; 16],
    label: &str,
    accept: impl Fn(&pb::ConversationQueueEntry) -> bool,
) -> Result<pb::ConversationQueueEntry> {
    let end = Instant::now() + Duration::from_secs(60);
    loop {
        let entry = queue_entry(fixture, project, input)?;
        if accept(&entry) {
            return Ok(entry);
        }
        if Instant::now() >= end {
            return Err(format!("{label}: last entry {entry:?}").into());
        }
        thread::sleep(Duration::from_millis(20));
    }
}

pub(super) fn run() -> Result<()> {
    let server = Server::start()?;
    let mut fixture = Fixture::new()?;
    fixture.start()?;
    wait_ready(&fixture, None)?;
    let directory = fixture.home.join("project");
    fs::create_dir(&directory)?;
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))?;
    let project = checked_id(
        fixture
            .attach()?
            .register_project(
                pb::ProjectRegister {
                    request_id: Some(asura_platform::random_id().to_vec()),
                    location: Some(directory.to_str().ok_or("fixture path")?.into()),
                },
                &|| false,
            )?
            .project_id,
    )?;
    for (key, value) in [
        ("providers.ollama.endpoint", server.endpoint.as_str()),
        ("model", "ollama:fixture"),
    ] {
        assert!(
            fixture
                .attach()?
                .config(key, Some(value), &|| false)?
                .error
                .is_none()
        );
    }

    // WS-01: Hello is held, so the queued input waits on STARTING_MODEL.
    server.hold.store(HOLD_SHOW, Ordering::Release);
    let first = managed_submit_call(&fixture, new_input(project, "Stall at Hello"))?;
    let first_id = checked_id(first.accepted_input_id)?;
    let starting = entry_until(&fixture, project, first_id, "starting step", |entry| {
        entry.waiting_step == Some(pb::ConversationWaitStep::StartingModel as i32)
    })?;
    assert_eq!(starting.state, Some(1));
    assert_eq!(starting.operation_id, None);
    // Controls stay responsive while the helper is silent.
    for _ in 0..3 {
        let mut client = fixture.attach()?;
        within_100ms("Inspect during a held Hello", || client.inspect())??;
    }
    // A watcher at the current revision gets the full projection with the
    // live step at the pacing interval, not the empty pending stub.
    let mut watcher = fixture.attach()?;
    let current = watcher.observe_queue(project, None)?;
    let paced = watcher.observe_queue(project, current.revision)?;
    assert_ne!(
        paced.pending,
        Some(true),
        "paced reply must be a projection"
    );
    assert!(paced.entries.iter().any(|entry| {
        entry.input_id.as_deref() == Some(first_id.as_slice())
            && entry.waiting_step == Some(pb::ConversationWaitStep::StartingModel as i32)
    }));

    // WS-03: Hold ends the stalled Hello; nothing is accepted.
    let mut decider = fixture.attach()?;
    let mut decide = |input: [u8; 16]| {
        decider.decide_conversation_input(
            pb::ConversationQueueDecision {
                request_id: Some(asura_platform::random_id().to_vec()),
                input_id: Some(input.to_vec()),
                action: Some(4),
                ..Default::default()
            },
            &|| false,
        )
    };
    let wrong = decide(asura_platform::random_id());
    assert!(matches!(
        wrong,
        Err(asura_client::Error::Remote(_, ref message)) if message == "input_not_preparing"
    ));
    let held = decide(first_id)?;
    let entry = held
        .entries
        .iter()
        .find(|entry| entry.input_id.as_deref() == Some(first_id.as_slice()))
        .ok_or("held input absent from reply")?;
    assert_eq!(entry.state, Some(2));
    assert_eq!(entry.hold_reason.as_deref(), Some("cancelled"));
    assert_eq!(entry.waiting_step, None);
    // A second hold of the same input is refused: its preparation is ending.
    assert!(matches!(
        decide(first_id),
        Err(asura_client::Error::Remote(_, ref message)) if message == "input_not_preparing"
    ));
    // WS-03 race: Resume at once, while the held helper may still be winding
    // down. The input must run, not be held a second time. Output is held.
    server.hold.store(HOLD_CHAT, Ordering::Release);
    fixture.attach()?.decide_conversation_input(
        pb::ConversationQueueDecision {
            request_id: Some(asura_platform::random_id().to_vec()),
            input_id: Some(first_id.to_vec()),
            action: Some(1),
            ..Default::default()
        },
        &|| false,
    )?;
    let running = entry_until(
        &fixture,
        project,
        first_id,
        "running after resume",
        |entry| entry.state == Some(3),
    )?;
    let end = Instant::now() + Duration::from_secs(10);
    while server.released.load(Ordering::Acquire) == 0 {
        assert!(Instant::now() < end, "helper did not stop after the hold");
        thread::sleep(Duration::from_millis(20));
    }
    let operation = checked_id(running.operation_id)?;
    let generation = running.generation.ok_or("missing generation")?;
    let end = Instant::now() + Duration::from_secs(60);
    let mut client = fixture.attach()?;
    loop {
        let event = client.observe_conversation(operation, 0)?;
        if event.waiting_step == Some(pb::ConversationWaitStep::ModelOutput as i32) {
            assert_eq!(event.kind, Some(1));
            break;
        }
        assert!(
            !matches!(event.kind, Some(3..=6)),
            "turn ended early: {event:?}"
        );
        assert!(Instant::now() < end, "no MODEL_OUTPUT step: {event:?}");
        thread::sleep(Duration::from_millis(20));
    }
    let mut client = fixture.attach()?;
    within_100ms("Inspect during held output", || client.inspect())??;
    fixture
        .attach()?
        .cancel_conversation(operation, generation, &|| false)?;
    let terminal = observe(&fixture, operation)?;
    assert_eq!(
        terminal.kind,
        Some(5),
        "cancel must end the wait: {terminal:?}"
    );
    assert_eq!(terminal.waiting_step, None);
    fixture.stop()?;
    println!(
        "PASS WS-01..04 integration: STARTING_MODEL and MODEL_OUTPUT visible, paced full \
         projection, hold ends the stalled Hello, immediate Resume runs once, cancel ends \
         the stalled output"
    );
    Ok(())
}
