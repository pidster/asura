use super::*;
use std::fs;
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt, symlink};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::Command;
use std::time::{Duration, Instant};

struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Self {
        let random = u128::from_ne_bytes(random_id());
        let path = PathBuf::from(format!(
            "/private/tmp/asura-fnd-{}-{random:x}",
            std::process::id()
        ));
        fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
        Self(path)
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn owner_lifetime_socket_peer_and_readiness() {
    let root = Scratch::new();
    let runtime = RuntimeDirectory::scratch(&root.0, true).unwrap();
    let mut owner = runtime.acquire_owner().unwrap();
    let witness = runtime.capture_lock().unwrap();
    assert!(!witness.owner_released().unwrap());
    assert!(matches!(runtime.acquire_owner(), Err(Error::OwnerBusy)));
    let listener = owner.bind().unwrap();
    let mut client = runtime.connect().unwrap();
    let (stream, _) = listener.accept().unwrap();
    let mut server = AuthenticatedStream::from_stream(stream).unwrap();
    client.stream_mut().write_all(b"test").unwrap();
    let events = poll(
        &[PollInterest {
            fd: server.stream().as_raw_fd(),
            read: true,
            write: false,
        }],
        Duration::from_secs(1),
    )
    .unwrap();
    assert_eq!(events.len(), 1);
    assert!(events[0].read);
    let mut bytes = [0; 4];
    server.stream_mut().read_exact(&mut bytes).unwrap();
    assert_eq!(&bytes, b"test");
    owner.validate().unwrap();
    owner.remove_endpoint().unwrap();
    assert!(matches!(runtime.connect(), Err(Error::Absent)));
    drop(owner);
    assert!(witness.owner_released().unwrap());
}
#[test]
fn inspection_does_not_create_runtime() {
    let root = Scratch::new();
    assert!(matches!(
        RuntimeDirectory::scratch(&root.0, false),
        Err(Error::Absent)
    ));
    assert!(!root.0.join(".asura").exists());
}
#[test]
fn runtime_alias_and_unsafe_modes_are_preserved() {
    let root = Scratch::new();
    let target = Scratch::new();
    symlink(&target.0, root.0.join(".asura")).unwrap();
    assert!(matches!(
        RuntimeDirectory::scratch(&root.0, true),
        Err(Error::UnsafeRuntime)
    ));
    assert!(
        root.0
            .join(".asura")
            .symlink_metadata()
            .unwrap()
            .file_type()
            .is_symlink()
    );
    fs::remove_file(root.0.join(".asura")).unwrap();
    fs::DirBuilder::new()
        .mode(0o755)
        .create(root.0.join(".asura"))
        .unwrap();
    assert!(matches!(
        RuntimeDirectory::scratch(&root.0, true),
        Err(Error::UnsafeRuntime)
    ));
    assert_eq!(
        fs::metadata(root.0.join(".asura"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o755
    );
}
#[test]
fn lock_hardlinks_and_endpoint_substitution_are_rejected() {
    let root = Scratch::new();
    let runtime = RuntimeDirectory::scratch(&root.0, true).unwrap();
    let mut owner = runtime.acquire_owner().unwrap();
    let listener = owner.bind().unwrap();
    let socket = root.0.join(".asura/run/control.sock");
    fs::remove_file(&socket).unwrap();
    assert!(matches!(owner.validate(), Err(Error::UnsafeRuntime)));
    fs::write(&socket, b"replacement").unwrap();
    assert!(matches!(owner.remove_endpoint(), Err(Error::UnsafeRuntime)));
    assert_eq!(fs::read(&socket).unwrap(), b"replacement");
    drop(listener);
    drop(owner);
    fs::hard_link(
        root.0.join(".asura/run/owner.lock"),
        root.0.join("another-lock-name"),
    )
    .unwrap();
    assert!(matches!(runtime.acquire_owner(), Err(Error::UnsafeRuntime)));
}
#[test]
fn replaced_directory_and_lock_keep_replacements() {
    let root = Scratch::new();
    let runtime = RuntimeDirectory::scratch(&root.0, true).unwrap();
    let owner = runtime.acquire_owner().unwrap();
    let lock = root.0.join(".asura/run/owner.lock");
    fs::rename(&lock, root.0.join("saved-lock")).unwrap();
    assert!(matches!(owner.validate(), Err(Error::UnsafeRuntime)));
    fs::write(&lock, b"replacement").unwrap();
    assert!(matches!(owner.validate(), Err(Error::UnsafeRuntime)));
    assert_eq!(fs::read(&lock).unwrap(), b"replacement");
    fs::rename(root.0.join(".asura/run"), root.0.join("old-run")).unwrap();
    assert!(matches!(runtime.validate(), Err(Error::UnsafeRuntime)));
    fs::DirBuilder::new()
        .mode(0o700)
        .create(root.0.join(".asura/run"))
        .unwrap();
    assert!(matches!(runtime.validate(), Err(Error::UnsafeRuntime)));
}
#[test]
fn extended_acl_grant_to_another_principal_is_rejected() {
    let root = Scratch::new();
    let runtime = RuntimeDirectory::scratch(&root.0, true).unwrap();
    let path = root.0.join(".asura/run");
    let status = Command::new("/bin/chmod")
        .args(["+a", "everyone allow read,search"])
        .arg(&path)
        .status()
        .unwrap();
    assert!(status.success());
    assert!(matches!(runtime.validate(), Err(Error::UnsafeRuntime)));
    let status = Command::new("/bin/chmod")
        .arg("-N")
        .arg(path)
        .status()
        .unwrap();
    assert!(status.success());
}
#[test]
fn socket_length_and_poll_bounds() {
    let root = Scratch::new();
    let long = root.0.join("x".repeat(104));
    assert!(matches!(
        RuntimeDirectory::scratch(&long, true),
        Err(Error::UnsafeRuntime)
    ));
    assert!(matches!(
        poll(
            &vec![
                PollInterest {
                    fd: -1,
                    read: true,
                    write: false
                };
                66
            ],
            Duration::ZERO
        ),
        Err(Error::Unavailable)
    ));
    let start = Instant::now();
    assert!(poll(&[], Duration::from_millis(20)).unwrap().is_empty());
    assert!(start.elapsed() >= Duration::from_millis(20));
    assert!(start.elapsed() < Duration::from_secs(1));
    let (one, two) = UnixStream::pair().unwrap();
    drop(two);
    assert!(
        poll(
            &[PollInterest {
                fd: one.as_raw_fd(),
                read: true,
                write: false
            }],
            Duration::from_secs(1)
        )
        .unwrap()[0]
            .closed
    );
}
#[test]
fn process_lock_child() {
    if let Some(root) = std::env::var_os("ASURA_PLATFORM_LOCK_TEST") {
        let runtime = RuntimeDirectory::scratch(&PathBuf::from(root), false).unwrap();
        assert!(matches!(runtime.acquire_owner(), Err(Error::OwnerBusy)));
    } else {
        let root = Scratch::new();
        let runtime = RuntimeDirectory::scratch(&root.0, true).unwrap();
        let _owner = runtime.acquire_owner().unwrap();
        let status = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "tests::process_lock_child", "--nocapture"])
            .env("ASURA_PLATFORM_LOCK_TEST", &root.0)
            .status()
            .unwrap();
        assert!(status.success());
    }
}

#[test]
fn log_creation_is_private_and_reopening_appends() {
    let root = Scratch::new();
    let directory = root.0.join("new/nested");
    let mut first = open_private_append(&directory, "asura.log").unwrap();
    first.write_all(b"first\n").unwrap();
    let mut second = open_private_append(&directory, "asura.log").unwrap();
    second.write_all(b"second\n").unwrap();
    assert_eq!(
        fs::read(directory.join("asura.log")).unwrap(),
        b"first\nsecond\n"
    );
    for path in [root.0.join("new"), directory.clone()] {
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }
    assert_eq!(
        fs::metadata(directory.join("asura.log"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
}

#[test]
fn log_directory_and_file_aliases_are_rejected_without_mutation() {
    let root = Scratch::new();
    let other = Scratch::new();
    let alias = root.0.join("alias");
    symlink(&other.0, &alias).unwrap();
    assert!(open_private_append(&alias, "asura.log").is_err());
    assert!(!other.0.join("asura.log").exists());
    fs::write(other.0.join("protected"), b"preserve").unwrap();
    symlink(other.0.join("protected"), root.0.join("asura.log")).unwrap();
    assert!(open_private_append(&root.0, "asura.log").is_err());
    assert_eq!(fs::read(other.0.join("protected")).unwrap(), b"preserve");
}

#[test]
fn log_unsafe_modes_hardlinks_and_nonregular_entries_are_rejected() {
    let root = Scratch::new();
    let path = root.0.join("asura.log");
    fs::write(&path, b"preserve").unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(matches!(
        open_private_append(&root.0, "asura.log"),
        Err(Error::UnsafeRuntime)
    ));
    assert_eq!(fs::read(&path).unwrap(), b"preserve");
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o644
    );
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    fs::hard_link(&path, root.0.join("linked")).unwrap();
    assert!(matches!(
        open_private_append(&root.0, "asura.log"),
        Err(Error::UnsafeRuntime)
    ));
    fs::remove_file(&path).unwrap();
    let name = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
    // SAFETY: terminated scratch path; creates only the fixture FIFO with restrictive mode.
    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
    let start = Instant::now();
    assert!(open_private_append(&root.0, "asura.log").is_err());
    assert!(start.elapsed() < Duration::from_secs(1));
    fs::remove_file(path).unwrap();
    fs::set_permissions(&root.0, fs::Permissions::from_mode(0o777)).unwrap();
    assert!(matches!(
        open_private_append(&root.0, "asura.log"),
        Err(Error::UnsafeRuntime)
    ));
    fs::set_permissions(&root.0, fs::Permissions::from_mode(0o755)).unwrap();
    open_private_append(&root.0, "asura.log").unwrap();
    assert_eq!(
        fs::metadata(&root.0).unwrap().permissions().mode() & 0o777,
        0o755
    );
}

fn authority_fixture(root: &Scratch, bytes: &[u8]) -> RuntimeDirectory {
    let runtime = RuntimeDirectory::scratch(&root.0, true).unwrap();
    let directory = root.0.join(".asura/state/control");
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&directory)
        .unwrap();
    let journal = directory.join("slot-0.log");
    fs::write(&journal, bytes).unwrap();
    fs::set_permissions(&journal, fs::Permissions::from_mode(0o600)).unwrap();
    runtime
}
fn authority_scan(
    runtime: &RuntimeDirectory,
) -> std::result::Result<AuthorityScan, AuthorityError> {
    runtime.authority_source(
        Instant::now() + Duration::from_secs(2),
        &std::sync::atomic::AtomicBool::new(false),
    )
}
#[test]
fn authority_runtime_logs_remnants_and_unsupported_layout() {
    let root = Scratch::new();
    let runtime = RuntimeDirectory::scratch(&root.0, true).unwrap();
    assert!(matches!(
        authority_scan(&runtime).unwrap().source,
        AuthoritySource::RuntimeOnly
    ));
    fs::DirBuilder::new()
        .mode(0o755)
        .create(root.0.join(".asura/logs"))
        .unwrap();
    assert!(matches!(
        authority_scan(&runtime).unwrap().source,
        AuthoritySource::RuntimeOnly
    ));
    let config = root.0.join(".asura/config.yaml");
    fs::write(&config, b"model: system\n").unwrap();
    fs::set_permissions(&config, fs::Permissions::from_mode(0o600)).unwrap();
    assert!(matches!(
        authority_scan(&runtime).unwrap().source,
        AuthoritySource::RuntimeOnly
    ));
    let unknown = root.0.join(".asura/unknown");
    fs::write(&unknown, b"preserved").unwrap();
    fs::set_permissions(&unknown, fs::Permissions::from_mode(0o600)).unwrap();
    assert!(matches!(
        authority_scan(&runtime).unwrap().source,
        AuthoritySource::Remnants
    ));
    fs::remove_file(unknown).unwrap();
    let directory = root.0.join(".asura/state/control");
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&directory)
        .unwrap();
    assert!(matches!(
        authority_scan(&runtime).unwrap().source,
        AuthoritySource::Remnants
    ));
    fs::write(directory.join("slot-1.log"), b"do not select").unwrap();
    assert!(matches!(
        authority_scan(&runtime).unwrap().source,
        AuthoritySource::UnsupportedLayout
    ));
    assert_eq!(
        fs::read(directory.join("slot-1.log")).unwrap(),
        b"do not select"
    );
}
#[test]
fn finder_metadata_preserves_fresh_and_initialized_authority() {
    for initialized in [false, true] {
        let root = Scratch::new();
        let runtime = if initialized {
            authority_fixture(&root, b"journal bytes")
        } else {
            RuntimeDirectory::scratch(&root.0, true).unwrap()
        };
        let path = root.0.join(".asura/.DS_Store");
        fs::write(&path, b"Finder metadata bytes").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        let scan = authority_scan(&runtime).unwrap();
        match scan.source {
            AuthoritySource::RuntimeOnly => assert!(!initialized),
            AuthoritySource::Journal(mut reader) => {
                assert!(initialized);
                assert!(!reader.has_unknown_root_content());
                assert_eq!(
                    reader
                        .read_all(
                            Instant::now() + Duration::from_secs(1),
                            &std::sync::atomic::AtomicBool::new(false),
                        )
                        .unwrap(),
                    b"journal bytes"
                );
            }
            _ => panic!("Finder metadata changed authority classification"),
        }
        scan.witness.validate().unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"Finder metadata bytes");
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o644
        );
        // Content has no authority; descriptor identity and access checks remain live.
        fs::write(&path, b"updated Finder metadata").unwrap();
        scan.witness.validate().unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o666)).unwrap();
        assert_eq!(scan.witness.validate(), Err(AuthorityError::Unsafe));
    }
}

