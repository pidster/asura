//! Process-level setup/restart and opt-in native generation in an isolated home.
//! Every spawned service is owned by a guard that stops, kills if needed, and reaps.
#[path = "support/audit_journey.rs"]
mod audit_journey;
#[path = "support/cancellation_driven_journey.rs"]
mod cancellation_driven_journey;
#[path = "support/exec_cmd_journey.rs"]
mod exec_cmd_journey;
#[path = "support/memory_create_journey.rs"]
mod memory_create_journey;
#[path = "support/memory_tools_journey.rs"]
mod memory_tools_journey;
#[path = "support/queue_submit.rs"]
mod queue_submit;
#[path = "support/sensors_journey.rs"]
mod sensors_journey;
#[path = "support/tools_inventory_journey.rs"]
mod tools_inventory_journey;
#[path = "support/waiting_step_journey.rs"]
mod waiting_step_journey;
use asura_client::Client;
use asura_control::pb;
use asura_platform::RuntimeDirectory;
use std::{
    error::Error,
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};
type Result<T> = std::result::Result<T, Box<dyn Error>>;
const BUILD: &str = "asura/conversation-test";
struct Fixture {
    home: PathBuf,
    child: Option<Child>,
    /// The service executable; a relocated package copy when a journey sets it.
    executable: PathBuf,
}
impl Fixture {
    fn new() -> Result<Self> {
        let nonce: String = asura_platform::random_id()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let home = PathBuf::from(format!(
            "/private/tmp/asura-conversation-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir(&home)?;
        fs::set_permissions(&home, fs::Permissions::from_mode(0o700))?;
        Ok(Self {
            home,
            child: None,
            executable: std::env::current_exe()?,
        })
    }
    fn runtime(&self) -> Result<RuntimeDirectory> {
        Ok(RuntimeDirectory::scratch(&self.home, true)?)
    }
    fn start(&mut self) -> Result<()> {
        assert!(self.child.is_none());
        let log = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.home.join("service.log"))?;
        self.child = Some(
            Command::new(&self.executable)
                .arg("--conversation-child")
                .arg(&self.home)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(log)
                .spawn()?,
        );
        self.attach()?;
        Ok(())
    }
    fn attach(&self) -> Result<Client> {
        let runtime = self.runtime()?;
        let end = Instant::now() + Duration::from_secs(5);
        loop {
            // Test-only harness budget: client waits have no time bound in
            // production (owner, 2026-10-16); a stalled fixture fails here.
            let harness = Instant::now() + Duration::from_secs(180);
            let cancel: asura_client::Cancel =
                std::sync::Arc::new(move || Instant::now() >= harness);
            match Client::attach(&runtime, BUILD, &*cancel) {
                Ok(mut client) => {
                    client.set_cancel(cancel);
                    return Ok(client);
                }
                Err(error) if Instant::now() >= end => return Err(error.into()),
                Err(_) => thread::sleep(Duration::from_millis(10)),
            }
        }
    }
    fn print_model_diagnostics(&self) {
        use std::io::Read;
        let Ok(file) = fs::File::open(self.home.join("service.log")) else {
            return;
        };
        let mut bytes = Vec::new();
        if file.take(65_536).read_to_end(&mut bytes).is_err() {
            return;
        }
        let allowed = [
            "context_limit",
            "rate_limited",
            "guardrail",
            "refusal",
            "unsupported_capability",
            "unsupported_transcript",
            "unsupported_guide",
            "unsupported_language",
            "timeout",
            "unknown_sdk",
            "helper_protocol",
            "helper_limit",
            "helper_closed",
            "helper_unavailable",
            "helper_context",
            "helper_timeout",
            "task_cancelled",
            "provider_cancelled",
            "cancelled",
            "backend_failure",
            "generated_content",
            "generated_content_empty",
            "generated_content_oversized",
            "generated_content_object",
            "generated_content_array",
            "generated_content_scalar",
            "generated_content_invalid_json",
            "decoding",
            "tool_callback",
            "session_state",
            "unclassified",
            "model_helper_failed",
            "model_protocol_fault",
        ];
        for line in String::from_utf8_lossy(&bytes)
            .lines()
            .filter(|line| line.contains("model_helper_diagnostic"))
            .take(16)
        {
            let class = line
                .split_whitespace()
                .find_map(|field| field.strip_prefix("class="))
                .map(|value| value.trim_matches('"'));
            let code = line
                .split_whitespace()
                .find_map(|field| field.strip_prefix("code="))
                .and_then(|value| value.parse::<i64>().ok());
            if let Some(class) = class.filter(|class| allowed.contains(class)) {
                eprintln!(
                    "native model diagnostic: {class} code={}",
                    code.unwrap_or(0)
                );
            }
        }
    }
    fn stop(&mut self) -> Result<()> {
        if self.child.is_none() {
            return Ok(());
        }
        let _ = self
            .attach()
            .and_then(|client| client.stop().map_err(Into::into));
        let end = Instant::now() + Duration::from_secs(6);
        while Instant::now() < end {
            if let Some(status) = self.child.as_mut().unwrap().try_wait()? {
                self.child.take();
                if !status.success() {
                    return Err(format!("service exit {status}").into());
                }
                return Ok(());
            }
            thread::sleep(Duration::from_millis(10));
        }
        let child = self.child.as_mut().unwrap();
        child.kill()?;
        child.wait()?;
        self.child.take();
        Err("service failed clean shutdown; killed and reaped".into())
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        // Give the canonical drain owner its cleanup budget, including helper
        // settlement, even when an assertion or native-generation check fails.
        let _ = self.stop();
        if let Some(child) = self.child.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
        let _ = fs::remove_dir_all(&self.home);
    }
}
fn checked_id(id: Option<Vec<u8>>) -> Result<[u8; 16]> {
    Ok(id
        .ok_or("missing ID")?
        .try_into()
        .map_err(|_| "wrong ID length")?)
}
fn observe(fixture: &Fixture, operation: [u8; 16]) -> Result<pb::ConversationEvent> {
    let end = Instant::now() + Duration::from_secs(75);
    let mut observer = Observer::new(operation);
    while Instant::now() < end {
        if let Some(event) = observer.step(fixture)? {
            return Ok(event);
        }
    }
    Err("native operation observation deadline".into())
}
/// One operation's observation, advanced one paced poll at a time. The caller
/// chooses the closure: a harness bound, a terminal event or another condition.
struct Observer {
    operation: [u8; 16],
    // Windows assemble the whole response (response-text-transfer.md).
    assembly: asura_client::ResponseAssembly,
    client: Option<(Client, Instant)>,
}
impl Observer {
    fn new(operation: [u8; 16]) -> Self {
        Self {
            operation,
            assembly: asura_client::ResponseAssembly::default(),
            client: None,
        }
    }
    /// Polls once and paces; returns the whole event once the turn is terminal.
    fn step(&mut self, fixture: &Fixture) -> Result<Option<pb::ConversationEvent>> {
        // An attached client carries the fixture's per-client harness budget,
        // so a long wait renews the attachment instead of outliving it.
        if self
            .client
            .as_ref()
            .is_none_or(|(_, attached)| attached.elapsed() >= Duration::from_secs(60))
        {
            self.client = Some((fixture.attach()?, Instant::now()));
        }
        let (client, _) = self.client.as_mut().ok_or("observer client absent")?;
        let event = match client.observe_with_thinking(
            self.operation,
            self.assembly.cursor(),
            self.assembly.held(),
            self.assembly.held_thinking(),
        ) {
            Ok(event) => event,
            Err(asura_client::Error::Remote(_, message)) if message == "conversation_busy" => {
                thread::sleep(Duration::from_millis(10));
                return Ok(None);
            }
            Err(error) => return Err(error.into()),
        };
        self.assembly.apply(&event)?;
        if self.assembly.done() {
            return Ok(Some(self.assembly.whole(event)));
        }
        if self.assembly.complete() {
            thread::sleep(Duration::from_millis(25));
        }
        Ok(None)
    }
}
/// Queues one input and waits for its turn; there is no direct submission.
fn submit(
    fixture: &Fixture,
    project: [u8; 16],
    conversation: Option<Vec<u8>>,
    generation: u64,
    prompt: &str,
) -> Result<queue_submit::Accepted> {
    queue_submit::submit_queued(
        &|| fixture.attach(),
        project,
        conversation,
        generation,
        prompt,
    )
}
fn wait_ready(
    fixture: &Fixture,
    installation: Option<Vec<u8>>,
) -> Result<asura_client::InstallationSnapshot> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let status = fixture.attach()?.inspect_installation()?;
        if status.installation == pb::InstallationState::GraphReady
            && (installation.is_none()
                || status.installation_id.as_ref().map(|id| id.to_vec()) == installation)
        {
            return Ok(status);
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "installation did not become ready: {:?}/{:?}",
                status.installation, status.reason
            )
            .into());
        }
        thread::sleep(Duration::from_millis(20));
    }
}
fn journey(native: bool) -> Result<()> {
    if !native {
        preservation_journeys()?;
    }
    let mut fixture = Fixture::new()?;
    fixture.start()?;
    let initialized = wait_ready(&fixture, None)?;
    let installation = initialized.installation_id.map(|id| id.to_vec());
    println!("PASS fresh startup initialized without an initialization command");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !fixture.attach()?.inspect()?.new_installation {
        if Instant::now() >= deadline {
            return Err("automatic initialization did not report a new installation".into());
        }
        thread::sleep(Duration::from_millis(20));
    }
    println!("PASS automatic initialization reports a new installation for this run");
    let project_dir = fixture.home.join("project");
    fs::create_dir(&project_dir)?;
    fs::set_permissions(&project_dir, fs::Permissions::from_mode(0o700))?;
    let registration_request = asura_platform::random_id().to_vec();
    let registered = fixture.attach()?.register_project(
        pb::ProjectRegister {
            request_id: Some(registration_request.clone()),
            location: Some(project_dir.to_str().ok_or("non-UTF-8 path")?.into()),
        },
        &|| false,
    )?;
    if let Some(error) = registered.error {
        return Err(error.into());
    }
    let project = checked_id(registered.project_id)?;
    remote_endpoint_restriction(&fixture, project)?;
    let listed = fixture
        .attach()?
        .projects(asura_client::ProjectPage::First, 8)?;
    assert_eq!(listed.projects.len(), 1);
    assert_eq!(
        listed.projects[0].project_id.as_deref(),
        Some(project.as_slice())
    );
    assert_eq!(listed.projects[0].name.as_deref(), Some("Project"));
    assert_eq!(listed.projects[0].name_revision, Some(0));
    let rename = pb::ProjectRename {
        request_id: Some(asura_platform::random_id().to_vec()),
        project_id: Some(project.to_vec()),
        expected_name_revision: Some(0),
        name: Some("my project".into()),
    };
    let renamed = fixture
        .attach()?
        .rename_project(rename.clone(), &|| false)?;
    assert_eq!(renamed.error, None);
    assert_eq!(renamed.changed, Some(true));
    let renamed_project = renamed.project.ok_or("rename omitted project")?;
    assert_eq!(renamed_project.name.as_deref(), Some("My project"));
    assert_eq!(renamed_project.name_revision, Some(1));
    assert_eq!(
        renamed_project.project_id.as_deref(),
        Some(project.as_slice())
    );
    assert_eq!(renamed_project.location, listed.projects[0].location);
    let stale = fixture.attach()?.rename_project(
        pb::ProjectRename {
            request_id: Some(asura_platform::random_id().to_vec()),
            project_id: Some(project.to_vec()),
            expected_name_revision: Some(0),
            name: Some("different".into()),
        },
        &|| false,
    )?;
    assert_eq!(stale.error.as_deref(), Some("stale_project_name_revision"));
    assert_eq!(
        stale.project.as_ref().and_then(|p| p.name.as_deref()),
        Some("My project")
    );
    let retry = fixture
        .attach()?
        .rename_project(rename.clone(), &|| false)?;
    assert_eq!(
        retry.project.as_ref().and_then(|p| p.name_revision),
        Some(1)
    );
    let same_name = fixture.attach()?.rename_project(
        pb::ProjectRename {
            request_id: Some(asura_platform::random_id().to_vec()),
            project_id: Some(project.to_vec()),
            expected_name_revision: Some(1),
            name: Some("my project".into()),
        },
        &|| false,
    )?;
    assert_eq!(same_name.changed, Some(false));
    assert_eq!(
        same_name.project.as_ref().and_then(|p| p.name_revision),
        Some(2)
    );
    let late_retry = fixture.attach()?.rename_project(rename, &|| false)?;
    assert_eq!(
        late_retry.project.as_ref().and_then(|p| p.name_revision),
        Some(1)
    );
    assert_eq!(
        late_retry
            .current_project
            .as_ref()
            .and_then(|p| p.name_revision),
        Some(2)
    );
    assert_eq!(
        fixture
            .attach()?
            .projects(asura_client::ProjectPage::First, 8)?
            .projects[0]
            .name_revision,
        Some(2)
    );
    let empty = fixture
        .attach()?
        .conversation_history(project, None, None, 8)?;
    assert_eq!(empty.project_id.as_deref(), Some(project.as_slice()));
    assert!(empty.conversation_id.is_none());
    assert!(empty.entries.is_empty());
    assert_eq!(empty.has_more, Some(false));
    let unknown = fixture
        .attach()?
        .conversation_history([88; 16], None, None, 8);
    assert!(
        matches!(unknown, Err(asura_client::Error::Remote(_, ref message)) if message == "invalid_request")
    );
    println!("PASS empty project history and non-disclosing unknown project rejection");
    fixture.stop()?;
    fixture.start()?;
    wait_ready(&fixture, installation.clone())?;
    assert!(
        !fixture.attach()?.inspect()?.new_installation,
        "a restarted service must not report a new installation"
    );
    let repeated = fixture.attach()?.initialize(
        pb::InitializeInstallation {
            request_id: Some(asura_platform::random_id().to_vec()),
            mode: Some("embedded".into()),
            expected_authority_revision: Some(0),
        },
        &|| false,
    )?;
    assert_eq!(
        repeated.error.as_deref(),
        Some("installation_already_initialized")
    );
    wait_ready(&fixture, installation.clone())?;
    let after_restart = fixture
        .attach()?
        .projects(asura_client::ProjectPage::First, 8)?;
    assert_eq!(after_restart.projects.len(), 1);
    assert_eq!(
        after_restart.projects[0].name.as_deref(),
        Some("My project")
    );
    assert_eq!(after_restart.projects[0].name_revision, Some(2));
    let alias = fixture.attach()?.register_project(
        pb::ProjectRegister {
            request_id: Some(registration_request),
            location: Some(project_dir.to_str().ok_or("non-UTF-8 path")?.into()),
        },
        &|| false,
    )?;
    assert_eq!(alias.name.as_deref(), Some("My project"));
    assert_eq!(alias.name_revision, Some(2));
    println!("PASS setup, registration, list, clean stop and durable restart");
    if !native {
        managed_queue_journey(&mut fixture, project, installation.clone())?;
    }
    if native {
        let original = queue_submit::queue_request(
            project,
            None,
            0,
            "For this conversation only, remember the word ORCHARD. Reply with just ORCHARD. Do not call tools.",
        );
        let receipt = managed_submit_call(&fixture, original.clone())?;
        let accepted = queue_submit::dispatched(
            &|| fixture.attach(),
            project,
            checked_id(receipt.accepted_input_id.clone())?,
        )?;
        let operation = checked_id(accepted.operation_id.clone())?;
        let before_repeat = fixture.attach()?.observe_conversation(operation, 0, 0)?;
        assert!(
            matches!(before_repeat.kind, Some(1 | 2)),
            "need active admission to prove idempotent replay bypasses busy"
        );
        let repeated = managed_submit_call(&fixture, original.clone())?;
        assert_eq!(
            repeated.accepted_input_id, receipt.accepted_input_id,
            "same request must retain exact input identity"
        );
        assert_eq!(
            queue_submit::dispatched(&|| fixture.attach(), project, accepted.input_id)?,
            accepted,
            "an exact retry never dispatches a second turn"
        );
        let mut conflicting = original;
        conflicting.prompt = Some("Changed prompt with the same request identity".into());
        let conflict = fixture
            .attach()?
            .queue_conversation_input(conflicting, &|| false)
            .expect_err("changed digest must conflict");
        assert!(
            matches!(conflict,asura_client::Error::Remote(_,ref message) if message=="request_conflict")
        );
        println!("PASS active duplicate admission retains identities; changed digest conflicts");
        let start = Instant::now();
        fixture.attach()?.inspect()?;
        let elapsed = start.elapsed();
        println!("Inspect during generation: {} ms", elapsed.as_millis());
        assert!(
            elapsed < Duration::from_millis(100),
            "inspect response exceeded 100 ms target"
        );
        let event = observe(&fixture, operation)?;
        if event.kind != Some(3) {
            fixture.print_model_diagnostics();
            return Err(format!(
                "native inference not complete: kind {:?}, reason {:?}",
                event.kind, event.reason
            )
            .into());
        }
        assert!(
            event
                .text
                .as_deref()
                .unwrap_or_default()
                .contains("ORCHARD")
        );
        println!("PASS first native completion");
        let context = event
            .model_context
            .as_ref()
            .expect("native context measurement");
        assert!(!context.model_name.as_deref().unwrap_or_default().is_empty());
        assert_eq!(context.basis, Some(1));
        let input = context.input_tokens.expect("measured input tokens");
        let capacity = context.capacity_tokens.expect("native context capacity");
        assert!(input > 0 && input <= capacity);
        assert_eq!(context.reported_max_tokens, Some(capacity));
        assert_eq!(context.capacity_source, Some(1));
        let second = submit(
            &fixture,
            project,
            accepted.conversation_id.clone(),
            accepted.generation.unwrap(),
            "What word did I ask you to remember? Reply with only that word.",
        )?;
        let second_event = observe(&fixture, checked_id(second.operation_id.clone())?)?;
        assert_eq!(second_event.kind, Some(3));
        assert!(
            second_event
                .text
                .as_deref()
                .unwrap_or_default()
                .contains("ORCHARD"),
            "committed history was not reflected in response"
        );
        println!("PASS second native completion and committed context");
        let conversation = checked_id(second.conversation_id.clone())?;
        let history = fixture
            .attach()?
            .conversation_history(project, None, None, 8)?;
        assert_eq!(
            history.conversation_id.as_deref(),
            Some(conversation.as_slice())
        );
        assert_eq!(history.generation, second.generation);
        assert_eq!(history.entries.len(), 2);
        assert_eq!(history.entries[0].operation_id, second.operation_id);
        let latest_operation = checked_id(history.entries[0].operation_id.clone())?;
        let prompt = fixture
            .attach()?
            .read_conversation_prompt(project, latest_operation)?;
        assert_eq!(
            prompt.prompt.as_deref(),
            Some("What word did I ask you to remember? Reply with only that word.")
        );
        let before = history.entries[0].accepted_frame;
        let page =
            fixture
                .attach()?
                .conversation_history(project, Some(conversation), before, 8)?;
        assert_eq!(page.entries.len(), 1);
        assert_eq!(page.entries[0].operation_id, accepted.operation_id);
        let other_dir = fixture.home.join("other-project");
        fs::create_dir(&other_dir)?;
        fs::set_permissions(&other_dir, fs::Permissions::from_mode(0o700))?;
        let other = fixture.attach()?.register_project(
            pb::ProjectRegister {
                request_id: Some(asura_platform::random_id().to_vec()),
                location: Some(other_dir.to_str().ok_or("non-UTF-8 path")?.into()),
            },
            &|| false,
        )?;
        let other = checked_id(other.project_id)?;
        assert!(
            fixture
                .attach()?
                .conversation_history(other, None, None, 8)?
                .entries
                .is_empty()
        );
        let cross_page = fixture
            .attach()?
            .conversation_history(other, Some(conversation), None, 8);
        assert!(
            matches!(cross_page, Err(asura_client::Error::Remote(_, ref message)) if message == "invalid_request")
        );
        let cross_prompt = fixture
            .attach()?
            .read_conversation_prompt(other, latest_operation);
        assert!(
            matches!(cross_prompt, Err(asura_client::Error::Remote(_, ref message)) if message == "invalid_request")
        );
        let malformed = fixture.attach()?.conversation_history(
            project,
            Some(conversation),
            Some(before.unwrap() + 1),
            8,
        );
        assert!(
            matches!(malformed, Err(asura_client::Error::Remote(_, ref message)) if message == "invalid_request")
        );
        let denied = fixture
            .attach()?
            .read_conversation_prompt([88; 16], latest_operation);
        assert!(
            matches!(denied, Err(asura_client::Error::Remote(_, ref message)) if message == "invalid_request")
        );
        fixture.stop()?;
        fixture.start()?;
        wait_ready(&fixture, installation.clone())?;
        let restored = fixture
            .attach()?
            .conversation_history(project, None, None, 8)?;
        assert_eq!(restored.conversation_id, history.conversation_id);
        assert_eq!(restored.generation, history.generation);
        assert_eq!(restored.entries, history.entries);
        let recovered = observe(&fixture, operation)?;
        assert!(
            recovered.model_context.is_none(),
            "restart must not invent telemetry"
        );
        let mut expected_recovered = event.clone();
        expected_recovered.model_context = None;
        // Turn statistics are live-only measurements; restart leaves them
        // absent and replay must not invent them (response-activity.md).
        assert!(
            recovered.statistics.is_none(),
            "restart must not invent statistics"
        );
        expected_recovered.statistics = None;
        assert_eq!(recovered, expected_recovered);
        let cancelled = submit(
            &fixture,
            project,
            second.conversation_id,
            second.generation.unwrap(),
            "Write a detailed long explanation of trees.",
        )?;
        let cancel_operation = checked_id(cancelled.operation_id)?;
        let reply = fixture.attach()?.cancel_conversation(
            cancel_operation,
            cancelled.generation.unwrap(),
            &|| false,
        )?;
        let terminal = observe(&fixture, cancel_operation)?;
        if reply.terminal == Some(false) {
            assert_eq!(terminal.kind, Some(5));
        }
        println!(
            "PASS native response, committed history, restart replay, cancellation settlement"
        );
        queue_journey(&mut fixture, project, installation.clone())?;
        crash_recovery(&mut fixture, project, installation.clone())?;
    }
    fixture.stop()?;
    assert!(fixture.runtime()?.acquire_owner().is_ok());
    println!("PASS test service stopped and reaped; owner released");
    Ok(())
}
fn managed_queue_journey(
    fixture: &mut Fixture,
    project: [u8; 16],
    installation: Option<Vec<u8>>,
) -> Result<()> {
    let unavailable = fixture
        .attach()?
        .config("model", Some("unknown:queue-fixture"), &|| false)?;
    assert!(unavailable.error.is_none());
    let first_request = pb::ConversationQueueSubmit {
        request_id: Some(asura_platform::random_id().to_vec()),
        project_id: Some(project.to_vec()),
        conversation_id: None,
        expected_generation: Some(0),
        new_conversation: Some(true),
        prompt: Some("First queued turn".into()),
    };
    let first = managed_submit_call(fixture, first_request.clone())?;
    let first_id = checked_id(first.accepted_input_id.clone())?;
    let first_entry = first
        .entries
        .iter()
        .find(|entry| entry.input_id.as_deref() == Some(first_id.as_slice()))
        .ok_or("accepted input absent from projection")?;
    let conversation = first_entry
        .conversation_id
        .clone()
        .ok_or("missing provisional conversation")?;
    assert_eq!(first_entry.new_conversation, Some(true));
    assert_eq!(first_entry.target_operation_id, None);
    let retry = managed_submit_call(fixture, first_request)?;
    assert_eq!(retry.accepted_input_id, Some(first_id.to_vec()));
    let enqueue = |prompt: &str| pb::ConversationQueueSubmit {
        request_id: Some(asura_platform::random_id().to_vec()),
        project_id: Some(project.to_vec()),
        conversation_id: Some(conversation.clone()),
        expected_generation: Some(0),
        new_conversation: Some(false),
        prompt: Some(prompt.into()),
    };
    let second = managed_submit_call(fixture, enqueue("Second queued turn"))?;
    let second_id = checked_id(second.accepted_input_id.clone())?;
    let third = managed_submit_call(fixture, enqueue("Third queued turn"))?;
    let third_id = checked_id(third.accepted_input_id.clone())?;
    let held = queue_settled(fixture, project, first_id)?;
    assert_eq!(held.state, Some(2));
    assert_eq!(held.hold_reason.as_deref(), Some("model_unavailable"));
    let resume = pb::ConversationQueueDecision {
        request_id: Some(asura_platform::random_id().to_vec()),
        input_id: Some(first_id.to_vec()),
        action: Some(1),
        ..Default::default()
    };
    let decide = |request: pb::ConversationQueueDecision| -> Result<pb::ConversationQueueReply> {
        let end = Instant::now() + Duration::from_secs(3);
        loop {
            match fixture
                .attach()?
                .decide_conversation_input(request.clone(), &|| false)
            {
                Ok(reply) => return Ok(reply),
                Err(asura_client::Error::Remote(_, message))
                    if message == "conversation_busy" && Instant::now() < end =>
                {
                    thread::sleep(Duration::from_millis(10));
                }
                Err(error) => return Err(error.into()),
            }
        }
    };
    decide(resume.clone())?;
    let held_again = queue_settled(fixture, project, first_id)?;
    assert_eq!(held_again.state, Some(2));
    assert_eq!(held_again.hold_reason.as_deref(), Some("model_unavailable"));
    let replayed_resume = decide(resume)?;
    let still_held = replayed_resume
        .entries
        .iter()
        .find(|entry| entry.input_id.as_deref() == Some(first_id.as_slice()))
        .ok_or("replayed Resume lost input")?;
    assert_eq!(still_held.state, Some(2));
    assert_eq!(still_held.hold_reason.as_deref(), Some("model_unavailable"));
    let before = fixture.attach()?.conversation_queue(project, None)?;
    let before_revision = before.order_revision.ok_or("missing order revision")?;
    let move_request = pb::ConversationQueueReorder {
        request_id: Some(asura_platform::random_id().to_vec()),
        input_id: Some(third_id.to_vec()),
        after_input_id: Some(first_id.to_vec()),
        expected_order_revision: Some(before_revision),
    };
    let moved = managed_reorder_call(fixture, move_request.clone())?;
    assert_eq!(moved.stale_order, Some(false));
    assert_eq!(
        moved.order_revision,
        Some(before_revision + 1),
        "a committed move advances queue order"
    );
    let repeated = managed_reorder_call(fixture, move_request)?;
    assert_eq!(repeated.order_revision, moved.order_revision);
    let stale = managed_reorder_call(
        fixture,
        pb::ConversationQueueReorder {
            request_id: Some(asura_platform::random_id().to_vec()),
            input_id: Some(second_id.to_vec()),
            after_input_id: Some(third_id.to_vec()),
            expected_order_revision: Some(before_revision),
        },
    )?;
    assert_eq!(stale.stale_order, Some(true));
    assert_eq!(stale.order_revision, moved.order_revision);
    let order: Vec<_> = fixture
        .attach()?
        .conversation_queue(project, None)?
        .entries
        .into_iter()
        .filter(|entry| matches!(entry.state, Some(1 | 2)))
        .map(|entry| checked_id(entry.input_id).unwrap())
        .collect();
    assert_eq!(order, vec![first_id, third_id, second_id]);
    fixture.stop()?;
    fixture.start()?;
    wait_ready(fixture, installation)?;
    let after = fixture.attach()?.conversation_queue(project, None)?;
    assert!(
        after.order_revision >= moved.order_revision,
        "restart may commit a Hold decision and advance order revision"
    );
    let recovered_order: Vec<_> = after
        .entries
        .iter()
        .filter(|entry| matches!(entry.state, Some(1 | 2)))
        .map(|entry| checked_id(entry.input_id.clone()).unwrap())
        .collect();
    assert_eq!(recovered_order, vec![first_id, third_id, second_id]);
    for id in [first_id, second_id, third_id] {
        let entry = fixture.attach()?.conversation_queue(project, Some(id))?;
        assert_eq!(entry.entries.len(), 1);
        assert_eq!(entry.entries[0].input_id, Some(id.to_vec()));
        assert!(entry.entries[0].text.is_some(), "restart retained prompt");
        assert!(
            entry.entries[0].hold_reason.is_none(),
            "restart cannot invent a hold cause"
        );
    }
    let retry_after_restart = managed_submit_call(
        fixture,
        pb::ConversationQueueSubmit {
            request_id: retry.request_id.clone(),
            project_id: Some(project.to_vec()),
            conversation_id: None,
            expected_generation: Some(0),
            new_conversation: Some(true),
            prompt: Some("First queued turn".into()),
        },
    )?;
    assert_eq!(
        retry_after_restart.accepted_input_id,
        Some(first_id.to_vec())
    );
    assert!(
        retry_after_restart
            .entries
            .iter()
            .any(|entry| entry.input_id == Some(first_id.to_vec()))
    );
    let first_state = fixture
        .attach()?
        .conversation_queue(project, Some(first_id))?;
    assert_eq!(first_state.entries[0].state, Some(2));
    fixture.attach()?.decide_conversation_input(
        pb::ConversationQueueDecision {
            request_id: Some(asura_platform::random_id().to_vec()),
            input_id: Some(first_id.to_vec()),
            action: Some(2),
            target_operation_id: None,
            target_generation: None,
        },
        &|| false,
    )?;
    let retry_after_drop = managed_submit_call(
        fixture,
        pb::ConversationQueueSubmit {
            request_id: retry.request_id.clone(),
            project_id: Some(project.to_vec()),
            conversation_id: None,
            expected_generation: Some(0),
            new_conversation: Some(true),
            prompt: Some("First queued turn".into()),
        },
    )?;
    assert_eq!(retry_after_drop.accepted_input_id, Some(first_id.to_vec()));
    assert!(
        retry_after_drop
            .entries
            .iter()
            .any(|entry| entry.input_id == Some(first_id.to_vec()))
    );
    println!("PASS idle queue acceptance, exact retry, reorder CAS and restart retention");
    Ok(())
}
fn managed_submit_call(
    fixture: &Fixture,
    request: pb::ConversationQueueSubmit,
) -> Result<pb::ConversationQueueReply> {
    let end = Instant::now() + Duration::from_secs(3);
    loop {
        match fixture
            .attach()?
            .queue_conversation_input(request.clone(), &|| false)
        {
            Ok(reply) => return Ok(reply),
            Err(asura_client::Error::Remote(_, message))
                if message == "conversation_busy" && Instant::now() < end =>
            {
                thread::sleep(Duration::from_millis(10));
            }
            Err(error) => return Err(error.into()),
        }
    }
}
fn managed_reorder_call(
    fixture: &Fixture,
    request: pb::ConversationQueueReorder,
) -> Result<pb::ConversationQueueReply> {
    let end = Instant::now() + Duration::from_secs(3);
    loop {
        match fixture
            .attach()?
            .reorder_conversation_input(request.clone(), &|| false)
        {
            Ok(reply) => return Ok(reply),
            Err(asura_client::Error::Remote(_, message))
                if message == "conversation_busy" && Instant::now() < end =>
            {
                thread::sleep(Duration::from_millis(10));
            }
            Err(error) => return Err(error.into()),
        }
    }
}
fn queue_entry(
    fixture: &Fixture,
    project: [u8; 16],
    input: [u8; 16],
) -> Result<pb::ConversationQueueEntry> {
    let end = Instant::now() + Duration::from_secs(5);
    loop {
        match fixture.attach()?.conversation_queue(project, Some(input)) {
            Ok(reply) => {
                return reply
                    .entries
                    .into_iter()
                    .next()
                    .ok_or_else(|| "missing queued input".into());
            }
            Err(asura_client::Error::Remote(_, message))
                if message == "conversation_busy" && Instant::now() < end =>
            {
                thread::sleep(Duration::from_millis(10))
            }
            Err(error) => return Err(error.into()),
        }
    }
}
fn queue_settled(
    fixture: &Fixture,
    project: [u8; 16],
    input: [u8; 16],
) -> Result<pb::ConversationQueueEntry> {
    let end = Instant::now() + Duration::from_secs(150);
    loop {
        let entry = queue_entry(fixture, project, input)?;
        if matches!(entry.state, Some(2 | 4 | 5 | 6)) {
            return Ok(entry);
        }
        if Instant::now() >= end {
            return Err("queue settlement deadline".into());
        }
        thread::sleep(Duration::from_millis(25));
    }
}
fn decide_call(
    fixture: &Fixture,
    request: pb::ConversationQueueDecision,
) -> Result<pb::ConversationQueueReply> {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        match fixture
            .attach()?
            .decide_conversation_input(request.clone(), &|| false)
        {
            Ok(reply) => return Ok(reply),
            Err(asura_client::Error::Remote(_, reason))
                if reason == "conversation_busy" && Instant::now() < deadline =>
            {
                thread::sleep(Duration::from_millis(10));
            }
            Err(error) => return Err(error.into()),
        }
    }
}
/// A queued follow-up to the conversation of `accepted`.
fn follow_up(
    project: [u8; 16],
    accepted: &queue_submit::Accepted,
    prompt: &str,
) -> pb::ConversationQueueSubmit {
    queue_submit::queue_request(
        project,
        accepted.conversation_id.clone(),
        accepted.generation.unwrap_or(0),
        prompt,
    )
}
fn queue_journey(
    fixture: &mut Fixture,
    project: [u8; 16],
    installation: Option<Vec<u8>>,
) -> Result<()> {
    let first = submit(
        fixture,
        project,
        None,
        0,
        "Write a detailed 300 word explanation of how trees grow.",
    )?;
    let q1 = follow_up(project, &first, "Reply with only ORCHARD.");
    let q2 = follow_up(project, &first, "Reply with only MAPLE.");
    let ack = managed_submit_call(fixture, q1.clone())?;
    assert_eq!(ack.request_id, q1.request_id);
    let id1 = checked_id(ack.accepted_input_id.clone())?;
    let id2 = checked_id(managed_submit_call(fixture, q2)?.accepted_input_id)?;
    let retry = managed_submit_call(fixture, q1.clone())?;
    assert_eq!(retry.accepted_input_id, Some(id1.to_vec()));
    let mut conflict = q1;
    conflict.prompt = Some("changed".into());
    assert!(
        matches!(managed_submit_call(fixture,conflict).unwrap_err().downcast_ref::<asura_client::Error>(),Some(asura_client::Error::Remote(_,message)) if message=="request_conflict")
    );
    // Each call used an independent attachment: disconnect cannot own scheduling.
    let one = queue_settled(fixture, project, id1)?;
    let two = queue_settled(fixture, project, id2)?;
    assert_eq!(one.state, Some(4));
    assert_eq!(two.state, Some(4));
    assert_eq!(one.generation, Some(2));
    assert_eq!(two.generation, Some(3));
    assert_ne!(one.operation_id, two.operation_id);
    println!("PASS durable queue order, disconnect and original-request retry");

    // Ctrl+S: one kind 18 input, then IQ2 promotion (kind 16) during the
    // active turn (IQ2-R1). The promotion never reports repair_required.
    let active = submit(
        fixture,
        project,
        None,
        0,
        "Write a detailed 300 word explanation of oceans.",
    )?;
    let queued = managed_submit_call(
        fixture,
        follow_up(project, &active, "Instead reply with only CORAL."),
    )?;
    let input = checked_id(queued.accepted_input_id)?;
    let promote = pb::ConversationQueueDecision {
        request_id: Some(asura_platform::random_id().to_vec()),
        input_id: Some(input.to_vec()),
        action: Some(3),
        target_operation_id: active.operation_id.clone(),
        target_generation: active.generation,
    };
    let promoted = decide_call(fixture, promote.clone())?;
    assert_eq!(promoted.entries.len(), 1);
    assert_eq!(
        promoted.entries[0].input_id.as_deref(),
        Some(input.as_slice())
    );
    assert_eq!(promoted.entries[0].kind, Some(2));
    assert_eq!(promoted.entries[0].target_operation_id, active.operation_id);
    let repeated = decide_call(fixture, promote.clone())?;
    assert_eq!(repeated.entries[0].input_id, promoted.entries[0].input_id);
    let mut conflicting = promote;
    conflicting.target_generation = Some(active.generation.unwrap() + 1);
    assert!(
        matches!(decide_call(fixture, conflicting).unwrap_err().downcast_ref::<asura_client::Error>(),
        Some(asura_client::Error::Remote(_, reason)) if reason == "request_conflict")
    );
    assert_eq!(
        observe(fixture, checked_id(active.operation_id)?)?.kind,
        Some(5)
    );
    let replacement = queue_settled(fixture, project, input)?;
    assert_eq!(replacement.state, Some(4));
    assert_eq!(replacement.generation, Some(2));
    assert_eq!(replacement.input_id.as_deref(), Some(input.as_slice()));
    println!("PASS queued input promoted atomically; exact retry and one replacement");

    // A restart holds an undispatched input; explicit Resume runs it once.
    let active = submit(
        fixture,
        project,
        None,
        0,
        "Write a detailed 300 word explanation of leaves.",
    )?;
    let held = managed_submit_call(
        fixture,
        follow_up(project, &active, "Reply with only BIRCH."),
    )?;
    let held_id = checked_id(held.accepted_input_id)?;
    fixture.stop()?;
    fixture.start()?;
    wait_ready(fixture, installation)?;
    assert_eq!(queue_entry(fixture, project, held_id)?.state, Some(2));
    let request = pb::ConversationQueueDecision {
        request_id: Some(asura_platform::random_id().to_vec()),
        input_id: Some(held_id.to_vec()),
        action: Some(1),
        ..Default::default()
    };
    decide_call(fixture, request.clone())?;
    decide_call(fixture, request)?;
    assert_eq!(queue_settled(fixture, project, held_id)?.state, Some(4));
    println!("PASS restart holds queued input; explicit resume runs once");
    Ok(())
}

