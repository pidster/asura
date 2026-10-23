use super::*;
use asura_platform::RuntimeDirectory;
use std::{
    fs,
    io::Write,
    os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt, symlink},
    path::PathBuf,
    sync::atomic::AtomicBool,
};
struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Self {
        let path = PathBuf::from(format!(
            "/private/tmp/asura-audit-{:x}",
            u128::from_ne_bytes(asura_platform::random_id())
        ));
        fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
        Self(path)
    }
    fn runtime(&self) -> RuntimeDirectory {
        RuntimeDirectory::scratch(&self.0, true).unwrap()
    }
    fn logs(&self) -> PathBuf {
        self.0.join(".asura/logs")
    }
    fn active(&self) -> PathBuf {
        self.logs().join("audit.jsonl")
    }
    fn seed(&self, bytes: &[u8]) {
        let _ = self.runtime();
        fs::DirBuilder::new()
            .mode(0o700)
            .create(self.logs())
            .unwrap();
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(self.active())
            .unwrap();
        file.write_all(bytes).unwrap();
        file.sync_all().unwrap();
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn record(sequence: u64) -> Record {
    Record {
        schema: 1,
        service_build: "asura/test".into(),
        service_epoch: [1; 16],
        sequence,
        unix_time_ms: Some(12),
        event: Event::ServiceStopping {
            reason: StopReason::Stop,
        },
    }
}
fn open(s: &Scratch, enabled: bool) -> Result<Opened, Error> {
    AuditStore::open(
        s.runtime(),
        Settings {
            enabled,
            ..Settings::default()
        },
        [2; 16],
        &AtomicBool::new(false),
    )
}
#[test]
fn strict_schema_round_trip_and_rejects_untrusted_fields_duplicates_and_nulls() {
    let value = record(1);
    let bytes = value.encode().unwrap();
    assert_eq!(Record::decode(&bytes).unwrap(), value);
    let text = String::from_utf8(bytes).unwrap();
    for broken in [
        text.replace("\"schema\":1", "\"schema\":1,\"schema\":1"),
        text.replace("\"schema\":1", "\"schema\":1,\"prompt\":\"secret\""),
        text.replace(
            "\"reason\":\"stop\"",
            "\"reason\":\"stop\",\"reason\":\"stop\"",
        ),
        text.replace(
            "\"reason\":\"stop\"",
            "\"reason\":\"stop\",\"path\":\"secret\"",
        ),
        text.replace("\"unix_time_ms\":12", "\"unix_time_ms\":null"),
        text.replace("service.stopping", "unknown"),
        text.trim_end().to_owned(),
    ] {
        assert_eq!(
            Record::decode(broken.as_bytes()),
            Err(Error::MalformedRecord),
            "{broken}"
        );
    }
}
#[test]
fn absent_disabled_creates_nothing_and_config_source_is_truthful() {
    let s = Scratch::new();
    let runtime = s.runtime();
    let (settings, source) =
        crate::config::audit_settings(runtime, &AtomicBool::new(false)).unwrap();
    assert_eq!(settings, Settings::default());
    assert_eq!(source, ConfigLoad::Defaults);
    let mut opened = open(&s, false).unwrap();
    assert!(!s.logs().exists());
    assert!(opened.recent.is_empty());
    assert_eq!(
        opened
            .store
            .append_batch(&[record(1)], &AtomicBool::new(false)),
        Err(Error::Disabled)
    );
    opened.store.maintain(&AtomicBool::new(false)).unwrap();
    assert!(!s.logs().exists());
}
#[test]
fn disabled_hydrates_complete_records_without_repair_or_rotation() {
    let s = Scratch::new();
    let mut bytes = record(1).encode().unwrap();
    bytes.extend_from_slice(b"{partial");
    s.seed(&bytes);
    let opened = open(&s, false).unwrap();
    assert!(opened.lost_partial);
    assert_eq!(opened.recent, vec![record(1)]);
    assert_eq!(fs::read(s.active()).unwrap(), bytes);
    assert_eq!(fs::read_dir(s.logs()).unwrap().count(), 1);
}
#[test]
fn enabled_repairs_only_partial_suffix_seals_then_hydrates_after_restart() {
    let s = Scratch::new();
    let mut bytes = record(1).encode().unwrap();
    bytes.extend_from_slice(b"{partial");
    s.seed(&bytes);
    let mut opened = open(&s, true).unwrap();
    assert!(opened.lost_partial);
    assert_eq!(opened.recent, vec![record(1)]);
    assert!(fs::read(s.active()).unwrap().is_empty());
    let archives: Vec<_> = fs::read_dir(s.logs())
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p != &s.active())
        .collect();
    assert_eq!(archives.len(), 1);
    assert_eq!(fs::read(&archives[0]).unwrap(), record(1).encode().unwrap());
    let mut next = record(1);
    next.service_epoch = [2; 16];
    opened
        .store
        .append_batch(&[next.clone()], &AtomicBool::new(false))
        .unwrap();
    drop(opened);
    let reopened = AuditStore::open(
        s.runtime(),
        Settings::default(),
        [3; 16],
        &AtomicBool::new(false),
    )
    .unwrap();
    assert_eq!(reopened.recent, vec![next]);
}
#[test]
fn malformed_complete_line_is_preserved_and_rejected() {
    for bytes in [b"{}\npartial".to_vec(), vec![b'x'; 8195], b"{}\n".to_vec()] {
        let s = Scratch::new();
        s.seed(&bytes);
        assert!(matches!(open(&s, true), Err(Error::MalformedRecord)));
        assert_eq!(fs::read(s.active()).unwrap(), bytes);
    }
}
#[test]
fn recent_window_is_newest_first_bounded_and_rejects_duplicate_sequences() {
    let s = Scratch::new();
    let bytes: Vec<u8> = (1..=300)
        .flat_map(|n| record(n).encode().unwrap())
        .collect();
    s.seed(&bytes);
    let opened = open(&s, false).unwrap();
    assert!(opened.older_omitted);
    assert_eq!(opened.recent.len(), 256);
    assert_eq!(opened.recent.first().unwrap().sequence, 300);
    assert_eq!(opened.recent.last().unwrap().sequence, 45);
    let s = Scratch::new();
    let mut bytes = record(1).encode().unwrap();
    bytes.extend(record(1).encode().unwrap());
    s.seed(&bytes);
    assert!(matches!(open(&s, false), Err(Error::MalformedRecord)));
}
#[test]
fn rotation_retains_only_configured_archives_and_never_deletes_unrelated_files() {
    let s = Scratch::new();
    let cancel = AtomicBool::new(false);
    let max = record(1).encode().unwrap().len() as u64 + 8;
    let mut opened = AuditStore::open(
        s.runtime(),
        Settings {
            enabled: true,
            keep_files: 1,
            max_file_bytes: max,
        },
        [1; 16],
        &cancel,
    )
    .unwrap();
    fs::write(s.logs().join("other.log"), b"keep").unwrap();
    for n in 1..=5 {
        opened.store.append_batch(&[record(n)], &cancel).unwrap();
        opened.store.maintain(&cancel).unwrap();
    }
    assert_eq!(fs::read_dir(s.logs()).unwrap().count(), 3);
    assert_eq!(fs::read(s.logs().join("other.log")).unwrap(), b"keep");
    assert_eq!(
        Record::decode(&fs::read(s.active()).unwrap()).unwrap(),
        record(5)
    );
}
#[test]
fn unsafe_links_permissions_replacement_and_archive_collision_fail_closed() {
    let s = Scratch::new();
    s.seed(&record(1).encode().unwrap());
    fs::set_permissions(s.active(), fs::Permissions::from_mode(0o644)).unwrap();
    assert!(matches!(open(&s, true), Err(Error::UnsafeStorage)));
    let s = Scratch::new();
    s.seed(&record(1).encode().unwrap());
    fs::hard_link(s.active(), s.logs().join("alias")).unwrap();
    assert!(matches!(open(&s, true), Err(Error::UnsafeStorage)));
    let s = Scratch::new();
    s.seed(&record(1).encode().unwrap());
    fs::rename(s.active(), s.logs().join("target")).unwrap();
    symlink("target", s.active()).unwrap();
    assert!(open(&s, true).is_err());
    let s = Scratch::new();
    let mut opened = open(&s, true).unwrap();
    fs::rename(s.active(), s.logs().join("displaced")).unwrap();
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(s.active())
        .unwrap();
    f.write_all(b"replacement").unwrap();
    let mut rec = record(1);
    rec.service_epoch = [2; 16];
    assert!(
        opened
            .store
            .append_batch(&[rec], &AtomicBool::new(false))
            .is_err()
    );
    assert_eq!(fs::read(s.active()).unwrap(), b"replacement");
    let s = Scratch::new();
    let original = record(1).encode().unwrap();
    s.seed(&original);
    let name = format!("audit.{}.{:020}.jsonl", "02".repeat(16), 0);
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(s.logs().join(&name))
        .unwrap();
    f.write_all(b"collision").unwrap();
    assert!(open(&s, true).is_err());
    assert_eq!(fs::read(s.logs().join(name)).unwrap(), b"collision");
    assert_eq!(fs::read(s.active()).unwrap(), original);
}
#[test]
fn cancelled_expired_oversize_and_invalid_batch_do_not_append() {
    let s = Scratch::new();
    assert!(matches!(
        AuditStore::open(
            s.runtime(),
            Settings::default(),
            [1; 16],
            &AtomicBool::new(true)
        ),
        Err(Error::Cancelled)
    ));
    assert!(!s.logs().exists());
    let mut opened = AuditStore::open(
        s.runtime(),
        Settings {
            max_file_bytes: 1,
            ..Settings::default()
        },
        [1; 16],
        &AtomicBool::new(false),
    )
    .unwrap();
    assert_eq!(
        opened
            .store
            .append_batch(&[record(1)], &AtomicBool::new(false)),
        Err(Error::Limit)
    );
    assert!(fs::read(s.active()).unwrap().is_empty());
}

