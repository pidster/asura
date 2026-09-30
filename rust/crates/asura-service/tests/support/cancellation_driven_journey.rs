//! Real service/helper lifetime against a slow, bounded loopback provider.
use super::*;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

struct Server {
    endpoint: String,
    stop: Arc<AtomicBool>,
    thread: Option<thread::JoinHandle<std::io::Result<()>>>,
}
impl Server {
    fn start() -> Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        listener.set_nonblocking(true)?;
        let endpoint = format!("http://{}", listener.local_addr()?);
        let stop = Arc::new(AtomicBool::new(false));
        let token = stop.clone();
        let task = thread::spawn(move || {
            let end = Instant::now() + Duration::from_secs(120);
            let mut connections = 0;
            while !token.load(Ordering::Acquire) && Instant::now() < end && connections < 4 {
                let (mut socket, _) = match listener.accept() {
                    Ok(value) => value,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10));
                        continue;
                    }
                    Err(error) => return Err(error),
                };
                connections += 1;
                socket.set_read_timeout(Some(Duration::from_secs(2)))?;
                socket.set_write_timeout(Some(Duration::from_secs(2)))?;
                let mut request = Vec::new();
                loop {
                    let mut bytes = [0; 4096];
                    let count = socket.read(&mut bytes)?;
                    if count == 0 {
                        return Err(std::io::ErrorKind::UnexpectedEof.into());
                    }
                    request.extend_from_slice(&bytes[..count]);
                    if request.len() > 65536 {
                        return Err(std::io::ErrorKind::InvalidData.into());
                    }
                    if let Some(header_end) =
                        request.windows(4).position(|part| part == b"\r\n\r\n")
                    {
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
                if request.starts_with(b"POST /api/show ") {
                    let body = r#"{"capabilities":["completion"],"model_info":{"fixture.context_length":8192}}"#;
                    write!(
                        socket,
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    )?;
                } else if request.starts_with(b"POST /api/chat ") {
                    socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/x-ndjson\r\nConnection: close\r\n\r\n")?;
                    for _ in 0..16 {
                        socket.write_all(b"{\"message\":{\"role\":\"assistant\",\"content\":\"progress \"},\"done\":false}\n")?;
                        let next = Instant::now() + Duration::from_secs(4);
                        while Instant::now() < next {
                            if Instant::now() >= end {
                                return Err(std::io::ErrorKind::TimedOut.into());
                            }
                            if token.load(Ordering::Acquire) {
                                return Ok(());
                            }
                            thread::sleep(Duration::from_millis(100));
                        }
                    }
                    socket.write_all(b"{\"message\":{\"role\":\"assistant\",\"content\":\"COMPLETE\"},\"done\":true,\"done_reason\":\"stop\",\"prompt_eval_count\":1,\"eval_count\":17}\n")?;
                    return Ok(());
                } else {
                    return Err(std::io::ErrorKind::InvalidData.into());
                }
            }
            Ok(())
        });
        Ok(Self {
            endpoint,
            stop,
            thread: Some(task),
        })
    }
}
impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(task) = self.thread.take() {
            if let Ok(Err(error)) = task.join() {
                eprintln!("loopback fixture server error: {:?}", error.kind());
            }
        }
    }
}

pub(super) fn run() -> Result<()> {
    let server = Server::start()?;
    let mut fixture = Fixture::new()?;
    fixture.start()?;
    wait_ready(&fixture, None)?;
    let directory = fixture.home.join("project");
    fs::create_dir(&directory)?;
    let project = checked_id(
        fixture
            .attach()?
            .register_project(pb::ProjectRegister {
                request_id: Some(asura_platform::random_id().to_vec()),
                location: Some(directory.to_str().ok_or("fixture path")?.into()),
            })?
            .project_id,
    )?;
    for (key, value) in [
        ("providers.ollama.endpoint", server.endpoint.as_str()),
        ("model", "ollama:fixture"),
    ] {
        assert!(fixture.attach()?.config(key, Some(value))?.error.is_none());
    }
    let started = Instant::now();
    let accepted = match submit(&fixture, project, None, 0, "Reply when ready") {
        Ok(accepted) => accepted,
        Err(error) => {
            fixture.print_model_diagnostics();
            retain_failure_log(&fixture)?;
            return Err(error);
        }
    };
    let generation_started = Instant::now();
    let operation = checked_id(accepted.operation_id)?;
    let end = started + Duration::from_secs(120);
    let mut checked_control = false;
    let mut helper = None;
    loop {
        assert!(Instant::now() < end, "long inference fixture deadline");
        let event = fixture.attach()?.observe_conversation(operation, 0)?;
        if generation_started.elapsed() > Duration::from_secs(61) && !checked_control {
            assert!(
                !matches!(event.kind, Some(3..=6)),
                "inference must still be active"
            );
            helper = Some(helper_witness(&fixture)?);
            let before = Instant::now();
            fixture.attach()?.inspect()?;
            assert!(before.elapsed() < Duration::from_millis(100));
            checked_control = true;
        }
        if matches!(event.kind, Some(3..=6)) {
            if event.kind != Some(3) {
                fixture.print_model_diagnostics();
                retain_failure_log(&fixture)?;
            }
            assert_eq!(event.kind, Some(3), "long inference failed: {event:?}");
            assert!(
                event
                    .text
                    .as_deref()
                    .unwrap_or_default()
                    .ends_with("COMPLETE")
            );
            assert!(generation_started.elapsed() > Duration::from_secs(60));
            assert!(checked_control);
            let context = event
                .model_context
                .as_ref()
                .ok_or("missing model context")?;
            assert_eq!(context.input_tokens, Some(1));
            assert_eq!(context.capacity_tokens, Some(8192));
            assert_eq!(context.basis, Some(1));
            break;
        }
        thread::sleep(Duration::from_millis(100));
    }
    fixture.stop()?;
    assert!(!same_helper(
        &fixture.home,
        &helper.ok_or("missing helper witness")?
    )?);
    assert!(
        !fs::read_dir(fixture.home.join(".asura/run"))?
            .flatten()
            .any(|entry| entry.file_name().to_string_lossy().starts_with("model-")),
        "owned model copies leaked after shutdown"
    );
    println!(
        "PASS real service/helper inference beyond 60 seconds, responsive Inspect and owned cleanup"
    );
    Ok(())
}

fn retain_failure_log(fixture: &Fixture) -> Result<()> {
    let mut log = Vec::new();
    fs::File::open(fixture.home.join("service.log"))?
        .take(65536)
        .read_to_end(&mut log)?;
    fs::write("/private/tmp/asura-longturn-failure.log", log)?;
    Ok(())
}
