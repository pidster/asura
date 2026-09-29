//! Bounded Git observation and recursive macOS filesystem signals.
//! Call only from a retained IO worker, never the control reactor.
use crate::ProjectIdentity;
use std::{
    ffi::{CString, c_void},
    fs,
    io::{self, Read},
    os::{fd::AsRawFd, unix::fs::MetadataExt},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GitState {
    Unknown,
    NonRepository,
    Clean,
    Dirty,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GitSnapshot {
    pub state: GitState,
    pub branch: Option<String>,
    pub detached: bool,
    pub unborn: bool,
    pub conflicts: bool,
    pub files_changed: Option<u64>,
    pub added: Option<u64>,
    pub deleted: Option<u64>,
    pub reason: Option<&'static str>,
}
impl GitSnapshot {
    pub fn unknown(reason: &'static str) -> Self {
        Self {
            state: GitState::Unknown,
            branch: None,
            detached: false,
            unborn: false,
            conflicts: false,
            files_changed: None,
            added: None,
            deleted: None,
            reason: Some(reason),
        }
    }
}

pub fn validate_scope(root: &str, path: &str, device: u64, inode: u64) -> Result<(), &'static str> {
    if !Path::new(path).starts_with(root) || Path::new(path).components().count() > 64 {
        return Err("context_outside_project");
    }
    let identity = ProjectIdentity::open(root).map_err(|_| "project_identity_unavailable")?;
    if identity.device != device || identity.inode != inode {
        return Err("project_identity_changed");
    }
    ProjectIdentity::open(path).map_err(|_| "context_identity_unavailable")?;
    Ok(())
}

/// No parent search can cross the registered project boundary.
fn repository(root: &str, path: &str) -> Result<Option<PathBuf>, &'static str> {
    for directory in Path::new(path).ancestors().take(64) {
        if !directory.starts_with(root) {
            break;
        }
        let metadata = directory.join(".git");
        match fs::symlink_metadata(&metadata) {
            Ok(value) if value.file_type().is_dir() => {
                for unsupported in [
                    "commondir",
                    "objects/info/alternates",
                    "objects/info/http-alternates",
                ] {
                    match fs::symlink_metadata(metadata.join(unsupported)) {
                        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                        _ => return Err("git_external_metadata_unsupported"),
                    }
                }
                return Ok(Some(directory.to_owned()));
            }
            Ok(_) => return Err("git_metadata_unsupported"),
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(_) => return Err("git_metadata_unavailable"),
        }
        if directory == Path::new(root) {
            break;
        }
    }
    Ok(None)
}

/// Fixed developer-tool discovery. Never resolve an executable using repository PATH.
pub fn git_executable(cancelled: &AtomicBool) -> Result<PathBuf, &'static str> {
    let mut command = Command::new("/usr/bin/xcrun");
    command.env_clear().args(["--find", "git"]);
    let output = run(
        command,
        Instant::now() + Duration::from_secs(2),
        cancelled,
        4096,
    )?;
    let path = std::str::from_utf8(&output)
        .map_err(|_| "git_unavailable")?
        .trim();
    if !path.starts_with('/') || path.contains('\n') {
        return Err("git_unavailable");
    }
    let metadata = fs::metadata(path).map_err(|_| "git_unavailable")?;
    if !metadata.is_file() || metadata.uid() != 0 || metadata.mode() & 0o022 != 0 {
        return Err("git_executable_unsafe");
    }
    Ok(PathBuf::from(path))
}

pub(crate) fn quoted(value: &str) -> Result<String, &'static str> {
    if value.chars().any(char::is_control) {
        return Err("git_path_invalid");
    }
    Ok(format!(
        "\"{}\"",
        value.replace('\\', "\\\\").replace('"', "\\\"")
    ))
}

