//! PB0.1–PB0.2: one worktree lock and bounded process lifetime. Standard library only.
#![deny(unsafe_op_in_unsafe_fn)]

use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::process::Child;
use std::process::{Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicI32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

const AGGREGATE: Duration = Duration::from_secs(45 * 60);
const BUILD: Duration = Duration::from_secs(10 * 60);
const GRACE: Duration = Duration::from_secs(2);
const OUTPUT_LIMIT: usize = 16 * 1024 * 1024;
const POLL: Duration = Duration::from_millis(10);
static INTERRUPTED: AtomicI32 = AtomicI32::new(0);
static UNIQUE: AtomicU64 = AtomicU64::new(0);

#[derive(Debug)]
struct Failure {
    code: &'static str,
    detail: String,
}
type Result<T> = std::result::Result<T, Failure>;
impl Failure {
    fn new(code: &'static str, detail: impl Into<String>) -> Self {
        Self {
            code,
            detail: detail.into(),
        }
    }
}
impl From<io::Error> for Failure {
    fn from(error: io::Error) -> Self {
        Self::new("io_error", error.to_string())
    }
}
impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.detail)
    }
}

mod host {
    use super::*;
    unsafe extern "C" {
        #[cfg(test)]
        fn kill(pid: i32, signal: i32) -> i32;
        #[cfg(test)]
        fn getsid(pid: i32) -> i32;
        fn fcntl(fd: i32, command: i32, ...) -> i32;
        fn signal(signal: i32, handler: usize) -> usize;
        fn write(fd: i32, bytes: *const u8, count: usize) -> isize;
    }
    extern "C" fn interrupted(signal: i32) {
        INTERRUPTED.store(signal, Ordering::Relaxed);
    }
    pub fn install_signals() -> Result<()> {
        for number in [1, 2, 15] {
            // SAFETY: the function has C ABI, static lifetime, and only stores an atomic.
            if unsafe { signal(number, interrupted as *const () as usize) } == usize::MAX {
                return Err(io::Error::last_os_error().into());
            }
        }
        Ok(())
    }
    pub fn write_bytes(fd: i32, bytes: &[u8]) -> io::Result<usize> {
        // SAFETY: bytes remains valid for this syscall; caller holds fd throughout.
        let count = unsafe { write(fd, bytes.as_ptr(), bytes.len()) };
        if count < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(count as usize)
        }
    }
    pub fn flags(fd: i32) -> io::Result<i32> {
        // SAFETY: caller holds the descriptor for this synchronous query.
        let flags = unsafe { fcntl(fd, 3) };
        if flags < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(flags)
        }
    }
    pub fn set_flags(fd: i32, flags: i32) -> io::Result<()> {
        // SAFETY: caller retains this descriptor and supplies its prior/current flags.
        if unsafe { fcntl(fd, 4, flags) } < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
    pub fn nonblocking(fd: i32) -> io::Result<()> {
        // SAFETY: caller retains the owned pipe while these descriptor operations run.
        let flags = unsafe { fcntl(fd, 3) }; // Darwin F_GETFL
        if flags < 0 || unsafe { fcntl(fd, 4, flags | 4) } < 0 {
            // F_SETFL, O_NONBLOCK
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
    #[cfg(test)]
    pub fn group_signal(group: u32, signal: i32) -> io::Result<()> {
        let group = i32::try_from(group).map_err(|_| io::Error::other("invalid process group"))?;
        if group <= 1 {
            return Err(io::Error::other("unsafe process group"));
        }
        // SAFETY: Command::process_group(0) assigned this child its own positive PGID.
        if unsafe { kill(-group, signal) } < 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(3) {
                return Err(error);
            } // ESRCH
        }
        Ok(())
    }
    #[cfg(test)]
    pub fn fixture_session_id() -> io::Result<i32> {
        // SAFETY: zero requests the calling process session; no pointers are passed.
        let id = unsafe { getsid(0) };
        if id < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(id)
        }
    }
    #[cfg(test)]
    pub fn fixture_pid_exists(pid: u32) -> io::Result<bool> {
        let pid = i32::try_from(pid)
            .ok()
            .filter(|&p| p > 1)
            .ok_or_else(|| io::Error::other("unsafe fixture PID"))?;
        // SAFETY: signal zero observes the PID written by this test's private manifest.
        if unsafe { kill(pid, 0) } == 0 {
            return Ok(true);
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(3) {
            Ok(false)
        } else {
            Err(error)
        }
    }
    #[cfg(test)]
    pub fn fixture_pid_signal(pid: u32, value: i32) -> io::Result<()> {
        let pid = i32::try_from(pid)
            .ok()
            .filter(|&p| p > 1)
            .ok_or_else(|| io::Error::other("unsafe fixture PID"))?;
        // SAFETY: this PID comes only from the live test manifest readiness record.
        if unsafe { kill(pid, value) } == 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(3) {
            Ok(())
        } else {
            Err(error)
        }
    }
    #[cfg(test)]
    pub fn group_exists(group: u32) -> io::Result<bool> {
        let group = i32::try_from(group).map_err(|_| io::Error::other("invalid process group"))?;
        if group <= 1 {
            return Err(io::Error::other("unsafe process group"));
        }
        // SAFETY: signal zero only inspects the owned group; it sends no signal.
        if unsafe { kill(-group, 0) } == 0 {
            return Ok(true);
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(3) {
            Ok(false)
        } else {
            Err(error)
        }
    }
}

/// The marker is intentionally not removed by Drop or after an unknown outcome.
struct WorktreeRun {
    _lock: File,
    marker: PathBuf,
    marker_identity: (u64, u64),
}
impl WorktreeRun {
    fn acquire(root: &Path) -> Result<Self> {
        let build = root.join(".build");
        match fs::create_dir(&build) {
            Ok(()) => fs::set_permissions(&build, fs::Permissions::from_mode(0o700))?,
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => (),
            Err(e) => return Err(e.into()),
        }
        if !fs::symlink_metadata(&build)?.is_dir() {
            return Err(Failure::new(
                "unsafe_path",
                ".build must be a real directory",
            ));
        }
        let path = build.join("asura-toolchain.lock");
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(0x100)
            .open(&path)?; // Darwin O_NOFOLLOW
        let metadata = lock.metadata()?;
        if !metadata.is_file() || metadata.nlink() != 1 {
            return Err(Failure::new(
                "unsafe_path",
                "lock must be a single-link regular file",
            ));
        }
        match lock.try_lock() {
            Ok(()) => (),
            Err(std::fs::TryLockError::WouldBlock) => {
                return Err(Failure::new(
                    "worktree_busy",
                    "another check owns this worktree",
                ));
            }
            Err(std::fs::TryLockError::Error(e)) => return Err(e.into()),
        }
        let named = fs::symlink_metadata(path)?;
        if (named.dev(), named.ino()) != (metadata.dev(), metadata.ino()) {
            return Err(Failure::new("unsafe_path", "lock identity changed"));
        }
        let marker = build.join("asura-toolchain.incomplete");
        let mut file = match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&marker)
        {
            Ok(file) => file,
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                return Err(Failure::new(
                    "cleanup_required",
                    "prior run has unproved cleanup; inspect its processes before clearing the marker",
                ));
            }
            Err(e) => return Err(e.into()),
        };
        writeln!(file, "format=1\nowner_pid={}", std::process::id())?;
        file.sync_all()?;
        let m = file.metadata()?;
        Ok(Self {
            _lock: lock,
            marker,
            marker_identity: (m.dev(), m.ino()),
        })
    }
    fn settle(self, proved: bool) -> Result<()> {
        if !proved {
            return Err(Failure::new(
                "cleanup_required",
                "child or delegated group settlement is unproved; marker retained",
            ));
        }
        let current = fs::symlink_metadata(&self.marker)?;
        if (current.dev(), current.ino()) != self.marker_identity || !current.is_file() {
            return Err(Failure::new(
                "cleanup_required",
                "run marker identity changed",
            ));
        }
        fs::remove_file(&self.marker)?;
        Ok(())
    }
}

