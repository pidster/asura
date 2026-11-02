//! The llama.cpp relay (llamacpp-backend.md, key path to requests): class A
//! service code that takes the model helper's requests with a per-helper
//! token and forwards them to the llama.cpp router with the server key. The
//! helper (class B) never holds the key.
//!
//! One relay serves one conversation helper and lives with its
//! `ModelOwner`. It runs on its own thread, with nonblocking sockets and one
//! `poll`. Each wait ends on a socket event, the key, the key refusal or
//! stop; the 60 s poll interval is pacing and ends nothing. Bounds: 4
//! connections, a 16 KiB request head, 64 KiB buffered per direction.
use asura_platform::events::{WakeReader, WakeSender};
use asura_platform::{PollInterest, poll};
use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;
use zeroize::Zeroizing;

/// Connections open at once; one more is answered 503 and closed.
pub(crate) const MAX_CONNECTIONS: usize = 4;
/// The request head, request line and headers.
pub(crate) const MAX_HEAD_BYTES: usize = 16 * 1024;
/// Bytes buffered in each direction of one connection.
pub(crate) const BUFFER_BYTES: usize = 64 * 1024;
const PACING: Duration = Duration::from_secs(60);

/// The key as the conversation owner learns it.
enum KeyState {
    Pending,
    Ready(Zeroizing<Vec<u8>>),
    Refused,
}

struct Shared {
    key: Mutex<KeyState>,
    stop: AtomicBool,
    refused_used: AtomicBool,
}

/// A running relay. Dropping it stops it.
pub(crate) struct Relay {
    port: u16,
    token: Zeroizing<[u8; 32]>,
    shared: Arc<Shared>,
    wake: WakeSender,
    thread: Option<JoinHandle<()>>,
}

impl Relay {
    /// Binds `127.0.0.1:0` (one nonblocking socket call) and starts the
    /// relay thread for the router on `127.0.0.1:<upstream>`.
    pub(crate) fn start(upstream: u16) -> io::Result<Self> {
        let listener = TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))?;
        listener.set_nonblocking(true)?;
        let port = listener.local_addr()?.port();
        let mut token = Zeroizing::new([0u8; 32]);
        token[..16].copy_from_slice(&asura_platform::random_id());
        token[16..].copy_from_slice(&asura_platform::random_id());
        let shared = Arc::new(Shared {
            key: Mutex::new(KeyState::Pending),
            stop: AtomicBool::new(false),
            refused_used: AtomicBool::new(false),
        });
        let (reader, wake) = asura_platform::events::wake_pair()?;
        let worker = Worker {
            listener,
            upstream,
            token: *token,
            shared: shared.clone(),
            reader,
            connections: Vec::new(),
        };
        let thread = std::thread::Builder::new()
            .name("asura-llamacpp-relay".into())
            .spawn(move || worker.run())?;
        Ok(Self {
            port,
            token,
            shared,
            wake,
            thread: Some(thread),
        })
    }
    /// The endpoint the helper uses.
    pub(crate) fn url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }
    pub(crate) fn token(&self) -> Vec<u8> {
        self.token.to_vec()
    }
    fn set(&self, state: KeyState) {
        *self.shared.key.lock().unwrap_or_else(|e| e.into_inner()) = state;
        let _ = self.wake.notify();
    }
    /// The key arrived from the secret store.
    pub(crate) fn provide_key(&self, key: Zeroizing<Vec<u8>>) {
        self.set(KeyState::Ready(key));
    }
    /// The key is unavailable: waiting and later requests get 503.
    pub(crate) fn refuse_key(&self) {
        self.set(KeyState::Refused);
    }
    /// A request was refused because the key was unavailable.
    pub(crate) fn refused_a_request(&self) -> bool {
        self.shared.refused_used.load(Ordering::Acquire)
    }
    /// Closes the listener and every connection; the thread then exits.
    pub(crate) fn stop(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
        let _ = self.wake.notify();
    }
    /// Test-only: waits for the thread after [`Relay::stop`].
    #[cfg(test)]
    pub(crate) fn join(mut self) {
        self.stop();
        if let Some(thread) = self.thread.take() {
            thread.join().expect("relay thread");
        }
    }
}

impl Drop for Relay {
    /// The thread blocks only in `poll`, which the wake ends; it is not
    /// joined, so a drop on the reactor never waits.
    fn drop(&mut self) {
        self.stop();
        drop(self.thread.take());
    }
}

