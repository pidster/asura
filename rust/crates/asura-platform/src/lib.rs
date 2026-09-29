//! The single macOS platform boundary for the local service.
#![cfg(target_os = "macos")]

mod audit_io;
mod authority;
pub mod events;
mod journal_io;
pub use audit_io::{AuditArchive, AuditDirectory, AuditFile, audit_archive_name};
mod model_process;
pub use model_process::ModelProcess;
pub mod git_observer;
mod project_identity;
pub use journal_io::{DatabaseDirectory, JournalFile};
pub use project_identity::{ProjectIdentity, ProjectListing, ProjectPage, ProjectReadError};
mod config;
pub use authority::{
    AuthorityError, AuthorityReader, AuthorityScan, AuthoritySource, AuthorityWitness,
};
pub use config::{PrivateFileError, PrivateFileSnapshot};
mod lifetime;
pub use lifetime::{ServiceLease, ServiceLifetime};
mod runtime;
pub mod shell;
mod startup;
mod terminal;
pub use runtime::{
    AuthenticatedStream, LockWitness, OwnerGuard, OwnerValidation, RuntimeDirectory,
    account_home_path, open_private_append,
};
pub use startup::{StartupChild, StartupNotice, StartupPipe, spawn_service, spawn_service_owned};
pub use terminal::{TerminalIo, TerminalOutput};

use std::io;
use std::os::fd::RawFd;
use std::sync::atomic::{AtomicI32, Ordering};
use std::time::{Duration, Instant};

#[derive(Debug)]
pub enum Error {
    UnsafeRuntime,
    OwnerBusy,
    Absent,
    Refused,
    Unavailable,
    InvalidNotice,
    Deadline,
    Io(io::Error),
}
pub type Result<T> = std::result::Result<T, Error>;
impl From<io::Error> for Error {
    fn from(error: io::Error) -> Self {
        match error.raw_os_error() {
            Some(libc::ENOENT) => Self::Absent,
            Some(libc::ECONNREFUSED) => Self::Refused,
            _ => Self::Io(error),
        }
    }
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::UnsafeRuntime => "unsafe runtime",
            Self::OwnerBusy => "owner busy",
            Self::Absent => "service absent",
            Self::Refused => "connection refused",
            Self::Unavailable => "service unavailable",
            Self::InvalidNotice => "invalid startup notice",
            Self::Deadline => "deadline exceeded",
            Self::Io(_) => "platform I/O failure",
        })
    }
}
impl std::error::Error for Error {}

pub(crate) fn last_error() -> Error {
    io::Error::last_os_error().into()
}
pub(crate) fn checked(result: i32) -> Result<()> {
    if result == -1 {
        Err(last_error())
    } else {
        Ok(())
    }
}
pub(crate) fn nonblocking(fd: RawFd) -> Result<()> {
    // SAFETY: fcntl accepts a borrowed descriptor and does not transfer ownership.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    checked(flags)?;
    // SAFETY: preserve all existing flags, adding only nonblocking I/O.
    checked(unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) })
}
pub(crate) fn effective_uid() -> Result<libc::uid_t> {
    // SAFETY: read-only scalar process identity query.
    let uid = unsafe { libc::geteuid() };
    if uid == 0 {
        Err(Error::UnsafeRuntime)
    } else {
        Ok(uid)
    }
}

pub fn random_id() -> [u8; 16] {
    let mut bytes = [0; 16];
    // SAFETY: the writable buffer contains the stated 16 bytes. arc4random_buf
    // supplies system randomness and has no recoverable failure result.
    unsafe { libc::arc4random_buf(bytes.as_mut_ptr().cast(), bytes.len()) };
    bytes
}
#[derive(Clone, Copy, Debug)]
pub struct PollInterest {
    pub fd: RawFd,
    pub read: bool,
    pub write: bool,
}
#[derive(Clone, Copy, Debug)]
pub struct Ready {
    pub index: usize,
    pub read: bool,
    pub write: bool,
    pub closed: bool,
}
pub fn poll(interests: &[PollInterest], timeout: Duration) -> Result<Vec<Ready>> {
    if interests.len() > 65 {
        return Err(Error::Unavailable);
    }
    let end = Instant::now() + timeout;
    let mut fds: Vec<_> = interests
        .iter()
        .map(|i| libc::pollfd {
            fd: i.fd,
            events: (if i.read { libc::POLLIN } else { 0 })
                | (if i.write { libc::POLLOUT } else { 0 }),
            revents: 0,
        })
        .collect();
    loop {
        let remaining = end.saturating_duration_since(Instant::now());
        let ms = remaining
            .as_nanos()
            .div_ceil(1_000_000)
            .min(i32::MAX as u128) as i32;
        // SAFETY: fds owns len initialized pollfd records for this call.
        let result = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, ms) };
        if result >= 0 {
            break;
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error.into());
        }
        if Instant::now() >= end {
            return Ok(Vec::new());
        }
    }
    fds.iter()
        .enumerate()
        .filter(|(_, f)| f.revents != 0)
        .map(|(index, f)| {
            if f.revents & libc::POLLNVAL != 0 {
                return Err(Error::Unavailable);
            }
            Ok(Ready {
                index,
                read: f.revents & libc::POLLIN != 0,
                write: f.revents & libc::POLLOUT != 0,
                closed: f.revents & (libc::POLLHUP | libc::POLLERR) != 0,
            })
        })
        .collect()
}
static TERMINATION: AtomicI32 = AtomicI32::new(0);
static TERMINATION_FD: AtomicI32 = AtomicI32::new(-1);
static TERMINATION_WAKE: std::sync::OnceLock<
    std::io::Result<(events::WakeReader, events::WakeSender)>,
> = std::sync::OnceLock::new();
pub fn termination_descriptor() -> Result<RawFd> {
    use std::os::fd::AsRawFd;
    let (reader, writer) = TERMINATION_WAKE
        .get_or_init(events::wake_pair)
        .as_ref()
        .map_err(|_| Error::Unavailable)?;
    TERMINATION_FD.store(writer.as_raw_fd(), Ordering::Release);
    Ok(reader.as_raw_fd())
}
extern "C" fn terminate(signal: i32) {
    TERMINATION.store(signal, Ordering::Relaxed);
    let fd = TERMINATION_FD.load(Ordering::Acquire);
    if fd >= 0 {
        // SAFETY: process-lifetime socket pair retains both ends. write is signal-safe;
        // one nonblocking byte is a hint, and full means the reader is already ready.
        unsafe {
            let saved = *libc::__error();
            libc::write(fd, b"s".as_ptr().cast(), 1);
            *libc::__error() = saved;
        }
    }
}
pub fn install_signals() -> Result<()> {
    termination_descriptor()?;
    for signal in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP] {
        // SAFETY: initialized sigaction uses a static signal-safe atomic handler.
        let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
        action.sa_sigaction = terminate as *const () as usize;
        // SAFETY: action mask is writable, action lives through installation.
        unsafe {
            libc::sigemptyset(&mut action.sa_mask);
        }
        // SAFETY: kernel copies the supplied initialized record.
        checked(unsafe { libc::sigaction(signal, &action, std::ptr::null_mut()) })?;
    }
    Ok(())
}
pub fn take_termination_signal() -> Option<i32> {
    if let Some(Ok((reader, _))) = TERMINATION_WAKE.get() {
        let _ = reader.drain();
    }
    match TERMINATION.swap(0, Ordering::Relaxed) {
        0 => None,
        signal => Some(signal),
    }
}

#[cfg(test)]
mod tests;
