//! Native exec_cmd qualification uses only an owned scratch project and runtime.
//! Cases EQ-1 to EQ-4 and their failure classes are defined in
//! `docs/designs/exec-cmd-tool.md` (per-model exec_cmd qualification, EX10).
use super::*;
use asura_storage::authority::conversation as journal;
use std::io::Read;

const COMMAND: &str = "printf x >> effect.txt; cat proof.txt; printf stderr-proof >&2";

/// Who a failed case counts against (exec-cmd-tool.md, failure classes).
#[derive(Clone, Copy, Debug)]
enum Class {
    Model,
    Provider,
    Service,
    Harness,
}
#[derive(Debug)]
struct Failure {
    class: Class,
    reason: String,
}
type Case = std::result::Result<(), Failure>;
impl From<Box<dyn Error>> for Failure {
    /// A service refusal to prepare the selected model is a provider failure;
    /// every other fixture error is a harness failure.
    fn from(error: Box<dyn Error>) -> Self {
        let reason = error.to_string();
        let class = if reason.contains("model_unavailable") || reason.contains("context_limit") {
            Class::Provider
        } else {
            Class::Harness
        };
        Self { class, reason }
    }
}
fn fail(class: Class, reason: impl Into<String>) -> Failure {
    Failure {
        class,
        reason: reason.into(),
    }
}
fn ensure(condition: bool, class: Class, reason: impl Into<String>) -> Case {
    if condition {
        Ok(())
    } else {
        Err(fail(class, reason))
    }
}
/// Bounded excerpt of model-chosen text for a failure line.
fn excerpt(text: &str) -> String {
    format!("{:?}", text.chars().take(160).collect::<String>())
}

/// Every tool call of `operation`, each with its decoded kind 14 record and result.
fn recorded(
    fixture: &Fixture,
    operation: [u8; 16],
) -> Result<Vec<(journal::ToolCall, Option<journal::ToolResult>)>> {
    let bytes = asura_storage::authority::fixture::journal_bytes(&fixture.home)?;
    let replay = journal::replay(&bytes, &std::sync::atomic::AtomicBool::new(false))
        .map_err(|error| format!("exec_cmd replay: {error:?}"))?;
    let Some(operation) = replay.operations.get(&operation) else {
        return Ok(Vec::new());
    };
    let decode =
        |range: asura_storage::authority::conversation::Locator| -> Result<journal::Record> {
            let frame = &bytes[range.range()];
            Ok(journal::decode_record(
                u16::from_be_bytes([frame[10], frame[11]]),
                &frame[144..frame.len() - 40],
            )
            .map_err(|error| format!("exec_cmd record: {error:?}"))?)
        };
    let mut calls = Vec::new();
    for tool in &operation.tools {
        // Every model tool call records one kind 14 ToolCall (tool-call-records.md).
        let journal::Record::ToolCall(intent) = decode(tool.intent_frame)? else {
            return Err("tool intent variant".into());
        };
        let result = match tool.result_frame {
            Some(frame) => {
                let journal::Record::ToolResult(result) = decode(frame)? else {
                    return Err("tool result variant".into());
                };
                Some(result)
            }
            None => None,
        };
        calls.push((*intent, result));
    }
    Ok(calls)
}

