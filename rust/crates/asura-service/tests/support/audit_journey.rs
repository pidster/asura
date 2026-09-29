//! Canonical conflicts and project-filtered audit inspection in a private service.
use super::*;
use asura_storage::authority::{conversation as j, writer as w};

fn request(index: u8) -> [u8; 16] {
    [80 + index; 16]
}
fn conversation(index: u8) -> [u8; 16] {
    [90 + index; 16]
}
fn execute(writer: &w::WriterHandle, command: w::Command) -> Result<w::Reply> {
    let mut ticket = writer
        .try_submit(command)
        .map_err(|e| format!("audit seed submit:{e:?}"))?;
    let deadline = Instant::now() + Duration::from_secs(12);
    loop {
        if let Some(value) = ticket.poll() {
            return value.map_err(|e| format!("audit seed writer:{e:?}").into());
        }
        if Instant::now() >= deadline {
            return Err("audit seed deadline".into());
        }
        thread::sleep(Duration::from_millis(2));
    }
}
fn seed(fixture: &Fixture, projects: &[[u8; 16]]) -> Result<()> {
    assert!(fixture.child.is_none());
    let mut writer = w::WriterHandle::start(fixture.runtime()?)
        .map_err(|e| format!("audit seed start:{e:?}"))?;
    let result = (|| {
        let mut revision = execute(&writer, w::Command::Open)?
            .replay
            .ok_or("seed replay")?
            .revision;
        for (index, project) in projects.iter().enumerate() {
            let index = index as u8;
            let prompt = "AUDIT_SEED_PRIVATE_PROMPT_NOT_EXPOSED";
            let prepared = execute(
                &writer,
                w::Command::Prepare {
                    project: *project,
                    conversation: None,
                    expected_generation: 0,
                    prompt: prompt.into(),
                },
            )?
            .prepared
            .ok_or("seed preparation")?;
            let operation = [100 + index; 16];
            let turn = j::TurnAccepted {
                request: request(index),
                digest: j::request_digest(j::Request::Submit {
                    project: *project,
                    conversation: None,
                    expected_generation: 0,
                    prompt,
                })
                .map_err(|e| format!("seed digest:{e:?}"))?,
                project: *project,
                original_conversation: None,
                expected_generation: 0,
                conversation: conversation(index),
                generation: 1,
                task: [110 + index; 16],
                operation,
                model: "system".into(),
                configuration_digest: prepared.configuration_digest,
                instructions_digest: [1; 32],
                input_digest: [2; 32],
                prior_operations: vec![],
                prompt: prompt.into(),
                reserved_output_tokens: 512,
                event_cursor: 1,
            };
            revision = execute(
                &writer,
                w::Command::Append {
                    expected_revision: revision,
                    record: j::Record::TurnAccepted(Box::new(turn)),
                    admission: Some(w::AdmissionFence {
                        project: *project,
                        configuration_digest: prepared.configuration_digest,
                    }),
                },
            )?
            .replay
            .ok_or("accepted replay")?
            .revision;
            revision = execute(
                &writer,
                w::Command::Append {
                    expected_revision: revision,
                    record: j::Record::TurnTerminal(j::TurnTerminal {
                        operation,
                        generation: 1,
                        kind: j::TerminalKind::Failed,
                        cause: j::Cause::ProviderFailure,
                        final_cursor: u64::MAX,
                        usage_known: true,
                        output_tokens: 0,
                        charged_tokens: 0,
                        text: String::new(),
                    }),
                    admission: None,
                },
            )?
            .replay
            .ok_or("terminal replay")?
            .revision;
        }
        Ok(())
    })();
    writer.close();
    let deadline = Instant::now() + Duration::from_secs(12);
    while !writer.settled() {
        if Instant::now() >= deadline {
            return Err("audit seed cleanup deadline".into());
        }
        thread::sleep(Duration::from_millis(2));
    }
    result
}
fn conflict(fixture: &Fixture, project: [u8; 16], index: u8) -> Result<()> {
    let error = fixture
        .attach()?
        .submit_conversation(pb::ConversationSubmit {
            request_id: Some(request(index).to_vec()),
            project_id: Some(project.to_vec()),
            conversation_id: Some(conversation(index).to_vec()),
            expected_generation: Some(1),
            prompt: Some("AUDIT_CHANGED_PRIVATE_PROMPT_NOT_EXPOSED".into()),
        })
        .expect_err("canonical changed digest must conflict");
    assert!(
        matches!(error,asura_client::Error::Remote(_,ref code) if code=="request_conflict"),
        "unexpected conflict outcome:{error:?}"
    );
    Ok(())
}
fn read(fixture: &Fixture, project: [u8; 16], limit: u32) -> Result<pb::AuditReply> {
    Ok(fixture.attach()?.audit(pb::AuditRead {
        project_id: Some(project.to_vec()),
        limit: Some(limit),
    })?)
}
fn wait_conflict(
    fixture: &Fixture,
    project: [u8; 16],
    index: u8,
    state: u32,
) -> Result<pb::AuditReply> {
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        let reply = read(fixture, project, 16)?;
        if reply
            .health
            .as_ref()
            .is_some_and(|h| h.state == Some(state) && h.hydrated == Some(true))
            && reply.entries.iter().any(|e| {
                e.kind == Some(1)
                    && e.request == Some(request(index).to_vec())
                    && e.reason == Some(1)
            })
        {
            assert!(reply.error.is_none());
            assert!(
                reply
                    .entries
                    .iter()
                    .all(|e| e.project == Some(project.to_vec()))
            );
            let row = reply
                .entries
                .iter()
                .find(|e| e.request == Some(request(index).to_vec()))
                .unwrap();
            assert_eq!(row.requested_generation, Some(1));
            assert_eq!(row.current_generation, Some(1));
            assert_eq!(row.outcome, Some(2));
            assert_eq!(reply.health.as_ref().unwrap().window_capacity, Some(256));
            return Ok(reply);
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "audit publication deadline: health={:?} entries={}",
                reply.health,
                reply.entries.len()
            )
            .into());
        }
        thread::sleep(Duration::from_millis(20));
    }
}
fn setup() -> Result<(Fixture, Vec<[u8; 16]>)> {
    let mut fixture = Fixture::new()?;
    fixture.start()?;
    wait_ready(&fixture, None)?;
    let mut projects = vec![];
    for name in ["audit-project", "foreign-project"] {
        let directory = fixture.home.join(name);
        fs::create_dir(&directory)?;
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))?;
        projects.push(checked_id(
            fixture
                .attach()?
                .register_project(pb::ProjectRegister {
                    request_id: Some(asura_platform::random_id().to_vec()),
                    location: Some(directory.to_str().ok_or("scratch UTF8")?.into()),
                })?
                .project_id,
        )?);
    }
    fixture.stop()?;
    seed(&fixture, &projects)?;
    fixture.start()?;
    wait_ready(&fixture, None)?;
    for (i, p) in projects.iter().enumerate() {
        conflict(&fixture, *p, i as u8)?;
        wait_conflict(&fixture, *p, i as u8, 2)?;
    }
    Ok((fixture, projects))
}
pub(super) fn run(provider: Option<&str>) -> Result<()> {
    let (mut fixture, projects) = setup()?;
    let selected = wait_conflict(&fixture, projects[0], 0, 2)?;
    assert!(
        !selected
            .entries
            .iter()
            .any(|e| e.request == Some(request(1).to_vec()))
    );
    let limited = read(&fixture, projects[0], 1)?;
    assert_eq!(limited.entries.len(), 1);
    let unknown = read(&fixture, [254; 16], 16)?;
    assert_eq!(unknown.error.as_deref(), Some("project_unknown"));
    assert!(unknown.entries.is_empty());
    if let Some(provider) = provider {
        select_native_tool_provider(&fixture, provider)?;
        let accepted = submit(
            &fixture,
            projects[0],
            None,
            0,
            "Call service with command audit and limit 16 exactly once. If a returned admission reason is request_conflict, reply only CONFLICT. Otherwise reply only NONE. Do not use other tools.",
        )?;
        let operation = checked_id(accepted.operation_id)?;
        let event = observe(&fixture, operation)?;
        if event.kind != Some(3) {
            fixture.print_model_diagnostics();
        }
        assert_eq!(event.kind, Some(3), "native audit response failed");
        assert!(
            event
                .text
                .as_deref()
                .unwrap_or_default()
                .contains("CONFLICT"),
            "native answer did not ground in audit reason"
        );
        assert!(
            event
                .tools
                .iter()
                .any(|t| t.name.as_deref() == Some("service_read_audit") && t.status == Some(1))
        );
        fixture.stop()?;
        verify_tool(&fixture, operation, projects[0])?;
        fixture.start()?;
        wait_ready(&fixture, None)?;
        let replayed = observe(&fixture, operation)?;
        assert_eq!(replayed.text, event.text);
        assert_eq!(replayed.tools, event.tools);
        conflict(&fixture, projects[0], 0)?;
        wait_conflict(&fixture, projects[0], 0, 2)?;
    }
    fixture.stop()?;
    fixture.start()?;
    wait_ready(&fixture, None)?;
    wait_conflict(&fixture, projects[0], 0, 2)?;
    // Refresh the active file: hydration intentionally does not scan sealed archives.
    conflict(&fixture, projects[0], 0)?;
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        let r = read(&fixture, projects[0], 16)?;
        if r.entries
            .iter()
            .filter(|e| e.request == Some(request(0).to_vec()))
            .count()
            >= 2
        {
            break;
        }
        if Instant::now() >= deadline {
            return Err("fresh active audit record missing".into());
        }
        thread::sleep(Duration::from_millis(20));
    }
    assert!(
        fixture
            .attach()?
            .config("audit.enabled", Some("false"))?
            .error
            .is_none()
    );
    fixture.stop()?;
    let before = fs::read(fixture.home.join(".asura/logs/audit.jsonl"))?;
    fixture.start()?;
    wait_ready(&fixture, None)?;
    wait_conflict(&fixture, projects[0], 0, 3)?;
    conflict(&fixture, projects[0], 0)?;
    fixture.stop()?;
    assert_eq!(
        fs::read(fixture.home.join(".asura/logs/audit.jsonl"))?,
        before,
        "disabled audit mutated active file"
    );
    println!(
        "PASS audit canonical request conflict, requested/current generations, project isolation, limit, restart hydration, disabled no-write and owned cleanup; provider={provider:?}"
    );
    Ok(())
}
fn verify_tool(fixture: &Fixture, operation: [u8; 16], project: [u8; 16]) -> Result<()> {
    use std::io::Read;
    let mut bytes = vec![];
    fs::File::open(fixture.home.join(".asura/state/control/slot-0.log"))?
        .take(asura_storage::authority::MAX_JOURNAL as u64 + 1)
        .read_to_end(&mut bytes)?;
    let replay = j::replay(
        &bytes,
        Instant::now() + Duration::from_secs(2),
        &std::sync::atomic::AtomicBool::new(false),
    )
    .map_err(|e| format!("audit proof replay:{e:?}"))?;
    let tools = &replay
        .operations
        .get(&operation)
        .ok_or("audit operation")?
        .tools;
    let selected: Vec<_> = tools.iter().filter(|t| t.kind == 15).collect();
    assert_eq!(selected.len(), 1);
    let tool = selected[0];
    assert_eq!(tool.result_status, Some(1));
    let decode = |range: std::ops::Range<usize>| -> Result<j::Record> {
        let frame = &bytes[range];
        Ok(j::decode_record(
            u16::from_be_bytes([frame[10], frame[11]]),
            &frame[144..frame.len() - 40],
        )
        .map_err(|e| format!("audit record:{e:?}"))?)
    };
    let j::Record::ToolIntent(intent) = decode(tool.intent_frame.clone())? else {
        return Err("audit intent variant".into());
    };
    assert_eq!(intent.kind, 15);
    assert_eq!(intent.path, "");
    assert_eq!(intent.offset, 0);
    assert_eq!(intent.limit, 16);
    let j::Record::ToolResult(result) = decode(tool.result_frame.clone().ok_or("audit result")?)?
    else {
        return Err("audit result variant".into());
    };
    assert!(
        result.text.len() <= 16384
            && result
                .text
                .starts_with("Audit diagnostic evidence; window=recent capacity=256")
    );
    assert!(!result.text.contains("PRIVATE_PROMPT_NOT_EXPOSED"));
    let mut conflict_seen = false;
    for line in result.text.lines().skip(1) {
        let record = asura_storage::audit::Record::decode(format!("{line}\n").as_bytes())?;
        assert_eq!(record.event.project(), Some(project));
        if let asura_storage::audit::Event::ConversationAdmission {
            request: record_request,
            requested_generation,
            current_generation,
            reason,
            ..
        } = record.event
        {
            if record_request == request(0) {
                assert_eq!(requested_generation, 1);
                assert_eq!(current_generation, Some(1));
                assert_eq!(
                    reason,
                    asura_storage::audit::DecisionReason::RequestConflict
                );
                conflict_seen = true;
            }
            assert_ne!(record_request, request(1));
        }
    }
    assert!(
        conflict_seen,
        "exact canonical conflict absent from durable audit result"
    );
    Ok(())
}
