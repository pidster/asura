//! LC-U10 (head rules) and LC-I4 (the relay against a fake upstream).
use super::*;
use std::net::TcpListener;
use std::sync::mpsc;

const TOKEN: [u8; 32] = [7; 32];
fn bearer(token: &[u8]) -> String {
    format!(
        "Bearer {}",
        token.iter().map(|b| format!("{b:02x}")).collect::<String>()
    )
}
fn head(method: &str, target: &str, extra: &str) -> Vec<u8> {
    format!(
        "{method} {target} HTTP/1.1\r\nHost: 127.0.0.1:1\r\nAuthorization: {}\r\n{extra}\r\n",
        bearer(&TOKEN)
    )
    .into_bytes()
}

#[test]
fn heads_are_checked_and_rewritten() {
    let Check::Accept(request) = check(
        &[
            head(
                "POST",
                "/v1/chat/completions",
                "Content-Type: application/json\r\nContent-Length: 2\r\nConnection: keep-alive\r\nProxy-Authorization: x\r\n",
            ),
            b"{}".to_vec(),
        ]
        .concat(),
        &TOKEN,
        8934,
    ) else {
        panic!("a valid chat request is accepted");
    };
    assert_eq!(
        request.head,
        "POST /v1/chat/completions HTTP/1.1\r\nHost: 127.0.0.1:8934\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n"
    );
    assert!(!request.head.to_ascii_lowercase().contains("authorization"));
    assert_eq!(request.body, b"{}");
    assert!(matches!(
        check(
            &head("GET", "/props?model=ggml-org%2Fgemma%3AQ8_0", ""),
            &TOKEN,
            1
        ),
        Check::Accept(_)
    ));
    assert!(matches!(
        check(&head("GET", "/v1/models", ""), &TOKEN, 1),
        Check::Accept(_)
    ));
    assert!(matches!(
        check(&head("GET", "/health", ""), &TOKEN, 1),
        Check::Accept(_)
    ));
    for (bytes, status) in [
        (head("GET", "/slots", ""), 403),
        (head("POST", "/props", "Content-Length: 0\r\n"), 403),
        (head("GET", "/props?model=a/b", ""), 403),
        (head("GET", "/props?model=%zz", ""), 403),
        (head("GET", "/props?model=", ""), 403),
        (head("DELETE", "/v1/models", ""), 403),
        (head("POST", "/v1/chat/completions", ""), 400),
        (
            head(
                "POST",
                "/v1/chat/completions",
                "Transfer-Encoding: chunked\r\n",
            ),
            400,
        ),
        (
            head(
                "POST",
                "/v1/chat/completions",
                "Expect: 100-continue\r\nContent-Length: 1\r\n",
            ),
            400,
        ),
        (
            head(
                "POST",
                "/v1/chat/completions",
                "Content-Length: 1\r\nContent-Length: 2\r\n",
            ),
            400,
        ),
        (b"GET /v1/models HTTP/1.0\r\n\r\n".to_vec(), 400),
        (
            b"GET /v1/models HTTP/1.1\r\nAuthorization: Bearer 00\r\n\r\n".to_vec(),
            401,
        ),
        (b"GET /v1/models HTTP/1.1\r\n\r\n".to_vec(), 401),
        (
            format!(
                "GET /v1/models HTTP/1.1\r\nAuthorization: {}\r\n\r\n",
                bearer(&[8; 32])
            )
            .into_bytes(),
            401,
        ),
    ] {
        match check(&bytes, &TOKEN, 1) {
            Check::Refuse(code, _) => {
                assert_eq!(code, status, "{}", String::from_utf8_lossy(&bytes))
            }
            _ => panic!("refused: {}", String::from_utf8_lossy(&bytes)),
        }
    }
    assert!(matches!(
        check(b"GET /v1/models HTTP/1.1\r\n", &TOKEN, 1),
        Check::Incomplete
    ));
    assert!(!bearer_matches(&bearer(&TOKEN).to_uppercase(), &TOKEN));
}

