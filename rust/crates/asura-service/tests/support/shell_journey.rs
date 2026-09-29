//! Native shell evidence uses only an owned scratch project and runtime.
use super::*;
use asura_storage::authority::conversation as journal;
use std::io::Read;

const COMMAND: &str = "printf x >> effect.txt; cat proof.txt; printf stderr-proof >&2";

fn recorded(
    fixture: &Fixture,
    operation: [u8; 16],
) -> Result<(journal::ToolIntent, journal::ToolResult)> {
    let mut bytes = Vec::new();
    fs::File::open(fixture.home.join(".asura/state/control/slot-0.log"))?
        .take(asura_storage::authority::MAX_JOURNAL as u64 + 1)
        .read_to_end(&mut bytes)?;
    let replay = journal::replay(
        &bytes,
        Instant::now() + Duration::from_secs(2),
        &std::sync::atomic::AtomicBool::new(false),
    )
    .map_err(|error| format!("shell replay: {error:?}"))?;
    let operation = replay
        .operations
        .get(&operation)
        .ok_or("shell operation absent")?;
    assert_eq!(
        operation.tools.len(),
        1,
        "exactly one shell invocation required"
    );
    let tool = &operation.tools[0];
    assert_eq!(tool.kind, 16);
    let decode = |range: std::ops::Range<usize>| -> Result<journal::Record> {
        let frame = &bytes[range];
        Ok(journal::decode_record(
            u16::from_be_bytes([frame[10], frame[11]]),
            &frame[144..frame.len() - 40],
        )
        .map_err(|error| format!("shell record: {error:?}"))?)
    };
    let journal::Record::ToolIntent(intent) = decode(tool.intent_frame.clone())? else {
        return Err("shell intent variant".into());
    };
    let journal::Record::ToolResult(result) =
        decode(tool.result_frame.clone().ok_or("shell result absent")?)?
    else {
        return Err("shell result variant".into());
    };
    assert!(result.text.len() <= 16384);
    assert!(result.next_offset.is_none());
    Ok((intent, result))
}

pub(super) fn run(provider: &str) -> Result<()> {
    let mut fixture = Fixture::new()?;
    fixture.start()?;
    wait_ready(&fixture, None)?;
    select_native_tool_provider(&fixture, provider)?;
    let directory = fixture.home.join("shell-project");
    fs::create_dir(&directory)?;
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))?;
    let proof = format!(
        "SHELL-PROOF-{:032x}",
        u128::from_ne_bytes(asura_platform::random_id())
    );
    fs::write(directory.join("proof.txt"), format!("{proof}\n"))?;
    let project = checked_id(
        fixture
            .attach()?
            .register_project(pb::ProjectRegister {
                request_id: Some(asura_platform::random_id().to_vec()),
                location: Some(directory.to_str().ok_or("scratch UTF8")?.into()),
            })?
            .project_id,
    )?;
    let request = pb::ConversationSubmit {
        request_id: Some(asura_platform::random_id().to_vec()),
        project_id: Some(project.to_vec()),
        conversation_id: None,
        expected_generation: Some(0),
        prompt: Some(format!(
            "Call shell exactly once with command `{COMMAND}`, cwd `.`, timeout_seconds 30. Then reply only with the proof token returned on stdout. Do not use other tools or repeat the command."
        )),
    };
    let accepted = fixture.attach()?.submit_conversation(request.clone())?;
    let operation = checked_id(accepted.operation_id.clone())?;
    let event = observe(&fixture, operation)?;
    if event.kind != Some(3) {
        fixture.print_model_diagnostics();
    }
    assert_eq!(event.kind, Some(3), "native shell answer failed");
    assert!(
        event.text.as_deref().unwrap_or_default().contains(&proof),
        "answer lacks scratch proof"
    );
    assert!(
        event
            .tools
            .iter()
            .any(|tool| tool.name.as_deref() == Some("shell") && tool.status == Some(1))
    );
    assert_eq!(fs::read(directory.join("effect.txt"))?, b"x");
    let retry = fixture.attach()?.submit_conversation(request.clone())?;
    assert_eq!(retry.operation_id, accepted.operation_id);
    fixture.stop()?;
    let (intent, result) = recorded(&fixture, operation)?;
    assert_eq!(intent.path, format!("{COMMAND}\0."));
    assert_eq!(intent.limit, 30);
    assert!((1..=30_000).contains(&intent.offset));
    assert_eq!(result.status, 1);
    assert!(
        result.text.contains(&proof) && result.text.contains("stderr-proof"),
        "durable stdout/stderr evidence missing: {}",
        result.text
    );
    assert!(!result.truncated);
    assert!(result.text.starts_with("failure=None exit_code=0 signal=none reason=none truncated=false cleanup_confirmed=true\nstdout:\n"));
    assert!(result.text.contains("\nstderr:\nstderr-proof"));
    fixture.start()?;
    wait_ready(&fixture, None)?;
    let retry = fixture.attach()?.submit_conversation(request)?;
    assert_eq!(retry.operation_id, accepted.operation_id);
    let replayed = observe(&fixture, operation)?;
    assert_eq!(replayed.text, event.text);
    assert_eq!(replayed.tools, event.tools);
    fixture.stop()?;
    assert_eq!(
        fs::read(directory.join("effect.txt"))?,
        b"x",
        "retry repeated shell effect"
    );
    let (_, replayed_result) = recorded(&fixture, operation)?;
    assert_eq!(replayed_result.text, result.text);
    interrupted(&mut fixture, project, &directory, false)?;
    interrupted(&mut fixture, project, &directory, true)?;
    println!(
        "PASS native {provider} shell proof, exact exit/stdout/stderr, one effect, durable result, exact retry/restart, timeout/cancel cleanup; fixture service stopped/reaped"
    );
    Ok(())
}

