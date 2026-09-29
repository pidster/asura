//! Native discovery verifies committed canonical metadata, not a model's recollection.
use super::*;
use asura_storage::authority::conversation as journal;

pub(super) fn run() -> Result<()> {
    run_for(false)
}

pub(super) fn run_long_ollama() -> Result<()> {
    run_for(true)
}

fn run_for(long: bool) -> Result<()> {
    let mut fixture = Fixture::new()?;
    fixture.start()?;
    wait_ready(&fixture, None)?;
    let directory = fixture.home.join("inventory-project");
    fs::create_dir(&directory)?;
    let project = checked_id(
        fixture
            .attach()?
            .register_project(pb::ProjectRegister {
                request_id: Some(asura_platform::random_id().to_vec()),
                location: Some(directory.to_str().ok_or("invalid scratch path")?.into()),
            })?
            .project_id,
    )?;
    if long {
        let model =
            std::env::var("ASURA_TEST_OLLAMA_MODEL").unwrap_or_else(|_| "granite4.1:8b".into());
        let reply = fixture
            .attach()?
            .config("model", Some(&format!("ollama:{model}")))?;
        assert!(reply.error.is_none());
    }
    let accepted = submit(
        &fixture,
        project,
        None,
        0,
        if long {
            "Call service with command tools exactly once. Then write at least 180 words describing the listed tools, and end with the exact final marker ACTIVITY-RESPONSE-COMPLETE. Do not call other tools."
        } else {
            "Call service with command tools exactly once to inspect the registered tools. After it succeeds, reply only READY. Do not reproduce the inventory."
        },
    )?;
    let operation = checked_id(accepted.operation_id)?;
    let event = observe(&fixture, operation)?;
    if event.kind != Some(3) {
        fixture.print_model_diagnostics();
    }
    assert_eq!(event.kind, Some(3), "native inventory turn failed");
    let text = event.text.as_deref().unwrap_or_default();
    if long {
        assert!(
            text.split_whitespace().count() >= 150,
            "post-tool answer too short: {text}"
        );
        assert!(
            text.trim_end().ends_with("ACTIVITY-RESPONSE-COMPLETE"),
            "missing final marker: {text}"
        );
    } else {
        assert!(text.contains("READY"));
    }
    assert!(
        event
            .tools
            .iter()
            .any(|tool| tool.name.as_deref() == Some("service_list_tools")
                && tool.state == Some(2)
                && tool.status == Some(1))
    );
    fixture.stop()?;
    let bytes = fs::read(fixture.home.join(".asura/state/control/slot-0.log"))?;
    let replay = journal::replay(
        &bytes,
        Instant::now() + Duration::from_secs(2),
        &std::sync::atomic::AtomicBool::new(false),
    )
    .map_err(|error| format!("inventory replay: {error:?}"))?;
    let tools = &replay
        .operations
        .get(&operation)
        .ok_or("missing inventory operation")?
        .tools;
    let inventory: Vec<_> = tools.iter().filter(|tool| tool.kind == 14).collect();
    assert_eq!(
        inventory.len(),
        1,
        "exactly one committed inventory call required"
    );
    assert_eq!(inventory[0].result_status, Some(1));
    let frame = &bytes[inventory[0]
        .result_frame
        .clone()
        .ok_or("missing inventory result")?];
    let result = journal::decode_record(
        u16::from_be_bytes([frame[10], frame[11]]),
        &frame[144..frame.len() - 40],
    )
    .map_err(|error| format!("inventory result decode: {error:?}"))?;
    let journal::Record::ToolResult(result) = result else {
        return Err("wrong inventory record".into());
    };
    assert_eq!(
        result.text,
        asura_service::tools::inventory_text().map_err(|error| format!("inventory: {error:?}"))?
    );
    assert!(result.next_offset.is_none() && !result.truncated);
    fixture.start()?;
    wait_ready(&fixture, None)?;
    let recovered = observe(&fixture, operation)?;
    assert_eq!(recovered.text, event.text);
    assert_eq!(recovered.tools, event.tools);
    fixture.stop()?;
    println!(
        "PASS native tool inventory, exact canonical durable metadata, restart replay and cleanup"
    );
    Ok(())
}