#[test]
fn retention_decrease_removes_at_most_32_per_cycle_after_new_active_exists() {
    let s = Scratch::new();
    s.seed(&record(1).encode().unwrap());
    for n in 1..=70 {
        let name = format!("audit.{}.{n:020}.jsonl", "01".repeat(16));
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(s.logs().join(name))
            .unwrap();
    }
    let cancel = AtomicBool::new(false);
    let mut opened = AuditStore::open(
        s.runtime(),
        Settings {
            keep_files: 1,
            ..Settings::default()
        },
        [2; 16],
        &cancel,
    )
    .unwrap();
    // Startup sealed the old active and performed exactly one bounded cycle.
    assert!(s.active().exists());
    assert_eq!(fs::read_dir(s.logs()).unwrap().count(), 40);
    opened.store.maintain(&cancel).unwrap();
    assert!(s.active().exists());
    assert_eq!(fs::read_dir(s.logs()).unwrap().count(), 8);
    opened.store.maintain(&cancel).unwrap();
    assert!(s.active().exists());
    assert_eq!(fs::read_dir(s.logs()).unwrap().count(), 2);
    let mut row = record(1);
    row.service_epoch = [2; 16];
    opened.store.append_batch(&[row], &cancel).unwrap();
}

#[test]
fn exec_cmd_tool_label_round_trips_with_value_9() {
    assert_eq!(
        serde_json::to_string(&Tool::ExecCmd).unwrap(),
        "\"exec_cmd\""
    );
    assert_eq!(
        serde_json::from_str::<Tool>("\"exec_cmd\"").unwrap(),
        Tool::ExecCmd
    );
    assert_eq!(Tool::ExecCmd as u32, 9);
}

