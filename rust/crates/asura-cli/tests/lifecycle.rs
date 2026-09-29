//! Executes the real CLI application with only the runtime resolver changed.
use asura_cli::app;
mod context_status;
mod installation_status;
mod sensors_inspection;

use asura_client::Client;
use asura_platform::RuntimeDirectory;
use std::error::Error;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

type Result<T> = std::result::Result<T, Box<dyn Error>>;
const BUILD: &str = concat!("asura/", env!("CARGO_PKG_VERSION"));
const WAIT: Duration = Duration::from_secs(8);
fn require(condition: bool, message: &str) -> Result<()> {
    if condition {
        Ok(())
    } else {
        Err(message.into())
    }
}
fn resolver(create: bool) -> asura_platform::Result<RuntimeDirectory> {
    let executable = std::env::current_exe()?;
    let parent = executable
        .parent()
        .ok_or(asura_platform::Error::UnsafeRuntime)?;
    RuntimeDirectory::scratch(&parent.join("home"), create)
}
fn log_resolver() -> asura_platform::Result<File> {
    let executable = std::env::current_exe()?;
    let parent = executable
        .parent()
        .ok_or(asura_platform::Error::UnsafeRuntime)?;
    asura_storage::logs::open_log(&parent.join("home/.asura/logs"))
}
fn read(path: &Path) -> Result<String> {
    let mut bytes = Vec::new();
    File::open(path)?
        .take(128 * 1024 + 1)
        .read_to_end(&mut bytes)?;
    require(bytes.len() <= 128 * 1024, "fixture output bound exceeded")?;
    Ok(String::from_utf8(bytes)?)
}
fn wait(child: &mut Child, end: Instant) -> Result<ExitStatus> {
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        if Instant::now() >= end {
            return Err("owned command exceeded test deadline".into());
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}
struct Output {
    status: ExitStatus,
    stdout: String,
    stderr: String,
    stderr_path: PathBuf,
}
struct Case {
    root: PathBuf,
    executable: PathBuf,
    foreground: Option<Child>,
    commands: u32,
    uncertain_start: bool,
}
impl Case {
    fn new() -> Result<Self> {
        let suffix = u64::from_ne_bytes(asura_platform::random_id()[..8].try_into()?);
        let root = PathBuf::from(format!("/private/tmp/asura-cli-{suffix:x}"));
        fs::DirBuilder::new().mode(0o700).create(&root)?;
        fs::DirBuilder::new()
            .mode(0o700)
            .create(root.join("home"))?;
        let executable = root.join("asura-fixture");
        fs::copy(std::env::current_exe()?, &executable)?;
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700))?;
        Ok(Self {
            root,
            executable,
            foreground: None,
            commands: 0,
            uncertain_start: false,
        })
    }
    fn command(&mut self, args: &[&str]) -> Result<Output> {
        self.commands += 1;
        let stdout = self.root.join(format!("command-{}.stdout", self.commands));
        let stderr = self.root.join(format!("command-{}.stderr", self.commands));
        // Regular files avoid treating inherited detached stderr EOF as process completion.
        let mut child = Command::new(&self.executable)
            .args(args)
            .current_dir(&self.root)
            .stdin(Stdio::null())
            .stdout(File::create(&stdout)?)
            .stderr(File::create(&stderr)?)
            .spawn()?;
        let starting = args.get(1) == Some(&"start");
        if starting {
            self.uncertain_start = true;
        }
        let status = match wait(&mut child, Instant::now() + WAIT) {
            Ok(status) => status,
            Err(error) => {
                // Only this retained direct command is signalled; never its detached service.
                child.kill()?;
                wait(&mut child, Instant::now() + Duration::from_secs(2))?;
                return Err(error);
            }
        };
        let stdout_text = read(&stdout)?;
        let stderr_text = read(&stderr)?;
        // Success proves authenticated attachment. This one setup rejection is
        // explicitly before runtime lookup/spawn; other failures stay uncertain.
        if starting
            && (status.success()
                || (status.code() == Some(3) && stderr_text == "ERROR log_unavailable\n"))
        {
            self.uncertain_start = false;
        }
        Ok(Output {
            status,
            stdout: stdout_text,
            stderr: stderr_text,
            stderr_path: stderr,
        })
    }
    fn status(&mut self) -> Result<serde_json::Value> {
        let output = self.command(&["service", "status", "--json", "--logs", "status-logs"])?;
        require(
            output.status.success(),
            &format!("status failed: {}", output.stderr),
        )?;
        require(output.stderr.is_empty(), "file logging leaked to stderr")?;
        let value = serde_json::from_str(&output.stdout)?;
        let logs = read(&self.root.join("status-logs/asura.log"))?;
        require(
            logs.contains("INFO service_status") || logs.contains("INFO service_absent"),
            "status event absent",
        )?;
        Ok(value)
    }
    fn wait_serving(&mut self) -> Result<()> {
        let end = Instant::now() + WAIT;
        loop {
            if let Some(child) = &mut self.foreground {
                require(
                    child.try_wait()?.is_none(),
                    "foreground service exited before serving",
                )?;
            }
            if let Ok(runtime) = RuntimeDirectory::scratch(&self.root.join("home"), false)
                && let Ok(mut client) = Client::attach(
                    &runtime,
                    BUILD,
                    end.min(Instant::now() + Duration::from_millis(500)),
                )
                && client.inspect().is_ok()
            {
                return Ok(());
            }
            require(Instant::now() < end, "service did not become ready")?;
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    fn interrupt_foreground(&mut self) -> Result<()> {
        let child = self.foreground.as_mut().ok_or("missing foreground child")?;
        require(child.try_wait()?.is_none(), "foreground already exited")?;
        // SAFETY: this is a retained direct child, just observed unreaped. Its PID
        // cannot be reused before this controller reaps it. Test-only signal injection.
        if unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGINT) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        require(
            wait(child, Instant::now() + WAIT)?.success(),
            "foreground drain failed",
        )?;
        self.foreground = None;
        Ok(())
    }
    fn cleanup(&mut self) -> Result<()> {
        match RuntimeDirectory::scratch(&self.root.join("home"), false) {
            Ok(runtime) => match runtime.capture_lock() {
                Ok(witness) => {
                    if !witness.owner_released()? {
                        match Client::attach(
                            &runtime,
                            BUILD,
                            Instant::now() + Duration::from_secs(2),
                        ) {
                            Ok(client) => {
                                client.stop()?;
                            }
                            Err(error) => {
                                if self.foreground.is_some() {
                                    self.interrupt_foreground()?;
                                } else {
                                    return Err(error.into());
                                }
                            }
                        }
                    }
                    let end = Instant::now() + WAIT;
                    while !witness.owner_released()? {
                        require(Instant::now() < end, "scratch owner release unconfirmed")?;
                        std::thread::sleep(Duration::from_millis(20));
                    }
                }
                Err(asura_platform::Error::Absent) => (),
                Err(error) => return Err(error.into()),
            },
            Err(asura_platform::Error::Absent) => (),
            Err(error) => return Err(error.into()),
        }
        if let Some(child) = &mut self.foreground {
            require(
                wait(child, Instant::now() + WAIT)?.success(),
                "foreground cleanup failed",
            )?;
            self.foreground = None;
        }
        require(
            !self.uncertain_start,
            "detached startup outcome unconfirmed; future ownership remains possible",
        )?;
        fs::remove_dir_all(&self.root)?;
        Ok(())
    }
}
fn lifecycle_logs(path: &Path) -> Result<()> {
    let end = Instant::now() + Duration::from_secs(2);
    let text = loop {
        let text = read(path)?;
        if text
            .lines()
            .any(|line| line.ends_with("INFO service_stopped"))
        {
            break text;
        }
        require(Instant::now() < end, "service stopped event missing")?;
        std::thread::sleep(Duration::from_millis(10));
    };
    let serving = text
        .find("INFO service_serving")
        .ok_or("serving event missing")?;
    let draining = text
        .find("INFO service_draining")
        .ok_or("draining event missing")?;
    let stopped = text
        .find("INFO service_stopped")
        .ok_or("stopped event missing")?;
    require(
        serving < draining && draining < stopped,
        "lifecycle event ordering failed",
    )?;
    for line in text.lines().filter(|line| *line != "prior") {
        let timestamp = line.split_whitespace().next().ok_or("timestamp absent")?;
        require(
            timestamp.contains('T') && timestamp.ends_with('Z'),
            "UTC timestamp absent",
        )?;
        require(line.contains(" INFO "), "INFO level absent")?;
        require(!line.contains('\u{1b}'), "ANSI present in logs")?;
        require(!line.contains("/private/tmp"), "raw path disclosed in logs")?;
    }
    Ok(())
}
fn foreground(case: &mut Case, file: bool) -> Result<()> {
    let stderr = case.root.join("foreground.stderr");
    let stdout = case.root.join("foreground.stdout");
    let mut command = Command::new(&case.executable);
    command.args(["service", "run"]);
    if file {
        let mut log = asura_storage::logs::open_log(&case.root.join("logs"))?;
        log.write_all(b"prior\n")?;
        command.args(["--logs", "logs"]);
    }
    case.foreground = Some(
        command
            .current_dir(&case.root)
            .stdin(Stdio::null())
            .stdout(File::create(&stdout)?)
            .stderr(File::create(&stderr)?)
            .spawn()?,
    );
    case.wait_serving()?;
    let status = case.status()?;
    require(
        status["service"] == "current",
        "foreground status not current",
    )?;
    case.interrupt_foreground()?;
    require(
        case.status()?["service"] == "absent",
        "foreground remained running",
    )?;
    require(read(&stdout)?.is_empty(), "foreground logs polluted stdout")?;
    if file {
        require(read(&stderr)?.is_empty(), "selected file leaked to stderr")?;
        let log = case.root.join("logs/asura.log");
        require(read(&log)?.starts_with("prior\n"), "log was truncated")?;
        lifecycle_logs(&log)
    } else {
        lifecycle_logs(&stderr)
    }
}
fn detached(case: &mut Case, file: bool) -> Result<()> {
    let first = if file {
        let mut log = asura_storage::logs::open_log(&case.root.join("logs"))?;
        log.write_all(b"prior\n")?;
        case.command(&["service", "start", "--logs", "logs"])?
    } else {
        case.command(&["service", "start"])?
    };
    require(
        first.status.success(),
        &format!("detached start failed: {}", first.stderr),
    )?;
    require(first.stdout.is_empty(), "detached logs polluted stdout")?;
    let initial = case.status()?;
    require(
        initial["service"] == "current",
        "detached status not current",
    )?;
    let second = case.command(&["service", "start", "--logs", "other-logs"])?;
    require(second.status.success(), "repeat start failed")?;
    require(
        second.stdout.is_empty() && second.stderr.is_empty(),
        "file logging polluted standard output",
    )?;
    let attached = read(&case.root.join("other-logs/asura.log"))?;
    require(
        attached.contains("service_already_running") && attached.contains("sink_unchanged=true"),
        "repeat start omitted unchanged-sink event",
    )?;
    require(
        case.status()?["service_epoch"] == initial["service_epoch"],
        "repeat start changed owner epoch",
    )?;
    let stop = case.command(&["service", "stop", "--logs", "stop-logs"])?;
    require(stop.status.success(), "detached stop failed")?;
    require(
        case.status()?["service"] == "absent",
        "detached owner survived stop",
    )?;
    let daemon_log = if file {
        case.root.join("logs/asura.log")
    } else {
        first.stderr_path
    };
    lifecycle_logs(&daemon_log)?;
    let unchanged = read(&case.root.join("other-logs/asura.log"))?;
    require(
        !unchanged.contains("service_serving")
            && !unchanged.contains("service_draining")
            && !unchanged.contains("service_stopped"),
        "existing daemon sink was reconfigured",
    )?;
    if file {
        require(
            read(&daemon_log)?.starts_with("prior\n"),
            "detached log was truncated",
        )?;
    }
    Ok(())
}
fn unsafe_sink(case: &mut Case) -> Result<()> {
    fs::DirBuilder::new()
        .mode(0o700)
        .create(case.root.join("bad-logs"))?;
    fs::write(case.root.join("preserve"), b"preserve")?;
    symlink(
        case.root.join("preserve"),
        case.root.join("bad-logs/asura.log"),
    )?;
    for args in [
        vec!["service", "start", "--logs", "bad-logs"],
        vec!["service", "run", "--logs", "bad-logs"],
        vec!["service", "stop", "--logs", "bad-logs"],
        vec!["service", "status", "--json", "--logs", "bad-logs"],
    ] {
        let output = case.command(&args)?;
        require(
            output.status.code() == Some(3),
            "unsafe sink exit code not 3",
        )?;
        require(
            output.stderr == "ERROR log_unavailable\n",
            "unsafe sink diagnostic changed",
        )?;
        if args.contains(&"status") {
            let json: serde_json::Value = serde_json::from_str(&output.stdout)?;
            require(
                json["error_code"] == "log_unavailable" && json["service"] == "unavailable",
                "setup failure JSON incorrect",
            )?;
        } else {
            require(output.stdout.is_empty(), "setup failure stdout polluted")?;
        }
        require(
            !case.root.join("home/.asura").exists(),
            "unsafe sink mutated runtime",
        )?;
        require(
            fs::read(case.root.join("preserve"))? == b"preserve",
            "unsafe log target modified",
        )?;
    }
    Ok(())
}
fn terminal_regressions() -> Result<()> {
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/tui_pty.py");
    for selector in [
        None,
        Some("--setup"),
        Some("--queue-history"),
        Some("--queue-reorder"),
    ] {
        let mut command = Command::new("python3");
        command.arg(&script).arg(std::env::current_exe()?);
        if let Some(selector) = selector {
            command.arg(selector);
        }
        let mut child = command.spawn()?;
        let status = match wait(&mut child, Instant::now() + Duration::from_secs(75)) {
            Ok(status) => status,
            Err(error) => {
                // SAFETY: this is our retained, unreaped direct child. Python's
                // SIGTERM handler enters its fixture cleanup path.
                unsafe { libc::kill(child.id() as libc::pid_t, libc::SIGTERM) };
                if wait(&mut child, Instant::now() + Duration::from_secs(10)).is_err() {
                    child.kill()?;
                    wait(&mut child, Instant::now() + Duration::from_secs(2))?;
                }
                return Err(format!(
                    "terminal regression {selector:?}: {error}; cleanup unconfirmed"
                )
                .into());
            }
        };
        require(
            status.success(),
            &format!("terminal regression {selector:?} failed: {status}"),
        )?;
    }
    Ok(())
}
fn suite() -> Result<()> {
    for (name, execute) in [
        (
            "foreground-stderr",
            (|case: &mut Case| foreground(case, false)) as fn(&mut Case) -> Result<()>,
        ),
        ("foreground-file", |case| foreground(case, true)),
        ("detached-stderr", |case| detached(case, false)),
        ("detached-file", |case| detached(case, true)),
        ("unsafe-sink", unsafe_sink),
        ("context-observations", context_status::observations),
        ("sensor-inspection", sensors_inspection::inspect),
        ("installation-absent", installation_status::absent),
        (
            "installation-incompatible",
            installation_status::incompatible,
        ),
        ("installation-runtime", installation_status::runtime),
        ("installation-remnants", installation_status::remnants),
        ("installation-pending", installation_status::pending),
        ("installation-active", installation_status::active),
        ("installation-corrupt", installation_status::corrupt),
        ("installation-incomplete", installation_status::incomplete),
        ("installation-version", installation_status::version),
        ("installation-log-failure", installation_status::log_failure),
    ] {
        let mut case = Case::new()?;
        let result = execute(&mut case);
        let cleanup = case.cleanup();
        if let Err(error) = cleanup {
            return Err(format!(
                "{name}: cleanup failed: {error}; original={result:?}; retained={}",
                case.root.display()
            )
            .into());
        }
        result.map_err(|error| format!("{name}: {error}"))?;
        println!("PASS isolated CLI {name}");
    }
    terminal_regressions()?;
    Ok(())
}
// Fixture-only canonical journal seeding. This branch exists only in the copied
// integration executable; production CLI has no runtime or journal override.
fn queue_history_fixture(verify: bool) -> Result<()> {
    use asura_storage::authority::{conversation as j, writer as w};
    fn execute(writer: &w::WriterHandle, command: w::Command) -> Result<w::Reply> {
        let mut ticket = writer
            .try_submit(command)
            .map_err(|e| format!("fixture submit:{e:?}"))?;
        let end = Instant::now() + Duration::from_secs(12);
        loop {
            if let Some(result) = ticket.poll() {
                return result.map_err(|e| format!("fixture writer:{e:?}").into());
            }
            require(Instant::now() < end, "fixture writer deadline")?;
            std::thread::sleep(Duration::from_millis(2));
        }
    }
    fn append(
        writer: &w::WriterHandle,
        revision: &mut u64,
        record: j::Record,
        admission: Option<w::AdmissionFence>,
    ) -> Result<()> {
        *revision = execute(
            writer,
            w::Command::Append {
                expected_revision: *revision,
                record,
                admission,
            },
        )?
        .replay
        .ok_or("missing replay")?
        .revision;
        Ok(())
    }
    let runtime = resolver(true)?;
    let mut writer = w::WriterHandle::start(runtime).map_err(|e| format!("fixture start:{e:?}"))?;
    let result = (|| {
        if verify {
            let state = execute(&writer, w::Command::Open)?
                .replay
                .ok_or("missing replay")?;
            let expected: usize = std::env::args().nth(2).ok_or("expected count")?.parse()?;
            require(
                state.operations.len() == 4 + expected,
                "restored input was not durably admitted",
            )?;
            require(
                state
                    .conversations
                    .get(&[20; 16])
                    .is_some_and(|c| c.generation == 4 + expected as u64),
                "restored conversation did not advance",
            )?;
            let mut restored = 0;
            for operation in state.operations.values() {
                let record = execute(
                    &writer,
                    w::Command::ReadRecord {
                        range: operation.accepted_frame.clone(),
                    },
                )?
                .record
                .ok_or("missing accepted record")?;
                let j::Record::TurnAccepted(turn) = record else {
                    return Err("wrong record".into());
                };
                if let Some(index) = turn.prompt.strip_prefix("queue-history-admission-") {
                    let index: u64 = index.parse()?;
                    require(
                        turn.original_conversation == Some([20; 16])
                            && turn.expected_generation == 3 + index
                            && turn.generation == 4 + index,
                        "fresh TUI did not advance restored cursor",
                    )?;
                    restored += 1;
                }
            }
            require(restored == expected, "restored admission count")?;
            return Ok(());
        }
        execute(&writer, w::Command::Initialize { request: [1; 16] })?;
        let project_path = std::env::current_exe()?
            .parent()
            .ok_or("fixture parent")?
            .join("project");
        fs::DirBuilder::new().mode(0o700).create(&project_path)?;
        let registered = execute(
            &writer,
            w::Command::Register {
                request: [2; 16],
                location: project_path.to_str().ok_or("project UTF8")?.into(),
            },
        )?
        .replay
        .ok_or("missing registration")?;
        let project = *registered.projects.keys().next().ok_or("missing project")?;
        let mut revision = registered.revision;
        let mut dispatch = None;
        for generation in 1u8..=4 {
            let conversation = if generation == 1 {
                None
            } else {
                Some([20; 16])
            };
            let prompt = if generation == 3 {
                "queued fixture input"
            } else {
                "direct fixture input"
            };
            let prepared = execute(
                &writer,
                w::Command::Prepare {
                    project,
                    conversation,
                    expected_generation: u64::from(generation - 1),
                    prompt: prompt.into(),
                },
            )?
            .prepared
            .ok_or("missing prepare")?;
            let operation = [40 + generation; 16];
            let turn = j::TurnAccepted {
                request: if generation == 3 {
                    dispatch.ok_or("missing dispatch")?
                } else {
                    [30 + generation; 16]
                },
                digest: j::request_digest(j::Request::Submit {
                    project,
                    conversation,
                    expected_generation: u64::from(generation - 1),
                    prompt,
                })
                .map_err(|e| format!("digest:{e:?}"))?,
                project,
                original_conversation: conversation,
                expected_generation: u64::from(generation - 1),
                conversation: [20; 16],
                generation: u64::from(generation),
                task: [50 + generation; 16],
                operation,
                model: "system".into(),
                configuration_digest: prepared.configuration_digest,
                instructions_digest: [60; 32],
                input_digest: [61; 32],
                prior_operations: vec![],
                prompt: prompt.into(),
                reserved_output_tokens: 512,
                event_cursor: 1,
            };
            append(
                &writer,
                &mut revision,
                j::Record::TurnAccepted(Box::new(turn)),
                Some(w::AdmissionFence {
                    project,
                    configuration_digest: prepared.configuration_digest,
                }),
            )?;
            append(
                &writer,
                &mut revision,
                j::Record::StartAuthorized(j::StartAuthorized {
                    operation,
                    generation: u64::from(generation),
                    helper: [70 + generation; 16],
                    input_digest: [61; 32],
                }),
                None,
            )?;
            if generation == 2 {
                let queued = execute(
                    &writer,
                    w::Command::Enqueue {
                        request: [80; 16],
                        project,
                        conversation: [20; 16],
                        target_operation: operation,
                        target_generation: 2,
                        kind: j::InputKind::Queue,
                        prompt: "queued fixture input".into(),
                    },
                )?
                .replay
                .ok_or("missing queued replay")?;
                revision = queued.revision;
                dispatch = Some(
                    queued
                        .inputs
                        .get(&[80; 16])
                        .ok_or("missing queued row")?
                        .dispatch_request,
                );
            }
            append(
                &writer,
                &mut revision,
                j::Record::TurnTerminal(j::TurnTerminal {
                    operation,
                    generation: u64::from(generation),
                    kind: j::TerminalKind::Complete,
                    cause: j::Cause::None,
                    final_cursor: u64::MAX,
                    usage_known: true,
                    output_tokens: 1,
                    charged_tokens: 1,
                    text: "fixture completed".into(),
                }),
                None,
            )?;
        }
        let state = execute(&writer, w::Command::Open)?
            .replay
            .ok_or("missing seeded replay")?;
        require(
            state.input_status([80; 16]) == Some(j::InputStatus::Complete),
            "queued seed not complete",
        )?;
        require(
            state
                .conversations
                .get(&[20; 16])
                .is_some_and(|c| c.generation == 4),
            "seed generation",
        )
    })();
    writer.close();
    let end = Instant::now() + Duration::from_secs(12);
    while !writer.settled() {
        require(Instant::now() < end, "fixture writer cleanup deadline")?;
        std::thread::sleep(Duration::from_millis(2));
    }
    result
}

