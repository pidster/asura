//! Bounded noninteractive command process owner. Filesystem work is worker-only.
mod capture;
mod cleanup;
mod guardian;
mod protocol;
mod spawn;
use crate::runtime::{open_at, stat_fd};
use crate::{PollInterest, ProjectIdentity, RuntimeDirectory};
use capture::Capture;
pub use cleanup::ShellCleanup;
pub use guardian::run_guardian;
use protocol::Notice;
use std::ffi::CString;
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ShellError {
    Invalid,
    Denied,
    Deadline,
    Cancelled,
    Unavailable,
    Protocol,
    CleanupUnconfirmed,
    Io,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StopReason {
    Cancelled,
    Deadline,
    OutputLimit,
    LeaseEnded,
}
#[derive(Clone, Debug)]
pub struct ShellResult {
    pub failure: Option<ShellError>,
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
    pub reason: Option<StopReason>,
    pub stdout: String,
    pub stderr: String,
    pub truncated: bool,
    pub cleanup_confirmed: bool,
}
impl ShellResult {
    pub fn render_text(&self) -> String {
        format!(
            "failure={:?} exit_code={} signal={} reason={} truncated={} cleanup_confirmed={}\nstdout:\n{}\nstderr:\n{}",
            self.failure,
            self.exit_code
                .map_or_else(|| "none".into(), |n| n.to_string()),
            self.signal.map_or_else(|| "none".into(), |n| n.to_string()),
            match self.reason {
                None => "none",
                Some(StopReason::Cancelled) => "cancelled",
                Some(StopReason::Deadline) => "timeout",
                Some(StopReason::OutputLimit) => "output_limit",
                Some(StopReason::LeaseEnded) => "lease_ended",
            },
            self.truncated,
            self.cleanup_confirmed,
            self.stdout,
            self.stderr
        )
    }
}
pub struct ShellFailure {
    pub reason: ShellError,
    pub cleanup: Option<ShellCleanup>,
}
impl From<ShellError> for ShellFailure {
    fn from(reason: ShellError) -> Self {
        Self {
            reason,
            cleanup: None,
        }
    }
}
pub struct ShellRequest {
    pub job_id: [u8; 16],
    pub command: String,
    pub cwd: Option<String>,
    pub deadline: Instant,
}
pub struct PreparedShell {
    request: ShellRequest,
    original_project: String,
    original_cwd: String,
    project: OwnedFd,
    cwd: OwnedFd,
    cleanup: Option<ShellCleanup>,
    setup_deadline: Instant,
}
fn check(deadline: Instant, cancel: &AtomicBool) -> Result<(), ShellError> {
    if cancel.load(Ordering::Acquire) {
        Err(ShellError::Cancelled)
    } else if Instant::now() >= deadline {
        Err(ShellError::Deadline)
    } else {
        Ok(())
    }
}
fn validate(request: &ShellRequest) -> Result<(), ShellError> {
    let cwd = request.cwd.as_deref().unwrap_or(".");
    if request.job_id == [0; 16]
        || request.command.is_empty()
        || request.command.len() > 8192
        || request.command.contains('\0')
        || cwd.is_empty()
        || cwd.len() > 1024
        || cwd.starts_with('/')
        || cwd.contains('\0')
        || cwd.split('/').any(|p| p == "..")
    {
        return Err(ShellError::Invalid);
    }
    Ok(())
}
impl PreparedShell {
    /// Blocking descriptor validation and scratch creation; retained setup worker only.
    pub fn prepare(
        runtime: RuntimeDirectory,
        project: &ProjectIdentity,
        request: ShellRequest,
        cancel: &AtomicBool,
    ) -> Result<Self, ShellFailure> {
        validate(&request)?;
        let setup_deadline = request
            .deadline
            .min(Instant::now() + Duration::from_secs(2));
        check(setup_deadline, cancel)?;
        let original_project = project.process_location().to_owned();
        let project = project
            .process_directory()
            .map_err(|_| ShellError::Denied)?;
        let mut cwd = project.try_clone().map_err(|_| ShellError::Io)?;
        for part in request
            .cwd
            .as_deref()
            .unwrap_or(".")
            .split('/')
            .filter(|s| !s.is_empty() && *s != ".")
        {
            check(setup_deadline, cancel)?;
            let name = CString::new(part).map_err(|_| ShellError::Invalid)?;
            cwd = open_at(
                cwd.as_raw_fd(),
                &name,
                libc::O_RDONLY | libc::O_DIRECTORY,
                0,
            )
            .map_err(|_| ShellError::Denied)?;
        }
        let original_cwd = spawn::path(cwd.as_raw_fd())?;
        let cleanup = ShellCleanup::create(&runtime, request.job_id)?;
        let mut result = Self {
            request,
            original_project,
            original_cwd,
            project,
            cwd,
            cleanup: Some(cleanup),
            setup_deadline,
        };
        if let Err(reason) = check(setup_deadline, cancel) {
            return Err(ShellFailure {
                reason,
                cleanup: result.cleanup.take(),
            });
        }
        Ok(result)
    }
    /// After a successful spawn ownership always transfers to ShellJob.
    pub fn spawn(
        mut self,
        executable: &Path,
        cancel: &AtomicBool,
    ) -> Result<ShellJob, ShellFailure> {
        match self.spawn_inner(executable, cancel) {
            Ok(job) => Ok(job),
            Err(reason) => Err(ShellFailure {
                reason,
                cleanup: self.cleanup.take(),
            }),
        }
    }
    fn spawn_inner(
        &mut self,
        executable: &Path,
        cancel: &AtomicBool,
    ) -> Result<ShellJob, ShellError> {
        check(self.setup_deadline, cancel)?;
        let current =
            ProjectIdentity::open(&self.original_project).map_err(|_| ShellError::Denied)?;
        let pinned = stat_fd(self.project.as_raw_fd()).map_err(|_| ShellError::Io)?;
        if current.device != pinned.st_dev as u64 || current.inode != pinned.st_ino {
            return Err(ShellError::Denied);
        }
        let current_cwd =
            ProjectIdentity::open(&self.original_cwd).map_err(|_| ShellError::Denied)?;
        let pinned_cwd = stat_fd(self.cwd.as_raw_fd()).map_err(|_| ShellError::Io)?;
        if current_cwd.device != pinned_cwd.st_dev as u64 || current_cwd.inode != pinned_cwd.st_ino
        {
            return Err(ShellError::Denied);
        }
        for fd in [&self.project, &self.cwd] {
            if stat_fd(fd.as_raw_fd())
                .map_err(|_| ShellError::Io)?
                .st_nlink
                == 0
            {
                return Err(ShellError::Denied);
            }
        }
        let scratch = &self.cleanup.as_ref().ok_or(ShellError::Protocol)?.directory;
        let (lease_read, lease_write) = spawn::pipe(true)?;
        let (status_read, status_write) = spawn::pipe(true)?;
        let (out_read, out_write) = spawn::pipe(false)?;
        let (err_read, err_write) = spawn::pipe(false)?;
        let null =
            open_at(libc::AT_FDCWD, c"/dev/null", libc::O_RDWR, 0).map_err(|_| ShellError::Io)?;
        let ns = spawn::monotonic_ns()?
            .checked_add(
                self.request
                    .deadline
                    .saturating_duration_since(Instant::now())
                    .as_nanos()
                    .try_into()
                    .map_err(|_| ShellError::Invalid)?,
            )
            .ok_or(ShellError::Invalid)?;
        let args = vec![
            executable.to_string_lossy().into_owned(),
            "--asura-shell-guardian".into(),
            hex(self.request.job_id),
            ns.to_string(),
            self.request.command.clone(),
        ];
        if args.iter().map(|value| value.len() + 1).sum::<usize>() > 8704 {
            return Err(ShellError::Invalid);
        }
        let mappings = [
            (null.as_raw_fd(), 0),
            (null.as_raw_fd(), 1),
            (null.as_raw_fd(), 2),
            (lease_read.as_raw_fd(), 3),
            (status_write.as_raw_fd(), 4),
            (out_write.as_raw_fd(), 5),
            (err_write.as_raw_fd(), 6),
            (self.project.as_raw_fd(), 7),
            (self.cwd.as_raw_fd(), 8),
            (scratch.as_raw_fd(), 9),
        ];
        check(self.setup_deadline, cancel)?;
        let pid = spawn::launch(executable, &args, &[], &mappings, None, false)?;
        // Everything after this point is infallible and transfers the live child.
        Ok(ShellJob {
            pid,
            reaped: false,
            lease: Some(lease_write),
            status: status_read,
            status_bytes: Vec::with_capacity(128),
            status_eof: false,
            notices: 0,
            spawned: false,
            terminal: None,
            out: Capture::new(out_read),
            err: Capture::new(err_read),
            deadline: self.request.deadline,
            stop: None,
            cleanup: self.cleanup.take(),
            job_id: self.request.job_id,
            alternate: false,
            fault: false,
        })
    }
}
fn hex(id: [u8; 16]) -> String {
    id.iter().map(|b| format!("{b:02x}")).collect()
}
pub struct ShellJob {
    pid: libc::pid_t,
    reaped: bool,
    lease: Option<OwnedFd>,
    status: OwnedFd,
    status_bytes: Vec<u8>,
    status_eof: bool,
    notices: u8,
    spawned: bool,
    terminal: Option<Notice>,
    out: Capture,
    err: Capture,
    deadline: Instant,
    stop: Option<StopReason>,
    cleanup: Option<ShellCleanup>,
    job_id: [u8; 16],
    alternate: bool,
    fault: bool,
}
impl ShellJob {
    #[cfg(feature = "test-support")]
    pub fn guardian_pid_for_test(&self) -> i32 {
        self.pid
    }

    pub fn interests(&self) -> Vec<PollInterest> {
        let mut result = Vec::with_capacity(3);
        for (fd, eof) in [
            (self.status.as_raw_fd(), self.status_eof),
            (self.out.fd(), self.out.eof),
            (self.err.fd(), self.err.eof),
        ] {
            if !eof {
                result.push(PollInterest {
                    fd,
                    read: true,
                    write: false,
                })
            }
        }
        result
    }
    pub fn next_deadline(&self) -> Option<Instant> {
        if self.settled() {
            None
        } else {
            Some(if self.lease.is_some() {
                self.deadline
                    .min(Instant::now() + Duration::from_millis(100))
            } else {
                Instant::now() + Duration::from_millis(100)
            })
        }
    }
    pub fn stop(&mut self, reason: StopReason) {
        if self.stop.is_none() {
            self.stop = Some(reason)
        }
        self.lease.take();
    }
    pub fn poll(&mut self, now: Instant) -> Result<(), ShellError> {
        if now >= self.deadline {
            self.stop(StopReason::Deadline)
        }
        let result = self.poll_inner();
        if result.is_err() {
            self.fault = true;
            self.stop(StopReason::Cancelled)
        }
        result
    }
    fn poll_inner(&mut self) -> Result<(), ShellError> {
        let (first, second) = if self.alternate {
            (&mut self.err, &mut self.out)
        } else {
            (&mut self.out, &mut self.err)
        };
        self.alternate = !self.alternate;
        first.drain(8192)?;
        second.drain(8192)?;
        if self.out.total.saturating_add(self.err.total) > 64 * 1024 * 1024 {
            self.stop(StopReason::OutputLimit)
        }
        if !self.status_eof {
            let mut buf = [0u8; 128];
            // SAFETY: owned nonblocking descriptor and valid output buffer.
            let n =
                unsafe { libc::read(self.status.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
            if n == 0 {
                self.status_eof = true;
            } else if n > 0 {
                if self.status_bytes.len() + n as usize > 128 {
                    return Err(ShellError::Protocol);
                }
                self.status_bytes.extend_from_slice(&buf[..n as usize]);
                while self.status_bytes.len() >= 32 {
                    let notice = Notice::decode(&self.status_bytes[..32], self.job_id)?;
                    self.status_bytes.drain(..32);
                    self.notices += 1;
                    if self.notices > 3 || self.terminal.is_some() {
                        return Err(ShellError::Protocol);
                    }
                    match notice.kind {
                        1 if !self.spawned && self.notices == 1 => self.spawned = true,
                        2 if self.spawned => self.terminal = Some(notice),
                        3 if !self.spawned && self.notices == 1 => self.terminal = Some(notice),
                        4 if self.spawned && self.notices == 2 => {}
                        _ => return Err(ShellError::Protocol),
                    }
                }
            } else if !spawn::would_block() {
                return Err(ShellError::Io);
            }
        }
        if !self.reaped {
            let mut status = 0;
            // SAFETY: pid is our unreaped direct child; wait is nonblocking.
            let n = unsafe { libc::waitpid(self.pid, &mut status, libc::WNOHANG) };
            if n == self.pid {
                self.reaped = true;
                if !libc::WIFEXITED(status) || libc::WEXITSTATUS(status) != 0 {
                    self.fault = true;
                }
            } else if n < 0 && !spawn::interrupted() {
                return Err(ShellError::Io);
            }
        }
        if self.status_eof && (!self.status_bytes.is_empty() || self.terminal.is_none()) {
            return Err(ShellError::Protocol);
        }
        Ok(())
    }
    pub fn settled(&self) -> bool {
        self.reaped
            && self.status_eof
            && self.out.eof
            && self.err.eof
            && self.terminal.is_some_and(|n| n.cleanup)
            && !self.fault
    }
    pub fn result(&self) -> Option<ShellResult> {
        if !self.settled() {
            return None;
        }
        let n = self.terminal?;
        let (stdout, a) = self.out.text();
        let (stderr, b) = self.err.text();
        Some(ShellResult {
            failure: if n.kind == 3 {
                Some(match n.reason {
                    7 => ShellError::Denied,
                    8 => ShellError::Protocol,
                    9 => ShellError::CleanupUnconfirmed,
                    _ => ShellError::Unavailable,
                })
            } else {
                None
            },
            exit_code: (n.exit_kind == 1).then_some(n.value as i32),
            signal: (n.exit_kind == 2).then_some(n.value as i32),
            reason: self.stop.or(match n.reason {
                1 => Some(StopReason::Deadline),
                2 => Some(StopReason::Cancelled),
                3 => Some(StopReason::OutputLimit),
                4 => Some(StopReason::LeaseEnded),
                _ => None,
            }),
            stdout,
            stderr,
            truncated: a || b,
            cleanup_confirmed: true,
        })
    }
    pub fn take_cleanup(&mut self) -> Option<ShellCleanup> {
        if self.settled() {
            self.cleanup.take()
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests;