/// J3T-20 (unit): the six `job` tool values round trip by name with their
/// operation codes 11-16; a `tool.finished` row holds no text field.
#[test]
fn j3t_20_job_tool_values_round_trip_with_codes_11_to_16() {
    for (tool, name, code) in [
        (Tool::JobList, "job_list", 11),
        (Tool::JobStatus, "job_status", 12),
        (Tool::JobRead, "job_read", 13),
        (Tool::JobWait, "job_wait", 14),
        (Tool::JobCancel, "job_cancel", 15),
        (Tool::JobNote, "job_note", 16),
    ] {
        assert_eq!(serde_json::to_string(&tool).unwrap(), format!("\"{name}\""));
        assert_eq!(
            serde_json::from_str::<Tool>(&format!("\"{name}\"")).unwrap(),
            tool
        );
        assert_eq!(tool as u32, code);
    }
    assert!(serde_json::from_str::<Tool>("\"job\"").is_err());
}

fn job_record(event: Event) -> Record {
    Record { event, ..record(1) }
}
fn queued() -> Event {
    Event::JobQueued {
        project: [3; 16],
        conversation: [4; 16],
        job: [5; 16],
        origin: JobOrigin::Model,
        agent: 0,
        reason: JobBlockReason::AgentSlots,
    }
}
fn started_model() -> Event {
    Event::JobStarted {
        project: [3; 16],
        conversation: [4; 16],
        job: [5; 16],
        origin: JobOrigin::Model,
        agent: 0,
        profile: JobProfile::Model1,
        operation: Some([6; 16]),
        generation: Some(2),
        ordinal: Some(1),
        input: None,
    }
}
fn cancel_requested(requester: JobRequester, reason: JobReason, kill: bool) -> Event {
    Event::JobCancelRequested {
        project: [3; 16],
        conversation: [4; 16],
        job: [5; 16],
        requester,
        reason,
        kill,
    }
}
fn settled_exited() -> Event {
    Event::JobSettled {
        project: [3; 16],
        conversation: [4; 16],
        job: [5; 16],
        outcome: JobOutcome::Exited,
        reason: JobReason::None,
        exit_kind: JobExitKind::Exited,
        exit_value: 0,
        cleanup: JobCleanup::Confirmed,
        unconfirmed_reason: JobUnconfirmedReason::None,
        escapees: 3,
        capture_state: JobCaptureState::Complete,
        stdout_bytes: 12,
        stderr_bytes: 0,
        elapsed_ms: 40,
        pause_count: 0,
    }
}
/// Replace fields of a `job.settled` event.
fn settled(edit: impl FnOnce(&mut Event)) -> Event {
    let mut event = settled_exited();
    edit(&mut event);
    event
}

/// J2T-20 unit layer: the four job events have their names and exact field
/// sets, round-trip through the strict codec, and name their project.
#[test]
fn j2t_20_job_events_names_fields_and_round_trip() {
    let person = Event::JobStarted {
        project: [3; 16],
        conversation: [4; 16],
        job: [5; 16],
        origin: JobOrigin::Person,
        agent: 0,
        profile: JobProfile::Model1,
        operation: None,
        generation: None,
        ordinal: None,
        input: Some([7; 16]),
    };
    let cases: [(Event, &str, &[&str]); 5] = [
        (
            queued(),
            "job.queued",
            &[
                "project",
                "conversation",
                "job",
                "origin",
                "agent",
                "reason",
            ],
        ),
        (
            started_model(),
            "job.started",
            &[
                "project",
                "conversation",
                "job",
                "origin",
                "agent",
                "profile",
                "operation",
                "generation",
                "ordinal",
            ],
        ),
        (
            person,
            "job.started",
            &[
                "project",
                "conversation",
                "job",
                "origin",
                "agent",
                "profile",
                "input",
            ],
        ),
        (
            cancel_requested(JobRequester::Person, JobReason::PersonKill, true),
            "job.cancel_requested",
            &[
                "project",
                "conversation",
                "job",
                "requester",
                "reason",
                "kill",
            ],
        ),
        (
            settled_exited(),
            "job.settled",
            &[
                "project",
                "conversation",
                "job",
                "outcome",
                "reason",
                "exit_kind",
                "exit_value",
                "cleanup",
                "unconfirmed_reason",
                "escapees",
                "capture_state",
                "stdout_bytes",
                "stderr_bytes",
                "elapsed_ms",
                "pause_count",
            ],
        ),
    ];
    for (event, kind, fields) in cases {
        assert_eq!(event.project(), Some([3; 16]), "{kind}");
        let value = job_record(event);
        let bytes = value.encode().unwrap();
        assert_eq!(Record::decode(&bytes).unwrap(), value, "{kind}");
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["kind"], kind);
        let details = json["details"].as_object().unwrap();
        let mut names: Vec<&str> = details.keys().map(String::as_str).collect();
        let mut expected = fields.to_vec();
        names.sort_unstable();
        expected.sort_unstable();
        assert_eq!(names, expected, "{kind}");
    }
    let text = String::from_utf8(job_record(started_model()).encode().unwrap()).unwrap();
    assert!(text.contains("\"profile\":\"model-1\""), "{text}");
    assert!(text.contains("\"origin\":\"model\""), "{text}");
    let text = String::from_utf8(job_record(queued()).encode().unwrap()).unwrap();
    assert!(text.contains("\"reason\":\"agent_slots\""), "{text}");
    let text = String::from_utf8(job_record(settled_exited()).encode().unwrap()).unwrap();
    for part in [
        "\"outcome\":\"exited\"",
        "\"reason\":\"none\"",
        "\"exit_kind\":\"exited\"",
        "\"cleanup\":\"confirmed\"",
        "\"unconfirmed_reason\":\"none\"",
        "\"escapees\":3",
        "\"capture_state\":\"complete\"",
    ] {
        assert!(text.contains(part), "{part} in {text}");
    }
}

