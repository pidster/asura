//! Real socket/owner lifecycle in a private scratch home (FND1/3/7/10 subset).
use asura_client::Client;
use asura_control::{ControlCodec, Frame, encode_frame, pb};
use asura_platform::{PollInterest, RuntimeDirectory};
use std::{
    fs,
    io::{Read, Write},
    os::{fd::AsRawFd, unix::fs::PermissionsExt},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

struct Scratch(std::path::PathBuf);
impl Scratch {
    fn new() -> Self {
        let nonce: String = asura_platform::random_id()
            .iter()
            .map(|v| format!("{v:02x}"))
            .collect();
        let path = std::path::PathBuf::from("/private/tmp")
            .join(format!("asura-service-{}-{nonce}", std::process::id()));
        fs::create_dir(&path).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        Self(path)
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn launch(
    runtime: &RuntimeDirectory,
    log: &std::path::Path,
) -> (
    thread::JoinHandle<()>,
    mpsc::Receiver<Result<(), asura_service::Error>>,
) {
    let runtime = runtime.clone();
    let file = fs::OpenOptions::new()
        .append(true)
        .create(true)
        .open(log)
        .unwrap();
    let (tx, rx) = mpsc::channel();
    let handle = thread::spawn(move || {
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_writer(std::sync::Mutex::new(file))
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            let _ = tx.send(asura_service::run(runtime, "asura/test", None));
        });
    });
    (handle, rx)
}
fn attach(runtime: &RuntimeDirectory) -> Client {
    let end = Instant::now() + Duration::from_secs(3);
    loop {
        match Client::attach(runtime, "asura/test", end) {
            Ok(client) => return client,
            Err(error)
                if (asura_client::is_absent(&error)
                    || matches!(
                        error,
                        asura_client::Error::Platform(asura_platform::Error::OwnerBusy)
                    ))
                    && Instant::now() < end =>
            {
                asura_platform::poll(&[], Duration::from_millis(5)).unwrap();
            }
            Err(error) => panic!("attach failed: {error}"),
        }
    }
}
fn wire_exchange(runtime: &RuntimeDirectory, bytes: &[u8]) -> Frame {
    let mut stream = runtime.connect().unwrap();
    let end = Instant::now() + Duration::from_secs(2);
    // Send each byte separately through the real stream. Scheduling may coalesce
    // writes; the control tests separately prove every exact fragmentation.
    for byte in bytes {
        loop {
            assert!(Instant::now() < end);
            match stream.stream_mut().write(std::slice::from_ref(byte)) {
                Ok(1) => break,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    asura_platform::poll(
                        &[PollInterest {
                            fd: stream.stream().as_raw_fd(),
                            read: false,
                            write: true,
                        }],
                        Duration::from_millis(10),
                    )
                    .unwrap();
                }
                other => panic!("write: {other:?}"),
            }
        }
    }
    let mut codec = ControlCodec::new();
    let mut buffer = [0; 1024];
    loop {
        assert!(Instant::now() < end);
        match stream.stream_mut().read(&mut buffer) {
            Ok(0) => panic!("unexpected EOF"),
            Ok(n) => {
                assert_eq!(codec.push(&buffer[..n]).unwrap(), n);
                if let Some(frame) = codec.next_frame().unwrap() {
                    return frame;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                asura_platform::poll(
                    &[PollInterest {
                        fd: stream.stream().as_raw_fd(),
                        read: true,
                        write: false,
                    }],
                    Duration::from_millis(10),
                )
                .unwrap();
            }
            Err(e) => panic!("read: {e}"),
        }
    }
}
#[test]
fn real_socket_attach_inspect_stop_restart_and_negotiation() {
    let scratch = Scratch::new();
    let runtime = RuntimeDirectory::scratch(&scratch.0, true).unwrap();
    let (handle, done) = launch(&runtime, &scratch.0.join("events.log"));
    let mut first = attach(&runtime);
    let first_snapshot = first.inspect().unwrap();
    let mut second = attach(&runtime);
    let second_snapshot = second.inspect().unwrap();
    assert_eq!(second_snapshot.service_epoch, first_snapshot.service_epoch);
    assert_eq!(second_snapshot.service_build, first_snapshot.service_build);
    assert_eq!(second_snapshot.lifecycle, first_snapshot.lifecycle);
    assert!(runtime.acquire_owner().is_err());
    let hello = pb::Envelope {
        service_epoch: None,
        attachment_id: None,
        request_counter: None,
        body: Some(pb::envelope::Body::Hello(pb::Hello {
            client_build: Some("asura/test".into()),
        })),
    };
    let frame = wire_exchange(&runtime, &encode_frame(&hello).unwrap());
    assert!(matches!(
        frame,
        Frame::Message(envelope) if matches!(envelope.body, Some(pb::envelope::Body::HelloReply(_)))
    ));
    let mut bad_version = encode_frame(&hello).unwrap();
    bad_version[5] = 2;
    assert_eq!(
        wire_exchange(&runtime, &bad_version[..asura_control::HEADER_BYTES]),
        Frame::VersionRejected
    );
    first.stop().unwrap();
    done.recv_timeout(Duration::from_secs(3)).unwrap().unwrap();
    handle.join().unwrap();
    assert!(second.inspect().is_err());
    assert!(matches!(
        runtime.connect(),
        Err(asura_platform::Error::Absent | asura_platform::Error::Refused)
    ));
    let (handle, done) = launch(&runtime, &scratch.0.join("events.log"));
    let mut restarted = attach(&runtime);
    assert_ne!(
        restarted.inspect().unwrap().service_epoch,
        first_snapshot.service_epoch
    );
    restarted.stop().unwrap();
    done.recv_timeout(Duration::from_secs(3)).unwrap().unwrap();
    handle.join().unwrap();
    let events = fs::read_to_string(scratch.0.join("events.log")).unwrap();
    let lines: Vec<_> = events.lines().collect();
    assert_eq!(lines.len(), 6);
    for sequence in lines.chunks(3) {
        assert!(sequence[0].contains("service_serving"));
        assert!(sequence[1].contains("service_draining"));
        assert!(sequence[2].contains("service_stopped"));
    }
    let entries: Vec<_> = fs::read_dir(scratch.0.join(".asura"))
        .unwrap()
        .map(|v| v.unwrap().file_name())
        .collect();
    assert_eq!(entries, vec![std::ffi::OsString::from("run")]);
}

#[test]
fn config_persists_across_service_restart_and_rejects_without_mutation() {
    let scratch = Scratch::new();
    let runtime = RuntimeDirectory::scratch(&scratch.0, true).unwrap();
    let (handle, done) = launch(&runtime, &scratch.0.join("events.log"));
    let mut client = attach(&runtime);
    assert_eq!(
        client
            .config("audit.enabled", None)
            .unwrap()
            .value_yaml
            .as_deref(),
        Some("true\n")
    );
    assert!(client.config("model", None).unwrap().error.is_some());
    assert_eq!(
        client.config("", None).unwrap().value_yaml.as_deref(),
        Some("audit:\n  enabled: true\n  keepFiles: 5\n  maxFileBytes: 10485760\n")
    );
    assert!(!scratch.0.join(".asura/config.yaml").exists());
    assert!(
        client
            .config("model", Some("provider/conversation-model"))
            .unwrap()
            .error
            .is_none()
    );
    assert!(
        client
            .config("audit.keepFiles", Some("8"))
            .unwrap()
            .error
            .is_none()
    );
    assert!(
        client
            .config("audit.enabled", Some("false"))
            .unwrap()
            .error
            .is_none()
    );
    let path = scratch.0.join(".asura/config.yaml");
    let before = fs::read(&path).unwrap();
    assert_eq!(
        client
            .config("", None)
            .unwrap()
            .value_yaml
            .unwrap()
            .as_bytes(),
        before
    );
    assert_eq!(fs::read(&path).unwrap(), before);
    for (key, value) in [
        ("model", "true"),
        ("audit.keepFiles", "0"),
        ("audit.unknown", "1"),
        (
            "audit",
            "{enabled: true, enabled: false, keepFiles: 5, maxFileBytes: 100}",
        ),
    ] {
        assert!(client.config(key, Some(value)).unwrap().error.is_some());
        assert_eq!(fs::read(&path).unwrap(), before);
        assert_eq!(
            client.inspect().unwrap().lifecycle,
            pb::Lifecycle::Serving as i32
        );
    }
    client.stop().unwrap();
    done.recv_timeout(Duration::from_secs(3)).unwrap().unwrap();
    handle.join().unwrap();
    let (handle, done) = launch(&runtime, &scratch.0.join("events.log"));
    let mut client = attach(&runtime);
    assert_eq!(
        client.config("model", None).unwrap().value_yaml.as_deref(),
        Some("provider/conversation-model\n")
    );
    assert_eq!(
        client
            .config("audit.keepFiles", None)
            .unwrap()
            .value_yaml
            .as_deref(),
        Some("8\n")
    );
    assert_eq!(
        client
            .config("audit.enabled", None)
            .unwrap()
            .value_yaml
            .as_deref(),
        Some("false\n")
    );
    assert_eq!(
        client
            .config("audit.maxFileBytes", None)
            .unwrap()
            .value_yaml
            .as_deref(),
        Some("10485760\n")
    );
    client.stop().unwrap();
    done.recv_timeout(Duration::from_secs(3)).unwrap().unwrap();
    handle.join().unwrap();
}