// Seed two accepted but held inputs in the fixture's private journal. A failed
// predecessor keeps the scheduler from racing the TUI's reorder interaction.
fn queue_reorder_fixture(verify: bool) -> Result<()> {
    use asura_storage::authority::{conversation as j, writer as w};
    fn execute(writer: &w::WriterHandle, command: w::Command) -> Result<w::Reply> {
        let mut ticket = writer
            .try_submit(command)
            .map_err(|e| format!("fixture submit:{e:?}"))?;
        let end = Instant::now() + Duration::from_secs(12);
        loop {
            if let Some(result) = ticket.poll() {
                return result.map_err(|e| format!("fixture writer:{e:?}").into());
            }
            require(Instant::now() < end, "fixture writer deadline")?;
            std::thread::sleep(Duration::from_millis(2));
        }
    }
    let runtime = resolver(true)?;
    let mut writer = w::WriterHandle::start(runtime).map_err(|e| format!("fixture start:{e:?}"))?;
    let result = (|| {
        if verify {
            let state = execute(&writer, w::Command::Open)?
                .replay
                .ok_or("missing replay")?;
            let input = |request| -> Result<j::Id> {
                let result = &state
                    .requests
                    .get(&request)
                    .ok_or("missing queue request")?
                    .result;
                let j::Outcome::QueueInput { input, .. } = result else {
                    return Err("wrong queue request result".into());
                };
                Ok(*input)
            };
            let first_id = input([91; 16])?;
            let second_id = input([92; 16])?;
            let first = state.inputs.get(&first_id).ok_or("missing first input")?;
            let second = state.inputs.get(&second_id).ok_or("missing second input")?;
            require(
                first.v2 && second.v2,
                "fixture inputs are not managed inputs",
            )?;
            require(
                state.input_status(first_id) == Some(j::InputStatus::Held)
                    && state.input_status(second_id) == Some(j::InputStatus::Held),
                "fixture inputs were dispatched or lost",
            )?;
            let moved = std::env::args().nth(2).as_deref() == Some("moved");
            require(
                (first.order_position, second.order_position)
                    == if moved { (2, 1) } else { (1, 2) },
                "durable queue order mismatch",
            )?;
            require(
                state.order_revision == if moved { 3 } else { 2 },
                "queue order revision mismatch",
            )?;
            return Ok(());
        }
        execute(&writer, w::Command::Initialize { request: [1; 16] })?;
        let project_path = std::env::current_exe()?
            .parent()
            .ok_or("fixture parent")?
            .join("project");
        fs::DirBuilder::new().mode(0o700).create(&project_path)?;
        let registered = execute(
            &writer,
            w::Command::Register {
                request: [2; 16],
                location: project_path.to_str().ok_or("project UTF8")?.into(),
            },
        )?
        .replay
        .ok_or("missing registration")?;
        let project = *registered.projects.keys().next().ok_or("missing project")?;
        let prepared = execute(
            &writer,
            w::Command::Prepare {
                project,
                conversation: None,
                expected_generation: 0,
                prompt: "failed fixture predecessor".into(),
            },
        )?
        .prepared
        .ok_or("missing prepare")?;
        let mut revision = registered.revision;
        let mut append = |record, admission| -> Result<()> {
            revision = execute(
                &writer,
                w::Command::Append {
                    expected_revision: revision,
                    record,
                    admission,
                },
            )?
            .replay
            .ok_or("missing replay")?
            .revision;
            Ok(())
        };
        let prompt = "failed fixture predecessor";
        append(
            j::Record::TurnAccepted(Box::new(j::TurnAccepted {
                request: [31; 16],
                digest: j::request_digest(j::Request::Submit {
                    project,
                    conversation: None,
                    expected_generation: 0,
                    prompt,
                })
                .map_err(|e| format!("digest:{e:?}"))?,
                project,
                original_conversation: None,
                expected_generation: 0,
                conversation: [20; 16],
                generation: 1,
                task: [51; 16],
                operation: [41; 16],
                model: "system".into(),
                configuration_digest: prepared.configuration_digest,
                instructions_digest: [60; 32],
                input_digest: [61; 32],
                prior_operations: vec![],
                prompt: prompt.into(),
                reserved_output_tokens: 512,
                event_cursor: 1,
            })),
            Some(w::AdmissionFence {
                project,
                configuration_digest: prepared.configuration_digest,
            }),
        )?;
        append(
            j::Record::StartAuthorized(j::StartAuthorized {
                operation: [41; 16],
                generation: 1,
                helper: [71; 16],
                input_digest: [61; 32],
            }),
            None,
        )?;
        append(
            j::Record::TurnTerminal(j::TurnTerminal {
                operation: [41; 16],
                generation: 1,
                kind: j::TerminalKind::Failed,
                cause: j::Cause::ProviderFailure,
                final_cursor: u64::MAX,
                usage_known: true,
                output_tokens: 0,
                charged_tokens: 0,
                text: "fixture failure".into(),
            }),
            None,
        )?;
        let mut input_ids = Vec::new();
        for (request, prompt) in [
            ([91; 16], "queue-reorder-first"),
            ([92; 16], "queue-reorder-second"),
        ] {
            let reply = execute(
                &writer,
                w::Command::QueueInput {
                    request,
                    project,
                    conversation: Some([20; 16]),
                    expected_generation: 1,
                    new_conversation: false,
                    prompt: prompt.into(),
                },
            )?;
            input_ids.push(reply.accepted_input_id.ok_or("missing accepted input ID")?);
        }
        let state = execute(&writer, w::Command::Open)?
            .replay
            .ok_or("missing seeded replay")?;
        require(
            state.input_status(input_ids[0]) == Some(j::InputStatus::Held)
                && state.input_status(input_ids[1]) == Some(j::InputStatus::Held),
            "seeded inputs not held",
        )
    })();
    writer.close();
    let end = Instant::now() + Duration::from_secs(12);
    while !writer.settled() {
        require(Instant::now() < end, "fixture writer cleanup deadline")?;
        std::thread::sleep(Duration::from_millis(2));
    }
    result
}