fn wait_classification(
    fixture: &Fixture,
    expected: pb::InstallationState,
) -> Result<asura_client::InstallationSnapshot> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let status = fixture.attach()?.inspect_installation()?;
        if status.installation == expected
            && status.reason != pb::InstallationInspectionReason::InspectionPending
        {
            return Ok(status);
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "expected {expected:?}; got {:?}/{:?}",
                status.installation, status.reason
            )
            .into());
        }
        thread::sleep(Duration::from_millis(20));
    }
}
fn write_journal_fixture(fixture: &Fixture, bytes: &[u8]) -> Result<PathBuf> {
    fixture.runtime()?;
    let state = fixture.home.join(".asura/state");
    let control = state.join("control");
    fs::create_dir_all(&control)?;
    for dir in [&state, &control] {
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    }
    let file = control.join("seg-0000000000000001.log");
    fs::write(&file, bytes)?;
    fs::set_permissions(&file, fs::Permissions::from_mode(0o600))?;
    Ok(file)
}
fn preservation_journeys() -> Result<()> {
    use asura_storage::authority::conversation as journal;
    use std::sync::atomic::AtomicBool;
    // A request-bearing pending intent must remain the same intent across starts.
    {
        let mut fixture = Fixture::new()?;
        let pending = journal::PendingInit {
            request: [7; 16],
            digest: journal::request_digest(journal::Request::Initialize { mode: 1 })?,
            mode: 1,
            configuration_revision: 1,
            configuration_digest: [9; 32],
            graph: [8; 16],
        };
        let bytes = journal::encode_frame(
            &journal::FrameContext {
                installation_id: [6; 16],
                transition_id: [5; 16],
                sequence: 1,
                prior_digest: [0; 32],
                expected_revision: 0,
                owner_generation: 1,
            },
            &journal::Record::PendingInit(pending.clone()),
        )?;
        let file = write_journal_fixture(&fixture, &bytes)?;
        for expected_owner in 2..=3 {
            fixture.start()?;
            let status = wait_classification(&fixture, pb::InstallationState::Recovering)?;
            assert_eq!(
                status.reason,
                pb::InstallationInspectionReason::InitializationPending
            );
            assert_eq!(status.installation_id, Some([6; 16]));
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                let current = fixture.attach()?.inspect_installation()?;
                if current
                    .recorded_owner_generation
                    .is_some_and(|generation| generation >= expected_owner)
                {
                    break;
                }
                if Instant::now() >= deadline {
                    return Err("pending writer Open did not publish owner generation".into());
                }
                thread::sleep(Duration::from_millis(10));
            }
            fixture.stop()?;
            assert!(
                !fixture.home.join(".asura/db").exists(),
                "pending intent must not create a graph"
            );
            let after = fs::read(&file)?;
            assert!(after.starts_with(&bytes));
            let replay = journal::replay(&after, &AtomicBool::new(false))?;
            assert_eq!(replay.initialization, pending);
            assert!(replay.binding.is_none());
        }
        println!("PASS pending initialization preserves original intent without graph creation");
    }
    {
        let mut fixture = Fixture::new()?;
        let bytes = b"preserve damaged authority exactly";
        let file = write_journal_fixture(&fixture, bytes)?;
        fixture.start()?;
        wait_classification(&fixture, pb::InstallationState::RepairRequired)?;
        fixture.stop()?;
        assert_eq!(fs::read(file)?, bytes);
        assert!(!fixture.home.join(".asura/db").exists());
        println!("PASS damaged authority preserved without automatic replacement");
    }
    {
        let mut fixture = Fixture::new()?;
        fixture.start()?;
        let first = wait_ready(&fixture, None)?;
        fixture.stop()?;
        let file = fixture
            .home
            .join(".asura/state/control/seg-0000000000000001.log");
        let before = fs::read(&file)?;
        let original = journal::replay(&before, &AtomicBool::new(false))?;
        fs::rename(
            fixture.home.join(".asura/db"),
            fixture.home.join("retained-db"),
        )?;
        fixture.start()?;
        let status = wait_classification(&fixture, pb::InstallationState::GraphUnavailable)?;
        assert_eq!(status.installation_id, first.installation_id);
        fixture.stop()?;
        assert!(
            !fixture.home.join(".asura/db").exists(),
            "missing bound graph must not be recreated"
        );
        let after = fs::read(&file)?;
        assert!(after.starts_with(&before));
        let replay = journal::replay(&after, &AtomicBool::new(false))?;
        assert_eq!(replay.initialization, original.initialization);
        assert_eq!(replay.binding, original.binding);
        println!("PASS missing bound graph leaves original installation and binding intact");
    }
    {
        let mut fixture = Fixture::new()?;
        fixture.runtime()?;
        let config = fixture.home.join(".asura/config.yaml");
        let bytes = b"model: [invalid YAML";
        fs::write(&config, bytes)?;
        fs::set_permissions(&config, fs::Permissions::from_mode(0o600))?;
        fixture.start()?;
        let status = wait_classification(&fixture, pb::InstallationState::Unavailable)?;
        assert_ne!(status.reason, pb::InstallationInspectionReason::RuntimeOnly);
        fixture.stop()?;
        assert_eq!(fs::read(config)?, bytes);
        assert!(!fixture.home.join(".asura/state").exists());
        println!("PASS automatic initialization failure is visible and preserves source files");
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ProcessWitness {
    pid: u32,
    parent: u32,
    started: String,
    command: String,
}
fn process_table(home: &Path) -> Result<Vec<ProcessWitness>> {
    let output = home.join("process-witness.txt");
    let mut child = Command::new("/bin/ps")
        .args(["-ww", "-axo", "pid=,ppid=,lstart=,command="])
        .stdin(Stdio::null())
        .stdout(fs::File::create(&output)?)
        .stderr(Stdio::null())
        .spawn()?;
    let deadline = Instant::now() + Duration::from_secs(2);
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(5)),
            other => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!("bounded ps failed: {other:?}").into());
            }
        }
    };
    if !status.success() {
        return Err("ps process witness failed".into());
    }
    if fs::metadata(&output)?.len() > 2 * 1024 * 1024 {
        return Err("process table exceeds test bound".into());
    }
    let text = fs::read_to_string(output)?;
    let mut rows = Vec::new();
    for line in text.lines() {
        let words: Vec<_> = line.split_whitespace().collect();
        if words.len() < 8 {
            continue;
        }
        if let (Ok(pid), Ok(parent)) = (words[0].parse(), words[1].parse()) {
            rows.push(ProcessWitness {
                pid,
                parent,
                started: words[2..7].join(" "),
                command: words[7..].join(" "),
            });
        }
    }
    Ok(rows)
}
fn helper_witness(fixture: &Fixture) -> Result<ProcessWitness> {
    let parent = fixture.child.as_ref().ok_or("missing service child")?.id();
    let prefix = fixture
        .home
        .join(".asura/run/model-")
        .to_string_lossy()
        .into_owned();
    let matches: Vec<_> = process_table(&fixture.home)?
        .into_iter()
        .filter(|p| p.parent == parent && p.command.starts_with(&prefix))
        .collect();
    if matches.len() != 1 {
        return Err(format!("expected one exact direct helper, found {}", matches.len()).into());
    }
    Ok(matches.into_iter().next().unwrap())
}
fn same_helper(home: &Path, witness: &ProcessWitness) -> Result<bool> {
    Ok(process_table(home)?
        .iter()
        .any(|p| p.pid == witness.pid && p.started == witness.started))
}
fn terminate_witness(home: &Path, witness: &ProcessWitness) -> Result<()> {
    for signal in ["-TERM", "-KILL"] {
        if !same_helper(home, witness)? {
            return Ok(());
        }
        let current = process_table(home)?;
        if !current.iter().any(|p| {
            p.pid == witness.pid && p.started == witness.started && p.command == witness.command
        }) {
            return Err("helper command witness changed; refusing signal".into());
        }
        let mut child = Command::new("/bin/kill")
            .args([signal, &witness.pid.to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        let end = Instant::now() + Duration::from_secs(2);
        loop {
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if Instant::now() < end => thread::sleep(Duration::from_millis(5)),
                _ => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err("kill command timeout".into());
                }
            }
        }
        let end = Instant::now() + Duration::from_secs(2);
        while Instant::now() < end {
            if !same_helper(home, witness)? {
                return Ok(());
            }
            thread::sleep(Duration::from_millis(20));
        }
    }
    Err("owned orphan helper cleanup unconfirmed".into())
}
fn crash_recovery(
    fixture: &mut Fixture,
    project: [u8; 16],
    installation: Option<Vec<u8>>,
) -> Result<()> {
    let accepted = submit(
        fixture,
        project,
        None,
        0,
        "Write a long numbered list of 100 different trees, with a description of each tree. Continue until the list is complete.",
    )?;
    let operation = checked_id(accepted.operation_id)?;
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut client = fixture.attach()?;
    loop {
        let event = client.observe_conversation(operation, 0, 0)?;
        if event.kind == Some(2) {
            break;
        }
        if matches!(event.kind, Some(3..=6)) {
            return Err("operation completed before crash boundary; no crash proof".into());
        }
        if Instant::now() >= deadline {
            return Err("no started snapshot before crash deadline".into());
        }
        thread::sleep(Duration::from_millis(10));
    }
    let witness = helper_witness(fixture)?;
    let latest = client.observe_conversation(operation, 0, 0)?;
    if matches!(latest.kind, Some(3..=6)) {
        return Err("operation completed before witnessed crash".into());
    }
    let mut service = fixture.child.take().ok_or("missing service child")?;
    // This handle owns the exact service created by Fixture::start.
    if let Err(error) = service.kill() {
        fixture.child = Some(service);
        return Err(error.into());
    }
    let status = service.wait()?;
    assert!(!status.success());
    let deadline = Instant::now() + Duration::from_secs(5);
    let settled = loop {
        match same_helper(&fixture.home, &witness) {
            Ok(false) => break true,
            Ok(true) if Instant::now() < deadline => thread::sleep(Duration::from_millis(20)),
            _ => break false,
        }
    };
    if !settled {
        terminate_witness(&fixture.home, &witness)?;
        return Err(
            "helper did not exit after service EOF within five seconds; fallback cleanup used"
                .into(),
        );
    }
    println!(
        "PASS exact service killed/reaped; witnessed helper exited after EOF (orphan reaping belongs to OS)"
    );
    fixture.start()?;
    wait_ready(fixture, installation)?;
    let recovered = observe(fixture, operation)?;
    assert_eq!(recovered.kind, Some(6));
    assert_eq!(recovered.reason, Some(11));
    assert_eq!(recovered.cursor, Some(u64::MAX));
    assert_eq!(recovered.usage_known, Some(false));
    let parent = fixture.child.as_ref().unwrap().id();
    let prefix = fixture
        .home
        .join(".asura/run/model-")
        .to_string_lossy()
        .into_owned();
    assert!(
        !process_table(&fixture.home)?
            .iter()
            .any(|p| p.parent == parent && p.command.starts_with(&prefix)),
        "recovery must not restart a helper"
    );
    println!("PASS interrupted recovery retains operation identity without native redispatch");
    Ok(())
}