#[test]
fn finder_metadata_rejects_unsafe_types_permissions_and_links() {
    for variant in 0..5 {
        let root = Scratch::new();
        let runtime = RuntimeDirectory::scratch(&root.0, true).unwrap();
        let path = root.0.join(".asura/.DS_Store");
        match variant {
            0 => symlink(root.0.join("outside"), &path).unwrap(),
            1 => fs::DirBuilder::new().mode(0o700).create(&path).unwrap(),
            _ => {
                fs::write(&path, b"preserve").unwrap();
                let mode = match variant {
                    2 => 0o646,
                    3 => 0o664,
                    _ => 0o644,
                };
                fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
                if variant == 4 {
                    fs::hard_link(&path, root.0.join("linked-metadata")).unwrap();
                }
            }
        }
        assert!(
            matches!(authority_scan(&runtime), Err(AuthorityError::Unsafe)),
            "variant {variant}"
        );
        assert!(fs::symlink_metadata(&path).is_ok());
    }
}

#[test]
fn finder_metadata_exception_does_not_accept_other_unknown_names() {
    let root = Scratch::new();
    let runtime = RuntimeDirectory::scratch(&root.0, true).unwrap();
    let path = root.0.join(".asura/.DS_Store.backup");
    fs::write(&path, b"preserve").unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    assert!(matches!(
        authority_scan(&runtime),
        Err(AuthorityError::Unsafe)
    ));
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    assert!(matches!(
        authority_scan(&runtime).unwrap().source,
        AuthoritySource::Remnants
    ));
    assert_eq!(fs::read(&path).unwrap(), b"preserve");
}