pub fn collect_git(
    root: &str,
    path: &str,
    device: u64,
    inode: u64,
    git: &Path,
    cancelled: &AtomicBool,
) -> GitSnapshot {
    fn collect(
        root: &str,
        path: &str,
        device: u64,
        inode: u64,
        git: &Path,
        cancelled: &AtomicBool,
    ) -> Result<GitSnapshot, &'static str> {
        validate_scope(root, path, device, inode)?;
        let Some(repository) = repository(root, path)? else {
            let mut snapshot = GitSnapshot::unknown("unused");
            snapshot.state = GitState::NonRepository;
            snapshot.reason = None;
            return Ok(snapshot);
        };
        let executable = quoted(git.to_str().ok_or("git_path_invalid")?)?;
        let scope = quoted(root)?;
        // Default denial includes network, file writes and arbitrary subprocess execution.
        // Git reads system runtime code and this project's tree only.
        let profile = format!(
            "(version 1)(deny default)(import \"dyld-support.sb\")\
            (allow process-exec (literal {executable}))\
            (allow sysctl-read)(allow mach-lookup)\
            (allow file-write* (literal \"/dev/null\"))\
            (allow file-read* (subpath {scope})(literal \"/dev/null\"))\
            (allow file-read-metadata (path-ancestors {scope}))\
            (allow file-read* file-map-executable (literal {executable})\
            (subpath \"/System/Library\")(subpath \"/usr/lib\")\
            (subpath \"/System/Volumes/Preboot/Cryptexes/OS/System/Library\")\
            (subpath \"/System/Volumes/Preboot/Cryptexes/OS/usr/lib\"))"
        );
        let deadline = Instant::now() + Duration::from_secs(2);
        // Reserve each child's stderr cap in the aggregate output budget.
        let mut remaining: usize = 1024 * 1024;
        let command = || {
            let mut command = Command::new("/usr/bin/sandbox-exec");
            command
                .env_clear()
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_OPTIONAL_LOCKS", "0")
                .env("GIT_TERMINAL_PROMPT", "0")
                .env("GIT_PAGER", "")
                .env("LC_ALL", "C")
                .current_dir(path)
                .args(["-p", &profile])
                .arg(git)
                .arg(format!("--git-dir={}", repository.join(".git").display()))
                .arg(format!("--work-tree={}", repository.display()))
                .args([
                    "-c",
                    "core.fsmonitor=false",
                    "-c",
                    "core.untrackedCache=false",
                    "-c",
                    "core.hooksPath=/dev/null",
                    "-c",
                    "submodule.recurse=false",
                ]);
            command
        };
        let mut execute = |args: &[&str]| {
            remaining = remaining.checked_sub(16_384).ok_or("git_output_limit")?;
            let mut child = command();
            child.args(args);
            let output = run(child, deadline, cancelled, remaining)?;
            remaining -= output.len();
            Ok::<_, &'static str>(output)
        };
        let output = execute(&[
            "status",
            "--porcelain=v2",
            "--branch",
            "-z",
            "--untracked-files=all",
            "--ignore-submodules=all",
        ])?;
        let mut snapshot = parse_status(&output)?;
        let totals = (|| {
            let baseline = if snapshot.unborn {
                let bytes = execute(&["hash-object", "-t", "tree", "--stdin"])?;
                let oid = std::str::from_utf8(&bytes)
                    .map_err(|_| "git_diff_invalid")?
                    .trim();
                if ![40, 64].contains(&oid.len()) || !oid.bytes().all(|b| b.is_ascii_hexdigit()) {
                    return Err("git_diff_invalid");
                }
                oid.to_owned()
            } else {
                "HEAD".to_owned()
            };
            let bytes = execute(&[
                "diff",
                "--numstat",
                "-z",
                "--no-renames",
                "--no-ext-diff",
                "--no-textconv",
                "--ignore-submodules=all",
                &baseline,
                "--",
            ])?;
            parse_numstat(&bytes)
        })();
        if let Ok((added, deleted)) = totals {
            snapshot.added = Some(added);
            snapshot.deleted = Some(deleted);
        }
        validate_scope(root, path, device, inode)?;
        Ok(snapshot)
    }
    collect(root, path, device, inode, git, cancelled).unwrap_or_else(GitSnapshot::unknown)
}