/// The pending read of the key a relay adds: the server key item (`asura`
/// mode) or the named secret `providers.llamacpp.keySecret` (`url` mode).
pub(crate) enum KeyTicket {
    Server(crate::secret_store::Ticket<crate::secret_store::ServerKeyReply>),
    Named(crate::secret_store::Ticket<crate::secret_store::NamedGetReply>),
}
impl KeyTicket {
    /// Gives the relay the key or its refusal once the read ends. Returns
    /// true when the read has ended. A value that cannot be an HTTP header
    /// value (outside printable ASCII) is refused, never sent.
    pub(crate) fn poll(&mut self, relay: &Relay) -> bool {
        use crate::secret_store::{NamedGetReply, Polled, ServerKeyReply};
        let key = match self {
            Self::Server(ticket) => match ticket.poll() {
                Polled::Pending => return false,
                Polled::Ready(ServerKeyReply::Key(key)) => Some(key),
                Polled::Ready(_) | Polled::Lost => None,
            },
            Self::Named(ticket) => match ticket.poll() {
                Polled::Pending => return false,
                Polled::Ready(NamedGetReply::Values(mut values)) if values.len() == 1 => {
                    values.pop()
                }
                Polled::Ready(_) | Polled::Lost => None,
            },
        };
        match key.filter(|key| header_value(key)) {
            Some(key) => relay.provide_key(key),
            None => relay.refuse_key(),
        }
        true
    }
}

/// 1 to 4,096 bytes of visible ASCII: safe in an `Authorization` line.
fn header_value(key: &[u8]) -> bool {
    (1..=4096).contains(&key.len()) && key.iter().all(|b| (0x21..=0x7e).contains(b))
}

/// Where a `llamacpp:` turn goes (LC-F2).
pub(crate) struct Route {
    pub endpoint: String,
    /// The relay and its key read, when a key is added.
    pub relay: Option<(Relay, KeyTicket)>,
    /// Asura's own server: the verified local route for tools.
    pub local: bool,
}

/// Resolves the mode at turn preparation. Pure except for the relay bind and
/// the key request, which are one nonblocking socket call and one queue push.
pub(crate) fn route(
    settings: &asura_storage::config::LlamacppSettings,
    store: Option<&crate::secret_store::Store>,
) -> Result<Route, &'static str> {
    use asura_storage::config::LlamacppServer;
    let relayed =
        |upstream: u16, key: &dyn Fn(&crate::secret_store::Store) -> Option<KeyTicket>| {
            let store = store.ok_or("llamacpp_key_unavailable")?;
            let ticket = key(store).ok_or("llamacpp_key_unavailable")?;
            let relay = Relay::start(upstream).map_err(|_| "model_unavailable")?;
            Ok::<_, &'static str>((relay, ticket))
        };
    match settings.server {
        LlamacppServer::Off => Err("llamacpp_server_off"),
        LlamacppServer::LlamaApp => Ok(Route {
            endpoint: asura_storage::config::LLAMA_APP_ENDPOINT.into(),
            relay: None,
            local: false,
        }),
        LlamacppServer::Asura => {
            let (relay, ticket) = relayed(settings.port, &|store| {
                store.get_server_key().ok().map(KeyTicket::Server)
            })?;
            Ok(Route {
                endpoint: relay.url(),
                relay: Some((relay, ticket)),
                local: true,
            })
        }
        LlamacppServer::Url => {
            let endpoint = settings.endpoint.clone().ok_or("llamacpp_endpoint_unset")?;
            if !asura_storage::config::loopback_endpoint(&endpoint) {
                return Err("remote_endpoint_restricted");
            }
            let Some(name) = settings.key_secret.clone() else {
                return Ok(Route {
                    endpoint,
                    relay: None,
                    local: false,
                });
            };
            // The relay speaks HTTP to 127.0.0.1 only.
            let upstream = endpoint
                .strip_prefix("http://127.0.0.1")
                .map(|rest| rest.trim_end_matches('/'))
                .and_then(|rest| match rest {
                    "" => Some(80),
                    rest => rest.strip_prefix(':')?.parse::<u16>().ok(),
                })
                .ok_or("llamacpp_endpoint_unsupported")?;
            let (relay, ticket) = relayed(upstream, &|store| {
                store
                    .get_named(vec![name.clone()])
                    .ok()
                    .map(KeyTicket::Named)
            })?;
            Ok(Route {
                endpoint: relay.url(),
                relay: Some((relay, ticket)),
                local: false,
            })
        }
    }
}

