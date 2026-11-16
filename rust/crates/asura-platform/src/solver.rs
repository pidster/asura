//! The solver process owner (security-policy.md, PL3 platform solver,
//! PL3 solver memory bound and PL3-F2): a verified private copy of the
//! solver package, `sandbox-exec` with the solver profile, three pipes, an
//! exchange loop that writes a query while it drains both outputs, a
//! supervised footprint bound, cancel, kill and reap.
//!
//! Every call blocks and runs on the admission worker thread, never on the
//! reactor. No time limit ends any wait: an exchange ends on its answer,
//! the closure's fault, the end of the solver's output, cancel or the
//! memory bound. The 10 ms poll interval only samples the footprint
//! (pacing under AE-04); it ends nothing.
use crate::model_process::{PackageDirectory, PinnedCopy, bundle_digest, open_source, pinned_copy};
use crate::startup::{SpawnActions, SpawnAttributes, duplicate_for_spawn, pipe, spawn_result};
use crate::{Error, RuntimeDirectory, checked, nonblocking};
use std::ffi::CString;
use std::io;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

#[cfg(test)]
mod tests;

/// The footprint sample interval: pacing that ends nothing (AE-04).
pub const SOLVER_SAMPLE: Duration = Duration::from_millis(10);
/// The supervised footprint bound of the service (PL3 solver memory bound).
pub const SOLVER_MEMORY_BYTES: u64 = 1024 * 1024 * 1024;
/// The manifest size bound.
pub const MAX_MANIFEST_BYTES: u64 = 64 * 1024;
/// Package files the platform copies.
pub const MAX_PACKAGE_FILES: usize = 4;
/// The largest package file.
const MAX_FILE_BYTES: u64 = 128 * 1024 * 1024;
/// Bytes read from one output per read call.
const READ_CHUNK: usize = 64 * 1024;
/// `F_SETNOSIGPIPE` (`<sys/fcntl.h>`): a write to a closed pipe returns
/// `EPIPE` instead of raising `SIGPIPE`.
const F_SETNOSIGPIPE: libc::c_int = 73;
const SANDBOX_EXEC: &std::ffi::CStr = c"/usr/bin/sandbox-exec";

/// One package file from the manifest.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SolverFile {
    /// `MacOS/<name>` or `Frameworks/<name>`.
    pub path: String,
    pub bytes: u64,
    pub sha256: [u8; 32],
}

/// A solver package: a directory and the files the platform copies.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SolverPackage {
    pub directory: PathBuf,
    pub files: Vec<SolverFile>,
}

/// Why a package could not be read or a solver could not start.
#[derive(Debug)]
pub enum SolverError {
    DigestMismatch,
    CopyFailed,
    SpawnFailed,
    Cancelled,
    /// The manifest is missing, too large or malformed.
    Manifest,
    Io(io::Error),
}
impl SolverError {
    /// The `policy.solver_failed` cause code.
    pub fn cause(&self) -> &'static str {
        match self {
            Self::DigestMismatch => "digest_mismatch",
            Self::CopyFailed => "copy_failed",
            Self::SpawnFailed => "spawn_failed",
            Self::Cancelled => "cancelled",
            Self::Manifest => "unavailable",
            Self::Io(_) => "io",
        }
    }
}
impl std::fmt::Display for SolverError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.cause())
    }
}
impl std::error::Error for SolverError {}

fn valid_package_path(path: &str) -> bool {
    let Some((directory, name)) = path.split_once('/') else {
        return false;
    };
    matches!(directory, "MacOS" | "Frameworks")
        && !name.is_empty()
        && name.len() <= 64
        && name != "."
        && name != ".."
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-+".contains(&b))
}

fn parse_hex(text: &str) -> Option<[u8; 32]> {
    if text.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        let pair = text.get(i * 2..i * 2 + 2)?;
        if !pair
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return None;
        }
        *byte = u8::from_str_radix(pair, 16).ok()?;
    }
    Some(out)
}