/// J2T-20 unit layer: no job event can hold command, cwd, environment, path,
/// PID, note or output text; the closed schema rejects such fields, nulls,
/// duplicates and unknown enum names on decode.
#[test]
fn j2t_20_job_events_reject_text_fields_and_unknown_values() {
    for (event, anchor) in [
        (queued(), "\"agent\":0"),
        (started_model(), "\"ordinal\":1"),
        (
            cancel_requested(JobRequester::Model, JobReason::ModelCancel, false),
            "\"kill\":false",
        ),
        (settled_exited(), "\"pause_count\":0"),
    ] {
        let text = String::from_utf8(job_record(event).encode().unwrap()).unwrap();
        for field in [
            "command", "cwd", "env", "path", "pid", "note", "output", "stdout",
        ] {
            let broken = text.replacen(anchor, &format!("{anchor},\"{field}\":\"x\""), 1);
            assert_ne!(broken, text);
            assert_eq!(
                Record::decode(broken.as_bytes()),
                Err(Error::MalformedRecord),
                "{broken}"
            );
        }
        let duplicate = text.replacen(anchor, &format!("{anchor},{anchor}"), 1);
        assert_eq!(
            Record::decode(duplicate.as_bytes()),
            Err(Error::MalformedRecord),
            "{duplicate}"
        );
    }
    let text = String::from_utf8(job_record(started_model()).encode().unwrap()).unwrap();
    for broken in [
        text.replace("\"ordinal\":1", "\"ordinal\":null"),
        text.replace("\"generation\":2", "\"generation\":null"),
        text.replace("model-1", "person-1"),
        text.replace("\"origin\":\"model\"", "\"origin\":\"other\""),
        text.replace("\"ordinal\":1", "\"ordinal\":4294967296"),
    ] {
        assert_eq!(
            Record::decode(broken.as_bytes()),
            Err(Error::MalformedRecord),
            "{broken}"
        );
    }
    let text = String::from_utf8(job_record(queued()).encode().unwrap()).unwrap();
    assert_eq!(
        Record::decode(text.replace("agent_slots", "slots").as_bytes()),
        Err(Error::MalformedRecord)
    );
}

/// J2T-20 unit layer: every block reason, requester, reason, outcome, exit
/// kind, cleanup, unconfirmed reason and capture state has its design name
/// and journal code.
#[test]
fn j2t_20_job_enum_names_and_codes() {
    let name = |value: serde_json::Value| value.as_str().unwrap().to_owned();
    for (reason, text) in [
        (JobBlockReason::Recovering, "recovering"),
        (JobBlockReason::JournalUnavailable, "journal_unavailable"),
        (JobBlockReason::JournalHeld, "journal_held"),
        (JobBlockReason::PayloadUnavailable, "payload_unavailable"),
        (JobBlockReason::AgentSlots, "agent_slots"),
        (JobBlockReason::ServiceSlots, "service_slots"),
        (JobBlockReason::PendingInstalls, "pending_installs"),
        (JobBlockReason::Quota, "quota"),
        (JobBlockReason::DiskSpace, "disk_space"),
    ] {
        assert_eq!(name(serde_json::to_value(reason).unwrap()), text);
    }
    let reasons = [
        (JobReason::None, "none"),
        (JobReason::ModelCancel, "model_cancel"),
        (JobReason::TurnEnded, "turn_ended"),
        (JobReason::ClientCancel, "client_cancel"),
        (JobReason::PersonStop, "person_stop"),
        (JobReason::PersonKill, "person_kill"),
        (JobReason::IdentityChanged, "identity_changed"),
        (JobReason::Shutdown, "shutdown"),
        (JobReason::CaptureFault, "capture_fault"),
        (JobReason::Quota, "quota"),
        (JobReason::Enospc, "enospc"),
        (JobReason::Restart, "restart"),
        (JobReason::CwdUnavailable, "cwd_unavailable"),
        (JobReason::ProjectChanged, "project_changed"),
        (JobReason::SandboxUnavailable, "sandbox_unavailable"),
        (JobReason::SpawnFailed, "spawn_failed"),
        (JobReason::CaptureUnavailable, "capture_unavailable"),
        (JobReason::ProtocolFault, "protocol_fault"),
        (JobReason::SetupFailed, "setup_failed"),
        (JobReason::ExecutionOff, "execution_off"),
        (JobReason::OutsideSandboxOff, "outside_sandbox_off"),
    ];
    for (code, (reason, text)) in reasons.iter().enumerate() {
        assert_eq!(*reason as u8, code as u8);
        assert_eq!(name(serde_json::to_value(reason).unwrap()), *text);
    }
    for (value, text, code) in [
        (JobRequester::Model, "model", 1),
        (JobRequester::Client, "client", 2),
        (JobRequester::Person, "person", 3),
        (JobRequester::Service, "service", 4),
    ] {
        assert_eq!(name(serde_json::to_value(value).unwrap()), text);
        assert_eq!(value as u8, code);
    }
    for (value, text, code) in [
        (JobOutcome::Exited, "exited", 1),
        (JobOutcome::Signalled, "signalled", 2),
        (JobOutcome::Cancelled, "cancelled", 3),
        (JobOutcome::NotStarted, "not_started", 4),
        (JobOutcome::Interrupted, "interrupted", 5),
    ] {
        assert_eq!(name(serde_json::to_value(value).unwrap()), text);
        assert_eq!(value as u8, code);
    }
    for (value, text, code) in [
        (JobCaptureState::Complete, "complete", 1),
        (JobCaptureState::Incomplete, "incomplete", 2),
        (JobCaptureState::Unconfirmed, "unconfirmed", 3),
        (JobCaptureState::NotStarted, "not_started", 4),
        (JobCaptureState::Interrupted, "interrupted", 5),
    ] {
        assert_eq!(name(serde_json::to_value(value).unwrap()), text);
        assert_eq!(value as u8, code);
    }
    for (value, text, code) in [
        (JobExitKind::Unknown, "unknown", 0),
        (JobExitKind::Exited, "exited", 1),
        (JobExitKind::Signalled, "signalled", 2),
    ] {
        assert_eq!(name(serde_json::to_value(value).unwrap()), text);
        assert_eq!(value as u8, code);
    }
    for (value, text, code) in [
        (JobCleanup::None, "none", 0),
        (JobCleanup::Confirmed, "confirmed", 1),
        (JobCleanup::Late, "late", 2),
    ] {
        assert_eq!(name(serde_json::to_value(value).unwrap()), text);
        assert_eq!(value as u8, code);
    }
    for (value, text, code) in [
        (JobUnconfirmedReason::None, "none", 0),
        (JobUnconfirmedReason::CleanupFailed, "cleanup_failed", 9),
        (
            JobUnconfirmedReason::EscapeeUnterminated,
            "escapee_unterminated",
            13,
        ),
    ] {
        assert_eq!(name(serde_json::to_value(value).unwrap()), text);
        assert_eq!(value as u8, code);
    }
    assert_eq!(
        name(serde_json::to_value(JobOrigin::Person).unwrap()),
        "person"
    );
    assert_eq!(JobOrigin::Model as u8, 1);
    assert_eq!(JobOrigin::Person as u8, 2);
}