/// Child handles stay on the retained worker through kill and reap; never detached.
fn run(
    mut command: Command,
    deadline: Instant,
    cancelled: &AtomicBool,
    limit: usize,
) -> Result<Vec<u8>, &'static str> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|_| "git_spawn_failed")?;
    let Some(mut output) = child.stdout.take() else {
        settle_child(&mut child, true);
        return Err("git_pipe_failed");
    };
    let Some(mut errors) = child.stderr.take() else {
        settle_child(&mut child, true);
        return Err("git_pipe_failed");
    };
    for fd in [output.as_raw_fd(), errors.as_raw_fd()] {
        // SAFETY: owned live pipe descriptor, flag operations do not transfer ownership.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
            settle_child(&mut child, true);
            return Err("git_pipe_failed");
        }
    }
    let mut bytes = Vec::new();
    let mut error_bytes = 0usize;
    #[cfg(test)]
    let mut diagnostics = Vec::new();
    let result = 'running: loop {
        if cancelled.load(Ordering::Acquire) {
            break Err("git_cancelled");
        }
        if Instant::now() >= deadline {
            break Err("git_deadline");
        }
        let mut buffer = [0u8; 8192];
        match output.read(&mut buffer) {
            Ok(n) => {
                if bytes.len() + n > limit {
                    break Err("git_output_limit");
                }
                bytes.extend_from_slice(&buffer[..n]);
            }
            Err(e)
                if [io::ErrorKind::WouldBlock, io::ErrorKind::Interrupted].contains(&e.kind()) => {}
            Err(_) => break Err("git_read_failed"),
        }
        match errors.read(&mut buffer) {
            Ok(n) => {
                error_bytes += n;
                #[cfg(test)]
                diagnostics.extend_from_slice(&buffer[..n]);
                if error_bytes > 16_384 {
                    break Err("git_output_limit");
                }
            }
            Err(e)
                if [io::ErrorKind::WouldBlock, io::ErrorKind::Interrupted].contains(&e.kind()) => {}
            Err(_) => break Err("git_read_failed"),
        }
        match child.try_wait() {
            Ok(Some(status)) if !status.success() => {
                #[cfg(test)]
                eprintln!(
                    "git fixture child failed: {status}: {}",
                    String::from_utf8_lossy(&diagnostics)
                );
                break Err("git_observation_failed");
            }
            Ok(Some(_)) => {
                loop {
                    match output.read(&mut buffer) {
                        Ok(0) => break,
                        Ok(n) if bytes.len() + n <= limit => bytes.extend_from_slice(&buffer[..n]),
                        Ok(_) => break 'running Err("git_output_limit"),
                        Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                        Err(_) => break 'running Err("git_read_failed"),
                    }
                }
                break Ok(bytes);
            }
            Ok(None) => thread::sleep(Duration::from_millis(10)),
            Err(_) => break Err("git_wait_failed"),
        }
    };
    settle_child(&mut child, result.is_err());
    result
}

fn settle_child(child: &mut std::process::Child, terminate: bool) {
    if terminate {
        let _ = child.kill();
    }
    // An OS wait error is not reap evidence. Retain the exact child and worker slot
    // until wait supplies evidence; the reactor never joins an unfinished worker.
    while child.wait().is_err() {
        thread::sleep(Duration::from_millis(20));
    }
}

pub fn parse_status(output: &[u8]) -> Result<GitSnapshot, &'static str> {
    if !output.ends_with(&[0]) {
        return Err("git_status_invalid");
    }
    let mut value = GitSnapshot::unknown("unused");
    value.state = GitState::Clean;
    value.reason = None;
    value.files_changed = Some(0);
    let mut branch_seen = false;
    let mut skip_rename_path = false;
    for record in output.split(|b| *b == 0).filter(|v| !v.is_empty()) {
        if skip_rename_path {
            skip_rename_path = false;
            continue;
        }
        if let Some(branch) = record.strip_prefix(b"# branch.head ") {
            branch_seen = true;
            let branch = std::str::from_utf8(branch).map_err(|_| "git_branch_encoding")?;
            if branch == "(detached)" {
                value.detached = true;
            } else if branch.is_empty()
                || branch.len() > 1024
                || branch.chars().any(char::is_control)
            {
                return Err("git_status_invalid");
            } else {
                value.branch = Some(branch.to_owned());
            }
        } else if let Some(oid) = record.strip_prefix(b"# branch.oid ") {
            value.unborn = oid == b"(initial)";
        } else if record.starts_with(b"# ") {
            continue;
        } else if record.starts_with(b"1 ") || record.starts_with(b"? ") {
            value.files_changed = value.files_changed.and_then(|n| n.checked_add(1));
            value.state = GitState::Dirty;
        } else if record.starts_with(b"2 ") {
            value.files_changed = value.files_changed.and_then(|n| n.checked_add(1));
            value.state = GitState::Dirty;
            skip_rename_path = true;
        } else if record.starts_with(b"u ") {
            value.files_changed = value.files_changed.and_then(|n| n.checked_add(1));
            value.state = GitState::Dirty;
            value.conflicts = true;
        } else {
            return Err("git_status_invalid");
        }
    }
    if !branch_seen || skip_rename_path {
        return Err("git_status_invalid");
    }
    Ok(value)
}