fn interrupted(
    fixture: &mut Fixture,
    project: [u8; 16],
    directory: &Path,
    cancel: bool,
) -> Result<()> {
    fixture.start()?;
    wait_ready(fixture, None)?;
    let marker = if cancel { "cancel.pid" } else { "timeout.pid" };
    let command = format!(
        "printf partial-output; printf '%s\\n' \"$$\" > {marker}; sleep 30; printf unexpected > {marker}.late"
    );
    let timeout = if cancel { 30 } else { 1 };
    let prompt = format!(
        "Call shell exactly once with command `{command}`, cwd `.`, timeout_seconds {timeout}. After it returns reply only FINISHED. Do not use other tools or repeat the command."
    );
    let accepted = submit(fixture, project, None, 0, &prompt)?;
    let operation = checked_id(accepted.operation_id)?;
    let deadline = Instant::now() + Duration::from_secs(60);
    while !directory.join(marker).is_file() {
        if Instant::now() >= deadline {
            return Err("native shell spawn marker deadline".into());
        }
        // Actual service control remains responsive while model/setup work is pending.
        fixture.attach()?.inspect_installation()?;
        thread::sleep(Duration::from_millis(25));
    }
    if cancel {
        fixture
            .attach()?
            .cancel_conversation(operation, accepted.generation.ok_or("generation absent")?)?;
    }
    let terminal = observe(fixture, operation)?;
    if cancel {
        assert_eq!(terminal.kind, Some(5));
    }
    fixture.stop()?;
    let (intent, result) = recorded(fixture, operation)?;
    assert_eq!(intent.path, format!("{command}\0."));
    assert_eq!(intent.limit, timeout);
    assert_eq!(result.status, if cancel { 6 } else { 5 });
    assert!(result.text.contains(if cancel {
        "reason=cancelled"
    } else {
        "reason=timeout"
    }));
    assert!(result.text.contains("cleanup_confirmed=true"));
    assert!(
        result.text.contains("partial-output"),
        "partial command output must survive timeout/cancel"
    );
    assert!(!directory.join(format!("{marker}.late")).exists());
    let pid: i32 = fs::read_to_string(directory.join(marker))?.trim().parse()?;
    assert!(pid > 1);
    assert_group_absent(pid)?;
    Ok(())
}

// A read-only signal probe is independent of the guardian's cleanup report.
// It never sends a terminating signal to a potentially reused PID/group.
fn assert_group_absent(pid: i32) -> Result<()> {
    let mut child = Command::new("/bin/kill")
        .args(["-0", "--", &format!("-{pid}")])
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()?;
    let deadline = Instant::now() + Duration::from_secs(2);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error.into());
            }
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err("read-only group probe deadline".into());
        }
        thread::sleep(Duration::from_millis(5));
    };
    let mut diagnostic = String::new();
    child
        .stderr
        .take()
        .ok_or("group probe stderr absent")?
        .take(1024)
        .read_to_string(&mut diagnostic)?;
    assert!(!status.success(), "owned shell process group remains live");
    assert!(
        diagnostic.contains("No such process"),
        "group absence not established: {diagnostic}"
    );
    Ok(())
}