/// A fake router: records each request's head and body and answers with a
/// scripted reply; `None` holds the connection open without answering.
struct Upstream {
    port: u16,
    heads: mpsc::Receiver<String>,
}
fn upstream(replies: Vec<Option<&'static str>>) -> Upstream {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (send, heads) = mpsc::channel();
    std::thread::spawn(move || {
        let mut held = Vec::new();
        for reply in replies {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            let mut request = Vec::new();
            let mut chunk = [0u8; 4096];
            loop {
                let n = stream.read(&mut chunk).unwrap_or(0);
                if n == 0 {
                    break;
                }
                request.extend_from_slice(&chunk[..n]);
                if let Some(end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                    let text = String::from_utf8_lossy(&request[..end]).to_string();
                    let length = text
                        .lines()
                        .find_map(|l| l.strip_prefix("Content-Length: "))
                        .and_then(|v| v.parse::<usize>().ok())
                        .unwrap_or(0);
                    if request.len() >= end + 4 + length {
                        break;
                    }
                }
            }
            let _ = send.send(String::from_utf8_lossy(&request).to_string());
            match reply {
                Some(reply) => {
                    let _ = stream.write_all(reply.as_bytes());
                }
                None => held.push(stream),
            }
        }
        // Held streams stay open until the test ends.
        std::thread::park();
        drop(held);
    });
    Upstream { port, heads }
}

fn exchange(relay: &Relay, request: &[u8]) -> String {
    let mut stream = TcpStream::connect(("127.0.0.1", relay.port)).unwrap();
    stream.write_all(request).unwrap();
    let mut reply = String::new();
    stream.read_to_string(&mut reply).unwrap();
    reply
}
fn request(relay: &Relay, method: &str, target: &str, body: &str) -> Vec<u8> {
    format!(
        "{method} {target} HTTP/1.1\r\nHost: {}\r\nAuthorization: {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        relay.url().trim_start_matches("http://"),
        bearer(&relay.token()),
        body.len()
    )
    .into_bytes()
}

