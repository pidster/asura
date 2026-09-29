#[path = "sensors_cli.rs"]
mod sensors_cli;
use asura_client::{Client, Error as ClientError};
use asura_platform::{Error as PlatformError, RuntimeDirectory, StartupPipe};
use serde_json::{Value, json};
use std::time::{Duration, Instant};

const BUILD: &str = concat!("asura/", env!("CARGO_PKG_VERSION"));

#[derive(Debug, PartialEq, Eq)]
enum Command {
    Help,
    Initialize,
    ProjectAdd(String),
    ProjectList,
    Sensors {
        project: [u8; 16],
        offset: u32,
        revision: Option<u64>,
    },
    Tui,
    Start,
    Status,
    InstallationStatus {
        json: bool,
    },
    Stop,
    Run {
        notice: bool,
        owned: bool,
    },
}
fn parse(args: &[String]) -> Result<Command, ()> {
    let words: Vec<_> = args.iter().map(String::as_str).collect();
    match words.as_slice() {
        [] => Ok(Command::Tui),
        ["init"] => Ok(Command::Initialize),
        ["project", "add", path] => Ok(Command::ProjectAdd((*path).into())),
        ["project", "list"] => Ok(Command::ProjectList),
        ["sensors", project] => Ok(Command::Sensors {
            project: sensors_cli::parse_id(project)?,
            offset: 0,
            revision: None,
        }),
        ["sensors", project, offset, revision] => {
            let offset = offset.parse::<u32>().map_err(|_| ())?;
            if offset > 64 {
                return Err(());
            }
            Ok(Command::Sensors {
                project: sensors_cli::parse_id(project)?,
                offset,
                revision: Some(revision.parse().map_err(|_| ())?),
            })
        }
        ["--help" | "-h"] => Ok(Command::Help),
        ["service", "start"] => Ok(Command::Start),
        ["service", "status", "--json"] => Ok(Command::Status),
        ["installation", "status"] => Ok(Command::InstallationStatus { json: false }),
        ["installation", "status", "--json"] => Ok(Command::InstallationStatus { json: true }),
        ["service", "stop"] => Ok(Command::Stop),
        ["service", "run"] => Ok(Command::Run {
            notice: false,
            owned: false,
        }),
        ["service", "run", "--internal-startup-notice"] => Ok(Command::Run {
            notice: true,
            owned: false,
        }),
        [
            "service",
            "run",
            "--internal-startup-notice",
            "--internal-owner-lifetime",
        ] => Ok(Command::Run {
            notice: true,
            owned: true,
        }),
        _ => Err(()),
    }
}
fn empty_status(service: &str, error: Option<&str>) -> Value {
    json!({"schema_version": 1, "service": service, "service_epoch": null,
        "service_build": null, "lifecycle": null, "installation": null,
        "unavailable_reason": null, "error_code": error})
}
pub(crate) fn lifecycle_name(lifecycle: i32) -> &'static str {
    match lifecycle {
        1 => "starting",
        2 => "serving",
        3 => "draining",
        4 => "faulted",
        5 => "repair_only",
        _ => unreachable!("the control owner validated the lifecycle"),
    }
}
fn snapshot_status(snapshot: &asura_client::Snapshot) -> Value {
    let epoch: String = snapshot
        .service_epoch
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let lifecycle = lifecycle_name(snapshot.lifecycle);
    json!({"schema_version": 1, "service": "current", "service_epoch": epoch,
        "service_build": snapshot.service_build, "lifecycle": lifecycle,
        "installation": installation_name(snapshot.installation), "unavailable_reason": reason_name(snapshot.reason), "error_code": null})
}
pub(crate) fn installation_name(state: i32) -> &'static str {
    match state {
        1 => "unavailable",
        2 => "uninitialized",
        3 => "recovering",
        4 => "graph_unavailable",
        5 => "repair_required",
        6 => "graph_ready",
        _ => unreachable!("the control owner validated installation state"),
    }
}
pub(crate) fn reason_name(reason: i32) -> &'static str {
    match reason {
        1 => "inspection_pending",
        2 => "runtime_only",
        3 => "initialization_pending",
        4 => "graph_verification_unavailable",
        5 => "installation_remnants",
        6 => "unknown_content",
        7 => "unsupported_layout",
        8 => "unsupported_format",
        9 => "incomplete_tail",
        10 => "corrupt_authority",
        11 => "inspection_limit",
        12 => "inspection_timeout",
        13 => "unsafe_authority",
        14 => "authority_changed",
        15 => "inspection_io",
        16 => "graph_verified",
        _ => unreachable!("the control owner validated inspection reason"),
    }
}
fn hex_id(id: &[u8; 16]) -> String {
    id.iter().map(|byte| format!("{byte:02x}")).collect()
}
fn empty_installation(reason: &str) -> Value {
    json!({"schema_version": 1, "service_epoch": null, "installation": "unavailable",
        "reason": reason, "installation_id": null, "authority_revision": null,
        "recorded_owner_generation": null, "binding_generation": null,
        "authority_format": null, "error_code": reason})
}
fn installation_value(snapshot: &asura_client::InstallationSnapshot) -> Value {
    json!({"schema_version": 1, "service_epoch": hex_id(&snapshot.service_epoch),
        "installation": installation_name(snapshot.installation as i32),
        "reason": reason_name(snapshot.reason as i32),
        "installation_id": snapshot.installation_id.as_ref().map(hex_id),
        "authority_revision": snapshot.authority_revision,
        "recorded_owner_generation": snapshot.recorded_owner_generation,
        "binding_generation": snapshot.binding_generation,
        "authority_format": snapshot.authority_format, "error_code": null})
}
fn show_installation(value: &Value, json: bool) {
    if json {
        println!("{value}");
    } else {
        tracing::info!(
            "Installation: {} ({})",
            value["installation"].as_str().unwrap(),
            value["reason"].as_str().unwrap()
        );
    }
}
fn installation_status(resolve: RuntimeResolver, json: bool) -> i32 {
    let result = (|| {
        let runtime = resolve(false).map_err(ClientError::from)?;
        Client::attach(&runtime, BUILD, Instant::now() + Duration::from_secs(2))?
            .inspect_installation()
    })();
    match result {
        Ok(snapshot) => {
            show_installation(&installation_value(&snapshot), json);
            0
        }
        Err(error) => {
            let (code, reason) = if asura_client::is_absent(&error) {
                (3, "service_absent")
            } else {
                let (code, _, reason) = classify(&error);
                (code, reason)
            };
            show_installation(&empty_installation(reason), json);
            tracing::error!(error_code = reason, "installation_error");
            code
        }
    }
}
pub(crate) fn classify(error: &ClientError) -> (i32, &'static str, &'static str) {
    match error {
        ClientError::Platform(PlatformError::UnsafeRuntime) => (5, "unavailable", "unsafe_runtime"),
        ClientError::Incompatible => (4, "incompatible", "incompatible_protocol"),
        ClientError::OutcomeUnconfirmed => (6, "unavailable", "outcome_unconfirmed"),
        _ => (3, "unavailable", "service_unavailable"),
    }
}
fn status(resolve: RuntimeResolver) -> i32 {
    let result = (|| {
        let runtime = resolve(false).map_err(ClientError::from)?;
        Client::attach(&runtime, BUILD, Instant::now() + Duration::from_secs(2))?.inspect()
    })();
    match result {
        Ok(snapshot) => {
            tracing::info!("service_status: Asura service is running");
            println!("{}", snapshot_status(&snapshot));
            0
        }
        Err(error) if asura_client::is_absent(&error) => {
            tracing::info!("service_absent: Asura service is absent");
            println!("{}", empty_status("absent", None));
            0
        }
        Err(error) => {
            let (code, service, reason) = classify(&error);
            println!("{}", empty_status(service, Some(reason)));
            tracing::error!(error_code = reason, "service_error");
            code
        }
    }
}
fn lifecycle(start: bool, log: Option<&std::fs::File>, resolve: RuntimeResolver) -> i32 {
    let result = (|| {
        let runtime = resolve(start).map_err(ClientError::from)?;
        if start {
            let (mut client, spawned) = asura_client::start(&runtime, BUILD, log)?;
            client.inspect()?;
            if spawned {
                tracing::info!("service_started: Asura service is running");
            } else {
                tracing::info!(
                    sink_unchanged = true,
                    "service_already_running: Asura service is running"
                );
            }
        } else {
            let client = Client::attach(&runtime, BUILD, Instant::now() + Duration::from_secs(2))?;
            client.stop()?;
            tracing::info!("service_stopped: Asura service stopped");
        }
        Ok::<_, ClientError>(())
    })();
    match result {
        Ok(()) => 0,
        Err(error) if !start && asura_client::is_absent(&error) => {
            tracing::info!("service_absent: Asura service is absent");
            0
        }
        Err(error) => {
            let (code, _, reason) = classify(&error);
            tracing::error!(error_code = reason, "service_error");
            code
        }
    }
}
fn run(notice: bool, owned: bool, resolve: RuntimeResolver) -> i32 {
    // Validate the private pipe before opening or creating account runtime state.
    let pipe = if notice {
        match StartupPipe::inherited() {
            Ok(pipe) => Some(pipe),
            Err(_) => {
                tracing::error!(error_code = "invalid_startup_notice", "service_error");
                return 3;
            }
        }
    } else {
        None
    };
    let lifetime = if owned {
        match asura_platform::ServiceLifetime::inherited() {
            Ok(lifetime) => Some(lifetime),
            Err(_) => {
                tracing::error!(error_code = "invalid_owner_lifetime", "service_error");
                return 3;
            }
        }
    } else {
        None
    };
    let runtime = match resolve(true) {
        Ok(runtime) => runtime,
        Err(error) => {
            let unsafe_runtime = matches!(error, PlatformError::UnsafeRuntime);
            if let Some(pipe) = pipe {
                let _ = pipe.send(if unsafe_runtime {
                    asura_platform::StartupNotice::UnsafeRuntime
                } else {
                    asura_platform::StartupNotice::Unavailable
                });
            }
            tracing::error!(
                error_code = if unsafe_runtime {
                    "unsafe_runtime"
                } else {
                    "service_unavailable"
                },
                "service_error"
            );
            return if unsafe_runtime { 5 } else { 3 };
        }
    };
    let result = match lifetime {
        Some(lifetime) => asura_service::run_owned(runtime, BUILD, pipe, lifetime),
        None => asura_service::run(runtime, BUILD, pipe),
    };
    match result {
        Ok(()) => 0,
        Err(_) => {
            tracing::error!(error_code = "service_unavailable", "service_error");
            3
        }
    }
}
fn logging_options(args: &[String]) -> Result<(Command, Option<std::path::PathBuf>), ()> {
    let mut words = Vec::new();
    let mut logs = None;
    let mut iter = args.iter();
    while let Some(word) = iter.next() {
        if word == "--logs" {
            let value = iter.next().ok_or(())?;
            if logs.is_some() || value.is_empty() || value.starts_with("--") {
                return Err(());
            }
            logs = Some(std::path::PathBuf::from(value));
        } else {
            words.push(word.clone());
        }
    }
    let command = parse(&words)?;
    if logs.is_some()
        && matches!(
            command,
            Command::Help | Command::Tui | Command::Run { notice: true, .. }
        )
    {
        return Err(());
    }
    Ok((command, logs))
}
fn install_logger(file: Option<&std::fs::File>) -> Result<(), ()> {
    use tracing_subscriber::fmt::writer::BoxMakeWriter;
    use tracing_subscriber::prelude::*;
    let writer = match file {
        Some(file) => BoxMakeWriter::new(std::sync::Mutex::new(file.try_clone().map_err(|_| ())?)),
        None => BoxMakeWriter::new(std::io::stderr),
    };
    let formatting = tracing_subscriber::fmt::layer()
        .with_writer(writer)
        .with_ansi(false)
        .with_target(false)
        .log_internal_errors(false)
        .with_filter(tracing_subscriber::filter::filter_fn(|metadata| {
            *metadata.level() <= tracing::Level::INFO && metadata.target().starts_with("asura_")
        }));
    tracing_subscriber::registry()
        .with(formatting)
        .try_init()
        .map_err(|_| ())
}
pub type RuntimeResolver = fn(bool) -> asura_platform::Result<RuntimeDirectory>;

