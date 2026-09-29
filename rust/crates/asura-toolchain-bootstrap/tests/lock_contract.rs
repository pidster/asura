//! Public API and file-to-validation tests. This is not tool preparation.
#![forbid(unsafe_code)]
#[cfg(not(target_os = "macos"))]
compile_error!("this qualification requires macOS sandbox-exec");
use asura_toolchain_bootstrap::{ErrorCode, parse_lock};
use std::{
    fs,
    process::Command,
    thread,
    time::{Duration, Instant},
};
const FIXTURE: &[u8] = include_bytes!("fixtures/lock.json");

#[test]
fn public_validated_contract() {
    let lock = parse_lock(FIXTURE).unwrap();
    assert_eq!(lock.protoc().version(), "36.2");
    assert!(lock.protoc().url().starts_with("https://github.com/"));
    assert_eq!(lock.protoc().sha256().len(), 64);
    assert_eq!(lock.protoc().archive_kind(), "zip");
    assert_eq!(lock.protoc().expected_tool_version(), "36.2");
    assert_eq!(lock.protoc().required_member(), "bin/protoc");
    assert_eq!(
        lock.protoc().allowed_top_level(),
        ["bin", "include", "readme.txt"]
    );
    assert_eq!(
        lock.protoc().redirect_hosts(),
        ["release-assets.githubusercontent.com"]
    );
    let swift = lock.swift_protobuf();
    assert_eq!(swift.version(), "1.38.1");
    assert!(swift.url().ends_with("1.38.1.tar.gz"));
    assert_eq!(swift.sha256().len(), 64);
    assert_eq!(swift.archive_kind(), "tar.gz");
    assert_eq!(swift.top_level(), "swift-protobuf-1.38.1");
    assert_eq!(swift.source_commit().len(), 40);
    assert_eq!(swift.expected_tool_version(), "1.38.1");
    assert_eq!(swift.redirect_hosts(), ["codeload.github.com"]);
    lock.validate_host(lock.host()).unwrap();
    for field in 0..7 {
        let mut observed = lock.host().clone();
        match field {
            0 => observed.os = "linux".into(),
            1 => observed.os_major = 28,
            2 => observed.arch = "x86_64".into(),
            3 => observed.xcode_build = "other".into(),
            4 => observed.swift_version = "6.5".into(),
            5 => observed.rust_version = "1.99.0".into(),
            _ => observed.cargo_version = "1.99.0".into(),
        }
        assert_eq!(
            lock.validate_host(&observed).unwrap_err().code,
            ErrorCode::HostMismatch
        );
    }
}

/// Selected only by the parent test. Its environment names a file the harness owns.
#[test]
fn file_validation_child() {
    let Some(path) = std::env::var_os("ASURA_LOCK_TEST_FILE") else {
        let lock = parse_lock(FIXTURE).unwrap();
        lock.validate_host(lock.host()).unwrap();
        assert_eq!(lock.protoc().required_member(), "bin/protoc");
        return;
    };
    // Probe actual OS network denial, without sending a packet or depending on a remote host.
    assert!(
        std::net::TcpListener::bind("127.0.0.1:0").is_err(),
        "network sandbox not enforced"
    );
    let input = fs::read(path).unwrap();
    let result = parse_lock(&input);
    if std::env::var_os("ASURA_LOCK_TEST_REJECT").is_some() {
        assert_eq!(result.unwrap_err().code, ErrorCode::InvalidJson);
    } else {
        let lock = result.unwrap();
        assert_eq!(lock.protoc().required_member(), "bin/protoc");
        lock.validate_host(lock.host()).unwrap();
    }
}

#[test]
fn file_validation_under_network_denial() {
    let directory =
        std::env::temp_dir().join(format!("asura-lock-contract-{}", std::process::id()));
    fs::create_dir(&directory).unwrap();
    let path = directory.join("lock.json");
    for rejected in [false, true] {
        fs::write(
            &path,
            if rejected {
                b"{\"unexpected\":true}"
            } else {
                FIXTURE
            },
        )
        .unwrap();
        let mut command = Command::new("/usr/bin/sandbox-exec");
        command
            .args(["-p", "(version 1)(allow default)(deny network*)"])
            .arg(std::env::current_exe().unwrap())
            .args(["--exact", "file_validation_child", "--nocapture"])
            .env("ASURA_LOCK_TEST_FILE", &path)
            .env_remove("ASURA_LOCK_TEST_REJECT");
        if rejected {
            command.env("ASURA_LOCK_TEST_REJECT", "1");
        }
        // Inherit the driver's supervised group. This child creates no descendants.
        let mut child = command.spawn().unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            assert!(
                Instant::now() < deadline,
                "child exceeded its bound; driver must settle the group"
            );
            thread::sleep(Duration::from_millis(10));
        };
        assert!(status.success(), "file validation child failed: {status}");
    }
    fs::remove_dir_all(directory).unwrap();
}