fn parse_numstat(output: &[u8]) -> Result<(u64, u64), &'static str> {
    if !output.is_empty() && !output.ends_with(&[0]) {
        return Err("git_diff_invalid");
    }
    let mut totals = (0u64, 0u64);
    for record in output.split(|b| *b == 0).filter(|r| !r.is_empty()) {
        let mut fields = record.splitn(3, |b| *b == b'\t');
        let added = fields.next().ok_or("git_diff_invalid")?;
        let deleted = fields.next().ok_or("git_diff_invalid")?;
        if fields.next().is_none_or(|path| path.is_empty()) {
            return Err("git_diff_invalid");
        }
        if added == b"-" && deleted == b"-" {
            continue;
        }
        let number = |field: &[u8]| -> Result<u64, &'static str> {
            if field.is_empty() || !field.iter().all(u8::is_ascii_digit) {
                return Err("git_diff_invalid");
            }
            std::str::from_utf8(field)
                .map_err(|_| "git_diff_invalid")?
                .parse()
                .map_err(|_| "git_diff_invalid")
        };
        totals.0 = totals
            .0
            .checked_add(number(added)?)
            .ok_or("git_diff_invalid")?;
        totals.1 = totals
            .1
            .checked_add(number(deleted)?)
            .ok_or("git_diff_invalid")?;
    }
    Ok(totals)
}

#[repr(C)]
struct StreamContext {
    version: isize,
    info: *mut c_void,
    retain: *const c_void,
    release: *const c_void,
    description: *const c_void,
}
#[link(name = "CoreServices", kind = "framework")]
unsafe extern "C" {
    fn FSEventStreamCreate(
        allocator: *const c_void,
        callback: unsafe extern "C" fn(
            *const c_void,
            *mut c_void,
            usize,
            *mut c_void,
            *const u32,
            *const u64,
        ),
        context: *mut StreamContext,
        paths: *const c_void,
        since: u64,
        latency: f64,
        flags: u32,
    ) -> *mut c_void;
    fn FSEventStreamScheduleWithRunLoop(
        stream: *mut c_void,
        loop_: *mut c_void,
        mode: *const c_void,
    );
    fn FSEventStreamStart(stream: *mut c_void) -> u8;
    fn FSEventStreamStop(stream: *mut c_void);
    fn FSEventStreamInvalidate(stream: *mut c_void);
    fn FSEventStreamRelease(stream: *mut c_void);
}
#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFStringCreateWithCString(
        allocator: *const c_void,
        text: *const i8,
        encoding: u32,
    ) -> *const c_void;
    fn CFArrayCreate(
        allocator: *const c_void,
        values: *const *const c_void,
        count: isize,
        callbacks: *const c_void,
    ) -> *const c_void;
    fn CFRelease(value: *const c_void);
    fn CFRunLoopGetCurrent() -> *mut c_void;
    fn CFRunLoopRunInMode(mode: *const c_void, seconds: f64, return_after_source: u8) -> i32;
    static kCFRunLoopDefaultMode: *const c_void;
}

