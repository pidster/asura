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
}

#[cfg(not(any(test, feature = "test-support")))]
pub(crate) fn append_point(_: &RuntimeDirectory) -> Option<Fault> {
    None
}
#[cfg(not(any(test, feature = "test-support")))]
pub(crate) fn read_point(_: &RuntimeDirectory, _: &AtomicBool, _: bool) -> Result<()> {
    Ok(())
}

#[cfg(any(test, feature = "test-support"))]
pub(crate) use support::{append_point, read_point};

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
    }
    static GATES: Mutex<Vec<Entry>> = Mutex::new(Vec::new());
    static NEXT: AtomicU64 = AtomicU64::new(1);

    fn run_path(home: &Path) -> PathBuf {
        home.join(".asura/run")
    }
    /// Waits on the storage worker while the matching gate holds. This is a
    /// test harness wait; production builds never compile it.
    #[derive(Clone, Copy, PartialEq)]
    enum Point {
        Append,
        Read,
        WriterRead,
    }
    fn wait(runtime: &RuntimeDirectory, point: Point, cancel: Option<&AtomicBool>) -> bool {
        let mut counted: Option<Arc<AtomicUsize>> = None;
        loop {
            let held = {
                let gates = GATES.lock().unwrap_or_else(|e| e.into_inner());
                gates
                    .iter()
                    .find(|entry| entry.run == runtime.run_path())
                    .map(|entry| {
                        let held = match point {
                            Point::Append => entry.appends.held(),
                            Point::Read => entry.reads.held(),
                            Point::WriterRead => entry.reads.held() || entry.writer_reads.held(),
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
        wait(runtime, Point::Append, None);
        let gates = GATES.lock().unwrap_or_else(|e| e.into_inner());
        gates
            .iter()
            .find(|entry| entry.run == runtime.run_path())
            .and_then(|entry| entry.fault.lock().unwrap_or_else(|e| e.into_inner()).take())
    }
    /// `writer` marks the journal writer's own replay read, which the
    /// writer-read hold also stops; inspection reads ignore that hold.
    pub(crate) fn read_point(
        runtime: &RuntimeDirectory,
        cancel: &AtomicBool,
        writer: bool,
    ) -> Result<()> {
        let point = if writer {
            Point::WriterRead
        } else {
            Point::Read
        };
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
            };
            GATES.lock().unwrap_or_else(|e| e.into_inner()).push(Entry {
                id: gate.id,
                run: run_path(home),
                appends: Hold::Flag(gate.appends.clone()),
                reads: Hold::Flag(gate.reads.clone()),
                writer_reads: Hold::Flag(gate.writer_reads.clone()),
                fault: gate.fault.clone(),
                waiting: gate.waiting.clone(),
            });
            gate
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
            GATES
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .retain(|entry| entry.id != self.id);
        }
    }

    /// Child-process gate for a scratch home: appends wait while
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
        });
    }
}
