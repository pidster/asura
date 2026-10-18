use asura_platform::{JournalFile, ProjectIdentity, RuntimeDirectory, random_id};
use asura_storage::authority::{conversation::*, writer::*};
use std::sync::atomic::AtomicBool;
use std::{
    fs,
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    time::{Duration, Instant},
};
static TEST_WRITER: std::sync::Mutex<()> = std::sync::Mutex::new(());
struct Scratch(std::path::PathBuf);
impl Scratch {
    fn new() -> Self {
        let path = format!(
            "/private/tmp/asura-writer-{:x}",
            u128::from_ne_bytes(random_id())
        )
        .into();
        fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
        Self(path)
    }
    fn runtime(&self) -> RuntimeDirectory {
        RuntimeDirectory::scratch(&self.0, true).unwrap()
    }
    /// A project beside the Asura home; the home itself may not be a project.
    fn project(&self) -> String {
        let path = self.0.join("project");
        if !path.exists() {
            fs::create_dir(&path).unwrap();
        }
        path.to_str().unwrap().into()
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
/// Test-harness settling bound; the writer itself has no time bound.
fn settle_by() -> Instant {
    Instant::now() + Duration::from_secs(5)
}
fn pending() -> Vec<u8> {
    encode_frame(
        &FrameContext {
            installation_id: [1; 16],
            transition_id: [2; 16],
            sequence: 1,
            prior_digest: [0; 32],
            expected_revision: 0,
            owner_generation: 1,
        },
        &Record::PendingInit(PendingInit {
            request: [3; 16],
            digest: request_digest(Request::Initialize { mode: 1 }).unwrap(),
            mode: 1,
            configuration_revision: 1,
            configuration_digest: [4; 32],
            graph: [5; 16],
        }),
    )
    .unwrap()
}
fn wait(ticket: &mut Ticket) -> Result<Reply> {
    let end = settle_by();
    loop {
        if let Some(result) = ticket.poll() {
            return result;
        }
        assert!(Instant::now() < end);
        std::thread::sleep(Duration::from_millis(1));
    }
}
fn close(writer: &mut WriterHandle) {
    writer.close();
    let end = settle_by();
    while !writer.settled() {
        assert!(Instant::now() < end);
        std::thread::sleep(Duration::from_millis(1));
    }
}
#[cfg(feature = "embedded-memory")]
#[test]
fn project_rename_preserves_registration_alias_and_exact_retry_across_restart() {
    let _serial = TEST_WRITER.lock().unwrap_or_else(|p| p.into_inner());
    let scratch = Scratch::new();
    let runtime = scratch.runtime();
    let mut writer = WriterHandle::start(runtime.clone()).unwrap();
    wait(
        &mut writer
            .try_submit(Command::Initialize { request: [201; 16] })
            .unwrap(),
    )
    .unwrap();
    let project_path = scratch.0.join("asura");
    fs::create_dir(&project_path).unwrap();
    let location = project_path.to_str().unwrap().to_owned();
    let registered = wait(
        &mut writer
            .try_submit(Command::Register {
                request: [202; 16],
                location: location.clone(),
            })
            .unwrap(),
    )
    .unwrap();
    let registered_state = registered.replay.unwrap();
    let project = *registered_state.projects.keys().next().unwrap();
    let (default_name, revision) = registered_state.project_name(project).unwrap();
    assert_eq!(revision, 0);
    assert_eq!(default_name, "Asura");
    drop(registered_state);

    let rename = |request, expected_name_revision, name: &str| Command::RenameProject {
        request,
        project,
        expected_name_revision,
        name: name.into(),
    };
    let first = wait(
        &mut writer
            .try_submit(rename([203; 16], 0, "my project"))
            .unwrap(),
    )
    .unwrap();
    assert_eq!(first.project_name.as_deref(), Some("My project"));
    assert_eq!(first.project_name_revision, Some(1));
    assert_eq!(first.project_name_changed, Some(true));
    let first_sequence = first.replay.as_ref().unwrap().sequence;
    drop(first);
    let stale = wait(
        &mut writer
            .try_submit(rename([204; 16], 0, "other name"))
            .unwrap(),
    )
    .unwrap();
    assert!(stale.stale_project_name_revision);
    assert_eq!(stale.project_name_revision, Some(1));
    assert_eq!(stale.replay.as_ref().unwrap().sequence, first_sequence);
    drop(stale);
    let second = wait(
        &mut writer
            .try_submit(rename([205; 16], 1, "second name"))
            .unwrap(),
    )
    .unwrap();
    assert_eq!(second.project_name.as_deref(), Some("Second name"));
    assert_eq!(second.project_name_revision, Some(2));
    let second_sequence = second.replay.as_ref().unwrap().sequence;
    drop(second);
    let retry = wait(
        &mut writer
            .try_submit(rename([203; 16], 0, "my project"))
            .unwrap(),
    )
    .unwrap();
    assert_eq!(retry.project_name.as_deref(), Some("My project"));
    assert_eq!(retry.project_name_revision, Some(1));
    assert_eq!(
        retry.replay.as_ref().unwrap().project_name(project),
        Some(("Second name".into(), 2))
    );
    assert_eq!(retry.replay.as_ref().unwrap().sequence, second_sequence);
    drop(retry);
    assert!(matches!(
        wait(
            &mut writer
                .try_submit(rename([203; 16], 0, "different name"))
                .unwrap()
        ),
        Err(Error::Conflict)
    ));
    let alias = wait(
        &mut writer
            .try_submit(Command::Register {
                request: [206; 16],
                location,
            })
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        alias.replay.as_ref().unwrap().projects[&project].registry_revision,
        1
    );
    assert_eq!(
        alias.replay.as_ref().unwrap().project_name(project),
        Some(("Second name".into(), 2))
    );
    drop(alias);
    close(&mut writer);
    drop(writer);
    let mut restarted = WriterHandle::start(runtime).unwrap();
    let restored = wait(&mut restarted.try_submit(Command::Open).unwrap()).unwrap();
    assert_eq!(
        restored.replay.as_ref().unwrap().project_name(project),
        Some(("Second name".into(), 2))
    );
    assert_eq!(
        restored.replay.as_ref().unwrap().projects[&project].registry_revision,
        1
    );
    drop(restored);
    fs::remove_dir(&project_path).unwrap();
    fs::create_dir(&project_path).unwrap();
    let sequence = wait(&mut restarted.try_submit(Command::Open).unwrap())
        .unwrap()
        .replay
        .unwrap()
        .sequence;
    assert!(matches!(
        wait(
            &mut restarted
                .try_submit(rename([207; 16], 2, "third name"))
                .unwrap()
        ),
        Err(Error::StaleProject)
    ));
    let after = wait(&mut restarted.try_submit(Command::Open).unwrap())
        .unwrap()
        .replay
        .unwrap();
    assert_eq!(after.sequence, sequence);
    assert_eq!(after.project_name(project), Some(("Second name".into(), 2)));
    drop(after);
    close(&mut restarted);
}
#[cfg(feature = "embedded-memory")]
#[test]
fn managed_queue_commits_full_projection_cas_retry_and_restart_hold() {
    let _serial = TEST_WRITER.lock().unwrap_or_else(|p| p.into_inner());
    let scratch = Scratch::new();
    let mut writer = WriterHandle::start(scratch.runtime()).unwrap();
    wait(
        &mut writer
            .try_submit(Command::Initialize { request: [110; 16] })
            .unwrap(),
    )
    .unwrap();
    let registered = wait(
        &mut writer
            .try_submit(Command::Register {
                request: [111; 16],
                location: scratch.project(),
            })
            .unwrap(),
    )
    .unwrap()
    .replay
    .unwrap();
    let project = *registered.projects.keys().next().unwrap();
    drop(registered);
    let first = wait(
        &mut writer
            .try_submit(Command::QueueInput {
                request: [112; 16],
                project,
                conversation: None,
                expected_generation: 0,
                new_conversation: true,
                prompt: "first".into(),
            })
            .unwrap(),
    )
    .unwrap();
    let first_id = first.accepted_input_id.unwrap();
    let lane = first.inputs[0].input.conversation;
    assert_eq!(first.order_revision, Some(1));
    assert_eq!(first.inputs.len(), 1);
    drop(first);
    let second = wait(
        &mut writer
            .try_submit(Command::QueueInput {
                request: [113; 16],
                project,
                conversation: Some(lane),
                expected_generation: 0,
                new_conversation: false,
                prompt: "second".into(),
            })
            .unwrap(),
    )
    .unwrap();
    let second_id = second.accepted_input_id.unwrap();
    assert_eq!(second.inputs.len(), 2);
    assert_eq!(second.order_revision, Some(2));
    drop(second);
    let changed = wait(
        &mut writer
            .try_submit(Command::ReorderInput {
                request: [114; 16],
                input: second_id,
                after: None,
                expected_order_revision: 2,
            })
            .unwrap(),
    )
    .unwrap();
    assert_eq!(changed.order_revision, Some(3));
    assert_eq!(
        changed
            .inputs
            .iter()
            .map(|q| q.input.input)
            .collect::<Vec<_>>(),
        vec![second_id, first_id]
    );
    drop(changed);
    let retry = wait(
        &mut writer
            .try_submit(Command::ReorderInput {
                request: [114; 16],
                input: second_id,
                after: None,
                expected_order_revision: 2,
            })
            .unwrap(),
    )
    .unwrap();
    assert_eq!(retry.order_revision, Some(3));
    drop(retry);
    let stale = wait(
        &mut writer
            .try_submit(Command::ReorderInput {
                request: [115; 16],
                input: first_id,
                after: None,
                expected_order_revision: 2,
            })
            .unwrap(),
    )
    .unwrap();
    assert!(stale.stale_order);
    assert_eq!(stale.order_revision, Some(3));
    assert_eq!(stale.inputs.len(), 2);
    drop(stale);
    close(&mut writer);
    let mut writer = WriterHandle::start(scratch.runtime()).unwrap();
    let state = wait(&mut writer.try_submit(Command::Open).unwrap())
        .unwrap()
        .replay
        .unwrap();
    assert_eq!(state.order_revision, 3);
    assert_eq!(state.input_status(first_id), Some(InputStatus::Held));
    assert_eq!(state.input_status(second_id), Some(InputStatus::Held));
    close(&mut writer);
}
#[test]
fn descriptor_append_reopens_and_rejects_replacement_without_truncation() {
    let scratch = Scratch::new();
    let runtime = scratch.runtime();
    let bytes = pending();
    let mut file = JournalFile::open(runtime.clone(), true).unwrap();
    file.append(0, &bytes).unwrap();
    assert!(JournalFile::open(runtime.clone(), false).is_err());
    assert_eq!(file.read_all(&AtomicBool::new(false)).unwrap(), bytes);
    assert_eq!(file.read_all(&AtomicBool::new(false)).unwrap(), bytes);
    drop(file);
    let mut file = JournalFile::open(runtime, false).unwrap();
    assert_eq!(file.read_all(&AtomicBool::new(false)).unwrap(), bytes);
    let path = scratch.0.join(".asura/state/control/slot-0.log");
    fs::rename(&path, path.with_extension("saved")).unwrap();
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .unwrap();
    assert!(file.append(bytes.len(), &[1]).is_err());
    assert_eq!(fs::read(path.with_extension("saved")).unwrap(), bytes);
}
#[test]
fn project_identity_detects_same_path_replacement_and_symlink() {
    let scratch = Scratch::new();
    let path = scratch.0.join("project");
    fs::create_dir(&path).unwrap();
    let boundary = asura_platform::StateBoundary::detached_for_test();
    let held = ProjectIdentity::open(path.to_str().unwrap(), &boundary).unwrap();
    held.validate().unwrap();
    fs::rename(&path, scratch.0.join("old")).unwrap();
    fs::create_dir(&path).unwrap();
    assert!(held.validate().is_err());
    let link = scratch.0.join("link");
    std::os::unix::fs::symlink(&path, &link).unwrap();
    assert!(ProjectIdentity::open(link.to_str().unwrap(), &boundary).is_err());
}
#[test]
fn writer_preserves_incomplete_tail_and_does_not_recreate_missing_authority() {
    let _guard = TEST_WRITER.lock().unwrap_or_else(|p| p.into_inner());
    let scratch = Scratch::new();
    let runtime = scratch.runtime();
    let mut file = JournalFile::open(runtime.clone(), true).unwrap();
    let mut bytes = pending();
    bytes.extend_from_slice(b"ASU");
    file.append(0, &bytes).unwrap();
    drop(file);
    let mut writer = WriterHandle::start(runtime).unwrap();
    let result = wait(&mut writer.try_submit(Command::Open).unwrap());
    assert!(matches!(result, Err(Error::RepairRequired)));
    close(&mut writer);
    assert_eq!(
        fs::read(scratch.0.join(".asura/state/control/slot-0.log")).unwrap(),
        bytes
    );
}

#[cfg(feature = "embedded-memory")]
#[test]
fn different_initialization_request_preserves_pending_state() {
    let _guard = TEST_WRITER.lock().unwrap_or_else(|p| p.into_inner());
    let scratch = Scratch::new();
    let runtime = scratch.runtime();
    let mut file = JournalFile::open(runtime.clone(), true).unwrap();
    file.append(0, &pending()).unwrap();
    drop(file);
    let mut writer = WriterHandle::start(runtime).unwrap();
    let state = wait(&mut writer.try_submit(Command::Open).unwrap())
        .unwrap()
        .replay
        .unwrap();
    assert!(state.binding.is_none());
    let journal = scratch.0.join(".asura/state/control/slot-0.log");
    let before = fs::read(&journal).unwrap();
    assert!(matches!(
        wait(
            &mut writer
                .try_submit(Command::Initialize { request: [21; 16] })
                .unwrap()
        ),
        Err(Error::Busy)
    ));
    assert_eq!(before, fs::read(&journal).unwrap());
    assert!(!scratch.0.join(".asura/db").exists());
    close(&mut writer);
}

#[cfg(feature = "embedded-memory")]
#[test]
fn initialization_registration_admission_and_restart_are_durable() {
    let _guard = TEST_WRITER.lock().unwrap_or_else(|p| p.into_inner());
    initialization_registration_admission_and_restart_with_budget();
}
#[cfg(feature = "embedded-memory")]
fn initialization_registration_admission_and_restart_with_budget() {
    let scratch = Scratch::new();
    let runtime = scratch.runtime();
    let mut writer = WriterHandle::start(runtime.clone()).unwrap();
    assert!(
        wait(&mut writer.try_submit(Command::Open).unwrap())
            .unwrap()
            .replay
            .is_none()
    );
    let init = wait(
        &mut writer
            .try_submit(Command::Initialize { request: [11; 16] })
            .unwrap(),
    )
    .unwrap()
    .replay
    .unwrap();
    assert!(init.binding.is_some());
    let duplicate = wait(
        &mut writer
            .try_submit(Command::Initialize { request: [11; 16] })
            .unwrap(),
    )
    .unwrap()
    .replay
    .unwrap();
    assert_eq!(init.revision, duplicate.revision);
    let journal = scratch.0.join(".asura/state/control/slot-0.log");
    let before = fs::read(&journal).unwrap();
    assert!(matches!(
        wait(
            &mut writer
                .try_submit(Command::Initialize { request: [21; 16] })
                .unwrap()
        ),
        Err(Error::AlreadyInitialized)
    ));
    let after = wait(&mut writer.try_submit(Command::Open).unwrap())
        .unwrap()
        .replay
        .unwrap();
    assert_eq!(init.installation_id, after.installation_id);
    assert_eq!(init.revision, after.revision);
    drop(after);
    assert_eq!(before, fs::read(&journal).unwrap());
    let location = scratch.project();
    let registered = wait(
        &mut writer
            .try_submit(Command::Register {
                request: [12; 16],
                location,
            })
            .unwrap(),
    )
    .unwrap()
    .replay
    .unwrap();
    let project = *registered.projects.keys().next().unwrap();
    assert!(matches!(
        wait(
            &mut writer
                .try_submit(Command::Initialize { request: [12; 16] })
                .unwrap()
        ),
        Err(Error::Conflict)
    ));
    drop(init);
    drop(duplicate);
    let prepared = wait(
        &mut writer
            .try_submit(Command::Prepare {
                project,
                conversation: None,
                expected_generation: 0,
                prompt: "hello".into(),
            })
            .unwrap(),
    )
    .unwrap()
    .prepared
    .unwrap();
    let turn = TurnAccepted {
        request: [13; 16],
        digest: request_digest(Request::Submit {
            project,
            conversation: None,
            expected_generation: 0,
            prompt: "hello",
        })
        .unwrap(),
        project,
        original_conversation: None,
        expected_generation: 0,
        conversation: [14; 16],
        generation: 1,
        task: [15; 16],
        operation: [16; 16],
        model: "system".into(),
        configuration_digest: prepared.configuration_digest,
        instructions_digest: [17; 32],
        input_digest: [18; 32],
        prior_operations: Vec::new(),
        prompt: "hello".into(),
        event_cursor: 1,
        composition: Composition::default(),
    };
    let accepted = wait(
        &mut writer
            .try_submit(Command::Append {
                expected_revision: registered.revision,
                record: Record::TurnAccepted(Box::new(turn.clone())),
                admission: Some(AdmissionFence {
                    project,
                    configuration_digest: prepared.configuration_digest,
                }),
            })
            .unwrap(),
    )
    .unwrap()
    .replay
    .unwrap();
    assert_eq!(accepted.active_operation, Some([16; 16]));
    drop(registered);
    drop(accepted);
    // Every queued input is kind 18, also while an operation is active.
    let queue_command = || Command::QueueInput {
        request: [60; 16],
        project,
        conversation: Some([14; 16]),
        expected_generation: 1,
        new_conversation: false,
        prompt: "durable queued text".into(),
    };
    let queued = wait(&mut writer.try_submit(queue_command()).unwrap()).unwrap();
    let input = queued.accepted_input_id.unwrap();
    let entry = queued
        .inputs
        .iter()
        .find(|v| v.input.input == input)
        .unwrap();
    assert_eq!(entry.status, InputStatus::Queued);
    assert_eq!(entry.kind, InputKind::Queue);
    assert_eq!(entry.target_operation, None);
    let queued_revision = queued.replay.as_ref().unwrap().revision;
    drop(queued);
    let repeated = wait(&mut writer.try_submit(queue_command()).unwrap()).unwrap();
    assert_eq!(repeated.replay.unwrap().revision, queued_revision);
    let before_queue_conflict = fs::read(&journal).unwrap();
    assert!(matches!(
        wait(
            &mut writer
                .try_submit(Command::QueueInput {
                    request: [60; 16],
                    project,
                    conversation: Some([14; 16]),
                    expected_generation: 1,
                    new_conversation: false,
                    prompt: "changed text".into(),
                })
                .unwrap()
        ),
        Err(Error::Conflict)
    ));
    assert_eq!(fs::read(&journal).unwrap(), before_queue_conflict);
    // MQ06 regression: promoting a queued kind 18 input during the active
    // turn commits kind 16; it never reports repair_required.
    let promote = |generation| Command::PromoteInput {
        request: [65; 16],
        input,
        target_operation: [16; 16],
        target_generation: generation,
    };
    assert!(matches!(
        wait(&mut writer.try_submit(promote(2)).unwrap()),
        Err(Error::Conflict)
    ));
    assert_eq!(fs::read(&journal).unwrap(), before_queue_conflict);
    let promoted = wait(&mut writer.try_submit(promote(1)).unwrap()).unwrap();
    assert_eq!(promoted.inputs[0].input.input, input);
    assert_eq!(promoted.inputs[0].kind, InputKind::Steer);
    assert_eq!(promoted.inputs[0].target_operation, Some([16; 16]));
    assert_eq!(promoted.replay.as_ref().unwrap().inputs.len(), 1);
    assert_eq!(
        promoted.replay.as_ref().unwrap().steering_input([16; 16]),
        Some(input)
    );
    let promoted_revision = promoted.replay.as_ref().unwrap().revision;
    drop(promoted);
    let retried = wait(&mut writer.try_submit(promote(1)).unwrap()).unwrap();
    assert_eq!(retried.replay.unwrap().revision, promoted_revision);
    assert!(matches!(
        wait(&mut writer.try_submit(promote(2)).unwrap()),
        Err(Error::Conflict)
    ));
    let original_retry = wait(&mut writer.try_submit(queue_command()).unwrap()).unwrap();
    assert_eq!(original_retry.replay.unwrap().revision, promoted_revision);
    close(&mut writer);
    let mut writer = WriterHandle::start(runtime.clone()).unwrap();
    let recovered = wait(&mut writer.try_submit(Command::Open).unwrap())
        .unwrap()
        .replay
        .unwrap();
    assert_eq!(recovered.active_operation, None);
    let terminal = recovered.operations[&[16; 16]].terminal.as_ref().unwrap();
    assert_eq!(terminal.kind, TerminalKind::Interrupted);
    assert!(terminal.usage_known);
    assert_eq!(terminal.output_tokens, 0);
    let read = wait(
        &mut writer
            .try_submit(Command::ReadRecord {
                range: terminal.frame.clone(),
            })
            .unwrap(),
    )
    .unwrap();
    assert!(matches!(read.record, Some(Record::TurnTerminal(_))));
    drop(read);
    assert_eq!(recovered.input_status(input), Some(InputStatus::Held));
    assert_eq!(recovered.ready_input(), None);
    drop(recovered);
    let inspected = wait(
        &mut writer
            .try_submit(Command::Inputs {
                project,
                input: Some(input),
            })
            .unwrap(),
    )
    .unwrap();
    assert_eq!(inspected.inputs[0].input.prompt, "durable queued text");
    assert_eq!(inspected.inputs[0].status, InputStatus::Held);
    drop(inspected);
    let resumed = wait(
        &mut writer
            .try_submit(Command::InputDecision {
                request: [61; 16],
                input,
                action: InputAction::Resume,
            })
            .unwrap(),
    )
    .unwrap();
    assert_eq!(resumed.inputs[0].status, InputStatus::Queued);
    drop(resumed);
    let queued_preparation =
        wait(&mut writer.try_submit(Command::PrepareInput { input }).unwrap()).unwrap();
    assert_eq!(
        queued_preparation.queued.unwrap().prompt,
        "Original request:\nhello\n\nSteering instruction:\ndurable queued text"
    );
    drop(queued_preparation.replay);
    let held = wait(
        &mut writer
            .try_submit(Command::InputDecision {
                request: [62; 16],
                input,
                action: InputAction::Hold,
            })
            .unwrap(),
    )
    .unwrap();
    drop(held);
    let dropped = wait(
        &mut writer
            .try_submit(Command::InputDecision {
                request: [63; 16],
                input,
                action: InputAction::Drop,
            })
            .unwrap(),
    )
    .unwrap();
    assert_eq!(dropped.inputs[0].status, InputStatus::Dropped);
    let revision = dropped.replay.unwrap().revision;
    let before_repeat = fs::read(&journal).unwrap();
    let repeated = wait(
        &mut writer
            .try_submit(Command::InputDecision {
                request: [63; 16],
                input,
                action: InputAction::Drop,
            })
            .unwrap(),
    )
    .unwrap();
    assert_eq!(repeated.replay.unwrap().revision, revision);
    assert_eq!(fs::read(&journal).unwrap(), before_repeat);
    let mut second = turn;
    second.request = [21; 16];
    second.task = [22; 16];
    second.operation = [23; 16];
    second.original_conversation = Some([14; 16]);
    second.expected_generation = 1;
    second.generation = 2;
    second.digest = request_digest(Request::Submit {
        project,
        conversation: Some([14; 16]),
        expected_generation: 1,
        prompt: "hello",
    })
    .unwrap();
    let accepted = wait(
        &mut writer
            .try_submit(Command::Append {
                expected_revision: revision,
                record: Record::TurnAccepted(Box::new(second)),
                admission: Some(AdmissionFence {
                    project,
                    configuration_digest: prepared.configuration_digest,
                }),
            })
            .unwrap(),
    )
    .unwrap()
    .replay
    .unwrap();
    let revision = accepted.revision;
    drop(accepted);
    let started = wait(
        &mut writer
            .try_submit(Command::Append {
                expected_revision: revision,
                record: Record::StartAuthorized(StartAuthorized {
                    operation: [23; 16],
                    generation: 2,
                    helper: [24; 16],
                    input_digest: [18; 32],
                }),
                admission: None,
            })
            .unwrap(),
    )
    .unwrap();
    drop(started);
    close(&mut writer);
    let mut writer = WriterHandle::start(runtime.clone()).unwrap();
    let recovered = wait(&mut writer.try_submit(Command::Open).unwrap())
        .unwrap()
        .replay
        .unwrap();
    let terminal = recovered.operations[&[23; 16]].terminal.as_ref().unwrap();
    assert_eq!(terminal.kind, TerminalKind::Interrupted);
    assert!(!terminal.usage_known);
    assert_eq!(terminal.output_tokens, 0);
    close(&mut writer);
    let db = scratch.0.join(".asura/db");
    fs::rename(&db, scratch.0.join("saved-db")).unwrap();
    let mut writer = WriterHandle::start(runtime).unwrap();
    assert!(matches!(
        wait(&mut writer.try_submit(Command::Open).unwrap()),
        Err(Error::RepairRequired)
    ));
    assert!(!db.exists());
    close(&mut writer);
}

#[test]
fn retained_snapshot_generations_apply_backpressure_before_writes() {
    let _guard = TEST_WRITER.lock().unwrap_or_else(|p| p.into_inner());
    let scratch = Scratch::new();
    let runtime = scratch.runtime();
    let mut file = JournalFile::open(runtime.clone(), true).unwrap();
    file.append(0, &pending()).unwrap();
    drop(file);
    let mut writer = WriterHandle::start(runtime).unwrap();
    let first = wait(&mut writer.try_submit(Command::Open).unwrap())
        .unwrap()
        .replay
        .unwrap();
    let second = wait(
        &mut writer
            .try_submit(Command::Append {
                expected_revision: first.revision,
                record: Record::OwnerGeneration,
                admission: None,
            })
            .unwrap(),
    )
    .unwrap()
    .replay
    .unwrap();
    let path = scratch.0.join(".asura/state/control/slot-0.log");
    let before = fs::read(&path).unwrap();
    let command = || Command::Append {
        expected_revision: second.revision,
        record: Record::OwnerGeneration,
        admission: None,
    };
    assert!(matches!(
        wait(&mut writer.try_submit(command()).unwrap()),
        Err(Error::Busy)
    ));
    assert_eq!(fs::read(&path).unwrap(), before);
    drop(first);
    assert!(wait(&mut writer.try_submit(command()).unwrap()).is_ok());
    close(&mut writer);
}

#[cfg(feature = "embedded-memory")]
struct SettledWriter(WriterHandle);
#[cfg(feature = "embedded-memory")]
impl std::ops::Deref for SettledWriter {
    type Target = WriterHandle;
    fn deref(&self) -> &WriterHandle {
        &self.0
    }
}
#[cfg(feature = "embedded-memory")]
impl std::ops::DerefMut for SettledWriter {
    fn deref_mut(&mut self) -> &mut WriterHandle {
        &mut self.0
    }
}
#[cfg(feature = "embedded-memory")]
impl Drop for SettledWriter {
    fn drop(&mut self) {
        self.0.close();
        let end = Instant::now() + Duration::from_secs(10);
        while !self.0.settled() && Instant::now() < end {
            std::thread::sleep(Duration::from_millis(1));
        }
        if !std::thread::panicking() {
            assert!(self.0.settled(), "fixture writer failed to settle");
        }
    }
}

#[cfg(feature = "embedded-memory")]
#[test]
/// HM3 with TCR-D2: a `run` create_note kind 14, a put with the typed body,
/// a lost response and restart, then resolution by the derived identity.
fn memory_create_recovery_retains_intent_and_resolves_without_repeating_write() {
    use asura_storage::memory::{self, MemoryCreateResult};
    let _serial = TEST_WRITER.lock().unwrap();
    let scratch = Scratch::new();
    let mut writer = SettledWriter(WriterHandle::start(scratch.runtime()).unwrap());
    wait(
        &mut writer
            .try_submit(Command::Initialize { request: [100; 16] })
            .unwrap(),
    )
    .unwrap();
    let registered = wait(
        &mut writer
            .try_submit(Command::Register {
                request: [101; 16],
                location: scratch.project(),
            })
            .unwrap(),
    )
    .unwrap()
    .replay
    .unwrap();
    let project = *registered.projects.keys().next().unwrap();
    let prepared = wait(
        &mut writer
            .try_submit(Command::Prepare {
                project,
                conversation: None,
                expected_generation: 0,
                prompt: "remember".into(),
            })
            .unwrap(),
    )
    .unwrap()
    .prepared
    .unwrap();
    let accepted = TurnAccepted {
        request: [102; 16],
        digest: request_digest(Request::Submit {
            project,
            conversation: None,
            expected_generation: 0,
            prompt: "remember",
        })
        .unwrap(),
        project,
        original_conversation: None,
        expected_generation: 0,
        conversation: [103; 16],
        generation: 1,
        task: [104; 16],
        operation: [105; 16],
        model: "system".into(),
        configuration_digest: prepared.configuration_digest,
        instructions_digest: [106; 32],
        input_digest: [107; 32],
        prior_operations: vec![],
        prompt: "remember".into(),
        event_cursor: 1,
        composition: Composition::default(),
    };
    let mut state = wait(
        &mut writer
            .try_submit(Command::Append {
                expected_revision: registered.revision,
                record: Record::TurnAccepted(Box::new(accepted)),
                admission: Some(AdmissionFence {
                    project,
                    configuration_digest: prepared.configuration_digest,
                }),
            })
            .unwrap(),
    )
    .unwrap()
    .replay
    .unwrap();
    drop(registered);
    state = wait(
        &mut writer
            .try_submit(Command::Append {
                expected_revision: state.revision,
                record: Record::StartAuthorized(StartAuthorized {
                    operation: [105; 16],
                    generation: 1,
                    helper: [108; 16],
                    input_digest: [107; 32],
                }),
                admission: None,
            })
            .unwrap(),
    )
    .unwrap()
    .replay
    .unwrap();
    let binding = memory::Binding {
        installation_id: memory::Id::new(state.installation_id).unwrap(),
        graph_id: memory::Id::new(state.initialization.graph).unwrap(),
        init_operation_id: memory::Id::new(state.initialization.request).unwrap(),
    };
    let (intent, _) = memory::prepare_create(
        binding,
        memory::Id::new(project).unwrap(),
        [105; 16],
        1,
        1,
        "durable note".into(),
        None,
    )
    .unwrap();
    // HM3 (TCR-D2): the `run` kind 14 is the authority; the identities are
    // derived from the call, never read from the journal.
    let arguments = "{\"command\":\"create_note\",\"body\":\"durable note\"}";
    state = wait(
        &mut writer
            .try_submit(Command::Append {
                expected_revision: state.revision,
                record: Record::ToolCall(Box::new(ToolCall {
                    operation: [105; 16],
                    generation: 1,
                    ordinal: 1,
                    source: ToolCallSource::GeneratedContent,
                    name: "memory".into(),
                    name_bytes: 6,
                    tool_operation: Some(ToolOperation::CreateNote),
                    decision: ToolDecision::Run,
                    fault: None,
                    busy: None,
                    form: ArgumentForm::Verbatim,
                    arguments_bytes: arguments.len() as u32,
                    arguments: arguments.into(),
                    number_kinds: vec![],
                    unnumbered: 0,
                    rule_set_revision: 1,
                    rule_set_digest: [21; 32],
                    command_cut: false,
                    refusal: None,
                })),
                admission: None,
            })
            .unwrap(),
    )
    .unwrap()
    .replay
    .unwrap();
    assert_eq!(state.unresolved_creates().next(), Some(&intent));
    assert_eq!(state.operations[&[105; 16]].tools[0].create, Some(intent));
    assert!(state.project_memory_blocked(project));
    assert!(matches!(
        wait(
            &mut writer
                .try_submit(Command::MemoryRead {
                    project,
                    query: memory::ReadNotes::List {
                        after: None,
                        limit: 16
                    }
                })
                .unwrap()
        ),
        Err(Error::Busy)
    ));
    // Cancellation queued before database submission produces no partial effect.
    let cancelled = wait(
        &mut writer
            .try_submit(Command::MemoryCreate {
                turn: [105; 16],
                generation: 1,
                ordinal: 1,
                body: "durable note".into(),
                source: None,
                cancel: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
            })
            .unwrap(),
    )
    .unwrap()
    .memory_create_result
    .unwrap();
    assert_eq!(cancelled, Err(memory::Error::Cancelled));
    let absent = wait(
        &mut writer
            .try_submit(Command::MemoryResolveCreate {
                turn: [105; 16],
                generation: 1,
                ordinal: 1,
            })
            .unwrap(),
    )
    .unwrap()
    .memory_create_result
    .unwrap()
    .unwrap();
    assert_eq!(absent, MemoryCreateResult::NotCommitted);
    let outcome = wait(
        &mut writer
            .try_submit(Command::MemoryCreate {
                cancel: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
                turn: [105; 16],
                generation: 1,
                ordinal: 1,
                body: "durable note".into(),
                source: None,
            })
            .unwrap(),
    )
    .unwrap()
    .memory_create_result
    .unwrap()
    .unwrap();
    let MemoryCreateResult::Committed(receipt) = &outcome else {
        panic!("committed");
    };
    assert_eq!(receipt.version_id.bytes(), intent.identity.version);
    assert_eq!(receipt.operation_id.bytes(), intent.identity.operation);
    // The journal holds no body digest (TCR-D2): the memory store's receipt
    // is the one source, so a later put with another body conflicts with it.
    let bad = wait(
        &mut writer
            .try_submit(Command::MemoryCreate {
                cancel: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
                turn: [105; 16],
                generation: 1,
                ordinal: 1,
                body: "changed".into(),
                source: None,
            })
            .unwrap(),
    )
    .unwrap()
    .memory_create_result
    .unwrap();
    assert_eq!(bad, Err(memory::Error::IdempotencyConflict));
    // Lose the client response and restart before committing ToolResult.
    close(&mut writer);
    let mut writer = SettledWriter(WriterHandle::start(scratch.runtime()).unwrap());
    let reopened = wait(&mut writer.try_submit(Command::Open).unwrap())
        .unwrap()
        .replay
        .unwrap();
    assert!(reopened.operations[&[105; 16]].terminal.is_none());
    assert!(reopened.project_memory_blocked(project));
    assert!(matches!(
        wait(
            &mut writer
                .try_submit(Command::MemoryCreate {
                    cancel: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
                    turn: [105; 16],
                    generation: 1,
                    ordinal: 1,
                    body: "durable note".into(),
                    source: None,
                })
                .unwrap()
        ),
        Err(Error::Conflict)
    ));
    let resolved = wait(
        &mut writer
            .try_submit(Command::MemoryResolveCreate {
                turn: [105; 16],
                generation: 1,
                ordinal: 1,
            })
            .unwrap(),
    )
    .unwrap()
    .memory_create_result
    .unwrap()
    .unwrap();
    assert_eq!(resolved, outcome);
    let resolved_state = wait(
        &mut writer
            .try_submit(Command::Append {
                expected_revision: reopened.revision,
                record: Record::ToolResult(ToolResult {
                    operation: [105; 16],
                    generation: 1,
                    ordinal: 1,
                    status: 1,
                    // TCR-D1: kind 15 of create_note stores no text.
                    text: String::new(),
                    truncated: false,
                    next_offset: None,
                }),
                admission: None,
            })
            .unwrap(),
    )
    .unwrap()
    .replay
    .unwrap();
    assert!(!resolved_state.project_memory_blocked(project));
    let notes = wait(
        &mut writer
            .try_submit(Command::MemoryRead {
                project,
                query: memory::ReadNotes::List {
                    after: None,
                    limit: 16,
                },
            })
            .unwrap(),
    )
    .unwrap()
    .memory_result
    .unwrap()
    .unwrap();
    match notes {
        memory::ReadResult::Page(page) => assert_eq!(page.notes.len(), 1),
        _ => panic!("expected page"),
    }
    close(&mut writer);
    // The journal keeps no body digest and no derived identity (TCR-I03,
    // TCR-O5); the memory store is their one source.
    let journal = fs::read(scratch.0.join(".asura/state/control/slot-0.log")).unwrap();
    {
        use sha2::{Digest, Sha256};
        let body = Sha256::digest(b"durable note");
        for value in [
            &body[..],
            &intent.identity.operation[..],
            &intent.identity.object[..],
            &intent.identity.version[..],
            &intent.identity.edge[..],
        ] {
            assert!(!journal.windows(value.len()).any(|w| w == value));
        }
    }
}

/// SI-I1 and SI-I2: overlapping roots are refused before any append.
#[cfg(feature = "embedded-memory")]
#[test]
fn registration_rejects_projects_overlapping_asura_state() {
    let _serial = TEST_WRITER.lock().unwrap_or_else(|p| p.into_inner());
    let scratch = Scratch::new();
    let mut writer = WriterHandle::start(scratch.runtime()).unwrap();
    wait(
        &mut writer
            .try_submit(Command::Initialize { request: [150; 16] })
            .unwrap(),
    )
    .unwrap();
    fs::create_dir_all(scratch.0.join(".asura/tmp")).unwrap();
    let sequence = || {
        wait(&mut writer.try_submit(Command::Open).unwrap())
            .unwrap()
            .replay
            .unwrap()
            .sequence
    };
    let before = sequence();
    let overlapping = [
        scratch.0.clone(),
        scratch.0.join(".asura"),
        scratch.0.join(".asura/tmp"),
        scratch.0.parent().unwrap().to_owned(),
        "/".into(),
        "/System".into(),
        "/System/Volumes".into(),
        "/System/Volumes/Data".into(),
    ];
    for (n, location) in overlapping.iter().enumerate() {
        assert!(
            matches!(
                wait(
                    &mut writer
                        .try_submit(Command::Register {
                            request: [160 + n as u8; 16],
                            location: location.to_str().unwrap().into(),
                        })
                        .unwrap()
                ),
                Err(Error::OverlapsAsuraState)
            ),
            "{}",
            location.display()
        );
    }
    assert_eq!(sequence(), before, "nothing appended");
    let sibling = Scratch(format!("{}-sibling", scratch.0.display()).into());
    fs::create_dir(&sibling.0).unwrap();
    for (n, location) in [sibling.0.to_str().unwrap().to_owned(), scratch.project()]
        .into_iter()
        .enumerate()
    {
        wait(
            &mut writer
                .try_submit(Command::Register {
                    request: [180 + n as u8; 16],
                    location,
                })
                .unwrap(),
        )
        .unwrap();
    }
    close(&mut writer);
}

/// SI-I3: a registration that overlaps the home is kept, listed unavailable,
/// and refused for every use.
#[cfg(feature = "embedded-memory")]
#[test]
fn existing_overlapping_registration_is_listed_unavailable() {
    use std::os::unix::fs::PermissionsExt;
    let _serial = TEST_WRITER.lock().unwrap_or_else(|p| p.into_inner());
    let scratch = Scratch::new();
    let location = scratch.project();
    let register = |location: &str| Command::Register {
        request: [190; 16],
        location: location.into(),
    };
    let mut writer = WriterHandle::start(scratch.runtime()).unwrap();
    wait(
        &mut writer
            .try_submit(Command::Initialize { request: [189; 16] })
            .unwrap(),
    )
    .unwrap();
    let project = *wait(&mut writer.try_submit(register(&location)).unwrap())
        .unwrap()
        .replay
        .unwrap()
        .projects
        .keys()
        .next()
        .unwrap();
    close(&mut writer);
    drop(writer);
    // The registered directory becomes the Asura home's parent.
    let home = scratch.0.join("project");
    fs::set_permissions(&home, fs::Permissions::from_mode(0o700)).unwrap();
    fs::rename(scratch.0.join(".asura"), home.join(".asura")).unwrap();
    let runtime = RuntimeDirectory::scratch(&home, false).unwrap();
    let mut writer = WriterHandle::start(runtime).unwrap();
    let opened = wait(&mut writer.try_submit(Command::Open).unwrap()).unwrap();
    let sequence = opened.replay.as_ref().unwrap().sequence;
    assert!(
        opened
            .replay
            .as_ref()
            .unwrap()
            .projects
            .contains_key(&project)
    );
    drop(opened);
    let listed = wait(
        &mut writer
            .try_submit(Command::Projects {
                after: None,
                limit: 8,
            })
            .unwrap(),
    )
    .unwrap();
    assert_eq!(listed.projects.len(), 1);
    assert_eq!(listed.projects[0].0.project, project);
    assert_eq!(listed.projects[0].1, Some(Error::OverlapsAsuraState));
    drop(listed);
    assert!(matches!(
        wait(&mut writer.try_submit(register(&location)).unwrap()),
        Err(Error::OverlapsAsuraState)
    ));
    assert!(matches!(
        wait(
            &mut writer
                .try_submit(Command::QueueInput {
                    request: [191; 16],
                    project,
                    conversation: None,
                    expected_generation: 0,
                    new_conversation: true,
                    prompt: "refused".into(),
                })
                .unwrap()
        ),
        Err(Error::OverlapsAsuraState)
    ));
    let after = wait(&mut writer.try_submit(Command::Open).unwrap())
        .unwrap()
        .replay
        .unwrap();
    assert_eq!(after.sequence, sequence, "records kept, nothing appended");
    assert!(after.projects.contains_key(&project));
    drop(after);
    close(&mut writer);
}

fn open_pending(scratch: &Scratch) -> (WriterHandle, std::sync::Arc<Replay>) {
    let mut file = JournalFile::open(scratch.runtime(), true).unwrap();
    file.append(0, &pending()).unwrap();
    drop(file);
    let writer = WriterHandle::start(scratch.runtime()).unwrap();
    let state = wait(&mut writer.try_submit(Command::Open).unwrap())
        .unwrap()
        .replay
        .unwrap();
    (writer, state)
}
fn owner_generation(revision: u64) -> Command {
    Command::Append {
        expected_revision: revision,
        record: Record::OwnerGeneration,
        admission: None,
    }
}
fn journal_replay(scratch: &Scratch) -> Replay {
    let bytes = fs::read(scratch.0.join(".asura/state/control/slot-0.log")).unwrap();
    replay(&bytes, &AtomicBool::new(false)).unwrap()
}
#[test]
fn j1_0_01_gated_real_append_holds_five_seconds_then_commits() {
    let _guard = TEST_WRITER.lock().unwrap_or_else(|p| p.into_inner());
    let scratch = Scratch::new();
    let gate = asura_platform::journal_gate::JournalGate::new(&scratch.0);
    let mut file = JournalFile::open(scratch.runtime(), true).unwrap();
    file.append(0, &pending()).unwrap();
    drop(file);
    let mut writer = WriterHandle::start(scratch.runtime()).unwrap();
    gate.hold_appends();
    // Open replays, then appends OwnerGeneration through the held gate.
    let mut ticket = writer.try_submit(Command::Open).unwrap();
    let held = Instant::now() + Duration::from_secs(5);
    while Instant::now() < held {
        assert!(ticket.poll().is_none(), "a slow append must not settle");
        std::thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(gate.waiting(), 1);
    gate.release_appends();
    let state = wait(&mut ticket).unwrap().replay.unwrap();
    assert_eq!(state.sequence, 2);
    close(&mut writer);
    let durable = journal_replay(&scratch);
    assert_eq!(durable.sequence, 2);
    assert_eq!(durable.owner_generation, state.owner_generation);
}
#[test]
fn j1_0_03_queued_appends_commit_in_order_after_a_long_stall() {
    let _guard = TEST_WRITER.lock().unwrap_or_else(|p| p.into_inner());
    let scratch = Scratch::new();
    let gate = asura_platform::journal_gate::JournalGate::new(&scratch.0);
    let (mut writer, state) = open_pending(&scratch);
    let base = state.revision;
    drop(state);
    gate.hold_appends();
    let mut tickets: Vec<_> = (0..3)
        .map(|n| writer.try_submit(owner_generation(base + n)).unwrap())
        .collect();
    // Longer than every removed default deadline (2 s and 5 s).
    std::thread::sleep(Duration::from_millis(5200));
    assert!(tickets.iter_mut().all(|ticket| ticket.poll().is_none()));
    gate.release_appends();
    for (n, ticket) in tickets.iter_mut().enumerate() {
        let state = wait(ticket).unwrap().replay.unwrap();
        assert_eq!(state.revision, base + n as u64 + 1);
    }
    close(&mut writer);
    assert_eq!(journal_replay(&scratch).revision, base + 3);
}
#[test]
fn j1_0_06_gated_append_requests_beyond_eight_slots_are_busy() {
    let _guard = TEST_WRITER.lock().unwrap_or_else(|p| p.into_inner());
    let scratch = Scratch::new();
    let gate = asura_platform::journal_gate::JournalGate::new(&scratch.0);
    let (mut writer, state) = open_pending(&scratch);
    let base = state.revision;
    drop(state);
    gate.hold_appends();
    let mut tickets: Vec<_> = (0..8)
        .map(|n| writer.try_submit(owner_generation(base + n)).unwrap())
        .collect();
    while gate.waiting() == 0 {
        std::thread::sleep(Duration::from_millis(1));
    }
    let started = Instant::now();
    assert!(matches!(
        writer.try_submit(owner_generation(base + 8)),
        Err(Error::Busy)
    ));
    assert!(matches!(
        writer.try_submit(Command::Projects {
            after: None,
            limit: 1
        }),
        Err(Error::Busy)
    ));
    assert!(started.elapsed() < Duration::from_millis(100));
    gate.release_appends();
    for ticket in &mut tickets {
        wait(ticket).unwrap();
    }
    close(&mut writer);
    assert_eq!(journal_replay(&scratch).revision, base + 8);
}
#[test]
fn j1_0_08_injected_real_file_errors_fence_and_later_appends_need_repair() {
    let _guard = TEST_WRITER.lock().unwrap_or_else(|p| p.into_inner());
    for fault in [
        asura_platform::journal_gate::Fault::Write,
        asura_platform::journal_gate::Fault::Sync,
    ] {
        let scratch = Scratch::new();
        let gate = asura_platform::journal_gate::JournalGate::new(&scratch.0);
        let (mut writer, state) = open_pending(&scratch);
        let base = state.revision;
        drop(state);
        gate.inject(fault);
        assert!(matches!(
            wait(&mut writer.try_submit(owner_generation(base)).unwrap()),
            Err(Error::OutcomeUnconfirmed)
        ));
        assert!(matches!(
            wait(&mut writer.try_submit(owner_generation(base)).unwrap()),
            Err(Error::RepairRequired)
        ));
        close(&mut writer);
    }
}
#[test]
fn j1_0_04_gated_replay_holds_until_shutdown_cancels_it() {
    let _guard = TEST_WRITER.lock().unwrap_or_else(|p| p.into_inner());
    let scratch = Scratch::new();
    let gate = asura_platform::journal_gate::JournalGate::new(&scratch.0);
    let mut file = JournalFile::open(scratch.runtime(), true).unwrap();
    file.append(0, &pending()).unwrap();
    drop(file);
    let mut writer = WriterHandle::start(scratch.runtime()).unwrap();
    gate.hold_reads();
    let mut ticket = writer.try_submit(Command::Open).unwrap();
    while gate.waiting() == 0 {
        std::thread::sleep(Duration::from_millis(1));
    }
    std::thread::sleep(Duration::from_millis(5200));
    assert!(ticket.poll().is_none(), "replay has no time bound");
    let started = Instant::now();
    writer.close();
    assert!(matches!(wait(&mut ticket), Err(Error::Cancelled)));
    assert!(started.elapsed() < Duration::from_millis(100));
    close(&mut writer);
    // The cancelled replay changed nothing on disk.
    assert_eq!(journal_replay(&scratch).sequence, 1);
}

// J1T-01 and J1T-02 integration: kinds 22, 23 and 15 through the real writer.
fn submit(writer: &mut WriterHandle, revision: &mut u64, record: Record) -> Result<()> {
    let admission = None;
    let state = wait(
        &mut writer
            .try_submit(Command::Append {
                expected_revision: *revision,
                record,
                admission,
            })
            .unwrap(),
    )?
    .replay
    .unwrap();
    *revision = state.revision;
    Ok(())
}
/// Initialize and register a project; returns the configuration digest.
fn j1_initialize(scratch: &Scratch, writer: &mut WriterHandle) -> (Id, Hash) {
    wait(&mut writer.try_submit(Command::Open).unwrap()).unwrap();
    wait(
        &mut writer
            .try_submit(Command::Initialize { request: [30; 16] })
            .unwrap(),
    )
    .unwrap();
    let registered = wait(
        &mut writer
            .try_submit(Command::Register {
                request: [31; 16],
                location: scratch.project(),
            })
            .unwrap(),
    )
    .unwrap()
    .replay
    .unwrap();
    let project = *registered.projects.keys().next().unwrap();
    drop(registered);
    let prepared = wait(
        &mut writer
            .try_submit(Command::Prepare {
                project,
                conversation: None,
                expected_generation: 0,
                prompt: "hello".into(),
            })
            .unwrap(),
    )
    .unwrap()
    .prepared
    .unwrap();
    (project, prepared.configuration_digest)
}
fn j1_turn(project: Id, configuration_digest: Hash) -> TurnAccepted {
    TurnAccepted {
        request: [32; 16],
        digest: request_digest(Request::Submit {
            project,
            conversation: None,
            expected_generation: 0,
            prompt: "hello",
        })
        .unwrap(),
        project,
        original_conversation: None,
        expected_generation: 0,
        conversation: [33; 16],
        generation: 1,
        task: [34; 16],
        operation: [35; 16],
        model: "system".into(),
        configuration_digest,
        instructions_digest: [36; 32],
        input_digest: [37; 32],
        prior_operations: Vec::new(),
        prompt: "hello".into(),
        event_cursor: 1,
        composition: Composition::default(),
    }
}
fn j1_start() -> Record {
    Record::StartAuthorized(StartAuthorized {
        operation: [35; 16],
        generation: 1,
        helper: [38; 16],
        input_digest: [37; 32],
    })
}
/// Admit and start the turn; returns the revision after the start.
fn j1_admit_and_start(
    writer: &mut WriterHandle,
    project: Id,
    configuration_digest: Hash,
) -> std::sync::Arc<Replay> {
    let accepted = wait(
        &mut writer
            .try_submit(Command::Append {
                expected_revision: wait(&mut writer.try_submit(Command::Open).unwrap())
                    .unwrap()
                    .replay
                    .unwrap()
                    .revision,
                record: Record::TurnAccepted(Box::new(j1_turn(project, configuration_digest))),
                admission: Some(AdmissionFence {
                    project,
                    configuration_digest,
                }),
            })
            .unwrap(),
    )
    .unwrap()
    .replay
    .unwrap();
    let mut revision = accepted.revision;
    drop(accepted);
    submit(writer, &mut revision, j1_start()).unwrap();
    wait(&mut writer.try_submit(Command::Open).unwrap())
        .unwrap()
        .replay
        .unwrap()
}
/// A kind 14 exec_cmd call at `ordinal`: `run`, or a refused call that holds
/// the display-redacted command (cut at 8,192 bytes).
fn j1_call(ordinal: u32, decision: ToolDecision) -> ToolCall {
    let run = decision == ToolDecision::Run;
    let arguments = if run {
        "{\"command\":\"printf %s \\\"$ASURA_SECRET_1\\\"\",\"cwd\":\".\"}"
    } else {
        "{\"command\":\"printf %s '[REDACTED:github-token]'\",\"cwd\":\".\"}"
    };
    ToolCall {
        operation: [35; 16],
        generation: 1,
        ordinal,
        source: ToolCallSource::GeneratedContent,
        name: "exec_cmd".into(),
        name_bytes: 8,
        tool_operation: Some(ToolOperation::ExecCmd),
        decision,
        fault: None,
        busy: None,
        form: ArgumentForm::Canonical,
        arguments_bytes: arguments.len() as u32,
        arguments: arguments.into(),
        number_kinds: if run {
            vec![SecretKind::GithubToken]
        } else {
            Vec::new()
        },
        unnumbered: 0,
        rule_set_revision: 1,
        rule_set_digest: [39; 32],
        command_cut: !run,
        refusal: (!run).then_some(ExecCmdRefusal {
            code: RefusalCode::SecretSingleQuoted,
            kind: Some(SecretKind::GithubToken),
            word: Some(3),
        }),
    }
}
/// A `run` service status call, or the same call with another decision; both
/// encode to the same size.
fn j1_status(decision: ToolDecision) -> Record {
    let arguments = "{\"command\":\"status\"}";
    Record::ToolCall(Box::new(ToolCall {
        name: "service".into(),
        name_bytes: 7,
        tool_operation: Some(ToolOperation::Status),
        decision,
        form: ArgumentForm::Verbatim,
        arguments_bytes: arguments.len() as u32,
        arguments: arguments.into(),
        number_kinds: Vec::new(),
        command_cut: false,
        refusal: None,
        ..j1_call(1, ToolDecision::Run)
    }))
}
#[test]
fn j1t_01_real_writer_appends_kinds_14_23_15_and_reopens() {
    use asura_storage::payload::{ObjectKind, payload_id};
    let _guard = TEST_WRITER.lock().unwrap_or_else(|p| p.into_inner());
    let scratch = Scratch::new();
    let mut writer = WriterHandle::start(scratch.runtime()).unwrap();
    let (project, configuration_digest) = j1_initialize(&scratch, &mut writer);
    let state = j1_admit_and_start(&mut writer, project, configuration_digest);
    let installation = state.installation_id;
    let mut revision = state.revision;
    drop(state);
    let call = j1_call(1, ToolDecision::Run);
    let settled = CaptureSettled {
        operation: [35; 16],
        generation: 1,
        ordinal: 1,
        state: CaptureState::Incomplete,
        reason: CaptureReason::Quota,
        exit_kind: 2,
        exit_value: 15,
        stdout: CaptureStream {
            observed_bytes: 40,
            stored_bytes: 33,
            digest: Some([40; 32]),
            replacements: 1,
            literal_replacements: 1,
            kinds: vec![(SecretKind::GithubToken, 1)],
            ..CaptureStream::default()
        },
        stderr: CaptureStream {
            digest: Some([41; 32]),
            eof: true,
            ..CaptureStream::default()
        },
        distinct_numbers: 1,
        numbers: vec![CaptureNumber {
            number: 1,
            kind: SecretKind::GithubToken,
            origin: NumberOrigin::Command,
        }],
        pause_count: 1,
        first_pause_ms: 10,
        paused_ms: 20,
        final_reservation: 2 * INITIAL_RESERVATION,
    };
    let result = ToolResult {
        operation: [35; 16],
        generation: 1,
        ordinal: 1,
        status: 5,
        text: "failure=None exit_code=none signal=15 reason=none truncated=false cleanup_confirmed=true capture=incomplete\nstdout:\n$ASURA_SECRET_1\nstderr:\n".into(),
        next_offset: None,
        truncated: false,
    };
    // The refused call is complete at its kind 14 (TCR-R5).
    let refused = j1_call(2, ToolDecision::Refused);
    for record in [
        Record::ToolCall(Box::new(call.clone())),
        Record::CaptureSettled(Box::new(settled.clone())),
        Record::ToolResult(result),
        Record::ToolCall(Box::new(refused.clone())),
    ] {
        submit(&mut writer, &mut revision, record).unwrap();
    }
    close(&mut writer);
    // The journal on disk replays with both calls and the derived identities.
    let durable = journal_replay(&scratch);
    let tools = &durable.operations[&[35; 16]].tools;
    assert_eq!(tools.len(), 2);
    let derived = tools[0].exec_cmd.as_deref().unwrap();
    assert_eq!(derived.job, exec_cmd_job_id([35; 16], 1, 1));
    assert_eq!(
        derived.stdout_payload,
        payload_id(installation, [35; 16], 1, 1, ObjectKind::Stdout)
    );
    assert_eq!(
        derived.stderr_payload,
        payload_id(installation, [35; 16], 1, 1, ObjectKind::Stderr)
    );
    let settlement = derived.settled.as_ref().unwrap();
    assert_eq!(settlement.state, CaptureState::Incomplete);
    assert_eq!(settlement.reason, CaptureReason::Quota);
    assert_eq!(
        settlement.streams,
        [(33, Some([40; 32])), (0, Some([41; 32]))]
    );
    assert_eq!(tools[0].result_status, Some(5));
    assert_eq!(tools[1].decision, ToolDecision::Refused);
    assert!(tools[1].exec_cmd.is_none() && tools[1].result_frame.is_none());
    let intent_frame = tools[0].intent_frame.clone();
    let settled_frame = settlement.frame.clone();
    let refused_frame = tools[1].intent_frame.clone();
    // A restarted writer reopens the journal and reads each record back.
    let mut writer = WriterHandle::start(scratch.runtime()).unwrap();
    let reopened = wait(&mut writer.try_submit(Command::Open).unwrap())
        .unwrap()
        .replay
        .unwrap();
    assert_eq!(reopened.operations[&[35; 16]].tools, *tools);
    drop(reopened);
    close(&mut writer);
    // Each verified frame range decodes to the appended record.
    let bytes = fs::read(scratch.0.join(".asura/state/control/slot-0.log")).unwrap();
    for (range, kind, expected) in [
        (intent_frame, 14, Record::ToolCall(Box::new(call))),
        (settled_frame, 23, Record::CaptureSettled(Box::new(settled))),
        (refused_frame, 14, Record::ToolCall(Box::new(refused))),
    ] {
        let frame = &bytes[range];
        assert_eq!(u16::from_be_bytes([frame[10], frame[11]]), kind);
        assert_eq!(
            decode_record(kind, &frame[144..frame.len() - 40]).unwrap(),
            expected
        );
    }
}
const FRAME: usize = 144 + 40;
const TERMINAL: usize = FRAME + 43 + MAX_TEXT;
const RESULT: usize = FRAME + 43 + 16_384;
const OWNER: usize = FRAME;
const CANCEL: usize = FRAME + 25;
const CAPTURE: usize = FRAME + 4_066;
/// Frame sizes do not depend on the frame context.
fn frame_size(record: &Record) -> usize {
    encode_frame(
        &FrameContext {
            installation_id: [1; 16],
            transition_id: [2; 16],
            sequence: 2,
            prior_digest: [0; 32],
            expected_revision: 1,
            owner_generation: 1,
        },
        record,
    )
    .unwrap()
    .len()
}
/// A started turn on a journal padded so that exactly `free` bytes remain
/// beyond the started turn's own reservation (terminal, owner and cancel).
/// Returns the writer, the installation and the revision.
fn j1_padded(scratch: &Scratch, free: usize) -> (WriterHandle, Id, u64) {
    let size = frame_size;
    let mut writer = WriterHandle::start(scratch.runtime()).unwrap();
    let (project, configuration_digest) = j1_initialize(scratch, &mut writer);
    close(&mut writer);
    let started_reserved = TERMINAL + OWNER + CANCEL;
    let started_end = asura_storage::authority::MAX_JOURNAL - started_reserved - free;
    let padded_end = started_end
        - OWNER
        - size(&Record::TurnAccepted(Box::new(j1_turn(
            project,
            configuration_digest,
        ))))
        - size(&j1_start());
    // Pad with completed turns written directly to the closed journal.
    let state = journal_replay(scratch);
    let mut file = JournalFile::open(scratch.runtime(), false).unwrap();
    let (mut sequence, mut digest, mut end) = (state.sequence, state.last_digest, state.end_offset);
    let owner_generation = state.owner_generation;
    let mut push = |record: Record, file: &mut JournalFile, end: &mut usize| {
        let frame = encode_frame(
            &FrameContext {
                installation_id: state.installation_id,
                transition_id: random_id(),
                sequence: sequence + 1,
                prior_digest: digest,
                expected_revision: sequence,
                owner_generation,
            },
            &record,
        )
        .unwrap();
        file.append(*end, &frame).unwrap();
        digest.copy_from_slice(&frame[frame.len() - 40..frame.len() - 8]);
        sequence += 1;
        *end += frame.len();
    };
    let pad = |n: u64| {
        let mut id = [0xa5; 16];
        id[8..].copy_from_slice(&n.to_be_bytes());
        id
    };
    let conversation = pad(0);
    let turn = |generation: u64| {
        let original = (generation > 1).then_some(conversation);
        (
            Record::TurnAccepted(Box::new(TurnAccepted {
                request: pad(generation * 4 + 1),
                digest: request_digest(Request::Submit {
                    project,
                    conversation: original,
                    expected_generation: generation - 1,
                    prompt: "pad",
                })
                .unwrap(),
                project,
                original_conversation: original,
                expected_generation: generation - 1,
                conversation,
                generation,
                task: pad(generation * 4 + 2),
                operation: pad(generation * 4 + 3),
                model: "system".into(),
                configuration_digest,
                instructions_digest: [1; 32],
                input_digest: [2; 32],
                prior_operations: Vec::new(),
                prompt: "pad".into(),
                event_cursor: 1,
                // Every earlier padding turn completed and is omitted.
                composition: Composition {
                    omitted_turns: (generation - 1) as u32,
                    ..Composition::default()
                },
            })),
            Record::StartAuthorized(StartAuthorized {
                operation: pad(generation * 4 + 3),
                generation,
                helper: pad(generation * 4 + 4),
                input_digest: [2; 32],
            }),
            move |text: usize| {
                Record::TurnTerminal(TurnTerminal {
                    operation: pad(generation * 4 + 3),
                    generation,
                    kind: TerminalKind::Complete,
                    cause: Cause::None,
                    final_cursor: u64::MAX,
                    usage_known: true,
                    output_tokens: 0,
                    text: "x".repeat(text),
                })
            },
        )
    };
    // The fixed part of a later turn: accepted, start and an empty terminal.
    let base = {
        let (accepted, start, _) = turn(2);
        size(&accepted) + size(&start) + FRAME + 43
    };
    let mut generation = 1;
    while end < padded_end {
        let (accepted, start, terminal) = turn(generation);
        let fixed = size(&accepted) + size(&start) + FRAME + 43;
        let remaining = padded_end - end;
        let text = if generation == 1 || remaining > fixed + MAX_TEXT + base {
            MAX_TEXT
        } else if remaining - fixed <= MAX_TEXT {
            remaining - fixed
        } else {
            // Leave exactly one later turn with a 1-byte text.
            remaining - fixed - base - 1
        };
        assert!((1..=MAX_TEXT).contains(&text));
        push(accepted, &mut file, &mut end);
        push(start, &mut file, &mut end);
        push(terminal(text), &mut file, &mut end);
        generation += 1;
    }
    assert_eq!(end, padded_end);
    drop(file);
    // The restarted writer appends its owner generation, then the turn.
    let mut writer = WriterHandle::start(scratch.runtime()).unwrap();
    let started = j1_admit_and_start(&mut writer, project, configuration_digest);
    assert_eq!(started.end_offset, started_end);
    assert_eq!(started.reserved_bytes, started_reserved);
    let installation = started.installation_id;
    let revision = started.revision;
    drop(started);
    (writer, installation, revision)
}
#[test]
fn j1t_02_real_writer_refuses_a_run_exec_cmd_when_the_capture_reservation_cannot_fit() {
    const MARGIN: usize = 100;
    let _guard = TEST_WRITER.lock().unwrap_or_else(|p| p.into_inner());
    let scratch = Scratch::new();
    let exec_cmd = || Record::ToolCall(Box::new(j1_call(1, ToolDecision::Run)));
    let status = j1_status(ToolDecision::Run);
    // The run exec_cmd kind 14 and its kind 15 fit by MARGIN bytes, but not
    // with its capture frame; the smaller run status call fits.
    assert!(frame_size(&status) <= frame_size(&exec_cmd()) + MARGIN && MARGIN < CAPTURE);
    let free = frame_size(&exec_cmd()) + RESULT + MARGIN;
    let (mut writer, _, mut revision) = j1_padded(&scratch, free);
    let journal = scratch.0.join(".asura/state/control/slot-0.log");
    let before = fs::read(&journal).unwrap();
    assert!(matches!(
        submit(&mut writer, &mut revision, exec_cmd()),
        Err(Error::Invalid)
    ));
    assert_eq!(fs::read(&journal).unwrap(), before);
    submit(&mut writer, &mut revision, status.clone()).unwrap();
    let state = wait(&mut writer.try_submit(Command::Open).unwrap())
        .unwrap()
        .replay
        .unwrap();
    assert_eq!(state.reserved_bytes, TERMINAL + RESULT + OWNER + CANCEL);
    assert_eq!(
        asura_storage::authority::MAX_JOURNAL - state.end_offset - state.reserved_bytes,
        frame_size(&exec_cmd()) + MARGIN - frame_size(&status)
    );
    drop(state);
    close(&mut writer);
}
/// TCR-I08 and TCR-D16: kind 14 fits but kind 15 and the terminal do not.
/// The `run` record is refused with the writer's existing capacity error and
/// changes nothing; the same call with a decision that does not run (same
/// size) needs no result frame and is accepted, and the turn can still end.
#[test]
fn tcr_i08_run_kind_14_is_refused_when_its_result_cannot_fit() {
    const MARGIN: usize = 100;
    let _guard = TEST_WRITER.lock().unwrap_or_else(|p| p.into_inner());
    let scratch = Scratch::new();
    let run = j1_status(ToolDecision::Run);
    let denied = j1_status(ToolDecision::Denied);
    assert_eq!(frame_size(&run), frame_size(&denied));
    const { assert!(MARGIN < RESULT) };
    let free = frame_size(&run) + RESULT - MARGIN;
    let (mut writer, _, mut revision) = j1_padded(&scratch, free);
    let journal = scratch.0.join(".asura/state/control/slot-0.log");
    let before = fs::read(&journal).unwrap();
    assert!(matches!(
        submit(&mut writer, &mut revision, run),
        Err(Error::Invalid)
    ));
    assert_eq!(fs::read(&journal).unwrap(), before);
    submit(&mut writer, &mut revision, denied).unwrap();
    let state = wait(&mut writer.try_submit(Command::Open).unwrap())
        .unwrap()
        .replay
        .unwrap();
    let tool = &state.operations[&[35; 16]].tools[0];
    assert_eq!(tool.decision, ToolDecision::Denied);
    assert_eq!(state.reserved_bytes, TERMINAL + OWNER + CANCEL);
    drop(state);
    submit(
        &mut writer,
        &mut revision,
        Record::TurnTerminal(TurnTerminal {
            operation: [35; 16],
            generation: 1,
            kind: TerminalKind::Failed,
            cause: Cause::ProviderFailure,
            final_cursor: u64::MAX,
            usage_known: false,
            output_tokens: 0,
            text: "x".repeat(MAX_TEXT),
        }),
    )
    .unwrap();
    close(&mut writer);
}