const OK: &str = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\ndata: {\"x\":1}\n\ndata: [DONE]\n\n";

#[test]
fn the_relay_adds_the_key_and_passes_the_reply_through() {
    let fake = upstream(vec![Some(OK), Some(OK)]);
    let relay = Relay::start(fake.port).unwrap();
    relay.provide_key(Zeroizing::new(b"k".repeat(64)));
    let reply = exchange(
        &relay,
        &request(&relay, "POST", "/v1/chat/completions", "{\"a\":1}"),
    );
    assert_eq!(reply, OK, "the reply is unchanged");
    let seen = fake.heads.recv().unwrap();
    assert!(
        seen.starts_with("POST /v1/chat/completions HTTP/1.1\r\n"),
        "{seen}"
    );
    assert!(seen.contains(&format!("Authorization: Bearer {}\r\n", "k".repeat(64))));
    assert!(
        !seen.contains(&bearer(&relay.token())),
        "the token never reaches the router"
    );
    assert!(seen.contains("Connection: close\r\n") && seen.ends_with("\r\n\r\n{\"a\":1}"));
    // A second request on a new connection.
    assert_eq!(
        exchange(&relay, &request(&relay, "GET", "/v1/models", "")),
        OK
    );
    assert!(!relay.refused_a_request());
    relay.join();
}

#[test]
fn the_relay_refuses_wrong_tokens_and_paths_without_contacting_the_router() {
    let fake = upstream(vec![]);
    let relay = Relay::start(fake.port).unwrap();
    relay.provide_key(Zeroizing::new(b"k".repeat(64)));
    let wrong = String::from_utf8(request(&relay, "GET", "/v1/models", ""))
        .unwrap()
        .replace(&bearer(&relay.token()), &bearer(&[1; 32]));
    assert!(exchange(&relay, wrong.as_bytes()).starts_with("HTTP/1.1 401 "));
    assert!(exchange(&relay, &request(&relay, "GET", "/slots", "")).starts_with("HTTP/1.1 403 "));
    let oversized = [
        b"GET /v1/models HTTP/1.1\r\nX: ".as_slice(),
        &[b'a'; MAX_HEAD_BYTES + 8],
    ]
    .concat();
    assert!(exchange(&relay, &oversized).starts_with("HTTP/1.1 400 "));
    assert!(fake.heads.try_recv().is_err(), "nothing reached the router");
    relay.join();
}

#[test]
fn a_request_waits_for_the_key_or_its_refusal() {
    let fake = upstream(vec![Some(OK)]);
    let relay = Relay::start(fake.port).unwrap();
    let mut waiting = TcpStream::connect(("127.0.0.1", relay.port)).unwrap();
    waiting
        .write_all(&request(&relay, "GET", "/v1/models", ""))
        .unwrap();
    // A refused request on another connection passes through the same
    // loop; nothing was forwarded while the key is pending.
    assert!(exchange(&relay, &request(&relay, "GET", "/slots", "")).starts_with("HTTP/1.1 403 "));
    assert!(fake.heads.try_recv().is_err());
    relay.provide_key(Zeroizing::new(b"z".repeat(64)));
    let mut reply = String::new();
    waiting.read_to_string(&mut reply).unwrap();
    assert_eq!(reply, OK);
    assert!(fake.heads.recv().unwrap().contains(&"z".repeat(64)));
    relay.join();

    let refused = Relay::start(fake.port).unwrap();
    let mut waiting = TcpStream::connect(("127.0.0.1", refused.port)).unwrap();
    waiting
        .write_all(&request(&refused, "GET", "/v1/models", ""))
        .unwrap();
    refused.refuse_key();
    let mut reply = String::new();
    waiting.read_to_string(&mut reply).unwrap();
    assert!(reply.starts_with("HTTP/1.1 503 ") && reply.contains("asura_key_unavailable"));
    assert!(refused.refused_a_request());
    refused.join();
}

#[test]
fn the_relay_bounds_connections_and_reports_a_missing_router() {
    // Nothing listens on the upstream port: 502.
    let free = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = free.local_addr().unwrap().port();
    drop(free);
    let relay = Relay::start(port).unwrap();
    relay.provide_key(Zeroizing::new(b"k".repeat(64)));
    assert!(
        exchange(&relay, &request(&relay, "GET", "/v1/models", "")).starts_with("HTTP/1.1 502 ")
    );
    relay.join();

    // A stalled router holds four connections; the fifth is refused, and
    // stop still ends the relay at once.
    let fake = upstream(vec![None, None, None, None]);
    let relay = Relay::start(fake.port).unwrap();
    relay.provide_key(Zeroizing::new(b"k".repeat(64)));
    let mut held = Vec::new();
    for _ in 0..MAX_CONNECTIONS {
        let mut stream = TcpStream::connect(("127.0.0.1", relay.port)).unwrap();
        stream
            .write_all(&request(&relay, "GET", "/v1/models", ""))
            .unwrap();
        fake.heads.recv().unwrap();
        held.push(stream);
    }
    // The fifth connection is answered at once, before it sends anything.
    let mut fifth = TcpStream::connect(("127.0.0.1", relay.port)).unwrap();
    let mut busy = String::new();
    fifth.read_to_string(&mut busy).unwrap();
    assert!(
        busy.starts_with("HTTP/1.1 503 ") && busy.contains("asura_relay_busy"),
        "{busy}"
    );
    relay.join();
    for mut stream in held {
        let mut rest = Vec::new();
        // The stop closed every connection.
        assert!(stream.read_to_end(&mut rest).is_ok_and(|n| n == 0) || rest.is_empty());
    }
}

/// LC-U11: each mode's route and hold reason at turn preparation.
#[test]
fn modes_resolve_to_routes_or_hold_reasons() {
    use asura_storage::config::{LlamacppServer, LlamacppSettings};
    let settings = |server, endpoint: Option<&str>, key: Option<&str>| LlamacppSettings {
        server,
        endpoint: endpoint.map(str::to_owned),
        key_secret: key.map(str::to_owned),
        ..LlamacppSettings::default()
    };
    let reason = |s: LlamacppSettings| route(&s, None).err().unwrap();
    assert_eq!(
        reason(settings(LlamacppServer::Off, None, None)),
        "llamacpp_server_off"
    );
    assert_eq!(
        reason(settings(LlamacppServer::Url, None, None)),
        "llamacpp_endpoint_unset"
    );
    assert_eq!(
        reason(settings(
            LlamacppServer::Url,
            Some("https://models.example"),
            None
        )),
        "remote_endpoint_restricted"
    );
    // A key needs the secret store; without it the input is held.
    assert_eq!(
        reason(settings(LlamacppServer::Asura, None, None)),
        "llamacpp_key_unavailable"
    );
    assert_eq!(
        reason(settings(
            LlamacppServer::Url,
            Some("http://127.0.0.1:8933"),
            Some("WISP")
        )),
        "llamacpp_key_unavailable"
    );
    // The relay speaks HTTP to 127.0.0.1 only.
    assert_eq!(
        reason(settings(
            LlamacppServer::Url,
            Some("https://127.0.0.1:8933"),
            Some("WISP")
        )),
        "llamacpp_endpoint_unsupported"
    );
    let app = route(&settings(LlamacppServer::LlamaApp, None, None), None).unwrap();
    assert_eq!(app.endpoint, "http://127.0.0.1:9931");
    assert!(app.relay.is_none());
    let url = route(
        &settings(LlamacppServer::Url, Some("http://[::1]:8080"), None),
        None,
    )
    .unwrap();
    assert_eq!(url.endpoint, "http://[::1]:8080");
    // Every route is loopback; tools follow the destination rule in
    // `model_owner` for every mode (LCQ1).
    assert!(url.relay.is_none());
}

#[test]
fn only_visible_ascii_keys_reach_a_header() {
    assert!(header_value(b"0123abcdef"));
    assert!(header_value(&[b'k'; 4096]));
    for bad in [
        &b""[..],
        b"a b",
        b"a\r\nX-Injected: 1",
        b"\x7f",
        &[b'k'; 4097],
    ] {
        assert!(!header_value(bad), "{bad:?}");
    }
}

/// LC-I5, LC-I6 and LC-I7, native: the server plan through the test-only launch
/// path with the real `llama-server`, its key from a pipe, in a scratch
/// folder; then a chat request through the relay. Opt-in:
///   ASURA_TEST_LLAMACPP_SERVER=/opt/homebrew/bin/llama-server
///   ASURA_TEST_LLAMACPP_GGUF=<a small GGUF file>
/// It never registers a LaunchAgent and never uses `~/.asura`.
#[test]
fn native_server_plan_and_relay() {
    let (Some(binary), Some(gguf)) = (
        std::env::var_os("ASURA_TEST_LLAMACPP_SERVER"),
        std::env::var_os("ASURA_TEST_LLAMACPP_GGUF"),
    ) else {
        return;
    };
    let gguf = std::path::PathBuf::from(gguf);
    let name = gguf
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap()
        .to_owned();
    let root = std::path::PathBuf::from(format!(
        "/private/tmp/asura-llama-native-{:x}",
        u128::from_ne_bytes(asura_platform::random_id())
    ));
    let models = root.join("models");
    std::fs::create_dir_all(&models).unwrap();
    std::os::unix::fs::symlink(&gguf, models.join(format!("{name}.gguf"))).unwrap();
    let free = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = free.local_addr().unwrap().port();
    drop(free);
    let plan = asura_platform::llamacpp::ServerPlan {
        binary: binary.into(),
        models,
        port,
        context_length: 8192,
        slots: 1,
        models_loaded: 1,
        idle_seconds: 60,
    };
    let key = asura_platform::llamacpp::new_key();
    let log = std::fs::File::create(root.join("server.log")).unwrap();
    let mut child = asura_platform::llamacpp::spawn_for_test(&plan, &key, &log).unwrap();
    /// Stops the router's group if an assertion fails first, so no scratch
    /// router or model process outlives a failed run.
    struct StopGroup(Option<i32>);
    impl Drop for StopGroup {
        fn drop(&mut self) {
            if let Some(group) = self.0 {
                // SAFETY: signals the group made by spawn_for_test.
                unsafe { libc::kill(-group, libc::SIGTERM) };
            }
        }
    }
    let mut on_failure = StopGroup(Some(child.id() as i32));
    // Ready when /health answers; the child's exit ends the wait. 100 ms
    // between attempts is pacing.
    let cancel = AtomicBool::new(false);
    while health(port, &cancel) != Some(true) {
        assert!(
            child.try_wait().unwrap().is_none(),
            "llama-server exited; see {}",
            root.display()
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    let direct = |request: &str| {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        stream.write_all(request.as_bytes()).unwrap();
        let mut reply = String::new();
        stream.read_to_string(&mut reply).unwrap();
        reply
    };
    // LC-I5: the router needs the key from the pipe.
    let unauthorized = direct(&format!(
        "GET /v1/models HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"
    ));
    assert!(unauthorized.starts_with("HTTP/1.1 401"), "{unauthorized}");
    let args = std::process::Command::new("/bin/ps")
        .args(["-o", "command=", "-p", &child.id().to_string()])
        .output()
        .unwrap();
    let args = String::from_utf8_lossy(&args.stdout);
    assert!(
        args.contains("--api-key-file /dev/fd/")
            && !args.contains(std::str::from_utf8(&key).unwrap()),
        "{args}"
    );
    // LC-I6: through the relay with the token; the relay adds the key.
    let relay = Relay::start(port).unwrap();
    relay.provide_key(Zeroizing::new(key.to_vec()));
    let listed = exchange(&relay, &request(&relay, "GET", "/v1/models", ""));
    assert!(
        listed.starts_with("HTTP/1.1 200") && listed.contains(&name),
        "{listed}"
    );
    let body = format!(
        r#"{{"model":"{name}","stream":true,"stream_options":{{"include_usage":true}},"chat_template_kwargs":{{"enable_thinking":false}},"messages":[{{"role":"user","content":"Say hi."}}]}}"#
    );
    let chat = exchange(
        &relay,
        &request(&relay, "POST", "/v1/chat/completions", &body),
    );
    assert!(
        chat.starts_with("HTTP/1.1 200") && chat.contains("data: [DONE]"),
        "{chat}"
    );
    relay.join();
    // LC-I7 (LCD16): the chat loaded the model; its process inherits the
    // CORS arguments, not the key, and no browser origin may read a reply
    // from the router or from it.
    let model_pid = String::from_utf8(
        std::process::Command::new("/usr/bin/pgrep")
            .args(["-P", &child.id().to_string()])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();
    let model_pid = model_pid.trim();
    assert!(
        !model_pid.is_empty() && !model_pid.contains('\n'),
        "one model process: {model_pid:?}"
    );
    let model_args = String::from_utf8(
        std::process::Command::new("/bin/ps")
            .args(["-o", "command=", "-p", model_pid])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();
    assert!(
        model_args.contains(&asura_platform::llamacpp::CORS_ARGUMENTS[..2].join(" "))
            && model_args.contains("--no-cors-credentials")
            && model_args.contains("--cors-methods GET")
            && model_args.contains("--cors-headers Content-Type")
            && !model_args.contains("api-key")
            && !model_args.contains(std::str::from_utf8(&key).unwrap()),
        "{model_args}"
    );
    let listening = String::from_utf8(
        std::process::Command::new("/usr/sbin/lsof")
            .args(["-nP", "-a", "-p", model_pid, "-iTCP", "-sTCP:LISTEN", "-Fn"])
            .output()
            .unwrap()
            .stdout,
    )
    .unwrap();
    let model_port: u16 = listening
        .lines()
        .find_map(|line| line.strip_prefix("n127.0.0.1:"))
        .and_then(|port| port.parse().ok())
        .unwrap_or_else(|| panic!("model process port: {listening}"));
    let ask = |port: u16, request: String| {
        let mut stream = TcpStream::connect(("127.0.0.1", port)).unwrap();
        stream.write_all(request.as_bytes()).unwrap();
        let mut reply = String::new();
        stream.read_to_string(&mut reply).unwrap();
        reply
    };
    for port in [port, model_port] {
        let preflight = ask(
            port,
            format!(
                "OPTIONS /v1/chat/completions HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nOrigin: http://evil.example\r\nAccess-Control-Request-Method: POST\r\nAccess-Control-Request-Headers: content-type\r\nConnection: close\r\n\r\n"
            ),
        );
        let simple = ask(
            port,
            format!(
                "GET /v1/models HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nOrigin: http://evil.example\r\nConnection: close\r\n\r\n"
            ),
        );
        for reply in [&preflight, &simple] {
            let head = reply.split("\r\n\r\n").next().unwrap().to_ascii_lowercase();
            assert!(
                !head.contains("access-control-allow-origin: http://evil.example")
                    && !head.contains("access-control-allow-origin: *")
                    && !head.contains("access-control-allow-credentials: true"),
                "port {port}: {head}"
            );
        }
        if port == model_port {
            // The LCQ3 gap (LT3, LT4): a model process has no key and answers.
            // A llama.cpp change that closes it fails here, so the threat
            // model is revised with it.
            assert!(simple.starts_with("HTTP/1.1 200"), "{simple}");
        } else {
            assert!(simple.starts_with("HTTP/1.1 401"), "{simple}");
        }
    }
    // SIGTERM stops the router and the model process it started.
    on_failure.0 = None;
    // SAFETY: signals the child's own process group, made by spawn_for_test.
    unsafe { libc::kill(-(child.id() as i32), libc::SIGTERM) };
    child.wait().unwrap();
    let left = std::process::Command::new("/usr/bin/pgrep")
        .args(["-f", &root.display().to_string()])
        .output()
        .unwrap();
    assert!(
        left.stdout.is_empty(),
        "a model process outlived the router"
    );
    std::fs::remove_dir_all(&root).unwrap();
}