impl SolverPackage {
    /// Reads `<directory>/manifest.json` as `tools/release/solver/build`
    /// writes it (`format_version` 1, `files` by path with `bytes` and
    /// `sha256`). Entries outside `MacOS/` and `Frameworks/` (the licence
    /// texts) are not copied and are ignored. The copied set holds 1 to 4
    /// files and `MacOS/cvc5`. Blocking file I/O.
    pub fn read(directory: &Path) -> Result<Self, SolverError> {
        use std::io::Read;
        let file = std::fs::File::open(directory.join("manifest.json"))
            .map_err(|_| SolverError::Manifest)?;
        let mut text = Vec::new();
        file.take(MAX_MANIFEST_BYTES + 1)
            .read_to_end(&mut text)
            .map_err(SolverError::Io)?;
        if text.len() as u64 > MAX_MANIFEST_BYTES {
            return Err(SolverError::Manifest);
        }
        let document: serde_json::Value =
            serde_json::from_slice(&text).map_err(|_| SolverError::Manifest)?;
        if document
            .get("format_version")
            .and_then(serde_json::Value::as_u64)
            != Some(1)
        {
            return Err(SolverError::Manifest);
        }
        let listed = document
            .get("files")
            .and_then(serde_json::Value::as_object)
            .ok_or(SolverError::Manifest)?;
        let mut files = Vec::new();
        for (path, item) in listed {
            if path.starts_with("licenses/") {
                continue;
            }
            if !valid_package_path(path) {
                return Err(SolverError::Manifest);
            }
            let bytes = item
                .get("bytes")
                .and_then(serde_json::Value::as_u64)
                .filter(|b| (1..=MAX_FILE_BYTES).contains(b))
                .ok_or(SolverError::Manifest)?;
            let sha256 = item
                .get("sha256")
                .and_then(serde_json::Value::as_str)
                .and_then(parse_hex)
                .ok_or(SolverError::Manifest)?;
            files.push(SolverFile {
                path: path.clone(),
                bytes,
                sha256,
            });
        }
        files.sort_by(|a, b| a.path.cmp(&b.path));
        if files.is_empty()
            || files.len() > MAX_PACKAGE_FILES
            || !files.iter().any(|f| f.path == "MacOS/cvc5")
        {
            return Err(SolverError::Manifest);
        }
        Ok(Self {
            directory: directory.to_path_buf(),
            files,
        })
    }
    /// The package digest: the Hub bundle digest encoding over the copied
    /// files. Kind 35 records it as the solver digest.
    pub fn digest(&self) -> [u8; 32] {
        let mut files: Vec<(String, [u8; 32])> = self
            .files
            .iter()
            .map(|f| (f.path.clone(), f.sha256))
            .collect();
        bundle_digest(&mut files)
    }
}

/// The resource limits of one solver process.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SolverLimits {
    /// The footprint above which the solver is killed.
    pub memory_bytes: u64,
}
impl Default for SolverLimits {
    fn default() -> Self {
        Self {
            memory_bytes: SOLVER_MEMORY_BYTES,
        }
    }
}

struct CancelInner {
    flag: AtomicBool,
    read: OwnedFd,
    write: OwnedFd,
}
/// A cloneable cancel handle: sets a flag and wakes the exchange loop
/// through a pipe. Safe to call from any thread, any number of times.
#[derive(Clone)]
pub struct SolverCancel(Arc<CancelInner>);
impl SolverCancel {
    pub fn new() -> io::Result<Self> {
        let (read, write) = pipe().map_err(platform_io)?;
        nonblocking(read.as_raw_fd()).map_err(platform_io)?;
        nonblocking(write.as_raw_fd()).map_err(platform_io)?;
        Ok(Self(Arc::new(CancelInner {
            flag: AtomicBool::new(false),
            read,
            write,
        })))
    }
    pub fn cancel(&self) {
        self.0.flag.store(true, Ordering::Release);
        // SAFETY: one byte from a valid buffer to an owned nonblocking pipe;
        // a full pipe already holds a wake byte, so the result is ignored.
        unsafe { libc::write(self.0.write.as_raw_fd(), [1u8].as_ptr().cast(), 1) };
    }
    pub fn is_cancelled(&self) -> bool {
        self.0.flag.load(Ordering::Acquire)
    }
    fn fd(&self) -> RawFd {
        self.0.read.as_raw_fd()
    }
}

