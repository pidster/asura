//! Journal fault points for tests (J1-0). Without the `test-support` feature,
//! both points compile to inert functions and never wait or inject.
use crate::{Result, RuntimeDirectory};
use std::sync::atomic::AtomicBool;

/// One injected append failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fault {
    /// The next `write_at` returns `EIO`.
    Write,
    /// The frame is written; the next `F_FULLFSYNC` returns `EIO`.
    Sync,
    /// The next append fails with `ENOSPC` and no byte written (SJ-F16).
    NoSpace,
}

/// Named hold and crash points of the segmented journal's worker steps.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Point {
    /// Roll step 1: the run tmp file is written, before its `F_FULLFSYNC`.
    RunWrite,
    /// After the run rename, before the checkpoint tmp exists.
    RunRenamed,
    /// Roll step 2: the checkpoint tmp is written, before its `F_FULLFSYNC`.
    CheckpointSync,
    /// After the checkpoint rename and directory sync.
    CheckpointRenamed,
    /// Roll step 3: before the next segment file is created.
    SegmentCreate,
    /// After the next segment's directory entry, before its first frame.
    SegmentCreated,
    /// Before a merge step writes its pages.
    MergeStep,
    /// Before an index lookup reads run pages.
    Lookup,
}
impl Point {
    pub fn file_name(self) -> &'static str {
        match self {
            Self::RunWrite => "journal-hold-run-write",
            Self::RunRenamed => "journal-hold-run-renamed",
            Self::CheckpointSync => "journal-hold-checkpoint-sync",
            Self::CheckpointRenamed => "journal-hold-checkpoint-renamed",
            Self::SegmentCreate => "journal-hold-segment-create",
            Self::SegmentCreated => "journal-hold-segment-created",
            Self::MergeStep => "journal-hold-merge-step",
            Self::Lookup => "journal-hold-lookup",
        }
    }
}

#[cfg(not(any(test, feature = "test-support")))]
pub(crate) fn append_point(_: &RuntimeDirectory) -> Option<Fault> {
    None
}
#[cfg(not(any(test, feature = "test-support")))]
pub(crate) fn read_point(_: &RuntimeDirectory, _: &AtomicBool, _: bool) -> Result<()> {
    Ok(())
}
#[cfg(not(any(test, feature = "test-support")))]
pub(crate) fn named_point(_: &RuntimeDirectory, _: Point, _: &AtomicBool) -> Result<()> {
    Ok(())
}
#[cfg(not(any(test, feature = "test-support")))]
pub(crate) fn free_bytes(_: &RuntimeDirectory) -> Option<u64> {
    None
}

#[cfg(any(test, feature = "test-support"))]
pub(crate) use support::{append_point, free_bytes, named_point, read_point};

#[cfg(any(test, feature = "test-support"))]
#[cfg_attr(not(feature = "test-support"), allow(dead_code))]
pub mod support {
    use super::*;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    enum Hold {
        Flag(Arc<AtomicBool>),
        File(PathBuf),
    }
    impl Hold {
        fn held(&self) -> bool {
            match self {
                Self::Flag(flag) => flag.load(Ordering::Acquire),
                Self::File(path) => path.exists(),
            }
        }
    }
    struct Entry {
        id: u64,
        run: PathBuf,
        appends: Hold,
        reads: Hold,
        writer_reads: Hold,
        fault: Arc<Mutex<Option<Fault>>>,
        waiting: Arc<AtomicUsize>,
        /// Held named points: flags for in-process gates, files for child processes.
        points: Arc<Mutex<std::collections::HashSet<Point>>>,
        home: Option<PathBuf>,
        free: Arc<Mutex<Option<u64>>>,
        reached: Arc<Mutex<std::collections::HashSet<Point>>>,
    }
    impl Entry {
        fn point_held(&self, point: Point) -> bool {
            self.points
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .contains(&point)
                || self
                    .home
                    .as_ref()
                    .is_some_and(|home| home.join(point.file_name()).exists())
        }
    }
    static GATES: Mutex<Vec<Entry>> = Mutex::new(Vec::new());
    static NEXT: AtomicU64 = AtomicU64::new(1);

