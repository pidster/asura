//! Native memory tools against isolated, canonically seeded project evidence.
use super::*;
use asura_storage::{authority::conversation as journal, memory};
fn id(n: u8) -> memory::Id {
    memory::Id::new([n; 16]).unwrap()
}
fn wait<T>(mut pending: memory::Pending<T>) -> Result<T> {
    let deadline = Instant::now() + Duration::from_secs(12);
    loop {
        if let Some(result) = pending.poll() {
            return result.map_err(Into::into);
        }
        if Instant::now() >= deadline {
            return Err("memory fixture deadline".into());
        }
        thread::sleep(Duration::from_millis(2));
    }
}
fn seed(fixture: &Fixture, project: [u8; 16], foreign: [u8; 16], fact: &str) -> Result<()> {
    let bytes = fs::read(fixture.home.join(".asura/state/control/slot-0.log"))?;
    let replay = journal::replay(
        &bytes,
        Instant::now() + Duration::from_secs(2),
        &std::sync::atomic::AtomicBool::new(false),
    )
    .map_err(|e| format!("seed replay:{e:?}"))?;
    let binding = memory::Binding {
        installation_id: memory::Id::new(replay.installation_id)?,
        graph_id: memory::Id::new(replay.initialization.graph)?,
        init_operation_id: memory::Id::new(replay.initialization.request)?,
    };
    let (mut database, ready) = memory::Memory::open(fixture.home.clone(), binding.clone())?;
    let result = (|| {
        wait(ready)?;
        for (version, context, body, source) in [
            (21, project, "Source evidence for the note".to_owned(), None),
            (22, project, fact.to_owned(), Some((id(21), id(61)))),
            (
                23,
                foreign,
                "FOREIGN_MEMORY_SECRET_NEVER_DISCLOSE".to_owned(),
                None,
            ),
        ] {
            wait(database.put_note(memory::PutNote {
                binding: binding.clone(),
                context_id: memory::Id::new(context)?,
                object_id: id(version + 10),
                version_id: id(version),
                operation_id: id(version + 30),
                body,
                source,
            })?)?;
        }
        let sources =
            wait(database.get_sources(binding.clone(), memory::Id::new(project)?, id(22))?)?;
        assert_eq!(sources.len(), 1, "seed must retain the exact direct source");
        assert_eq!(sources[0].version_id, id(21));
        Ok(())
    })();
    let deadline = Instant::now() + Duration::from_secs(12);
    loop {
        match database.close() {
            Ok(true) => break,
            Ok(false) | Err(memory::Error::Timeout) => (),
            Err(error) => return Err(error.into()),
        }
        if Instant::now() >= deadline {
            return Err("seed database did not settle".into());
        }
        thread::sleep(Duration::from_millis(2));
    }
    result
}
pub(super) fn run(provider: &str) -> Result<()> {
    let mut fixture = Fixture::new()?;
    // The standalone canonical memory adapter accepts only its private scratch naming contract.
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
    let fact = format!(
        "The project proof token is MEMORY_{:032x}",
        u128::from_ne_bytes(asura_platform::random_id())
    );
    fixture.stop()?;
    seed(&fixture, projects[0], projects[1], &fact)?;
    fixture.start()?;
    wait_ready(&fixture, None)?;
    let version = "16".repeat(16);
    let foreign = "17".repeat(16);
    let steps = [
        (
            "memory_list_notes",
            "Call memory with command list_notes with limit 8 and no after cursor. Report the available note version IDs. Do not use other tools.".to_string(),
            1,
        ),
        (
            "memory_get_note",
            format!(
                "Call memory with command get_note with version {version}, offset 0, limit 1024. Reply with the exact project proof token from the note. Do not guess and do not use other tools."
            ),
            1,
        ),
        (
            "memory_note_sources",
            format!(
                "Call memory with command note_sources with version {version}. Copy the full 32-character version ID from the returned source row exactly, without abbreviating it. Return only that source ID. Do not return the requested note ID and do not use other tools."
            ),
            1,
        ),
        (
            "memory_get_note",
            format!(
                "Call memory with command get_note with version {foreign}, offset 0, limit 1024. Report if it is unavailable. Do not use other tools."
            ),
            4,
        ),
    ];
    let mut saved = Vec::new();
    for (name, prompt, status) in steps {
        let accepted = submit(&fixture, projects[0], None, 0, &prompt)?;
        let operation = checked_id(accepted.operation_id)?;
        let event = observe(&fixture, operation)?;
        let records = committed_tools(&fixture, operation)?;
        let output = event.text.as_deref().unwrap_or_default();
        let expected_kind = match name {
            "memory_list_notes" => 6,
            "memory_get_note" => 7,
            _ => 8,
        };
        let requested_path = if name == "memory_list_notes" {
            ""
        } else if status == 4 {
            &foreign
        } else {
            &version
        };
        let expected_limit = match name {
            "memory_list_notes" => 8,
            "memory_get_note" => 1024,
            _ => 0,
        };
        let exact = records.iter().find(|(intent, result)| {
            intent.kind == expected_kind
                && intent.path == requested_path
                && intent.offset == 0
                && intent.limit == expected_limit
                && result.status == status as u8
        });
        let expected_sources = format!(
            "sources=1\n{}",
            source_line(21, "Source evidence for the note")
        );
        let expected_list = format!(
            "next=none\n{}{}",
            summary_line(21, "Source evidence for the note"),
            summary_line(22, &fact)
        );
        let failure = if event.kind != Some(3) {
            Some("native memory turn failed")
        } else if !event
            .tools
            .iter()
            .any(|tool| tool.name.as_deref() == Some(name) && tool.status == Some(status))
        {
            Some("required real tool event absent")
        } else if output.contains("FOREIGN_MEMORY_SECRET")
            || records
                .iter()
                .any(|(_, result)| result.text.contains("FOREIGN_MEMORY_SECRET"))
        {
            Some("foreign-context evidence disclosed")
        } else if let Some((_, result)) = exact {
            match name {
                "memory_list_notes"
                    if result.text != expected_list
                        || result.next_offset.is_some()
                        || result.truncated =>
                {
                    Some("committed list differs from exact seeded evidence")
                }
                "memory_note_sources"
                    if result.text != expected_sources
                        || result.next_offset.is_some()
                        || result.truncated =>
                {
                    Some("committed sources differ from exact seeded provenance")
                }
                "memory_get_note"
                    if status == 1
                        && (result.text != fact
                            || result.next_offset != Some(fact.len() as u64)
                            || result.truncated
                            || !output.contains(fact.split_whitespace().last().unwrap())) =>
                {
                    Some("stored proof or final grounded answer mismatch")
                }
                "memory_get_note" if status == 4 && result.text != "memory_note_not_found" => {
                    Some("foreign note did not produce fixed unavailable reason")
                }
                _ => None,
            }
        } else {
            Some("committed result did not match exact requested tool and arguments")
        };
        if let Some(reason) = failure {
            fixture.stop()?;
            diagnose(&fixture, operation, &event)?;
            return Err(format!("native memory {name}: {reason}").into());
        }
        saved.push((operation, event));
    }
    fixture.stop()?;
    let bytes = fs::read(fixture.home.join(".asura/state/control/slot-0.log"))?;
    let replay = journal::replay(
        &bytes,
        Instant::now() + Duration::from_secs(2),
        &std::sync::atomic::AtomicBool::new(false),
    )
    .map_err(|e| format!("memory replay:{e:?}"))?;
    for (operation, event) in &saved {
        assert_eq!(replay.operations[operation].tools.len(), event.tools.len());
    }
    fixture.start()?;
    wait_ready(&fixture, None)?;
    for (operation, event) in saved {
        let recovered = observe(&fixture, operation)?;
        assert_eq!(recovered.tools, event.tools);
        assert_eq!(recovered.text, event.text);
    }
    fixture.stop()?;
    println!("PASS native {provider} memory list/get/sources, scope denial and retained replay");
    Ok(())
}

