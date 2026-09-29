//! Cache contract fixtures are not complete toolchain snapshots.
use asura_toolchain_bootstrap::{
    cache::{self, CacheError, Manifest},
    parse_lock,
    preparation::run_operation,
};
use std::{
    fs,
    os::unix::fs::symlink,
    path::PathBuf,
    process::Command,
    sync::atomic::{AtomicU64, Ordering},
};
static NEXT: AtomicU64 = AtomicU64::new(0);
struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "asura-cache-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path.canonicalize().unwrap())
    }
    fn metadata(&self) -> Vec<u8> {
        fs::write(
            self.0.join("Cargo.toml"),
            "[package]\nname=\"asura-toolchain-bootstrap\"\nversion=\"0.1.0\"\n",
        )
        .unwrap();
        serde_json::to_vec(&serde_json::json!({"workspace_root":self.0,"workspace_members":["local"],"packages":[{"id":"local","name":"asura-toolchain-bootstrap","version":"0.1.0","source":null,"manifest_path":self.0.join("Cargo.toml")}]})).unwrap()
    }
    fn setup(&self, id: &str) {
        let metadata = self.metadata();
        let run = self.0.join(".build/asura-protobuf/.runs").join(id);
        fs::create_dir_all(&run).unwrap();
        fs::create_dir_all(self.0.join(".build/asura-deps/cargo")).unwrap();
        fs::create_dir_all(self.0.join("tools/protobuf")).unwrap();
        fs::write(self.0.join("tools/protobuf/lock.json"), TOOL_LOCK).unwrap();
        fs::write(self.0.join("Cargo.lock"), LOCAL_LOCK).unwrap();
        let lock: serde_json::Value = serde_json::from_slice(TOOL_LOCK).unwrap();
        fs::write(
            run.join("host.json"),
            serde_json::to_vec(&lock["host"]).unwrap(),
        )
        .unwrap();
        fs::write(run.join("cargo-metadata.json"), metadata).unwrap();
    }
    fn cargo_fixture(&self, name: &str) -> PathBuf {
        let path = self.0.join(".build/asura-deps/cargo").join(name);
        fs::create_dir(&path).unwrap();
        let lock = parse_lock(TOOL_LOCK).unwrap();
        let mut m = Manifest::new("cargo", &lock, LOCAL_LOCK, &path).unwrap();
        m.packages = Some(vec![]);
        m.checked_targets = Some(vec!["asura-toolchain-bootstrap".into()]);
        m.write(&path).unwrap();
        path
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
const LOCAL_LOCK: &[u8] =
    b"version=4\n[[package]]\nname=\"asura-toolchain-bootstrap\"\nversion=\"0.1.0\"\n";
const TOOL_LOCK: &[u8] = include_bytes!("fixtures/lock.json");

#[test]
fn cargo_inventory_rejects_unowned_packages_and_unapproved_sources() {
    let f = Fixture::new();
    let metadata = f.metadata();
    assert_eq!(cache::packages(LOCAL_LOCK, &metadata).unwrap().len(), 1);
    let changed = String::from_utf8(LOCAL_LOCK.to_vec())
        .unwrap()
        .replace("asura-toolchain-bootstrap", "unowned");
    assert_eq!(
        cache::packages(changed.as_bytes(), &metadata).unwrap_err(),
        CacheError::InvalidInput
    );
    for extra in [
        "source=\"git+https://example.invalid/repo\"\n",
        "checksum=\"00\"\n",
        "unexpected=true\n",
        "name=\"duplicate\"\n",
    ] {
        let bytes = [LOCAL_LOCK, extra.as_bytes()].concat();
        assert_eq!(
            cache::packages(&bytes, &metadata).unwrap_err(),
            CacheError::InvalidInput
        );
    }
    assert_eq!(
        cache::packages(&vec![b' '; 1024 * 1024 + 1], &metadata).unwrap_err(),
        CacheError::Limit
    );
    let mut wrong: serde_json::Value = serde_json::from_slice(&metadata).unwrap();
    wrong["workspace_members"] = serde_json::json!([]);
    assert_eq!(
        cache::packages(LOCAL_LOCK, &serde_json::to_vec(&wrong).unwrap()).unwrap_err(),
        CacheError::InvalidInput
    );
}

#[test]
fn snapshot_inventory_rejects_missing_changed_extra_or_linked_inputs() {
    let f = Fixture::new();
    let lock = parse_lock(TOOL_LOCK).unwrap();
    fs::create_dir_all(f.0.join("registry/index/fixture")).unwrap();
    let file = f.0.join("registry/index/fixture/config.json");
    fs::write(&file, b"{}").unwrap();
    let mut manifest = Manifest::new("cargo", &lock, LOCAL_LOCK, &f.0).unwrap();
    manifest.packages = Some(vec![]);
    manifest.checked_targets = Some(vec!["asura-toolchain-bootstrap".into()]);
    manifest.write(&f.0).unwrap();
    Manifest::read(&f.0, "cargo", &lock, LOCAL_LOCK).unwrap();
    fs::write(&file, b"changed").unwrap();
    assert!(Manifest::read(&f.0, "cargo", &lock, LOCAL_LOCK).is_err());
    fs::write(&file, b"{}").unwrap();
    fs::write(f.0.join("registry/index/extra"), b"extra").unwrap();
    assert!(Manifest::read(&f.0, "cargo", &lock, LOCAL_LOCK).is_err());
    fs::remove_file(f.0.join("registry/index/extra")).unwrap();
    fs::remove_file(&file).unwrap();
    assert!(Manifest::read(&f.0, "cargo", &lock, LOCAL_LOCK).is_err());
    symlink(f.0.join("manifest.json"), &file).unwrap();
    assert!(Manifest::read(&f.0, "cargo", &lock, LOCAL_LOCK).is_err());
    fs::remove_file(&file).unwrap();
    fs::write(&file, b"{}").unwrap();
    assert!(Manifest::read(&f.0, "cargo", &lock, b"other lock").is_err());
    let original = fs::read(f.0.join("manifest.json")).unwrap();
    let mut bad = original.clone();
    bad.splice(1..1, b"\"format_version\":1,".iter().copied());
    fs::write(f.0.join("manifest.json"), bad).unwrap();
    assert_eq!(
        Manifest::read(&f.0, "cargo", &lock, LOCAL_LOCK).unwrap_err(),
        CacheError::InvalidInput
    );
}

#[test]
fn inspection_binary_validates_files_with_network_disabled() {
    let f = Fixture::new();
    let metadata = f.metadata();
    let run = f.0.join(".build/asura-protobuf/.runs/fixture");
    fs::create_dir_all(&run).unwrap();
    fs::create_dir_all(f.0.join(".build/asura-deps/cargo")).unwrap();
    fs::create_dir_all(f.0.join("tools/protobuf")).unwrap();
    fs::write(f.0.join("tools/protobuf/lock.json"), TOOL_LOCK).unwrap();
    fs::write(f.0.join("Cargo.lock"), LOCAL_LOCK).unwrap();
    let lock: serde_json::Value = serde_json::from_slice(TOOL_LOCK).unwrap();
    fs::write(
        run.join("host.json"),
        serde_json::to_vec(&lock["host"]).unwrap(),
    )
    .unwrap();
    fs::write(run.join("cargo-metadata.json"), metadata).unwrap();
    let invoke = |operation: &str| {
        Command::new("/usr/bin/sandbox-exec")
            .args(["-p", "(version 1)(allow default)(deny network*)"])
            .arg(env!("CARGO_BIN_EXE_asura-toolchain-bootstrap"))
            .arg(operation)
            .arg("--root")
            .arg(&f.0)
            .args(["--run-id", "fixture"])
            .output()
            .unwrap()
    };
    assert!(invoke("inspect").status.success());
    let receipt = fs::read_to_string(run.join("inspect.receipt")).unwrap();
    assert!(receipt.starts_with("ASURA-PB03 1 inspect fixture "));
    assert!(receipt.ends_with(" unqualified\n"));
    assert!(
        !invoke("inspect").status.success(),
        "reused receipt must fail"
    );
    assert!(!invoke("arbitrary-command").status.success());
    fs::write(run.join("host.json"), b"{}").unwrap();
    assert!(!invoke("stage-tools").status.success());
    assert!(!f.0.join(".build/asura-protobuf/.stage-fixture").exists());
}

#[test]
fn failed_publication_preserves_prior_identity_and_quarantines_only_candidate() {
    use std::os::unix::fs::MetadataExt;
    // A failure before the first rename must not move the existing snapshot.
    let before = Fixture::new();
    before.setup("before");
    let prior = before.cargo_fixture("snapshot");
    let inode = fs::metadata(&prior).unwrap().ino();
    assert!(run_operation(&before.0, "before", "begin-publish-cargo").is_err());
    run_operation(&before.0, "before", "rollback-cargo").unwrap();
    assert_eq!(fs::metadata(&prior).unwrap().ino(), inode);
    // An unqualified first publication has no prior snapshot to restore.
    let first = Fixture::new();
    first.setup("first");
    first.cargo_fixture(".stage-first");
    run_operation(&first.0, "first", "begin-publish-cargo").unwrap();
    run_operation(&first.0, "first", "rollback-cargo").unwrap();
    assert!(!first.0.join(".build/asura-deps/cargo/snapshot").exists());
    assert!(
        first
            .0
            .join(".build/asura-deps/cargo/.quarantine-first-snapshot")
            .is_dir()
    );
    // A failed replacement restores the same prior directory, not candidate bytes.
    let replacement = Fixture::new();
    replacement.setup("replacement");
    let prior = replacement.cargo_fixture("snapshot");
    let inode = fs::metadata(&prior).unwrap().ino();
    replacement.cargo_fixture(".stage-replacement");
    run_operation(&replacement.0, "replacement", "begin-publish-cargo").unwrap();
    run_operation(&replacement.0, "replacement", "rollback-cargo").unwrap();
    assert_eq!(fs::metadata(&prior).unwrap().ino(), inode);
}

#[test]
fn restart_restores_unfinished_backup_and_rejects_ambiguous_backups() {
    use std::os::unix::fs::MetadataExt;
    let f = Fixture::new();
    f.setup("old");
    let prior = f.cargo_fixture("snapshot");
    let inode = fs::metadata(&prior).unwrap().ino();
    f.cargo_fixture(".stage-old");
    run_operation(&f.0, "old", "begin-publish-cargo").unwrap();
    f.setup("restart");
    run_operation(&f.0, "restart", "recover-start").unwrap();
    assert_eq!(fs::metadata(&prior).unwrap().ino(), inode);
    let ambiguous = Fixture::new();
    ambiguous.setup("ambiguous");
    ambiguous.cargo_fixture(".backup-one");
    ambiguous.cargo_fixture(".backup-two");
    assert_eq!(
        run_operation(&ambiguous.0, "ambiguous", "recover-start").unwrap_err(),
        CacheError::CleanupRequired
    );
    assert!(
        ambiguous
            .0
            .join(".build/asura-deps/cargo/.backup-one/manifest.json")
            .is_file()
    );
    assert!(
        ambiguous
            .0
            .join(".build/asura-deps/cargo/.backup-two/manifest.json")
            .is_file()
    );
}

#[test]
fn recovery_rejects_invalid_publication_without_backup_or_success_receipt() {
    for operation in ["recover", "recover-start"] {
        for tools in [true, false] {
            let f = Fixture::new();
            f.setup("invalid");
            let published = if tools {
                let key: String = parse_lock(TOOL_LOCK)
                    .unwrap()
                    .digest()
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect();
                let path = f.0.join(".build/asura-protobuf").join(key);
                fs::create_dir(&path).unwrap();
                path
            } else {
                f.cargo_fixture("snapshot")
            };
            let manifest = published.join("manifest.json");
            fs::write(&manifest, b"damaged manifest").unwrap();
            for command_retry in [false, true] {
                if command_retry {
                    let output = Command::new("/usr/bin/sandbox-exec")
                        .args(["-p", "(version 1)(allow default)(deny network*)"])
                        .arg(env!("CARGO_BIN_EXE_asura-toolchain-bootstrap"))
                        .arg(operation)
                        .arg("--root")
                        .arg(&f.0)
                        .args(["--run-id", "invalid"])
                        .output()
                        .unwrap();
                    assert_eq!(output.status.code(), Some(1));
                    assert_eq!(output.stderr, b"bootstrap: CleanupRequired\n");
                    assert!(output.stdout.is_empty());
                } else {
                    assert_eq!(
                        run_operation(&f.0, "invalid", operation).unwrap_err(),
                        CacheError::CleanupRequired
                    );
                }
                assert_eq!(fs::read(&manifest).unwrap(), b"damaged manifest");
                assert!(
                    !f.0.join(".build/asura-protobuf/.runs/invalid")
                        .join(format!("{operation}.receipt"))
                        .exists()
                );
            }
        }
    }
}

#[test]
fn archive_record_requires_source_commit_key_and_accepts_explicit_null() {
    use cache::ArchiveRecord;
    let explicit = serde_json::json!({
        "id":"protoc", "version":"34.1", "sha256":"a".repeat(64),
        "path":"archives/protoc.zip", "source_commit":null
    });
    let record: ArchiveRecord = serde_json::from_value(explicit.clone()).unwrap();
    assert!(record.source_commit.is_none());
    assert!(
        serde_json::to_value(record.clone())
            .unwrap()
            .get("source_commit")
            .unwrap()
            .is_null()
    );
    // This fixture exercises the persisted schema reader, not tool qualification.
    let f = Fixture::new();
    let lock = parse_lock(TOOL_LOCK).unwrap();
    let mut manifest = Manifest::new("tools", &lock, LOCAL_LOCK, &f.0).unwrap();
    manifest.archives = Some(vec![record.clone(), record]);
    let executable = cache::ExecutableRecord {
        id: "fixture".into(),
        path: "bin/fixture".into(),
        sha256: "a".repeat(64),
        expected_version: "1.0".into(),
    };
    manifest.executables = Some(vec![executable.clone(), executable]);
    manifest.generator_build_path = Some(f.0.to_str().unwrap().into());
    manifest.write(&f.0).unwrap();
    assert!(Manifest::read(&f.0, "tools", &lock, LOCAL_LOCK).is_ok());
    let mut persisted: serde_json::Value =
        serde_json::from_slice(&fs::read(f.0.join("manifest.json")).unwrap()).unwrap();
    persisted["archives"][0]
        .as_object_mut()
        .unwrap()
        .remove("source_commit");
    fs::write(
        f.0.join("manifest.json"),
        serde_json::to_vec(&persisted).unwrap(),
    )
    .unwrap();
    assert_eq!(
        Manifest::read(&f.0, "tools", &lock, LOCAL_LOCK).unwrap_err(),
        CacheError::InvalidInput
    );
    let mut missing = explicit.clone();
    missing.as_object_mut().unwrap().remove("source_commit");
    assert!(serde_json::from_value::<ArchiveRecord>(missing).is_err());
    let mut source = explicit;
    source["source_commit"] = serde_json::json!("b".repeat(40));
    assert_eq!(
        serde_json::from_value::<ArchiveRecord>(source)
            .unwrap()
            .source_commit,
        Some("b".repeat(40))
    );
}