fn select_native_tool_provider(fixture: &Fixture, provider: &str) -> Result<()> {
    if !matches!(provider, "system" | "ollama" | "mlx" | "coreai") {
        return Err("unknown native fixture provider".into());
    }
    if provider != "system" {
        let selection = if provider == "ollama" {
            format!(
                "ollama:{}",
                std::env::var("ASURA_TEST_OLLAMA_MODEL").unwrap_or_else(|_| "granite4.1:8b".into())
            )
        } else {
            let variable = if provider == "coreai" {
                "ASURA_TEST_COREAI_ASSET"
            } else {
                "ASURA_TEST_MLX_ASSET"
            };
            let source = PathBuf::from(
                std::env::var_os(variable).ok_or("native tool asset environment required")?,
            )
            .canonicalize()?;
            copy_assets(
                &source,
                &fixture
                    .home
                    .join(format!(".asura/data/models/{provider}/native-fixture")),
                0,
                &mut 0,
                &mut 0,
            )?;
            if provider == "mlx" {
                let capabilities = std::env::var("ASURA_TEST_MLX_CAPABILITIES")
                    .unwrap_or_else(|_| "[toolCalling]".into());
                let declaration = format!("native-fixture: {{capabilities: {capabilities}}}");
                let configured = fixture.attach()?.config(
                    "providers.mlx.models",
                    Some(&declaration),
                    &|| false,
                )?;
                assert!(
                    configured.error.is_none(),
                    "MLX declaration failed: {configured:?}"
                );
            }
            format!("{provider}:native-fixture")
        };
        let configured = fixture
            .attach()?
            .config("model", Some(&selection), &|| false)?;
        assert!(
            configured.error.is_none(),
            "provider selection failed: {configured:?}"
        );
    }
    Ok(())
}