#[repr(C)]
struct RunLoopSourceContext {
    version: isize,
    info: *mut c_void,
    retain: Option<unsafe extern "C" fn(*const c_void) -> *const c_void>,
    release: Option<unsafe extern "C" fn(*const c_void)>,
    description: Option<unsafe extern "C" fn(*const c_void) -> *const c_void>,
    equal: Option<unsafe extern "C" fn(*const c_void, *const c_void) -> u8>,
    hash: Option<unsafe extern "C" fn(*const c_void) -> usize>,
    schedule: Option<unsafe extern "C" fn(*mut c_void, *const c_void, *const c_void)>,
    cancel: Option<unsafe extern "C" fn(*mut c_void, *const c_void, *const c_void)>,
    perform: Option<unsafe extern "C" fn(*mut c_void)>,
}
unsafe extern "C" {
    fn CFRetain(value: *const c_void) -> *const c_void;
    fn CFRunLoopSourceCreate(
        allocator: *const c_void,
        order: isize,
        context: *mut RunLoopSourceContext,
    ) -> *const c_void;
    fn CFRunLoopSourceSignal(source: *const c_void);
    fn CFRunLoopWakeUp(run_loop: *const c_void);
    fn CFRunLoopAddSource(run_loop: *const c_void, source: *const c_void, mode: *const c_void);
    fn CFRunLoopRemoveSource(run_loop: *const c_void, source: *const c_void, mode: *const c_void);
}
unsafe extern "C" fn wake_perform(_: *mut c_void) {}
struct WatchWakeInner {
    run_loop: *const c_void,
    source: *const c_void,
}
// SAFETY: retained CF objects are accessed cross-thread only by the thread-safe
// SourceSignal/WakeUp and retain/release APIs. Registration/removal stays on owner.
unsafe impl Send for WatchWakeInner {}
unsafe impl Sync for WatchWakeInner {}
impl Drop for WatchWakeInner {
    fn drop(&mut self) {
        // SAFETY: each object carries one owning reference until the final shared handle.
        unsafe {
            CFRelease(self.source);
            CFRelease(self.run_loop);
        }
    }
}
#[derive(Clone)]
pub struct GitWatchWake(Arc<WatchWakeInner>);
impl GitWatchWake {
    pub fn signal(&self) {
        // SAFETY: objects are retained, and no callback context crosses threads.
        unsafe {
            CFRunLoopSourceSignal(self.0.source);
            CFRunLoopWakeUp(self.0.run_loop);
        }
    }
    fn new() -> Result<Self, &'static str> {
        let mut context = RunLoopSourceContext {
            version: 0,
            info: std::ptr::null_mut(),
            retain: None,
            release: None,
            description: None,
            equal: None,
            hash: None,
            schedule: None,
            cancel: None,
            perform: Some(wake_perform),
        };
        // SAFETY: CF copies the complete version-zero context. Current runloop is retained.
        unsafe {
            let source = CFRunLoopSourceCreate(std::ptr::null(), 0, &mut context);
            if source.is_null() {
                return Err("watch_unavailable");
            }
            let run_loop = CFRetain(CFRunLoopGetCurrent());
            CFRunLoopAddSource(run_loop, source, kCFRunLoopDefaultMode);
            Ok(Self(Arc::new(WatchWakeInner { run_loop, source })))
        }
    }
}