struct TempDir(PathBuf, bool);
impl TempDir {
    fn within(parent: &Path, label: &str) -> Result<Self> {
        for _ in 0..100 {
            let serial = UNIQUE.fetch_add(1, Ordering::Relaxed);
            let path = parent.join(format!("{label}-{}-{serial}", std::process::id()));
            match fs::create_dir(&path) {
                Ok(()) => {
                    fs::set_permissions(&path, fs::Permissions::from_mode(0o700))?;
                    return Ok(Self(path, false));
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => (),
                Err(e) => return Err(e.into()),
            }
        }
        Err(Failure::new(
            "io_error",
            "cannot create private run directory",
        ))
    }
}
impl Drop for TempDir {
    fn drop(&mut self) {
        if !self.1 && !std::thread::panicking() {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
}

fn emit(fd: i32, mut bytes: &[u8], deadline: Instant) -> Result<()> {
    let original = host::flags(fd)?;
    host::nonblocking(fd)?;
    let result = (|| {
        while !bytes.is_empty() {
            if Instant::now() >= deadline {
                return Err(Failure::new(
                    "report_unavailable",
                    "diagnostic output did not drain",
                ));
            }
            match host::write_bytes(fd, &bytes[..bytes.len().min(8192)]) {
                Ok(0) => return Err(Failure::new("report_unavailable", "output closed")),
                Ok(n) => bytes = &bytes[n..],
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => std::thread::sleep(POLL),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => (),
                Err(e) => return Err(e.into()),
            }
        }
        Ok(())
    })();
    let restored = host::set_flags(fd, original).map_err(Failure::from);
    match result {
        Ok(()) => restored,
        Err(error) => Err(combine(error, restored)),
    }
}

struct Outcome {
    result: Result<ExitStatus>,
    settled: bool,
    output: Vec<u8>,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

fn drain(pipe: &mut impl Read, output: &mut Vec<u8>, cap: usize) -> Result<bool> {
    let mut buffer = [0; 8192];
    // A busy writer cannot monopolize the loop and starve deadlines/signals.
    for _ in 0..16 {
        match pipe.read(&mut buffer) {
            Ok(0) => return Ok(true),
            Ok(n) => {
                if n > cap.saturating_sub(output.len()) {
                    return Err(Failure::new(
                        "output_limit",
                        "child exceeded captured-output limit",
                    ));
                }
                output.extend_from_slice(&buffer[..n]);
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(false),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.into()),
        }
    }
    Ok(false)
}

#[cfg(test)]
fn terminate(child: &mut Child, grace: Duration) -> bool {
    let group = child.id();
    for signal in [15, 9] {
        if host::group_signal(group, signal).is_err() {
            return false;
        }
        let end = Instant::now() + grace;
        loop {
            let reaped = match child.try_wait() {
                Ok(value) => value.is_some(),
                Err(_) => return false,
            };
            match host::group_exists(group) {
                Ok(false) if reaped => return true,
                Ok(_) => (),
                Err(_) => return false,
            }
            if Instant::now() >= end {
                break;
            }
            std::thread::sleep(POLL);
        }
    }
    false
}

/// A delegated supervisor must publish this receipt only after all its groups settle.
/// Parent death, EOF and a successful wait status cannot substitute for the receipt.
fn supervise(
    command: &mut Command,
    deadline: Instant,
    cap: usize,
    grace: Duration,
    delegated_receipt: Option<&Path>,
) -> Outcome {
    let not_started = |error| Outcome {
        result: Err(error),
        settled: true,
        output: Vec::new(),
        stdout: Vec::new(),
        stderr: Vec::new(),
    };
    if INTERRUPTED.load(Ordering::Relaxed) != 0 || Instant::now() >= deadline {
        return not_started(Failure::new(
            "not_started",
            "signal or deadline before spawn",
        ));
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // SAFETY: after fork this hook performs only setsid and the syscall error conversion.
    // No process group is created before setsid. Parent retains the unreaped session anchor.
    unsafe {
        command.pre_exec(|| {
            if process_api::setsid() < 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(())
            }
        });
    }
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(e) => return not_started(e.into()),
    };
    let mut scope = ProcessScope::new(child.id(), delegated_receipt.is_some());
    let mut output = Vec::new();
    let mut captured_stdout = Vec::new();
    let mut captured_stderr = Vec::new();
    let mut stdout = child.stdout.take().expect("requested pipe");
    let mut stderr = child.stderr.take().expect("requested pipe");
    let mut failure = host::nonblocking(stdout.as_raw_fd())
        .and_then(|_| host::nonblocking(stderr.as_raw_fd()))
        .err()
        .map(Failure::from);
    let mut exit = None;
    let mut out_done = false;
    let mut err_done = false;
    let mut next_scan = Instant::now();
    let mut read_output = |output: &mut Vec<u8>| -> Result<bool> {
        let previous = output.len();
        let out = drain(&mut stdout, output, cap);
        captured_stdout.extend_from_slice(&output[previous..]);
        out_done |= out?;
        let previous = output.len();
        let err = drain(&mut stderr, output, cap);
        captured_stderr.extend_from_slice(&output[previous..]);
        err_done |= err?;
        Ok(out_done && err_done)
    };
    while failure.is_none() {
        if INTERRUPTED.load(Ordering::Relaxed) != 0 {
            failure = Some(Failure::new("interrupted", "termination signal received"));
            break;
        }
        if Instant::now() >= deadline {
            failure = Some(Failure::new("deadline", "child exceeded absolute deadline"));
            break;
        }
        if let Err(e) = read_output(&mut output) {
            failure = Some(e);
            break;
        }
        match process_api::observe_exit(child.id(), deadline) {
            Ok(value) => exit = value,
            Err(error) => {
                scope.uncertain(error.clone());
                failure = Some(Failure::new("cleanup_required", error));
                break;
            }
        }
        if exit.is_some() {
            break;
        }
        if Instant::now() >= next_scan {
            if let Err(error) = scope.scan(deadline, None, false) {
                scope.uncertain(error)
            }
            if let Some(error) = &scope.fault {
                failure = Some(Failure::new("cleanup_required", error.clone()));
                break;
            }
            next_scan = Instant::now() + Duration::from_millis(100);
        }
        std::thread::sleep(POLL);
    }
    let cleanup_start = Instant::now();
    let cleanup_end = cleanup_start + grace + grace;
    let mut clear_scans = 0;
    let mut last_clear = None;
    let mut scope_clear = false;
    let mut phase_scans = 0;
    let mut phase = 15;
    let mut capture_failed = false;
    next_scan = cleanup_start;
    while Instant::now() < cleanup_end {
        let pipes_done = match read_output(&mut output) {
            Ok(done) => done,
            Err(error) => {
                capture_failed = true;
                if failure.is_none() {
                    failure = Some(error);
                }
                false
            }
        };
        if exit.is_none() {
            match process_api::observe_exit(child.id(), cleanup_end) {
                Ok(value) => exit = value,
                Err(error) => scope.uncertain(error),
            }
        }
        let now = Instant::now();
        if now >= cleanup_start + grace && phase == 15 {
            phase = 9;
            phase_scans = 0;
            next_scan = now;
        }
        if now >= next_scan {
            phase_scans += 1;
            if phase_scans > 64 {
                scope.uncertain("cleanup scan bound".into());
                break;
            }
            let phase_end = if phase == 15 {
                cleanup_start + grace
            } else {
                cleanup_end
            };
            match scope.scan(phase_end, Some(phase), exit.is_some()) {
                Ok((clear, residual)) => {
                    if residual && exit.is_some_and(|e| e.success()) && failure.is_none() {
                        failure = Some(Failure::new(
                            "unexpected_descendant",
                            "successful child left live observed descendants",
                        ));
                    }
                    if clear {
                        if last_clear.is_none_or(|previous| {
                            now.duration_since(previous) >= Duration::from_millis(100)
                        }) {
                            clear_scans += 1;
                            last_clear = Some(now);
                        }
                    } else {
                        clear_scans = 0;
                        last_clear = None;
                    }
                    if clear_scans >= 2 && (pipes_done || capture_failed) {
                        scope_clear = true;
                        break;
                    }
                }
                Err(error) => scope.uncertain(error),
            }
            next_scan = Instant::now() + Duration::from_millis(100);
        }
        std::thread::sleep(POLL);
    }
    if !scope_clear && scope.fault.is_none() {
        let pending: Vec<_> = scope
            .members
            .values()
            .filter(|m| !m.gone)
            .take(16)
            .map(|m| (m.facts.pid, m.facts.session, m.facts.status, m.pending))
            .collect();
        scope.uncertain(format!("scope observation incomplete anchor={} exit={exit:?} members(pid,sid,status,pending)={pending:?}", child.id()));
    }
    // Stop all identity inspection/signalling before releasing the anchor by reaping.
    let mut status = None;
    if let Some(observation) = exit {
        match child.wait() {
            Ok(value) => {
                if !observation.matches(value) {
                    scope.uncertain("final wait status mismatch".into())
                }
                status = Some(value);
            }
            Err(error) => scope.uncertain(format!("final reap: {error}")),
        }
    }
    let delegated_settled = delegated_receipt.is_none_or(|path| {
        failure.is_none()
            && status.is_some_and(|s| s.success())
            && fs::read(path).is_ok_and(|bytes| bytes == b"settled\n")
    });
    let settled = scope_clear
        && scope.fault.is_none()
        && status.is_some()
        && delegated_settled
        && process_api::RIGHT_FAILURE.load(Ordering::Relaxed) == 0;
    let result = if let Some(error) = failure {
        Err(match scope.fault {
            Some(detail) => combine(error, Err(Failure::new("cleanup_required", detail))),
            None => error,
        })
    } else if let Some(error) = scope.fault {
        Err(Failure::new("cleanup_required", error))
    } else if let Some(status) = status {
        if status.success() {
            Ok(status)
        } else {
            Err(Failure::new("child_failed", status.to_string()))
        }
    } else {
        Err(Failure::new("cleanup_required", "child exit unproved"))
    };
    Outcome {
        result,
        settled,
        output,
        stdout: captured_stdout,
        stderr: captured_stderr,
    }
}

fn arguments(args: &[String]) -> Result<()> {
    if args.len() == 2
        && args[0] == "--check"
        && ["driver", "bootstrap", "preparation", "preparation-offline"].contains(&args[1].as_str())
    {
        return Ok(());
    }
    if args.is_empty()
        || args == ["--offline"]
        || args == ["--prepare-only"]
        || (args.len() == 2
            && args[0] == "--check"
            && ["unit", "integration", "e2e", "all"].contains(&args[1].as_str()))
    {
        return Err(Failure::new(
            "not_implemented",
            "only scoped driver, bootstrap and preparation checks are delivered",
        ));
    }
    Err(Failure::new(
        "invalid_arguments",
        "expected a fixed --check selector",
    ))
}

// These fixed steps apply only to the reviewed PB0.2 crate. Its dependency build
// scripts and tests inherit Cargo's group; they must not create delegated groups.
const BOOTSTRAP_STEPS: &[&[&str]] = &[
    &[
        "build",
        "--locked",
        "--offline",
        "--package",
        "asura-toolchain-bootstrap",
    ],
    &[
        "clippy",
        "--locked",
        "--offline",
        "--package",
        "asura-toolchain-bootstrap",
        "--all-targets",
        "--",
        "-D",
        "warnings",
    ],
    &[
        "test",
        "--locked",
        "--offline",
        "--package",
        "asura-toolchain-bootstrap",
        "--all-targets",
    ],
];

fn bootstrap_inputs(root: &Path, rustc: &Path) -> Result<PathBuf> {
    if !root
        .join("rust/crates/asura-toolchain-bootstrap/Cargo.toml")
        .is_file()
        || !root.join("Cargo.lock").is_file()
    {
        return Err(Failure::new(
            "bootstrap_unavailable",
            "bootstrap manifest and reviewed Cargo.lock required",
        ));
    }
    for relative in [
        ".build/asura-deps",
        ".build/asura-deps/cargo",
        ".build/asura-deps/cargo/work",
    ] {
        let metadata = fs::symlink_metadata(root.join(relative)).map_err(|_| {
            Failure::new(
                "cargo_cache_unavailable",
                "prepared worktree-local Cargo work cache required; no preparation attempted",
            )
        })?;
        if !metadata.is_dir() {
            return Err(Failure::new(
                "unsafe_path",
                "Cargo work cache ancestors must be real directories",
            ));
        }
    }
    let target = root.join(".build/asura-deps/cargo/target");
    match fs::symlink_metadata(target) {
        Ok(metadata) if !metadata.is_dir() => {
            return Err(Failure::new(
                "unsafe_path",
                "Cargo target must be a real directory",
            ));
        }
        Err(error) if error.kind() != io::ErrorKind::NotFound => return Err(error.into()),
        _ => (),
    }
    let bin = rustc.parent().ok_or_else(|| {
        Failure::new(
            "toolchain_unavailable",
            "physical compiler directory required",
        )
    })?;
    for name in ["rustc", "rustdoc", "cargo", "cargo-clippy", "clippy-driver"] {
        let metadata = fs::symlink_metadata(bin.join(name)).map_err(|_| {
            Failure::new(
                "toolchain_unavailable",
                format!("installed physical {name} required"),
            )
        })?;
        if !metadata.is_file() || metadata.permissions().mode() & 0o111 == 0 {
            return Err(Failure::new(
                "toolchain_unavailable",
                format!("installed physical {name} required"),
            ));
        }
    }
    Ok(bin.join("cargo"))
}

fn bootstrap_command(root: &Path, rustc: &Path, cargo: &Path, args: &[&str]) -> Result<Command> {
    let bin = cargo.parent().ok_or_else(|| {
        Failure::new("toolchain_unavailable", "physical Cargo directory required")
    })?;
    let mut paths = vec![bin.to_path_buf()];
    paths.extend(env::split_paths(&env::var_os("PATH").unwrap_or_default()));
    let mut command = Command::new(cargo);
    command
        .args(args)
        .current_dir(root)
        .env("CARGO_HOME", root.join(".build/asura-deps/cargo/work"))
        .env(
            "CARGO_TARGET_DIR",
            root.join(".build/asura-deps/cargo/target"),
        )
        .env("RUSTC", rustc)
        .env("RUSTDOC", bin.join("rustdoc"))
        .env(
            "PATH",
            env::join_paths(paths).map_err(|_| {
                Failure::new("toolchain_unavailable", "invalid executable search path")
            })?,
        )
        .env(
            "CARGO_BUILD_BUILD_DIR",
            root.join(".build/asura-deps/cargo/target"),
        )
        .env("CARGO_BUILD_TARGET", "aarch64-apple-darwin")
        .env("RUSTC_WRAPPER", "")
        .env("RUSTC_WORKSPACE_WRAPPER", "")
        .env_remove("CARGO_BUILD_RUSTC_WRAPPER")
        .env_remove("CARGO_BUILD_RUSTC_WORKSPACE_WRAPPER");
    Ok(command)
}

const SWIFT_BUILD: Duration = Duration::from_secs(20 * 60);
const PROBE: Duration = Duration::from_secs(10);
const FETCH: Duration = Duration::from_secs(120);
const NETWORK_DENIAL: &str = "(version 1)(allow default)(deny network*)";

fn alive(deadline: Instant) -> Result<()> {
    if INTERRUPTED.load(Ordering::Relaxed) != 0 || Instant::now() >= deadline {
        return Err(Failure::new("not_started", "signal or aggregate deadline"));
    }
    Ok(())
}
fn private_directory(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(m) if m.is_dir() => return Ok(()),
        Ok(_) => return Err(Failure::new("unsafe_path", "real directory required")),
        Err(e) if e.kind() == io::ErrorKind::NotFound => (),
        Err(e) => return Err(e.into()),
    }
    fs::create_dir(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}
fn write_record(path: &Path, bytes: &[u8], cap: usize) -> Result<()> {
    if bytes.len() > cap {
        return Err(Failure::new("output_limit", "record exceeds bound"));
    }
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}
fn read_record(path: &Path, cap: usize) -> Result<Vec<u8>> {
    let mut file = OpenOptions::new()
        .read(true)
        .custom_flags(0x100)
        .open(path)?;
    let meta = file.metadata()?;
    if !meta.is_file() || meta.nlink() != 1 || meta.len() > cap as u64 {
        return Err(Failure::new(
            "invalid_receipt",
            "single-link bounded regular record required",
        ));
    }
    let mut bytes = Vec::new();
    Read::by_ref(&mut file)
        .take(cap as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > cap {
        return Err(Failure::new("invalid_receipt", "record grew past bound"));
    }
    Ok(bytes)
}
fn parse_receipt(
    bytes: &[u8],
    operation: &str,
    run_id: &str,
    qualification: &str,
) -> Result<(String, String)> {
    let invalid = || Failure::new("invalid_receipt", "operation receipt mismatch");
    if bytes.len() > 512 || !bytes.is_ascii() || !bytes.ends_with(b"\n") {
        return Err(invalid());
    }
    let text = std::str::from_utf8(&bytes[..bytes.len() - 1]).map_err(|_| invalid())?;
    let fields: Vec<_> = text.split(' ').collect();
    if fields.len() != 7
        || fields[..4] != ["ASURA-PB03", "1", operation, run_id]
        || fields[6] != qualification
        || fields[4..6].iter().any(|hash| {
            hash.len() != 64
                || !hash
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        })
    {
        return Err(invalid());
    }
    Ok((fields[4].to_owned(), fields[5].to_owned()))
}
const REGISTRY_FILE_LIMIT: u64 = 65_536;
const REGISTRY_TOTAL_LIMIT: u64 = 2 * 1024 * 1024 * 1024;
struct RegistryBudget {
    files: u64,
    bytes: u64,
    entries: u64,
}
impl RegistryBudget {
    fn entry(&mut self) -> Result<()> {
        if self.entries >= REGISTRY_FILE_LIMIT {
            return Err(Failure::new(
                "cache_copy_limit",
                "registry entry count exceeds bound",
            ));
        }
        self.entries += 1;
        Ok(())
    }
    fn admit(&mut self, relative: &Path, declared: u64) -> Result<u64> {
        let limit = if relative.starts_with("cache") {
            32 * 1024 * 1024
        } else {
            16 * 1024 * 1024
        };
        if declared > limit
            || self.files >= REGISTRY_FILE_LIMIT
            || self
                .bytes
                .checked_add(declared)
                .is_none_or(|n| n > REGISTRY_TOTAL_LIMIT)
        {
            return Err(Failure::new(
                "cache_copy_limit",
                "registry input exceeds fixed bounds",
            ));
        }
        self.files += 1;
        Ok(limit)
    }
    fn account(&mut self, file_bytes: u64, amount: usize, limit: u64) -> Result<u64> {
        let next_file = file_bytes
            .checked_add(amount as u64)
            .filter(|&n| n <= limit)
            .ok_or_else(|| Failure::new("cache_copy_limit", "registry file exceeds bound"))?;
        let next_total = self
            .bytes
            .checked_add(amount as u64)
            .filter(|&n| n <= REGISTRY_TOTAL_LIMIT)
            .ok_or_else(|| Failure::new("cache_copy_limit", "registry total exceeds bound"))?;
        self.bytes = next_total;
        Ok(next_file)
    }
}
fn copy_registry(source: &Path, destination: &Path, deadline: Instant) -> Result<()> {
    fn copy(
        source: &Path,
        destination: &Path,
        relative: &Path,
        deadline: Instant,
        budget: &mut RegistryBudget,
    ) -> Result<()> {
        alive(deadline)?;
        if relative.to_str().is_none_or(|path| path.len() > 4096) {
            return Err(Failure::new(
                "cache_copy_limit",
                "registry path exceeds 4096 bytes",
            ));
        }
        budget.entry()?;
        let metadata = fs::symlink_metadata(source)?;
        if metadata.is_dir() {
            private_directory(destination)?;
            for entry in fs::read_dir(source)? {
                let entry = entry?;
                let name = entry.file_name();
                let text = name
                    .to_str()
                    .ok_or_else(|| Failure::new("unsafe_path", "non UTF-8 registry name"))?;
                if text.is_empty()
                    || !text
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"._-+".contains(&b))
                    || text == "."
                    || text == ".."
                {
                    return Err(Failure::new("unsafe_path", "invalid registry name"));
                }
                copy(
                    &entry.path(),
                    &destination.join(&name),
                    &relative.join(&name),
                    deadline,
                    budget,
                )?;
            }
            return Ok(());
        }
        let parts: Vec<_> = relative.iter().filter_map(|p| p.to_str()).collect();
        let allowed = (parts.len() == 3 && parts[0] == "cache" && parts[2].ends_with(".crate"))
            || (parts.len() == 3 && parts[0] == "index" && parts[2] == "config.json")
            || (parts.len() >= 4 && parts[0] == "index" && parts[2] == ".cache");
        if !allowed
            || !metadata.is_file()
            || metadata.nlink() != 1
            || metadata.permissions().mode() & 0o111 != 0
        {
            return Err(Failure::new(
                "unsafe_path",
                "only regular archive and index inputs may be restored",
            ));
        }
        let limit = budget.admit(relative, metadata.len())?;
        let mut input = OpenOptions::new()
            .read(true)
            .custom_flags(0x100)
            .open(source)?;
        let mut output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(destination)?;
        let mut buffer = vec![0u8; 65536];
        let mut file_bytes = 0;
        loop {
            alive(deadline)?;
            let count = input.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            file_bytes = budget.account(file_bytes, count, limit)?;
            output.write_all(&buffer[..count])?;
        }
        if file_bytes != metadata.len() {
            return Err(Failure::new(
                "unsafe_path",
                "registry source size changed during copy",
            ));
        }
        Ok(())
    }
    if !fs::symlink_metadata(source)?.is_dir() {
        return Err(Failure::new("unsafe_path", "registry root must be real"));
    }
    private_directory(destination)?;
    let mut budget = RegistryBudget {
        files: 0,
        bytes: 0,
        entries: 0,
    };
    for name in ["cache", "index"] {
        let input = source.join(name);
        if !fs::symlink_metadata(&input)?.is_dir() {
            return Err(Failure::new(
                "unsafe_path",
                "registry inputs must be real directories",
            ));
        }
        copy(
            &input,
            &destination.join(name),
            Path::new(name),
            deadline,
            &mut budget,
        )?;
    }
    for entry in fs::read_dir(source)? {
        let name = entry?.file_name();
        if name != "cache" && name != "index" {
            return Err(Failure::new("unsafe_path", "unlisted registry subtree"));
        }
    }
    Ok(())
}
fn initial_snapshot(cargo_root: &Path) -> Result<PathBuf> {
    let mut backups = Vec::new();
    for entry in fs::read_dir(cargo_root)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name
            .to_str()
            .ok_or_else(|| Failure::new("unsafe_path", "invalid cache name"))?;
        if name.starts_with(".backup-") {
            if !fs::symlink_metadata(entry.path())?.is_dir() {
                return Err(Failure::new(
                    "cleanup_required",
                    "backup must be a real directory",
                ));
            }
            backups.push(entry.path());
        }
    }
    if backups.len() > 1 {
        return Err(Failure::new(
            "cleanup_required",
            "multiple Cargo backups require reconciliation",
        ));
    }
    Ok(backups.pop().unwrap_or_else(|| cargo_root.join("snapshot")))
}
#[derive(Debug)]
struct SwiftLaunch {
    path: PathBuf,
    target: PathBuf,
    developer: PathBuf,
    identity: (u64, u64),
}
impl SwiftLaunch {
    fn select(path: &Path, developer: &Path) -> Result<Self> {
        let invalid = || {
            Failure::new(
                "toolchain_unavailable",
                "invalid selected Swift launch path",
            )
        };
        if !path.is_absolute() || path.file_name() != Some(std::ffi::OsStr::new("swift")) {
            return Err(invalid());
        }
        let developer = developer.canonicalize().map_err(|_| invalid())?;
        if !fs::metadata(&developer).is_ok_and(|m| m.is_dir()) {
            return Err(invalid());
        }
        let parent = path
            .parent()
            .ok_or_else(invalid)?
            .canonicalize()
            .map_err(|_| invalid())?;
        let target = path.canonicalize().map_err(|_| invalid())?;
        let metadata = fs::symlink_metadata(&target).map_err(|_| invalid())?;
        if !parent.starts_with(&developer)
            || !target.starts_with(&developer)
            || !metadata.is_file()
            || metadata.permissions().mode() & 0o111 == 0
        {
            return Err(invalid());
        }
        Ok(Self {
            path: path.to_owned(),
            target,
            developer,
            identity: (metadata.dev(), metadata.ino()),
        })
    }
    fn from_xcrun(output: &str, developer: &Path) -> Result<Self> {
        let path = output.strip_suffix('\n').unwrap_or(output);
        if path.contains(['\n', '\r']) {
            return Err(Failure::new(
                "toolchain_unavailable",
                "xcrun returned multiple Swift paths",
            ));
        }
        Self::select(Path::new(path), developer)
    }
    fn recheck(&self) -> Result<&Path> {
        let current = Self::select(&self.path, &self.developer)?;
        if current.target != self.target || current.identity != self.identity {
            return Err(Failure::new(
                "toolchain_changed",
                "selected Swift target identity changed",
            ));
        }
        Ok(&self.path)
    }
}
fn validate_swift_driver_identity(bytes: &[u8]) -> Result<()> {
    if bytes != b"swift-driver version: 1.168.6 " {
        return Err(Failure::new(
            "invalid_identity",
            "unexpected Swift driver identity",
        ));
    }
    Ok(())
}
enum SwiftPm {
    Build,
    #[cfg(test)]
    Package,
}
struct Preparation<'a> {
    root: &'a Path,
    rustc: &'a Path,
    directory: PathBuf,
    id: String,
    deadline: Instant,
    offline: bool,
    settled: bool,
    mutated: bool,
    publishing: Option<&'static str>,
    identities: Option<(String, String)>,
    developer: Option<PathBuf>,
}
impl Preparation<'_> {
    fn command(&self, program: &Path, denied: bool) -> Result<Command> {
        let mut command = if denied {
            let mut sandbox = Command::new("/usr/bin/sandbox-exec");
            sandbox.args(["-p", NETWORK_DENIAL]).arg(program);
            sandbox
        } else {
            Command::new(program)
        };
        let bin = self
            .rustc
            .parent()
            .ok_or_else(|| Failure::new("toolchain_unavailable", "compiler directory required"))?;
        command
            .env_clear()
            .current_dir(self.root)
            .env("HOME", self.directory.join("home"))
            .env("TMPDIR", self.directory.join("tmp"))
            .env(
                "PATH",
                env::join_paths([
                    bin,
                    Path::new("/usr/bin"),
                    Path::new("/bin"),
                    Path::new("/usr/sbin"),
                    Path::new("/sbin"),
                ])
                .map_err(|_| Failure::new("unsafe_path", "invalid PATH"))?,
            )
            .env("LC_ALL", "C");
        if let Some(developer) = &self.developer {
            command.env("DEVELOPER_DIR", developer);
        }
        Ok(command)
    }
    fn capture(
        &mut self,
        command: &mut Command,
        budget: Duration,
        cap: usize,
        report: bool,
    ) -> Result<(Vec<u8>, Vec<u8>)> {
        let outcome = supervise(
            command,
            self.deadline.min(Instant::now() + budget),
            cap,
            GRACE,
            None,
        );
        self.settled &= outcome.settled;
        if report {
            let reporting = emit(1, &outcome.output, Instant::now() + GRACE);
            if let Err(error) = outcome.result {
                return Err(combine(error, reporting));
            }
            reporting?;
        } else {
            outcome.result?;
        }
        if !self.settled {
            return Err(Failure::new(
                "cleanup_required",
                "preparation child settlement unproved",
            ));
        }
        Ok((outcome.stdout, outcome.stderr))
    }
    fn swift_command(&self, swift: &SwiftLaunch, denied: bool) -> Result<Command> {
        if self.developer.as_ref() != Some(&swift.developer) {
            return Err(Failure::new(
                "toolchain_changed",
                "Swift selection and DEVELOPER_DIR disagree",
            ));
        }
        self.command(swift.recheck()?, denied)
    }
    fn swiftpm_command(&self, swift: &SwiftLaunch, operation: SwiftPm) -> Result<Command> {
        let mut command = self.swift_command(swift, true)?;
        command
            .arg(match operation {
                SwiftPm::Build => "build",
                #[cfg(test)]
                SwiftPm::Package => "package",
            })
            .arg("--disable-sandbox");
        Ok(command)
    }
    fn swift_probe(&mut self, swift: &SwiftLaunch) -> Result<String> {
        let mut command = self.swift_command(swift, self.offline)?;
        command.arg("--version");
        let (bytes, diagnostics) = self.capture(&mut command, PROBE, 4096, false)?;
        validate_swift_driver_identity(&diagnostics)?;
        let identity = String::from_utf8(bytes)
            .map_err(|_| Failure::new("invalid_identity", "non UTF-8 identity"))?;
        write_record(
            &self.directory.join("swift-driver.version"),
            &diagnostics,
            4096,
        )?;
        Ok(identity)
    }
    fn probe(&mut self, program: &Path, args: &[&str]) -> Result<String> {
        let mut command = self.command(program, self.offline)?;
        command.args(args);
        self.probe_output(&mut command)
    }
    fn probe_output(&mut self, command: &mut Command) -> Result<String> {
        let (bytes, diagnostics) = self.capture(command, PROBE, 4096, false)?;
        if !diagnostics.is_empty() {
            return Err(Failure::new(
                "invalid_identity",
                "version probe emitted stderr",
            ));
        }
        String::from_utf8(bytes).map_err(|_| Failure::new("invalid_identity", "non UTF-8 identity"))
    }
    fn cargo(&self, args: &[&str], home: &Path, target: &Path, denied: bool) -> Result<Command> {
        let bin = self.rustc.parent().unwrap();
        let mut command = self.command(&bin.join("cargo"), denied)?;
        command
            .args(args)
            .env("CARGO_HOME", home)
            .env("CARGO_TARGET_DIR", target)
            .env("CARGO_BUILD_BUILD_DIR", target)
            .env("CARGO_BUILD_TARGET", "aarch64-apple-darwin")
            .env("RUSTC", self.rustc)
            .env("RUSTDOC", bin.join("rustdoc"))
            .env("RUSTC_WRAPPER", "")
            .env("RUSTC_WORKSPACE_WRAPPER", "");
        Ok(command)
    }
    fn operation(&mut self, operation: &str, qualification: &str) -> Result<()> {
        let receipt = self.directory.join(format!("{operation}.receipt"));
        match fs::symlink_metadata(&receipt) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => (),
            _ => {
                return Err(Failure::new(
                    "invalid_receipt",
                    "operation receipt already exists",
                ));
            }
        }
        let binary = self.root.join(
            ".build/asura-deps/cargo/target/aarch64-apple-darwin/debug/asura-toolchain-bootstrap",
        );
        let mut command = self.command(&binary, self.offline)?;
        command
            .arg(operation)
            .arg("--root")
            .arg(self.root)
            .arg("--run-id")
            .arg(&self.id);
        if operation != "inspect" {
            self.mutated = true;
        }
        match operation {
            "begin-publish-tools" => self.publishing = Some("tools"),
            "begin-publish-cargo" => self.publishing = Some("cargo"),
            _ => (),
        }
        self.capture(
            &mut command,
            if operation.starts_with("fetch-") {
                FETCH
            } else {
                BUILD
            },
            OUTPUT_LIMIT,
            true,
        )?;
        let identities = parse_receipt(
            &read_record(&receipt, 512)?,
            operation,
            &self.id,
            qualification,
        )?;
        if self
            .identities
            .as_ref()
            .is_some_and(|old| old != &identities)
        {
            return Err(Failure::new(
                "invalid_receipt",
                "lock identity changed during preparation",
            ));
        }
        self.identities = Some(identities);
        if matches!(operation, "finish-publish-tools" | "finish-publish-cargo") {
            self.publishing = None;
        }
        Ok(())
    }
    fn observe_host(&mut self) -> Result<SwiftLaunch> {
        let os = self.probe(Path::new("/usr/bin/sw_vers"), &["-productVersion"])?;
        let major = os.trim().split('.').next().unwrap_or("");
        let arch = self.probe(Path::new("/usr/bin/uname"), &["-m"])?;
        if major != "27" || arch.trim() != "arm64" {
            return Err(Failure::new("unsupported_host", "macOS27 arm64 required"));
        }
        let developer = self.probe(Path::new("/usr/bin/xcode-select"), &["-p"])?;
        let developer = PathBuf::from(developer.trim()).canonicalize()?;
        if !developer.is_absolute() || !developer.is_dir() {
            return Err(Failure::new(
                "toolchain_unavailable",
                "selected Xcode directory required",
            ));
        }
        self.developer = Some(developer);
        let xcode = self.probe(Path::new("/usr/bin/xcodebuild"), &["-version"])?;
        let build = xcode
            .lines()
            .find_map(|line| line.strip_prefix("Build version "))
            .ok_or_else(|| Failure::new("invalid_identity", "missing Xcode build"))?;
        let swift_path = self.probe(Path::new("/usr/bin/xcrun"), &["--find", "swift"])?;
        let swift = SwiftLaunch::from_xcrun(&swift_path, self.developer.as_ref().unwrap())?;
        let swift_identity = self.swift_probe(&swift)?;
        write_record(
            &self.directory.join("swift.version"),
            swift_identity.as_bytes(),
            4096,
        )?;
        let swift_version = swift_identity
            .split_whitespace()
            .collect::<Vec<_>>()
            .windows(2)
            .find_map(|pair| (pair[0] == "version").then_some(pair[1]))
            .ok_or_else(|| Failure::new("invalid_identity", "missing Swift version"))?;
        let rust = self.probe(self.rustc, &["--version"])?;
        let rust_version = rust.split_whitespace().nth(1).unwrap_or("");
        let cargo = self.probe(&self.rustc.parent().unwrap().join("cargo"), &["--version"])?;
        let cargo_version = cargo.split_whitespace().nth(1).unwrap_or("");
        if [build, swift_version, rust_version, cargo_version]
            .iter()
            .any(|v| v.is_empty() || !v.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.'))
        {
            return Err(Failure::new("invalid_identity", "unsafe version identity"));
        }
        let host = format!(
            "{{\"os\":\"macos\",\"os_major\":27,\"arch\":\"aarch64\",\"xcode_build\":\"{build}\",\"swift_version\":\"{swift_version}\",\"rust_version\":\"{rust_version}\",\"cargo_version\":\"{cargo_version}\"}}\n"
        );
        write_record(&self.directory.join("host.json"), host.as_bytes(), 4096)?;
        Ok(swift)
    }
    fn prepare(&mut self) -> Result<()> {
        let cargo_root = self.root.join(".build/asura-deps/cargo");
        let work = cargo_root.join("work");
        let target = cargo_root.join("target");
        match fs::symlink_metadata(&work) {
            Ok(meta) if meta.is_dir() => fs::rename(&work, self.directory.join("previous-work"))?,
            Ok(_) => return Err(Failure::new("unsafe_path", "work must be real directory")),
            Err(e) if e.kind() == io::ErrorKind::NotFound => (),
            Err(e) => return Err(e.into()),
        }
        private_directory(&work)?;
        let snapshot = initial_snapshot(&cargo_root)?;
        match fs::symlink_metadata(&snapshot) {
            Ok(meta) if meta.is_dir() => copy_registry(
                &snapshot.join("registry"),
                &work.join("registry"),
                self.deadline,
            )?,
            Ok(_) => {
                return Err(Failure::new(
                    "unsafe_path",
                    "snapshot must be a real directory",
                ));
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound && !self.offline => (),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                return Err(Failure::new(
                    "cargo_cache_unavailable",
                    "offline preparation requires a snapshot",
                ));
            }
            Err(e) => return Err(e.into()),
        }
        private_directory(&target)?;
        bootstrap_inputs(self.root, self.rustc)?;
        let swift = self.observe_host()?;
        let mut args = vec![
            "build",
            "--locked",
            "--package",
            "asura-toolchain-bootstrap",
            "--bin",
            "asura-toolchain-bootstrap",
        ];
        if self.offline {
            args.push("--offline");
        }
        if !self.offline {
            let mut fetch = self.cargo(&["fetch", "--locked"], &work, &target, false)?;
            self.capture(&mut fetch, BUILD, OUTPUT_LIMIT, true)?;
        }
        let mut build = self.cargo(&args, &work, &target, self.offline)?;
        self.capture(&mut build, BUILD, OUTPUT_LIMIT, true)?;
        let mut metadata = self.cargo(
            &["metadata", "--locked", "--offline", "--format-version", "1"],
            &work,
            &target,
            true,
        )?;
        let (data, _) = self.capture(&mut metadata, PROBE, OUTPUT_LIMIT, false)?;
        write_record(
            &self.directory.join("cargo-metadata.json"),
            &data,
            OUTPUT_LIMIT,
        )?;
        self.operation("recover-start", "unqualified")?;
        self.operation("inspect", "unqualified")?;
        if !self.offline {
            self.operation("fetch-protoc", "unqualified")?;
            self.operation("fetch-swift", "unqualified")?;
        }
        self.operation("stage-tools", "unqualified")?;
        let mut build = self.swiftpm_command(&swift, SwiftPm::Build)?;
        build
            .args([
                "--configuration",
                "release",
                "--product",
                "protoc-gen-swift",
                "--package-path",
            ])
            .arg(self.directory.join("swift-build"))
            .arg("--scratch-path")
            .arg(self.directory.join("swift-scratch"));
        self.capture(&mut build, SWIFT_BUILD, OUTPUT_LIMIT, true)?;
        let candidate = self
            .root
            .join(format!(".build/asura-protobuf/.stage-{}", self.id));
        let generator = self
            .directory
            .join("swift-scratch/release/protoc-gen-swift")
            .canonicalize()?;
        let scratch = self.directory.join("swift-scratch").canonicalize()?;
        if !generator.starts_with(&scratch)
            || !fs::symlink_metadata(&generator)?.is_file()
            || fs::metadata(&generator)?.permissions().mode() & 0o111 == 0
        {
            return Err(Failure::new(
                "unsafe_path",
                "generator must resolve to executable inside scratch",
            ));
        }
        for (program, record) in [
            (candidate.join("protoc/bin/protoc"), "protoc.version"),
            (generator, "generator.version"),
        ] {
            let value = self.probe(&program, &["--version"])?;
            write_record(&self.directory.join(record), value.as_bytes(), 4096)?;
        }
        self.operation("verify-tools", "tools-verified")?;
        self.operation("begin-publish-tools", "tools-verified")?;
        let published = self
            .root
            .join(".build/asura-protobuf")
            .join(&self.identities.as_ref().unwrap().0);
        for (program, record) in [
            (
                published.join("protoc/bin/protoc"),
                "published-protoc.version",
            ),
            (
                published.join("bin/protoc-gen-swift"),
                "published-generator.version",
            ),
        ] {
            let value = self.probe(&program, &["--version"])?;
            write_record(&self.directory.join(record), value.as_bytes(), 4096)?;
        }
        self.operation("finish-publish-tools", "tools-verified")?;
        self.operation("stage-cargo", "unqualified")?;
        let cargo_candidate = cargo_root.join(format!(".stage-{}", self.id));
        let verify = self.directory.join("cargo-verify");
        private_directory(&verify)?;
        copy_registry(
            &cargo_candidate.join("registry"),
            &verify.join("registry"),
            self.deadline,
        )?;
        let mut qualify = self.cargo(
            &[
                "build",
                "--locked",
                "--offline",
                "--package",
                "asura-toolchain-bootstrap",
                "--bin",
                "asura-toolchain-bootstrap",
            ],
            &verify,
            &self.directory.join("cargo-target"),
            true,
        )?;
        self.capture(&mut qualify, BUILD, OUTPUT_LIMIT, true)?;
        write_record(
            &self.directory.join("offline-build.receipt"),
            b"bootstrap-only\n",
            64,
        )?;
        self.operation("verify-cargo", "bootstrap-only")?;
        self.operation("begin-publish-cargo", "bootstrap-only")?;
        self.operation("finish-publish-cargo", "bootstrap-only")?;
        Ok(())
    }
}
fn preparation_check(
    root: &Path,
    rustc: &Path,
    run: WorktreeRun,
    started: Instant,
    offline: bool,
) -> Result<()> {
    let mut state = None;
    let result = (|| {
        for relative in [
            ".build/asura-protobuf",
            ".build/asura-protobuf/.runs",
            ".build/asura-deps",
            ".build/asura-deps/cargo",
        ] {
            private_directory(&root.join(relative))?;
        }
        let directory = TempDir::within(&root.join(".build/asura-protobuf/.runs"), "prepare")?;
        let id = directory
            .0
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();
        // Retain receipts and recovery evidence; this directory has no published authority.
        let path = directory.0.clone();
        std::mem::forget(directory);
        private_directory(&path.join("home"))?;
        private_directory(&path.join("tmp"))?;
        state = Some(Preparation {
            root,
            rustc,
            directory: path,
            id,
            deadline: started + AGGREGATE,
            offline,
            settled: true,
            mutated: false,
            publishing: None,
            identities: None,
            developer: None,
        });
        state.as_mut().unwrap().prepare()
    })();
    let mut proved = state.as_ref().is_none_or(|s| s.settled);
    let result = match result {
        Err(mut failure) => {
            if let Some(state) = state.as_mut()
                && state.mutated
            {
                if state.settled && alive(state.deadline).is_ok() {
                    let recovery_operation = match state.publishing {
                        Some("tools") => "rollback-tools",
                        Some("cargo") => "rollback-cargo",
                        _ => "recover",
                    };
                    if let Err(recovery) = state.operation(recovery_operation, "unqualified") {
                        failure.detail.push_str(&format!("; recovery: {recovery}"));
                        proved = false;
                    }
                } else {
                    proved = false;
                }
            }
            Err(failure)
        }
        Ok(()) => Ok(()),
    };
    if result
        .as_ref()
        .is_err_and(|failure| failure.code == "cleanup_required")
    {
        proved = false;
    }
    let cleanup = run.settle(proved);
    if let Err(failure) = result {
        return Err(combine(failure, cleanup));
    }
    cleanup?;
    emit(1,format!("PASS PB0.3 bootstrap-only preparation: {:.3}s; tools verified, current bootstrap Cargo snapshot qualified; smoke and portable completeness remain pending\n",started.elapsed().as_secs_f64()).as_bytes(),Instant::now()+GRACE)
}