/// Live-model qualification: the secret exists only in a scoped file, never the prompt.
fn native_tools(provider: &str) -> Result<()> {
    use asura_storage::authority::conversation as journal;
    use std::sync::atomic::AtomicBool;
    let mut fixture = Fixture::new().map_err(|e| format!("native-tools fixture: {e}"))?;
    fixture
        .start()
        .map_err(|e| format!("native-tools service start: {e}"))?;
    wait_ready(&fixture, None).map_err(|e| format!("native-tools wait ready: {e}"))?;
    select_native_tool_provider(&fixture, provider)?;

    let directory = fixture.home.join("tool-project");
    fs::create_dir(&directory)?;
    fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))?;
    let secret = format!(
        "READ_{:032x}",
        u128::from_ne_bytes(asura_platform::random_id())
    );
    fs::write(directory.join("proof.txt"), format!("{secret}\n"))?;
    let outside = format!(
        "OUTSIDE_{:032x}",
        u128::from_ne_bytes(asura_platform::random_id())
    );
    fs::write(fixture.home.join("outside.txt"), &outside)?;
    let project = checked_id(
        fixture
            .attach()
            .map_err(|e| format!("native-tools attach for registration: {e}"))?
            .register_project(
                pb::ProjectRegister {
                    request_id: Some(asura_platform::random_id().to_vec()),
                    location: Some(directory.to_str().ok_or("invalid scratch path")?.into()),
                },
                &|| false,
            )
            .map_err(|e| format!("native-tools register project: {e}"))?
            .project_id,
    )?;
    let accepted = submit(
        &fixture,
        project,
        None,
        0,
        "Use project with command read_file to read proof.txt with offset 0 and limit 1024. Reply with the exact token in the file, and nothing else. Do not guess the file contents.",
    ).map_err(|e| format!("native-tools file submit: {e}"))?;
    let operation = checked_id(accepted.operation_id.clone())?;
    // The submission connection is gone; accepted service work must continue.
    let event =
        observe(&fixture, operation).map_err(|e| format!("native-tools file observe: {e}"))?;
    if event.kind != Some(3) {
        fixture.print_model_diagnostics();
        eprintln!(
            "native tool failure counters: tools={} output_bytes={}",
            event.tools.len(),
            event.text.as_ref().map_or(0, String::len)
        );
    }
    assert_eq!(
        event.kind,
        Some(3),
        "native tool turn failed: {:?}",
        event.reason
    );
    assert!(
        event.text.as_deref().unwrap_or_default().contains(&secret),
        "answer did not contain file-only proof: {:?}",
        event.text
    );
    assert!(
        event
            .tools
            .iter()
            .any(|tool| tool.name.as_deref() == Some("project")
                && tool.state == Some(2)
                && tool.status == Some(1)),
        "no committed successful tool event"
    );
    assert_eq!(
        event.usage_known,
        Some(false),
        "per-step usage cannot become aggregate tool usage"
    );
    if provider == "ollama" {
        let stats = event
            .statistics
            .as_ref()
            .ok_or("native Ollama request measurements absent")?;
        assert_eq!(stats.complete, Some(true));
        assert!(stats.request_count.is_some_and(|n| (2..=64).contains(&n)));
        assert!(stats.input_tokens.is_some_and(|n| n > 0));
        // No request output cap (owner, 2026-10-19): any positive measured sum.
        assert!(stats.output_tokens.is_some_and(|n| n > 0));
        assert_eq!(stats.cached_input_tokens, None);
        assert!(stats.elapsed_ms.is_some());
        println!(
            "PASS native Ollama measured tool continuation: requests={} input={} output={}",
            stats.request_count.unwrap(),
            stats.input_tokens.unwrap(),
            stats.output_tokens.unwrap()
        );
    }

    fixture
        .stop()
        .map_err(|e| format!("native-tools stop: {e}"))?;
    let bytes = asura_storage::authority::fixture::journal_bytes(&fixture.home)?;
    let replay = journal::replay(&bytes, &AtomicBool::new(false))
        .map_err(|error| format!("tool journal replay: {error:?}"))?;
    let record = &replay.operations[&operation];
    assert!(!record.tools.is_empty());
    assert!(record.tools.iter().any(|tool| tool.tool_operation
        == Some(journal::ToolOperation::ReadFile)
        && tool.result_status == Some(1)));
    let terminal = record.terminal.as_ref().expect("committed terminal");
    assert!(!terminal.usage_known);
    assert_eq!(terminal.output_tokens, 0);
    fixture
        .start()
        .map_err(|e| format!("native-tools service start: {e}"))?;
    wait_ready(&fixture, None).map_err(|e| format!("native-tools wait ready: {e}"))?;
    let recovered =
        observe(&fixture, operation).map_err(|e| format!("native-tools replay observe: {e}"))?;
    assert_eq!(recovered.text, event.text);
    assert!(
        recovered.statistics.is_none(),
        "restart cannot reconstruct transient request counters"
    );
    assert!(
        recovered.tools.iter().all(|tool| tool.elapsed_ms.is_none()),
        "restart cannot invent tool duration"
    );
    let mut durable_tools = event.tools.clone();
    for tool in &mut durable_tools {
        tool.elapsed_ms = None;
    }
    assert_eq!(recovered.tools, durable_tools);
    let status = submit(
        &fixture,
        project,
        None,
        0,
        "Call service with command status now and summarize the current service status. Do not guess the status.",
    ).map_err(|e| format!("native-tools status submit: {e}"))?;
    let status = observe(&fixture, checked_id(status.operation_id)?)
        .map_err(|e| format!("native-tools status observe: {e}"))?;
    assert_eq!(
        status.kind,
        Some(3),
        "native status turn failed: {:?}",
        status.reason
    );
    assert!(
        status
            .tools
            .iter()
            .any(|tool| tool.name.as_deref() == Some("service")
                && tool.state == Some(2)
                && tool.status == Some(1)),
        "missing persisted successful status tool result"
    );
    let denied = submit(
        &fixture,
        project,
        None,
        0,
        "Use project with command read_file to read ../outside.txt at offset 0 and limit 1024. Return the exact contents if available.",
    ).map_err(|e| format!("native-tools outside submit: {e}"))?;
    let denied = observe(&fixture, checked_id(denied.operation_id)?)
        .map_err(|e| format!("native-tools outside observe: {e}"))?;
    assert!(
        !denied
            .text
            .as_deref()
            .unwrap_or_default()
            .contains(&outside),
        "outside-project data escaped"
    );
    // Native choice can refuse the request or inspect a different safe file.
    // Deterministic executor tests prove traversal denial; this model journey
    // proves the outside-only secret never reaches the conversation output.
    fixture
        .stop()
        .map_err(|e| format!("native-tools stop: {e}"))?;
    println!(
        "PASS native project tool, file-only answer proof, disconnect continuation, conservative accounting, durable replay and outside-secret non-disclosure"
    );
    Ok(())
}