// Bounded fixture diagnostics only: this database contains exclusively generated test data.
fn diagnose(fixture: &Fixture, operation: [u8; 16], event: &pb::ConversationEvent) -> Result<()> {
    eprintln!(
        "memory fixture final output: {:?}",
        event
            .text
            .as_deref()
            .unwrap_or_default()
            .chars()
            .take(2048)
            .collect::<String>()
    );
    let bytes = fs::read(fixture.home.join(".asura/state/control/slot-0.log"))?;
    let replay = journal::replay(
        &bytes,
        Instant::now() + Duration::from_secs(2),
        &std::sync::atomic::AtomicBool::new(false),
    )
    .map_err(|e| format!("diagnostic replay:{e:?}"))?;
    for tool in replay.operations[&operation].tools.iter().take(8) {
        // Fixed journal-format-1 header/trailer; replay above has validated these exact frames.
        for range in [Some(tool.intent_frame.clone()), tool.result_frame.clone()]
            .into_iter()
            .flatten()
        {
            let frame = &bytes[range];
            let kind = u16::from_be_bytes([frame[10], frame[11]]);
            match journal::decode_record(kind, &frame[144..frame.len() - 40])
                .map_err(|e| format!("diagnostic record:{e:?}"))?
            {
                journal::Record::ToolIntent(intent) => eprintln!(
                    "memory fixture intent kind={} ordinal={} offset={} limit={} argument={:?}",
                    intent.kind,
                    intent.ordinal,
                    intent.offset,
                    intent.limit,
                    intent.path.chars().take(1024).collect::<String>()
                ),
                journal::Record::ToolResult(result) => eprintln!(
                    "memory fixture result status={} text={:?}",
                    result.status,
                    result.text.chars().take(2048).collect::<String>()
                ),
                _ => (),
            }
        }
    }
    Ok(())
}

