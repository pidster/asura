#![cfg(feature = "embedded-memory")]
//! HM0 real embedded tests. Every engine path is a private scratch fixture.
use asura_storage::memory::{Binding, Error, Id, Memory, Pending, PutNote, Result};
use std::{
    fs,
    os::unix::fs::{DirBuilderExt, symlink},
    path::PathBuf,
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
// HM0 permits one scratch engine per process, even when fixture paths differ.
// Serialize fixture lifetimes inside this target so default Cargo parallelism
// cannot race the production admission claim. This never retries a Busy result.
fn fixture_owner() -> std::sync::MutexGuard<'static, ()> {
    static OWNER: std::sync::Mutex<()> = std::sync::Mutex::new(());
    OWNER
        .lock()
        .expect("prior embedded fixture failed while owning the engine")
}
fn id(n: u8) -> Id {
    Id::new([n; 16]).unwrap()
}
fn binding() -> Binding {
    Binding {
        installation_id: id(1),
        graph_id: id(2),
        init_operation_id: id(3),
    }
}
fn note() -> PutNote {
    PutNote {
        binding: binding(),
        context_id: id(4),
        object_id: id(5),
        version_id: id(6),
        operation_id: id(7),
        body: "literal: '); DELETE memory_note; --\n\u{1b}[31m".into(),
        source: None,
    }
}
fn root() -> PathBuf {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let path = PathBuf::from(format!(
        "/private/tmp/asura-memory-{}-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
        NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
    path
}
fn wait<T>(mut result: Pending<T>) -> Result<T> {
    let end = Instant::now() + Duration::from_secs(12);
    loop {
        if let Some(result) = result.poll() {
            return result;
        }
        assert!(
            Instant::now() < end,
            "adapter did not return within test deadline"
        );
        thread::sleep(Duration::from_millis(5));
    }
}
fn close(memory: &mut Memory) {
    let end = Instant::now() + Duration::from_secs(10);
    loop {
        match memory.close() {
            Ok(true) => break,
            Ok(false) | Err(Error::Timeout) => (),
            Err(error) => panic!("close: {error}"),
        };
        assert!(Instant::now() < end, "owner not settled, fixture retained");
        thread::sleep(Duration::from_millis(5));
    }
}
#[test]
fn literal_documents_graph_receipts_and_reopen() {
    let _fixture_owner = fixture_owner();
    let path = root();
    let (mut memory, ready) = Memory::initialize(path.clone(), binding()).unwrap();
    assert_eq!(wait(ready).unwrap(), binding());
    let command = note();
    let first = wait(memory.put_note(command.clone()).unwrap()).unwrap();
    assert_eq!(
        wait(memory.put_note(command.clone()).unwrap()).unwrap(),
        first
    );
    let mut changed = command.clone();
    changed.body.push('x');
    assert_eq!(
        wait(memory.put_note(changed).unwrap()),
        Err(Error::IdempotencyConflict)
    );
    let mut derived = command.clone();
    derived.version_id = id(8);
    derived.operation_id = id(9);
    derived.object_id = id(10);
    derived.body = "Derived note".into();
    derived.source = Some((id(6), id(11)));
    wait(memory.put_note(derived).unwrap()).unwrap();
    let sources = wait(memory.get_sources(binding(), id(4), id(8)).unwrap()).unwrap();
    assert_eq!(sources.len(), 1);
    assert_eq!(sources[0].body, command.body);
    assert_eq!(
        wait(memory.get_note(binding(), id(12), id(6)).unwrap()),
        Err(Error::NotFound)
    );
    close(&mut memory);
    drop(memory);
    let (mut memory, ready) = Memory::open(path.clone(), binding()).unwrap();
    wait(ready).unwrap();
    assert_eq!(
        wait(
            memory
                .resolve(id(7), command.command_digest().unwrap())
                .unwrap()
        )
        .unwrap(),
        first
    );
    assert_eq!(
        wait(memory.get_note(binding(), id(4), id(6)).unwrap())
            .unwrap()
            .body,
        command.body
    );
    assert_eq!(
        wait(memory.get_sources(binding(), id(4), id(8)).unwrap())
            .unwrap()
            .len(),
        1
    );
    close(&mut memory);
    fs::remove_dir_all(path).unwrap();
}
#[test]
fn missing_database_wrong_binding_and_symlinks_never_initialize() {
    let _fixture_owner = fixture_owner();
    let path = root();
    let (mut memory, ready) = Memory::open(path.clone(), binding()).unwrap();
    assert_eq!(wait(ready), Err(Error::MissingDatabase));
    close(&mut memory);
    assert!(!path.join(".asura").exists());
    fs::DirBuilder::new()
        .mode(0o700)
        .create(path.join(".asura"))
        .unwrap();
    fs::DirBuilder::new()
        .mode(0o700)
        .create(path.join(".asura/db"))
        .unwrap();
    let junk = path.join(".asura/db/junk");
    fs::write(&junk, b"preserve this evidence").unwrap();
    let (mut memory, ready) = Memory::open(path.clone(), binding()).unwrap();
    assert_eq!(wait(ready), Err(Error::MissingDatabase));
    close(&mut memory);
    assert_eq!(fs::read(&junk).unwrap(), b"preserve this evidence");
    assert_eq!(fs::read_dir(path.join(".asura/db")).unwrap().count(), 1);
    fs::remove_file(junk).unwrap();

    let (mut memory, ready) = Memory::initialize(path.clone(), binding()).unwrap();
    wait(ready).unwrap();
    close(&mut memory);
    let mut wrong = binding();
    wrong.graph_id = id(15);
    let (mut memory, ready) = Memory::open(path.clone(), wrong).unwrap();
    assert_eq!(wait(ready), Err(Error::BindingMismatch));
    close(&mut memory);
    let link = root();
    fs::remove_dir(&link).unwrap();
    symlink(&path, &link).unwrap();
    let (mut memory, ready) = Memory::open(link.clone(), binding()).unwrap();
    assert_eq!(wait(ready), Err(Error::UnsafePath));
    close(&mut memory);
    fs::remove_file(link).unwrap();
    let manifest = path.join(".asura/db/manifest/00000000000000000000.manifest");
    fs::write(&manifest, [0u8]).unwrap();
    let (mut memory, ready) = Memory::open(path.clone(), binding()).unwrap();
    assert_eq!(wait(ready), Err(Error::InvalidRecord));
    close(&mut memory);
    assert_eq!(fs::read(&manifest).unwrap(), vec![0]);
    fs::remove_dir_all(path).unwrap();
}
struct ChildGuard(Child);
impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
#[test]
fn crash_boundaries_reconcile_original_receipt() {
    let _fixture_owner = fixture_owner();
    for phase in ["before", "submitted", "acknowledged"] {
        let path = root();
        let child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "memory_child", "--nocapture"])
            .env("ASURA_MEMORY_TEST_ROOT", &path)
            .env("ASURA_MEMORY_TEST_PHASE", phase)
            .env("SURREAL_SURREALKV_BLOCK_CACHE_CAPACITY", "33554432")
            .env("SURREAL_SURREALKV_MAX_MEMTABLE_SIZE", "67108864")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let mut child = ChildGuard(child);
        let end = Instant::now() + Duration::from_secs(15);
        while !path.join("boundary").exists() {
            assert!(
                child.0.try_wait().unwrap().is_none(),
                "child exited before boundary"
            );
            assert!(Instant::now() < end, "child boundary timeout");
            thread::sleep(Duration::from_millis(5));
        }
        child.0.kill().unwrap();
        child.0.wait().unwrap();
        let (mut memory, ready) = Memory::open(path.clone(), binding()).unwrap();
        wait(ready).unwrap();
        let command = note();
        let result = wait(
            memory
                .resolve(id(7), command.command_digest().unwrap())
                .unwrap(),
        );
        match phase {
            "before" => assert_eq!(result, Err(Error::NotFound)),
            "acknowledged" => assert!(result.is_ok()),
            _ => assert!(result.is_ok() || result == Err(Error::NotFound)),
        }
        let receipt = wait(memory.put_note(command.clone()).unwrap()).unwrap();
        assert_eq!(wait(memory.put_note(command).unwrap()).unwrap(), receipt);
        close(&mut memory);
        fs::remove_dir_all(path).unwrap();
    }
}
#[test]
fn memory_child() {
    let Some(path) = std::env::var_os("ASURA_MEMORY_TEST_ROOT") else {
        return;
    };
    let _fixture_owner = fixture_owner();
    let path = PathBuf::from(path);
    let phase = std::env::var("ASURA_MEMORY_TEST_PHASE").unwrap();
    let (memory, ready) = Memory::initialize(path.clone(), binding()).unwrap();
    wait(ready).unwrap();
    if phase != "before" {
        let operation = memory.put_note(note()).unwrap();
        if phase == "acknowledged" {
            wait(operation).unwrap();
        } else {
            std::mem::forget(operation);
        }
    }
    fs::write(path.join("boundary"), phase).unwrap();
    loop {
        thread::sleep(Duration::from_secs(1));
    }
}
