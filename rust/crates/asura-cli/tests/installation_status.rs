//! Installation journeys share the isolated copied CLI executable and cleanup owner.
use super::*;

fn inspect(case: &mut Case) -> Result<serde_json::Value> {
    let deadline = Instant::now() + WAIT;
    loop {
        let output = case.command(&["installation", "status", "--json"])?;
        require(output.status.success(), "installation inspection failed")?;
        let value: serde_json::Value = serde_json::from_str(&output.stdout)?;
        require(
            value.as_object().unwrap().len() == 10,
            "inspection field count",
        )?;
        if value["reason"] != "inspection_pending" {
            return Ok(value);
        }
        require(
            Instant::now() < deadline,
            "installation scan did not settle",
        )?;
        std::thread::sleep(Duration::from_millis(10));
    }
}
fn no_identity(value: &serde_json::Value) -> Result<()> {
    for field in [
        "installation_id",
        "authority_revision",
        "recorded_owner_generation",
        "binding_generation",
        "authority_format",
    ] {
        require(
            value[field].is_null(),
            "failed replay disclosed authority identity",
        )?;
    }
    Ok(())
}
pub(super) fn absent(case: &mut Case) -> Result<()> {
    let output = case.command(&["installation", "status", "--json"])?;
    require(output.status.code() == Some(3), "absent inspection exit")?;
    let value: serde_json::Value = serde_json::from_str(&output.stdout)?;
    require(
        value["reason"] == "service_absent" && value["error_code"] == "service_absent",
        "absent reason",
    )?;
    require(value["service_epoch"].is_null(), "absent epoch fabricated")?;
    no_identity(&value)?;
    require(
        !case.root.join("home/.asura").exists(),
        "inspection created runtime",
    )
}
fn start(case: &mut Case) -> Result<()> {
    let output = case.command(&["service", "start"])?;
    require(output.status.success(), "fixture service start failed")
}
fn ready(case: &mut Case) -> Result<serde_json::Value> {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let value = inspect(case)?;
        if value["installation"] == "graph_ready" {
            require(
                value["reason"] == "graph_verified",
                "unverified graph reported ready",
            )?;
            require(
                value["error_code"].is_null(),
                "ready inspection reported error",
            )?;
            return Ok(value);
        }
        require(
            Instant::now() < deadline,
            &format!("automatic initialization did not become ready: {value}"),
        )?;
        std::thread::sleep(Duration::from_millis(10));
    }
}
pub(super) fn runtime(case: &mut Case) -> Result<()> {
    let root = case.root.join("home/.asura");
    fs::create_dir_all(&root)?;
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700))?;
    let finder = root.join(".DS_Store");
    fs::write(&finder, b"Finder metadata fixture")?;
    fs::set_permissions(&finder, fs::Permissions::from_mode(0o644))?;
    start(case)?;
    let value = ready(case)?;
    let installation = value["installation_id"].clone();
    require(
        installation.as_str().is_some_and(|id| id.len() == 32),
        "installation identity missing",
    )?;
    require(
        value["binding_generation"] == 1 && value["authority_format"] == 1,
        "automatic binding metadata",
    )?;
    let human = case.command(&["installation", "status"])?;
    require(
        human.status.success()
            && human.stdout.is_empty()
            && human
                .stderr
                .contains("Installation: graph_ready (graph_verified)"),
        "human status",
    )?;
    require(
        case.status()?["installation"] == "graph_ready",
        "service status disagrees",
    )?;
    require(
        case.root.join("home/.asura/db").is_dir(),
        "automatic database missing",
    )?;
    let journal = case.root.join("home/.asura/state/control/slot-0.log");
    let original = fs::read(&journal)?;
    require(
        !original.is_empty(),
        "automatic initialization journal missing",
    )?;
    require(
        case.command(&["service", "stop"])?.status.success(),
        "fixture stop failed",
    )?;
    start(case)?;
    let restarted = ready(case)?;
    require(
        fs::read(&finder)? == b"Finder metadata fixture",
        "startup modified Finder metadata",
    )?;
    require(
        restarted["installation_id"] == installation,
        "restart replaced installation",
    )?;
    require(
        restarted["service_epoch"] != value["service_epoch"],
        "restart retained service epoch",
    )?;
    require(
        restarted["binding_generation"] == value["binding_generation"],
        "restart replaced binding",
    )?;
    require(
        fs::read(journal)?.starts_with(&original),
        "restart overwrote initialization evidence",
    )
}
pub(super) fn remnants(case: &mut Case) -> Result<()> {
    RuntimeDirectory::scratch(&case.root.join("home"), true)?;
    let path = case.root.join("home/.asura/config.yaml");
    fs::write(&path, b"preserve")?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
    // Configuration alone is no longer installation evidence; actual state remains so.
    fs::DirBuilder::new()
        .mode(0o700)
        .create(case.root.join("home/.asura/state"))?;
    start(case)?;
    let value = inspect(case)?;
    require(
        value["reason"] == "installation_remnants",
        "remnant classification",
    )?;
    no_identity(&value)?;
    require(fs::read(path)? == b"preserve", "remnant modified")?;
    require(
        !case.root.join("home/.asura/db").exists(),
        "remnants triggered database creation",
    )?;
    require(
        !case.root.join("home/.asura/state/control").exists(),
        "remnants triggered journal creation",
    )
}
fn journal(case: &mut Case, bytes: &[u8], reason: &str, revision: Option<u64>) -> Result<()> {
    RuntimeDirectory::scratch(&case.root.join("home"), true)?;
    let root = case.root.join("home/.asura/state");
    fs::DirBuilder::new().mode(0o700).create(&root)?;
    fs::DirBuilder::new()
        .mode(0o700)
        .create(root.join("control"))?;
    let path = root.join("control/slot-0.log");
    fs::write(&path, bytes)?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
    let mut prior_epoch = serde_json::Value::Null;
    for _ in 0..2 {
        start(case)?;
        let value = inspect(case)?;
        require(
            value["reason"] == reason,
            &format!("journal classification: {value}"),
        )?;
        require(
            value["error_code"].is_null(),
            "valid reply reported client error",
        )?;
        require(
            value["service_epoch"] != prior_epoch,
            "restart retained epoch",
        )?;
        prior_epoch = value["service_epoch"].clone();
        if let Some(revision) = revision {
            require(
                value["authority_revision"] == revision
                    && value["authority_format"] == 1
                    && value["recorded_owner_generation"] == 1
                    && value["installation_id"] == "11111111111111111111111111111111",
                "replay identity",
            )?;
            if revision == 2 {
                require(
                    value["installation"] == "graph_unavailable"
                        && value["binding_generation"] == 1,
                    "binding readiness fabricated",
                )?;
            } else {
                require(
                    value["installation"] == "recovering" && value["binding_generation"].is_null(),
                    "pending binding fabricated",
                )?;
            }
        } else {
            require(
                value["installation"] == "repair_required",
                "invalid journal not repair required",
            )?;
            no_identity(&value)?;
        }
        require(fs::read(&path)? == bytes, "inspection modified journal")?;
        require(
            !case.root.join("home/.asura/db").exists(),
            "existing authority triggered replacement database",
        )?;
        require(
            case.command(&["service", "stop"])?.status.success(),
            "fixture stop failed",
        )?;
    }
    Ok(())
}
const PENDING: &[u8] = include_bytes!("../../asura-storage/tests/fixtures/pending.bin");
const ACTIVE: &[u8] = include_bytes!("../../asura-storage/tests/fixtures/active.bin");
pub(super) fn pending(case: &mut Case) -> Result<()> {
    journal(case, PENDING, "initialization_pending", Some(1))
}
pub(super) fn active(case: &mut Case) -> Result<()> {
    journal(case, ACTIVE, "graph_verification_unavailable", Some(2))
}
pub(super) fn corrupt(case: &mut Case) -> Result<()> {
    let mut bytes = PENDING.to_vec();
    bytes[150] ^= 1;
    journal(case, &bytes, "corrupt_authority", None)
}
pub(super) fn incomplete(case: &mut Case) -> Result<()> {
    journal(case, &PENDING[..PENDING.len() - 1], "incomplete_tail", None)
}
pub(super) fn version(case: &mut Case) -> Result<()> {
    let mut bytes = PENDING.to_vec();
    bytes[9] = 2;
    journal(case, &bytes, "unsupported_format", None)
}
pub(super) fn log_failure(case: &mut Case) -> Result<()> {
    fs::DirBuilder::new()
        .mode(0o700)
        .create(case.root.join("logs"))?;
    symlink("missing", case.root.join("logs/asura.log"))?;
    let output = case.command(&["installation", "status", "--json", "--logs", "logs"])?;
    require(output.status.code() == Some(3), "log failure exit")?;
    let value: serde_json::Value = serde_json::from_str(&output.stdout)?;
    require(value["error_code"] == "log_unavailable", "log failure JSON")?;
    no_identity(&value)?;
    require(
        !case.root.join("home/.asura").exists(),
        "log failure accessed runtime",
    )
}