/// J2T-20 unit layer: validation applies the origin fields, the kind 27
/// reason table and kill rule, and the kind 28 outcome rules.
#[test]
fn j2t_20_job_events_validate_design_rules() {
    let invalid = |event: Event| {
        assert_eq!(event.validate(), Err(Error::Invalid), "{event:?}");
        assert_eq!(job_record(event).encode(), Err(Error::Invalid));
    };
    // IDs are nonzero.
    for index in 0..3 {
        let mut event = queued();
        if let Event::JobQueued {
            project,
            conversation,
            job,
            ..
        } = &mut event
        {
            *[project, conversation, job][index] = [0; 16];
        }
        invalid(event);
    }
    // Origin fields: a model start names its call, a person start its input.
    let started = |edit: fn(&mut Event)| {
        let mut event = started_model();
        edit(&mut event);
        event
    };
    for edit in [
        (|e: &mut Event| {
            if let Event::JobStarted { operation, .. } = e {
                *operation = None;
            }
        }) as fn(&mut Event),
        |e| {
            if let Event::JobStarted { generation, .. } = e {
                *generation = Some(0);
            }
        },
        |e| {
            if let Event::JobStarted { ordinal, .. } = e {
                *ordinal = Some(0);
            }
        },
        |e| {
            if let Event::JobStarted { ordinal, .. } = e {
                *ordinal = Some(crate::authority::conversation::MAX_TOOL_CALL_ORDINAL + 1);
            }
        },
        |e| {
            if let Event::JobStarted { input, .. } = e {
                *input = Some([7; 16]);
            }
        },
        |e| {
            if let Event::JobStarted { origin, .. } = e {
                *origin = JobOrigin::Person;
            }
        },
        |e| {
            if let Event::JobStarted { job, .. } = e {
                *job = [0; 16];
            }
        },
    ] {
        invalid(started(edit));
    }
    started(|e| {
        if let Event::JobStarted { ordinal, .. } = e {
            *ordinal = Some(crate::authority::conversation::MAX_TOOL_CALL_ORDINAL);
        }
    })
    .validate()
    .unwrap();
    // Kind 27: reason table per requester; kill only with `person_kill`.
    let allowed = [
        (JobRequester::Model, JobReason::ModelCancel),
        (JobRequester::Model, JobReason::TurnEnded),
        (JobRequester::Client, JobReason::ClientCancel),
        (JobRequester::Person, JobReason::PersonStop),
        (JobRequester::Service, JobReason::IdentityChanged),
        (JobRequester::Service, JobReason::Shutdown),
        (JobRequester::Service, JobReason::CaptureFault),
        (JobRequester::Service, JobReason::Quota),
        (JobRequester::Service, JobReason::Enospc),
        (JobRequester::Service, JobReason::ExecutionOff),
    ];
    for requester in [
        JobRequester::Model,
        JobRequester::Client,
        JobRequester::Person,
        JobRequester::Service,
    ] {
        for code in 0..=20u8 {
            let reason: JobReason =
                serde_json::from_value(serde_json::to_value(code_reason(code)).unwrap()).unwrap();
            let expected = allowed.contains(&(requester, reason));
            assert_eq!(
                cancel_requested(requester, reason, false)
                    .validate()
                    .is_ok(),
                expected,
                "{requester:?} {reason:?}"
            );
            let kill_ok = requester == JobRequester::Person && reason == JobReason::PersonKill;
            assert_eq!(
                cancel_requested(requester, reason, true).validate().is_ok(),
                kill_ok,
                "kill {requester:?} {reason:?}"
            );
        }
    }
    // Kind 28 outcome rules.
    macro_rules! set {
        ($($field:ident = $value:expr),*) => {
            settled(|e| {
                if let Event::JobSettled { $($field,)* .. } = e {
                    $(*$field = $value;)*
                }
            })
        };
    }
    settled_exited().validate().unwrap();
    set!(reason = JobReason::Enospc).validate().unwrap();
    set!(
        cleanup = JobCleanup::Late,
        unconfirmed_reason = JobUnconfirmedReason::EscapeeUnterminated
    )
    .validate()
    .unwrap();
    set!(
        outcome = JobOutcome::Signalled,
        exit_kind = JobExitKind::Signalled,
        exit_value = 9
    )
    .validate()
    .unwrap();
    set!(
        outcome = JobOutcome::Cancelled,
        reason = JobReason::PersonKill,
        exit_kind = JobExitKind::Signalled,
        exit_value = 9,
        capture_state = JobCaptureState::Unconfirmed
    )
    .validate()
    .unwrap();
    let not_started = || {
        set!(
            outcome = JobOutcome::NotStarted,
            reason = JobReason::SpawnFailed,
            exit_kind = JobExitKind::Unknown,
            cleanup = JobCleanup::None,
            escapees = 0,
            elapsed_ms = 0,
            capture_state = JobCaptureState::NotStarted,
            stdout_bytes = 0
        )
    };
    not_started().validate().unwrap();
    let interrupted = || {
        set!(
            outcome = JobOutcome::Interrupted,
            reason = JobReason::Restart,
            exit_kind = JobExitKind::Unknown,
            cleanup = JobCleanup::None,
            escapees = 0,
            elapsed_ms = 0,
            capture_state = JobCaptureState::Interrupted,
            stdout_bytes = 0
        )
    };
    interrupted().validate().unwrap();
    for event in [
        // Exit value bounds per exit kind.
        set!(exit_value = 256),
        set!(
            outcome = JobOutcome::Signalled,
            exit_kind = JobExitKind::Signalled,
            exit_value = 0
        ),
        set!(
            outcome = JobOutcome::Signalled,
            exit_kind = JobExitKind::Signalled,
            exit_value = 128
        ),
        // Outcome and exit kind agree.
        set!(exit_kind = JobExitKind::Signalled, exit_value = 9),
        set!(exit_kind = JobExitKind::Unknown),
        // A process outcome has cleanup and a capture outcome of 1-3.
        set!(cleanup = JobCleanup::None, escapees = 0),
        set!(capture_state = JobCaptureState::NotStarted),
        // The unconfirmed reason exists exactly with late cleanup.
        set!(cleanup = JobCleanup::Late),
        set!(unconfirmed_reason = JobUnconfirmedReason::CleanupFailed),
        // Reasons per outcome.
        set!(reason = JobReason::Restart),
        set!(reason = JobReason::SpawnFailed),
        set!(outcome = JobOutcome::Cancelled, reason = JobReason::None),
        set!(outcome = JobOutcome::Cancelled, reason = JobReason::Quota),
    ] {
        invalid(event);
    }
    for edit in [
        (|e: &mut Event| {
            if let Event::JobSettled { reason, .. } = e {
                *reason = JobReason::Restart;
            }
        }) as fn(&mut Event),
        |e| {
            if let Event::JobSettled { reason, .. } = e {
                *reason = JobReason::CaptureFault;
            }
        },
        |e| {
            if let Event::JobSettled { escapees, .. } = e {
                *escapees = 1;
            }
        },
        |e| {
            if let Event::JobSettled { elapsed_ms, .. } = e {
                *elapsed_ms = 1;
            }
        },
        |e| {
            if let Event::JobSettled { exit_value, .. } = e {
                *exit_value = 1;
            }
        },
        |e| {
            if let Event::JobSettled { capture_state, .. } = e {
                *capture_state = JobCaptureState::Complete;
            }
        },
    ] {
        let mut event = not_started();
        edit(&mut event);
        invalid(event);
    }
    for edit in [
        (|e: &mut Event| {
            if let Event::JobSettled { reason, .. } = e {
                *reason = JobReason::Shutdown;
            }
        }) as fn(&mut Event),
        |e| {
            if let Event::JobSettled { stdout_bytes, .. } = e {
                *stdout_bytes = 1;
            }
        },
        |e| {
            if let Event::JobSettled { stderr_bytes, .. } = e {
                *stderr_bytes = 1;
            }
        },
        |e| {
            if let Event::JobSettled { pause_count, .. } = e {
                *pause_count = 1;
            }
        },
        |e| {
            if let Event::JobSettled { capture_state, .. } = e {
                *capture_state = JobCaptureState::NotStarted;
            }
        },
    ] {
        let mut event = interrupted();
        edit(&mut event);
        invalid(event);
    }
}
fn code_reason(code: u8) -> &'static str {
    [
        "none",
        "model_cancel",
        "turn_ended",
        "client_cancel",
        "person_stop",
        "person_kill",
        "identity_changed",
        "shutdown",
        "capture_fault",
        "quota",
        "enospc",
        "restart",
        "cwd_unavailable",
        "project_changed",
        "sandbox_unavailable",
        "spawn_failed",
        "capture_unavailable",
        "protocol_fault",
        "setup_failed",
        "execution_off",
        "outside_sandbox_off",
    ][usize::from(code)]
}