/// Thread-affine recursive watcher. Create, poll and drop on the same retained worker.
pub struct GitWatch {
    stream: *mut c_void,
    paths: *const c_void,
    path: *const c_void,
    dirty: Box<AtomicBool>,
    wake: GitWatchWake,
}
unsafe extern "C" fn changed(
    _: *const c_void,
    info: *mut c_void,
    _: usize,
    _: *mut c_void,
    _: *const u32,
    _: *const u64,
) {
    // SAFETY: info points to GitWatch's boxed flag, retained until stream invalidation.
    if let Some(flag) = unsafe { (info as *const AtomicBool).as_ref() } {
        flag.store(true, Ordering::Release);
    }
}
impl GitWatch {
    pub fn new(root: &str) -> Result<Self, &'static str> {
        let cpath = CString::new(root).map_err(|_| "watch_path_invalid")?;
        let dirty = Box::new(AtomicBool::new(false));
        // SAFETY: owned CF objects and stable boxed callback context outlive the stream.
        unsafe {
            let path = CFStringCreateWithCString(std::ptr::null(), cpath.as_ptr(), 0x08000100);
            if path.is_null() {
                return Err("watch_unavailable");
            }
            let paths = CFArrayCreate(std::ptr::null(), &path, 1, std::ptr::null());
            if paths.is_null() {
                CFRelease(path);
                return Err("watch_unavailable");
            }
            let mut context = StreamContext {
                version: 0,
                info: (&*dirty as *const AtomicBool).cast_mut().cast(),
                retain: std::ptr::null(),
                release: std::ptr::null(),
                description: std::ptr::null(),
            };
            let stream = FSEventStreamCreate(
                std::ptr::null(),
                changed,
                &mut context,
                paths,
                u64::MAX,
                0.1,
                0x06,
            );
            if stream.is_null() {
                CFRelease(paths);
                CFRelease(path);
                return Err("watch_unavailable");
            }
            let wake = match GitWatchWake::new() {
                Ok(wake) => wake,
                Err(error) => {
                    FSEventStreamRelease(stream);
                    CFRelease(paths);
                    CFRelease(path);
                    return Err(error);
                }
            };
            FSEventStreamScheduleWithRunLoop(stream, CFRunLoopGetCurrent(), kCFRunLoopDefaultMode);
            let value = Self {
                stream,
                paths,
                path,
                dirty,
                wake,
            };
            if FSEventStreamStart(stream) == 0 {
                return Err("watch_unavailable");
            }
            Ok(value)
        }
    }
    pub fn wake_handle(&self) -> GitWatchWake {
        self.wake.clone()
    }
    pub fn poll(&self, seconds: f64) -> bool {
        // SAFETY: this stream remains scheduled on the current worker's runloop.
        unsafe {
            CFRunLoopRunInMode(kCFRunLoopDefaultMode, seconds.max(0.0), 1);
        }
        self.dirty.swap(false, Ordering::AcqRel)
    }
}
impl Drop for GitWatch {
    fn drop(&mut self) {
        // SAFETY: stream is stopped and invalidated before callback context or CF objects die.
        unsafe {
            CFRunLoopRemoveSource(
                self.wake.0.run_loop,
                self.wake.0.source,
                kCFRunLoopDefaultMode,
            );
            FSEventStreamStop(self.stream);
            FSEventStreamInvalidate(self.stream);
            FSEventStreamRelease(self.stream);
            CFRelease(self.paths);
            CFRelease(self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn status_states_preserve_unknown_counts_and_branch_modes() {
        let clean = parse_status(b"# branch.oid abc\0# branch.head main\0").unwrap();
        assert_eq!(clean.state, GitState::Clean);
        assert_eq!(clean.branch.as_deref(), Some("main"));
        assert_eq!((clean.added, clean.deleted), (None, None));
        let unborn =
            parse_status(b"# branch.oid (initial)\0# branch.head topic\0? new.txt\0").unwrap();
        assert!(unborn.unborn);
        assert_eq!(unborn.state, GitState::Dirty);
        let conflict = parse_status(b"# branch.head (detached)\0u conflict\0").unwrap();
        assert!(conflict.detached && conflict.conflicts);
        assert!(conflict.branch.is_none());
    }
    #[test]
    fn malformed_and_truncated_rename_status_is_not_clean() {
        assert!(parse_status(b"garbage\0").is_err());
        assert!(parse_status(b"# branch.head main\0unexpected\0").is_err());
        assert!(parse_status(b"# branch.head main\x00\x32 rename\0").is_err());
        assert!(parse_status(b"# branch.head main\x00\x32 rename\0old\0").is_ok());
    }
}

#[cfg(test)]
mod process_tests {
    use super::*;
    struct Fixture {
        root: PathBuf,
        git: PathBuf,
        token: AtomicBool,
    }
    impl Fixture {
        fn new() -> Self {
            let root = PathBuf::from(format!(
                "/private/tmp/asura-git-observer-{}-{}",
                std::process::id(),
                u128::from_be_bytes(crate::random_id())
            ));
            fs::create_dir(&root).unwrap();
            let token = AtomicBool::new(false);
            let git = git_executable(&token).unwrap();
            Self { root, git, token }
        }
        fn command(&self, args: &[&str]) {
            let mut command = Command::new(&self.git);
            command
                .env_clear()
                .current_dir(&self.root)
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .args(args);
            run(
                command,
                Instant::now() + Duration::from_secs(2),
                &self.token,
                1024 * 1024,
            )
            .unwrap();
        }
        fn observe(&self, cwd: &Path) -> GitSnapshot {
            let metadata = fs::metadata(&self.root).unwrap();
            collect_git(
                self.root.to_str().unwrap(),
                cwd.to_str().unwrap(),
                metadata.dev(),
                metadata.ino(),
                &self.git,
                &self.token,
            )
        }
        fn init(&self) {
            self.command(&["init", "-q", "-b", "main"]);
        }
        fn commit(&self) {
            self.command(&[
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "-qm",
                "fixture",
            ]);
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn text_totals_include_staged_and_unstaged_once_and_count_untracked_files() {
        let f = Fixture::new();
        f.init();
        fs::write(f.root.join("file"), "base\n").unwrap();
        f.command(&["add", "file"]);
        let unborn = f.observe(&f.root);
        assert_eq!(
            (unborn.files_changed, unborn.added, unborn.deleted),
            (Some(1), Some(1), Some(0)),
            "{unborn:?}"
        );
        f.commit();
        fs::write(f.root.join("file"), "base\nstaged\n").unwrap();
        f.command(&["add", "file"]);
        fs::write(f.root.join("file"), "base\nstaged\nworking\n").unwrap();
        fs::create_dir(f.root.join("new")).unwrap();
        fs::write(f.root.join("new/a"), "untracked\n").unwrap();
        fs::write(f.root.join("new/b"), "untracked\n").unwrap();
        let value = f.observe(&f.root);
        assert_eq!(
            (value.files_changed, value.added, value.deleted),
            (Some(3), Some(2), Some(0)),
            "{value:?}"
        );
    }

    #[test]
    fn rename_binary_and_unusual_names_have_explicit_text_totals() {
        let f = Fixture::new();
        f.init();
        fs::write(f.root.join("old"), "one\ntwo\n").unwrap();
        fs::write(f.root.join("binary"), b"a\0b").unwrap();
        f.command(&["add", "."]);
        f.commit();
        f.command(&["mv", "old", "new\tname\n"]);
        fs::write(f.root.join("binary"), b"c\0d").unwrap();
        let value = f.observe(&f.root);
        assert_eq!(
            (value.files_changed, value.added, value.deleted),
            (Some(2), Some(2), Some(2)),
            "{value:?}"
        );
        assert_eq!(parse_numstat(b"-\t-\tbinary\0"), Ok((0, 0)));
        assert!(parse_numstat(b"1\t2\tfile").is_err());
        assert!(parse_numstat(b"-\t2\tfile\0").is_err());
    }

    #[test]
    fn real_git_nonrepo_unborn_clean_dirty_detached_and_nested_repository() {
        let f = Fixture::new();
        assert_eq!(f.observe(&f.root).state, GitState::NonRepository);
        f.init();
        let unborn = f.observe(&f.root);
        assert_eq!(unborn.state, GitState::Clean, "{unborn:?}");
        assert!(unborn.unborn);
        fs::write(f.root.join("file"), "first\n").unwrap();
        f.command(&["add", "file"]);
        f.commit();
        assert_eq!(f.observe(&f.root).state, GitState::Clean);
        fs::write(f.root.join("file"), "second\n").unwrap();
        assert_eq!(f.observe(&f.root).state, GitState::Dirty);
        f.command(&["checkout", "--detach", "-q"]);
        assert!(f.observe(&f.root).detached);
        fs::create_dir(f.root.join("nested")).unwrap();
        f.command(&["init", "-q", "-b", "nested-branch", "nested"]);
        assert_eq!(
            f.observe(&f.root.join("nested")).branch.as_deref(),
            Some("nested-branch")
        );
    }

    #[test]
    fn constrained_git_does_not_write_index_or_execute_monitor_and_filters() {
        let f = Fixture::new();
        f.init();
        fs::write(f.root.join("file"), "first\n").unwrap();
        f.command(&["add", "file"]);
        f.commit();
        let index_before = fs::read(f.root.join(".git/index")).unwrap();
        let marker = f.root.join("helper-ran");
        let helper = format!("touch {}", marker.display());
        f.command(&["config", "core.fsmonitor", &helper]);
        let clean = f.observe(&f.root);
        assert_eq!(clean.state, GitState::Clean, "{clean:?}");
        assert!(!marker.exists());
        assert_eq!(fs::read(f.root.join(".git/index")).unwrap(), index_before);
        fs::write(f.root.join(".gitattributes"), "file filter=malicious\n").unwrap();
        f.command(&["config", "filter.malicious.clean", &helper]);
        fs::write(f.root.join("file"), "changed\n").unwrap();
        let result = f.observe(&f.root);
        assert!(matches!(result.state, GitState::Unknown | GitState::Dirty));
        assert!(!marker.exists());
        assert_eq!(fs::read(f.root.join(".git/index")).unwrap(), index_before);
    }

    #[test]
    fn external_metadata_and_replaced_scope_are_explicitly_unknown() {
        let f = Fixture::new();
        f.init();
        fs::write(
            f.root.join(".git/objects/info/alternates"),
            "/private/tmp/elsewhere\n",
        )
        .unwrap();
        assert_eq!(
            f.observe(&f.root).reason,
            Some("git_external_metadata_unsupported")
        );
        let metadata = fs::metadata(&f.root).unwrap();
        assert_eq!(
            collect_git(
                f.root.to_str().unwrap(),
                f.root.to_str().unwrap(),
                metadata.dev(),
                metadata.ino().wrapping_add(1),
                &f.git,
                &f.token
            )
            .reason,
            Some("project_identity_changed")
        );
    }

    #[test]
    fn recursive_watch_sees_nested_change_without_git_polling() {
        let f = Fixture::new();
        let nested = f.root.join("nested");
        fs::create_dir(&nested).unwrap();
        let watch = GitWatch::new(f.root.to_str().unwrap()).unwrap();
        watch.poll(0.1);
        fs::write(nested.join("changed"), "change\n").unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline {
            if watch.poll(0.1) {
                return;
            }
        }
        panic!("recursive filesystem signal did not arrive");
    }
    fn child_pid_gone(path: &Path) {
        let pid: i32 = fs::read_to_string(path).unwrap().trim().parse().unwrap();
        // SAFETY: signal zero only observes this fixture PID and never sends a signal.
        let observed = unsafe { libc::kill(pid, 0) };
        assert_eq!(observed, -1, "fixture child still exists");
        assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::ESRCH));
    }
    fn child_fixture(path: &Path, output: bool) -> Command {
        let mut command = Command::new("/bin/sh");
        let script = if output {
            "printf '%s' \"$$\" > \"$1\"; exec /usr/bin/yes bounded-output"
        } else {
            "printf '%s' \"$$\" > \"$1\"; exec /bin/sleep 30"
        };
        command
            .env_clear()
            .args(["-c", script, "fixture"])
            .arg(path);
        command
    }
    #[test]
    fn process_deadline_and_output_overflow_kill_and_reap_exact_child() {
        let f = Fixture::new();
        let deadline_pid = f.root.join("deadline-pid");
        assert_eq!(
            run(
                child_fixture(&deadline_pid, false),
                Instant::now() + Duration::from_millis(250),
                &f.token,
                1024
            ),
            Err("git_deadline")
        );
        child_pid_gone(&deadline_pid);
        let output_pid = f.root.join("output-pid");
        assert_eq!(
            run(
                child_fixture(&output_pid, true),
                Instant::now() + Duration::from_secs(2),
                &f.token,
                16
            ),
            Err("git_output_limit")
        );
        child_pid_gone(&output_pid);
    }
    #[test]
    fn process_cancellation_kills_and_reaps_exact_child() {
        let f = Fixture::new();
        let pid_path = f.root.join("cancel-pid");
        let token = std::sync::Arc::new(AtomicBool::new(false));
        let setter_token = token.clone();
        let setter_path = pid_path.clone();
        let setter = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(1);
            while !setter_path.exists() && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(5));
            }
            setter_token.store(true, Ordering::Release);
        });
        let outcome = run(
            child_fixture(&pid_path, false),
            Instant::now() + Duration::from_secs(2),
            &token,
            1024,
        );
        setter.join().unwrap();
        assert_eq!(outcome, Err("git_cancelled"));
        child_pid_gone(&pid_path);
    }
    #[test]
    fn external_config_is_not_loaded_and_symlinked_metadata_is_unavailable() {
        let f = Fixture::new();
        f.init();
        let external = Fixture::new();
        let config = external.root.join("secret-config");
        fs::write(&config, "[core]\n bare = true\n").unwrap();
        f.command(&["config", "include.path", config.to_str().unwrap()]);
        // Git cannot read an included config outside its sandbox. It must not report
        // source-dependent details from that file as a successful observation.
        assert_eq!(f.observe(&f.root).state, GitState::Unknown);
        f.command(&["config", "--unset", "include.path"]);
        fs::remove_file(f.root.join(".git/HEAD")).unwrap();
        std::os::unix::fs::symlink(&config, f.root.join(".git/HEAD")).unwrap();
        assert_eq!(f.observe(&f.root).state, GitState::Unknown);
    }
    #[test]
    fn watch_signal_is_retained_before_wait_and_wakes_active_wait() {
        for before in [true, false] {
            let signal = GitWatchWake::new().unwrap();
            let sent = Arc::new(AtomicBool::new(false));
            let flag = sent.clone();
            let wake = signal.clone();
            let sender = thread::spawn(move || {
                if !before {
                    thread::sleep(Duration::from_millis(30));
                }
                flag.store(true, Ordering::Release);
                wake.signal();
            });
            let sender = if before {
                sender.join().unwrap();
                None
            } else {
                Some(sender)
            };
            let now = Instant::now();
            // SAFETY: source registered on this thread, owner retained for the full wait.
            unsafe {
                CFRunLoopRunInMode(kCFRunLoopDefaultMode, 3.0, 1);
            }
            if let Some(sender) = sender {
                sender.join().unwrap();
            }
            assert!(sent.load(Ordering::Acquire));
            assert!(
                now.elapsed() < Duration::from_secs(1),
                "cancellation wake was lost"
            );
            // SAFETY: remove on the registering thread before releasing its objects.
            unsafe {
                CFRunLoopRemoveSource(signal.0.run_loop, signal.0.source, kCFRunLoopDefaultMode);
            }
        }
    }
}
