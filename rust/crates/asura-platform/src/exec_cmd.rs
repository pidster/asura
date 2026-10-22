//! Bounded noninteractive command process owner. Filesystem work is worker-only.
mod capture;
mod cleanup;
mod escapee;
mod guardian;
mod protocol;
mod spawn;
pub mod split;
use crate::runtime::{open_at, stat_fd};
use crate::secrets::rewrite::{MAX_SECRET_VARIABLES, SECRET_COMMAND_NAME, WHOLE_COMMAND};
use crate::secrets::{SecretCommand, SecretVariable};
use crate::{PollInterest, ProjectIdentity, RuntimeDirectory, StateBoundary};
use capture::Capture;
pub use capture::{DRAIN_BUDGET, OutputSink, READ_BUFFER, Stream, render_tail};
pub use cleanup::ExecCmdCleanup;
#[cfg(feature = "test-support")]
pub use escapee::scan_for_test as escapee_scan_for_test;
pub use guardian::run_guardian;
#[cfg(feature = "test-support")]
pub use guardian::run_guardian_for_test;
pub use protocol::Profile;
use protocol::{
    KIND_ESCAPEE_REPORT, KIND_LEADER_EXITED, LEASE_KILL, LEASE_TERM, Notice, REASON_CANCEL,
    REASON_CLEANUP_FAILED, REASON_ESCAPEE_TERMINATED, REASON_ESCAPEE_UNTERMINATED, REASON_KILL,
    REASON_LEASE_ENDED,
};
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
/// Why a settled job stopped. Reason 3 `output_limit` is unassigned since
/// J1; nothing produces it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StopReason {
    /// Lease byte `T` (kind 2 reason 2), or a service stop request.
    Cancelled,
    /// Lease byte `K` (kind 2 reason 11).
    Killed,
    /// Lease EOF or a lease fault (kind 2 reason 4).
    LeaseEnded,
}
/// A service stop request (tool-job-controller.md, guardian contract
/// additions). Each writes one lease byte with a nonblocking `write`; a
/// `WouldBlock` or an error closes the lease instead, which the guardian
/// handles as EOF, so a failed write never leaves the job running.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum StopKind {
    /// `T`: TERM now, KILL after the profile's grace. Ignored after any
    /// earlier stop request.
    Term,
    /// `K`: KILL now. Escalates an earlier `T`; ignored after an earlier `K`.
    Kill,
}
impl StopKind {
    fn byte(self) -> u8 {
        match self {
            Self::Term => LEASE_TERM,
            Self::Kill => LEASE_KILL,
        }
    }
}
/// The leader's exit facts from notice kind 5; the settled notice repeats
/// them.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Exit {
    /// Exit code 0–255 (`CLD_EXITED`).
    Exited(u8),
    /// Signal 1–127 (`CLD_KILLED` or `CLD_DUMPED`).
    Signalled(u8),
}
impl Exit {
    fn from_notice(notice: &Notice) -> Option<Self> {
        let value = u8::try_from(notice.value).ok()?;
        match notice.exit_kind {
            1 => Some(Self::Exited(value)),
            2 => Some(Self::Signalled(value)),
            _ => None,
        }
    }
}
/// Why the guardian reported `cleanup_unconfirmed` (notice kind 4).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UnconfirmedReason {
    /// Reason 9: cleanup did not settle by the 2.5 s report point.
    CleanupFailed,
    /// Reason 13: a descendant could not be verified gone (EX5).
    EscapeeUnterminated,
}
/// Process facts of a settled job. Output is never kept here: the service's
/// capture worker owns the redacted stream and its tails (J1, G10).
///
/// `cleanup_confirmed` is true only when the guardian's settlement gate
/// passed with no earlier `cleanup_unconfirmed` notice; a late settlement
/// releases resources but never asserts that cleanup was clean.
/// `escapees` counts the escapees the guardian terminated (notice kind 6).
/// `setup_reason` is the guardian's kind 3 reason (5–9), or 0 when the
/// terminal notice is not kind 3; `failure` folds reasons 5 and 6 into
/// `Unavailable`, so a job maps reason 6 to `sandbox_unavailable` from here
/// (tool-job-controller.md, J2-A2).
#[derive(Clone, Debug)]
pub struct ExecCmdResult {
    pub failure: Option<ExecCmdError>,
    pub setup_reason: u8,
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
    pub reason: Option<StopReason>,
    pub cleanup_confirmed: bool,
    pub escapees: u32,
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
    /// state as `capture=<state>` at the end of the header line, followed by
    /// ` escapees=<n>` when the guardian terminated escapees (EX5).
    pub fn render_text(&self, tails: &Tails, capture: &str) -> String {
        format!(
            "failure={:?} exit_code={} signal={} reason={} truncated={} cleanup_confirmed={} capture={}{}\nstdout:\n{}\nstderr:\n{}",
            self.failure,
            self.exit_code
                .map_or_else(|| "none".into(), |n| n.to_string()),
            self.signal.map_or_else(|| "none".into(), |n| n.to_string()),
            match self.reason {
                None => "none",
                Some(StopReason::Cancelled) => "cancelled",
                Some(StopReason::Killed) => "killed",
                Some(StopReason::LeaseEnded) => "lease_ended",
            },
            tails.truncated,
            self.cleanup_confirmed,
            capture,
            if self.escapees > 0 {
                format!(" escapees={}", self.escapees)
            } else {
                String::new()
            },
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
/// [`MAX_COMMAND_BYTES`] in total. `secret_command` is the whole-command
/// variable (SO5, SL7): 1–8192 bytes without NUL, only with no numbered
/// variable and the command [`WHOLE_COMMAND`]. Both reach the guardian only
/// through its environment. No `Debug` is derived, and the secret types'
/// omit values.
///
/// The command has no time bound: it ends on completion or cancel, and setup
/// checks only cancellation. `profile` selects the guardian's sandbox and
/// grace through its argv token.
pub struct ExecCmdRequest {
    pub job_id: [u8; 16],
    pub profile: Profile,
    pub command: String,
    pub cwd: Option<String>,
    pub secrets: Vec<SecretVariable>,
    pub secret_command: Option<SecretCommand>,
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
    #[cfg(feature = "test-support")]
    guardian_fault_for_test: Option<String>,
    #[cfg(feature = "test-support")]
    profile_token_for_test: Option<String>,
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
    validate_secrets(&request.secrets)?;
    match &request.secret_command {
        None => Ok(()),
        Some(command)
            if request.secrets.is_empty()
                && request.command == WHOLE_COMMAND
                && !command.value().is_empty()
                && command.value().len() <= MAX_COMMAND_BYTES
                && !command.value().contains(&0) =>
        {
            Ok(())
        }
        Some(_) => Err(ExecCmdError::Invalid),
    }
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
/// `ASURA_SECRET_<n>=<value>` entries, or the one `ASURA_SECRET_COMMAND`
/// entry, for the guardian's environment, in zeroizing memory.
fn secret_environment(
    secrets: &[SecretVariable],
    command: Option<&SecretCommand>,
) -> Vec<Zeroizing<Vec<u8>>> {
    let entry = |name: &str, value: &[u8]| {
        let mut entry = Zeroizing::new(Vec::with_capacity(name.len() + 1 + value.len()));
        entry.extend_from_slice(name.as_bytes());
        entry.push(b'=');
        entry.extend_from_slice(value);
        entry
    };
    secrets
        .iter()
        .map(|secret| entry(&secret.name(), secret.value()))
        .chain(command.map(|command| entry(SECRET_COMMAND_NAME, command.value())))
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
            #[cfg(feature = "test-support")]
            guardian_fault_for_test: None,
            #[cfg(feature = "test-support")]
            profile_token_for_test: None,
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
    /// Pass one escapee-control fault to the guardian (`group-only`,
    /// `undyable` or `unverifiable:<n>`); the test executable must dispatch
    /// `MARKER JOBHEX PROFILE COMMAND FAULT` to [`run_guardian_for_test`].
    #[cfg(feature = "test-support")]
    pub fn guardian_fault_for_test(&mut self, fault: &str) {
        self.guardian_fault_for_test = Some(fault.into());
    }
    /// Replace the argv profile token, to qualify the guardian's refusal
    /// of an unknown token.
    #[cfg(feature = "test-support")]
    pub fn profile_token_for_test(&mut self, token: &str) {
        self.profile_token_for_test = Some(token.into());
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
        self.request.secret_command = None;
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
        spawn::no_sigpipe(lease_write.as_raw_fd())?;
        let (status_read, status_write) = spawn::pipe(true)?;
        let (out_read, out_write) = spawn::pipe(false)?;
        let (err_read, err_write) = spawn::pipe(false)?;
        let null =
            open_at(libc::AT_FDCWD, c"/dev/null", libc::O_RDWR, 0).map_err(|_| ExecCmdError::Io)?;
        #[allow(unused_mut)]
        let mut args = vec![
            executable.to_string_lossy().into_owned(),
            "--asura-exec-cmd-guardian".into(),
            hex(self.request.job_id),
            self.request.profile.token().into(),
            self.request.command.clone(),
        ];
        #[cfg(feature = "test-support")]
        if let Some(token) = &self.profile_token_for_test {
            args[3] = token.clone();
        }
        #[cfg(feature = "test-support")]
        args.extend(self.guardian_fault_for_test.clone());
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
        let environment =
            secret_environment(&self.request.secrets, self.request.secret_command.as_ref());
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
            status_bytes: Vec::with_capacity(STATUS_BUFFER),
            status_eof: false,
            status_received: false,
            notices: 0,
            spawned: false,
            last_kind: 0,
            leader: None,
            unconfirmed: None,
            escapees: 0,
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
/// Status receive bound: five 32-byte notices fit (escapee-control.md).
const STATUS_BUFFER: usize = 192;
/// Most notices one guardian may send: spawned, leader exit, cleanup
/// unconfirmed, escapee report and settled, 160 bytes in all.
const MAX_NOTICES: u8 = 5;
pub struct ExecCmdJob {
    pid: libc::pid_t,
    reaped: bool,
    lease: Option<OwnedFd>,
    status: OwnedFd,
    status_bytes: Vec<u8>,
    status_eof: bool,
    /// Whether any status byte arrived; a notice that failed to decode
    /// counts, so the pre-spawn token release never follows one.
    status_received: bool,
    notices: u8,
    spawned: bool,
    /// Kind of the previous notice; 0 before the first.
    last_kind: u8,
    /// Kind 5 facts: exit kind and value.
    leader: Option<(u8, u32)>,
    unconfirmed: Option<UnconfirmedReason>,
    escapees: u32,
    terminal: Option<Notice>,
    out: Capture,
    err: Capture,
    sink: Box<dyn OutputSink>,
    /// The strongest stop request so far.
    stop: Option<StopKind>,
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
        let stopping = self.stopping();
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
    /// A stop request: at most one `T` and one `K` reach the lease, each
    /// with one nonblocking 1-byte write into a pipe that holds at most 2
    /// bytes. Idempotent; never blocks. After a cancel trigger both output
    /// pipes are read to EOF.
    pub fn stop(&mut self, kind: StopKind) {
        let send = match (self.stop, kind) {
            (None, _) | (Some(StopKind::Term), StopKind::Kill) => true,
            (Some(_), _) => false,
        };
        if !send {
            return;
        }
        self.stop = Some(kind);
        if let Some(lease) = &self.lease {
            let byte = [kind.byte()];
            // SAFETY: one-byte write, below PIPE_BUF, from a live buffer to
            // the owned nonblocking lease writer; SIGPIPE is off for it.
            let written = unsafe { libc::write(lease.as_raw_fd(), byte.as_ptr().cast(), 1) };
            if written != 1 {
                // WouldBlock or an error: EOF stops the group instead.
                self.lease = None;
            }
        }
    }
    /// Close the lease: the guardian stops the group on EOF, or only ends
    /// lease polling after an earlier stop. Used for a protocol fault or a
    /// lost guardian, after which no notice is trusted.
    pub fn close_lease(&mut self) {
        self.lease = None;
    }
    fn stopping(&self) -> bool {
        self.stop.is_some() || self.fault
    }
    pub fn poll(&mut self, _now: Instant) -> Result<(), ExecCmdError> {
        let result = self.poll_inner();
        if result.is_err() {
            self.fault = true;
            self.close_lease();
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
        let stopping = self.stop.is_some() || self.fault;
        self.sink.flush();
        first.drain(DRAIN_BUDGET, self.sink.as_mut(), stopping)?;
        second.drain(DRAIN_BUDGET, self.sink.as_mut(), stopping)?;
        self.sink.flush();
        // No combined output stop (J1-Q2): the shared quota and pause apply.
        if !self.status_eof {
            let mut buf = [0u8; STATUS_BUFFER];
            // SAFETY: owned nonblocking descriptor and valid output buffer.
            let n =
                unsafe { libc::read(self.status.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len()) };
            if n == 0 {
                self.status_eof = true;
            } else if n > 0 {
                self.status_received = true;
                if self.status_bytes.len() + n as usize > STATUS_BUFFER {
                    return Err(ExecCmdError::Protocol);
                }
                self.status_bytes.extend_from_slice(&buf[..n as usize]);
                while self.status_bytes.len() >= 32 {
                    let notice = Notice::decode(&self.status_bytes[..32], self.job_id)?;
                    self.status_bytes.drain(..32);
                    self.accept(notice)?;
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
    /// Order rules (tool-job-controller.md, TJ-S3): kind 1 first, or kind 3
    /// alone; at most one kind 5, after kind 1; at most one kind 4, after
    /// kind 1 and before kinds 6 and 2, either side of kind 5; at most one
    /// kind 6, after kind 5 and immediately before kind 2 with reason 12;
    /// kind 2 last, after kind 5 and with its exit facts; at most five.
    fn accept(&mut self, notice: Notice) -> Result<(), ExecCmdError> {
        self.notices += 1;
        if self.notices > MAX_NOTICES || self.terminal.is_some() {
            return Err(ExecCmdError::Protocol);
        }
        let after_report = self.last_kind == KIND_ESCAPEE_REPORT;
        let exit = (notice.exit_kind, notice.value);
        match notice.kind {
            1 if self.notices == 1 => self.spawned = true,
            3 if self.notices == 1 => self.terminal = Some(notice),
            KIND_LEADER_EXITED if self.spawned && self.leader.is_none() => self.leader = Some(exit),
            2 if self.leader == Some(exit)
                && (notice.reason == REASON_ESCAPEE_TERMINATED) == after_report =>
            {
                self.terminal = Some(notice)
            }
            4 if self.spawned && self.unconfirmed.is_none() && !after_report => {
                self.unconfirmed = Some(match notice.reason {
                    REASON_CLEANUP_FAILED => UnconfirmedReason::CleanupFailed,
                    REASON_ESCAPEE_UNTERMINATED => UnconfirmedReason::EscapeeUnterminated,
                    _ => return Err(ExecCmdError::Protocol),
                })
            }
            KIND_ESCAPEE_REPORT if self.leader.is_some() && self.escapees == 0 => {
                self.escapees = notice.value
            }
            _ => return Err(ExecCmdError::Protocol),
        }
        self.last_kind = notice.kind;
        Ok(())
    }
    /// Whether notice kind 1 arrived: the command was spawned.
    pub fn spawned(&self) -> bool {
        self.spawned
    }
    /// The leader's exit from notice kind 5, once it arrived. Output may
    /// still flow from other group members until settlement.
    pub fn leader_exit(&self) -> Option<Exit> {
        let (exit_kind, value) = self.leader?;
        Exit::from_notice(&Notice {
            kind: KIND_LEADER_EXITED,
            reason: 0,
            exit_kind,
            cleanup: false,
            value,
        })
    }
    /// Whether the job faulted: a protocol fault, a lost guardian or an
    /// I/O error. The lease is closed and no later notice is trusted.
    pub fn faulted(&self) -> bool {
        self.fault
    }
    /// The guardian's `cleanup_unconfirmed` report, if one arrived. The job
    /// keeps its slot and scratch until a later settled notice.
    pub fn cleanup_unconfirmed(&self) -> Option<UnconfirmedReason> {
        self.unconfirmed
    }
    /// Escapees the guardian reported terminated (notice kind 6).
    pub fn escapees(&self) -> u32 {
        self.escapees
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
            setup_reason: if n.kind == 3 { n.reason } else { 0 },
            exit_code: (n.exit_kind == 1).then_some(n.value as i32),
            signal: (n.exit_kind == 2).then_some(n.value as i32),
            // The guardian's reason names the stop that acted. Reasons 1
            // and 3 are unassigned. Reason 12 replaces the lease reason and
            // reason 0 means the leader exited first; then the service's
            // own stop request, if any, names it.
            reason: match n.reason {
                REASON_CANCEL => Some(StopReason::Cancelled),
                REASON_KILL => Some(StopReason::Killed),
                REASON_LEASE_ENDED => Some(StopReason::LeaseEnded),
                _ => self.stop.map(|kind| match kind {
                    StopKind::Term => StopReason::Cancelled,
                    StopKind::Kill => StopReason::Killed,
                }),
            },
            cleanup_confirmed: self.unconfirmed.is_none(),
            escapees: self.escapees,
        })
    }
    /// The scratch cleanup token, once no process can hold the scratch:
    /// after settlement, or when the guardian ended before any notice
    /// ([`Self::ended_before_notice`]). Otherwise `None`, and the token stays.
    pub fn take_cleanup(&mut self) -> Option<ExecCmdCleanup> {
        if self.settled() || self.ended_before_notice() {
            self.cleanup.take()
        } else {
            None
        }
    }
    /// Whether [`Self::take_cleanup`] may still release the token of a job
    /// that has not settled: no status byte arrived and the guardian or a
    /// capture pipe has not ended yet. False once a status byte arrived,
    /// because the guardian may then have spawned the command.
    pub fn release_pending(&self) -> bool {
        self.cleanup.is_some() && !self.status_received && !self.ended_before_notice()
    }
    /// J2-A2 (tool-job-controller.md): the guardian was reaped, its status
    /// pipe reached EOF with no byte, so no notice arrived, and both capture
    /// pipes reached EOF. The guardian returns before `spawn::launch`
    /// succeeds with at most kind 3; after the launch it always attempts
    /// kind 1 and returns only from its settlement gate (pinned by the
    /// guardian's `j2t_25_no_return_after_launch_without_a_notice`). So no
    /// command was spawned, or its group is gone; an open capture pipe
    /// means a process may still hold the scratch, and the token stays.
    fn ended_before_notice(&self) -> bool {
        self.reaped && self.status_eof && !self.status_received && self.out.eof && self.err.eof
    }
}

#[cfg(test)]
mod tests;
