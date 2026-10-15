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

#[test]
fn j1_0_01_gated_real_append_past_two_seconds_commits_and_does_not_fence() {
    let scratch = Scratch::new();
    let gate = JournalGate::new(&scratch.0);
    let mut file = JournalFile::open(scratch.runtime(), true).unwrap();
    gate.hold_appends();
    let worker = std::thread::spawn(move || {
        let result = file.append(0, b"first");
        (file, result)
    });
    while gate.waiting() == 0 {
        std::thread::yield_now();
    }
    std::thread::sleep(Duration::from_millis(2100));
    assert!(!worker.is_finished());
    gate.release_appends();
    let (mut file, result) = worker.join().unwrap();
    assert!(result.is_ok());
    file.append(5, b"second").unwrap();
    assert_eq!(
        file.read_all(&AtomicBool::new(false)).unwrap(),
        b"firstsecond"
    );
}

#[test]
fn j1_0_04_gated_read_returns_only_on_release_or_cancel() {
    let scratch = Scratch::new();
    let gate = JournalGate::new(&scratch.0);
    let mut file = JournalFile::open(scratch.runtime(), true).unwrap();
    file.append(0, b"bytes").unwrap();
    let file = Arc::new(file);
    gate.hold_reads();
    let cancel = Arc::new(AtomicBool::new(false));
    let reader = {
        let (file, cancel) = (file.clone(), cancel.clone());
        std::thread::spawn(move || file.read_all(&cancel))
    };
    while gate.waiting() == 0 {
        std::thread::yield_now();
    }
    std::thread::sleep(Duration::from_millis(200));
    assert!(!reader.is_finished(), "a held read must keep waiting");
    cancel.store(true, Ordering::Release);
    assert!(reader.join().unwrap().is_err(), "cancel stops the read");
    let reader = {
        let file = file.clone();
        std::thread::spawn(move || file.read_all(&AtomicBool::new(false)))
    };
    while gate.waiting() == 0 {
        std::thread::yield_now();
    }
    gate.release_reads();
    assert_eq!(reader.join().unwrap().unwrap(), b"bytes");
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
fn j1_0_08_injected_real_file_errors_fence_the_file() {
    for fault in [Fault::Write, Fault::Sync] {
        let scratch = Scratch::new();
        let gate = JournalGate::new(&scratch.0);
        let mut file = JournalFile::open(scratch.runtime(), true).unwrap();
        file.append(0, b"ok").unwrap();
        gate.inject(fault);
        assert!(file.append(2, b"failed").is_err());
        // A fenced file refuses every later append; nothing guesses the outcome.
        assert!(matches!(file.append(2, b"again"), Err(Error::Unavailable)));
    }
}