/// The single exact exec_cmd call of a case, or the model/service failure.
fn exact_call(
    calls: Vec<(journal::ToolCall, Option<journal::ToolResult>)>,
    command: &str,
) -> std::result::Result<journal::ToolResult, Failure> {
    ensure(!calls.is_empty(), Class::Model, "no tool call")?;
    for (intent, _) in &calls {
        ensure(
            intent.tool_operation == Some(journal::ToolOperation::ExecCmd),
            Class::Model,
            format!("called another tool: {}", excerpt(&intent.name)),
        )?;
    }
    ensure(
        calls.len() == 1,
        Class::Model,
        format!("{} exec_cmd calls, one required", calls.len()),
    )?;
    let (intent, result) = calls
        .into_iter()
        .next()
        .ok_or_else(|| fail(Class::Harness, "call list changed during classification"))?;
    let arguments: serde_json::Value = serde_json::from_str(&intent.arguments)
        .map_err(|_| fail(Class::Model, "arguments are not JSON"))?;
    let recorded = arguments["command"].as_str().unwrap_or_default();
    ensure(
        recorded == command,
        Class::Model,
        format!("changed command: {}", excerpt(recorded)),
    )?;
    ensure(
        arguments["cwd"].as_str().is_none_or(|cwd| cwd == "."),
        Class::Model,
        "changed cwd",
    )?;
    ensure(
        intent.decision == journal::ToolDecision::Run,
        Class::Service,
        format!("exact call decision {:?}", intent.decision),
    )?;
    let result = result.ok_or_else(|| fail(Class::Service, "durable result absent"))?;
    ensure(
        result.text.len() <= 16384 && result.next_offset.is_none(),
        Class::Service,
        "result exceeds 16 KiB or has a continuation",
    )?;
    Ok(result)
}

/// A model-class call failure in a turn that failed (neither completed nor
/// cancelled) is reported as the provider failure that ended the turn.
fn unexplained(fixture: &Fixture, event: &pb::ConversationEvent, failure: Failure) -> Failure {
    if matches!(failure.class, Class::Model) && !matches!(event.kind, Some(3) | Some(5)) {
        fixture.print_model_diagnostics();
        return fail(
            Class::Provider,
            format!(
                "turn kind {:?} reason {:?}; {}",
                event.kind, event.reason, failure.reason
            ),
        );
    }
    failure
}
/// The turn's terminal kind; any other ending is a provider failure.
fn terminal(fixture: &Fixture, event: &pb::ConversationEvent, expected: i32) -> Case {
    if event.kind != Some(expected) {
        fixture.print_model_diagnostics();
        return Err(fail(
            Class::Provider,
            format!(
                "turn kind {:?} reason {:?}, expected {expected}",
                event.kind, event.reason
            ),
        ));
    }
    Ok(())
}

fn report(case: &str, outcome: &Case) -> bool {
    match outcome {
        Ok(()) => println!("{case} PASS"),
        Err(failure) => println!(
            "{case} FAIL {}: {}",
            format!("{:?}", failure.class).to_lowercase(),
            failure.reason
        ),
    }
    outcome.is_ok()
}

/// Stops the fixture after a case. A failed clean stop is a service failure,
/// unless the case already failed for another reason.
fn settle(fixture: &mut Fixture, outcome: Case) -> Case {
    let stopped = fixture.stop();
    outcome?;
    stopped.map_err(|error| fail(Class::Service, format!("stop: {error}")))
}