/// The `/status` facts (llamacpp-backend.md, status), on the status
/// sampler thread. `None` when the configuration cannot be read. In `asura`
/// mode the approved binary is hashed (at most 256 MiB) and the folder
/// listed (at most 64 links). The health check ends on the reply, an error,
/// or `cancel`; its 250 ms poll interval is pacing and ends nothing.
pub(crate) fn sample(
    runtime: &asura_platform::RuntimeDirectory,
    cancel: &AtomicBool,
) -> Option<asura_control::pb::LlamaCppStatus> {
    use asura_storage::config::LlamacppServer;
    use asura_storage::llamacpp::{self as files, ApprovalState};
    let settings = asura_storage::config::llamacpp_settings(runtime.clone(), cancel).ok()?;
    let paths = files::Paths::of(runtime);
    let mut status = asura_control::pb::LlamaCppStatus {
        mode: Some(match settings.server {
            LlamacppServer::Off => 1,
            LlamacppServer::Asura => 2,
            LlamacppServer::LlamaApp => 3,
            LlamacppServer::Url => 4,
        }),
        ..Default::default()
    };
    let upstream = match settings.server {
        LlamacppServer::Off => None,
        LlamacppServer::Asura => {
            status.port = Some(u32::from(settings.port));
            status.endpoint = Some(format!("http://127.0.0.1:{}", settings.port));
            status.approval = Some(match files::check(&paths) {
                Ok(ApprovalState::Unset) => 1,
                Ok(ApprovalState::Approved(_)) => 2,
                Ok(ApprovalState::Changed { .. }) => 3,
                Ok(ApprovalState::Missing(_)) => 4,
                Err(_) => 5,
            });
            status.model_count = files::list(&paths)
                .ok()
                .map(|links| links.iter().filter(|l| l.size.is_some()).count() as u32);
            let home = paths.logs.parent().and_then(|asura| asura.parent());
            status.agent_file =
                home.map(|home| asura_platform::llamacpp::agent_path(home).exists());
            Some(settings.port)
        }
        LlamacppServer::LlamaApp => {
            status.endpoint = Some(asura_storage::config::LLAMA_APP_ENDPOINT.into());
            Some(9931)
        }
        LlamacppServer::Url => {
            status.endpoint = settings.endpoint.clone();
            settings.endpoint.as_deref().and_then(|e| {
                let rest = e.strip_prefix("http://127.0.0.1")?.trim_end_matches('/');
                match rest {
                    "" => Some(80),
                    rest => rest.strip_prefix(':')?.parse().ok(),
                }
            })
        }
    };
    if let Some(port) = upstream {
        status.answering = health(port, cancel);
    }
    Some(status)
}

