use super::*;
use asura_platform::RuntimeDirectory;
use std::{
    fs,
    io::Write,
    os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt, symlink},
    path::PathBuf,
    sync::atomic::AtomicBool,
    time::{Duration, Instant},
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
fn deadline() -> Instant {
    Instant::now() + Duration::from_secs(10)
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
        deadline(),
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
        crate::config::audit_settings(runtime, deadline(), &AtomicBool::new(false)).unwrap();
    assert_eq!(settings, Settings::default());
    assert_eq!(source, ConfigLoad::Defaults);
    let mut opened = open(&s, false).unwrap();
    assert!(!s.logs().exists());
    assert!(opened.recent.is_empty());
    assert_eq!(
        opened
            .store
            .append_batch(&[record(1)], deadline(), &AtomicBool::new(false)),
        Err(Error::Disabled)
    );
    opened
        .store
        .maintain(deadline(), &AtomicBool::new(false))
        .unwrap();
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
        .append_batch(&[next.clone()], deadline(), &AtomicBool::new(false))
        .unwrap();
    drop(opened);
    let reopened = AuditStore::open(
        s.runtime(),
        Settings::default(),
        [3; 16],
        deadline(),
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
        deadline(),
        &cancel,
    )
    .unwrap();
    fs::write(s.logs().join("other.log"), b"keep").unwrap();
    for n in 1..=5 {
        opened
            .store
            .append_batch(&[record(n)], deadline(), &cancel)
            .unwrap();
        opened.store.maintain(deadline(), &cancel).unwrap();
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
            .append_batch(&[rec], deadline(), &AtomicBool::new(false))
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
            deadline(),
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
        deadline(),
        &AtomicBool::new(false),
    )
    .unwrap();
    assert_eq!(
        opened
            .store
            .append_batch(&[record(1)], deadline(), &AtomicBool::new(false)),
        Err(Error::Limit)
    );
    assert!(fs::read(s.active()).unwrap().is_empty());
    assert_eq!(
        opened
            .store
            .append_batch(&[record(1)], Instant::now(), &AtomicBool::new(false)),
        Err(Error::Deadline)
    );
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
        deadline(),
        &cancel,
    )
    .unwrap();
    // Startup sealed the old active and performed exactly one bounded cycle.
    assert!(s.active().exists());
    assert_eq!(fs::read_dir(s.logs()).unwrap().count(), 40);
    opened.store.maintain(deadline(), &cancel).unwrap();
    assert!(s.active().exists());
    assert_eq!(fs::read_dir(s.logs()).unwrap().count(), 8);
    opened.store.maintain(deadline(), &cancel).unwrap();
    assert!(s.active().exists());
    assert_eq!(fs::read_dir(s.logs()).unwrap().count(), 2);
    let mut row = record(1);
    row.service_epoch = [2; 16];
    opened
        .store
        .append_batch(&[row], deadline(), &cancel)
        .unwrap();
}
