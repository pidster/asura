use asura_platform::RuntimeDirectory;
use asura_storage::stored_memory::{SizeError, stored_bytes};
use std::{
    fs,
    os::unix::fs::DirBuilderExt,
    path::PathBuf,
    sync::atomic::AtomicBool,
    time::{Duration, Instant},
};
struct Scratch(PathBuf);
impl Scratch {
    fn new() -> Self {
        let path = PathBuf::from(format!(
            "/private/tmp/asura-memory-{:x}",
            u128::from_ne_bytes(asura_platform::random_id())
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
fn footprint_missing_database_is_unknown_and_never_created() {
    let scratch = Scratch::new();
    let runtime = RuntimeDirectory::scratch(&scratch.0, true).unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    assert_eq!(
        stored_bytes(runtime.clone(), deadline, &AtomicBool::new(false)),
        Err(SizeError::Unavailable)
    );
    assert!(!scratch.0.join(".asura/db").exists());
    assert_eq!(
        stored_bytes(runtime, deadline, &AtomicBool::new(true)),
        Err(SizeError::Cancelled)
    );
}
#[cfg(feature = "embedded-memory")]
#[test]
fn footprint_reports_real_embedded_store_without_another_engine_owner() {
    use asura_storage::memory::{Binding, Id, Memory};
    let scratch = Scratch::new();
    let runtime = RuntimeDirectory::scratch(&scratch.0, true).unwrap();
    let binding = Binding {
        installation_id: Id::new([1; 16]).unwrap(),
        graph_id: Id::new([2; 16]).unwrap(),
        init_operation_id: Id::new([3; 16]).unwrap(),
    };
    let (mut memory, mut pending) = Memory::initialize(scratch.0.clone(), binding).unwrap();
    let end = Instant::now() + Duration::from_secs(12);
    loop {
        if let Some(result) = pending.poll() {
            result.unwrap();
            break;
        }
        assert!(Instant::now() < end);
        std::thread::sleep(Duration::from_millis(1));
    }
    let bytes = stored_bytes(
        runtime,
        Instant::now() + Duration::from_secs(2),
        &AtomicBool::new(false),
    );
    while !memory.close().unwrap() {
        assert!(Instant::now() < end);
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(bytes.unwrap() > 0);
}