fn report_tui_error(writer: &mut impl std::io::Write, error: &std::io::Error) {
    // A terminal failure can also leave stderr unavailable. Reporting must not
    // panic and obscure the original error or its exit status.
    let _ = writeln!(writer, "{error}");
}

pub fn entry(resolve: RuntimeResolver) -> i32 {
    entry_with_logs(resolve, asura_storage::logs::open_account_log)
}

pub fn entry_with_logs(
    resolve: RuntimeResolver,
    log_resolve: fn() -> asura_platform::Result<std::fs::File>,
) -> i32 {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.first().is_some_and(|a| a == "--asura-shell-guardian") {
        return match args.as_slice() {
            [_, job, deadline, command] => {
                asura_platform::shell::run_guardian(job, deadline, command)
            }
            _ => 2,
        };
    }
    let parsed = logging_options(&args);
    if matches!(parsed, Ok((Command::Tui, _))) {
        return match crate::tui::run(resolve, log_resolve) {
            Ok(()) => 0,
            Err(error) => {
                report_tui_error(&mut std::io::stderr(), &error);
                if error.kind() == std::io::ErrorKind::InvalidInput {
                    2
                } else {
                    3
                }
            }
        };
    }
    let mut log = None;
    if let Ok((command, path)) = &parsed {
        let setup = (|| {
            if let Some(path) = path {
                log = Some(asura_storage::logs::open_log(path).map_err(|_| ())?);
            }
            install_logger(log.as_ref())
        })();
        if setup.is_err() {
            eprintln!("ERROR log_unavailable");
            if *command == Command::Status {
                println!("{}", empty_status("unavailable", Some("log_unavailable")));
            }
            if *command == (Command::InstallationStatus { json: true }) {
                println!("{}", empty_installation("log_unavailable"));
            }
            return 3;
        }
    } else {
        let _ = install_logger(None);
    }
    match parsed.map(|(command, _)| command) {
        Ok(Command::Help) => {
            println!(
                "Asura\nUsage:\n  asura (interactive TUI)\n  asura service start [--logs DIR]\n  asura service status --json [--logs DIR]\n  asura service stop [--logs DIR]\n  asura service run [--logs DIR]\n  asura installation status [--json] [--logs DIR]\n  asura init\n  asura project add PATH\n  asura project list\n  asura sensors PROJECT_ID [OFFSET REVISION]"
            );
            0
        }
        Ok(Command::Tui) => unreachable!("interactive command handled before logger setup"),
        Ok(Command::Initialize) => setup(
            crate::conversation::Command::Initialize(asura_platform::random_id()),
            resolve,
        ),
        Ok(Command::ProjectAdd(path)) => match crate::conversation::location(&path) {
            Ok(path) => setup(
                crate::conversation::Command::Register(asura_platform::random_id(), path),
                resolve,
            ),
            Err(error) => {
                tracing::error!("{error}");
                2
            }
        },
        Ok(Command::Sensors {
            project,
            offset,
            revision,
        }) => sensors_cli::run(resolve, project, offset, revision),
        Ok(Command::ProjectList) => setup(crate::conversation::Command::List, resolve),
        Ok(Command::Start) => lifecycle(true, log.as_ref(), resolve),
        Ok(Command::Status) => status(resolve),
        Ok(Command::InstallationStatus { json }) => installation_status(resolve, json),
        Ok(Command::Stop) => lifecycle(false, log.as_ref(), resolve),
        Ok(Command::Run { notice, owned }) => run(notice, owned, resolve),
        Err(()) => {
            tracing::error!(error_code = "invalid_arguments", "Use --help");
            2
        }
    }
}

#[cfg(test)]
mod tests;

fn setup(command: crate::conversation::Command, resolve: RuntimeResolver) -> i32 {
    let result = (|| {
        let runtime = resolve(false).map_err(|e| e.to_string())?;
        let mut client = Client::attach(&runtime, BUILD, Instant::now() + Duration::from_secs(2))
            .map_err(|e| e.to_string())?;
        crate::conversation::execute(&mut client, command)
    })();
    match result {
        Ok((message, _)) => {
            println!("{message}");
            0
        }
        Err(error) => {
            tracing::error!("{error}");
            3
        }
    }
}