fn bootstrap_check(root: &Path, rustc: &Path, run: WorktreeRun, started: Instant) -> Result<()> {
    let mut settled = true;
    let result = (|| {
        let cargo = bootstrap_inputs(root, rustc)?;
        let mut version = bootstrap_command(root, rustc, &cargo, &["--version"])?;
        let outcome = supervise(
            &mut version,
            (started + AGGREGATE).min(Instant::now() + Duration::from_secs(5)),
            OUTPUT_LIMIT,
            GRACE,
            None,
        );
        settled &= outcome.settled;
        outcome.result?;
        if !settled {
            return Err(Failure::new(
                "cleanup_required",
                "Cargo version group not settled",
            ));
        }
        let identity = String::from_utf8_lossy(&outcome.output);
        let mut fields = identity.split_whitespace();
        if fields.next() != Some("cargo") || fields.next() != Some("1.98.0") {
            return Err(Failure::new(
                "toolchain_unavailable",
                "installed Cargo 1.98.0 required",
            ));
        }
        emit(1, &outcome.output, Instant::now() + GRACE)?;
        for args in BOOTSTRAP_STEPS {
            let mut command = bootstrap_command(root, rustc, &cargo, args)?;
            let outcome = supervise(
                &mut command,
                (started + AGGREGATE).min(Instant::now() + BUILD),
                OUTPUT_LIMIT,
                GRACE,
                None,
            );
            settled &= outcome.settled;
            let reporting = emit(1, &outcome.output, Instant::now() + GRACE);
            if let Err(error) = outcome.result {
                return Err(combine(error, reporting));
            }
            reporting?;
            if !settled {
                return Err(Failure::new(
                    "cleanup_required",
                    "Cargo process group not settled",
                ));
            }
        }
        Ok(())
    })();
    let cleanup = run.settle(settled);
    if let Err(error) = result {
        return Err(combine(error, cleanup));
    }
    cleanup?;
    emit(1, format!("PASS bootstrap library checks: {:.3}s; locked/offline build, Clippy and tests; full preparation and smoke qualification are not established by this selector\n", started.elapsed().as_secs_f64()).as_bytes(), Instant::now() + GRACE)
}