fn platform_io(error: Error) -> io::Error {
    match error {
        Error::Io(error) => error,
        other => io::Error::other(other.to_string()),
    }
}

/// Which output a chunk came from.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Stream {
    Stdout,
    Stderr,
}
/// The closure's verdict on the output so far.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Flow {
    More,
    Answered,
    Fault,
}
/// Why an exchange ended without a complete answer.
#[derive(Debug)]
pub enum ExchangeEnd {
    /// End of standard output before the answer, or a broken input pipe.
    Closed,
    Cancelled,
    MemoryBound,
    /// The closure answered `Fault`.
    Fault,
    Io(io::Error),
}

/// How a solver process ended.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SolverExit {
    /// The exit code; `None` when a signal ended it.
    pub status: Option<i32>,
    /// The largest footprint sampled, in bytes.
    pub peak_footprint: u64,
    /// The memory bound killed it.
    pub memory_bound: bool,
}

/// The private copy: files first, then subdirectories, then the package
/// directory are removed when it drops (field order).
struct PackageCopy {
    files: Vec<PinnedCopy>,
    subdirectories: Vec<PackageDirectory>,
    _top: PackageDirectory,
    path: PathBuf,
}

/// One running solver under `sandbox-exec`.
pub struct SolverProcess {
    pid: libc::pid_t,
    reaped: Option<i32>,
    stdin: Option<OwnedFd>,
    stdout: Option<OwnedFd>,
    stderr: Option<OwnedFd>,
    cancel: SolverCancel,
    limits: SolverLimits,
    peak: u64,
    memory_bound: bool,
    copy: Option<PackageCopy>,
}

fn sandbox_profile(directory: &str) -> Result<String, SolverError> {
    let quote = |s: &str| crate::git_observer::quoted(s).map_err(|_| SolverError::SpawnFailed);
    Ok(format!(
        "(version 1)(deny default)(allow process-exec (literal {}))(allow file-read* file-map-executable (subpath {}) (subpath \"/usr/lib\") (subpath \"/System\"))(allow file-read-metadata file-read-data (literal \"/\"))",
        quote(&format!("{directory}/MacOS/cvc5"))?,
        quote(directory)?
    ))
}

fn get_path(fd: RawFd) -> io::Result<String> {
    let mut buffer = vec![0u8; libc::PATH_MAX as usize];
    // SAFETY: F_GETPATH writes at most PATH_MAX bytes into the buffer.
    if unsafe { libc::fcntl(fd, libc::F_GETPATH, buffer.as_mut_ptr()) } < 0 {
        return Err(io::Error::last_os_error());
    }
    let end = buffer.iter().position(|b| *b == 0).unwrap_or(buffer.len());
    buffer.truncate(end);
    String::from_utf8(buffer).map_err(|_| io::Error::other("path not UTF-8"))
}