fn remote_endpoint_restriction(fixture: &Fixture, project: [u8; 16]) -> Result<()> {
    let rejected = fixture.attach()?.config(
        "providers.ollama.endpoint",
        Some("https://models.example"),
        &|| false,
    )?;
    assert_eq!(
        rejected.error.as_deref(),
        Some("config_remote_endpoint_restricted")
    );
    let path = fixture.home.join(".asura/config.yaml");
    let original = fs::read(&path).ok();
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&path)?
            .write_all(
                b"model: ollama:fixture\nproviders:\n  ollama:\n    endpoint: https://models.example\n",
            )?;
    }
    let failure = provider_submit(fixture, project, None, 0, "Hello")
        .expect_err("remote endpoint must not reach a model");
    assert!(
        matches!(failure.downcast_ref::<asura_client::Error>(), Some(asura_client::Error::Remote(_, message)) if message == "remote_endpoint_restricted"),
        "unexpected remote endpoint failure: {failure}"
    );
    match original {
        Some(bytes) => fs::write(&path, bytes)?,
        None => fs::remove_file(&path)?,
    }
    println!("PASS remote model endpoints are restricted to loopback");
    Ok(())
}

fn provider_submit(
    fixture: &Fixture,
    project: [u8; 16],
    conversation: Option<Vec<u8>>,
    generation: u64,
    prompt: &str,
) -> Result<queue_submit::Accepted> {
    submit(fixture, project, conversation, generation, prompt)
}

/// Relative path and bytes of every file in a small resource bundle, sorted.
fn bundle_files(root: &Path) -> Result<Vec<(PathBuf, Vec<u8>)>> {
    let mut files = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(&directory)? {
            let path = entry?.path();
            if path.is_dir() {
                pending.push(path);
            } else {
                files.push((path.strip_prefix(root)?.to_path_buf(), fs::read(&path)?));
            }
            if files.len() + pending.len() > 64 {
                return Err("bundle fixture entry limit".into());
            }
        }
    }
    files.sort();
    Ok(files)
}

fn copy_assets(
    source: &Path,
    target: &Path,
    depth: usize,
    entries: &mut usize,
    bytes: &mut u64,
) -> Result<()> {
    if depth > 16 {
        return Err("model fixture depth limit".into());
    }
    use std::os::unix::fs::DirBuilderExt;
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(target)?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        *entries += 1;
        if *entries > 8192 {
            return Err("model fixture entry limit".into());
        }
        let metadata = fs::metadata(entry.path())?;
        if metadata.is_dir() {
            if entry.file_type()?.is_symlink() {
                return Err("model fixture directory symlink".into());
            }
            copy_assets(
                &entry.path(),
                &target.join(entry.file_name()),
                depth + 1,
                entries,
                bytes,
            )?;
        } else if metadata.is_file() {
            *bytes += metadata.len();
            if *bytes > 128 * 1024 * 1024 * 1024 {
                return Err("model fixture byte limit".into());
            }
            fs::copy(entry.path(), target.join(entry.file_name()))?;
        } else {
            return Err("model fixture nonregular asset".into());
        }
    }
    Ok(())
}

