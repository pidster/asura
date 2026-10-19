//! J1-0 append and read waits: no time bound; only real errors fence.
use super::gate::Fault;
use super::gate::support::JournalGate;
use super::*;
use std::os::unix::fs::DirBuilderExt;
use std::path::PathBuf;
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Self {
        let random = u128::from_ne_bytes(crate::random_id());
        let path = PathBuf::from(format!(
            "/private/tmp/asura-j10-{}-{random:x}",
            std::process::id()
        ));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&path)
            .unwrap();
        Self(path)
    }
    fn runtime(&self) -> RuntimeDirectory {
        RuntimeDirectory::scratch(&self.0, true).unwrap()
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A fake file whose write and sync each wait for an explicit release.
struct Stalled {
    release: Mutex<mpsc::Receiver<()>>,
    written: Mutex<Vec<u8>>,
    write_error: bool,
    sync_error: bool,
}
impl AppendTarget for Stalled {
    fn write_at(&self, bytes: &[u8], _: u64) -> std::io::Result<usize> {
        self.release.lock().unwrap().recv().unwrap();
        if self.write_error {
            return Err(std::io::Error::from_raw_os_error(libc::EIO));
        }
        // A short write: the loop must advance the same offset.
        let n = bytes.len().min(3);
        self.written.lock().unwrap().extend_from_slice(&bytes[..n]);
        Ok(n)
    }
    fn full_sync(&self) -> Result<()> {
        self.release.lock().unwrap().recv().unwrap();
        if self.sync_error {
            return Err(std::io::Error::from_raw_os_error(libc::EIO).into());
        }
        Ok(())
    }
}
fn stalled(write_error: bool, sync_error: bool) -> (Arc<Stalled>, mpsc::Sender<()>) {
    let (release, receiver) = mpsc::channel();
    (
        Arc::new(Stalled {
            release: Mutex::new(receiver),
            written: Mutex::new(Vec::new()),
            write_error,
            sync_error,
        }),
        release,
    )
}

#[test]
fn j1_0_01_stalled_write_and_sync_succeed_after_release() {
    let (target, release) = stalled(false, false);
    let worker = {
        let target = target.clone();
        std::thread::spawn(move || write_and_sync(&*target, 0, b"journal"))
    };
    // Hold the write and sync past the removed 2 s append cap.
    std::thread::sleep(Duration::from_millis(2100));
    assert!(!worker.is_finished(), "a stalled write must keep waiting");
    for _ in 0..4 {
        release.send(()).unwrap();
    }
    assert!(worker.join().unwrap().is_ok());
    assert_eq!(target.written.lock().unwrap().as_slice(), b"journal");
}

fn open(scratch: &Scratch) -> JournalDirectory {
    let mut directory =
        JournalDirectory::open(scratch.runtime(), true, Arc::new(AtomicBool::new(false))).unwrap();
    directory.create(JournalName::Segment(1)).unwrap();
    directory
}
fn read(directory: &mut JournalDirectory, size: usize) -> Vec<u8> {
    let mut bytes = vec![0; size];
    directory
        .read_at(JournalName::Segment(1), 0, &mut bytes)
        .unwrap();
    bytes
}

#[test]
fn j1_0_01_gated_real_append_past_two_seconds_commits_and_does_not_fence() {
    let scratch = Scratch::new();
    let gate = JournalGate::new(&scratch.0);
    let mut directory = open(&scratch);
    gate.hold_appends();
    let worker = std::thread::spawn(move || {
        let result = directory.append(1, 0, b"first");
        (directory, result)
    });
    while gate.waiting() == 0 {
        std::thread::yield_now();
    }
    std::thread::sleep(Duration::from_millis(2100));
    assert!(!worker.is_finished());
    gate.release_appends();
    let (mut directory, result) = worker.join().unwrap();
    assert!(result.is_ok());
    directory.append(1, 5, b"second").unwrap();
    assert_eq!(read(&mut directory, 11), b"firstsecond");
}

#[test]
fn j1_0_04_gated_read_returns_only_on_release_or_cancel() {
    let scratch = Scratch::new();
    let gate = JournalGate::new(&scratch.0);
    let mut directory = open(&scratch);
    directory.append(1, 0, b"bytes").unwrap();
    drop(directory);
    gate.hold_reads();
    let cancel = Arc::new(AtomicBool::new(false));
    let reader = {
        let (runtime, cancel) = (scratch.runtime(), cancel.clone());
        std::thread::spawn(move || {
            let mut directory = JournalDirectory::open_read_only(runtime, cancel).unwrap();
            let mut bytes = [0; 5];
            directory
                .read_at(JournalName::Segment(1), 0, &mut bytes)
                .map(|_| bytes)
        })
    };
    while gate.waiting() == 0 {
        std::thread::yield_now();
    }
    std::thread::sleep(Duration::from_millis(200));
    assert!(!reader.is_finished(), "a held read must keep waiting");
    cancel.store(true, Ordering::Release);
    assert!(reader.join().unwrap().is_err(), "cancel stops the read");
    let reader = {
        let runtime = scratch.runtime();
        std::thread::spawn(move || {
            let mut directory =
                JournalDirectory::open_read_only(runtime, Arc::new(AtomicBool::new(false)))
                    .unwrap();
            let mut bytes = [0; 5];
            directory
                .read_at(JournalName::Segment(1), 0, &mut bytes)
                .map(|_| bytes)
        })
    };
    while gate.waiting() == 0 {
        std::thread::yield_now();
    }
    gate.release_reads();
    assert_eq!(&reader.join().unwrap().unwrap(), b"bytes");
}

#[test]
fn j1_0_08_write_and_sync_errors_are_reported() {
    for (write_error, sync_error) in [(true, false), (false, true)] {
        let (target, release) = stalled(write_error, sync_error);
        for _ in 0..4 {
            release.send(()).unwrap();
        }
        assert!(write_and_sync(&*target, 0, b"abc").is_err());
    }
}

#[test]
fn j1_0_08_injected_real_file_errors_are_reported_and_sj_f16_no_space_is_unchanged() {
    for fault in [Fault::Write, Fault::Sync, Fault::NoSpace] {
        let scratch = Scratch::new();
        let gate = JournalGate::new(&scratch.0);
        let mut directory = open(&scratch);
        directory.append(1, 0, b"ok").unwrap();
        gate.inject(fault);
        let result = directory.append(1, 2, b"failed");
        match fault {
            Fault::NoSpace => {
                assert_eq!(result, Err(JournalError::NoSpace { unchanged: true }));
                // No byte reached the file; the next append continues at 2.
                directory.append(1, 2, b"next").unwrap();
                assert_eq!(read(&mut directory, 6), b"oknext");
            }
            _ => assert!(result.is_err()),
        }
    }
}

#[test]
fn sj01_names_parse_exactly() {
    let valid = [
        ("seg-0000000000000001.log", JournalName::Segment(1)),
        ("ckpt-00000000000000ff.chk", JournalName::Checkpoint(255)),
        (
            "run-0000000000000001-0000000000000010.idx",
            JournalName::Run(1, 16),
        ),
        ("ckpt-0000000000000002.tmp", JournalName::CheckpointTmp(2)),
        (
            "run-0000000000000001-0000000000000002.tmp",
            JournalName::RunTmp(1, 2),
        ),
        ("lock", JournalName::Lock),
    ];
    for (text, name) in valid {
        assert_eq!(JournalName::parse(text.as_bytes()), Some(name));
        assert_eq!(name.text(), text);
    }
    for text in [
        "slot-0.log",
        "seg-0000000000000001.LOG",
        "seg-000000000000001.log",
        "seg-00000000000000001.log",
        "seg-000000000000000A.log",
        "seg-0000000000000001.log.bak",
        "ckpt-0000000000000001.idx",
        "run-0000000000000001.idx",
        "run-0000000000000001-0000000000000002-0000000000000003.idx",
        "Lock",
        "",
    ] {
        assert_eq!(JournalName::parse(text.as_bytes()), None, "{text}");
    }
}

#[test]
fn sj_f18_census_rejects_unknown_names_links_modes_and_owners() {
    use std::os::unix::fs::PermissionsExt;
    let scratch = Scratch::new();
    let mut directory = open(&scratch);
    directory.append(1, 0, b"frame").unwrap();
    let names: Vec<_> = directory
        .census()
        .unwrap()
        .into_iter()
        .map(|e| e.name)
        .collect();
    assert_eq!(names, [JournalName::Segment(1), JournalName::Lock]);
    let control = scratch.0.join(".asura/state/control");
    let write = |name: &str, mode: u32| {
        let path = control.join(name);
        std::fs::write(&path, b"x").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).unwrap();
        path
    };
    let path = write("slot-0.log", 0o600);
    assert_eq!(directory.census(), Err(JournalError::UnsupportedLayout));
    std::fs::remove_file(path).unwrap();
    let path = write("ckpt-0000000000000001.chk", 0o644);
    assert_eq!(directory.census(), Err(JournalError::Unsafe));
    std::fs::remove_file(&path).unwrap();
    std::os::unix::fs::symlink(control.join("lock"), &path).unwrap();
    assert_eq!(directory.census(), Err(JournalError::Unsafe));
    std::fs::remove_file(&path).unwrap();
    let path = write("ckpt-0000000000000001.chk", 0o600);
    std::fs::hard_link(&path, scratch.0.join("extra-link")).unwrap();
    assert_eq!(directory.census(), Err(JournalError::Unsafe));
    std::fs::remove_file(scratch.0.join("extra-link")).unwrap();
    assert_eq!(directory.census().unwrap().len(), 3);
    // A segment file is never removed; derived files are.
    assert_eq!(
        directory.remove(JournalName::Segment(1)),
        Err(JournalError::Unsafe)
    );
    directory.remove(JournalName::Checkpoint(1)).unwrap();
    assert!(!path.exists());
}

