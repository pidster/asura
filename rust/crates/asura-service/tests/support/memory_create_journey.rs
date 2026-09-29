//! HM3-E1: native creation is proved by exact journal, note, edge and receipt evidence.
use super::*;
use asura_storage::{authority::conversation as journal, memory};
use std::{io::Read, sync::atomic::AtomicBool};

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}
fn replay(fixture: &Fixture) -> Result<(Vec<u8>, journal::Replay)> {
    let mut bytes = Vec::new();
    fs::File::open(fixture.home.join(".asura/state/control/slot-0.log"))?
        .take(asura_storage::authority::MAX_JOURNAL as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > asura_storage::authority::MAX_JOURNAL {
        return Err("HM3 fixture journal exceeds bound".into());
    }
    let state = journal::replay(
        &bytes,
        Instant::now() + Duration::from_secs(2),
        &AtomicBool::new(false),
    )
    .map_err(|e| format!("HM3 replay: {e:?}"))?;
    Ok((bytes, state))
}
fn binding(state: &journal::Replay) -> Result<memory::Binding> {
    Ok(memory::Binding {
        installation_id: memory::Id::new(state.installation_id)?,
        graph_id: memory::Id::new(state.initialization.graph)?,
        init_operation_id: memory::Id::new(state.initialization.request)?,
    })
}
fn decode(bytes: &[u8], range: std::ops::Range<usize>) -> Result<journal::Record> {
    // Canonical replay above validates the format-1 envelope before this extraction.
    let frame = &bytes[range];
    journal::decode_record(
        u16::from_be_bytes([frame[10], frame[11]]),
        &frame[144..frame.len() - 40],
    )
    .map_err(|e| format!("HM3 record: {e:?}").into())
}
fn records(
    fixture: &Fixture,
    operation: [u8; 16],
) -> Result<Vec<(journal::Record, journal::ToolResult)>> {
    let (bytes, state) = replay(fixture)?;
    let mut out = Vec::new();
    for tool in &state
        .operations
        .get(&operation)
        .ok_or("HM3 missing operation")?
        .tools
    {
        let result = tool.result_frame.clone().ok_or("HM3 unresolved tool")?;
        let journal::Record::ToolResult(result) = decode(&bytes, result)? else {
            return Err("HM3 unexpected result frame".into());
        };
        out.push((decode(&bytes, tool.intent_frame.clone())?, result));
    }
    Ok(out)
}
fn diagnose(fixture: &Fixture, operation: [u8; 16], event: &pb::ConversationEvent, step: &str) {
    fixture.print_model_diagnostics();
    eprintln!(
        "HM3 step={step} event_kind={:?} reason={:?} generation={:?} tools={}",
        event.kind,
        event.reason,
        event.generation,
        event.tools.len()
    );
    for tool in event.tools.iter().take(8) {
        // Names come from the closed service registry. Never emit model text or arguments.
        let name = match tool.name.as_deref() {
            Some("memory_create_note") => "memory_create_note",
            Some("memory_get_note") => "memory_get_note",
            Some("memory_note_sources") => "memory_note_sources",
            _ => "other",
        };
        eprintln!(
            "HM3 progress ordinal={:?} name={name} state={:?} status={:?}",
            tool.ordinal, tool.state, tool.status
        );
    }
    match replay(fixture) {
        Ok((_, state)) => {
            if let Some(record) = state.operations.get(&operation) {
                for (index, tool) in record.tools.iter().take(8).enumerate() {
                    eprintln!(
                        "HM3 durable ordinal={} kind={} status={:?} result_present={} result_bytes={} create_body_bytes={:?} create_has_source={}",
                        index + 1,
                        tool.kind,
                        tool.result_status,
                        tool.result_frame.is_some(),
                        tool.result_bytes,
                        tool.create.as_ref().map(|value| value.body_bytes),
                        tool.create
                            .as_ref()
                            .is_some_and(|value| value.source.is_some())
                    );
                }
            } else {
                eprintln!("HM3 durable operation missing");
            }
        }
        Err(_) => eprintln!("HM3 bounded diagnostic replay failed"),
    }
}
fn diagnose_read_arguments(
    evidence: &[(journal::Record, journal::ToolResult)],
    expected: [u8; 16],
) {
    for (index, (record, _)) in evidence.iter().take(8).enumerate() {
        let journal::Record::ToolIntent(intent) = record else {
            continue;
        };
        // Rejected proposals retain canonical argument metadata. Emit predicates only.
        let version = intent.path.as_bytes();
        eprintln!(
            "HM3 read arguments ordinal={} kind={} version_length={} version_lowerhex={} version_nonzero={} expected_version_match={} limit_in_range={} offset_plus_limit_valid={} offset_zero={}",
            index + 1,
            intent.kind,
            version.len(),
            version
                .iter()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte)),
            version.iter().any(|byte| *byte != b'0'),
            intent.path == hex(&expected),
            (1..=16384).contains(&intent.limit),
            intent
                .offset
                .checked_add(u64::from(intent.limit))
                .is_some_and(|end| end <= i64::MAX as u64),
            intent.offset == 0,
        );
    }
}
fn run_turn(
    fixture: &Fixture,
    project: [u8; 16],
    prompt: &str,
    step: &str,
) -> Result<([u8; 16], pb::ConversationEvent)> {
    let accepted = submit(fixture, project, None, 0, prompt)?;
    let operation = checked_id(accepted.operation_id)?;
    let event = observe(fixture, operation)?;
    if event.kind != Some(3) {
        diagnose(fixture, operation, &event, step);
        return Err(format!(
            "HM3 native step {step} incomplete: kind={:?} reason={:?}",
            event.kind, event.reason
        )
        .into());
    }
    Ok((operation, event))
}
fn create(
    fixture: &Fixture,
    project: [u8; 16],
    body: &str,
    source: Option<[u8; 16]>,
) -> Result<(journal::MemoryCreateIntent, pb::ConversationEvent)> {
    let source_arg = source
        .map(|id| format!(", source_version {}", hex(&id)))
        .unwrap_or_default();
    let prompt = format!(
        "Call memory exactly once with command create_note and body exactly {body:?}{source_arg}. Preserve the body bytes exactly. Do not call any other tool. Report the returned version ID."
    );
    let (operation, event) = run_turn(
        fixture,
        project,
        &prompt,
        if source.is_some() {
            "create-derived"
        } else {
            "create-source"
        },
    )?;
    let records = records(fixture, operation)?;
    if records.len() != 1 {
        diagnose(
            fixture,
            operation,
            &event,
            if source.is_some() {
                "create-derived"
            } else {
                "create-source"
            },
        );
        return Err("HM3 expected exactly one tool call".into());
    }
    let (journal::Record::MemoryCreateIntent(intent), result) = &records[0] else {
        diagnose(
            fixture,
            operation,
            &event,
            if source.is_some() {
                "create-derived"
            } else {
                "create-source"
            },
        );
        return Err("HM3 native create intent missing".into());
    };
    let (_, state) = replay(fixture)?;
    let (expected, _) = memory::prepare_create(
        binding(&state)?,
        memory::Id::new(project)?,
        operation,
        intent.generation,
        intent.ordinal,
        body.into(),
        source.map(memory::Id::new).transpose()?,
    )?;
    let text = format!(
        "operation={}\nobject={}\nversion={}\nsha256={}\n",
        hex(&expected.memory_operation),
        hex(&expected.object),
        hex(&expected.version),
        hex(&expected.body_sha256)
    );
    if intent != &expected
        || result.status != 1
        || result.text != text
        || result.next_offset.is_some()
        || result.truncated
    {
        diagnose(
            fixture,
            operation,
            &event,
            if source.is_some() {
                "create-derived"
            } else {
                "create-source"
            },
        );
        return Err("HM3 exact durable creation evidence mismatch".into());
    }
    if !event
        .tools
        .iter()
        .any(|t| t.name.as_deref() == Some("memory_create_note") && t.status == Some(1))
    {
        diagnose(
            fixture,
            operation,
            &event,
            if source.is_some() {
                "create-derived"
            } else {
                "create-source"
            },
        );
        return Err("HM3 missing successful native tool progress".into());
    }
    Ok((intent.clone(), event))
}
fn read(
    fixture: &Fixture,
    project: [u8; 16],
    version: [u8; 16],
    body: Option<&str>,
) -> Result<([u8; 16], pb::ConversationEvent)> {
    let step = if body.is_some() {
        "read-own"
    } else {
        "read-foreign"
    };
    let pair = run_turn(
        fixture,
        project,
        &format!(
            "Call memory exactly once with command get_note, version {}, offset 0, limit 4096. Report its result. Do not use other tools.",
            hex(&version)
        ),
        if body.is_some() {
            "read-own"
        } else {
            "read-foreign"
        },
    )?;
    let evidence = records(fixture, pair.0)?;
    if evidence.len() != 1 {
        diagnose(fixture, pair.0, &pair.1, step);
        diagnose_read_arguments(&evidence, version);
        return Err(format!(
            "HM3 step {step}: expected one read, actual={}",
            evidence.len()
        )
        .into());
    }
    let (journal::Record::ToolIntent(intent), result) = &evidence[0] else {
        diagnose(fixture, pair.0, &pair.1, step);
        diagnose_read_arguments(&evidence, version);
        return Err("HM3 read intent absent".into());
    };
    let (status, text) = body.map_or((4, "memory_note_not_found"), |body| (1, body));
    if intent.kind != 7
        || intent.path != hex(&version)
        || intent.offset != 0
        || intent.limit != 4096
        || result.status != status
        || result.text != text
        || result.next_offset != body.map(|value| value.len() as u64)
        || result.truncated
    {
        diagnose(fixture, pair.0, &pair.1, step);
        diagnose_read_arguments(&evidence, version);
        return Err("HM3 exact native read evidence mismatch".into());
    }
    Ok(pair)
}
fn wait<T>(mut pending: memory::Pending<T>) -> Result<T> {
    let end = Instant::now() + Duration::from_secs(12);
    loop {
        if let Some(result) = pending.poll() {
            return result.map_err(Into::into);
        }
        if Instant::now() >= end {
            return Err("HM3 database deadline".into());
        }
        thread::sleep(Duration::from_millis(2));
    }
}
fn verify_database(
    fixture: &Fixture,
    project: [u8; 16],
    foreign: [u8; 16],
    notes: &[(journal::MemoryCreateIntent, String)],
) -> Result<()> {
    if fixture.child.is_some() {
        return Err("HM3 database inspection requires stopped service".into());
    }
    let (_, state) = replay(fixture)?;
    let binding = binding(&state)?;
    let (mut database, ready) = memory::Memory::open(fixture.home.clone(), binding.clone())?;
    let result = (|| {
        wait(ready)?;
        for (intent, body) in notes {
            let receipt = wait(database.resolve(
                memory::Id::new(intent.memory_operation)?,
                intent.command_sha256,
            )?)?;
            if receipt.operation_id.bytes() != intent.memory_operation
                || receipt.version_id.bytes() != intent.version
                || receipt.command_sha256 != intent.command_sha256
                || receipt.edge_id.map(|id| id.bytes()) != intent.source.map(|(_, edge)| edge)
            {
                return Err("HM3 exact receipt mismatch".into());
            }
            let note = wait(database.get_note(
                binding.clone(),
                memory::Id::new(project)?,
                memory::Id::new(intent.version)?,
            )?)?;
            if note.body != *body
                || note.version_id.bytes() != intent.version
                || note.object_id.bytes() != intent.object
                || note.operation_id.bytes() != intent.memory_operation
                || note.body_sha256 != intent.body_sha256
                || note.binding != binding
                || note.context_id.bytes() != project
            {
                return Err("HM3 exact stored note mismatch".into());
            }
            let sources = wait(database.get_sources(
                binding.clone(),
                memory::Id::new(project)?,
                memory::Id::new(intent.version)?,
            )?)?;
            if sources
                .iter()
                .map(|n| n.version_id.bytes())
                .collect::<Vec<_>>()
                != intent.source.iter().map(|(id, _)| *id).collect::<Vec<_>>()
            {
                return Err("HM3 exact source edge mismatch".into());
            }
        }
        for (context, count) in [(project, notes.len()), (foreign, 0)] {
            let page = wait(database.list_notes(memory::ListNotes {
                binding: binding.clone(),
                context_id: memory::Id::new(context)?,
                after: None,
                limit: 16,
            })?)?;
            if page.notes.len() != count || page.next.is_some() {
                return Err("HM3 duplicate or foreign note present".into());
            }
        }
        Ok(())
    })();
    let end = Instant::now() + Duration::from_secs(12);
    loop {
        match database.close() {
            Ok(true) => break,
            Ok(false) | Err(memory::Error::Timeout) => (),
            Err(error) => return Err(error.into()),
        }
        if Instant::now() >= end {
            return Err("HM3 database cleanup did not settle".into());
        }
        thread::sleep(Duration::from_millis(2));
    }
    result
}
pub(super) fn run(provider: &str) -> Result<()> {
    let mut fixture = Fixture::new()?;
    let home = PathBuf::from(format!(
        "/private/tmp/asura-memory-native-{:032x}",
        u128::from_ne_bytes(asura_platform::random_id())
    ));
    fs::rename(&fixture.home, &home)?;
    fixture.home = home;
    fixture.start()?;
    wait_ready(&fixture, None)?;
    select_native_tool_provider(&fixture, provider)?;
    let mut projects = Vec::new();
    for name in ["project", "foreign"] {
        let path = fixture.home.join(name);
        fs::create_dir(&path)?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700))?;
        projects.push(checked_id(
            fixture
                .attach()?
                .register_project(pb::ProjectRegister {
                    request_id: Some(asura_platform::random_id().to_vec()),
                    location: Some(path.to_str().unwrap().into()),
                })?
                .project_id,
        )?);
    }
    let body = format!(
        "HM3_PROOF_{:032x}",
        u128::from_ne_bytes(asura_platform::random_id())
    );
    let derived = format!(
        "HM3_DERIVED_{:032x}",
        u128::from_ne_bytes(asura_platform::random_id())
    );
    let (source, source_event) = create(&fixture, projects[0], &body, None)?;
    let (note, note_event) = create(&fixture, projects[0], &derived, Some(source.version))?;
    let mut saved = vec![
        (source.operation, source_event),
        (note.operation, note_event),
    ];
    saved.push(read(&fixture, projects[0], source.version, Some(&body))?);
    saved.push(read(&fixture, projects[0], note.version, Some(&derived))?);
    let sources = run_turn(
        &fixture,
        projects[0],
        &format!(
            "Call memory exactly once with command note_sources and version {}. Report the exact source ID. Do not use other tools.",
            hex(&note.version)
        ),
        "read-sources",
    )?;
    let evidence = records(&fixture, sources.0)?;
    if evidence.len() != 1 {
        diagnose(&fixture, sources.0, &sources.1, "read-sources");
        return Err(format!("HM3 expected one source read, actual={}", evidence.len()).into());
    }
    let (journal::Record::ToolIntent(intent), result) = &evidence[0] else {
        diagnose(&fixture, sources.0, &sources.1, "read-sources");
        return Err("HM3 source read intent absent".into());
    };
    let expected = format!(
        "sources=1\nversion={} object={} sha256={}\n",
        hex(&source.version),
        hex(&source.object),
        hex(&source.body_sha256)
    );
    if intent.kind != 8
        || intent.path != hex(&note.version)
        || result.status != 1
        || result.text != expected
        || result.truncated
        || result.next_offset.is_some()
    {
        diagnose(&fixture, sources.0, &sources.1, "read-sources");
        return Err("HM3 exact native source evidence mismatch".into());
    }
    saved.push(sources);
    let foreign_read = read(&fixture, projects[1], source.version, None)?;
    if foreign_read
        .1
        .text
        .as_deref()
        .unwrap_or_default()
        .contains(&body)
    {
        diagnose(&fixture, foreign_read.0, &foreign_read.1, "read-foreign");
        return Err("HM3 foreign native answer disclosed source body".into());
    }
    saved.push(foreign_read);
    // Use the same argument syntax as successful creates, without cueing the expected denial.
    let foreign_body = format!(
        "HM3_PROOF_{:032x}",
        u128::from_ne_bytes(asura_platform::random_id())
    );
    let denied_prompt = format!(
        "Call memory exactly once with command create_note and body exactly {foreign_body:?}, source_version {}. Preserve the body bytes exactly. Do not call any other tool. Report the tool result.",
        hex(&source.version)
    );
    let denied = run_turn(
        &fixture,
        projects[1],
        &denied_prompt,
        "create-foreign-source",
    )?;
    let evidence = records(&fixture, denied.0)?;
    if evidence.len() != 1 {
        diagnose(&fixture, denied.0, &denied.1, "create-foreign-source");
        return Err(format!(
            "HM3 expected one denied source proposal, actual={}",
            evidence.len()
        )
        .into());
    }
    let (journal::Record::MemoryCreateIntent(intent), result) = &evidence[0] else {
        diagnose(&fixture, denied.0, &denied.1, "create-foreign-source");
        return Err("HM3 denied source intent absent".into());
    };
    if intent.project != projects[1]
        || intent.source.map(|(id, _)| id) != Some(source.version)
        || result.status != 4
        || !result.text.is_empty()
        || result.truncated
        || result.next_offset.is_some()
    {
        diagnose(&fixture, denied.0, &denied.1, "create-foreign-source");
        return Err("HM3 foreign source was not denied exactly".into());
    }
    saved.push(denied);
    let notes = vec![(source, body), (note, derived)];
    fixture.stop()?;
    verify_database(&fixture, projects[0], projects[1], &notes)?;
    let before = replay(&fixture)?.1.operations.len();
    fixture.start()?;
    wait_ready(&fixture, None)?;
    for (operation, event) in saved {
        let recovered = observe(&fixture, operation)?;
        if recovered.tools != event.tools || recovered.text != event.text {
            diagnose(&fixture, operation, &recovered, "restart-replay");
            return Err("HM3 restart changed durable response".into());
        }
    }
    fixture.stop()?;
    if replay(&fixture)?.1.operations.len() != before {
        return Err("HM3 restart admitted new work".into());
    }
    verify_database(&fixture, projects[0], projects[1], &notes)?;
    println!(
        "PASS native {provider} HM3 create/read/source, exact receipts, cross-project denial, restart without duplicate and owned cleanup"
    );
    Ok(())
}
