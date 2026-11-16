//! Test and bench glue: the admission crate's `SolverPipe` over the
//! platform's real solver process, in a scratch runtime under
//! `/private/tmp` (never the account's Asura home). The service has its
//! own bridge; this one serves the crate's real-solver tests and the bench.
use asura_platform::RuntimeDirectory;
use asura_platform::solver::{
    ExchangeEnd, Flow, SolverCancel, SolverExit, SolverLimits, SolverPackage, SolverProcess, Stream,
};
use asura_policy_admission::{PipeEnd, PipeFlow, PipeStream, SolverPipe};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

/// The development package: `<Cargo profile directory>/solver`, the
/// `solver/` of `tools/release/solver/build`, copied there once.
pub fn package_directory() -> PathBuf {
    let exe = std::env::current_exe().expect("test executable");
    asura_platform::solver::cargo_profile_directory(&exe)
        .expect("profile directory")
        .join("solver")
}

/// The package, or a test failure: an absent solver is a gap, never a
/// pass (PL-I6).
pub fn package() -> SolverPackage {
    package_at(&package_directory())
}

/// The package in `dir`, or a failure that names it.
pub fn package_at(dir: &Path) -> SolverPackage {
    SolverPackage::read(dir).unwrap_or_else(|e| {
        panic!(
            "the development solver package is required at {} (tools/release/solver/build, then copy solver/ there): {e}",
            dir.display()
        )
    })
}

/// A private scratch home with its runtime; removed on drop.
pub struct Scratch {
    pub root: PathBuf,
    pub runtime: RuntimeDirectory,
}
impl Scratch {
    pub fn new(tag: &str) -> Self {
        let root = PathBuf::from(format!(
            "/private/tmp/asura-admission-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
        ));
        std::fs::create_dir(&root).expect("scratch root");
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700))
            .expect("scratch mode");
        let runtime = RuntimeDirectory::scratch(&root, true).expect("scratch runtime");
        Self { root, runtime }
    }
}
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// One solver process as a `SolverPipe`.
pub struct Bridge {
    process: Option<SolverProcess>,
}
impl Bridge {
    pub fn start(scratch: &Scratch, package: &SolverPackage) -> Self {
        let cancel = SolverCancel::new().expect("cancel pipe");
        let process =
            SolverProcess::start(&scratch.runtime, package, SolverLimits::default(), &cancel)
                .expect("solver starts");
        Self {
            process: Some(process),
        }
    }
    /// Closes the input and reaps the process.
    pub fn finish(mut self) -> SolverExit {
        self.process.take().expect("process").finish()
    }
    #[allow(dead_code, reason = "the tests use it; the bench does not")]
    pub fn copy_directory(&self) -> &Path {
        self.process.as_ref().expect("process").copy_directory()
    }
}
impl SolverPipe for Bridge {
    fn exchange(
        &mut self,
        input: &[u8],
        output: &mut dyn FnMut(PipeStream, &[u8]) -> PipeFlow,
    ) -> Result<(), PipeEnd> {
        let Some(process) = self.process.as_mut() else {
            return Err(PipeEnd::Closed);
        };
        process
            .exchange(input, &mut |stream, bytes| {
                let stream = match stream {
                    Stream::Stdout => PipeStream::Stdout,
                    Stream::Stderr => PipeStream::Stderr,
                };
                match output(stream, bytes) {
                    PipeFlow::More => Flow::More,
                    PipeFlow::Answered => Flow::Answered,
                    PipeFlow::Fault => Flow::Fault,
                }
            })
            .map_err(|end| match end {
                ExchangeEnd::Closed => PipeEnd::Closed,
                ExchangeEnd::Cancelled => PipeEnd::Cancelled,
                ExchangeEnd::MemoryBound => PipeEnd::MemoryBound,
                ExchangeEnd::Fault => PipeEnd::Fault,
                ExchangeEnd::Io(_) => PipeEnd::Io,
            })
    }
}