fn copy_package(
    runtime: &RuntimeDirectory,
    package: &SolverPackage,
    cancel: &SolverCancel,
) -> Result<PackageCopy, SolverError> {
    let check = || {
        if cancel.is_cancelled() {
            Err(Error::Deadline)
        } else {
            Ok(())
        }
    };
    let (root, _, uid) = runtime.model_root().map_err(|_| SolverError::CopyFailed)?;
    let suffix: String = crate::random_id()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    let name = CString::new(format!("solver-{suffix}")).map_err(|_| SolverError::CopyFailed)?;
    let top = PackageDirectory::create(root, name).map_err(|_| SolverError::CopyFailed)?;
    let path = PathBuf::from(get_path(top.directory.as_raw_fd()).map_err(SolverError::Io)?);
    let mut copy = PackageCopy {
        files: Vec::new(),
        subdirectories: Vec::new(),
        _top: top,
        path,
    };
    for sub in ["MacOS", "Frameworks"] {
        let parent = copy
            ._top
            .directory
            .try_clone()
            .map_err(|_| SolverError::CopyFailed)?;
        let created = PackageDirectory::create(parent, CString::new(sub).expect("fixed name"))
            .map_err(|_| SolverError::CopyFailed)?;
        copy.subdirectories.push(created);
    }
    for file in &package.files {
        if cancel.is_cancelled() {
            return Err(SolverError::Cancelled);
        }
        let (directory, name) = file.path.split_once('/').ok_or(SolverError::CopyFailed)?;
        let index = usize::from(directory == "Frameworks");
        let parent = copy.subdirectories[index]
            .directory
            .try_clone()
            .map_err(|_| SolverError::CopyFailed)?;
        let source_path = CString::new(package.directory.join(&file.path).as_os_str().as_bytes())
            .map_err(|_| SolverError::CopyFailed)?;
        let source =
            open_source(libc::AT_FDCWD, &source_path).map_err(|_| SolverError::CopyFailed)?;
        let executable = file.path == "MacOS/cvc5";
        let (pinned, digest) = pinned_copy(
            parent,
            CString::new(name).map_err(|_| SolverError::CopyFailed)?,
            source,
            uid,
            executable,
            false,
            MAX_FILE_BYTES,
            &check,
        )
        .map_err(|e| match e {
            Error::Deadline => SolverError::Cancelled,
            _ => SolverError::CopyFailed,
        })?;
        let size = pinned.file.metadata().map_err(SolverError::Io)?.len();
        copy.files.push(pinned);
        if digest != file.sha256 || size != file.bytes {
            return Err(SolverError::DigestMismatch);
        }
    }
    Ok(copy)
}