fn run() -> Result<()> {
    let args = env::args().skip(1).collect::<Vec<_>>();
    arguments(&args)?;
    if !cfg!(all(target_os = "macos", target_arch = "aarch64")) {
        return Err(Failure::new("unsupported_host", "macOS arm64 required"));
    }
    host::install_signals()?;
    let started = Instant::now();
    let root = PathBuf::from(
        env::var_os("ASURA_DRIVER_ROOT")
            .ok_or_else(|| Failure::new("launcher_required", "use scripts/check-i0-toolchain"))?,
    )
    .canonicalize()?;
    let rustc = PathBuf::from(
        env::var_os("ASURA_DRIVER_RUSTC")
            .ok_or_else(|| Failure::new("launcher_required", "missing verified compiler"))?,
    );
    let run = WorktreeRun::acquire(&root)?;
    if args[1] == "bootstrap" {
        return bootstrap_check(&root, &rustc, run, started);
    }
    if args[1] == "preparation" || args[1] == "preparation-offline" {
        return preparation_check(
            &root,
            &rustc,
            run,
            started,
            args[1] == "preparation-offline",
        );
    }
    let mut temp = TempDir::within(&root.join(".build"), "driver-check")?;
    let tests = temp.0.join("driver-tests");
    let mut compile = Command::new(&rustc);
    compile
        .args(["--edition=2024", "--test", "-D", "warnings"])
        .arg(root.join("rust/check-i0-driver.rs"))
        .arg("-o")
        .arg(&tests);
    let build = supervise(
        &mut compile,
        (started + AGGREGATE).min(Instant::now() + BUILD),
        OUTPUT_LIMIT,
        GRACE,
        None,
    );
    temp.1 = !build.settled;
    if !build.output.is_empty() {
        if let Err(report) = emit(1, &build.output, Instant::now() + GRACE) {
            return Err(combine(report, run.settle(build.settled)));
        }
    }
    if let Err(error) = build.result {
        let cleanup = run.settle(build.settled);
        return Err(combine(error, cleanup));
    }
    if !build.settled {
        return Err(Failure::new(
            "cleanup_required",
            "test compiler group not settled",
        ));
    }
    let receipt = temp.0.join("settled");
    let mut command = Command::new(&tests);
    command
        .args(["--test-threads=1", "--nocapture"])
        .env("ASURA_TEST_RECEIPT", &receipt)
        .env("ASURA_TEST_ROOT", &root)
        .env("ASURA_TEST_RUSTC", &rustc)
        .env("ASURA_TEST_DRIVER", env::current_exe()?);
    let outcome = supervise(
        &mut command,
        started + AGGREGATE,
        OUTPUT_LIMIT,
        GRACE,
        Some(&receipt),
    );
    temp.1 = !outcome.settled;
    let cleanup = run.settle(outcome.settled);
    let reporting = emit(1, &outcome.output, Instant::now() + GRACE);
    match outcome.result {
        Err(error) => Err(combine(combine(error, cleanup), reporting)),
        Ok(_) => {
            cleanup?;
            reporting?;
            emit(1, format!(
                "PASS PB0.1 driver: {:.3}s; Rust 1.98.0; no Cargo dependencies or tool downloads\n",
                started.elapsed().as_secs_f64()
            ).as_bytes(), Instant::now() + GRACE)?;
            Ok(())
        }
    }
}
fn combine(mut error: Failure, cleanup: Result<()>) -> Failure {
    if let Err(cleanup) = cleanup {
        error.detail.push_str(&format!("; {cleanup}"));
    }
    error
}
fn main() -> std::process::ExitCode {
    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(error) => {
            let _ = emit(
                2,
                format!("check-i0-driver: {error}\n").as_bytes(),
                Instant::now() + GRACE,
            );
            std::process::ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;
    pub(super) static UNPROVED: AtomicBool = AtomicBool::new(false);
    pub(super) fn scratch() -> TempDir {
        TempDir::within(&env::temp_dir(), "asura-pb01-test").unwrap()
    }
    fn short() -> Duration {
        Duration::from_millis(150)
    }
    fn shell(text: &str) -> Command {
        let mut c = Command::new("/bin/sh");
        c.args(["-c", text]);
        c
    }
    fn check(mut command: Command) -> Outcome {
        supervise(
            &mut command,
            Instant::now() + Duration::from_secs(10),
            OUTPUT_LIMIT,
            GRACE,
            None,
        )
    }
    fn root() -> PathBuf {
        PathBuf::from(env::var_os("ASURA_TEST_ROOT").unwrap())
    }
    fn driver() -> PathBuf {
        PathBuf::from(env::var_os("ASURA_TEST_DRIVER").unwrap())
    }
    fn rustc() -> PathBuf {
        PathBuf::from(env::var_os("ASURA_TEST_RUSTC").unwrap())
    }
    fn observed(outcome: &Outcome) {
        if !outcome.settled {
            UNPROVED.store(true, Ordering::Relaxed);
        }
    }

    fn preparation_fixture<'a>(root: &'a Path, rustc: &'a Path) -> Preparation<'a> {
        let directory = root.join("run");
        private_directory(&directory).unwrap();
        private_directory(&directory.join("home")).unwrap();
        private_directory(&directory.join("tmp")).unwrap();
        Preparation {
            root,
            rustc,
            directory,
            id: "prepare-test".into(),
            deadline: Instant::now() + Duration::from_secs(10),
            offline: false,
            settled: true,
            mutated: false,
            publishing: None,
            identities: None,
            developer: None,
        }
    }
    #[test]
    fn preparation_receipts_are_strict_and_bound_to_run() {
        let good = format!(
            "ASURA-PB03 1 inspect prepare-test {} {} unqualified\n",
            "a".repeat(64),
            "b".repeat(64)
        );
        let identities =
            parse_receipt(good.as_bytes(), "inspect", "prepare-test", "unqualified").unwrap();
        assert_eq!(identities, ("a".repeat(64), "b".repeat(64)));
        for bad in [
            good.trim_end().to_owned(),
            good.clone() + "\n",
            good.replace("prepare-test", "other"),
            good.replace("unqualified", "tools-verified"),
            good.replace("ASURA-PB03 1", "ASURA-PB03  1"),
            good.replace(&"a".repeat(64), &"A".repeat(64)),
            "x".repeat(513),
        ] {
            assert!(
                parse_receipt(bad.as_bytes(), "inspect", "prepare-test", "unqualified").is_err()
            );
        }
        let temp = scratch();
        let receipt = temp.0.join("receipt");
        write_record(&receipt, good.as_bytes(), 512).unwrap();
        assert_eq!(read_record(&receipt, 512).unwrap(), good.as_bytes());
        assert!(write_record(&receipt, b"overwrite", 512).is_err());
        fs::hard_link(&receipt, temp.0.join("alias")).unwrap();
        assert!(read_record(&receipt, 512).is_err());
    }
    #[test]
    fn preparation_capture_separates_stdout_from_diagnostics() {
        let outcome = check(shell("printf '{\"valid\":true}'; printf 'diagnostic' >&2"));
        observed(&outcome);
        assert!(outcome.result.is_ok());
        assert!(outcome.settled);
        assert_eq!(outcome.stdout, b"{\"valid\":true}");
        assert_eq!(outcome.stderr, b"diagnostic");
        assert!(String::from_utf8_lossy(&outcome.output).contains("diagnostic"));
    }
    #[test]
    fn preparation_registry_copy_rejects_links_and_executables() {
        let temp = scratch();
        let source = temp.0.join("registry");
        fs::create_dir_all(source.join("cache/source")).unwrap();
        fs::create_dir_all(source.join("index/source/.cache/a")).unwrap();
        let package = source.join("cache/source/package-1+metadata.crate");
        fs::write(&package, b"archive").unwrap();
        fs::write(source.join("index/source/config.json"), b"{}").unwrap();
        fs::write(source.join("index/source/.cache/a/package"), b"index").unwrap();
        let destination = temp.0.join("copied");
        copy_registry(&source, &destination, Instant::now() + PROBE).unwrap();
        assert_eq!(
            fs::read(destination.join("cache/source/package-1+metadata.crate")).unwrap(),
            b"archive"
        );
        fs::set_permissions(&package, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(
            copy_registry(&source, &temp.0.join("executable"), Instant::now() + PROBE).is_err()
        );
        fs::set_permissions(&package, fs::Permissions::from_mode(0o600)).unwrap();
        std::os::unix::fs::symlink(&package, source.join("cache/source/alias.crate")).unwrap();
        assert!(copy_registry(&source, &temp.0.join("linked"), Instant::now() + PROBE).is_err());
        fs::remove_file(source.join("cache/source/alias.crate")).unwrap();
        fs::write(source.join("credentials.toml"), b"must not copy").unwrap();
        assert!(
            copy_registry(&source, &temp.0.join("credentials"), Instant::now() + PROBE).is_err()
        );
    }
    #[test]
    fn preparation_commands_confine_offline_and_use_private_environment() {
        let temp = scratch();
        let compiler = rustc();
        let state = preparation_fixture(&temp.0, &compiler);
        let command = state
            .cargo(
                &["build", "--locked", "--offline"],
                &temp.0.join("work"),
                &temp.0.join("target"),
                true,
            )
            .unwrap();
        assert_eq!(command.get_program(), "/usr/bin/sandbox-exec");
        let args = command.get_args().collect::<Vec<_>>();
        assert_eq!(args[0], "-p");
        assert_eq!(args[1], NETWORK_DENIAL);
        let vars = command
            .get_envs()
            .collect::<std::collections::BTreeMap<_, _>>();
        assert_eq!(
            vars[std::ffi::OsStr::new("HOME")],
            Some(state.directory.join("home").as_os_str())
        );
        assert_eq!(
            vars[std::ffi::OsStr::new("RUSTC_WRAPPER")],
            Some(std::ffi::OsStr::new(""))
        );
        assert_eq!(
            vars[std::ffi::OsStr::new("RUSTC_WORKSPACE_WRAPPER")],
            Some(std::ffi::OsStr::new(""))
        );
        assert!(!vars.contains_key(std::ffi::OsStr::new("RUSTFLAGS")));
        assert!(!vars.contains_key(std::ffi::OsStr::new("HTTPS_PROXY")));
        assert!(arguments(&["--check".into(), "preparation".into()]).is_ok());
        assert!(arguments(&["--check".into(), "preparation-offline".into()]).is_ok());
        assert!(arguments(&["--check".into(), "preparation".into(), "arbitrary".into()]).is_err());
        assert_eq!(SWIFT_BUILD.as_secs(), 1200);
        assert_eq!(PROBE.as_secs(), 10);
        assert_eq!(FETCH.as_secs(), 120);
    }
    #[test]
    fn preparation_failed_child_cannot_supply_an_accepted_receipt() {
        let temp = scratch();
        let compiler = rustc();
        let mut state = preparation_fixture(&temp.0, &compiler);
        let binary = temp.0.join(
            ".build/asura-deps/cargo/target/aarch64-apple-darwin/debug/asura-toolchain-bootstrap",
        );
        fs::create_dir_all(binary.parent().unwrap()).unwrap();
        let script = format!(
            "#!/bin/sh\nprintf 'ASURA-PB03 1 inspect prepare-test {} {} unqualified\\n' > '{}'\nexit 7\n",
            "a".repeat(64),
            "b".repeat(64),
            state.directory.join("inspect.receipt").display()
        );
        fs::write(&binary, script).unwrap();
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o700)).unwrap();
        let result = state.operation("inspect", "unqualified");
        if !state.settled {
            UNPROVED.store(true, Ordering::Relaxed);
        }
        assert_eq!(result.unwrap_err().code, "child_failed");
        assert!(state.identities.is_none());
        assert!(state.settled);
        assert_eq!(
            state.operation("inspect", "unqualified").unwrap_err().code,
            "invalid_receipt"
        );
    }

    #[test]
    fn preparation_failed_publication_stays_pending_until_finish_receipt() {
        let temp = scratch();
        let compiler = rustc();
        let mut state = preparation_fixture(&temp.0, &compiler);
        let binary = temp.0.join(
            ".build/asura-deps/cargo/target/aarch64-apple-darwin/debug/asura-toolchain-bootstrap",
        );
        fs::create_dir_all(binary.parent().unwrap()).unwrap();
        let script = format!(
            "#!/bin/sh\nprintf 'ASURA-PB03 1 %s prepare-test {} {} tools-verified\\n' \"$1\" > '{}/'\"$1\"'.receipt'\nexit 7\n",
            "a".repeat(64),
            "b".repeat(64),
            state.directory.display()
        );
        fs::write(&binary, &script).unwrap();
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o700)).unwrap();
        let failure = state.operation("begin-publish-tools", "tools-verified");
        if !state.settled {
            UNPROVED.store(true, Ordering::Relaxed);
        }
        assert!(failure.is_err());
        assert_eq!(state.publishing, Some("tools"));
        let failure = state.operation("finish-publish-tools", "tools-verified");
        if !state.settled {
            UNPROVED.store(true, Ordering::Relaxed);
        }
        assert!(failure.is_err());
        assert_eq!(state.publishing, Some("tools"));
        fs::remove_file(state.directory.join("finish-publish-tools.receipt")).unwrap();
        fs::write(&binary, script.replace("exit 7", "exit 0")).unwrap();
        let success = state.operation("finish-publish-tools", "tools-verified");
        if !state.settled {
            UNPROVED.store(true, Ordering::Relaxed);
        }
        success.unwrap();
        assert_eq!(state.publishing, None);
    }
    #[test]
    fn preparation_startup_prefers_one_backup_and_rejects_ambiguity() {
        let temp = scratch();
        fs::create_dir(temp.0.join("snapshot")).unwrap();
        assert_eq!(initial_snapshot(&temp.0).unwrap(), temp.0.join("snapshot"));
        fs::create_dir(temp.0.join(".backup-one")).unwrap();
        assert_eq!(
            initial_snapshot(&temp.0).unwrap(),
            temp.0.join(".backup-one")
        );
        fs::create_dir(temp.0.join(".backup-two")).unwrap();
        assert_eq!(
            initial_snapshot(&temp.0).unwrap_err().code,
            "cleanup_required"
        );
    }
    #[test]
    fn preparation_version_stderr_is_rejected() {
        let temp = scratch();
        let compiler = rustc();
        let mut state = preparation_fixture(&temp.0, &compiler);
        let value = state.probe(
            Path::new("/bin/sh"),
            &["-c", "printf version; printf warning >&2"],
        );
        if !state.settled {
            UNPROVED.store(true, Ordering::Relaxed);
        }
        assert_eq!(value.unwrap_err().code, "invalid_identity");
    }
    #[test]
    fn preparation_registry_copy_handles_deep_bounded_paths() {
        let temp = scratch();
        let source = temp.0.join("registry");
        fs::create_dir_all(source.join("cache")).unwrap();
        let mut relative = PathBuf::from("index/source/.cache");
        for _ in 0..96 {
            relative.push("a");
        }
        fs::create_dir_all(source.join(&relative)).unwrap();
        relative.push("package");
        fs::write(source.join(&relative), b"index").unwrap();
        let destination = temp.0.join("copied");
        copy_registry(&source, &destination, Instant::now() + PROBE).unwrap();
        assert_eq!(fs::read(destination.join(&relative)).unwrap(), b"index");
    }
    #[test]
    fn preparation_registry_limits_precede_copy_writes() {
        let mut budget = RegistryBudget {
            files: REGISTRY_FILE_LIMIT - 1,
            bytes: 0,
            entries: 0,
        };
        assert!(
            budget
                .admit(Path::new("cache/source/a.crate"), 32 * 1024 * 1024)
                .is_ok()
        );
        assert!(budget.admit(Path::new("cache/source/b.crate"), 0).is_err());
        let mut budget = RegistryBudget {
            files: 0,
            bytes: REGISTRY_TOTAL_LIMIT - 1,
            entries: 0,
        };
        assert!(budget.account(0, 1, 32 * 1024 * 1024).is_ok());
        assert!(budget.account(1, 1, 32 * 1024 * 1024).is_err());
        assert_eq!(budget.bytes, REGISTRY_TOTAL_LIMIT);
        let mut budget = RegistryBudget {
            files: 0,
            bytes: 0,
            entries: 0,
        };
        assert!(
            budget
                .admit(Path::new("index/source/config.json"), 16 * 1024 * 1024 + 1)
                .is_err()
        );
        assert!(
            budget
                .account(32 * 1024 * 1024, 1, 32 * 1024 * 1024)
                .is_err()
        );
        assert_eq!(budget.bytes, 0);
        budget.entries = REGISTRY_FILE_LIMIT - 1;
        assert!(budget.entry().is_ok());
        assert!(budget.entry().is_err());
        let temp = scratch();
        let source = temp.0.join("registry");
        fs::create_dir_all(source.join("cache/source")).unwrap();
        fs::create_dir_all(source.join("index/source")).unwrap();
        let file = File::create(source.join("cache/source/oversized.crate")).unwrap();
        file.set_len(32 * 1024 * 1024 + 1).unwrap();
        let output = temp.0.join("copied");
        assert_eq!(
            copy_registry(&source, &output, Instant::now() + PROBE)
                .unwrap_err()
                .code,
            "cache_copy_limit"
        );
        assert!(!output.join("cache/source/oversized.crate").exists());
    }

    #[test]
    fn preparation_swift_driver_identity_is_exact() {
        assert!(validate_swift_driver_identity(b"swift-driver version: 1.168.6 ").is_ok());
        for invalid in [
            b"".as_slice(),
            b"swift-driver version: 1.168.7 ",
            b"swift-driver version: 1.168.6",
            b"swift-driver version: 1.168.6 \n",
            b"warning: swift-driver version: 1.168.6 ",
        ] {
            assert_eq!(
                validate_swift_driver_identity(invalid).unwrap_err().code,
                "invalid_identity"
            );
        }
    }
    #[test]
    fn preparation_native_swift_timeout_settles_manifest_process() {
        let temp = scratch();
        let compiler = rustc();
        let mut state = preparation_fixture(&temp.0, &compiler);
        state.offline = true;
        let developer = state.probe(Path::new("/usr/bin/xcode-select"), &["-p"]);
        if !state.settled {
            UNPROVED.store(true, Ordering::Relaxed);
        }
        state.developer = Some(
            PathBuf::from(developer.unwrap().trim())
                .canonicalize()
                .unwrap(),
        );
        let path = state.probe(Path::new("/usr/bin/xcrun"), &["--find", "swift"]);
        if !state.settled {
            UNPROVED.store(true, Ordering::Relaxed);
        }
        let swift =
            SwiftLaunch::from_xcrun(&path.unwrap(), state.developer.as_ref().unwrap()).unwrap();
        let version = state.swift_probe(&swift);
        if !state.settled {
            UNPROVED.store(true, Ordering::Relaxed);
        }
        assert!(version.unwrap().contains("Swift version"));
        assert_eq!(
            fs::read(state.directory.join("swift-driver.version")).unwrap(),
            b"swift-driver version: 1.168.6 "
        );
        let package = temp.0.join("native-package");
        fs::create_dir(&package).unwrap();
        fs::write(package.join("Package.swift"), r#"// swift-tools-version: 6.4
import PackageDescription
import Foundation
import Darwin
let ready = ProcessInfo.processInfo.environment["ASURA_NATIVE_READY"]!
try! "pid=\(getpid()) ppid=\(getppid()) pgid=\(getpgrp()) sid=\(getsid(0))".write(toFile: ready + ".identity", atomically: false, encoding: .utf8)
try! String(getpid()).write(toFile: ready, atomically: false, encoding: .utf8)
Thread.sleep(forTimeInterval: 90)
let package = Package(name: "NativeDeadline")
"#).unwrap();
        let ready = temp.0.join("manifest-pid");
        let mut command = state.swiftpm_command(&swift, SwiftPm::Package).unwrap();
        command
            .arg("--package-path")
            .arg(&package)
            .arg("--scratch-path")
            .arg(temp.0.join("native-build"))
            .arg("dump-package")
            .env("ASURA_NATIVE_READY", &ready);
        let outcome = supervise(
            &mut command,
            Instant::now() + Duration::from_secs(30),
            OUTPUT_LIMIT,
            GRACE,
            None,
        );
        observed(&outcome);
        let pid = fs::read_to_string(&ready)
            .ok()
            .and_then(|text| text.parse::<u32>().ok());
        let present = pid.is_some_and(|pid| host::fixture_pid_exists(pid).unwrap_or(true));
        let mut diagnostic = format!(
            "supervisor result={:?} settled={} session={:?}\nreadiness identity={:?}\n",
            outcome.result,
            outcome.settled,
            host::fixture_session_id(),
            fs::read_to_string(ready.with_extension("identity"))
        );
        if present {
            let mut query = Command::new("/bin/ps");
            query.args([
                "-p",
                &pid.unwrap().to_string(),
                "-o",
                "pid=,ppid=,pgid=,stat=,lstart=,command=",
            ]);
            let snapshot = supervise(
                &mut query,
                Instant::now() + Duration::from_secs(5),
                8192,
                GRACE,
                None,
            );
            observed(&snapshot);
            diagnostic.push_str(&format!(
                "ps result={:?} settled={}\n{}",
                snapshot.result,
                snapshot.settled,
                String::from_utf8_lossy(&snapshot.output)
            ));
        }
        if present {
            let pid = pid.unwrap();
            for signal in [15, 9] {
                let _ = host::fixture_pid_signal(pid, signal);
                let until = Instant::now() + GRACE;
                while host::fixture_pid_exists(pid).unwrap_or(true) && Instant::now() < until {
                    std::thread::sleep(POLL);
                }
            }
            if host::fixture_pid_exists(pid).unwrap_or(true) {
                UNPROVED.store(true, Ordering::Relaxed);
            }
        }
        fs::write(temp.0.join("native-process-diagnostic"), &diagnostic).unwrap();
        assert!(
            pid.is_some(),
            "native manifest never reached readiness: {}",
            String::from_utf8_lossy(&outcome.output)
        );
        assert!(
            !present,
            "native Swift manifest PID was present after supervision; preparation remains blocked: {diagnostic}"
        );
        assert!(outcome.settled, "native Swift group did not settle");
        assert_eq!(outcome.result.unwrap_err().code, "deadline");
        assert!(!host::fixture_pid_exists(pid.unwrap()).unwrap());
    }
    #[test]
    fn preparation_native_cargo_failure_is_bounded_and_settled() {
        let temp = scratch();
        let compiler = rustc();
        let state = preparation_fixture(&temp.0, &compiler);
        let manifest = temp.0.join("Cargo.toml");
        fs::write(&manifest, b"[package\nmalformed").unwrap();
        let mut command = state
            .cargo(
                &[
                    "metadata",
                    "--locked",
                    "--offline",
                    "--format-version",
                    "1",
                    "--manifest-path",
                ],
                &temp.0.join("cargo-home"),
                &temp.0.join("target"),
                true,
            )
            .unwrap();
        command.arg(&manifest);
        let outcome = supervise(
            &mut command,
            Instant::now() + Duration::from_secs(5),
            OUTPUT_LIMIT,
            GRACE,
            None,
        );
        observed(&outcome);
        assert!(outcome.settled);
        assert_eq!(outcome.result.unwrap_err().code, "child_failed");
        assert!(String::from_utf8_lossy(&outcome.output).contains("Cargo.toml"));
    }

    #[test]
    fn swift_launch_rejects_missing_nonexecutable_and_outside_targets() {
        let temp = scratch();
        let developer = temp.0.join("Developer");
        let bin = developer.join("toolchain/bin");
        fs::create_dir_all(&bin).unwrap();
        let alias = bin.join("swift");
        assert!(SwiftLaunch::select(&alias, &developer).is_err());
        let target = bin.join("swift-frontend");
        fs::write(&target, b"fixture executable").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).unwrap();
        std::os::unix::fs::symlink(&target, &alias).unwrap();
        assert!(SwiftLaunch::select(&alias, &developer).is_err());
        fs::set_permissions(&target, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(SwiftLaunch::select(&alias, &developer).is_ok());
        let outside = temp.0.join("outside");
        fs::write(&outside, b"fixture").unwrap();
        fs::set_permissions(&outside, fs::Permissions::from_mode(0o700)).unwrap();
        fs::remove_file(&alias).unwrap();
        std::os::unix::fs::symlink(&outside, &alias).unwrap();
        assert!(SwiftLaunch::select(&alias, &developer).is_err());
        fs::remove_file(&alias).unwrap();
        std::os::unix::fs::symlink(&bin, &alias).unwrap();
        assert!(SwiftLaunch::select(&alias, &developer).is_err());
        let outside_parent = temp.0.join("other-bin");
        fs::create_dir(&outside_parent).unwrap();
        let outside_alias = outside_parent.join("swift");
        std::os::unix::fs::symlink(&target, &outside_alias).unwrap();
        assert!(SwiftLaunch::select(&outside_alias, &developer).is_err());
        assert!(SwiftLaunch::from_xcrun("relative/swift\n", &developer).is_err());
        assert!(SwiftLaunch::from_xcrun("/first/swift\n/second/swift\n", &developer).is_err());
    }
    #[test]
    fn swift_launch_rechecks_inode_and_preserves_alias_under_sandbox() {
        let temp = scratch();
        let compiler = rustc();
        let mut state = preparation_fixture(&temp.0, &compiler);
        let developer = temp.0.join("Developer");
        let bin = developer.join("bin");
        fs::create_dir_all(&bin).unwrap();
        let target = bin.join("swift-frontend");
        fs::write(&target, b"fixture").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o700)).unwrap();
        let alias = bin.join("swift");
        std::os::unix::fs::symlink(&target, &alias).unwrap();
        let selected = SwiftLaunch::select(&alias, &developer).unwrap();
        state.developer = Some(selected.developer.clone());
        let direct = state.swift_command(&selected, false).unwrap();
        assert_eq!(direct.get_program(), alias.as_os_str());
        let confined = state.swift_command(&selected, true).unwrap();
        assert_eq!(confined.get_program(), "/usr/bin/sandbox-exec");
        assert_eq!(confined.get_args().nth(2).unwrap(), alias.as_os_str());
        for (operation, name) in [(SwiftPm::Build, "build"), (SwiftPm::Package, "package")] {
            let command = state.swiftpm_command(&selected, operation).unwrap();
            assert_eq!(command.get_program(), "/usr/bin/sandbox-exec");
            let args = command.get_args().collect::<Vec<_>>();
            assert_eq!(args.len(), 5);
            assert_eq!(args[0], "-p");
            assert_eq!(args[1], "(version 1)(allow default)(deny network*)");
            assert_eq!(args[2], alias.as_os_str());
            assert_eq!(args[3], name);
            assert_eq!(args[4], "--disable-sandbox");
        }
        fs::rename(&target, bin.join("retained-old-target")).unwrap();
        fs::write(&target, b"replacement").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o700)).unwrap();
        assert_eq!(selected.recheck().unwrap_err().code, "toolchain_changed");
        assert!(state.swift_command(&selected, true).is_err());
        let selected = SwiftLaunch::select(&alias, &developer).unwrap();
        fs::remove_file(&alias).unwrap();
        std::os::unix::fs::symlink(bin.join("retained-old-target"), &alias).unwrap();
        assert_eq!(selected.recheck().unwrap_err().code, "toolchain_changed");
    }

    #[test]
    fn preparation_fixed_fetch_timeout_rejects_receipt_and_publication() {
        let temp = scratch();
        let compiler = rustc();
        let mut state = preparation_fixture(&temp.0, &compiler);
        let binary = temp.0.join(
            ".build/asura-deps/cargo/target/aarch64-apple-darwin/debug/asura-toolchain-bootstrap",
        );
        fs::create_dir_all(binary.parent().unwrap()).unwrap();
        let ready = state.directory.join("fetch-ready");
        fs::write(&binary,format!("#!/bin/sh\n[ \"$1\" = fetch-protoc ] || exit 3\nprintf ready > '{}'\nexec /bin/sleep 10\n",ready.display())).unwrap();
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o700)).unwrap();
        state.deadline = Instant::now() + Duration::from_secs(1);
        let result = state.operation("fetch-protoc", "unqualified");
        if !state.settled {
            UNPROVED.store(true, Ordering::Relaxed);
        }
        assert_eq!(fs::read(&ready).unwrap(), b"ready");
        assert_eq!(result.as_ref().unwrap_err().code, "deadline", "{result:?}");
        assert!(state.settled);
        assert!(state.identities.is_none());
        assert!(!state.directory.join("fetch-protoc.receipt").exists());
        assert!(!temp.0.join(".build/asura-protobuf").exists());
    }
    #[test]
    fn bounds_and_fixed_arguments() {
        assert_eq!(AGGREGATE.as_secs(), 2700);
        assert_eq!(BUILD.as_secs(), 600);
        assert_eq!(GRACE.as_secs(), 2);
        assert_eq!(OUTPUT_LIMIT, 16_777_216);
        assert!(arguments(&["--check".into(), "driver".into()]).is_ok());
        for args in [
            vec![],
            vec!["--offline".into()],
            vec!["--check".into(), "all".into()],
        ] {
            assert_eq!(arguments(&args).unwrap_err().code, "not_implemented");
        }
        assert_eq!(
            arguments(&["--execute".into(), "anything".into()])
                .unwrap_err()
                .code,
            "invalid_arguments"
        );
    }
    #[test]
    fn bootstrap_fixed_steps_keep_locked_offline_scope() {
        assert!(arguments(&["--check".into(), "bootstrap".into()]).is_ok());
        assert!(arguments(&["--check".into(), "bootstrap".into(), "--offline".into()]).is_err());
        let temp = scratch();
        for args in BOOTSTRAP_STEPS {
            let compiler = rustc();
            let cargo = compiler.parent().unwrap().join("cargo");
            let command = bootstrap_command(&temp.0, &compiler, &cargo, args).unwrap();
            let args = command
                .get_args()
                .map(|v| v.to_str().unwrap())
                .collect::<Vec<_>>();
            assert!(args.contains(&"--offline"));
            assert!(args.contains(&"--locked"));
            assert!(
                args.windows(2)
                    .any(|p| p == ["--package", "asura-toolchain-bootstrap"])
            );
            assert!(!args.contains(&"--workspace"));
            assert_eq!(command.get_current_dir(), Some(temp.0.as_path()));
            let variables = command
                .get_envs()
                .collect::<std::collections::BTreeMap<_, _>>();
            assert_eq!(
                variables[std::ffi::OsStr::new("CARGO_HOME")],
                Some(temp.0.join(".build/asura-deps/cargo/work").as_os_str())
            );
            assert_eq!(
                variables[std::ffi::OsStr::new("CARGO_TARGET_DIR")],
                Some(temp.0.join(".build/asura-deps/cargo/target").as_os_str())
            );
            assert_eq!(
                variables[std::ffi::OsStr::new("RUSTC")],
                Some(compiler.as_os_str())
            );
            assert_eq!(
                variables[std::ffi::OsStr::new("RUSTC_WRAPPER")],
                Some(std::ffi::OsStr::new(""))
            );
            assert_eq!(
                variables[std::ffi::OsStr::new("RUSTC_WORKSPACE_WRAPPER")],
                Some(std::ffi::OsStr::new(""))
            );
            assert_eq!(
                variables[std::ffi::OsStr::new("CARGO_BUILD_RUSTC_WRAPPER")],
                None
            );
            assert_eq!(
                variables[std::ffi::OsStr::new("CARGO_BUILD_TARGET")],
                Some(std::ffi::OsStr::new("aarch64-apple-darwin"))
            );
            assert_eq!(
                variables[std::ffi::OsStr::new("CARGO_BUILD_BUILD_DIR")],
                Some(temp.0.join(".build/asura-deps/cargo/target").as_os_str())
            );
        }
    }

    #[test]
    fn bootstrap_missing_inputs_fail_without_creating_cache() {
        let temp = scratch();
        for expected in ["bootstrap_unavailable", "cargo_cache_unavailable"] {
            let mut command = Command::new(driver());
            command
                .args(["--check", "bootstrap"])
                .env("ASURA_DRIVER_ROOT", &temp.0)
                .env("ASURA_DRIVER_RUSTC", rustc());
            let outcome = check(command);
            observed(&outcome);
            assert!(outcome.result.is_err());
            assert!(String::from_utf8_lossy(&outcome.output).contains(expected));
            assert!(!temp.0.join(".build/asura-deps").exists());
            assert!(!temp.0.join(".build/asura-toolchain.incomplete").exists());
            fs::create_dir_all(temp.0.join("rust/crates/asura-toolchain-bootstrap")).unwrap();
            fs::write(
                temp.0
                    .join("rust/crates/asura-toolchain-bootstrap/Cargo.toml"),
                b"",
            )
            .unwrap();
            fs::write(temp.0.join("Cargo.lock"), b"").unwrap();
        }
    }

    #[test]
    fn lock_lifetime_and_marker_recovery() {
        let temp = scratch();
        let run = WorktreeRun::acquire(&temp.0).unwrap();
        assert_eq!(
            WorktreeRun::acquire(&temp.0).err().unwrap().code,
            "worktree_busy"
        );
        drop(run);
        assert_eq!(
            WorktreeRun::acquire(&temp.0).err().unwrap().code,
            "cleanup_required"
        );
        fs::remove_file(temp.0.join(".build/asura-toolchain.incomplete")).unwrap();
        WorktreeRun::acquire(&temp.0).unwrap().settle(true).unwrap();
        assert!(temp.0.join(".build/asura-toolchain.lock").exists());
        assert!(!temp.0.join(".build/asura-toolchain.incomplete").exists());
    }
    #[test]
    fn unsafe_cache_alias_rejected() {
        let temp = scratch();
        let elsewhere = scratch();
        std::os::unix::fs::symlink(&elsewhere.0, temp.0.join(".build")).unwrap();
        assert_eq!(
            WorktreeRun::acquire(&temp.0).err().unwrap().code,
            "unsafe_path"
        );
        assert!(fs::read_dir(&elsewhere.0).unwrap().next().is_none());
    }
    #[test]
    fn real_child_success_and_failure() {
        let out = check(shell("printf hello; printf error >&2"));
        observed(&out);
        assert!(out.result.is_ok());
        assert!(out.settled);
        assert_eq!(out.output, b"helloerror");
        let out = check(shell("exit 7"));
        observed(&out);
        assert_eq!(out.result.unwrap_err().code, "child_failed");
        assert!(out.settled);
    }
    #[test]
    fn absolute_deadline_stops_stubborn_child_group() {
        let mut command = shell("trap '' TERM; while :; do /bin/sleep 1; done");
        let start = Instant::now();
        let out = supervise(&mut command, start + short(), 4096, GRACE, None);
        observed(&out);
        assert!(out.settled, "{:?}", out.result);
        assert_eq!(out.result.unwrap_err().code, "deadline");
        assert!(start.elapsed() < short() + 2 * GRACE + Duration::from_secs(1));
    }
    #[test]
    fn successful_exit_with_live_group_orphan_is_not_success() {
        let temp = scratch();
        let pidfile = temp.0.join("orphan.pid");
        let mut command = shell(
            "set -m; /bin/sleep 10 >/dev/null 2>&1 & echo $! > \"$1\"; /bin/sleep 0.3; exit 0",
        );
        command.arg("fixture").arg(&pidfile);
        let out = check(command);
        observed(&out);
        let pid: u32 = fs::read_to_string(&pidfile)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let absent = !host::fixture_pid_exists(pid).unwrap();
        if !absent {
            UNPROVED.store(true, Ordering::Relaxed);
        }
        assert!(absent, "orphan survives: {:?}", out.result);
        assert!(out.settled, "{:?}", out.result);
        assert_eq!(out.result.unwrap_err().code, "unexpected_descendant");
    }
    #[test]
    fn injected_inspection_denial_retains_marker_after_actual_exit() {
        let temp = scratch();
        let run = WorktreeRun::acquire(&temp.0).unwrap();
        process_api::INSPECTION_FAULT.store(-1, Ordering::Relaxed);
        let out = check(shell("exec /bin/sleep 10"));
        let pid = process_api::INSPECTION_FAULT.swap(0, Ordering::Relaxed);
        let absent = pid > 0 && !host::fixture_pid_exists(pid as u32).unwrap();
        if !absent {
            UNPROVED.store(true, Ordering::Relaxed);
        }
        assert!(
            absent,
            "fault target cleanup unproved pid={pid}: {:?}",
            out.result
        );
        assert!(!out.settled);
        assert!(
            out.result
                .as_ref()
                .unwrap_err()
                .detail
                .contains("Operation not permitted"),
            "{:?}",
            out.result
        );
        assert_eq!(
            run.settle(out.settled).unwrap_err().code,
            "cleanup_required"
        );
        assert!(temp.0.join(".build/asura-toolchain.incomplete").exists());
    }
    #[test]
    fn repeated_short_lived_native_children_settle() {
        let out = check(shell(
            "i=0; while [ $i -lt 12 ]; do /bin/sleep 0.02; i=$((i + 1)); done",
        ));
        observed(&out);
        assert!(out.settled, "{:?}", out.result);
        assert!(out.result.is_ok(), "{:?}", out.result);
    }
    #[test]
    fn output_cap_stops_flood_without_starving_timer() {
        let mut command = shell("while :; do printf 012345678901234567890123456789; done");
        let out = supervise(
            &mut command,
            Instant::now() + Duration::from_secs(2),
            4096,
            GRACE,
            None,
        );
        observed(&out);
        assert!(out.settled, "{:?}", out.result);
        assert_eq!(out.result.unwrap_err().code, "output_limit");
        assert!(out.output.len() <= 4096);
    }
    #[test]
    fn nested_supervisor_loss_preserves_marker() {
        let temp = scratch();
        let run = WorktreeRun::acquire(&temp.0).unwrap();
        let pidfile = temp.0.join("child.pid");
        let mut nested = Command::new("/bin/sh");
        // A missing receipt remains unknown even when session cleanup stops this separate group.
        nested
            .args([
                "-c",
                "set -m; /bin/sleep 30 >/dev/null 2>&1 & echo $! > \"$1\"; kill -KILL $$",
                "fixture",
            ])
            .arg(&pidfile);
        let out = supervise(
            &mut nested,
            Instant::now() + Duration::from_secs(5),
            4096,
            GRACE,
            Some(&temp.0.join("never-settled")),
        );
        assert!(!out.settled);
        assert_eq!(
            run.settle(out.settled).unwrap_err().code,
            "cleanup_required"
        );
        assert_eq!(
            WorktreeRun::acquire(&temp.0).err().unwrap().code,
            "cleanup_required"
        );
        let pid: u32 = fs::read_to_string(pidfile).unwrap().trim().parse().unwrap();
        if host::group_exists(pid).unwrap_or(true) {
            UNPROVED.store(true, Ordering::Relaxed);
            panic!(
                "missing-receipt fixture descendant still present: {:?}",
                out.result
            );
        }
        fs::remove_file(temp.0.join(".build/asura-toolchain.incomplete")).unwrap();
        WorktreeRun::acquire(&temp.0).unwrap().settle(true).unwrap();
    }
    #[test]
    fn command_busy_lock_and_unsupported_arguments_have_no_effects() {
        let temp = scratch();
        let run = WorktreeRun::acquire(&temp.0).unwrap();
        let mut command = Command::new(driver());
        command
            .args(["--check", "driver"])
            .env("ASURA_DRIVER_ROOT", &temp.0)
            .env("ASURA_DRIVER_RUSTC", rustc());
        let out = check(command);
        observed(&out);
        assert!(String::from_utf8_lossy(&out.output).contains("worktree_busy"));
        run.settle(true).unwrap();
        let mut command = Command::new(driver());
        command.arg("--offline").env("ASURA_DRIVER_ROOT", &temp.0);
        let out = check(command);
        observed(&out);
        assert!(String::from_utf8_lossy(&out.output).contains("not_implemented"));
        assert!(!temp.0.join(".build/asura-toolchain.incomplete").exists());
    }
    #[test]
    fn launcher_no_toolchain_or_invalid_args_never_calls_rustup() {
        let temp = scratch();
        let bin = temp.0.join("bin");
        fs::create_dir(&bin).unwrap();
        let proxy = bin.join("rustup");
        fs::write(&proxy, "#!/bin/sh\necho UNEXPECTED_RUSTUP >&2\nexit 99\n").unwrap();
        fs::set_permissions(proxy, fs::Permissions::from_mode(0o700)).unwrap();
        let mut command = Command::new(root().join("scripts/check-i0-toolchain"));
        command
            .args(["--check", "driver"])
            .env("RUSTUP_HOME", temp.0.join("absent"))
            .env("PATH", format!("{}:/usr/bin:/bin", bin.display()));
        let out = check(command);
        observed(&out);
        let text = String::from_utf8_lossy(&out.output);
        assert!(text.contains("toolchain_unavailable"), "{text}");
        assert!(!text.contains("UNEXPECTED_RUSTUP"));
        let mut command = Command::new(root().join("scripts/check-i0-toolchain"));
        command.arg("--bad");
        let out = check(command);
        observed(&out);
        assert!(String::from_utf8_lossy(&out.output).contains("invalid_arguments"));
    }
    #[test]
    fn signal_during_driver_child_retains_unproved_delegation() {
        let temp = scratch();
        fs::create_dir(temp.0.join("rust")).unwrap();
        // Its libtest child waits indefinitely. The driver must stop it on TERM.
        fs::write(temp.0.join("rust/check-i0-driver.rs"), "#[test] fn wait_for_signal() { let p = std::path::PathBuf::from(std::env::var_os(\"ASURA_TEST_RECEIPT\").unwrap()); std::fs::write(p.parent().unwrap().join(\"tests-started\"), b\"ready\").unwrap(); loop { std::thread::park(); } }\n").unwrap();
        let mut command = Command::new(driver());
        command
            .args(["--check", "driver"])
            .env("ASURA_DRIVER_ROOT", &temp.0)
            .env("ASURA_DRIVER_RUSTC", rustc());
        let mut child = command
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .unwrap();
        let marker = temp.0.join(".build/asura-toolchain.incomplete");
        let end = Instant::now() + Duration::from_secs(10);
        while !marker.exists() && Instant::now() < end {
            std::thread::sleep(POLL);
        }
        assert!(marker.exists());
        // Wait for the delegated test process to start, without timing a compiler race.
        let end = Instant::now() + Duration::from_secs(10);
        loop {
            let ready = fs::read_dir(temp.0.join(".build"))
                .unwrap()
                .filter_map(|e| e.ok())
                .any(|e| e.path().join("tests-started").exists());
            if ready {
                break;
            }
            if Instant::now() >= end {
                terminate(&mut child, GRACE);
                panic!("delegated test did not start");
            }
            std::thread::sleep(POLL);
        }
        host::group_signal(child.id(), 15).unwrap();
        let end = Instant::now() + Duration::from_secs(6);
        while child.try_wait().unwrap().is_none() && Instant::now() < end {
            std::thread::sleep(POLL);
        }
        assert!(child.try_wait().unwrap().is_some());
        assert!(marker.exists());
    }
    #[test]
    fn preexisting_signal_or_deadline_does_not_spawn() {
        let temp = scratch();
        let target = temp.0.join("effect");
        let mut command = Command::new("/usr/bin/touch");
        command.arg(&target);
        let out = supervise(&mut command, Instant::now(), OUTPUT_LIMIT, GRACE, None);
        assert_eq!(out.result.unwrap_err().code, "not_started");
        INTERRUPTED.store(15, Ordering::Relaxed);
        let out = supervise(
            &mut command,
            Instant::now() + GRACE,
            OUTPUT_LIMIT,
            GRACE,
            None,
        );
        INTERRUPTED.store(0, Ordering::Relaxed);
        assert_eq!(out.result.unwrap_err().code, "not_started");
        assert!(!target.exists());
    }
    #[test]
    fn blocked_report_is_bounded_and_restores_descriptor_flags() {
        let (writer, _reader) = std::os::unix::net::UnixStream::pair().unwrap();
        // Darwin records FWASWRITTEN after the first write. Establish that kernel
        // state before comparing the complete flags; keep exact equality below.
        host::write_bytes(writer.as_raw_fd(), b"baseline").unwrap();
        let flags = host::flags(writer.as_raw_fd()).unwrap();
        let start = Instant::now();
        let error = emit(
            writer.as_raw_fd(),
            &vec![b'x'; OUTPUT_LIMIT],
            start + short(),
        )
        .unwrap_err();
        assert_eq!(error.code, "report_unavailable");
        assert!(start.elapsed() < Duration::from_secs(1));
        assert_eq!(flags, host::flags(writer.as_raw_fd()).unwrap());
    }
    #[test]
    fn failed_test_receipt_cannot_clear_marker_or_delete_run_evidence() {
        let temp = scratch();
        fs::create_dir(temp.0.join("rust")).unwrap();
        fs::write(temp.0.join("rust/check-i0-driver.rs"), "#[test] fn failed() { std::fs::write(std::env::var_os(\"ASURA_TEST_RECEIPT\").unwrap(), b\"settled\\n\").unwrap(); panic!(\"fixture failure\"); }\n").unwrap();
        let mut command = Command::new(driver());
        command
            .args(["--check", "driver"])
            .env("ASURA_DRIVER_ROOT", &temp.0)
            .env("ASURA_DRIVER_RUSTC", rustc());
        let out = check(command);
        observed(&out);
        assert!(out.result.is_err());
        assert!(String::from_utf8_lossy(&out.output).contains("cleanup_required"));
        assert!(temp.0.join(".build/asura-toolchain.incomplete").exists());
        assert!(
            fs::read_dir(temp.0.join(".build"))
                .unwrap()
                .filter_map(|e| e.ok())
                .any(|e| e.path().join("driver-tests").exists())
        );
    }
    #[test]
    fn launcher_rejects_empty_home_and_symlinked_compiler_proxy() {
        let temp = scratch();
        let bin = temp.0.join("toolchains/mock/bin");
        fs::create_dir_all(&bin).unwrap();
        let proxy = temp.0.join("rustup-proxy");
        fs::write(&proxy, "#!/bin/sh\necho UNEXPECTED_PROXY >&2\nexit 99\n").unwrap();
        fs::set_permissions(&proxy, fs::Permissions::from_mode(0o700)).unwrap();
        std::os::unix::fs::symlink(&proxy, bin.join("rustc")).unwrap();
        for home in [temp.0.as_os_str(), std::ffi::OsStr::new("")] {
            let mut command = Command::new(root().join("scripts/check-i0-toolchain"));
            command.args(["--check", "driver"]).env("RUSTUP_HOME", home);
            let out = check(command);
            observed(&out);
            let text = String::from_utf8_lossy(&out.output);
            assert!(text.contains("toolchain_unavailable"), "{text}");
            assert!(!text.contains("UNEXPECTED_PROXY"));
        }
    }
    #[test]
    fn launcher_real_120_second_compile_watchdog() {
        let temp = scratch();
        let bin = temp.0.join("toolchains/mock/bin");
        fs::create_dir_all(&bin).unwrap();
        let compiler = bin.join("rustc");
        fs::write(&compiler, "#!/bin/sh\nif [ \"$1\" = --version ]; then echo 'rustc 1.98.0 (fixture)'; exit 0; fi\necho $$ > \"$ASURA_TEST_COMPILER_PIDFILE\"\ntrap '' TERM\nwhile :; do /bin/sleep 1; done\n").unwrap();
        fs::set_permissions(&compiler, fs::Permissions::from_mode(0o700)).unwrap();
        let pidfile = temp.0.join("compiler.pid");
        let mut command = Command::new(root().join("scripts/check-i0-toolchain"));
        command
            .args(["--check", "driver"])
            .env("RUSTUP_HOME", &temp.0)
            .env("ASURA_TEST_COMPILER_PIDFILE", &pidfile);
        let start = Instant::now();
        let out = supervise(
            &mut command,
            start + Duration::from_secs(132),
            OUTPUT_LIMIT,
            GRACE,
            None,
        );
        let pid: u32 = fs::read_to_string(&pidfile)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let leftover = host::group_exists(pid).unwrap();
        if leftover {
            UNPROVED.store(true, Ordering::Relaxed);
            let _ = host::group_signal(pid, 9);
        }
        observed(&out);
        let text = String::from_utf8_lossy(&out.output);
        assert!(text.contains("compiler_timeout_or_interrupted"), "{text}");
        assert!(text.contains("compiler_failed"));
        assert!(start.elapsed() >= Duration::from_secs(120));
        assert!(start.elapsed() < Duration::from_secs(130));
        assert!(!leftover, "watchdog did not settle compiler group");
        assert!(out.settled);
    }

    #[test]
    fn actual_driver_stdout_backpressure_cannot_hold_lock() {
        let mut temp = scratch();
        temp.1 = true;
        let diagnostics = temp.0.join("driver-stderr.txt");
        fs::create_dir(temp.0.join("rust")).unwrap();
        fs::write(temp.0.join("rust/check-i0-driver.rs"), "#[test] fn output() { print!(\"{}\", \"x\".repeat(4 * 1024 * 1024)); std::fs::write(std::env::var_os(\"ASURA_TEST_RECEIPT\").unwrap(), b\"settled\\n\").unwrap(); }\n").unwrap();
        let mut child = Command::new(driver())
            .args(["--check", "driver"])
            .env("ASURA_DRIVER_ROOT", &temp.0)
            .env("ASURA_DRIVER_RUSTC", rustc())
            .stdout(Stdio::piped())
            .stderr(Stdio::from(File::create(&diagnostics).unwrap()))
            .process_group(0)
            .spawn()
            .unwrap();
        let _unread = child.stdout.take().unwrap();
        let end = Instant::now() + Duration::from_secs(8);
        let status = loop {
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if Instant::now() >= end {
                UNPROVED.store(true, Ordering::Relaxed);
                let _ = terminate(&mut child, GRACE);
                panic!("driver hung on actual stdout pipe");
            }
            std::thread::sleep(POLL);
        };
        let mut diagnostic = Vec::new();
        File::open(&diagnostics)
            .unwrap()
            .take(8192)
            .read_to_end(&mut diagnostic)
            .unwrap();
        let diagnostic = String::from_utf8_lossy(&diagnostic);
        assert!(!status.success(), "unexpected success; {diagnostic}");
        assert!(
            !temp.0.join(".build/asura-toolchain.incomplete").exists(),
            "marker retained; {diagnostic}; evidence={}",
            temp.0.display()
        );
        WorktreeRun::acquire(&temp.0).unwrap().settle(true).unwrap();
        temp.1 = false;
    }
    #[test]
    fn launcher_stalled_stderr_diagnostic_is_bounded() {
        let (writer, _unread) = std::os::unix::net::UnixStream::pair().unwrap();
        assert!(
            emit(
                writer.as_raw_fd(),
                &vec![b'x'; OUTPUT_LIMIT],
                Instant::now() + short()
            )
            .is_err()
        );
        let stderr: std::os::fd::OwnedFd = writer.into();
        let mut child = Command::new(root().join("scripts/check-i0-toolchain"))
            .arg("--bad")
            .stdout(Stdio::null())
            .stderr(Stdio::from(stderr))
            .process_group(0)
            .spawn()
            .unwrap();
        let end = Instant::now() + Duration::from_secs(6);
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                assert!(!status.success());
                break;
            }
            if Instant::now() >= end {
                UNPROVED.store(true, Ordering::Relaxed);
                let _ = terminate(&mut child, GRACE);
                panic!("launcher hung reporting to unread stderr");
            }
            std::thread::sleep(POLL);
        }
        assert!(!host::group_exists(child.id()).unwrap());
    }
    #[test]
    fn launcher_unreapable_compiler_model_never_enters_wait() {
        // Execute the real shell cleanup function against a modeled unkillable PID.
        // No process is made unkillable and no production deadline override is added.
        let source = include_str!("../scripts/check-i0-toolchain");
        let start = source.find("settle_group() {").unwrap();
        let end = source[start..].find("\ninterrupt() {").unwrap() + start;
        let temp = scratch();
        let witness = temp.0.join("wait-was-called");
        let script = format!(
            "{}\nkill() {{ return 0; }}\nwait() {{ /usr/bin/touch \"$witness\"; }}\nfail() {{ printf '%s\\n' \"$*\"; exit 42; }}\nwitness=\"$1\"\nchild=424242\ngroup_verified=1\ntmp=fixture\nsettle_group\n",
            &source[start..end]
        );
        let mut command = Command::new("/bin/sh");
        command.args(["-c", &script, "model"]).arg(&witness);
        let out = check(command);
        observed(&out);
        assert!(String::from_utf8_lossy(&out.output).contains("cleanup_required"));
        assert!(!witness.exists());
        assert!(out.result.is_err());
        assert!(out.settled);
    }

    #[test]
    fn zz_confirm_test_descendants_settled() {
        assert!(
            !UNPROVED.load(Ordering::Relaxed),
            "test child cleanup was unproved"
        );
        if let Some(path) = env::var_os("ASURA_TEST_RECEIPT") {
            fs::write(path, b"settled\n").unwrap();
        }
    }
}