pub(super) fn run(provider: &str) -> Result<()> {
    let mut fixture = Fixture::new()?;
    fixture.start()?;
    wait_ready(&fixture, None)?;
    select_native_tool_provider(&fixture, provider)?;
    let directory = fixture.home.join("exec-cmd-project");
    fs::create_dir(&directory)?;
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))?;
    let proof = format!(
        "EXEC-CMD-PROOF-{:032x}",
        u128::from_ne_bytes(asura_platform::random_id())
    );
    fs::write(directory.join("proof.txt"), format!("{proof}\n"))?;
    let project = checked_id(
        fixture
            .attach()?
            .register_project(
                pb::ProjectRegister {
                    request_id: Some(asura_platform::random_id().to_vec()),
                    location: Some(directory.to_str().ok_or("scratch UTF8")?.into()),
                },
                &|| false,
            )?
            .project_id,
    )?;
    let mut passed = 0;
    let proof_case = proof_call(&mut fixture, project, &directory, &proof);
    let (eq1, retry) = match proof_case {
        Ok(retry) => (Ok(()), Some(retry)),
        Err(failure) => (Err(failure), None),
    };
    passed += usize::from(report("EQ-1", &eq1));
    match retry {
        Some(retry) => {
            let outcome = retry_restart(&mut fixture, &directory, retry);
            let outcome = settle(&mut fixture, outcome);
            passed += usize::from(report("EQ-2", &outcome));
        }
        None => println!("EQ-2 NOT-REACHED: EQ-1 failed"),
    }
    for (case, cancel) in [("EQ-3", false), ("EQ-4", true)] {
        let outcome = interrupted(&mut fixture, project, &directory, cancel, false);
        let outcome = settle(&mut fixture, outcome);
        passed += usize::from(report(case, &outcome));
    }
    // Diagnostic variants without escapes: reported, not part of the verdict.
    for (case, cancel) in [("EQ-3b", false), ("EQ-4b", true)] {
        let outcome = interrupted(&mut fixture, project, &directory, cancel, true);
        let outcome = settle(&mut fixture, outcome);
        report(case, &outcome);
    }
    if passed != 4 {
        return Err(format!(
            "native {provider} exec_cmd qualification: {passed} of 4 cases passed"
        )
        .into());
    }
    println!(
        "PASS native {provider} exec_cmd proof, exact exit/stdout/stderr, one effect, durable result, exact retry/restart, completion/cancel cleanup; fixture service stopped/reaped"
    );
    Ok(())
}

/// What EQ-2 needs from a passed EQ-1.
struct Retry {
    request: pb::ConversationQueueSubmit,
    receipt: pb::ConversationQueueReply,
    accepted: queue_submit::Accepted,
    project: [u8; 16],
    operation: [u8; 16],
    event: pb::ConversationEvent,
    result: journal::ToolResult,
}

/// EQ-1: one exact call, its scratch proof, exact output and one effect.
fn proof_call(
    fixture: &mut Fixture,
    project: [u8; 16],
    directory: &Path,
    proof: &str,
) -> std::result::Result<Retry, Failure> {
    let request = queue_submit::queue_request(
        project,
        None,
        0,
        &format!(
            "Call exec_cmd exactly once with command `{COMMAND}` and cwd `.`. Then reply only with the proof token returned on stdout. Do not use other tools or repeat the command."
        ),
    );
    let observed = (|| -> Result<_> {
        let receipt = managed_submit_call(fixture, request.clone())?;
        let accepted = queue_submit::dispatched(
            &|| fixture.attach(),
            project,
            checked_id(receipt.accepted_input_id.clone())?,
        )?;
        let operation = checked_id(accepted.operation_id.clone())?;
        // No time bound on model work: the turn's terminal event ends this wait.
        let mut observer = Observer::new(operation);
        let event = loop {
            if let Some(event) = observer.step(fixture)? {
                break event;
            }
        };
        Ok((receipt, accepted, operation, event))
    })();
    let (receipt, accepted, operation, event) = match observed {
        Ok(observed) => observed,
        Err(error) => {
            let _ = fixture.stop();
            return Err(error.into());
        }
    };
    // A retry inside the same service lifetime returns the same input.
    let live_retry = managed_submit_call(fixture, request.clone())
        .map(|retry| retry.accepted_input_id == receipt.accepted_input_id);
    let stopped = fixture.stop();
    let calls = recorded(fixture, operation)?;
    let result = exact_call(calls, COMMAND).map_err(|f| unexplained(fixture, &event, f))?;
    terminal(fixture, &event, 3)?;
    stopped.map_err(|error| fail(Class::Service, format!("stop: {error}")))?;
    ensure(
        event.text.as_deref().unwrap_or_default().contains(proof),
        Class::Model,
        "answer lacks the scratch proof token",
    )?;
    ensure(
        event
            .tools
            .iter()
            .any(|tool| tool.name.as_deref() == Some("exec_cmd") && tool.status == Some(1)),
        Class::Service,
        "turn lacks a successful exec_cmd",
    )?;
    ensure(
        fs::read(directory.join("effect.txt")).ok().as_deref() == Some(b"x"),
        Class::Service,
        "effect.txt is not exactly one effect",
    )?;
    ensure(
        result.status == 1,
        Class::Service,
        format!("result status {}", result.status),
    )?;
    ensure(
        result.text.contains(proof) && result.text.contains("stderr-proof"),
        Class::Service,
        "durable stdout/stderr evidence missing",
    )?;
    ensure(!result.truncated, Class::Service, "result truncated")?;
    ensure(
        result.text.starts_with("failure=None exit_code=0 signal=none reason=none truncated=false cleanup_confirmed=true capture=complete\nstdout:\n")
            && result.text.contains("\nstderr:\nstderr-proof"),
        Class::Service,
        "result envelope differs",
    )?;
    ensure(
        live_retry.unwrap_or(false),
        Class::Service,
        "live retry returned another input",
    )?;
    Ok(Retry {
        request,
        receipt,
        accepted,
        project,
        operation,
        event,
        result,
    })
}