fn committed_tools(
    fixture: &Fixture,
    operation: [u8; 16],
) -> Result<Vec<(journal::ToolIntent, journal::ToolResult)>> {
    use std::io::Read;
    let mut bytes = Vec::new();
    fs::File::open(fixture.home.join(".asura/state/control/slot-0.log"))?
        .take(asura_storage::authority::MAX_JOURNAL as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > asura_storage::authority::MAX_JOURNAL {
        return Err("fixture journal exceeds bound".into());
    }
    let replay = journal::replay(
        &bytes,
        Instant::now() + Duration::from_secs(2),
        &std::sync::atomic::AtomicBool::new(false),
    )
    .map_err(|e| format!("evidence replay:{e:?}"))?;
    let mut records = Vec::new();
    for tool in replay
        .operations
        .get(&operation)
        .ok_or("operation missing from evidence")?
        .tools
        .iter()
        .take(8)
    {
        let Some(result_range) = tool.result_frame.clone() else {
            continue;
        };
        let decode = |range: std::ops::Range<usize>| -> Result<journal::Record> {
            // Fixed journal-format-1 envelope, validated by canonical replay above.
            let frame = &bytes[range];
            journal::decode_record(
                u16::from_be_bytes([frame[10], frame[11]]),
                &frame[144..frame.len() - 40],
            )
            .map_err(|e| format!("evidence decode:{e:?}").into())
        };
        if let (journal::Record::ToolIntent(intent), journal::Record::ToolResult(result)) =
            (decode(tool.intent_frame.clone())?, decode(result_range)?)
        {
            records.push((intent, result));
        }
    }
    Ok(records)
}

fn source_line(version: u8, body: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(body.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    format!(
        "version={} object={} sha256={digest}\n",
        format!("{version:02x}").repeat(16),
        format!("{:02x}", version + 10).repeat(16)
    )
}
fn summary_line(version: u8, body: &str) -> String {
    let line = source_line(version, body);
    format!(
        "{} preview=\"{}\"\n",
        line.trim_end(),
        body.chars()
            .flat_map(char::escape_debug)
            .collect::<String>()
    )
}