/// Bounded API measurements only. None of these adapters is a production supervisor.
#[cfg(test)]
mod qualification {
    use super::*;
    use std::os::fd::{FromRawFd, OwnedFd};
    use std::os::unix::net::{UnixListener, UnixStream};

    type QResult<T> = std::result::Result<T, String>;
    const FIXTURE_LIFE: Duration = Duration::from_secs(15);
    const CASE: Duration = Duration::from_secs(30);
    const REPORT_CAP: usize = 1024 * 1024;
    use super::process_api::*;
    fn admit(known_cleanup: bool, now: Instant, deadline: Instant) -> bool {
        known_cleanup && now < deadline
    }
    fn acquire(entry: &Entry, report: &mut String) -> QResult<Option<KernelToken>> {
        if entry.child.is_none() || entry.reaped {
            return Err("token requires unreaped direct child".into());
        }
        match acquire_identity(&entry.identity) {
            Ok(token) => {
                report.push_str(&format!("kernel task-name/token acquired; words=8 pid={} pidversion={} rights released\n", token.identity.pid, token.generation));
                Ok(Some(token))
            }
            Err(error) if error.starts_with("token unavailable") => {
                report.push_str(&format!("{error}\n"));
                Ok(None)
            }
            Err(error) => Err(error),
        }
    }
    impl KernelToken {
        fn current_signal(&mut self, entry: &Entry, value: i32) -> QResult<i32> {
            if ![0, 15, 9].contains(&value) || entry.child.is_none() || entry.reaped {
                return Err("candidate signal requires held direct child".into());
            }
            let current = facts(entry.identity.pid).map_err(|e| e.to_string())?;
            if !same_identity(&current, &self.identity) || !same_identity(&current, &entry.identity)
            {
                return Err("signal identity mismatch".into());
            }
            // SAFETY: authentic kernel token for the retained unreaped direct child.
            // This API returns errno directly, not through last_os_error.
            Ok(unsafe { signal_token_raw(&mut self.token, value) })
        }
        fn stale_zero(&mut self) -> i32 {
            // SAFETY: authentic former token; signal zero cannot terminate a replacement.
            unsafe { signal_token_raw(&mut self.token, 0) }
        }
    }
    struct Channel {
        stream: UnixStream,
        pending: Vec<u8>,
        bytes: usize,
    }
    impl Channel {
        fn new(stream: UnixStream) -> QResult<Self> {
            stream.set_nonblocking(true).map_err(|e| e.to_string())?;
            Ok(Self {
                stream,
                pending: Vec::new(),
                bytes: 0,
            })
        }
        fn send(&mut self, text: &str, end: Instant) -> QResult<()> {
            self.bytes += text.len();
            if self.bytes > 8192 {
                return Err("channel byte bound".into());
            }
            let mut bytes = text.as_bytes();
            while !bytes.is_empty() {
                if Instant::now() >= end {
                    return Err("channel write deadline".into());
                }
                match self.stream.write(bytes) {
                    Ok(0) => return Err("channel closed".into()),
                    Ok(n) => bytes = &bytes[n..],
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => std::thread::sleep(POLL),
                    Err(e) => return Err(e.to_string()),
                }
            }
            Ok(())
        }
        fn line(&mut self, end: Instant) -> QResult<String> {
            loop {
                if let Some(index) = self.pending.iter().position(|b| *b == b'\n') {
                    let bytes: Vec<_> = self.pending.drain(..=index).collect();
                    return String::from_utf8(bytes).map_err(|e| e.to_string());
                }
                if Instant::now() >= end {
                    return Err("channel read deadline".into());
                }
                let mut buffer = [0; 1024];
                match self.stream.read(&mut buffer) {
                    Ok(0) => return Err("channel EOF".into()),
                    Ok(n) => {
                        self.bytes += n;
                        if self.bytes > 8192 {
                            return Err("channel byte bound".into());
                        }
                        self.pending.extend_from_slice(&buffer[..n]);
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => std::thread::sleep(POLL),
                    Err(e) => return Err(e.to_string()),
                }
            }
        }
    }
    struct Entry {
        child: Option<Child>,
        reaped: bool,
        channel: Channel,
        identity: Facts,
        self_exit: Instant,
    }
    impl Entry {
        fn wait(&mut self, end: Instant) -> QResult<bool> {
            loop {
                if let Some(child) = self.child.as_mut() {
                    if self.reaped || child.try_wait().map_err(|e| e.to_string())?.is_some() {
                        self.reaped = true;
                        return Ok(true);
                    }
                } else {
                    match facts(self.identity.pid) {
                        Err(e) if e.raw_os_error() == Some(3) => return Ok(true),
                        Ok(f) if !same_identity(&f, &self.identity) => return Ok(true),
                        Ok(_) => (),
                        Err(e) => return Err(e.to_string()),
                    }
                }
                if Instant::now() >= end {
                    return Ok(false);
                }
                std::thread::sleep(POLL);
            }
        }
        fn cleanup(&mut self, end: Instant, report: &mut String) -> bool {
            if self.reaped {
                return true;
            }
            let _ = self.channel.send("exit\n", end.min(Instant::now() + GRACE));
            if self.wait(end.min(Instant::now() + GRACE)).unwrap_or(false) {
                return true;
            }
            if self.child.is_some() && !self.reaped {
                for signal in [15, 9] {
                    report.push_str(&format!(
                        "direct unreaped fallback pid={} signal={signal}\n",
                        self.identity.pid
                    ));
                    // PID cannot be reused while our direct child remains unreaped.
                    let _ = host::fixture_pid_signal(self.identity.pid as u32, signal);
                    if self.wait(end.min(Instant::now() + GRACE)).unwrap_or(false) {
                        return true;
                    }
                }
                false
            } else {
                report.push_str("orphan channel/self-exit cleanup only; no PID signal\n");
                self.wait(end.min(self.self_exit + GRACE)).unwrap_or(false)
            }
        }
    }
    fn fixture_command(mode: &str) -> QResult<Command> {
        let mut command = Command::new(env::current_exe().map_err(|e| e.to_string())?);
        command
            .args(["--exact", "qualification::fixture", "--nocapture"])
            .env("ASURA_QUAL_MODE", mode)
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        Ok(command)
    }
    struct Case {
        entries: Vec<Entry>,
        end: Instant,
        report: String,
        safe: bool,
    }
    impl Case {
        fn new() -> Self {
            Self {
                entries: Vec::new(),
                end: Instant::now() + CASE,
                report: String::new(),
                safe: true,
            }
        }
        fn spawn(&mut self, mode: &str, socket: Option<&Path>) -> QResult<usize> {
            if self.entries.len() >= 6 || Instant::now() >= self.end {
                return Err("case launch bound".into());
            }
            let (parent, child) = UnixStream::pair().map_err(|e| e.to_string())?;
            let mut command = fixture_command(mode)?;
            command.stdin(Stdio::from(OwnedFd::from(child)));
            if let Some(path) = socket {
                command.env("ASURA_QUAL_SOCKET", path);
            }
            let channel = Channel::new(parent)?;
            let child = command.spawn().map_err(|e| e.to_string())?;
            let pid = child.id() as i32;
            // Register ownership before fallible readiness I/O, so every exit cleans the child.
            let index = self.entries.len();
            self.entries.push(Entry {
                child: Some(child),
                reaped: false,
                channel,
                identity: Facts {
                    pid,
                    parent: 0,
                    group: 0,
                    session: 0,
                    status: 0,
                    start: [0, 0],
                },
                self_exit: Instant::now() + FIXTURE_LIFE,
            });
            let ready = self.entries[index]
                .channel
                .line(self.end.min(Instant::now() + GRACE))?;
            let info = facts(pid).map_err(|e| e.to_string())?;
            if ready != format!("ready {pid} {} {}\n", info.start[0], info.start[1]) {
                return Err("fixture readiness identity mismatch".into());
            }
            self.report.push_str(&format!("ready {info:?}\n"));
            self.entries[index].identity = info;
            Ok(index)
        }
        fn cleanup(&mut self) -> bool {
            let end = self.end + GRACE + GRACE;
            for entry in self.entries.iter_mut().rev() {
                self.safe &= entry.cleanup(end, &mut self.report);
            }
            if !self.safe {
                super::tests::UNPROVED.store(true, Ordering::Relaxed);
            }
            self.safe
        }
    }
    impl Drop for Case {
        fn drop(&mut self) {
            self.cleanup();
        }
    }
    fn identity_line() -> QResult<String> {
        let f = facts(std::process::id() as i32).map_err(|e| e.to_string())?;
        Ok(format!("ready {} {} {}\n", f.pid, f.start[0], f.start[1]))
    }
    #[test]
    fn fixture() {
        let Ok(mode) = env::var("ASURA_QUAL_MODE") else {
            assert_eq!(std::mem::size_of::<BsdInfo>(), 136);
            assert_eq!(std::mem::align_of::<BsdInfo>(), 8);
            assert_eq!(std::mem::size_of::<AuditToken>(), 32);
            assert_eq!(std::mem::offset_of!(BsdInfo, start), 120);
            let f = Facts {
                pid: 42,
                parent: 1,
                group: 42,
                session: 7,
                status: 2,
                start: [1, 2],
            };
            assert_eq!(census_count(4, 16), Ok(Some(1)));
            assert_eq!(census_count(16, 16), Ok(None));
            for invalid in [-1, 0, 3, 17, 20] {
                assert!(census_count(invalid, 16).is_err());
            }
            assert_eq!(token_query_available(0, 8), Ok(true));
            assert_eq!(token_query_available(5, 0), Ok(false));
            assert!(token_query_available(0, 7).is_err());
            assert!(token_query_available(0, 9).is_err());
            assert!(valid_token_response(8, 42, &f, &f));
            assert!(!valid_token_response(7, 42, &f, &f));
            assert!(!valid_token_response(8, 43, &f, &f));
            let mut replaced = f.clone();
            replaced.start[1] += 1;
            assert!(!valid_token_response(8, 42, &f, &replaced));
            let now = Instant::now();
            assert!(admit(true, now, now + GRACE));
            assert!(!admit(false, now, now + GRACE));
            assert!(!admit(true, now, now));
            return;
        };
        let end = Instant::now() + FIXTURE_LIFE;
        // SAFETY: qualification spawn transfers exclusive ownership of its socket to stdin.
        let stream = unsafe { UnixStream::from_raw_fd(0) };
        let mut channel = Channel::new(stream).unwrap();
        if mode == "ignore" {
            // SAFETY: SIG_IGN is Darwin's constant 1; this fixture tests KILL only.
            unsafe {
                signal(15, 1);
            }
        }
        channel.send(&identity_line().unwrap(), end).unwrap();
        let mut nested = None;
        while Instant::now() < end {
            let command = match channel.line(end) {
                Ok(line) => line,
                Err(_) => break,
            };
            match command.as_str() {
                "exit\n" => break,
                "hold\n" => {
                    while Instant::now() < end {
                        std::thread::sleep(POLL);
                    }
                }
                "exec\n" => {
                    channel.send("exec-ready\n", end).unwrap();
                    // The fixed executable self-exits even if the coordinator fails.
                    let error = Command::new("/bin/sleep").arg("10").exec();
                    panic!("fixture exec failed: {error}");
                }
                "group\n" | "session\n" => {
                    // SAFETY: mutates only this cooperative fixture's POSIX identity.
                    let result = unsafe {
                        if command == "group\n" {
                            setpgid(0, 0)
                        } else {
                            setsid()
                        }
                    };
                    let errno = if result < 0 {
                        io::Error::last_os_error().raw_os_error()
                    } else {
                        None
                    };
                    let info = facts(std::process::id() as i32).unwrap();
                    channel
                        .send(
                            &format!("transition result={result} errno={errno:?} {info:?}\n"),
                            end,
                        )
                        .unwrap();
                }
                "spawn\n" if mode == "intermediate" && nested.is_none() => {
                    let socket = env::var_os("ASURA_QUAL_SOCKET").unwrap();
                    let child_channel = UnixStream::connect(socket).unwrap();
                    let mut child = fixture_command("orphan").unwrap();
                    child.stdin(Stdio::from(OwnedFd::from(child_channel)));
                    nested = Some(child.spawn().unwrap());
                    channel.send("spawned\n", end).unwrap();
                }
                _ => break,
            }
        }
        // Deliberately no wait of registered nested fixture: harness owns its channel/lifetime.
    }
    fn token_case(case: &mut Case, value: i32) -> QResult<()> {
        let target = case.spawn(if value == 9 { "ignore" } else { "direct" }, None)?;
        let acquired = acquire(&case.entries[target], &mut case.report)?;
        let Some(mut token) = acquired else {
            case.report.push_str("Q-A1 unavailable: token acquisition denied; cooperative cleanup\nQ-A2 skipped: no authentic token\n");
            return Ok(());
        };
        let result = token.current_signal(&case.entries[target], value)?;
        case.report
            .push_str(&format!("Q-A1 signal={value} returned_errno={result}\n"));
        if value == 0 || result != 0 {
            case.entries[target].channel.send("exit\n", case.end)?;
        }
        if !case.entries[target].wait(Instant::now() + GRACE)? {
            return Err("candidate target did not exit within bound".into());
        }
        let decoy = case.spawn("direct", None)?;
        let result = token.stale_zero();
        let current = facts(case.entries[decoy].identity.pid).map_err(|e| e.to_string())?;
        if !same_identity(&current, &case.entries[decoy].identity) {
            return Err("decoy changed".into());
        }
        case.report.push_str(&format!("Q-A2 authentic stale token signal0 returned_errno={result}; decoy unchanged; no forced PID reuse/destructive stale signal\n"));
        Ok(())
    }
    fn transition(case: &mut Case, index: usize, session: bool) -> QResult<()> {
        let before = facts(case.entries[index].identity.pid).map_err(|e| e.to_string())?;
        if session && before.group == before.pid as u32 {
            return Err("setsid fixture already group leader".into());
        }
        case.entries[index]
            .channel
            .send(if session { "session\n" } else { "group\n" }, case.end)?;
        let reply = case.entries[index].channel.line(case.end)?;
        let after = facts(before.pid).map_err(|e| e.to_string())?;
        case.report
            .push_str(&format!("Q-A3 before={before:?}; {reply}after={after:?}\n"));
        if !same_identity(&before, &after)
            || after.group != after.pid as u32
            || (session && after.session != after.pid)
            || (!session && before.session != after.session)
        {
            return Err("unexpected POSIX transition".into());
        }
        Ok(())
    }
    fn registered_nested(
        case: &mut Case,
        root: &Path,
        own_session: bool,
    ) -> QResult<(usize, usize)> {
        let path = root.join("registered.sock");
        let _ = fs::remove_file(&path);
        let listener = UnixListener::bind(&path).map_err(|e| e.to_string())?;
        listener.set_nonblocking(true).map_err(|e| e.to_string())?;
        let parent = case.spawn("intermediate", Some(&path))?;
        if own_session {
            transition(case, parent, true)?;
        }
        let before = census()?;
        // A partially delivered spawn command can create an unregistered child.
        // Keep cleanup unknown until the retained channel and identity are registered.
        case.safe = false;
        case.entries[parent].channel.send("spawn\n", case.end)?;
        let (stream, _) = loop {
            match listener.accept() {
                Ok(value) => break value,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock && Instant::now() < case.end => {
                    std::thread::sleep(POLL)
                }
                Err(e) => {
                    case.safe = false;
                    return Err(format!("unregistered descendant: {e}"));
                }
            }
        };
        let mut channel = Channel::new(stream)?;
        let ready = channel.line(case.end).map_err(|e| {
            case.safe = false;
            e
        })?;
        case.safe = false;
        let values: Vec<_> = ready.split_whitespace().collect();
        let pid: i32 = values
            .get(1)
            .ok_or("missing registered PID")?
            .parse()
            .map_err(|_| "invalid registered PID")?;
        let info = facts(pid).map_err(|e| {
            case.safe = false;
            e.to_string()
        })?;
        let index = case.entries.len();
        case.entries.push(Entry {
            child: None,
            reaped: false,
            channel,
            identity: info.clone(),
            self_exit: Instant::now() + FIXTURE_LIFE,
        });
        if ready != format!("ready {pid} {} {}\n", info.start[0], info.start[1]) {
            return Err("orphan identity mismatch".into());
        }
        case.safe = true;
        if case.entries[parent].channel.line(case.end)? != "spawned\n" {
            return Err("missing spawn receipt".into());
        }
        case.report.push_str(&format!(
            "census before registered birth omitted child={}\n",
            !before.contains(&pid)
        ));
        Ok((parent, index))
    }
    fn orphan_case(case: &mut Case, root: &Path, session: bool) -> QResult<()> {
        let (parent, index) = registered_nested(case, root, false)?;
        let pid = case.entries[index].identity.pid;
        transition(case, index, session)?;
        case.entries[parent].channel.send("exit\n", case.end)?;
        if !case.entries[parent].wait(Instant::now() + GRACE)? {
            return Err("intermediate did not exit".into());
        }
        let orphan = facts(pid).map_err(|e| e.to_string())?;
        case.report.push_str(&format!(
            "registered orphan={orphan:?}; no ancestry-only/empty-session completeness inference\n"
        ));
        Ok(())
    }
    #[test]
    fn observed_session_escape_stays_uncertain_after_token_cleanup() {
        let temp = super::tests::scratch();
        let run = WorktreeRun::acquire(&temp.0).unwrap();
        let mut case = Case::new();
        let result = (|| -> QResult<()> {
            let (parent, index) = registered_nested(&mut case, &temp.0, true)?;
            let anchor = case.entries[parent].identity.pid as u32;
            let target = case.entries[index].identity.clone();
            let mut scope = ProcessScope::new(anchor, false);
            scope.scan(case.end, None, false)?;
            if !scope.members.contains_key(&(target.pid, target.start)) {
                return Err("descendant was not observed before escape".into());
            }
            transition(&mut case, index, true)?;
            scope.scan(case.end, Some(15), false)?;
            if !scope
                .fault
                .as_ref()
                .is_some_and(|fault| fault.contains("unsupported session escape"))
            {
                return Err("observed escape did not retain uncertainty".into());
            }
            if !case.entries[parent].wait(Instant::now() + GRACE)?
                || !case.entries[index].wait(Instant::now() + GRACE)?
            {
                return Err("token cleanup did not establish actual fixture exit".into());
            }
            if run.settle(scope.fault.is_none()).unwrap_err().code != "cleanup_required"
                || !temp.0.join(".build/asura-toolchain.incomplete").exists()
            {
                return Err("escape did not retain marker".into());
            }
            Ok(())
        })();
        let settled = case.cleanup();
        assert!(settled, "escape fixture cleanup unknown: {result:?}");
        assert!(result.is_ok(), "escape regression: {result:?}");
    }
    #[test]
    fn same_lifetime_partial_facts_remain_pending_without_signal() {
        let mut case = Case::new();
        let result = (|| -> QResult<()> {
            let index = case.spawn("direct", None)?;
            transition(&mut case, index, true)?;
            let pid = case.entries[index].identity.pid;
            let key = (pid, case.entries[index].identity.start);
            let mut scope = ProcessScope::new(pid as u32, false);
            scope.scan(case.end, None, false)?;
            for status in [2, 5] {
                RACE_STATUS.store(status, Ordering::Relaxed);
                FACTS_RACE.store(pid, Ordering::Relaxed);
                let (clear, _) = scope.scan(case.end, Some(15), false)?;
                let member = &scope.members[&key];
                if clear
                    || !member.pending
                    || member.gone
                    || member.sent != 0
                    || scope.fault.is_some()
                {
                    return Err(format!(
                        "partial facts incorrectly accepted status={status}"
                    ));
                }
                if scope.scan(Instant::now(), Some(15), false).is_ok() {
                    return Err("pending observation received a fresh deadline".into());
                }
                scope.scan(case.end, None, false)?;
                if scope.members[&key].pending {
                    return Err("later complete facts did not resolve pending observation".into());
                }
            }
            scope.scan(case.end, Some(15), false)?;
            if !case.entries[index].wait(Instant::now() + GRACE)? {
                return Err("fixture did not exit after validated token cleanup".into());
            }
            Ok(())
        })();
        FACTS_RACE.store(0, Ordering::Relaxed);
        let settled = case.cleanup();
        assert!(settled, "partial-facts fixture cleanup unknown: {result:?}");
        assert!(result.is_ok(), "partial facts regression: {result:?}");
    }
    #[test]
    fn owned_exec_refreshes_token_and_settles() {
        let mut case = Case::new();
        let result = (|| -> QResult<()> {
            let index = case.spawn("direct", None)?;
            transition(&mut case, index, true)?;
            let before = facts(case.entries[index].identity.pid).map_err(|e| e.to_string())?;
            let key = (before.pid, before.start);
            let mut scope = ProcessScope::new(before.pid as u32, false);
            scope.scan(case.end, None, false)?;
            let generation = scope
                .members
                .get(&key)
                .and_then(|member| member.token.as_ref())
                .ok_or("initial scope token missing")?
                .generation;
            case.entries[index].channel.send("exec\n", case.end)?;
            if case.entries[index].channel.line(case.end)? != "exec-ready\n" {
                return Err("missing exec readiness".into());
            }
            let observe_end = case.end.min(Instant::now() + GRACE);
            loop {
                if Instant::now() >= observe_end {
                    return Err("new execution generation not observed".into());
                }
                let current = facts(before.pid).map_err(|e| e.to_string())?;
                if !same_identity(&before, &current) || current.session != before.session {
                    return Err("exec changed owned lifetime/session".into());
                }
                if let Some(token) = ProcessScope::certify(&current, observe_end)?
                    && token.generation != generation
                {
                    break;
                }
                std::thread::sleep(POLL);
            }
            let cleanup_end = case.end.min(Instant::now() + GRACE);
            scope.scan(cleanup_end, Some(15), false)?;
            let member = scope.members.get(&key).ok_or("owned member lost")?;
            if member
                .token
                .as_ref()
                .is_none_or(|token| token.generation == generation)
                || member.sent != 15
                || scope.fault.is_some()
            {
                return Err("fresh execution token was not used for TERM".into());
            }
            if !case.entries[index].wait(cleanup_end)? {
                return Err("exec fixture did not actually exit after token TERM".into());
            }
            Ok(())
        })();
        let settled = case.cleanup();
        assert!(
            settled,
            "exec fixture cleanup unknown: {result:?} {}",
            case.report
        );
        assert!(
            result.is_ok(),
            "exec refresh failed: {result:?} {}",
            case.report
        );
    }
    #[test]
    fn api_packet() {
        let mut root = super::tests::scratch();
        root.1 = true;
        let end = Instant::now() + Duration::from_secs(300);
        let mut report =
            String::from("Bounded API qualification; not production settlement proof\n");
        let mut failures = Vec::new();
        for name in [
            "signal0",
            "term",
            "kill",
            "group",
            "session",
            "orphan-group",
            "orphan-session",
            "self-exit",
            "fallback",
        ] {
            if !admit(
                !super::tests::UNPROVED.load(Ordering::Relaxed),
                Instant::now(),
                end,
            ) {
                failures.push("later cases skipped: deadline/unknown cleanup".to_owned());
                break;
            }
            let mut case = Case::new();
            case.end = case.end.min(end);
            let result = match name {
                "signal0" => token_case(&mut case, 0),
                "term" => token_case(&mut case, 15),
                "kill" => token_case(&mut case, 9),
                "group" | "session" => case
                    .spawn("direct", None)
                    .and_then(|i| transition(&mut case, i, name == "session")),
                "orphan-group" | "orphan-session" => {
                    orphan_case(&mut case, &root.0, name == "orphan-session")
                }
                "self-exit" | "fallback" => (|| {
                    let i = case.spawn(
                        if name == "fallback" {
                            "ignore"
                        } else {
                            "direct"
                        },
                        None,
                    )?;
                    case.entries[i].channel.send("hold\n", case.end)?;
                    if name == "self-exit" {
                        if !case.entries[i].wait(case.end)? {
                            return Err("fixture self-exit failed".into());
                        }
                        case.report.push_str("Q-A5 hard self-exit completed\n");
                    } else {
                        return Err("injected fixture work failure".into());
                    }
                    Ok(())
                })(),
                _ => unreachable!(),
            };
            let settled = case.cleanup();
            report.push_str(&format!(
                "case={name} result={result:?} cleanup={settled}\n{}",
                case.report
            ));
            if let Err(error) = result {
                if name != "fallback" || error != "injected fixture work failure" {
                    failures.push(format!("{name}: {error}"));
                } else if !case.report.contains("signal=9") {
                    failures.push("Q-A5 expected direct KILL fallback was not observed".into());
                }
            }
            if report.len() > REPORT_CAP {
                failures.push("report limit".into());
                break;
            }
            if !settled {
                failures.push("unknown cleanup; later cases skipped".into());
                break;
            }
        }
        report.push_str(
            "Q-A4 unsupported: no qualified sandbox enforcement rule; no invented profile tested\n",
        );
        let right_failure = RIGHT_FAILURE.load(Ordering::Relaxed);
        if right_failure != 0 {
            failures.push(format!("Mach right release failed {right_failure}"));
        }
        fs::write(root.0.join("qualification.txt"), &report).unwrap();
        emit(
            1,
            format!("{}\nqualification evidence: {}\n", report, root.0.display()).as_bytes(),
            Instant::now() + GRACE,
        )
        .unwrap();
        assert!(failures.is_empty(), "qualification failures: {failures:?}");
    }
}

/// Canonical macOS process facts and kernel-token adapter; no cache or admission policy.
mod process_api {
    use super::*;
    pub(super) type QResult<T> = std::result::Result<T, String>;
    #[repr(C)]
    #[derive(Clone, Copy)]
    pub(super) struct BsdInfo {
        leading: [u32; 12],
        names: [u8; 48],
        trailing: [u32; 6],
        pub(super) start: [u64; 2],
    }
    #[repr(C)]
    #[derive(Clone, Copy)]
    pub(super) struct AuditToken([u32; 8]);
    #[repr(C)]
    struct WaitInfo {
        signo: i32,
        error: i32,
        code: i32,
        pid: i32,
        uid: u32,
        status: i32,
        opaque: [u64; 10],
    }
    const _: () = assert!(std::mem::size_of::<BsdInfo>() == 136);
    const _: () = assert!(std::mem::size_of::<AuditToken>() == 32);
    const _: () = assert!(std::mem::size_of::<WaitInfo>() == 104);
    const _: () = assert!(std::mem::align_of::<WaitInfo>() == 8);
    const _: () = assert!(std::mem::offset_of!(WaitInfo, pid) == 12);
    #[link(name = "proc")]
    unsafe extern "C" {
        static mach_task_self_: u32;
        fn task_name_for_pid(task: u32, pid: i32, port: *mut u32) -> i32;
        fn task_info(port: u32, flavor: u32, data: *mut i32, count: *mut u32) -> i32;
        fn mach_port_deallocate(task: u32, port: u32) -> i32;
        fn proc_pidinfo(
            pid: i32,
            flavor: i32,
            arg: u64,
            buf: *mut std::ffi::c_void,
            size: i32,
        ) -> i32;
        fn proc_listpids(kind: u32, info: u32, buf: *mut std::ffi::c_void, size: i32) -> i32;
        fn proc_signal_with_audittoken(token: *mut AuditToken, signal: i32) -> i32;
        fn getsid(pid: i32) -> i32;
        pub(super) fn setsid() -> i32;
        #[cfg(test)]
        pub(super) fn setpgid(pid: i32, pgid: i32) -> i32;
        #[cfg(test)]
        pub(super) fn signal(number: i32, handler: usize) -> usize;
        fn waitid(kind: i32, pid: u32, info: *mut WaitInfo, options: i32) -> i32;
    }
    #[link(name = "bsm")]
    unsafe extern "C" {
        fn audit_token_to_pid(token: AuditToken) -> i32;
        fn audit_token_to_pidversion(token: AuditToken) -> i32;
    }
    #[derive(Clone, Debug, PartialEq, Eq)]
    pub(super) struct Facts {
        pub(super) pid: i32,
        pub(super) parent: u32,
        pub(super) group: u32,
        pub(super) session: i32,
        pub(super) status: u32,
        pub(super) start: [u64; 2],
    }
    pub(super) fn session(pid: i32) -> io::Result<i32> {
        // SAFETY: read-only syscall; zero is used only by deliberate fixture observations.
        let sid = unsafe { getsid(pid) };
        if sid < 0 {
            Err(io::Error::last_os_error())
        } else if sid == 0 {
            Err(io::Error::other("invalid zero SID"))
        } else {
            Ok(sid)
        }
    }
    pub(super) fn bsd(pid: i32) -> io::Result<Facts> {
        let mut info = BsdInfo {
            leading: [0; 12],
            names: [0; 48],
            trailing: [0; 6],
            start: [0; 2],
        };
        // SAFETY: exact installed proc_bsdinfo layout and complete writable buffer.
        let n = unsafe { proc_pidinfo(pid, 3, 1, (&mut info as *mut BsdInfo).cast(), 136) };
        if n == 0 {
            return Err(io::Error::last_os_error());
        }
        if n != 136 || info.leading[3] != pid as u32 {
            return Err(io::Error::other("invalid BSD record"));
        }
        #[cfg(test)]
        if BSD_RACE
            .compare_exchange(pid, 0, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
        {
            info.leading[1] = RACE_STATUS.load(Ordering::Relaxed) as u32;
        }
        Ok(Facts {
            pid,
            parent: info.leading[4],
            group: info.trailing[1],
            session: 0,
            status: info.leading[1],
            start: info.start,
        })
    }
    #[cfg(test)]
    pub(super) static INSPECTION_FAULT: AtomicI32 = AtomicI32::new(0);
    #[cfg(test)]
    pub(super) static FACTS_RACE: AtomicI32 = AtomicI32::new(0);
    #[cfg(test)]
    static BSD_RACE: AtomicI32 = AtomicI32::new(0);
    #[cfg(test)]
    pub(super) static RACE_STATUS: AtomicI32 = AtomicI32::new(0);
    pub(super) fn facts(pid: i32) -> io::Result<Facts> {
        #[cfg(test)]
        if INSPECTION_FAULT
            .compare_exchange(-1, pid, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
        {
            return Err(io::Error::from_raw_os_error(1));
        }
        let mut f = bsd(pid)?;
        #[cfg(test)]
        if FACTS_RACE
            .compare_exchange(pid, 0, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
        {
            // Inject precisely BSD success then session ESRCH, with one fresh BSD status.
            BSD_RACE.store(pid, Ordering::Relaxed);
            return Err(io::Error::from_raw_os_error(3));
        }
        match session(pid) {
            Ok(sid) => f.session = sid,
            Err(e) if e.raw_os_error() == Some(3) && f.status == 5 => (),
            Err(e) => return Err(e),
        }
        Ok(f)
    }
    pub(super) fn same_identity(a: &Facts, b: &Facts) -> bool {
        a.pid == b.pid && a.start == b.start
    }
    pub(super) fn census_count(n: i32, bytes: i32) -> QResult<Option<usize>> {
        if n <= 0 || n % 4 != 0 || n > bytes {
            return Err(format!("invalid census byte count {n}"));
        }
        Ok((n < bytes).then_some(n as usize / 4))
    }
    pub(super) fn census_until(end: Instant) -> QResult<Vec<i32>> {
        for size in [4096usize, 16384, 65536] {
            if Instant::now() >= end {
                return Err("census deadline".into());
            }
            let mut pids = vec![0i32; size];
            let bytes = (size * 4) as i32;
            // SAFETY: buffer holds exactly the stated number of bytes.
            let n = unsafe { proc_listpids(1, 0, pids.as_mut_ptr().cast(), bytes) };
            let Some(count) = census_count(n, bytes)? else {
                continue;
            };
            pids.truncate(count);
            if pids.iter().any(|p| *p < 0) {
                return Err("negative census PID".into());
            }
            pids.retain(|p| *p > 0);
            pids.sort_unstable();
            pids.dedup();
            return Ok(pids);
        }
        Err("census capacity exceeded".into())
    }
    #[cfg(test)]
    pub(super) fn census() -> QResult<Vec<i32>> {
        census_until(Instant::now() + GRACE)
    }
    pub(super) static RIGHT_FAILURE: AtomicI32 = AtomicI32::new(0);
    struct Port(u32);
    impl Port {
        fn close(mut self) -> QResult<()> {
            // SAFETY: this guard exclusively owns a valid acquired Mach send right.
            let result = unsafe { mach_port_deallocate(mach_task_self_, self.0) };
            self.0 = 0;
            if result == 0 {
                Ok(())
            } else {
                RIGHT_FAILURE.store(result, Ordering::Relaxed);
                Err(format!("Mach right release {result}"))
            }
        }
    }
    impl Drop for Port {
        fn drop(&mut self) {
            if self.0 != 0 {
                // SAFETY: guard retains the right until this release, including unwinding.
                let result = unsafe { mach_port_deallocate(mach_task_self_, self.0) };
                if result != 0 {
                    RIGHT_FAILURE.store(result, Ordering::Relaxed);
                }
            }
        }
    }
    pub(super) fn token_query_available(kr: i32, count: u32) -> QResult<bool> {
        if kr != 0 {
            return Ok(false);
        }
        if count != 8 {
            return Err("audit token word count mismatch".into());
        }
        Ok(true)
    }
    pub(super) fn valid_token_response(
        count: u32,
        pid: i32,
        before: &Facts,
        after: &Facts,
    ) -> bool {
        count == 8 && pid == before.pid && same_identity(before, after)
    }
    pub(super) struct KernelToken {
        pub(super) token: AuditToken,
        pub(super) identity: Facts,
        pub(super) generation: i32,
    }
    pub(super) fn acquire_identity(expected: &Facts) -> QResult<KernelToken> {
        let before = facts(expected.pid).map_err(|e| e.to_string())?;
        if !same_identity(expected, &before) {
            return Err("token identity changed".into());
        }
        let mut raw = 0;
        // SAFETY: valid output slot and currently observed candidate PID; no signal here.
        let kr = unsafe { task_name_for_pid(mach_task_self_, before.pid, &mut raw) };
        let port = Port(raw);
        if kr != 0 {
            return Err(format!("token unavailable: task_name kr={kr}"));
        }
        if raw == 0 {
            return Err("null task-name right".into());
        }
        let mut token = AuditToken([0; 8]);
        let mut count = 8;
        // SAFETY: eight-word writable kernel token buffer and acquired name right.
        let kr = unsafe { task_info(raw, 15, token.0.as_mut_ptr().cast(), &mut count) };
        port.close()?;
        if !token_query_available(kr, count)? {
            return Err(format!(
                "token unavailable: task_info kr={kr} count={count}"
            ));
        }
        // SAFETY: both accessors consume authentic kernel-returned token by value.
        let (pid, generation) =
            unsafe { (audit_token_to_pid(token), audit_token_to_pidversion(token)) };
        let after = facts(before.pid).map_err(|e| e.to_string())?;
        if !valid_token_response(count, pid, &before, &after) {
            return Err("token identity mismatch".into());
        }
        Ok(KernelToken {
            token,
            identity: after,
            generation,
        })
    }
    pub(super) unsafe fn signal_token_raw(token: &mut AuditToken, value: i32) -> i32 {
        // SAFETY: caller supplies authentic token; test-only caller may request harmless zero.
        unsafe { proc_signal_with_audittoken(token, value) }
    }
    impl KernelToken {
        pub(super) fn send(&mut self, value: i32) -> QResult<bool> {
            if ![15, 9].contains(&value) {
                return Err("unsupported token signal".into());
            }
            // SAFETY: token came only from validated kernel acquisition; no PID fallback.
            let result = unsafe { signal_token_raw(&mut self.token, value) };
            if result == 0 {
                Ok(true)
            } else if result == 3 {
                Ok(false)
            } else {
                Err(format!("token signal errno={result}"))
            }
        }
    }
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(super) struct ExitObservation {
        code: i32,
        status: i32,
    }
    pub(super) fn observe_exit(pid: u32, end: Instant) -> QResult<Option<ExitObservation>> {
        loop {
            if Instant::now() >= end {
                return Err("exit observation deadline".into());
            }
            let mut info = WaitInfo {
                signo: 0,
                error: 0,
                code: 0,
                pid: 0,
                uid: 0,
                status: 0,
                opaque: [0; 10],
            };
            // SAFETY: complete zeroed siginfo_t; WNOWAIT deliberately keeps child unreaped.
            let result = unsafe { waitid(1, pid, &mut info, 4 | 1 | 32) };
            if result < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(format!("waitid: {error}"));
            }
            if info.pid == 0 {
                return Ok(None);
            }
            if info.pid != pid as i32 || ![1, 2, 3].contains(&info.code) {
                return Err("invalid waitid event".into());
            }
            return Ok(Some(ExitObservation {
                code: info.code,
                status: info.status,
            }));
        }
    }
    impl ExitObservation {
        pub(super) fn success(self) -> bool {
            self.code == 1 && self.status == 0
        }
        pub(super) fn matches(self, status: ExitStatus) -> bool {
            use std::os::unix::process::ExitStatusExt;
            if self.code == 1 {
                status.code() == Some(self.status)
            } else {
                status.signal() == Some(self.status) && status.core_dumped() == (self.code == 3)
            }
        }
    }
}

struct ObservedMember {
    pending: bool,
    facts: process_api::Facts,
    token: Option<process_api::KernelToken>,
    gone: bool,
    sent: i32,
    esrch_generation: Option<i32>,
}
struct ProcessScope {
    anchor: u32,
    delegated: bool,
    members: std::collections::BTreeMap<(i32, [u64; 2]), ObservedMember>,
    fault: Option<String>,
}
impl ProcessScope {
    fn new(anchor: u32, delegated: bool) -> Self {
        Self {
            anchor,
            delegated,
            members: std::collections::BTreeMap::new(),
            fault: None,
        }
    }
    fn uncertain(&mut self, error: String) {
        if self.fault.is_none() {
            self.fault = Some(error)
        }
    }
    fn old_identity_gone(
        f: &process_api::Facts,
        end: Instant,
    ) -> std::result::Result<bool, String> {
        let pids = process_api::census_until(end)?;
        match process_api::bsd(f.pid) {
            Ok(current) if !process_api::same_identity(&current, f) => {
                Err(format!("process lifetime replaced pid={}", f.pid))
            }
            Ok(_) => Ok(false),
            Err(e) if e.raw_os_error() == Some(3) && !pids.contains(&f.pid) => Ok(true),
            Err(e) => Err(format!("identity absence unproved: {e}")),
        }
    }
    fn certify(
        f: &process_api::Facts,
        end: Instant,
    ) -> std::result::Result<Option<process_api::KernelToken>, String> {
        if Instant::now() >= end {
            return Err("token deadline".into());
        }
        let first = process_api::acquire_identity(f)?;
        let between = process_api::facts(f.pid).map_err(|e| e.to_string())?;
        if Instant::now() >= end {
            return Err("token deadline".into());
        }
        let second = process_api::acquire_identity(&between)?;
        if !process_api::same_identity(f, &between)
            || !process_api::same_identity(&first.identity, &second.identity)
        {
            return Err("process lifetime changed during token inspection".into());
        }
        if first.identity.session != f.session
            || between.session != f.session
            || second.identity.session != f.session
        {
            return Err("session changed during token inspection".into());
        }
        // Exec changes audit execution generation while preserving the owned lifetime.
        // Retry only at the caller's next bounded observation; never signal this pair.
        if first.generation != second.generation {
            return Ok(None);
        }
        Ok(Some(second))
    }
    /// Returns (observed scope clear, live residual other than the anchor).
    fn scan(
        &mut self,
        end: Instant,
        phase: Option<i32>,
        exited: bool,
    ) -> std::result::Result<(bool, bool), String> {
        if process_api::RIGHT_FAILURE.load(Ordering::Relaxed) != 0 {
            return Err("Mach right release failed".into());
        }
        let pids = process_api::census_until(end)?;
        let census: std::collections::BTreeSet<_> = pids.iter().copied().collect();
        let known: Vec<_> = self.members.keys().copied().collect();
        for key in known {
            if Instant::now() >= end {
                return Err("scope inspection deadline".into());
            }
            if self.members[&key].gone || (exited && key.0 == self.anchor as i32) {
                continue;
            }
            self.members.get_mut(&key).unwrap().pending = false;
            let old = &self.members[&key].facts;
            match process_api::bsd(key.0) {
                Ok(now) if !process_api::same_identity(old, &now) => {
                    return Err(format!(
                        "owned process lifetime replaced pid={} anchor={}",
                        key.0, self.anchor
                    ));
                }
                Err(e) if e.raw_os_error() == Some(3) && !census.contains(&key.0) => {
                    self.members.get_mut(&key).unwrap().gone = true;
                }
                Err(e) if e.raw_os_error() == Some(3) => {
                    self.members.get_mut(&key).unwrap().pending = true;
                }
                Err(e) => {
                    if Self::old_identity_gone(old, end).unwrap_or(false) {
                        self.members.get_mut(&key).unwrap().gone = true;
                    } else {
                        return Err(format!(
                            "known member lookup pid={} anchor={} known=true previous_sid={} phase={phase:?}: {e}",
                            key.0, self.anchor, old.session
                        ));
                    }
                }
                Ok(_) => (),
            }
        }
        let mut candidates = pids;
        if !candidates.contains(&(self.anchor as i32)) {
            candidates.push(self.anchor as i32)
        }
        for old in self.members.values().filter(|m| !m.gone) {
            if !candidates.contains(&old.facts.pid) {
                candidates.push(old.facts.pid)
            }
        }
        for pid in candidates {
            if exited && pid == self.anchor as i32 {
                continue;
            }
            if self
                .members
                .values()
                .any(|m| !m.gone && m.pending && m.facts.pid == pid)
            {
                continue;
            }
            if Instant::now() >= end {
                return Err("scope inspection deadline".into());
            }
            let known = self.members.values().any(|m| !m.gone && m.facts.pid == pid);
            let mut initial_sid = None;
            if !known && pid != self.anchor as i32 {
                match process_api::session(pid) {
                    Ok(sid) if sid != self.anchor as i32 => continue,
                    Err(e) if e.raw_os_error() == Some(3) => continue,
                    Err(e) => {
                        return Err(format!(
                            "candidate session pid={pid} anchor={} known={known} phase={phase:?}: {e}",
                            self.anchor
                        ));
                    }
                    Ok(sid) => initial_sid = Some(sid),
                }
            }
            let mut f = match process_api::facts(pid) {
                Ok(f) => f,
                Err(e) if e.raw_os_error() == Some(3) => {
                    // Unknown candidates can disappear between the census and identity read.
                    if !known && pid != self.anchor as i32 {
                        continue;
                    }
                    if let Some(old) = self
                        .members
                        .values()
                        .find(|m| !m.gone && m.facts.pid == pid)
                        .map(|m| m.facts.clone())
                    {
                        let fresh = process_api::bsd(pid);
                        match fresh {
                            Ok(ref now) if process_api::same_identity(&old, now) => {
                                self.members.get_mut(&(old.pid, old.start)).unwrap().pending = true;
                                continue;
                            }
                            _ if Self::old_identity_gone(&old, end).unwrap_or(false) => {
                                self.members.get_mut(&(old.pid, old.start)).unwrap().gone = true;
                                continue;
                            }
                            Err(ref error) if error.raw_os_error() == Some(3) => {
                                self.members.get_mut(&(old.pid, old.start)).unwrap().pending = true;
                                continue;
                            }
                            _ => {
                                return Err(format!(
                                    "known process facts unavailable pid={pid} anchor={} known={known} initial_sid={initial_sid:?} phase={phase:?}: {e}; fresh BSD={fresh:?}",
                                    self.anchor
                                ));
                            }
                        }
                    } else {
                        // The unreaped anchor can become a zombie between waitid and BSD.
                        // Only the next waitid observation can establish its exit.
                        continue;
                    }
                }
                Err(e) => {
                    return Err(format!(
                        "process facts pid={pid} anchor={} known={known} initial_sid={initial_sid:?} phase={phase:?}: {e}",
                        self.anchor
                    ));
                }
            };
            if self.members.values().any(|member| {
                !member.gone
                    && member.facts.pid == pid
                    && !process_api::same_identity(&member.facts, &f)
            }) {
                return Err(format!(
                    "owned process lifetime replaced during scan pid={pid} anchor={}",
                    self.anchor
                ));
            }
            let key = (f.pid, f.start);
            if !self.members.contains_key(&key) {
                // A preliminary SID read does not own a PID that was reused before facts.
                if pid != self.anchor as i32 && f.session != self.anchor as i32 {
                    continue;
                }
                if self.members.len() >= 4096 {
                    return Err("retained process bound".into());
                }
                let mut pending = false;
                let initial = if f.status != 5 && !(pid == self.anchor as i32 && exited) {
                    match Self::certify(&f, end) {
                        Ok(Some(token)) if token.identity.session == self.anchor as i32 => {
                            Some(token)
                        }
                        Ok(None) => {
                            pending = true;
                            None
                        }
                        Ok(Some(_)) => {
                            return Err(
                                "candidate session changed before ownership qualification".into()
                            );
                        }
                        Err(error) => match process_api::bsd(pid) {
                            Ok(now) if process_api::same_identity(&f, &now) && now.status == 5 => {
                                f = now;
                                None
                            }
                            _ if Self::old_identity_gone(&f, end).unwrap_or(false) => continue,
                            Err(e) if e.raw_os_error() == Some(3) => {
                                pending = true;
                                None
                            }
                            _ => {
                                return Err(format!(
                                    "token qualification pid={pid} anchor={} known={known} initial_sid={initial_sid:?} phase={phase:?}: {error}",
                                    self.anchor
                                ));
                            }
                        },
                    }
                } else {
                    None
                };
                self.members.insert(
                    key,
                    ObservedMember {
                        pending,
                        facts: f.clone(),
                        token: initial,
                        gone: false,
                        sent: 0,
                        esrch_generation: None,
                    },
                );
            }
            if self.members[&key].pending {
                continue;
            }
            if f.status != 5 && f.session != self.anchor as i32 && !self.delegated {
                self.uncertain(format!("observed unsupported session escape pid={pid}"));
            }
            if f.status == 5 || (pid == self.anchor as i32 && exited) {
                self.members.get_mut(&key).unwrap().facts = f;
                continue;
            }
            let need = self.members[&key].token.is_none() || phase.is_some();
            if need {
                match Self::certify(&f, end) {
                    Ok(None) => {
                        self.members.get_mut(&key).unwrap().pending = true;
                        continue;
                    }
                    Ok(Some(token)) => {
                        if token.identity.session != self.anchor as i32 && !self.delegated {
                            self.uncertain(format!(
                                "observed unsupported session escape pid={pid}"
                            ));
                        }
                        let member = self.members.get_mut(&key).unwrap();
                        if member.esrch_generation == Some(token.generation) {
                            return Err(format!(
                                "signal ESRCH for still-live unchanged execution pid={pid}"
                            ));
                        }
                        if member
                            .token
                            .as_ref()
                            .is_some_and(|previous| previous.generation != token.generation)
                        {
                            member.sent = 0;
                            member.esrch_generation = None;
                        }
                        member.token = Some(token);
                    }
                    Err(error) => {
                        if let Ok(now) = process_api::bsd(pid)
                            && process_api::same_identity(&f, &now)
                            && now.status == 5
                        {
                            self.members.get_mut(&key).unwrap().facts = now;
                            continue;
                        }
                        if Self::old_identity_gone(&f, end).unwrap_or(false) {
                            self.members.get_mut(&key).unwrap().gone = true;
                            continue;
                        }
                        if process_api::bsd(pid).is_err_and(|e| e.raw_os_error() == Some(3)) {
                            self.members.get_mut(&key).unwrap().pending = true;
                            continue;
                        }
                        return Err(format!(
                            "retained token qualification pid={pid} anchor={} known={known} initial_sid={initial_sid:?} phase={phase:?}: {error}",
                            self.anchor
                        ));
                    }
                }
            }
            let member = self.members.get_mut(&key).unwrap();
            member.facts = f;
            if let Some(value) = phase
                && member.sent != value
            {
                let token = member.token.as_mut().ok_or("missing signal token")?;
                if token.send(value)? {
                    member.sent = value;
                } else {
                    member.esrch_generation = Some(token.generation);
                    member.pending = true;
                }
            }
        }
        let mut clear = exited;
        let mut live_residual = false;
        for member in self.members.values().filter(|m| !m.gone) {
            if member.facts.pid == self.anchor as i32 && exited {
                continue;
            }
            clear = false;
            if member.facts.pid != self.anchor as i32 && member.facts.status != 5 && !member.pending
            {
                live_residual = true
            }
        }
        Ok((clear, live_residual))
    }
}