/// EQ-2: the same request after restart returns the same operation and no new effect.
fn retry_restart(fixture: &mut Fixture, directory: &Path, retry: Retry) -> Case {
    fixture.start()?;
    wait_ready(fixture, None)?;
    let again = managed_submit_call(fixture, retry.request)?;
    ensure(
        again.accepted_input_id == retry.receipt.accepted_input_id,
        Class::Service,
        "restart retry returned another input",
    )?;
    let again =
        queue_submit::dispatched(&|| fixture.attach(), retry.project, retry.accepted.input_id)?;
    ensure(
        again.operation_id == retry.accepted.operation_id,
        Class::Service,
        "restart retry returned another operation",
    )?;
    let replayed = observe(fixture, retry.operation)?;
    ensure(
        replayed.text == retry.event.text,
        Class::Service,
        "replayed answer differs",
    )?;
    // Restart leaves live measurements absent rather than inventing history.
    let without_elapsed = |tools: &[pb::ToolProgress]| {
        tools
            .iter()
            .cloned()
            .map(|mut tool| {
                tool.elapsed_ms = None;
                tool
            })
            .collect::<Vec<_>>()
    };
    ensure(
        without_elapsed(&replayed.tools) == without_elapsed(&retry.event.tools)
            && replayed.tools.iter().all(|tool| tool.elapsed_ms.is_none()),
        Class::Service,
        "replayed tool progress differs",
    )?;
    fixture
        .stop()
        .map_err(|error| fail(Class::Service, format!("stop: {error}")))?;
    ensure(
        fs::read(directory.join("effect.txt")).ok().as_deref() == Some(b"x"),
        Class::Service,
        "retry repeated the exec_cmd effect",
    )?;
    let calls = recorded(fixture, retry.operation)?;
    let replayed = exact_call(calls, COMMAND).map_err(|failure| Failure {
        class: Class::Service,
        reason: format!("replayed call: {}", failure.reason),
    })?;
    ensure(
        replayed.text == retry.result.text,
        Class::Service,
        "replayed durable result differs",
    )
}

