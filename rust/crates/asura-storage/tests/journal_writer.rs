use asura_platform::{JournalFile, ProjectIdentity, RuntimeDirectory, random_id};
use asura_storage::authority::{conversation::*, writer::*};
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
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn deadline() -> Instant {
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
    let end = deadline();
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
    let end = deadline();
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
                location: scratch.0.to_str().unwrap().into(),
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
            .map(|q| q.input.request)
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
    file.append(0, &bytes, deadline()).unwrap();
    assert!(JournalFile::open(runtime.clone(), false).is_err());
    assert_eq!(file.read_all(deadline()).unwrap(), bytes);
    assert_eq!(file.read_all(deadline()).unwrap(), bytes);
    drop(file);
    let mut file = JournalFile::open(runtime, false).unwrap();
    assert_eq!(file.read_all(deadline()).unwrap(), bytes);
    let path = scratch.0.join(".asura/state/control/slot-0.log");
    fs::rename(&path, path.with_extension("saved")).unwrap();
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .unwrap();
    assert!(file.append(bytes.len(), &[1], deadline()).is_err());
    assert_eq!(fs::read(path.with_extension("saved")).unwrap(), bytes);
}
#[test]
fn project_identity_detects_same_path_replacement_and_symlink() {
    let scratch = Scratch::new();
    let path = scratch.0.join("project");
    fs::create_dir(&path).unwrap();
    let held = ProjectIdentity::open(path.to_str().unwrap()).unwrap();
    held.validate().unwrap();
    fs::rename(&path, scratch.0.join("old")).unwrap();
    fs::create_dir(&path).unwrap();
    assert!(held.validate().is_err());
    let link = scratch.0.join("link");
    std::os::unix::fs::symlink(&path, &link).unwrap();
    assert!(ProjectIdentity::open(link.to_str().unwrap()).is_err());
}
#[test]
fn writer_preserves_incomplete_tail_and_does_not_recreate_missing_authority() {
    let _guard = TEST_WRITER.lock().unwrap_or_else(|p| p.into_inner());
    let scratch = Scratch::new();
    let runtime = scratch.runtime();
    let mut file = JournalFile::open(runtime.clone(), true).unwrap();
    let mut bytes = pending();
    bytes.extend_from_slice(b"ASU");
    file.append(0, &bytes, deadline()).unwrap();
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
    file.append(0, &pending(), deadline()).unwrap();
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
    for reservation in [LEGACY_OUTPUT_RESERVATION, OUTPUT_RESERVATION] {
        initialization_registration_admission_and_restart_with_budget(reservation);
    }
}
#[cfg(feature = "embedded-memory")]
fn initialization_registration_admission_and_restart_with_budget(reservation: u32) {
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
    let location = scratch.0.to_str().unwrap().to_owned();
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
        reserved_output_tokens: reservation,
        event_cursor: 1,
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
    let queue_command = || Command::Enqueue {
        request: [60; 16],
        project,
        conversation: [14; 16],
        target_operation: [16; 16],
        target_generation: 1,
        kind: InputKind::Queue,
        prompt: "durable queued text".into(),
    };
    let queued = wait(&mut writer.try_submit(queue_command()).unwrap()).unwrap();
    assert_eq!(queued.inputs[0].status, InputStatus::Queued);
    let queued_revision = queued.replay.as_ref().unwrap().revision;
    drop(queued);
    let repeated = wait(&mut writer.try_submit(queue_command()).unwrap()).unwrap();
    assert_eq!(repeated.replay.unwrap().revision, queued_revision);
    let before_queue_conflict = fs::read(&journal).unwrap();
    assert!(matches!(
        wait(
            &mut writer
                .try_submit(Command::Enqueue {
                    request: [60; 16],
                    project,
                    conversation: [14; 16],
                    target_operation: [16; 16],
                    target_generation: 1,
                    kind: InputKind::Queue,
                    prompt: "changed text".into(),
                })
                .unwrap()
        ),
        Err(Error::Conflict)
    ));
    assert_eq!(fs::read(&journal).unwrap(), before_queue_conflict);
    let promote = |generation| Command::PromoteInput {
        request: [65; 16],
        input: [60; 16],
        target_operation: [16; 16],
        target_generation: generation,
    };
    assert!(matches!(
        wait(&mut writer.try_submit(promote(2)).unwrap()),
        Err(Error::Conflict)
    ));
    assert_eq!(fs::read(&journal).unwrap(), before_queue_conflict);
    let promoted = wait(&mut writer.try_submit(promote(1)).unwrap()).unwrap();
    assert_eq!(promoted.inputs[0].input.request, [60; 16]);
    assert_eq!(promoted.inputs[0].input.kind, InputKind::Steer);
    assert_eq!(promoted.replay.as_ref().unwrap().inputs.len(), 1);
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
    assert_eq!(terminal.charged_tokens, 0);
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
    assert_eq!(recovered.input_status([60; 16]), Some(InputStatus::Held));
    assert_eq!(recovered.ready_input(), None);
    drop(recovered);
    let inspected = wait(
        &mut writer
            .try_submit(Command::Inputs {
                project,
                input: Some([60; 16]),
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
                input: [60; 16],
                action: InputAction::Resume,
            })
            .unwrap(),
    )
    .unwrap();
    assert_eq!(resumed.inputs[0].status, InputStatus::Queued);
    drop(resumed);
    let queued_preparation = wait(
        &mut writer
            .try_submit(Command::PrepareInput { input: [60; 16] })
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        queued_preparation.queued.unwrap().prompt,
        "Original request:\nhello\n\nSteering instruction:\ndurable queued text"
    );
    drop(queued_preparation.replay);
    let held = wait(
        &mut writer
            .try_submit(Command::InputDecision {
                request: [62; 16],
                input: [60; 16],
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
                input: [60; 16],
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
                input: [60; 16],
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
    assert_eq!(terminal.charged_tokens, reservation);
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
    file.append(0, &pending(), deadline()).unwrap();
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
                location: scratch.0.to_str().unwrap().into(),
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
        reserved_output_tokens: 2048,
        event_cursor: 1,
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
    state = wait(
        &mut writer
            .try_submit(Command::Append {
                expected_revision: state.revision,
                record: Record::MemoryCreateIntent(intent.clone()),
                admission: None,
            })
            .unwrap(),
    )
    .unwrap()
    .replay
    .unwrap();
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
    let bad = wait(
        &mut writer
            .try_submit(Command::MemoryCreate {
                cancel: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
                turn: [105; 16],
                generation: 1,
                ordinal: 1,
                body: "changed".into(),
            })
            .unwrap(),
    )
    .unwrap()
    .memory_create_result
    .unwrap();
    assert_eq!(bad, Err(memory::Error::IdempotencyConflict));
    let outcome = wait(
        &mut writer
            .try_submit(Command::MemoryCreate {
                cancel: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
                turn: [105; 16],
                generation: 1,
                ordinal: 1,
                body: "durable note".into(),
            })
            .unwrap(),
    )
    .unwrap()
    .memory_create_result
    .unwrap()
    .unwrap();
    assert!(matches!(outcome, MemoryCreateResult::Committed(_)));
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
                    body: "durable note".into()
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
                    text: intent.success_text(),
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
}