#[test]
fn authority_reads_exact_bytes_and_preserves_known_model_areas() {
    let root = Scratch::new();
    let bytes = include_bytes!("../../asura-storage/tests/fixtures/pending.bin");
    let runtime = authority_fixture(&root, bytes);
    fs::DirBuilder::new()
        .mode(0o700)
        .create(root.0.join(".asura/data"))
        .unwrap();
    let scan = authority_scan(&runtime).unwrap();
    let AuthoritySource::Journal(mut reader) = scan.source else {
        panic!("journal missing");
    };
    assert!(!reader.has_unknown_root_content());
    assert_eq!(
        reader
            .read_all(
                Instant::now() + Duration::from_secs(1),
                &std::sync::atomic::AtomicBool::new(false)
            )
            .unwrap(),
        bytes
    );
    scan.witness.validate().unwrap();
    let path = root.0.join(".asura/state/control/slot-0.log");
    assert_eq!(fs::read(path).unwrap(), bytes);
    let unknown = root.0.join(".asura/unknown");
    fs::write(&unknown, b"preserve").unwrap();
    fs::set_permissions(&unknown, fs::Permissions::from_mode(0o600)).unwrap();
    let scan = authority_scan(&runtime).unwrap();
    let AuthoritySource::Journal(reader) = scan.source else {
        panic!("journal missing");
    };
    assert!(reader.has_unknown_root_content());
}
#[test]
fn queued_authority_witnesses_reject_namespace_and_file_changes() {
    let root = Scratch::new();
    let runtime = RuntimeDirectory::scratch(&root.0, true).unwrap();
    let scan = authority_scan(&runtime).unwrap();
    fs::DirBuilder::new()
        .mode(0o700)
        .create(root.0.join(".asura/state"))
        .unwrap();
    assert_eq!(scan.witness.validate(), Err(AuthorityError::Changed));
    let scan = authority_scan(&runtime).unwrap();
    fs::DirBuilder::new()
        .mode(0o700)
        .create(root.0.join(".asura/state/control"))
        .unwrap();
    assert_eq!(scan.witness.validate(), Err(AuthorityError::Changed));
    let root = Scratch::new();
    let runtime = authority_fixture(&root, b"journal bytes");
    let scan = authority_scan(&runtime).unwrap();
    let AuthoritySource::Journal(reader) = scan.source else {
        panic!("journal missing");
    };
    let path = root.0.join(".asura/state/control/slot-0.log");
    fs::write(&path, b"longer changed journal bytes").unwrap();
    assert_eq!(reader.validate(), Err(AuthorityError::Changed));
    assert_eq!(scan.witness.validate(), Err(AuthorityError::Changed));
    let scan = authority_scan(&runtime).unwrap();
    fs::rename(&path, root.0.join("old-journal")).unwrap();
    fs::write(&path, b"new inode").unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    assert_eq!(scan.witness.validate(), Err(AuthorityError::Changed));
}
#[test]
fn authority_unsafe_links_modes_and_nonregular_files_are_rejected() {
    for variant in 0..4 {
        let root = Scratch::new();
        let runtime = authority_fixture(&root, b"preserve");
        let path = root.0.join(".asura/state/control/slot-0.log");
        match variant {
            0 => fs::hard_link(&path, root.0.join("linked-journal")).unwrap(),
            1 => fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap(),
            2 => {
                fs::rename(&path, root.0.join("saved-journal")).unwrap();
                symlink(root.0.join("saved-journal"), &path).unwrap();
            }
            _ => {
                fs::remove_file(&path).unwrap();
                let name = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
                // SAFETY: fixed private scratch path and restrictive fixture mode.
                assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
            }
        }
        assert!(
            matches!(authority_scan(&runtime), Err(AuthorityError::Unsafe)),
            "variant {variant}"
        );
    }
}
#[test]
fn authority_size_census_deadline_and_cancellation_are_bounded() {
    let root = Scratch::new();
    let runtime = authority_fixture(&root, b"");
    let path = root.0.join(".asura/state/control/slot-0.log");
    fs::OpenOptions::new()
        .write(true)
        .open(&path)
        .unwrap()
        .set_len(8 * 1024 * 1024 + 1)
        .unwrap();
    let scan = authority_scan(&runtime).unwrap();
    let AuthoritySource::Journal(mut reader) = scan.source else {
        panic!("journal missing");
    };
    assert_eq!(
        reader.read_all(
            Instant::now() + Duration::from_secs(1),
            &std::sync::atomic::AtomicBool::new(false)
        ),
        Err(AuthorityError::Limit)
    );
    assert!(matches!(
        runtime.authority_source(Instant::now(), &std::sync::atomic::AtomicBool::new(false)),
        Err(AuthorityError::Timeout)
    ));
    assert!(matches!(
        runtime.authority_source(
            Instant::now() + Duration::from_secs(1),
            &std::sync::atomic::AtomicBool::new(true)
        ),
        Err(AuthorityError::Cancelled)
    ));
    for index in 0..64 {
        fs::write(root.0.join(format!(".asura/entry-{index}")), b"").unwrap();
    }
    assert!(matches!(
        authority_scan(&runtime),
        Err(AuthorityError::Limit)
    ));
}