/// EQ-3 (completion) and EQ-4 (cancel after the PID marker appears).
/// `plain` selects the EQ-3b/EQ-4b command: no quotes or backslash escapes,
/// so a failure there is a copying error rather than an escaping error.
fn interrupted(
    fixture: &mut Fixture,
    project: [u8; 16],
    directory: &Path,
    cancel: bool,
    plain: bool,
) -> Case {
    fixture.start()?;
    wait_ready(fixture, None)?;
    let marker = match (cancel, plain) {
        (false, false) => "complete.pid",
        (true, false) => "cancel.pid",
        (false, true) => "complete-plain.pid",
        (true, true) => "cancel-plain.pid",
    };
    // No time bound: a 3 s command completes; only cancel stops one.
    let sleep = if cancel { 30 } else { 3 };
    let command = if plain {
        format!(
            "printf partial-output; echo $$ > {marker}; sleep {sleep}; echo late > {marker}.late"
        )
    } else {
        format!(
            "printf partial-output; printf '%s\\n' \"$$\" > {marker}; sleep {sleep}; printf late > {marker}.late"
        )
    };
    let prompt = format!(
        "Call exec_cmd exactly once with command `{command}` and cwd `.`. After it returns reply only FINISHED. Do not use other tools or repeat the command."
    );
    let accepted = submit(fixture, project, None, 0, &prompt)?;
    let operation = checked_id(accepted.operation_id)?;
    let mut observer = Observer::new(operation);
    let mut ended = None;
    if cancel {
        // Loop closure: the marker appears or the turn ends; no time bound.
        while !directory.join(marker).is_file() {
            if let Some(event) = observer.step(fixture)? {
                ended = Some(event);
                break;
            }
            // Actual service control remains responsive while model/setup work is pending.
            let control: Result<_> = (|| Ok(fixture.attach()?.inspect_installation()?))();
            control?;
        }
        if ended.is_none() {
            let cancelled: Result<_> = (|| {
                Ok(fixture.attach()?.cancel_conversation(
                    operation,
                    accepted.generation.ok_or("generation absent")?,
                    &|| false,
                )?)
            })();
            cancelled?;
        }
    }
    let event = match ended {
        Some(event) => event,
        None => loop {
            if let Some(event) = observer.step(fixture)? {
                break event;
            }
        },
    };
    let stopped = fixture.stop();
    let calls = recorded(fixture, operation)?;
    let result = exact_call(calls, &command).map_err(|f| unexplained(fixture, &event, f))?;
    terminal(fixture, &event, if cancel { 5 } else { 3 })?;
    stopped.map_err(|error| fail(Class::Service, format!("stop: {error}")))?;
    ensure(
        result.status == if cancel { 6 } else { 1 },
        Class::Service,
        format!("result status {}", result.status),
    )?;
    ensure(
        result.text.contains(if cancel {
            "reason=cancelled"
        } else {
            "reason=none"
        }) && result.text.contains("cleanup_confirmed=true"),
        Class::Service,
        "result reason or cleanup differs",
    )?;
    ensure(
        result.text.contains("partial-output"),
        Class::Service,
        "partial command output must survive completion and cancel",
    )?;
    ensure(
        directory.join(format!("{marker}.late")).exists() != cancel,
        Class::Service,
        "cancel stops the command; nothing else does",
    )?;
    let pid: i32 = fs::read_to_string(directory.join(marker))
        .map_err(|_| fail(Class::Service, "PID marker absent after an exact call"))?
        .trim()
        .parse()
        .map_err(|_| fail(Class::Service, "PID marker unreadable"))?;
    ensure(pid > 1, Class::Service, "PID marker invalid")?;
    assert_group_absent(pid)
}

// A read-only signal probe is independent of the guardian's cleanup report.
// It never sends a terminating signal to a potentially reused PID/group.
fn assert_group_absent(pid: i32) -> Case {
    let mut child = Command::new("/bin/kill")
        .args(["-0", "--", &format!("-{pid}")])
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| fail(Class::Harness, error.to_string()))?;
    let deadline = Instant::now() + Duration::from_secs(2);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(fail(Class::Harness, error.to_string()));
            }
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(fail(Class::Harness, "read-only group probe deadline"));
        }
        thread::sleep(Duration::from_millis(5));
    };
    let mut diagnostic = String::new();
    child
        .stderr
        .take()
        .ok_or_else(|| fail(Class::Harness, "group probe stderr absent"))?
        .take(1024)
        .read_to_string(&mut diagnostic)
        .map_err(|error| fail(Class::Harness, error.to_string()))?;
    ensure(
        !status.success() && diagnostic.contains("No such process"),
        Class::Service,
        format!("owned exec_cmd process group remains live: {diagnostic}"),
    )
}