impl SolverProcess {
    /// Copies and verifies the package into `<run>/solver-<32 hex>/`, then
    /// starts `sandbox-exec -p PROFILE <copy>/MacOS/cvc5 --lang smt` with
    /// three pipes. A failure removes every copied file.
    pub fn start(
        runtime: &RuntimeDirectory,
        package: &SolverPackage,
        limits: SolverLimits,
        cancel: &SolverCancel,
    ) -> Result<Self, SolverError> {
        if cancel.is_cancelled() {
            return Err(SolverError::Cancelled);
        }
        let copy = copy_package(runtime, package, cancel)?;
        for file in &copy.files {
            if !file.still_named().unwrap_or(false) {
                return Err(SolverError::CopyFailed);
            }
        }
        if cancel.is_cancelled() {
            return Err(SolverError::Cancelled);
        }
        let directory = copy
            .path
            .to_str()
            .ok_or(SolverError::CopyFailed)?
            .to_owned();
        let profile =
            CString::new(sandbox_profile(&directory)?).map_err(|_| SolverError::SpawnFailed)?;
        let executable = CString::new(format!("{directory}/MacOS/cvc5"))
            .map_err(|_| SolverError::SpawnFailed)?;
        let spawn = |e: Error| match e {
            Error::Io(e) => SolverError::Io(e),
            _ => SolverError::SpawnFailed,
        };
        let (stdin_read, stdin_write) = pipe().map_err(spawn)?;
        let (stdout_read, stdout_write) = pipe().map_err(spawn)?;
        let (stderr_read, stderr_write) = pipe().map_err(spawn)?;
        for fd in [&stdin_write, &stdout_read, &stderr_read] {
            nonblocking(fd.as_raw_fd()).map_err(spawn)?;
        }
        // SAFETY: sets one descriptor flag on an owned pipe end.
        checked(unsafe { libc::fcntl(stdin_write.as_raw_fd(), F_SETNOSIGPIPE, 1) })
            .map_err(spawn)?;
        let child_in = duplicate_for_spawn(stdin_read.as_raw_fd()).map_err(spawn)?;
        let child_out = duplicate_for_spawn(stdout_write.as_raw_fd()).map_err(spawn)?;
        let child_err = duplicate_for_spawn(stderr_write.as_raw_fd()).map_err(spawn)?;
        drop((stdin_read, stdout_write, stderr_write));
        // SAFETY: opaque actions are initialized before use or destruction.
        let mut actions = unsafe { std::mem::zeroed() };
        spawn_result(unsafe { libc::posix_spawn_file_actions_init(&mut actions) })
            .map_err(spawn)?;
        let mut actions = SpawnActions(actions);
        // SAFETY: the duplicated descriptors outlive the spawn; CLOEXEC
        // default closes every descriptor not named here.
        unsafe {
            for (fd, target) in [(&child_in, 0), (&child_out, 1), (&child_err, 2)] {
                spawn_result(libc::posix_spawn_file_actions_adddup2(
                    &mut actions.0,
                    fd.as_raw_fd(),
                    target,
                ))
                .map_err(spawn)?;
            }
        }
        // SAFETY: initialize opaque spawn attributes before use.
        let mut attrs = unsafe { std::mem::zeroed() };
        spawn_result(unsafe { libc::posix_spawnattr_init(&mut attrs) }).map_err(spawn)?;
        let mut attrs = SpawnAttributes(attrs);
        spawn_result(unsafe {
            libc::posix_spawnattr_setflags(&mut attrs.0, libc::POSIX_SPAWN_CLOEXEC_DEFAULT as i16)
        })
        .map_err(spawn)?;
        let mut argv = [
            SANDBOX_EXEC.as_ptr().cast_mut(),
            c"-p".as_ptr().cast_mut(),
            profile.as_ptr().cast_mut(),
            executable.as_ptr().cast_mut(),
            c"--lang".as_ptr().cast_mut(),
            c"smt".as_ptr().cast_mut(),
            std::ptr::null_mut(),
        ];
        let mut env = [
            c"LANG=C".as_ptr().cast_mut(),
            c"LC_ALL=C".as_ptr().cast_mut(),
            std::ptr::null_mut(),
        ];
        let mut pid = 0;
        // SAFETY: all pointers refer to live initialized spawn state and
        // null-terminated arrays.
        spawn_result(unsafe {
            libc::posix_spawn(
                &mut pid,
                SANDBOX_EXEC.as_ptr(),
                &actions.0,
                &attrs.0,
                argv.as_mut_ptr(),
                env.as_mut_ptr(),
            )
        })
        .map_err(|_| SolverError::SpawnFailed)?;
        drop((child_in, child_out, child_err));
        Ok(Self {
            pid,
            reaped: None,
            stdin: Some(stdin_write),
            stdout: Some(stdout_read),
            stderr: Some(stderr_read),
            cancel: cancel.clone(),
            limits,
            peak: 0,
            memory_bound: false,
            copy: Some(copy),
        })
    }

    pub fn pid(&self) -> i32 {
        self.pid
    }
    /// The private copy directory (its `F_GETPATH` spelling).
    pub fn copy_directory(&self) -> &Path {
        self.copy
            .as_ref()
            .map_or(Path::new(""), |c| c.path.as_path())
    }