#[test]
fn concurrent_socket_binding_preserves_other_threads_creation_modes() {
    use std::os::unix::fs::OpenOptionsExt;
    use std::sync::{Arc, Barrier};
    let root = Scratch::new();
    let runtime = RuntimeDirectory::scratch(&root.0, true).unwrap();
    let mut owner = runtime.acquire_owner().unwrap();
    let baseline = root.0.join("baseline");
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o666)
        .open(&baseline)
        .unwrap();
    let expected_file_mode = fs::metadata(&baseline).unwrap().permissions().mode() & 0o777;
    let barrier = Arc::new(Barrier::new(2));
    let other = barrier.clone();
    let creation_root = root.0.clone();
    let creator = std::thread::spawn(move || {
        other.wait();
        for n in 0..256 {
            let dir = creation_root.join(format!("parallel-{n}"));
            fs::DirBuilder::new().mode(0o700).create(&dir).unwrap();
            assert_eq!(
                fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
                0o700
            );
            fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o666)
                .open(dir.join("file"))
                .unwrap();
            assert_eq!(
                fs::metadata(dir.join("file")).unwrap().permissions().mode() & 0o777,
                expected_file_mode
            );
        }
    });
    barrier.wait();
    for _ in 0..128 {
        let listener = owner.bind().unwrap();
        assert_eq!(
            fs::metadata(root.0.join(".asura/run/control.sock"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        drop(listener);
        owner.remove_endpoint().unwrap();
    }
    creator.join().unwrap();
}
