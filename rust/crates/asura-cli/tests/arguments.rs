use std::process::Command;

#[test]
fn help_and_nonterminal_chat_do_not_start_a_service() {
    let binary = env!("CARGO_BIN_EXE_asura");
    let help = Command::new(binary).arg("--help").output().unwrap();
    assert!(help.status.success());
    assert!(
        String::from_utf8(help.stdout)
            .unwrap()
            .contains("service status --json")
    );
    let bare = Command::new(binary).output().unwrap();
    assert_eq!(bare.status.code(), Some(2));
    assert!(bare.stdout.is_empty());
    assert!(
        String::from_utf8(bare.stderr)
            .unwrap()
            .contains("requires a terminal")
    );
}

#[test]
fn unsupported_arguments_and_invalid_private_launch_fail_before_runtime_access() {
    let binary = env!("CARGO_BIN_EXE_asura");
    for args in [
        vec!["service", "stop", "--force"],
        vec!["service", "run", "--root", "/tmp"],
        vec!["service", "status"],
    ] {
        let output = Command::new(binary).args(args).output().unwrap();
        assert_eq!(output.status.code(), Some(2));
        assert!(output.stdout.is_empty());
    }
    let output = Command::new(binary)
        .args(["service", "run", "--internal-startup-notice"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(3));
    assert!(output.stdout.is_empty());
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("invalid_startup_notice")
    );
}

#[test]
fn unsafe_log_setup_preserves_json_and_fails_before_service_access() {
    use std::os::unix::fs::DirBuilderExt;
    let path = std::env::temp_dir().join(format!("asura-cli-logs-{}", std::process::id()));
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&path)
        .unwrap();
    let binary = env!("CARGO_BIN_EXE_asura");
    let log = path.join("asura.log");
    std::os::unix::fs::symlink("missing-target", &log).unwrap();
    let output = Command::new(binary)
        .args(["service", "status", "--json", "--logs"])
        .arg(&path)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(3));
    let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value["error_code"], "log_unavailable");
    assert!(!path.join("missing-target").exists());
    std::fs::remove_file(log).unwrap();
    std::fs::remove_dir(path).unwrap();
}

/// HD8 (release-signing.md SG4): the formula test runs `asura --version`.
/// It must print only the package version and need no file write and no
/// network, so it runs here under a Seatbelt profile that denies both.
#[test]
fn version_prints_the_package_version_without_writes_or_network() {
    let binary = env!("CARGO_BIN_EXE_asura");
    let profile = "(version 1)(allow default)(deny file-write*)(deny network*)";
    let output = Command::new("/usr/bin/sandbox-exec")
        .args(["-p", profile, binary, "--version"])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout),
        format!("{}\n", env!("CARGO_PKG_VERSION"))
    );
    assert!(output.stderr.is_empty(), "{output:?}");
    let output = Command::new(binary)
        .args(["--version", "--logs", "/nonexistent"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
}