    /// Samples the footprint; `true` when it is above the bound.
    fn over_bound(&mut self) -> bool {
        if self.reaped.is_some() {
            return false;
        }
        // SAFETY: zeroed plain-data output of the size the flavor writes.
        let mut info: libc::rusage_info_v2 = unsafe { std::mem::zeroed() };
        // SAFETY: an unreaped child PID; the buffer is a valid rusage_info_v2.
        let result = unsafe {
            libc::proc_pid_rusage(
                self.pid,
                libc::RUSAGE_INFO_V2,
                (&mut info as *mut libc::rusage_info_v2).cast(),
            )
        };
        if result == 0 {
            self.peak = self.peak.max(info.ri_phys_footprint);
            if info.ri_phys_footprint > self.limits.memory_bytes {
                self.memory_bound = true;
                return true;
            }
        }
        false
    }

    fn drain_cancel(&self) {
        let mut byte = [0u8; 64];
        // SAFETY: reads into a valid buffer from an owned nonblocking pipe.
        while unsafe { libc::read(self.cancel.fd(), byte.as_mut_ptr().cast(), byte.len()) } > 0 {}
    }

    /// Writes all of `input` while it passes every chunk of standard output
    /// and standard error to `output` (PL3-F2). Returns once the input is
    /// written and `output` has answered `Answered`.
    pub fn exchange(
        &mut self,
        input: &[u8],
        output: &mut dyn FnMut(Stream, &[u8]) -> Flow,
    ) -> Result<(), ExchangeEnd> {
        let mut written = 0usize;
        let mut answered = false;
        let mut buffer = vec![0u8; READ_CHUNK];
        loop {
            if self.cancel.is_cancelled() {
                return Err(ExchangeEnd::Cancelled);
            }
            if written == input.len() && answered {
                return Ok(());
            }
            let stdin = self.stdin.as_ref().map(AsRawFd::as_raw_fd);
            if stdin.is_none() && written < input.len() {
                return Err(ExchangeEnd::Closed);
            }
            let Some(stdout) = self.stdout.as_ref().map(AsRawFd::as_raw_fd) else {
                return Err(ExchangeEnd::Closed);
            };
            let mut fds = vec![
                libc::pollfd {
                    fd: self.cancel.fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: stdout,
                    events: libc::POLLIN,
                    revents: 0,
                },
            ];
            if let Some(err) = self.stderr.as_ref() {
                fds.push(libc::pollfd {
                    fd: err.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                });
            }
            if let Some(fd) = stdin.filter(|_| written < input.len()) {
                fds.push(libc::pollfd {
                    fd,
                    events: libc::POLLOUT,
                    revents: 0,
                });
            }
            // SAFETY: a valid pollfd array of the given length; the
            // timeout is the footprint sampling pace, which ends nothing.
            let ready = unsafe {
                libc::poll(
                    fds.as_mut_ptr(),
                    fds.len() as libc::nfds_t,
                    SOLVER_SAMPLE.as_millis() as libc::c_int,
                )
            };
            if ready < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(ExchangeEnd::Io(error));
            }
            if self.over_bound() {
                return Err(ExchangeEnd::MemoryBound);
            }
            if fds[0].revents != 0 {
                self.drain_cancel();
                if self.cancel.is_cancelled() {
                    return Err(ExchangeEnd::Cancelled);
                }
            }
            // Write what the pipe accepts.
            if let Some(fd) = stdin.filter(|_| written < input.len()) {
                // SAFETY: writes from the unwritten part of a valid slice to
                // an owned nonblocking descriptor.
                let n = unsafe {
                    libc::write(fd, input[written..].as_ptr().cast(), input.len() - written)
                };
                if n > 0 {
                    written += n as usize;
                } else if n < 0 {
                    let error = io::Error::last_os_error();
                    match error.raw_os_error() {
                        Some(libc::EAGAIN | libc::EINTR) => {}
                        Some(libc::EPIPE) => {
                            self.stdin = None;
                            return Err(ExchangeEnd::Closed);
                        }
                        _ => return Err(ExchangeEnd::Io(error)),
                    }
                }
            }
            // Read standard error, then standard output, at most one chunk each.
            if let Some(fd) = self.stderr.as_ref().map(AsRawFd::as_raw_fd) {
                match read_chunk(fd, &mut buffer) {
                    Ok(Some(0)) => self.stderr = None,
                    Ok(Some(n)) => {
                        if output(Stream::Stderr, &buffer[..n]) == Flow::Fault {
                            return Err(ExchangeEnd::Fault);
                        }
                    }
                    Ok(None) => {}
                    Err(error) => return Err(ExchangeEnd::Io(error)),
                }
            }
            match read_chunk(stdout, &mut buffer) {
                Ok(Some(0)) => {
                    self.stdout = None;
                    if !answered {
                        return Err(ExchangeEnd::Closed);
                    }
                    if written < input.len() {
                        return Err(ExchangeEnd::Closed);
                    }
                }
                Ok(Some(n)) => match output(Stream::Stdout, &buffer[..n]) {
                    Flow::Fault => return Err(ExchangeEnd::Fault),
                    Flow::Answered => answered = true,
                    Flow::More => {}
                },
                Ok(None) => {}
                Err(error) => return Err(ExchangeEnd::Io(error)),
            }
        }
    }

    fn try_reap(&mut self) -> bool {
        if self.reaped.is_some() {
            return true;
        }
        let mut status = 0;
        // SAFETY: waits only on our own unreaped child, without blocking.
        let result = unsafe { libc::waitpid(self.pid, &mut status, libc::WNOHANG) };
        if result == self.pid {
            self.reaped = Some(status);
            true
        } else {
            result < 0 && io::Error::last_os_error().raw_os_error() == Some(libc::ECHILD) && {
                self.reaped = Some(-1);
                true
            }
        }
    }

    fn kill_and_reap(&mut self) {
        if self.reaped.is_some() {
            return;
        }
        // SAFETY: an unreaped child PID cannot be recycled.
        unsafe { libc::kill(self.pid, libc::SIGKILL) };
        loop {
            let mut status = 0;
            // SAFETY: a blocking wait on our own killed child.
            let result = unsafe { libc::waitpid(self.pid, &mut status, 0) };
            if result == self.pid {
                self.reaped = Some(status);
                return;
            }
            if result < 0 && io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
                self.reaped = Some(-1);
                return;
            }
        }
    }

    fn exit(&mut self) -> SolverExit {
        self.stdin = None;
        self.stdout = None;
        self.stderr = None;
        self.copy = None;
        let status = self.reaped.unwrap_or(-1);
        SolverExit {
            status: (status >= 0 && libc::WIFEXITED(status)).then(|| libc::WEXITSTATUS(status)),
            peak_footprint: self.peak,
            memory_bound: self.memory_bound,
        }
    }

    /// The normal end: closes standard input, drains and discards output,
    /// and reaps the solver when it exits. Cancel or the memory bound kills
    /// it. The copy is removed.
    pub fn finish(mut self) -> SolverExit {
        self.stdin = None;
        let mut buffer = vec![0u8; READ_CHUNK];
        while !self.try_reap() {
            if self.cancel.is_cancelled() || self.over_bound() {
                self.kill_and_reap();
                break;
            }
            let mut fds = vec![libc::pollfd {
                fd: self.cancel.fd(),
                events: libc::POLLIN,
                revents: 0,
            }];
            for fd in [&self.stdout, &self.stderr].into_iter().flatten() {
                fds.push(libc::pollfd {
                    fd: fd.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                });
            }
            // SAFETY: a valid pollfd array; the timeout is sampling pace.
            unsafe {
                libc::poll(
                    fds.as_mut_ptr(),
                    fds.len() as libc::nfds_t,
                    SOLVER_SAMPLE.as_millis() as libc::c_int,
                )
            };
            self.drain_cancel();
            for slot in [&mut self.stdout, &mut self.stderr] {
                if let Some(fd) = slot.as_ref().map(AsRawFd::as_raw_fd) {
                    while let Ok(Some(n)) = read_chunk(fd, &mut buffer) {
                        if n == 0 {
                            *slot = None;
                            break;
                        }
                    }
                }
            }
        }
        self.exit()
    }

    /// Sends `SIGKILL` to the unreaped solver, reaps it and removes the copy.
    pub fn kill(mut self) -> SolverExit {
        self.kill_and_reap();
        self.exit()
    }
}