/// The `jobs` setting names match their configuration keys, and every key a
/// `ConfigSet` may audit maps to its setting.
#[test]
fn j2t_20_job_setting_names_match_config_keys() {
    for (key, setting) in [
        ("jobs", Setting::Jobs),
        ("jobs.liveService", Setting::JobsLiveService),
        ("jobs.liveAgent", Setting::JobsLiveAgent),
        ("jobs.queuedAgent", Setting::JobsQueuedAgent),
        ("jobs.clientViewers", Setting::JobsClientViewers),
        (
            "jobs.modelReservedPageReads",
            Setting::JobsModelReservedPageReads,
        ),
    ] {
        assert_eq!(
            serde_json::to_string(&setting).unwrap(),
            format!("\"{key}\"")
        );
        assert_eq!(
            serde_json::from_str::<Setting>(&format!("\"{key}\"")).unwrap(),
            setting
        );
        assert_eq!(Setting::for_config_key(key), Some(setting));
    }
    for (key, setting) in [
        ("model", Setting::Model),
        ("audit", Setting::Audit),
        ("audit.enabled", Setting::AuditEnabled),
        ("audit.keepFiles", Setting::AuditKeepFiles),
        ("audit.maxFileBytes", Setting::AuditMaxFileBytes),
        ("providers.mlx.models", Setting::ProvidersMlxModels),
        ("retention.quotaBytes", Setting::RetentionQuotaBytes),
        ("journal", Setting::JournalQuotaBytes),
        ("journal.quotaBytes", Setting::JournalQuotaBytes),
    ] {
        assert_eq!(Setting::for_config_key(key), Some(setting), "{key}");
    }
    for key in ["", "thinking", "providers.ollama.endpoint", "jobs.other"] {
        assert_eq!(Setting::for_config_key(key), None, "{key}");
    }
}