fn native_mlx() -> Result<()> {
    let mut stage = "asset source";
    let result = (|| -> Result<()> {
        let source = PathBuf::from(
            std::env::var_os("ASURA_TEST_MLX_ASSET").ok_or("ASURA_TEST_MLX_ASSET is required")?,
        )
        .canonicalize()?;
        let mut fixture = Fixture::new()?;
        let assets = fixture.home.join(".asura/data/models/mlx/native-fixture");
        // Establish initialization authority before adding non-authority model data.
        stage = "service startup";
        fixture.start()?;
        stage = "installation ready";
        wait_ready(&fixture, None)?;
        stage = "asset copy";
        copy_assets(&source, &assets, 0, &mut 0, &mut 0)?;
        stage = "installation ready";
        wait_ready(&fixture, None)?;
        let directory = fixture.home.join("mlx-project");
        fs::create_dir(&directory)?;
        stage = "project registration";
        let project = checked_id(
            fixture
                .attach()?
                .register_project(
                    pb::ProjectRegister {
                        request_id: Some(asura_platform::random_id().to_vec()),
                        location: Some(directory.to_str().ok_or("invalid fixture path")?.into()),
                    },
                    &|| false,
                )?
                .project_id,
        )?;
        stage = "missing model configuration";
        let absent = fixture
            .attach()?
            .config("model", Some("mlx:missing-fixture"), &|| false)?;
        assert!(absent.error.is_none(), "configuration failed: {absent:?}");
        stage = "missing model rejection";
        let failure = provider_submit(&fixture, project, None, 0, "Hello")
            .expect_err("missing model must not fall back");
        assert!(
            matches!(failure.downcast_ref::<asura_client::Error>(), Some(asura_client::Error::Remote(_, message)) if message == "model_unavailable"),
            "unexpected missing-model failure: {failure}"
        );
        stage = "valid model configuration";
        let configured = fixture
            .attach()?
            .config("model", Some("mlx:native-fixture"), &|| false)?;
        assert!(
            configured.error.is_none(),
            "configuration failed: {configured:?}"
        );
        stage = "native submit";
        let accepted = provider_submit(
            &fixture,
            project,
            None,
            0,
            "Write three short sentences about a tree.",
        )?;
        let operation = checked_id(accepted.operation_id)?;
        stage = "private Metal resource proof";
        let copied = fs::read_dir(fixture.home.join(".asura/run"))?
            .flatten()
            .map(|entry| entry.path().join("mlx.metallib"))
            .find(|path| path.is_file())
            .ok_or("native helper did not retain its private Metal resource")?;
        let packaged = std::env::current_exe()?
            .parent()
            .ok_or("test binary parent")?
            .join("mlx.metallib");
        assert_eq!(
            fs::read(copied)?,
            fs::read(packaged)?,
            "native helper resource must match assembled package"
        );
        stage = "private Hub bundle proof";
        let copied = fs::read_dir(fixture.home.join(".asura/run"))?
            .flatten()
            .map(|entry| entry.path().join(asura_platform::HUB_BUNDLE_NAME))
            .find(|path| path.is_dir())
            .ok_or("native helper did not retain its private Hub bundle")?;
        let packaged = std::env::current_exe()?
            .parent()
            .ok_or("test binary parent")?
            .join(asura_platform::HUB_BUNDLE_NAME);
        assert_eq!(
            bundle_files(&copied)?,
            bundle_files(&packaged)?,
            "native helper Hub bundle must match assembled package"
        );
        stage = "native completion";
        let terminal = observe(&fixture, operation)?;
        assert_eq!(
            terminal.kind,
            Some(3),
            "MLX completion failed: {terminal:?}"
        );
        assert!(!terminal.text.as_deref().unwrap_or_default().is_empty());
        if let Some(context) = &terminal.model_context {
            assert_eq!(context.model_name.as_deref(), Some("mlx:native-fixture"));
            assert!(
                context.input_tokens.is_none(),
                "MLX must not invent measured input tokens"
            );
        }
        stage = "cancellation submit";
        let cancelled = provider_submit(
            &fixture,
            project,
            accepted.conversation_id,
            accepted.generation.unwrap(),
            "Write a very long detailed description of every part of a forest.",
        )?;
        let cancel_operation = checked_id(cancelled.operation_id)?;
        stage = "cancel request";
        let reply = fixture.attach()?.cancel_conversation(
            cancel_operation,
            cancelled.generation.unwrap(),
            &|| false,
        )?;
        stage = "cancel settlement";
        let stopped = observe(&fixture, cancel_operation)?;
        if reply.terminal == Some(false) {
            assert_eq!(stopped.kind, Some(5));
        }
        stage = "service shutdown";
        fixture.stop()?;
        assert!(
            !fs::read_dir(fixture.home.join(".asura/run"))?
                .flatten()
                .any(|entry| entry.file_name().to_string_lossy().starts_with("model-")),
            "owned model copies leaked after shutdown"
        );
        // SG-V10: a relocated package whose Hub bundle does not match the
        // embedded `hub_bundle_sha256`, or lacks it, never spawns the helper.
        stage = "relocated package";
        let built = std::env::current_exe()?
            .parent()
            .ok_or("test binary parent")?
            .to_path_buf();
        let package = fixture.home.join("package");
        fs::create_dir(&package)?;
        let relocated = package.join("conversation_flow");
        fs::copy(std::env::current_exe()?, &relocated)?;
        for name in ["asura-model", "mlx.metallib"] {
            fs::copy(built.join(name), package.join(name))?;
        }
        let bundle = package.join(asura_platform::HUB_BUNDLE_NAME);
        copy_assets(
            &built.join(asura_platform::HUB_BUNDLE_NAME),
            &bundle,
            0,
            &mut 0,
            &mut 0,
        )?;
        let plist = bundle.join("Contents/Info.plist");
        let mut tampered = fs::read(&plist)?;
        tampered.push(b'\n');
        fs::write(&plist, tampered)?;
        fixture.executable = relocated;
        for (case, class) in [
            ("tampered", "preparation_unsafe"),
            ("absent", "preparation_absent"),
        ] {
            if case == "absent" {
                fs::remove_dir_all(&bundle)?;
            }
            stage = "relocated service startup";
            fixture.start()?;
            wait_ready(&fixture, None)?;
            stage = "relocated submit";
            let outcome = match provider_submit(&fixture, project, None, 0, "Hello") {
                Err(error) => error.to_string(),
                Ok(accepted) => format!(
                    "{:?}",
                    observe(&fixture, checked_id(accepted.operation_id)?)?
                ),
            };
            assert!(
                outcome.contains("model_helper_unavailable"),
                "{case} Hub bundle must fail before spawn: {outcome}"
            );
            stage = "relocated shutdown";
            fixture.stop()?;
            assert!(
                !fs::read_dir(fixture.home.join(".asura/run"))?
                    .flatten()
                    .any(|entry| entry.file_name().to_string_lossy().starts_with("model-")),
                "{case} Hub bundle left a model copy"
            );
            let log = fs::read_to_string(fixture.home.join("service.log"))?;
            assert!(log.contains(class), "{case} Hub bundle must log {class}");
        }
        println!(
            "PASS native MLX missing-asset rejection, copied Metal resource and Hub bundle, completion, cancellation, cleanup, and relocated tampered or absent Hub bundle refused before spawn"
        );
        Ok(())
    })();
    result.map_err(|error| format!("native-mlx {stage}: {error}").into())
}

fn native_ollama() -> Result<()> {
    let mut stage = "model selection";
    let result = (|| -> Result<()> {
        let name =
            std::env::var("ASURA_TEST_OLLAMA_MODEL").unwrap_or_else(|_| "granite4.1:8b".into());
        assert!(!name.is_empty() && !name.chars().any(char::is_whitespace));
        let mut fixture = Fixture::new()?;
        stage = "service startup";
        fixture.start()?;
        stage = "installation ready";
        wait_ready(&fixture, None)?;
        let directory = fixture.home.join("ollama-project");
        fs::create_dir(&directory)?;
        stage = "project registration";
        let project = checked_id(
            fixture
                .attach()?
                .register_project(
                    pb::ProjectRegister {
                        request_id: Some(asura_platform::random_id().to_vec()),
                        location: Some(directory.to_str().ok_or("invalid fixture path")?.into()),
                    },
                    &|| false,
                )?
                .project_id,
        )?;
        stage = "provider configuration";
        for (key, value) in [
            (
                "providers.ollama.endpoint",
                "http://127.0.0.1:11434".to_owned(),
            ),
            ("model", format!("ollama:{name}")),
        ] {
            let reply = fixture.attach()?.config(key, Some(&value), &|| false)?;
            assert!(reply.error.is_none(), "configuration failed: {reply:?}");
        }
        stage = "native submit";
        let accepted = provider_submit(
            &fixture,
            project,
            None,
            0,
            "Say hello in one short sentence.",
        )
        .inspect_err(|_error| {
            fixture.print_model_diagnostics();
            let _ = fs::copy(
                fixture.home.join("service.log"),
                "/private/tmp/asura-native-ollama-startup.log",
            );
        })?;
        stage = "native completion";
        let terminal = observe(&fixture, checked_id(accepted.operation_id)?)?;
        assert_eq!(
            terminal.kind,
            Some(3),
            "Ollama completion failed: {terminal:?}"
        );
        assert!(!terminal.text.as_deref().unwrap_or_default().is_empty());
        assert!(
            terminal.tools.is_empty(),
            "endpoint-dependent model must not receive project tools"
        );
        {
            let context = terminal
                .model_context
                .as_ref()
                .ok_or("Ollama context measurement missing")?;
            assert_eq!(
                context.model_name.as_deref(),
                Some(format!("ollama:{name}").as_str())
            );
            assert!(context.input_tokens.is_some_and(|count| count > 0));
            assert!(context.input_tokens <= context.capacity_tokens);
            assert_eq!(context.basis, Some(1));
        }
        // FW-E1 (owner, 2026-10-20): the request window is the model's full
        // reported maximum, so a prompt above the removed 8,192-token clamp
        // completes. An over-window refusal is not reachable here: a prompt is
        // at most 32 KiB and the composer keeps input inside the window
        // (model-provider-integration.md, full-window validation).
        stage = "full window";
        let window = terminal
            .model_context
            .as_ref()
            .and_then(|context| context.capacity_tokens)
            .ok_or("Ollama capacity missing")?;
        let reported = terminal
            .model_context
            .as_ref()
            .and_then(|context| context.reported_max_tokens);
        assert_eq!(Some(window), reported, "window is not the reported maximum");
        assert!(
            window > 8192,
            "select a model whose maximum exceeds 8,192 tokens; {name} reports {window}"
        );
        let large = format!(
            "Ignore the filler text below. Reply with exactly one word: done.\n{}\nReply with exactly one word: done.",
            "x ".repeat(14_900)
        );
        stage = "large prompt submit";
        let accepted = provider_submit(&fixture, project, None, 0, &large)?;
        stage = "large prompt completion";
        let completed = observe(&fixture, checked_id(accepted.operation_id)?)?;
        if completed.kind != Some(3) {
            fixture.print_model_diagnostics();
            let _ = fs::copy(
                fixture.home.join("service.log"),
                "/private/tmp/asura-native-ollama-full-window.log",
            );
        }
        assert_eq!(
            completed.kind,
            Some(3),
            "prompt above 8,192 tokens failed: {completed:?}"
        );
        assert!(completed.tools.is_empty());
        let measured = completed
            .model_context
            .as_ref()
            .and_then(|context| context.input_tokens)
            .ok_or("large prompt measurement missing")?;
        assert!(
            measured > 8192 && measured <= window,
            "measured input {measured} of window {window}"
        );
        stage = "service shutdown";
        fixture.stop()?;
        println!(
            "PASS native Ollama ({name}) synthetic completion, no project disclosure, full window {window} tokens and a {measured}-token prompt above 8,192 completed"
        );
        Ok(())
    })();
    result.map_err(|error| format!("native-ollama {stage}: {error}").into())
}

/// TL-04, TL-09, TL-10 and TL-13 with a live local thinking model
/// (thinking-level.md). `ASURA_TEST_OLLAMA_THINKING_MODEL` selects it
/// (default `qwen3:1.7b`). The journey sets `thinking: high`, checks the
/// level and the form recorded on the turn, that thinking text was streamed,
/// journaled and shown apart from the answer, and that the model's support
/// reached the event; then a second turn with `thinking: off` records the
/// off form and journals no thinking text. The daemon is never started and
/// no model is pulled.
fn native_ollama_thinking() -> Result<()> {
    use asura_storage::authority::conversation as journal;
    let mut stage = "model selection";
    let result = (|| -> Result<()> {
        let name = std::env::var("ASURA_TEST_OLLAMA_THINKING_MODEL")
            .unwrap_or_else(|_| "qwen3:1.7b".into());
        assert!(!name.is_empty() && !name.chars().any(char::is_whitespace));
        let mut fixture = Fixture::new()?;
        stage = "service startup";
        fixture.start()?;
        stage = "installation ready";
        wait_ready(&fixture, None)?;
        let directory = fixture.home.join("thinking-project");
        fs::create_dir(&directory)?;
        stage = "project registration";
        let project = checked_id(
            fixture
                .attach()?
                .register_project(
                    pb::ProjectRegister {
                        request_id: Some(asura_platform::random_id().to_vec()),
                        location: Some(directory.to_str().ok_or("invalid fixture path")?.into()),
                    },
                    &|| false,
                )?
                .project_id,
        )?;
        stage = "provider configuration";
        for (key, value) in [
            (
                "providers.ollama.endpoint",
                "http://127.0.0.1:11434".to_owned(),
            ),
            ("model", format!("ollama:{name}")),
            ("thinking", "high".to_owned()),
        ] {
            let reply = fixture.attach()?.config(key, Some(&value), &|| false)?;
            assert!(reply.error.is_none(), "configuration failed: {reply:?}");
        }
        stage = "thinking submit";
        let accepted = provider_submit(
            &fixture,
            project,
            None,
            0,
            "What is 17 + 25? Think it through, then answer with the number.",
        )?;
        let operation = checked_id(accepted.operation_id)?;
        stage = "thinking completion";
        let terminal = observe(&fixture, operation)?;
        assert_eq!(terminal.kind, Some(3), "thinking turn failed: {terminal:?}");
        let thinking = terminal.thinking_text.clone().unwrap_or_default();
        let answer = terminal.text.clone().unwrap_or_default();
        assert!(
            !thinking.trim().is_empty(),
            "no thinking text: {terminal:?}"
        );
        assert!(!answer.trim().is_empty(), "no answer");
        // Ollama splits thinking from content; the answer holds no thinking.
        assert!(!answer.contains(thinking.trim()));
        // On/off controls: high is sent as `true`, recorded as request on.
        assert_eq!(terminal.thinking_level, Some(4));
        assert_eq!(terminal.thinking_request, Some(2));
        stage = "journal";
        let bytes = asura_storage::authority::fixture::journal_bytes(&fixture.home)?;
        let replay = journal::replay(&bytes, &std::sync::atomic::AtomicBool::new(false))
            .map_err(|e| format!("replay: {e:?}"))?;
        let op = &replay.operations[&operation];
        assert_eq!(op.thinking.len, thinking.len() as u64);
        let conversation = op.conversation;
        stage = "thinking off submit";
        let reply = fixture
            .attach()?
            .config("thinking", Some("off"), &|| false)?;
        assert!(reply.error.is_none());
        let accepted = provider_submit(
            &fixture,
            project,
            Some(conversation.to_vec()),
            1,
            "And 3 + 4? Answer with the number only.",
        )?;
        let second = checked_id(accepted.operation_id)?;
        let terminal = observe(&fixture, second)?;
        assert_eq!(terminal.kind, Some(3), "off turn failed: {terminal:?}");
        assert_eq!(
            (terminal.thinking_level, terminal.thinking_request),
            (Some(1), Some(1))
        );
        assert!(
            terminal
                .thinking_text
                .as_deref()
                .unwrap_or_default()
                .is_empty()
        );
        stage = "service restart";
        fixture.stop()?;
        fixture.start()?;
        wait_ready(&fixture, None)?;
        let restored = observe(&fixture, operation)?;
        assert_eq!(restored.thinking_text.as_deref(), Some(thinking.as_str()));
        fixture.stop()?;
        println!(
            "PASS native Ollama thinking ({name}): {} thinking bytes journaled and shown apart from a {}-byte answer; high sent as on; off recorded and no thinking",
            thinking.len(),
            answer.len()
        );
        Ok(())
    })();
    result.map_err(|error| format!("native-ollama-thinking {stage}: {error}").into())
}