impl Drop for SolverProcess {
    /// A safety net, not the normal settlement: an unreaped solver is
    /// killed and reaped, and the copy is removed.
    fn drop(&mut self) {
        self.kill_and_reap();
        self.copy = None;
    }
}

/// One nonblocking read: `Some(n)` bytes (0 is the end), `None` when no
/// byte is ready.
fn read_chunk(fd: RawFd, buffer: &mut [u8]) -> io::Result<Option<usize>> {
    // SAFETY: reads into a valid writable buffer of the given length.
    let n = unsafe { libc::read(fd, buffer.as_mut_ptr().cast(), buffer.len()) };
    if n >= 0 {
        return Ok(Some(n as usize));
    }
    let error = io::Error::last_os_error();
    match error.raw_os_error() {
        Some(libc::EAGAIN | libc::EINTR) => Ok(None),
        _ => Err(error),
    }
}

/// Writes a stalled-solver fixture package (PL3 validation map): `dir/MacOS/cvc5`,
/// a copy of `fixture`; `dir/Frameworks/fixture.mode`, the mode; and
/// `dir/manifest.json` naming both.
#[cfg(feature = "test-support")]
pub fn fixture_package(fixture: &Path, mode: &str, dir: &Path) -> io::Result<PathBuf> {
    use sha2::{Digest, Sha256};
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(dir.join("MacOS"))?;
    std::fs::create_dir_all(dir.join("Frameworks"))?;
    let exe = dir.join("MacOS/cvc5");
    std::fs::copy(fixture, &exe)?;
    std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755))?;
    let mode_path = dir.join("Frameworks/fixture.mode");
    std::fs::write(&mode_path, mode)?;
    std::fs::set_permissions(&mode_path, std::fs::Permissions::from_mode(0o644))?;
    let mut files = serde_json::Map::new();
    for (rel, path) in [
        ("MacOS/cvc5", &exe),
        ("Frameworks/fixture.mode", &mode_path),
    ] {
        let bytes = std::fs::read(path)?;
        let digest: String = Sha256::digest(&bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        files.insert(
            rel.to_owned(),
            serde_json::json!({"bytes": bytes.len(), "sha256": digest}),
        );
    }
    let manifest = serde_json::json!({"format_version": 1, "files": files});
    std::fs::write(dir.join("manifest.json"), manifest.to_string())?;
    Ok(dir.to_path_buf())
}

/// The `solver_fixture` test-support executable. Rule, with no search: a
/// test executable in `<profile>/build/<package>/<hash>/out/` (Cargo
/// 1.100's build-dir layout) finds it in `<profile>/`; any other executable
/// finds it in its own directory. `None` when it is not built.
#[cfg(feature = "test-support")]
pub fn fixture_executable() -> Option<PathBuf> {
    let directory = cargo_profile_directory(&std::env::current_exe().ok()?)?;
    Some(directory.join("solver_fixture")).filter(|p| p.is_file())
}

/// `<profile>` for an executable in `<profile>/build/<package>/<hash>/out/`;
/// otherwise the executable's own directory.
#[cfg(feature = "test-support")]
pub fn cargo_profile_directory(executable: &Path) -> Option<PathBuf> {
    let directory = executable.parent()?;
    if directory.file_name() == Some("out".as_ref())
        && let Some(build) = directory.ancestors().nth(3)
        && build.file_name() == Some("build".as_ref())
    {
        return build.parent().map(Path::to_path_buf);
    }
    Some(directory.to_path_buf())
}
