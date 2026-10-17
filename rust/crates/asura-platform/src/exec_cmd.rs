//! Bounded noninteractive command process owner. Filesystem work is worker-only.
mod capture;
mod cleanup;
mod guardian;
mod protocol;
mod spawn;
pub mod split;
use crate::runtime::{open_at, stat_fd};
use crate::secrets::SecretVariable;
use crate::secrets::rewrite::MAX_SECRET_VARIABLES;
use crate::{PollInterest, ProjectIdentity, RuntimeDirectory, StateBoundary};
use capture::Capture;
pub use capture::{DRAIN_BUDGET, OutputSink, READ_BUFFER, Stream, render_tail};
pub use cleanup::ExecCmdCleanup;
pub use guardian::run_guardian;
use protocol::Notice;
use std::ffi::CString;
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use zeroize::Zeroizing;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ExecCmdError {
    Invalid,
    Denied,
    Cancelled,
    Unavailable,
    Protocol,
    CleanupUnconfirmed,
    Io,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StopReason {
    Cancelled,
    OutputLimit,
    LeaseEnded,
}
/// Process facts of a settled job. Output is never kept here: the service's
/// capture worker owns the redacted stream and its tails (J1, G10).
#[derive(Clone, Debug)]
pub struct ExecCmdResult {
    pub failure: Option<ExecCmdError>,
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
    pub reason: Option<StopReason>,
    pub cleanup_confirmed: bool,
}
/// Rendered model-visible tails of the redacted streams (see [`render_tail`]).
#[derive(Clone, Debug, Default)]
pub struct Tails {
    pub stdout: String,
    pub stderr: String,
    pub truncated: bool,
}
impl ExecCmdResult {
    /// Kind 15 text: today's shape, with the redacted tails and the capture
    /// state as `capture=<state>` at the end of the header line.
    pub fn render_text(&self, tails: &Tails, capture: &str) -> String {
        format!(
            "failure={:?} exit_code={} signal={} reason={} truncated={} cleanup_confirmed={} capture={}\nstdout:\n{}\nstderr:\n{}",
            self.failure,
            self.exit_code
                .map_or_else(|| "none".into(), |n| n.to_string()),
            self.signal.map_or_else(|| "none".into(), |n| n.to_string()),
            match self.reason {
                None => "none",
                Some(StopReason::Cancelled) => "cancelled",
                Some(StopReason::OutputLimit) => "output_limit",
                Some(StopReason::LeaseEnded) => "lease_ended",
            },
            tails.truncated,
            self.cleanup_confirmed,
            capture,
            tails.stdout,
            tails.stderr
        )
    }
}
pub struct ExecCmdFailure {
    pub reason: ExecCmdError,
    pub cleanup: Option<ExecCmdCleanup>,
}
impl From<ExecCmdError> for ExecCmdFailure {
    fn from(reason: ExecCmdError) -> Self {
        Self {
            reason,
            cleanup: None,
        }
    }
}
/// Commands and secret variables share the 8192-byte limit (secret detection,
/// environment path).
pub const MAX_COMMAND_BYTES: usize = 8192;
/// One exec_cmd execution. `secrets` are the command's secret variables from the
/// rewrite: numbers 1–16, each distinct, values without NUL and at most
/// [`MAX_COMMAND_BYTES`] in total. They reach the guardian only through its
/// environment. No `Debug` is derived, and `SecretVariable`'s omits values.
///
/// The command has no time bound: it ends on completion or cancel, and setup
/// checks only cancellation.
pub struct ExecCmdRequest {
    pub job_id: [u8; 16],
    pub command: String,
    pub cwd: Option<String>,
    pub secrets: Vec<SecretVariable>,
}
pub struct PreparedExecCmd {
    request: ExecCmdRequest,
    original_project: String,
    original_cwd: String,
    boundary: StateBoundary,
    project: OwnedFd,
    cwd: OwnedFd,
    asura: OwnedFd,
    #[cfg(feature = "test-support")]
    omit_state_for_test: bool,
    #[cfg(feature = "test-support")]
    secret_environment_for_test: Option<Vec<Vec<u8>>>,
    cleanup: Option<ExecCmdCleanup>,
}
fn check(cancel: &AtomicBool) -> Result<(), ExecCmdError> {
    if cancel.load(Ordering::Acquire) {
        Err(ExecCmdError::Cancelled)
    } else {
        Ok(())
    }
}
fn validate(request: &ExecCmdRequest) -> Result<(), ExecCmdError> {
    let cwd = request.cwd.as_deref().unwrap_or(".");
    if request.job_id == [0; 16]
        || request.command.is_empty()
        || request.command.len() > MAX_COMMAND_BYTES
        || request.command.contains('\0')
        || cwd.is_empty()
        || cwd.len() > 1024
        || cwd.starts_with('/')
        || cwd.contains('\0')
        || cwd.split('/').any(|p| p == "..")
    {
        return Err(ExecCmdError::Invalid);
    }
    validate_secrets(&request.secrets)
}
/// The service-side check; the guardian repeats it on its environment.
fn validate_secrets(secrets: &[SecretVariable]) -> Result<(), ExecCmdError> {
    let mut seen = [false; MAX_SECRET_VARIABLES + 1];
    let mut total = 0usize;
    for secret in secrets {
        let number = usize::from(secret.number());
        if !(1..=MAX_SECRET_VARIABLES).contains(&number)
            || std::mem::replace(&mut seen[number], true)
            || secret.value().contains(&0)
        {
            return Err(ExecCmdError::Invalid);
        }
        total += secret.value().len();
        if total > MAX_COMMAND_BYTES {
            return Err(ExecCmdError::Invalid);
        }
    }
    Ok(())
}
/// `ASURA_SECRET_<n>=<value>` entries for the guardian's environment, in
/// zeroizing memory.
fn secret_environment(secrets: &[SecretVariable]) -> Vec<Zeroizing<Vec<u8>>> {
    secrets
        .iter()
        .map(|secret| {
            let name = secret.name();
            let mut entry =
                Zeroizing::new(Vec::with_capacity(name.len() + 1 + secret.value().len()));
            entry.extend_from_slice(name.as_bytes());
            entry.push(b'=');
            entry.extend_from_slice(secret.value());
            entry
        })
        .collect()
}
impl PreparedExecCmd {
    /// Blocking descriptor validation and scratch creation; retained setup worker only.
    pub fn prepare(
        runtime: RuntimeDirectory,
        project: &ProjectIdentity,
        request: ExecCmdRequest,
        cancel: &AtomicBool,
    ) -> Result<Self, ExecCmdFailure> {
        validate(&request)?;
        check(cancel)?;
        let original_project = project.process_location().to_owned();
        let boundary = project.boundary().clone();
        let project = project
            .process_directory()
            .map_err(|_| ExecCmdError::Denied)?;
        let mut cwd = project.try_clone().map_err(|_| ExecCmdError::Io)?;
        for part in request
            .cwd
            .as_deref()
            .unwrap_or(".")
            .split('/')
            .filter(|s| !s.is_empty() && *s != ".")
        {
            check(cancel)?;
            let name = CString::new(part).map_err(|_| ExecCmdError::Invalid)?;
            cwd = open_at(
                cwd.as_raw_fd(),
                &name,
                libc::O_RDONLY | libc::O_DIRECTORY,
                0,
            )
            .map_err(|_| ExecCmdError::Denied)?;
        }
        let original_cwd = spawn::path(cwd.as_raw_fd())?;
        let (asura, _) = runtime.authority_root().map_err(|_| ExecCmdError::Denied)?;
        let cleanup = ExecCmdCleanup::create(&runtime, request.job_id)?;
        let mut result = Self {
            request,
            original_project,
            original_cwd,
            boundary,
            project,
            cwd,
            asura,
            #[cfg(feature = "test-support")]
            omit_state_for_test: false,
            #[cfg(feature = "test-support")]
            secret_environment_for_test: None,
            cleanup: Some(cleanup),
        };
        if let Err(reason) = check(cancel) {
            return Err(ExecCmdFailure {
                reason,
                cleanup: result.cleanup.take(),
            });
        }
        Ok(result)
    }
    /// Replace FD 10, or omit it, to qualify the guardian's refusal.
    #[cfg(feature = "test-support")]
    pub fn replace_state_for_test(&mut self, fd: Option<OwnedFd>) {
        match fd {
            Some(fd) => self.asura = fd,
            None => self.omit_state_for_test = true,
        }
    }
    /// Replace the guardian's environment with raw entries that bypass the
    /// service-side check, to qualify the guardian's own validation (SDU-13).
    #[cfg(feature = "test-support")]
    pub fn replace_secret_environment_for_test(&mut self, entries: Vec<Vec<u8>>) {
        self.secret_environment_for_test = Some(entries);
    }
    /// After a successful spawn ownership always transfers to ExecCmdJob.
    /// The request's secret values are dropped (and zeroed) whether or not
    /// the spawn succeeds.
    /// `sink` receives the job's raw output on the reactor (J1).
    pub fn spawn(
        mut self,
        executable: &Path,
        cancel: &AtomicBool,
        sink: Box<dyn OutputSink>,
    ) -> Result<ExecCmdJob, ExecCmdFailure> {
        let spawned = self.spawn_inner(executable, cancel, sink);
        self.request.secrets.clear();
        match spawned {
            Ok(job) => Ok(job),
            Err(reason) => Err(ExecCmdFailure {
                reason,
                cleanup: self.cleanup.take(),
            }),
        }
    }
    fn spawn_inner(
        &mut self,
        executable: &Path,
        cancel: &AtomicBool,
        sink: Box<dyn OutputSink>,
    ) -> Result<ExecCmdJob, ExecCmdError> {
        check(cancel)?;
        let current = ProjectIdentity::open(&self.original_project, &self.boundary)
            .map_err(|_| ExecCmdError::Denied)?;
        let pinned = stat_fd(self.project.as_raw_fd()).map_err(|_| ExecCmdError::Io)?;
        if current.device != pinned.st_dev as u64 || current.inode != pinned.st_ino {
            return Err(ExecCmdError::Denied);
        }
        let current_cwd = ProjectIdentity::open(&self.original_cwd, &self.boundary)
            .map_err(|_| ExecCmdError::Denied)?;
        let pinned_cwd = stat_fd(self.cwd.as_raw_fd()).map_err(|_| ExecCmdError::Io)?;
        if current_cwd.device != pinned_cwd.st_dev as u64 || current_cwd.inode != pinned_cwd.st_ino
        {
            return Err(ExecCmdError::Denied);
        }
        for fd in [&self.project, &self.cwd] {
            if stat_fd(fd.as_raw_fd())
                .map_err(|_| ExecCmdError::Io)?
                .st_nlink
                == 0
            {
                return Err(ExecCmdError::Denied);
            }
        }
        let scratch = &self
            .cleanup
            .as_ref()
            .ok_or(ExecCmdError::Protocol)?
            .directory;
        let (lease_read, lease_write) = spawn::pipe(true)?;
        let (status_read, status_write) = spawn::pipe(true)?;
        let (out_read, out_write) = spawn::pipe(false)?;
        let (err_read, err_write) = spawn::pipe(false)?;
        let null =
            open_at(libc::AT_FDCWD, c"/dev/null", libc::O_RDWR, 0).map_err(|_| ExecCmdError::Io)?;
        let args = vec![
            executable.to_string_lossy().into_owned(),
            "--asura-exec-cmd-guardian".into(),
            hex(self.request.job_id),
            self.request.command.clone(),
        ];
        if args.iter().map(|value| value.len() + 1).sum::<usize>() > 8704 {
            return Err(ExecCmdError::Invalid);
        }
        let mappings: &[_] = &[
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
            (self.asura.as_raw_fd(), 10),
        ];
        #[cfg(feature = "test-support")]
        let mappings = &mappings[..mappings.len() - usize::from(self.omit_state_for_test)];
        let environment = secret_environment(&self.request.secrets);
        #[cfg(feature = "test-support")]
        let environment = match &self.secret_environment_for_test {
            Some(raw) => raw
                .iter()
                .map(|entry| Zeroizing::new(entry.clone()))
                .collect(),
            None => environment,
        };
        let env: Vec<&[u8]> = environment.iter().map(|entry| entry.as_slice()).collect();
        check(cancel)?;
        let pid = spawn::launch(executable, &args, &env, mappings, None, false)?;
        drop(env);
        drop(environment);
        // Everything after this point is infallible and transfers the live child.
        Ok(ExecCmdJob {
            pid,
            reaped: false,
            lease: Some(lease_write),
            status: status_read,
            status_bytes: Vec::with_capacity(128),
            status_eof: false,
            notices: 0,
            spawned: false,
            terminal: None,
            out: Capture::new(out_read, Stream::Stdout),
            err: Capture::new(err_read, Stream::Stderr),
            sink,
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
pub struct ExecCmdJob {
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
    sink: Box<dyn OutputSink>,
    stop: Option<StopReason>,
    cleanup: Option<ExecCmdCleanup>,
    job_id: [u8; 16],
    alternate: bool,
    fault: bool,
}
impl ExecCmdJob {
    #[cfg(feature = "test-support")]
    pub fn guardian_pid_for_test(&self) -> i32 {
        self.pid
    }

    /// Read interests. A stream's interest is omitted while the sink has less
    /// room than one read buffer (backpressure); the status pipe interest
    /// stays so exit notices still arrive. After a cancel trigger both pipes
    /// are read to EOF.
    pub fn interests(&self) -> Vec<PollInterest> {
        let mut result = Vec::with_capacity(3);
        let room = self.sink.room();
        let stopping = self.stop.is_some();
        for (fd, wanted) in [
            (self.status.as_raw_fd(), !self.status_eof),
            (self.out.fd(), self.out.wants_read(room, stopping)),
            (self.err.fd(), self.err.wants_read(room, stopping)),
        ] {
            if wanted {
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
            // The 100 ms poll tick (L5) ends nothing; it is the only timer.
            Some(Instant::now() + Duration::from_millis(100))
        }
    }
    pub fn stop(&mut self, reason: StopReason) {
        if self.stop.is_none() {
            self.stop = Some(reason)
        }
        self.lease.take();
    }
    pub fn poll(&mut self, _now: Instant) -> Result<(), ExecCmdError> {
        let result = self.poll_inner();
        if result.is_err() {
            self.fault = true;
            self.stop(StopReason::Cancelled)
        }
        result
    }
    fn poll_inner(&mut self) -> Result<(), ExecCmdError> {
        let (first, second) = if self.alternate {
            (&mut self.err, &mut self.out)
        } else {
            (&mut self.out, &mut self.err)
        };
        self.alternate = !self.alternate;
        let stopping = self.stop.is_some();
        self.sink.flush();
        first.drain(DRAIN_BUDGET, self.sink.as_mut(), stopping)?;
        second.drain(DRAIN_BUDGET, self.sink.as_mut(), stopping)?;
        self.sink.flush();
        // No combined output stop (J1-Q2): the shared quota and pause apply.
        if !self.status_eof {
            let mut buf = [0u8; 128];
            // SAFETY: owned nonblocking descriptor and valid output buffer.
            let n =
                unsafe { libc::read(self.status.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
            if n == 0 {
                self.status_eof = true;
            } else if n > 0 {
                if self.status_bytes.len() + n as usize > 128 {
                    return Err(ExecCmdError::Protocol);
                }
                self.status_bytes.extend_from_slice(&buf[..n as usize]);
                while self.status_bytes.len() >= 32 {
                    let notice = Notice::decode(&self.status_bytes[..32], self.job_id)?;
                    self.status_bytes.drain(..32);
                    self.notices += 1;
                    if self.notices > 3 || self.terminal.is_some() {
                        return Err(ExecCmdError::Protocol);
                    }
                    match notice.kind {
                        1 if !self.spawned && self.notices == 1 => self.spawned = true,
                        2 if self.spawned => self.terminal = Some(notice),
                        3 if !self.spawned && self.notices == 1 => self.terminal = Some(notice),
                        4 if self.spawned && self.notices == 2 => {}
                        _ => return Err(ExecCmdError::Protocol),
                    }
                }
            } else if !spawn::would_block() {
                return Err(ExecCmdError::Io);
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
                return Err(ExecCmdError::Io);
            }
        }
        if self.status_eof && (!self.status_bytes.is_empty() || self.terminal.is_none()) {
            return Err(ExecCmdError::Protocol);
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
    pub fn result(&self) -> Option<ExecCmdResult> {
        if !self.settled() {
            return None;
        }
        let n = self.terminal?;
        Some(ExecCmdResult {
            failure: if n.kind == 3 {
                Some(match n.reason {
                    7 => ExecCmdError::Denied,
                    8 => ExecCmdError::Protocol,
                    9 => ExecCmdError::CleanupUnconfirmed,
                    _ => ExecCmdError::Unavailable,
                })
            } else {
                None
            },
            exit_code: (n.exit_kind == 1).then_some(n.value as i32),
            signal: (n.exit_kind == 2).then_some(n.value as i32),
            // Reason 1 (the removed deadline) is unassigned.
            reason: self.stop.or(match n.reason {
                2 => Some(StopReason::Cancelled),
                3 => Some(StopReason::OutputLimit),
                4 => Some(StopReason::LeaseEnded),
                _ => None,
            }),
            cleanup_confirmed: true,
        })
    }
    pub fn take_cleanup(&mut self) -> Option<ExecCmdCleanup> {
        if self.settled() {
            self.cleanup.take()
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests;