pub(super) fn incompatible(case: &mut Case) -> Result<()> {
    let runtime = RuntimeDirectory::scratch(&case.root.join("home"), true)?;
    let mut guard = runtime.acquire_owner()?;
    let listener = guard.bind()?;
    let peer = std::thread::spawn(move || -> std::result::Result<(), String> {
        let deadline = Instant::now() + WAIT;
        let result = (|| -> Result<()> {
            let (mut stream, _) = loop {
                match listener.accept() {
                    Ok(pair) => break pair,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        require(Instant::now() < deadline, "version fixture accept timeout")?;
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(error) => return Err(error.into()),
                }
            };
            stream.set_nonblocking(false)?;
            stream.set_read_timeout(Some(Duration::from_secs(2)))?;
            stream.set_write_timeout(Some(Duration::from_secs(2)))?;
            let mut header = [0u8; 12];
            stream.read_exact(&mut header)?;
            let length = u32::from_be_bytes(header[8..12].try_into()?) as usize;
            require(length <= asura_control::MAX_FRAME_BYTES, "unbounded hello")?;
            let mut body = vec![0; length];
            stream.read_exact(&mut body)?;
            let mut rejection = asura_control::version_rejection();
            rejection[5] = 1;
            rejection[7] = 0; // An old service advertises exact protocol 1.0.
            stream.write_all(&rejection)?;
            Ok(())
        })();
        let cleanup = guard.remove_endpoint();
        drop(listener);
        drop(guard);
        result.map_err(|error| error.to_string())?;
        cleanup.map_err(|error| error.to_string())
    });
    let output = case.command(&["installation", "status", "--json"]);
    peer.join().map_err(|_| "version fixture panicked")??;
    let output = output?;
    require(
        output.status.code() == Some(4),
        "incompatible inspection exit",
    )?;
    let value: serde_json::Value = serde_json::from_str(&output.stdout)?;
    require(
        value["error_code"] == "incompatible_protocol",
        "incompatible JSON",
    )?;
    no_identity(&value)?;
    require(
        !case.root.join("home/.asura/db").exists() && !case.root.join("home/.asura/state").exists(),
        "incompatible inspection initialized installation",
    )
}