// Read-only helper inventory integration: synthetic metadata/catalog, no inference.
fn inventory_journey() -> Result<()> {
    use std::{
        io::{Read, Write},
        net::TcpListener,
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
            mpsc,
        },
    };
    struct Catalog {
        cancel: Arc<AtomicBool>,
        task: Option<thread::JoinHandle<()>>,
        accepted: mpsc::Receiver<()>,
        release: mpsc::SyncSender<()>,
        endpoint: String,
    }
    impl Catalog {
        fn start() -> Result<Self> {
            let listener = TcpListener::bind("127.0.0.1:0")?;
            let endpoint = format!("http://{}", listener.local_addr()?);
            listener.set_nonblocking(true)?;
            let cancel = Arc::new(AtomicBool::new(false));
            let token = cancel.clone();
            let (accepted_tx, accepted) = mpsc::sync_channel(1);
            let (release, release_rx) = mpsc::sync_channel(1);
            let task = thread::spawn(move || {
                let end = Instant::now() + Duration::from_secs(12);
                while !token.load(Ordering::Acquire) && Instant::now() < end {
                    let Ok((mut stream, _)) = listener.accept() else {
                        thread::sleep(Duration::from_millis(5));
                        continue;
                    };
                    stream
                        .set_read_timeout(Some(Duration::from_millis(100)))
                        .unwrap();
                    stream
                        .set_write_timeout(Some(Duration::from_millis(100)))
                        .unwrap();
                    let mut bytes = Vec::new();
                    while bytes.len() < 4096
                        && !token.load(Ordering::Acquire)
                        && Instant::now() < end
                    {
                        let mut chunk = [0; 256];
                        match stream.read(&mut chunk) {
                            Ok(0) => return,
                            Ok(n) => {
                                bytes.extend_from_slice(&chunk[..n]);
                                if bytes.windows(4).any(|v| v == b"\r\n\r\n") {
                                    break;
                                }
                            }
                            Err(error)
                                if matches!(
                                    error.kind(),
                                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                                ) =>
                            {
                                continue;
                            }
                            Err(_) => return,
                        }
                    }
                    assert!(bytes.starts_with(b"GET /api/tags HTTP/1.1"));
                    let _ = accepted_tx.send(());
                    while !token.load(Ordering::Acquire) && Instant::now() < end {
                        if release_rx.recv_timeout(Duration::from_millis(20)).is_ok() {
                            break;
                        }
                    }
                    if token.load(Ordering::Acquire) {
                        return;
                    }
                    let body = r#"{"models":[{"name":"fixture:tiny"}]}"#;
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    let _ = stream.write_all(response.as_bytes());
                    return;
                }
            });
            Ok(Self {
                cancel,
                task: Some(task),
                accepted,
                release,
                endpoint,
            })
        }
    }
    impl Drop for Catalog {
        fn drop(&mut self) {
            self.cancel.store(true, Ordering::Release);
            let _ = self.release.try_send(());
            if let Some(task) = self.task.take() {
                let _ = task.join();
            }
        }
    }
    let mut fixture = Fixture::new()?;
    fixture.start()?;
    wait_ready(&fixture, None)?;
    let model = fixture.home.join(".asura/data/models/mlx/fixture");
    fs::create_dir_all(&model)?;
    fs::write(
        model.join("config.json"),
        r#"{"model_type":"fixture","max_position_embeddings":4096}"#,
    )?;
    // No weights or vendor tokenizer exist: metadata listing must not load them.
    let server = Catalog::start()?;
    assert!(
        fixture
            .attach()?
            .config("providers.ollama.endpoint", Some(&server.endpoint), &|| {
                false
            })?
            .error
            .is_none()
    );
    assert!(
        fixture
            .attach()?
            .config("model", Some("mlx:missing"), &|| false)?
            .error
            .is_none()
    );
    let mut query_client = fixture.attach()?;
    let query = thread::spawn(move || query_client.models(&|| false));
    server.accepted.recv_timeout(Duration::from_secs(30))?;
    let started = Instant::now();
    assert_eq!(
        fixture.attach()?.inspect()?.lifecycle,
        pb::Lifecycle::Serving as i32
    );
    assert!(started.elapsed() < Duration::from_secs(1));
    assert_eq!(
        fixture.attach()?.models(&|| false)?.error.as_deref(),
        Some("models_busy")
    );
    server.release.send(())?;
    let reply = query.join().map_err(|_| "inventory client panicked")??;
    assert_eq!(reply.error, None);
    assert_eq!(reply.configured_model.as_deref(), Some("mlx:missing"));
    assert!(
        reply
            .models
            .iter()
            .any(|row| row.selector.as_deref() == Some("system"))
    );
    assert!(
        reply
            .models
            .iter()
            .any(|row| row.selector.as_deref() == Some("mlx:fixture") && row.status == Some(2))
    );
    assert!(
        reply
            .models
            .iter()
            .any(|row| row.selector.as_deref() == Some("ollama:fixture:tiny")
                && row.status == Some(3))
    );
    assert!(
        reply
            .models
            .iter()
            .any(|row| row.selector.as_deref() == Some("mlx:missing") && row.status == Some(5))
    );
    assert!(
        reply.issues.is_empty(),
        "unexpected inventory issues: {:?}",
        reply.issues
    );
    drop(server);
    // The catalog listener has closed; the next inventory must preserve local rows.
    let deadline = Instant::now() + Duration::from_secs(5);
    let partial = loop {
        let reply = fixture.attach()?.models(&|| false)?;
        if reply.error.as_deref() == Some("models_busy") && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(10));
            continue;
        }
        break reply;
    };
    assert_eq!(partial.error, None);
    assert!(
        partial
            .models
            .iter()
            .any(|row| row.selector.as_deref() == Some("mlx:fixture"))
    );
    assert!(
        partial
            .issues
            .iter()
            .any(|issue| issue.provider.as_deref() == Some("ollama"))
    );
    fixture.stop()?;
    println!(
        "PASS metadata-only model inventory, configured missing model, partial provider failure, busy admission, responsive Inspect and owned cleanup"
    );
    Ok(())
}

fn child(home: &Path) -> Result<()> {
    tracing_subscriber::fmt()
        .with_ansi(false)
        .with_target(false)
        .with_max_level(tracing::Level::DEBUG)
        .with_writer(std::io::stderr)
        .init();
    // Test-only journal holds: inert unless the journey creates a hold file.
    asura_platform::journal_gate::install_file_gate(home);
    asura_service::run(RuntimeDirectory::scratch(home, true)?, BUILD, None)?;
    Ok(())
}
fn main() {
    let args: Vec<_> = std::env::args().collect();
    if args
        .get(1)
        .is_some_and(|a| a == "--asura-exec-cmd-guardian")
    {
        let code = match args.as_slice() {
            [_, _, job, profile, command] => {
                asura_platform::exec_cmd::run_guardian(job, profile, command)
            }
            _ => 2,
        };
        std::process::exit(code);
    }
    let result = if args.get(1).map(String::as_str) == Some("--conversation-child") {
        child(Path::new(args.get(2).expect("scratch home")))
    } else if args.iter().any(|a| a == "--journal-waits") {
        journal_wait_journey()
    } else if args.iter().any(|a| a == "--stop-drain") {
        stop_drain_journey()
    } else if args.iter().any(|a| a == "--waiting-steps") {
        waiting_step_journey::run()
    } else if args.iter().any(|a| a == "--queue-steer") {
        waiting_step_journey::steer()
    } else if args.iter().any(|a| a == "--long-inference") {
        cancellation_driven_journey::run()
    } else if args.iter().any(|a| a == "--native-exec-cmd-tools") {
        exec_cmd_journey::run("system")
    } else if args.iter().any(|a| a == "--native-ollama-exec-cmd-tools") {
        exec_cmd_journey::run("ollama")
    } else if args.iter().any(|a| a == "--native-ollama-audit-tools") {
        audit_journey::run(Some("ollama"))
    } else if args.iter().any(|a| a == "--native-mlx-exec-cmd-tools") {
        exec_cmd_journey::run("mlx")
    } else if args.iter().any(|a| a == "--native-mlx-audit-tools") {
        audit_journey::run(Some("mlx"))
    } else if args.iter().any(|a| a == "--native-coreai-exec-cmd-tools") {
        exec_cmd_journey::run("coreai")
    } else if args.iter().any(|a| a == "--native-coreai-audit-tools") {
        audit_journey::run(Some("coreai"))
    } else if args.iter().any(|a| a == "--audit") {
        audit_journey::run(None)
    } else if args.iter().any(|a| a == "--native-audit-tools") {
        audit_journey::run(Some("system"))
    } else if args.iter().any(|a| a == "--sensors") {
        sensors_journey::run()
    } else if args.iter().any(|a| a == "--models") {
        inventory_journey()
    } else if args.iter().any(|a| a == "--native-ollama-thinking") {
        native_ollama_thinking()
    } else if args.iter().any(|a| a == "--native-ollama") {
        native_ollama()
    } else if args.iter().any(|a| a == "--native-mlx") {
        native_mlx()
    } else if args.iter().any(|a| a == "--native-coreai-tools") {
        native_tools("coreai")
    } else if args.iter().any(|a| a == "--native-mlx-tools") {
        native_tools("mlx")
    } else if args.iter().any(|a| a == "--native-ollama-tools") {
        native_tools("ollama")
    } else if args.iter().any(|a| {
        matches!(
            a.as_str(),
            "--native-memory-create" | "--native-system-memory-create"
        )
    }) {
        memory_create_journey::run("system")
    } else if args.iter().any(|a| a == "--native-ollama-memory-create") {
        memory_create_journey::run("ollama")
    } else if args.iter().any(|a| a == "--native-mlx-memory-create") {
        memory_create_journey::run("mlx")
    } else if args.iter().any(|a| a == "--native-coreai-memory-create") {
        memory_create_journey::run("coreai")
    } else if args.iter().any(|a| a == "--native-memory-tools") {
        memory_tools_journey::run("system")
    } else if args.iter().any(|a| a == "--native-coreai-memory-tools") {
        memory_tools_journey::run("coreai")
    } else if args.iter().any(|a| a == "--native-mlx-memory-tools") {
        memory_tools_journey::run("mlx")
    } else if args.iter().any(|a| a == "--native-ollama-memory-tools") {
        memory_tools_journey::run("ollama")
    } else if args.iter().any(|a| a == "--native-long-ollama") {
        tools_inventory_journey::run_long_ollama()
    } else if args.iter().any(|a| a == "--native-tools-inventory") {
        tools_inventory_journey::run()
    } else if args.iter().any(|a| a == "--native-tools") {
        native_tools("system")
    } else {
        journey(args.iter().any(|a| a == "--native")).and_then(|()| {
            if args.iter().any(|a| a == "--native") {
                Ok(())
            } else {
                audit_journey::run(None)
                    .and_then(|()| journal_wait_journey())
                    .and_then(|()| stop_drain_journey())
                    .and_then(|()| waiting_step_journey::run())
                    .and_then(|()| waiting_step_journey::steer())
            }
        })
    };
    if let Err(error) = result {
        eprintln!("conversation journey FAILED: {error}");
        std::process::exit(1)
    }
}

