use crate::{Error, Result, checked, last_error, nonblocking};
use std::ffi::CString;
use std::fs::File;
use std::io::{self, Read};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::time::Instant;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StartupNotice {
    Bound,
    OwnerBusy,
    UnsafeRuntime,
    Unavailable,
    InternalFailure,
}
impl StartupNotice {
    pub fn encode(self) -> [u8; 8] {
        let (kind, code) = match self {
            Self::Bound => (1, 0),
            Self::OwnerBusy => (2, 0),
            Self::UnsafeRuntime => (3, 1),
            Self::Unavailable => (3, 2),
            Self::InternalFailure => (3, 3),
        };
        [b'A', b'S', b'S', b'T', 1, kind, 0, code]
    }
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() != 8 || bytes[..5] != [b'A', b'S', b'S', b'T', 1] || bytes[6] != 0 {
            return Err(Error::InvalidNotice);
        }
        match (bytes[5], bytes[7]) {
            (1, 0) => Ok(Self::Bound),
            (2, 0) => Ok(Self::OwnerBusy),
            (3, 1) => Ok(Self::UnsafeRuntime),
            (3, 2) => Ok(Self::Unavailable),
            (3, 3) => Ok(Self::InternalFailure),
            _ => Err(Error::InvalidNotice),
        }
    }
}
pub struct StartupPipe(OwnedFd);
impl StartupPipe {
    pub fn inherited() -> Result<Self> {
        Self::validate_descriptor(3)?;
        // SAFETY: only the fixed private launch invocation transfers FD3 ownership here.
        let fd = unsafe { OwnedFd::from_raw_fd(3) };
        nonblocking(fd.as_raw_fd())?;
        Ok(Self(fd))
    }
    fn validate_descriptor(fd: i32) -> Result<()> {
        // SAFETY: initialized output record; fd is inspected without taking ownership.
        let mut stat: libc::stat = unsafe { std::mem::zeroed() };
        // SAFETY: output points to complete stat; fcntl is read-only.
        if unsafe { libc::fstat(fd, &mut stat) } != 0 {
            return Err(Error::InvalidNotice);
        }
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        if flags < 0
            || stat.st_mode & libc::S_IFMT != libc::S_IFIFO
            || flags & libc::O_ACCMODE != libc::O_WRONLY
        {
            return Err(Error::InvalidNotice);
        }
        Ok(())
    }
    pub fn send(self, notice: StartupNotice) -> Result<()> {
        let bytes = notice.encode();
        // SAFETY: one bounded write below PIPE_BUF; descriptor is nonblocking and owned.
        let count = unsafe { libc::write(self.0.as_raw_fd(), bytes.as_ptr().cast(), bytes.len()) };
        if count == bytes.len() as isize {
            Ok(())
        } else if count < 0 {
            Err(last_error())
        } else {
            Err(Error::InvalidNotice)
        }
    }
}
pub struct StartupChild {
    pid: libc::pid_t,
    reader: File,
    bytes: Vec<u8>,
    notice: Option<StartupNotice>,
    reaped: Option<i32>,
}
impl StartupChild {
    pub fn poll_notice(&mut self, deadline: Instant) -> Result<Option<StartupNotice>> {
        if let Some(notice) = self.notice {
            return Ok(Some(notice));
        }
        if Instant::now() >= deadline {
            return Err(Error::Deadline);
        }
        loop {
            let mut buffer = [0u8; 9];
            match self.reader.read(&mut buffer) {
                Ok(0) => {
                    let notice = StartupNotice::decode(&self.bytes)?;
                    self.notice = Some(notice);
                    return Ok(Some(notice));
                }
                Ok(n) => {
                    if self.bytes.len() + n > 8 {
                        return Err(Error::InvalidNotice);
                    }
                    self.bytes.extend_from_slice(&buffer[..n]);
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => return Ok(None),
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {
                    if Instant::now() >= deadline {
                        return Err(Error::Deadline);
                    }
                }
                Err(e) => return Err(e.into()),
            }
        }
    }
    pub fn try_reap(&mut self) -> Result<Option<i32>> {
        if self.reaped.is_some() {
            return Ok(self.reaped);
        }
        let mut status = 0;
        // SAFETY: pid is the direct child returned by our successful posix_spawn.
        let result = unsafe { libc::waitpid(self.pid, &mut status, libc::WNOHANG) };
        if result == self.pid {
            self.reaped = Some(status);
        } else if result < 0 && io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
            return Err(last_error());
        }
        Ok(self.reaped)
    }
}
unsafe extern "C" {
    fn posix_spawn_file_actions_addchdir(
        actions: *mut libc::posix_spawn_file_actions_t,
        path: *const libc::c_char,
    ) -> i32;
}
pub(crate) struct SpawnActions(pub(crate) libc::posix_spawn_file_actions_t);
impl Drop for SpawnActions {
    fn drop(&mut self) {
        // SAFETY: actions were successfully initialized and remain exclusively owned.
        unsafe {
            libc::posix_spawn_file_actions_destroy(&mut self.0);
        }
    }
}
pub(crate) struct SpawnAttributes(pub(crate) libc::posix_spawnattr_t);
impl Drop for SpawnAttributes {
    fn drop(&mut self) {
        // SAFETY: attributes were initialized and remain exclusively owned.
        unsafe {
            libc::posix_spawnattr_destroy(&mut self.0);
        }
    }
}
pub(crate) fn spawn_result(result: i32) -> Result<()> {
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(result).into())
    }
}
pub(crate) fn pipe() -> Result<(OwnedFd, OwnedFd)> {
    let mut fds = [-1; 2];
    // SAFETY: writable two-element descriptor output.
    checked(unsafe { libc::pipe(fds.as_mut_ptr()) })?;
    // SAFETY: pipe returned two new independently owned descriptors.
    let read = unsafe { OwnedFd::from_raw_fd(fds[0]) };
    let write = unsafe { OwnedFd::from_raw_fd(fds[1]) };
    for fd in [&read, &write] {
        // SAFETY: update only descriptor inheritance flag of owned descriptor.
        checked(unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) })?;
    }
    Ok((read, write))
}
pub(crate) fn duplicate_for_spawn(fd: i32) -> Result<OwnedFd> {
    // Keep sources above stdio and fixed FD3/FD4 to prevent action alias collisions.
    // SAFETY: fcntl duplicates the borrowed descriptor; no parent descriptor is changed.
    let duplicated = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 5) };
    if duplicated < 0 {
        return Err(last_error());
    }
    // SAFETY: successful duplication returns an exclusively owned descriptor.
    Ok(unsafe { OwnedFd::from_raw_fd(duplicated) })
}
fn startup_actions(
    notice: &OwnedFd,
    stderr: &OwnedFd,
    lifetime: Option<&OwnedFd>,
) -> Result<SpawnActions> {
    // SAFETY: opaque handle is initialized before any operation or destruction.
    let mut actions = unsafe { std::mem::zeroed() };
    spawn_result(unsafe { libc::posix_spawn_file_actions_init(&mut actions) })?;
    let mut actions = SpawnActions(actions);
    // SAFETY: fixed valid descriptors and terminated strings remain alive through spawn.
    unsafe {
        spawn_result(posix_spawn_file_actions_addchdir(
            &mut actions.0,
            c"/".as_ptr(),
        ))?;
        for fd in 0..=1 {
            spawn_result(libc::posix_spawn_file_actions_addopen(
                &mut actions.0,
                fd,
                c"/dev/null".as_ptr(),
                libc::O_RDWR,
                0,
            ))?;
        }
        spawn_result(libc::posix_spawn_file_actions_adddup2(
            &mut actions.0,
            stderr.as_raw_fd(),
            2,
        ))?;
        spawn_result(libc::posix_spawn_file_actions_adddup2(
            &mut actions.0,
            notice.as_raw_fd(),
            3,
        ))?;
        if let Some(lifetime) = lifetime {
            spawn_result(libc::posix_spawn_file_actions_adddup2(
                &mut actions.0,
                lifetime.as_raw_fd(),
                4,
            ))?;
            spawn_result(libc::posix_spawn_file_actions_addclose(
                &mut actions.0,
                lifetime.as_raw_fd(),
            ))?;
        }
        spawn_result(libc::posix_spawn_file_actions_addclose(
            &mut actions.0,
            stderr.as_raw_fd(),
        ))?;
        spawn_result(libc::posix_spawn_file_actions_addclose(
            &mut actions.0,
            notice.as_raw_fd(),
        ))?;
    }
    Ok(actions)
}
pub fn spawn_service(log: Option<&File>) -> Result<StartupChild> {
    spawn_service_inner(log, None)
}
pub fn spawn_service_owned(
    log: Option<&File>,
    lifetime: &crate::ServiceLifetime,
) -> Result<StartupChild> {
    spawn_service_inner(log, Some(lifetime))
}
fn spawn_service_inner(
    log: Option<&File>,
    lifetime: Option<&crate::ServiceLifetime>,
) -> Result<StartupChild> {
    let executable = std::env::current_exe()?.canonicalize()?;
    let path = CString::new(executable.as_os_str().as_bytes()).map_err(|_| Error::Unavailable)?;
    // SAFETY: terminated absolute path, no symlink following at final component.
    let raw = unsafe {
        libc::open(
            path.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if raw < 0 {
        return Err(last_error());
    }
    // SAFETY: successful open transfers this descriptor exclusively.
    let held = unsafe { File::from_raw_fd(raw) };
    let before = held.metadata()?;
    if !before.is_file() || before.mode() & 0o111 == 0 {
        return Err(Error::Unavailable);
    }
    let (read, write) = pipe()?;
    let source = duplicate_for_spawn(write.as_raw_fd())?;
    let stderr_source = duplicate_for_spawn(log.map_or(2, AsRawFd::as_raw_fd))?;
    let lifetime_source = lifetime
        .map(|reader| duplicate_for_spawn(reader.as_raw_fd()))
        .transpose()?;
    let actions = startup_actions(&source, &stderr_source, lifetime_source.as_ref())?;
    // SAFETY: same initialization requirement for attributes.
    let mut attributes = unsafe { std::mem::zeroed() };
    spawn_result(unsafe { libc::posix_spawnattr_init(&mut attributes) })?;
    let mut attributes = SpawnAttributes(attributes);
    // SAFETY: flags are installed SDK values, actions refer to retained descriptors/strings.
    unsafe {
        spawn_result(libc::posix_spawnattr_setflags(
            &mut attributes.0,
            (0x0400 | libc::POSIX_SPAWN_CLOEXEC_DEFAULT) as i16,
        ))?;
    }
    let named = std::fs::symlink_metadata(&executable)?;
    let after = held.metadata()?;
    let fingerprint =
        |m: &std::fs::Metadata| (m.dev(), m.ino(), m.len(), m.mtime(), m.mtime_nsec());
    if !named.is_file()
        || fingerprint(&before) != fingerprint(&named)
        || fingerprint(&before) != fingerprint(&after)
    {
        return Err(Error::Unavailable);
    }
    let mut argv = [
        path.as_ptr().cast_mut(),
        c"service".as_ptr().cast_mut(),
        c"run".as_ptr().cast_mut(),
        c"--internal-startup-notice".as_ptr().cast_mut(),
        if lifetime.is_some() {
            c"--internal-owner-lifetime".as_ptr().cast_mut()
        } else {
            std::ptr::null_mut()
        },
        std::ptr::null_mut(),
    ];
    let mut environment = [
        c"LANG=C".as_ptr().cast_mut(),
        c"LC_ALL=C".as_ptr().cast_mut(),
        std::ptr::null_mut(),
    ];
    let mut pid = 0;
    // SAFETY: initialized actions/attributes and NUL-terminated pointer arrays remain alive through spawn.
    spawn_result(unsafe {
        libc::posix_spawn(
            &mut pid,
            path.as_ptr(),
            &actions.0,
            &attributes.0,
            argv.as_mut_ptr(),
            environment.as_mut_ptr(),
        )
    })?;
    drop(source);
    drop(write);
    nonblocking(read.as_raw_fd())?;
    Ok(StartupChild {
        pid,
        reader: File::from(read),
        bytes: Vec::with_capacity(8),
        notice: None,
        reaped: None,
    })
}

#[cfg(test)]
#[path = "startup_tests.rs"]
mod tests;
