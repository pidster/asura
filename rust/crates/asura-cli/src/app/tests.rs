use super::*;
fn arguments(words: &[&str]) -> Vec<String> {
    words.iter().map(|s| s.to_string()).collect()
}
#[test]
fn command_surface_is_explicit_and_bounded() {
    assert_eq!(parse(&arguments(&[])), Ok(Command::Tui));
    assert_eq!(
        parse(&arguments(&["service", "status", "--json"])),
        Ok(Command::Status)
    );
    assert_eq!(
        parse(&arguments(&[
            "service",
            "run",
            "--internal-startup-notice",
            "--internal-owner-lifetime"
        ])),
        Ok(Command::Run {
            notice: true,
            owned: true
        })
    );
    for args in [
        &["service", "status"][..],
        &["service", "run", "--internal-owner-lifetime"],
        &["service", "stop", "--force"],
        &["service", "run", "--root", "/tmp"],
    ] {
        assert!(parse(&arguments(args)).is_err());
    }
}
#[test]
fn terminal_error_reporting_tolerates_stderr_backpressure() {
    struct Blocked;
    impl std::io::Write for Blocked {
        fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
            Err(std::io::ErrorKind::WouldBlock.into())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    // Regression: a failed diagnostic write must return without unwinding.
    report_tui_error(&mut Blocked, &std::io::ErrorKind::TimedOut.into());
}
#[test]
fn file_subscriber_appends_formatted_events() {
    let path = std::env::temp_dir().join(format!("asura-fmt-{}", std::process::id()));
    let file = asura_storage::logs::open_log(&path).unwrap();
    use std::io::Write;
    (&file).write_all(b"prior\n").unwrap();
    install_logger(Some(&file)).unwrap();
    tracing::info!("first_event");
    tracing::warn!("second_event");
    tracing::info!(target: "surrealdb::storage", "dependency path /private/tmp/secret-database");
    tracing::debug!("not_at_info");
    let content = std::fs::read_to_string(path.join("asura.log")).unwrap();
    let lines: Vec<_> = content.lines().collect();
    assert_eq!(lines.len(), 3);
    assert_eq!(lines[0], "prior");
    assert!(lines[1].contains("INFO first_event"));
    assert!(lines[2].contains("WARN second_event"));
    assert!(!content.contains('\u{1b}'));
    assert!(!content.contains("secret-database"));
    assert!(!content.contains("not_at_info"));
    std::fs::remove_file(path.join("asura.log")).unwrap();
    std::fs::remove_dir(path).unwrap();
}
#[test]
fn log_arguments_are_explicit() {
    for command in [
        vec!["service", "start"],
        vec!["service", "stop"],
        vec!["service", "run"],
        vec!["service", "status", "--json"],
    ] {
        let mut args = arguments(&command);
        args.extend(arguments(&["--logs", "relative"]));
        assert_eq!(
            logging_options(&args).unwrap().1.unwrap(),
            std::path::PathBuf::from("relative")
        );
    }
    for words in [
        vec!["service", "start", "--logs"],
        vec!["service", "start", "--logs", ""],
        vec!["service", "start", "--logs", "a", "--logs", "b"],
        vec!["service", "run", "--internal-startup-notice", "--logs", "a"],
    ] {
        assert!(logging_options(&arguments(&words)).is_err());
    }
}
#[test]
fn status_uses_nulls_for_absent_data() {
    let value = empty_status("absent", None);
    assert_eq!(value.as_object().unwrap().len(), 8);
    assert!(value["service_epoch"].is_null());
    assert!(value["installation"].is_null());
    let snapshot = asura_client::Snapshot {
        service_epoch: [1; 16],
        service_build: BUILD.into(),
        lifecycle: 2,
        installation: 2,
        reason: 2,
    };
    let value = snapshot_status(&snapshot);
    assert_eq!(value["installation"], "uninitialized");
    assert_eq!(value["unavailable_reason"], "runtime_only");
    assert_eq!(value["service_epoch"].as_str().unwrap().len(), 32);
}

#[test]
fn installation_commands_and_failure_nullability() {
    for (words, json) in [
        (vec!["installation", "status"], false),
        (vec!["installation", "status", "--json"], true),
    ] {
        assert_eq!(
            parse(&arguments(&words)),
            Ok(Command::InstallationStatus { json })
        );
    }
    assert!(parse(&arguments(&["installation", "initialize"])).is_err());
    let value = empty_installation("service_absent");
    assert_eq!(value.as_object().unwrap().len(), 10);
    for field in [
        "service_epoch",
        "installation_id",
        "authority_revision",
        "recorded_owner_generation",
        "binding_generation",
        "authority_format",
    ] {
        assert!(value[field].is_null());
    }
    assert_eq!(value["error_code"], "service_absent");
}