/// `GET /health` without a key: any HTTP reply means the server answers
/// (the router may answer 401 without the key). `None` when cancelled.
fn health(port: u16, cancel: &AtomicBool) -> Option<bool> {
    let Ok(mut stream) = asura_platform::llamacpp::connect_loopback(port) else {
        return Some(false);
    };
    let mut request =
        format!("GET /health HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n")
            .into_bytes();
    let mut reply = Vec::new();
    loop {
        if cancel.load(Ordering::Acquire) {
            return None;
        }
        let interest = PollInterest {
            fd: stream.as_raw_fd(),
            read: true,
            write: !request.is_empty(),
        };
        if poll(&[interest], Duration::from_millis(250)).is_err() {
            return Some(false);
        }
        match stream.take_error() {
            Ok(None) => {}
            _ => return Some(false),
        }
        if !request.is_empty() {
            match stream.peer_addr() {
                Ok(_) => {
                    if write_from(&mut stream, &mut request).is_err() {
                        return Some(false);
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::NotConnected => continue,
                Err(_) => return Some(false),
            }
        }
        match read_into(&mut stream, &mut reply, 4096) {
            Ok(eof) => {
                if reply.starts_with(b"HTTP/1.") && reply.len() >= 12 {
                    return Some(true);
                }
                if eof || reply.len() >= 4096 {
                    return Some(false);
                }
            }
            Err(()) => return Some(false),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Phase {
    Head,
    KeyWait,
    Connecting,
    Piping,
    Replying,
}

struct Connection {
    client: TcpStream,
    upstream: Option<TcpStream>,
    phase: Phase,
    head: Vec<u8>,
    /// The request head and body prefix, without the key, after the head.
    request: Option<Request>,
    to_upstream: Vec<u8>,
    to_client: Vec<u8>,
    client_eof: bool,
    upstream_eof: bool,
    closed: bool,
}

/// A checked request: its rewritten head before the key line, and the body
/// bytes read with the head.
struct Request {
    head: String,
    body: Vec<u8>,
}

struct Worker {
    listener: TcpListener,
    upstream: u16,
    token: [u8; 32],
    shared: Arc<Shared>,
    reader: WakeReader,
    connections: Vec<Connection>,
}

impl Worker {
    fn run(mut self) {
        while !self.shared.stop.load(Ordering::Acquire) {
            let mut interests = vec![
                PollInterest {
                    fd: self.reader.as_raw_fd(),
                    read: true,
                    write: false,
                },
                PollInterest {
                    fd: self.listener.as_raw_fd(),
                    read: true,
                    write: false,
                },
            ];
            for connection in &self.connections {
                interests.push(PollInterest {
                    fd: connection.client.as_raw_fd(),
                    read: connection.wants_client_read(),
                    write: !connection.to_client.is_empty(),
                });
                if let Some(upstream) = &connection.upstream {
                    interests.push(PollInterest {
                        fd: upstream.as_raw_fd(),
                        read: connection.wants_upstream_read(),
                        write: connection.phase == Phase::Connecting
                            || !connection.to_upstream.is_empty(),
                    });
                }
            }
            if poll(&interests, PACING).is_err() {
                break;
            }
            let _ = self.reader.drain();
            if self.shared.stop.load(Ordering::Acquire) {
                break;
            }
            self.accept();
            let key = self.key();
            for connection in &mut self.connections {
                connection.step(&self.token, self.upstream, &key, &self.shared);
            }
            self.connections.retain(|c| !c.closed);
        }
        // Stop: every socket closes as the worker drops.
    }

    fn key(&self) -> Option<Option<Zeroizing<Vec<u8>>>> {
        match &*self.shared.key.lock().unwrap_or_else(|e| e.into_inner()) {
            KeyState::Pending => None,
            KeyState::Ready(key) => Some(Some(key.clone())),
            KeyState::Refused => Some(None),
        }
    }

    fn accept(&mut self) {
        loop {
            match self.listener.accept() {
                Ok((stream, _)) => {
                    if stream.set_nonblocking(true).is_err() {
                        continue;
                    }
                    let mut connection = Connection::new(stream);
                    if self.connections.len() >= MAX_CONNECTIONS {
                        connection.reply(503, "asura_relay_busy");
                        connection.flush_client();
                    } else {
                        self.connections.push(connection);
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => return,
            }
        }
    }
}

impl Connection {
    fn new(client: TcpStream) -> Self {
        Self {
            client,
            upstream: None,
            phase: Phase::Head,
            head: Vec::new(),
            request: None,
            to_upstream: Vec::new(),
            to_client: Vec::new(),
            client_eof: false,
            upstream_eof: false,
            closed: false,
        }
    }
    fn wants_client_read(&self) -> bool {
        match self.phase {
            Phase::Head => true,
            Phase::Piping => !self.client_eof && self.to_upstream.len() < BUFFER_BYTES,
            // A close while waiting must be seen.
            Phase::KeyWait | Phase::Connecting => true,
            Phase::Replying => false,
        }
    }
    fn wants_upstream_read(&self) -> bool {
        self.phase == Phase::Piping && !self.upstream_eof && self.to_client.len() < BUFFER_BYTES
    }
    fn reply(&mut self, status: u16, kind: &str) {
        let reason = match status {
            400 => "Bad Request",
            401 => "Unauthorized",
            403 => "Forbidden",
            502 => "Bad Gateway",
            _ => "Service Unavailable",
        };
        let body = format!(r#"{{"error":{{"code":{status},"type":"{kind}"}}}}"#);
        self.to_client = format!(
            "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .into_bytes();
        self.to_upstream.clear();
        self.upstream = None;
        self.phase = Phase::Replying;
    }
    fn flush_client(&mut self) -> bool {
        while !self.to_client.is_empty() {
            match self.client.write(&self.to_client) {
                Ok(0) => return false,
                Ok(n) => {
                    self.to_client.drain(..n);
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return true,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => return false,
            }
        }
        true
    }
    fn close(&mut self) {
        let _ = self.client.shutdown(Shutdown::Both);
        if let Some(upstream) = &self.upstream {
            let _ = upstream.shutdown(Shutdown::Both);
        }
        self.closed = true;
    }
    fn step(
        &mut self,
        token: &[u8; 32],
        upstream: u16,
        key: &Option<Option<Zeroizing<Vec<u8>>>>,
        shared: &Shared,
    ) {
        if self.phase == Phase::Head {
            match read_into(&mut self.client, &mut self.head, MAX_HEAD_BYTES + 1) {
                Ok(true) => return self.close(),
                Ok(false) => {}
                Err(()) => return self.close(),
            }
            match check(&self.head, token, upstream) {
                Check::Incomplete if self.head.len() > MAX_HEAD_BYTES => {
                    self.reply(400, "asura_relay_head")
                }
                Check::Incomplete => return,
                Check::Refuse(status, kind) => self.reply(status, kind),
                Check::Accept(request) => {
                    self.request = Some(request);
                    self.phase = Phase::KeyWait;
                }
            }
        }
        if matches!(self.phase, Phase::KeyWait | Phase::Connecting) {
            // A client that closes while it waits ends the connection.
            let mut probe = [0u8; 1];
            match self.client.peek(&mut probe) {
                Ok(0) => return self.close(),
                Err(e) if e.kind() != io::ErrorKind::WouldBlock => return self.close(),
                _ => {}
            }
        }
        if self.phase == Phase::KeyWait {
            match key {
                None => return,
                Some(None) => {
                    shared.refused_used.store(true, Ordering::Release);
                    self.reply(503, "asura_key_unavailable");
                }
                Some(Some(key)) => {
                    let request = self.request.take().expect("checked request");
                    let mut bytes = Zeroizing::new(request.head.into_bytes());
                    bytes.extend_from_slice(b"Authorization: Bearer ");
                    bytes.extend_from_slice(key);
                    bytes.extend_from_slice(b"\r\n\r\n");
                    self.to_upstream = std::mem::take(&mut *bytes);
                    self.to_upstream.extend_from_slice(&request.body);
                    match asura_platform::llamacpp::connect_loopback(upstream) {
                        Ok(stream) => {
                            self.upstream = Some(stream);
                            self.phase = Phase::Connecting;
                        }
                        Err(_) => self.reply(502, "asura_upstream_unavailable"),
                    }
                }
            }
        }
        if self.phase == Phase::Connecting {
            let stream = self.upstream.as_ref().expect("connecting stream");
            match stream.take_error() {
                Ok(None) => match stream.peer_addr() {
                    Ok(_) => self.phase = Phase::Piping,
                    // Still connecting.
                    Err(e) if e.kind() == io::ErrorKind::NotConnected => return,
                    Err(_) => self.reply(502, "asura_upstream_unavailable"),
                },
                _ => self.reply(502, "asura_upstream_unavailable"),
            }
        }
        if self.phase == Phase::Piping {
            self.pipe();
        }
        if self.phase == Phase::Replying && (!self.flush_client() || self.to_client.is_empty()) {
            self.close();
        }
    }
    fn pipe(&mut self) {
        let upstream = self.upstream.as_mut().expect("piping stream");
        // Client to upstream.
        if !self.client_eof && self.to_upstream.len() < BUFFER_BYTES {
            match read_into(&mut self.client, &mut self.to_upstream, BUFFER_BYTES) {
                Ok(true) => self.client_eof = true,
                Ok(false) => {}
                Err(()) => return self.close(),
            }
        }
        if write_from(upstream, &mut self.to_upstream).is_err() {
            return self.close();
        }
        // Upstream to client.
        if !self.upstream_eof && self.to_client.len() < BUFFER_BYTES {
            match read_into(upstream, &mut self.to_client, BUFFER_BYTES) {
                Ok(true) => self.upstream_eof = true,
                Ok(false) => {}
                Err(()) => return self.close(),
            }
        }
        if !self.flush_client() {
            return self.close();
        }
        // The helper closed (a cancel), or the router finished its reply.
        if self.client_eof || (self.upstream_eof && self.to_client.is_empty()) {
            self.close();
        }
    }
}

/// Reads what is available into `buffer`, up to `limit` bytes in all.
/// `Ok(true)` is end of stream.
fn read_into(stream: &mut TcpStream, buffer: &mut Vec<u8>, limit: usize) -> Result<bool, ()> {
    let mut chunk = [0u8; 8192];
    while buffer.len() < limit {
        let room = (limit - buffer.len()).min(chunk.len());
        match stream.read(&mut chunk[..room]) {
            Ok(0) => return Ok(true),
            Ok(n) => buffer.extend_from_slice(&chunk[..n]),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(false),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => return Err(()),
        }
    }
    Ok(false)
}

fn write_from(stream: &mut TcpStream, buffer: &mut Vec<u8>) -> Result<(), ()> {
    while !buffer.is_empty() {
        match stream.write(buffer) {
            Ok(0) => return Err(()),
            Ok(n) => {
                buffer.drain(..n);
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(()),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => return Err(()),
        }
    }
    Ok(())
}

enum Check {
    Incomplete,
    Refuse(u16, &'static str),
    Accept(Request),
}

/// LC-U10: the head's method, path, token and headers.
fn check(bytes: &[u8], token: &[u8; 32], upstream: u16) -> Check {
    let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") else {
        return Check::Incomplete;
    };
    if end > MAX_HEAD_BYTES {
        return Check::Refuse(400, "asura_relay_head");
    }
    let Ok(head) = std::str::from_utf8(&bytes[..end]) else {
        return Check::Refuse(400, "asura_relay_head");
    };
    let mut lines = head.split("\r\n");
    let line = lines.next().unwrap_or_default();
    let mut parts = line.split(' ');
    let (Some(method), Some(target), Some(version), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Check::Refuse(400, "asura_relay_head");
    };
    if version != "HTTP/1.1" {
        return Check::Refuse(400, "asura_relay_head");
    }
    let mut authorized = false;
    let mut length = None;
    let mut kept = Vec::new();
    for header in lines {
        let Some((name, value)) = header.split_once(':') else {
            return Check::Refuse(400, "asura_relay_head");
        };
        let lower = name.trim().to_ascii_lowercase();
        let value = value.trim();
        match lower.as_str() {
            "authorization" => authorized = bearer_matches(value, token),
            "transfer-encoding" | "expect" => return Check::Refuse(400, "asura_relay_head"),
            "connection" | "keep-alive" | "upgrade" | "te" | "trailer" | "host" => {}
            name if name.starts_with("proxy-") => {}
            "content-length" => {
                let Ok(value) = value.parse::<u64>() else {
                    return Check::Refuse(400, "asura_relay_head");
                };
                if length.replace(value).is_some() {
                    return Check::Refuse(400, "asura_relay_head");
                }
                kept.push(format!("Content-Length: {value}"));
            }
            _ => kept.push(format!("{}: {value}", name.trim())),
        }
    }
    if !authorized {
        return Check::Refuse(401, "asura_relay_token");
    }
    if !allowed(method, target) {
        return Check::Refuse(403, "asura_relay_path");
    }
    if method == "POST" && length.is_none() {
        return Check::Refuse(400, "asura_relay_head");
    }
    let mut rewritten = format!("{method} {target} HTTP/1.1\r\nHost: 127.0.0.1:{upstream}\r\n");
    for header in kept {
        rewritten.push_str(&header);
        rewritten.push_str("\r\n");
    }
    rewritten.push_str("Connection: close\r\n");
    Check::Accept(Request {
        head: rewritten,
        body: bytes[end + 4..].to_vec(),
    })
}

/// `Bearer <64 hex digits>` equal to the token, compared in constant time.
fn bearer_matches(value: &str, token: &[u8; 32]) -> bool {
    let Some(hex) = value.strip_prefix("Bearer ") else {
        return false;
    };
    let hex = hex.as_bytes();
    if hex.len() != 64 {
        return false;
    }
    let digit = |c: u8| match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        _ => None,
    };
    let mut difference = 0u8;
    for (i, expected) in token.iter().enumerate() {
        let (Some(high), Some(low)) = (digit(hex[2 * i]), digit(hex[2 * i + 1])) else {
            return false;
        };
        difference |= ((high << 4) | low) ^ expected;
    }
    difference == 0
}

/// The four requests the backend makes.
fn allowed(method: &str, target: &str) -> bool {
    match (method, target) {
        ("GET", "/v1/models" | "/health") | ("POST", "/v1/chat/completions") => true,
        ("GET", target) => target
            .strip_prefix("/props?model=")
            .is_some_and(|name| encoded_name(name.as_bytes())),
        _ => false,
    }
}

/// 1 to 3,072 bytes of unreserved characters and `%XX` escapes: a name of
/// at most 1,024 bytes, percent-encoded by the helper.
fn encoded_name(bytes: &[u8]) -> bool {
    if bytes.is_empty() || bytes.len() > 3 * 1024 {
        return false;
    }
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            i += 1;
        } else if b == b'%'
            && bytes.get(i + 1).is_some_and(u8::is_ascii_hexdigit)
            && bytes.get(i + 2).is_some_and(u8::is_ascii_hexdigit)
        {
            i += 3;
        } else {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests;