/// J34-G: job reason 19 `execution_off` (EX11) and 20 `outside_sandbox_off`
/// (PCR23) follow the job reason table. Kind 27 accepts 19 from the service
/// alone and never 20; kind 28 accepts 19 for exited, signalled, cancelled
/// and not_started, and 20 for not_started alone.
#[test]
fn j34g_reasons_19_and_20_follow_the_reason_table() {
    assert_eq!(JobReason::ExecutionOff as u8, 19);
    assert_eq!(JobReason::OutsideSandboxOff as u8, 20);
    for requester in [
        JobRequester::Model,
        JobRequester::Client,
        JobRequester::Person,
        JobRequester::Service,
    ] {
        assert_eq!(
            cancel_requested(requester, JobReason::ExecutionOff, false)
                .validate()
                .is_ok(),
            requester == JobRequester::Service,
            "{requester:?}"
        );
        assert_eq!(
            cancel_requested(requester, JobReason::OutsideSandboxOff, false).validate(),
            Err(Error::Invalid),
            "{requester:?}"
        );
        assert_eq!(
            cancel_requested(requester, JobReason::ExecutionOff, true).validate(),
            Err(Error::Invalid),
            "kill {requester:?}"
        );
    }
    let exited = |reason| {
        settled(|e| {
            if let Event::JobSettled { reason: r, .. } = e {
                *r = reason;
            }
        })
    };
    let signalled = |reason| {
        settled(|e| {
            if let Event::JobSettled {
                outcome,
                reason: r,
                exit_kind,
                exit_value,
                ..
            } = e
            {
                *outcome = JobOutcome::Signalled;
                *r = reason;
                *exit_kind = JobExitKind::Signalled;
                *exit_value = 15;
            }
        })
    };
    let cancelled = |reason| {
        settled(|e| {
            if let Event::JobSettled {
                outcome,
                reason: r,
                exit_kind,
                exit_value,
                ..
            } = e
            {
                *outcome = JobOutcome::Cancelled;
                *r = reason;
                *exit_kind = JobExitKind::Signalled;
                *exit_value = 15;
            }
        })
    };
    let not_started = |reason| {
        settled(|e| {
            if let Event::JobSettled {
                outcome,
                reason: r,
                exit_kind,
                exit_value,
                cleanup,
                escapees,
                capture_state,
                stdout_bytes,
                elapsed_ms,
                ..
            } = e
            {
                *outcome = JobOutcome::NotStarted;
                *r = reason;
                *exit_kind = JobExitKind::Unknown;
                *exit_value = 0;
                *cleanup = JobCleanup::None;
                *escapees = 0;
                *capture_state = JobCaptureState::NotStarted;
                *stdout_bytes = 0;
                *elapsed_ms = 0;
            }
        })
    };
    let interrupted = |reason| {
        settled(|e| {
            if let Event::JobSettled {
                outcome,
                reason: r,
                exit_kind,
                exit_value,
                cleanup,
                escapees,
                capture_state,
                stdout_bytes,
                elapsed_ms,
                ..
            } = e
            {
                *outcome = JobOutcome::Interrupted;
                *r = reason;
                *exit_kind = JobExitKind::Unknown;
                *exit_value = 0;
                *cleanup = JobCleanup::None;
                *escapees = 0;
                *capture_state = JobCaptureState::Interrupted;
                *stdout_bytes = 0;
                *elapsed_ms = 0;
            }
        })
    };
    let off = JobReason::ExecutionOff;
    let outside = JobReason::OutsideSandboxOff;
    for event in [
        exited(off),
        signalled(off),
        cancelled(off),
        not_started(off),
    ] {
        event.validate().unwrap();
        let value = job_record(event);
        assert_eq!(Record::decode(&value.encode().unwrap()).unwrap(), value);
    }
    let text = String::from_utf8(job_record(cancelled(off)).encode().unwrap()).unwrap();
    assert!(text.contains("\"reason\":\"execution_off\""), "{text}");
    not_started(outside).validate().unwrap();
    let text = String::from_utf8(job_record(not_started(outside)).encode().unwrap()).unwrap();
    assert!(
        text.contains("\"reason\":\"outside_sandbox_off\""),
        "{text}"
    );
    for event in [
        exited(outside),
        signalled(outside),
        cancelled(outside),
        interrupted(outside),
        interrupted(off),
    ] {
        assert_eq!(event.validate(), Err(Error::Invalid), "{event:?}");
    }
    // A code past the table never decodes.
    let text = String::from_utf8(job_record(cancelled(off)).encode().unwrap()).unwrap();
    assert_eq!(
        Record::decode(
            text.replace("execution_off", "project_unregistered")
                .as_bytes()
        ),
        Err(Error::MalformedRecord)
    );
}