/// Times one already-attached request; responsiveness must not depend on a
/// held journal append or replay (J1-0-07).
fn within_100ms<T>(label: &str, call: impl FnOnce() -> T) -> Result<T> {
    let started = Instant::now();
    let value = call();
    let elapsed = started.elapsed();
    if elapsed >= Duration::from_millis(100) {
        return Err(format!("{label} took {elapsed:?} during a held journal step").into());
    }
    Ok(value)
}
fn journal_wait_journey() -> Result<()> {
    let mut fixture = Fixture::new()?;
    fixture.start()?;
    wait_ready(&fixture, None)?;
    let project_dir = fixture.home.join("project");
    fs::create_dir(&project_dir)?;
    fs::set_permissions(&project_dir, fs::Permissions::from_mode(0o700))?;
    let registered = fixture.attach()?.register_project(
        pb::ProjectRegister {
            request_id: Some(asura_platform::random_id().to_vec()),
            location: Some(project_dir.to_str().ok_or("non-UTF-8 path")?.into()),
        },
        &|| false,
    )?;
    let project = checked_id(registered.project_id)?;
    // Queued inputs are held, never dispatched to a model.
    fixture
        .attach()?
        .config("model", Some("unknown:queue-fixture"), &|| false)?;

    // J1-0-02, J1-0-05, J1-0-07: a queue mutation held at a real append.
    let hold_appends = fixture.home.join("journal-hold-appends");
    fs::write(&hold_appends, b"")?;
    let held_at = Instant::now();
    let request = pb::ConversationQueueSubmit {
        request_id: Some(asura_platform::random_id().to_vec()),
        project_id: Some(project.to_vec()),
        conversation_id: None,
        expected_generation: Some(0),
        new_conversation: Some(true),
        prompt: Some("Held journal append".into()),
    };
    let mut submitter = fixture.attach()?;
    let submit = thread::spawn(move || submitter.queue_conversation_input(request, &|| false));
    thread::sleep(Duration::from_millis(300));
    for _ in 0..5 {
        let mut client = fixture.attach()?;
        within_100ms("Inspect", || client.inspect())??;
        let mut client = fixture.attach()?;
        let cancel = within_100ms("cancel", || {
            client.cancel_conversation(asura_platform::random_id(), 1, &|| false)
        })?;
        assert!(matches!(
            cancel,
            Err(asura_client::Error::Remote(_, ref message)) if message == "unknown_operation"
        ));
    }
    // Initialization has no client budget: it waits behind the held append
    // until the person cancels, which ends only the wait.
    let cancel_at = std::cell::Cell::new(None::<Instant>);
    let started = Instant::now();
    let initialized = fixture.attach()?.initialize(
        pb::InitializeInstallation {
            request_id: Some(asura_platform::random_id().to_vec()),
            mode: Some("embedded".into()),
            expected_authority_revision: Some(0),
        },
        &|| {
            if started.elapsed() < Duration::from_millis(400) {
                return false;
            }
            cancel_at.set(cancel_at.get().or(Some(Instant::now())));
            true
        },
    );
    if !matches!(initialized, Err(asura_client::Error::OutcomeUnconfirmed)) {
        return Err(format!("initialization wait was not cancelled: {initialized:?}").into());
    }
    let stopped_after = cancel_at.get().ok_or("cancel never requested")?.elapsed();
    if stopped_after >= Duration::from_millis(100) {
        return Err(format!("initialization cancel took {stopped_after:?}").into());
    }
    // Hold past the removed 2 s ticket and foreground budgets and the removed
    // 3 s client mutation budget: the client waits for the real reply.
    thread::sleep(Duration::from_millis(5_200).saturating_sub(held_at.elapsed()));
    if submit.is_finished() {
        return Err("queue mutation finished while its append was held".into());
    }
    fs::remove_file(&hold_appends)?;
    let reply = submit.join().map_err(|_| "submit thread panicked")??;
    let input = checked_id(reply.accepted_input_id.clone())?;
    if !reply
        .entries
        .iter()
        .any(|entry| entry.input_id.as_deref() == Some(input.as_slice()))
    {
        return Err(format!("late queue reply was not applied: {reply:?}").into());
    }
    if held_at.elapsed() < Duration::from_secs(5) {
        return Err("the append was not held past the removed bounds".into());
    }
    let listed = fixture.attach()?.conversation_queue(project, Some(input))?;
    if !listed
        .entries
        .iter()
        .any(|entry| entry.input_id.as_deref() == Some(input.as_slice()))
    {
        return Err("late reply did not update the service queue".into());
    }
    // The owner is neither unavailable nor fenced after the slow append.
    fixture
        .attach()?
        .projects(asura_client::ProjectPage::First, 8)?;
    println!(
        "PASS held append: no timeout, no fence, responsive Inspect/cancel, late reply applied"
    );

    // J1-0-04, J1-0-07: a held startup replay shows loading and refuses work.
    fixture.stop()?;
    let hold_reads = fixture.home.join("journal-hold-reads");
    fs::write(&hold_reads, b"")?;
    fixture.start()?;
    // Past the removed 5 s inspection replay bound.
    thread::sleep(Duration::from_millis(5_200));
    for _ in 0..5 {
        let mut client = fixture.attach()?;
        let status = within_100ms("InspectInstallation", || client.inspect_installation())??;
        if status.installation != pb::InstallationState::Recovering
            || status.reason != pb::InstallationInspectionReason::InspectionPending
        {
            return Err(format!(
                "held replay was not reported as loading: {:?}/{:?}",
                status.installation, status.reason
            )
            .into());
        }
        let mut client = fixture.attach()?;
        let refused = within_100ms("refused work", || {
            client.projects(asura_client::ProjectPage::First, 8)
        })?;
        let refused = refused?;
        if refused.error.as_deref() != Some("conversation_busy") {
            return Err(format!("work was not refused during replay: {refused:?}").into());
        }
    }
    // Shutdown cancels the held replay; the service exits cleanly.
    fixture.stop()?;
    fs::remove_file(&hold_reads)?;
    // The writer's own replay also shows loading after inspection finishes.
    let hold_writer = fixture.home.join("journal-hold-writer-reads");
    fs::write(&hold_writer, b"")?;
    fixture.start()?;
    let end = Instant::now() + Duration::from_secs(5);
    loop {
        let status = fixture.attach()?.inspect_installation()?;
        if status.installation == pb::InstallationState::Recovering
            && status.reason == pb::InstallationInspectionReason::InspectionPending
        {
            break;
        }
        if Instant::now() >= end {
            return Err(format!(
                "writer replay was not reported as loading: {:?}/{:?}",
                status.installation, status.reason
            )
            .into());
        }
        thread::sleep(Duration::from_millis(20));
    }
    thread::sleep(Duration::from_millis(500));
    let status = fixture.attach()?.inspect_installation()?;
    if status.reason != pb::InstallationInspectionReason::InspectionPending {
        return Err(format!("writer replay loading ended early: {:?}", status.reason).into());
    }
    let refused = fixture
        .attach()?
        .projects(asura_client::ProjectPage::First, 8)?;
    if refused.error.as_deref() != Some("conversation_busy") {
        return Err(format!("work was not refused during writer replay: {refused:?}").into());
    }
    fs::remove_file(&hold_writer)?;
    wait_ready(&fixture, None)?;
    fixture
        .attach()?
        .projects(asura_client::ProjectPage::First, 8)?;
    fixture.stop()?;
    println!("PASS held replay: loading state, refused work, shutdown cancels replay");
    Ok(())
}

/// Hold one queue mutation at a real journal append and return its thread.
fn held_queue_mutation(
    fixture: &Fixture,
    project: [u8; 16],
) -> Result<thread::JoinHandle<asura_client::Result<pb::ConversationQueueReply>>> {
    let request = pb::ConversationQueueSubmit {
        request_id: Some(asura_platform::random_id().to_vec()),
        project_id: Some(project.to_vec()),
        conversation_id: None,
        expected_generation: Some(0),
        new_conversation: Some(true),
        prompt: Some("Held during stop".into()),
    };
    let mut submitter = fixture.attach()?;
    let submit = thread::spawn(move || submitter.queue_conversation_input(request, &|| false));
    thread::sleep(Duration::from_millis(300));
    Ok(submit)
}

/// Observe the draining owner past the removed one-second drain bound.
fn observe_draining(fixture: &Fixture) -> Result<()> {
    let held = Instant::now() + Duration::from_millis(2_500);
    let mut draining = false;
    while Instant::now() < held {
        if let Ok(mut client) = fixture.attach()
            && let Ok(status) = client.inspect()
        {
            if status.lifecycle == pb::Lifecycle::RepairOnly as i32 {
                return Err("a slow drain moved the owner to RepairOnly".into());
            }
            draining |= status.lifecycle == pb::Lifecycle::Draining as i32;
        }
        thread::sleep(Duration::from_millis(50));
    }
    if !draining {
        return Err("Inspect did not report Draining during a held stop".into());
    }
    Ok(())
}

/// Q-B, CW-03 and CW-04 against a real service process (owner, 2026-10-16):
/// Stop waits for a held journal append with no drain deadline. Inspect
/// reports Draining, never RepairOnly; the stop client shows progress; the
/// held request's wait ends with a lost connection. Releasing the hold lets
/// the original Stop complete; otherwise a second Stop forces exit and the
/// next owner recovers.
fn stop_drain_journey() -> Result<()> {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    let mut fixture = Fixture::new()?;
    fixture.start()?;
    let installation = wait_ready(&fixture, None)?
        .installation_id
        .map(|id| id.to_vec());
    let project_dir = fixture.home.join("project");
    fs::create_dir(&project_dir)?;
    fs::set_permissions(&project_dir, fs::Permissions::from_mode(0o700))?;
    let registered = fixture.attach()?.register_project(
        pb::ProjectRegister {
            request_id: Some(asura_platform::random_id().to_vec()),
            location: Some(project_dir.to_str().ok_or("non-UTF-8 path")?.into()),
        },
        &|| false,
    )?;
    let project = checked_id(registered.project_id)?;
    fixture
        .attach()?
        .config("model", Some("unknown:queue-fixture"), &|| false)?;
    let hold = fixture.home.join("journal-hold-appends");

    // Round 1: the held append drains; the original Stop then completes.
    fs::write(&hold, b"")?;
    let submit = held_queue_mutation(&fixture, project)?;
    let progress = Arc::new(AtomicUsize::new(0));
    let seen = progress.clone();
    let stopper = fixture.attach()?;
    let stop = thread::spawn(move || {
        stopper.stop_observed(&mut |snapshot| {
            if snapshot.lifecycle == pb::Lifecycle::Draining as i32 {
                seen.fetch_add(1, Ordering::AcqRel);
            }
        })
    });
    observe_draining(&fixture)?;
    if stop.is_finished() {
        return Err("stop wait ended while a worker was still settling".into());
    }
    if progress.load(Ordering::Acquire) == 0 {
        return Err("the stop wait showed no progress".into());
    }
    if fixture
        .child
        .as_mut()
        .ok_or("service child")?
        .try_wait()?
        .is_some()
    {
        return Err("the service exited before its held worker settled".into());
    }
    // CW-03: drain closed the held request's connection.
    if !matches!(
        submit.join().map_err(|_| "submit thread panicked")?,
        Err(asura_client::Error::OutcomeUnconfirmed)
    ) {
        return Err("held request did not end with a lost connection".into());
    }
    fs::remove_file(&hold)?;
    stop.join().map_err(|_| "stop thread panicked")??;
    let end = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(status) = fixture.child.as_mut().ok_or("service child")?.try_wait()? {
            fixture.child.take();
            if !status.success() {
                return Err(format!("drained service exit {status}").into());
            }
            break;
        }
        if Instant::now() >= end {
            return Err("service did not exit after its held worker settled".into());
        }
        thread::sleep(Duration::from_millis(10));
    }
    println!(
        "PASS stop drain: no deadline, Draining with progress, lost-connection wait, clean exit"
    );

    // Round 2: a second Stop forces exit while the append is still held.
    fixture.start()?;
    wait_ready(&fixture, installation.clone())?;
    fs::write(&hold, b"")?;
    let submit = held_queue_mutation(&fixture, project)?;
    let first = fixture.attach()?;
    let first_stop = thread::spawn(move || first.stop());
    let end = Instant::now() + Duration::from_secs(5);
    loop {
        if let Ok(mut client) = fixture.attach()
            && let Ok(status) = client.inspect()
            && status.lifecycle == pb::Lifecycle::Draining as i32
        {
            break;
        }
        if Instant::now() >= end {
            return Err("first stop did not start draining".into());
        }
        thread::sleep(Duration::from_millis(20));
    }
    let forced_at = Instant::now();
    fixture.attach()?.stop()?;
    let end = Instant::now() + Duration::from_secs(5);
    loop {
        if fixture
            .child
            .as_mut()
            .ok_or("service child")?
            .try_wait()?
            .is_some()
        {
            fixture.child.take();
            break;
        }
        if Instant::now() >= end {
            return Err("second stop did not force exit".into());
        }
        thread::sleep(Duration::from_millis(10));
    }
    let forced_after = forced_at.elapsed();
    first_stop
        .join()
        .map_err(|_| "first stop thread panicked")??;
    let _ = submit.join().map_err(|_| "submit thread panicked")?;
    fs::remove_file(&hold)?;
    // The next owner recovers by replay, as after a crash.
    fixture.start()?;
    wait_ready(&fixture, installation)?;
    fixture
        .attach()?
        .projects(asura_client::ProjectPage::First, 8)?;
    fixture.stop()?;
    println!("PASS second stop forced exit in {forced_after:?}; next owner recovered");
    Ok(())
}