#[test]
fn sj_f19_second_writer_is_owner_busy_and_tmp_publish_is_exclusive() {
    let scratch = Scratch::new();
    let mut directory = open(&scratch);
    assert!(matches!(
        JournalDirectory::open(scratch.runtime(), false, Arc::new(AtomicBool::new(false))),
        Err(JournalError::OwnerBusy)
    ));
    directory.create(JournalName::CheckpointTmp(3)).unwrap();
    assert!(directory.create(JournalName::CheckpointTmp(3)).is_err());
    directory
        .write_at(JournalName::CheckpointTmp(3), 0, b"state")
        .unwrap();
    directory.sync(JournalName::CheckpointTmp(3)).unwrap();
    assert_eq!(
        directory.publish(JournalName::CheckpointTmp(3), JournalName::Checkpoint(4)),
        Err(JournalError::Unsafe)
    );
    directory
        .publish(JournalName::CheckpointTmp(3), JournalName::Checkpoint(3))
        .unwrap();
    assert_eq!(directory.size(JournalName::Checkpoint(3)).unwrap(), 5);
    assert!(directory.free_bytes().unwrap() > 0);
    // Read-only views never write.
    let mut reader =
        JournalDirectory::open_read_only(scratch.runtime(), Arc::new(AtomicBool::new(false)))
            .unwrap();
    assert_eq!(
        reader.create(JournalName::CheckpointTmp(9)),
        Err(JournalError::Unsafe)
    );
    drop(directory);
    // The lock is released with the writer.
    JournalDirectory::open(scratch.runtime(), false, Arc::new(AtomicBool::new(false))).unwrap();
}