fn main() {
    if matches!(
        std::env::args().nth(1).as_deref(),
        Some("--seed-queue-reorder" | "--verify-queue-reorder")
    ) {
        let verify = std::env::args().nth(1).as_deref() == Some("--verify-queue-reorder");
        if let Err(error) = queue_reorder_fixture(verify) {
            eprintln!("queue reorder fixture: {error}");
            std::process::exit(1);
        }
        return;
    }
    if matches!(
        std::env::args().nth(1).as_deref(),
        Some("--seed-queue-history" | "--verify-queue-history")
    ) {
        let verify = std::env::args().nth(1).as_deref() == Some("--verify-queue-history");
        if let Err(error) = queue_history_fixture(verify) {
            eprintln!("queue history fixture: {error}");
            std::process::exit(1);
        }
        return;
    }
    if matches!(
        std::env::args().nth(1).as_deref(),
        Some("service" | "installation")
    ) || std::env::current_exe()
        .ok()
        .and_then(|path| path.file_name().map(|name| name == "asura-fixture"))
        .unwrap_or(false)
    {
        std::process::exit(app::entry_with_logs(resolver, log_resolver));
    }
    if let Err(error) = suite() {
        eprintln!("isolated CLI lifecycle failed: {error}");
        std::process::exit(1);
    }
}
