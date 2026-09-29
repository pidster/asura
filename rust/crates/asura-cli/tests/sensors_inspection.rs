//! Actual standalone command against an owned private service.
use super::*;
use asura_control::pb;
pub(super) fn inspect(case: &mut Case) -> Result<()> {
    let invalid = case.command(&["sensors", "00000000000000000000000000000000"])?;
    require(
        invalid.status.code() == Some(2),
        "invalid sensor ID accepted",
    )?;
    require(
        case.command(&["service", "start"])?.status.success(),
        "sensor service start failed",
    )?;
    let runtime = RuntimeDirectory::scratch(&case.root.join("home"), false)?;
    let end = Instant::now() + WAIT;
    let mut client = Client::attach(&runtime, BUILD, end)?;
    while client.inspect_installation()?.installation != pb::InstallationState::GraphReady {
        require(Instant::now() < end, "sensor installation unavailable")?;
        std::thread::sleep(Duration::from_millis(20));
    }
    let project = case.root.join("project");
    fs::create_dir(&project)?;
    let registered = client.register_project(pb::ProjectRegister {
        request_id: Some(asura_platform::random_id().to_vec()),
        location: Some(project.to_str().unwrap().into()),
    })?;
    require(
        registered.error.is_none(),
        "sensor project registration failed",
    )?;
    let id: String = registered
        .project_id
        .unwrap()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let page = loop {
        let result = case.command(&["sensors", &id])?;
        if result.status.success() {
            break result;
        }
        require(
            Instant::now() < end && result.stderr.contains("sensor_loading"),
            "sensor CLI failed",
        )?;
    };
    require(
        page.stdout.contains("No committed observations.")
            && page.stdout.contains("No committed proposals."),
        "sensor empty page missing",
    )?;
    let conflict = case.command(&["sensors", &id, "0", "18446744073709551615"])?;
    require(
        conflict.status.code() == Some(3) && conflict.stderr.contains("revision_conflict"),
        "sensor conflict not exposed",
    )?;
    require(
        case.command(&["service", "stop"])?.status.success(),
        "sensor service stop failed",
    )?;
    Ok(())
}