    fn run_path(home: &Path) -> PathBuf {
        home.join(".asura/run")
    }
    /// Waits on the storage worker while the matching gate holds. This is a
    /// test harness wait; production builds never compile it.
    #[derive(Clone, Copy, PartialEq)]
    enum Wait {
        Append,
        Read,
        WriterRead,
    }
    fn wait(runtime: &RuntimeDirectory, point: Wait, cancel: Option<&AtomicBool>) -> bool {
        let mut counted: Option<Arc<AtomicUsize>> = None;
        loop {
            let held = {
                let gates = GATES.lock().unwrap_or_else(|e| e.into_inner());
                gates
                    .iter()
                    .find(|entry| entry.run == runtime.run_path())
                    .map(|entry| {
                        let held = match point {
                            Wait::Append => entry.appends.held(),
                            Wait::Read => entry.reads.held(),
                            Wait::WriterRead => entry.reads.held() || entry.writer_reads.held(),
                        };
                        if held && counted.is_none() {
                            entry.waiting.fetch_add(1, Ordering::AcqRel);
                            counted = Some(entry.waiting.clone());
                        }
                        held
                    })
                    .unwrap_or(false)
            };
            let cancelled = cancel.is_some_and(|flag| flag.load(Ordering::Acquire));
            if !held || cancelled {
                if let Some(waiting) = counted {
                    waiting.fetch_sub(1, Ordering::AcqRel);
                }
                return !cancelled;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    pub(crate) fn append_point(runtime: &RuntimeDirectory) -> Option<Fault> {
        wait(runtime, Wait::Append, None);
        let gates = GATES.lock().unwrap_or_else(|e| e.into_inner());
        gates
            .iter()
            .find(|entry| entry.run == runtime.run_path())
            .and_then(|entry| entry.fault.lock().unwrap_or_else(|e| e.into_inner()).take())
    }
    /// Waits while the named point is held; records that it was reached.
    /// Cancel ends the wait.
    pub(crate) fn named_point(
        runtime: &RuntimeDirectory,
        point: Point,
        cancel: &AtomicBool,
    ) -> Result<()> {
        let mut counted: Option<Arc<AtomicUsize>> = None;
        loop {
            let held = {
                let gates = GATES.lock().unwrap_or_else(|e| e.into_inner());
                gates
                    .iter()
                    .find(|entry| entry.run == runtime.run_path())
                    .map(|entry| {
                        entry
                            .reached
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .insert(point);
                        if let Some(home) = &entry.home {
                            let _ = std::fs::write(
                                home.join(format!("{}.reached", point.file_name())),
                                b"",
                            );
                        }
                        let held = entry.point_held(point);
                        if held && counted.is_none() {
                            entry.waiting.fetch_add(1, Ordering::AcqRel);
                            counted = Some(entry.waiting.clone());
                        }
                        held
                    })
                    .unwrap_or(false)
            };
            let cancelled = cancel.load(Ordering::Acquire);
            if !held || cancelled {
                if let Some(waiting) = counted {
                    waiting.fetch_sub(1, Ordering::AcqRel);
                }
                return if cancelled {
                    Err(crate::Error::Unavailable)
                } else {
                    Ok(())
                };
            }
            std::thread::sleep(Duration::from_millis(1));
        }
    }
    /// A test override of free disk bytes.
    pub(crate) fn free_bytes(runtime: &RuntimeDirectory) -> Option<u64> {
        let gates = GATES.lock().unwrap_or_else(|e| e.into_inner());
        let entry = gates.iter().find(|entry| entry.run == runtime.run_path())?;
        if let Some(bytes) = *entry.free.lock().unwrap_or_else(|e| e.into_inner()) {
            return Some(bytes);
        }
        let home = entry.home.as_ref()?;
        std::fs::read_to_string(home.join("journal-free-bytes"))
            .ok()?
            .trim()
            .parse()
            .ok()
    }
    /// `writer` marks the journal writer's own replay read, which the
    /// writer-read hold also stops; inspection reads ignore that hold.
    pub(crate) fn read_point(
        runtime: &RuntimeDirectory,
        cancel: &AtomicBool,
        writer: bool,
    ) -> Result<()> {
        let point = if writer { Wait::WriterRead } else { Wait::Read };
        if wait(runtime, point, Some(cancel)) {
            Ok(())
        } else {
            Err(crate::Error::Unavailable)
        }
    }

    /// Holds or fails journal appends and reads for one scratch home.
    /// Dropping the gate releases every hold and unregisters it.
    pub struct JournalGate {
        id: u64,
        appends: Arc<AtomicBool>,
        reads: Arc<AtomicBool>,
        writer_reads: Arc<AtomicBool>,
        fault: Arc<Mutex<Option<Fault>>>,
        waiting: Arc<AtomicUsize>,
        points: Arc<Mutex<std::collections::HashSet<Point>>>,
        free: Arc<Mutex<Option<u64>>>,
        reached: Arc<Mutex<std::collections::HashSet<Point>>>,
    }
    impl JournalGate {
        pub fn new(home: &Path) -> Self {
            let gate = Self {
                id: NEXT.fetch_add(1, Ordering::Relaxed),
                appends: Arc::new(AtomicBool::new(false)),
                reads: Arc::new(AtomicBool::new(false)),
                writer_reads: Arc::new(AtomicBool::new(false)),
                fault: Arc::new(Mutex::new(None)),
                waiting: Arc::new(AtomicUsize::new(0)),
                points: Arc::default(),
                free: Arc::default(),
                reached: Arc::default(),
            };
            GATES.lock().unwrap_or_else(|e| e.into_inner()).push(Entry {
                id: gate.id,
                run: run_path(home),
                appends: Hold::Flag(gate.appends.clone()),
                reads: Hold::Flag(gate.reads.clone()),
                writer_reads: Hold::Flag(gate.writer_reads.clone()),
                fault: gate.fault.clone(),
                waiting: gate.waiting.clone(),
                points: gate.points.clone(),
                home: None,
                free: gate.free.clone(),
                reached: gate.reached.clone(),
            });
            gate
        }
        /// Holds the worker at a named point until released.
        pub fn hold(&self, point: Point) {
            self.points
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(point);
        }
        pub fn release(&self, point: Point) {
            self.points
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .remove(&point);
        }
        /// True once the worker reached the named point.
        pub fn reached(&self, point: Point) -> bool {
            self.reached
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .contains(&point)
        }
        /// Overrides the free disk bytes the journal sees; `None` reads the volume.
        pub fn set_free_bytes(&self, bytes: Option<u64>) {
            *self.free.lock().unwrap_or_else(|e| e.into_inner()) = bytes;
        }
        pub fn hold_appends(&self) {
            self.appends.store(true, Ordering::Release);
        }
        pub fn release_appends(&self) {
            self.appends.store(false, Ordering::Release);
        }
        pub fn hold_reads(&self) {
            self.reads.store(true, Ordering::Release);
        }
        pub fn release_reads(&self) {
            self.reads.store(false, Ordering::Release);
        }
        /// Holds only the journal writer's replay read, not inspection reads.
        pub fn hold_writer_reads(&self) {
            self.writer_reads.store(true, Ordering::Release);
        }
        pub fn release_writer_reads(&self) {
            self.writer_reads.store(false, Ordering::Release);
        }
        /// Fails the next append on this home with the given fault.
        pub fn inject(&self, fault: Fault) {
            *self.fault.lock().unwrap_or_else(|e| e.into_inner()) = Some(fault);
        }
        /// Appends or reads currently held at this gate.
        pub fn waiting(&self) -> usize {
            self.waiting.load(Ordering::Acquire)
        }
    }
    impl Drop for JournalGate {
        fn drop(&mut self) {
            self.release_appends();
            self.release_reads();
            self.release_writer_reads();
            self.points
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clear();
            GATES
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .retain(|entry| entry.id != self.id);
        }
    }

    /// Child-process gate for a scratch home: named points wait while
    /// `HOME/<Point::file_name>` exists and write `<name>.reached` when
    /// reached; `HOME/journal-free-bytes` overrides free disk bytes. Appends wait while
    /// `HOME/journal-hold-appends` exists and reads wait while
    /// `HOME/journal-hold-reads` exists; the writer's replay read also waits
    /// while `HOME/journal-hold-writer-reads` exists. It lasts for the process
    /// lifetime.
    pub fn install_file_gate(home: &Path) {
        GATES.lock().unwrap_or_else(|e| e.into_inner()).push(Entry {
            id: NEXT.fetch_add(1, Ordering::Relaxed),
            run: run_path(home),
            appends: Hold::File(home.join("journal-hold-appends")),
            reads: Hold::File(home.join("journal-hold-reads")),
            writer_reads: Hold::File(home.join("journal-hold-writer-reads")),
            fault: Arc::new(Mutex::new(None)),
            waiting: Arc::new(AtomicUsize::new(0)),
            points: Arc::default(),
            home: Some(home.to_owned()),
            free: Arc::default(),
            reached: Arc::default(),
        });
    }
}