fn execution_changed(
    scope: ExecutionScope,
    project: Option<Id>,
    from: ExecutionValue,
    to: ExecutionValue,
    revoked_projects: u32,
) -> Event {
    Event::ExecutionChanged {
        scope,
        project,
        from,
        to,
        revoked_projects,
    }
}

/// ES-11 unit layer: `execution.changed` has its name, exact fields and
/// value spellings, round-trips, holds no command or path text, and stays
/// out of the project view.
#[test]
fn es_11_execution_changed_names_fields_and_round_trip() {
    use ExecutionScope::{Global, Project};
    use ExecutionValue::{Inherit, Off, On};
    for (event, fields, parts) in [
        (
            execution_changed(Global, None, On, Off, 3),
            &["scope", "from", "to", "revoked_projects"][..],
            &[
                "\"scope\":\"global\"",
                "\"from\":\"on\"",
                "\"to\":\"off\"",
                "\"revoked_projects\":3",
            ][..],
        ),
        (
            execution_changed(Project, Some([3; 16]), Inherit, Off, 1),
            &["scope", "project", "from", "to", "revoked_projects"][..],
            &[
                "\"scope\":\"project\"",
                "\"project\":\"03030303030303030303030303030303\"",
                "\"from\":\"inherit\"",
                "\"to\":\"off\"",
                "\"revoked_projects\":1",
            ][..],
        ),
    ] {
        assert_eq!(event.project(), None, "not a project record (J4-E8)");
        let value = job_record(event);
        let bytes = value.encode().unwrap();
        assert_eq!(Record::decode(&bytes).unwrap(), value);
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["kind"], "execution.changed");
        let mut names: Vec<&str> = json["details"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        let mut expected = fields.to_vec();
        names.sort_unstable();
        expected.sort_unstable();
        assert_eq!(names, expected);
        let text = String::from_utf8(bytes).unwrap();
        for part in parts {
            assert!(text.contains(part), "{part} in {text}");
        }
        // The closed schema rejects text fields, nulls, duplicates and
        // unknown values.
        for field in ["command", "cwd", "path", "name", "note"] {
            let broken = text.replacen("\"scope\"", &format!("\"{field}\":\"x\",\"scope\""), 1);
            assert_eq!(
                Record::decode(broken.as_bytes()),
                Err(Error::MalformedRecord),
                "{broken}"
            );
        }
        for broken in [
            text.replacen("\"to\":\"off\"", "\"to\":\"off\",\"to\":\"off\"", 1),
            text.replacen("\"to\":\"off\"", "\"to\":null", 1),
            text.replacen("\"to\":\"off\"", "\"to\":\"maybe\"", 1),
            text.replacen("\"scope\":", "\"scope\":\"other\",\"x\":", 1),
        ] {
            assert_eq!(
                Record::decode(broken.as_bytes()),
                Err(Error::MalformedRecord),
                "{broken}"
            );
        }
    }
}

/// ES-11 unit layer: `execution.changed` validation. Global scope has no
/// project and no `inherit`; project scope names one nonzero project and
/// turns at most that project off.
#[test]
fn es_11_execution_changed_validates_scope_rules() {
    use ExecutionScope::{Global, Project};
    use ExecutionValue::{Inherit, Off, On};
    for event in [
        execution_changed(Global, None, On, Off, 0),
        execution_changed(Global, None, On, Off, 70_000),
        execution_changed(Global, None, Off, On, 0),
        execution_changed(Global, None, On, On, 0),
        execution_changed(Project, Some([3; 16]), On, Off, 1),
        execution_changed(Project, Some([3; 16]), Inherit, Off, 0),
        execution_changed(Project, Some([3; 16]), On, Inherit, 1),
        execution_changed(Project, Some([3; 16]), Off, On, 0),
        execution_changed(Project, Some([3; 16]), Off, Inherit, 0),
    ] {
        event.validate().unwrap();
        job_record(event).encode().unwrap();
    }
    for event in [
        execution_changed(Global, Some([3; 16]), On, Off, 0),
        execution_changed(Global, None, Inherit, Off, 0),
        execution_changed(Global, None, On, Inherit, 0),
        execution_changed(Global, None, Off, On, 1),
        execution_changed(Project, None, On, Off, 1),
        execution_changed(Project, Some([0; 16]), On, Off, 1),
        execution_changed(Project, Some([3; 16]), On, Off, 2),
        execution_changed(Project, Some([3; 16]), Off, On, 1),
    ] {
        assert_eq!(event.validate(), Err(Error::Invalid), "{event:?}");
        assert_eq!(job_record(event).encode(), Err(Error::Invalid));
    }
}

/// ES-02 and J34-G: every `execution` key maps to its `config.changed`
/// setting; a leaf maps to `execution.projects` only with a valid ID.
#[test]
fn es_02_execution_setting_names_match_config_keys() {
    for (name, setting) in [
        ("execution", Setting::Execution),
        ("execution.enabled", Setting::ExecutionEnabled),
        ("execution.projects", Setting::ExecutionProjects),
    ] {
        assert_eq!(
            serde_json::to_string(&setting).unwrap(),
            format!("\"{name}\"")
        );
        assert_eq!(
            serde_json::from_str::<Setting>(&format!("\"{name}\"")).unwrap(),
            setting
        );
        assert_eq!(Setting::for_config_key(name), Some(setting));
    }
    let leaf = crate::config::execution_project_key(&[0xab; 16]);
    assert_eq!(
        Setting::for_config_key(&leaf),
        Some(Setting::ExecutionProjects)
    );
    for key in [
        "execution.projects.",
        "execution.projects.00000000000000000000000000000000",
        "execution.projects.ABABABABABABABABABABABABABABABAB",
        "execution.projects.abababababababababababababababab.x",
        "execution.other",
        "executionx",
    ] {
        assert_eq!(Setting::for_config_key(key), None, "{key}");
    }
}
