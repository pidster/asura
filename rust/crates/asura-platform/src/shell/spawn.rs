use super::ShellError;
use crate::startup::{SpawnActions, SpawnAttributes};
use std::ffi::CString;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::Path;
unsafe extern "C" {
    fn posix_spawn_file_actions_addfchdir(
        actions: *mut libc::posix_spawn_file_actions_t,
        fd: i32,
    ) -> i32;
}
fn ok(n: i32) -> Result<(), ShellError> {
    if n == 0 { Ok(()) } else { Err(ShellError::Io) }
}
pub(super) fn interrupted() -> bool {
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EINTR)
}
pub(super) fn would_block() -> bool {
    matches!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::EAGAIN) | Some(libc::EINTR)
    )
}
pub(super) fn nonblocking(fd: RawFd) -> Result<(), ShellError> {
    // SAFETY: inspect and update flags on the caller-owned descriptor.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        Err(ShellError::Io)
    } else {
        Ok(())
    }
}
pub(super) fn pipe(nonblocking_write: bool) -> Result<(OwnedFd, OwnedFd), ShellError> {
    let (read, write) = crate::startup::pipe().map_err(|_| ShellError::Io)?;
    nonblocking(read.as_raw_fd())?;
    if nonblocking_write {
        nonblocking(write.as_raw_fd())?;
    }
    Ok((read, write))
}
pub(super) fn monotonic_ns() -> Result<u64, ShellError> {
    // SAFETY: initialized output passed to the fixed monotonic clock.
    let mut t: libc::timespec = unsafe { std::mem::zeroed() };
    if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut t) } != 0
        || t.tv_sec < 0
        || t.tv_nsec < 0
    {
        return Err(ShellError::Io);
    }
    (t.tv_sec as u64)
        .checked_mul(1_000_000_000)
        .and_then(|n| n.checked_add(t.tv_nsec as u64))
        .ok_or(ShellError::Invalid)
}
pub(super) fn path(fd: RawFd) -> Result<String, ShellError> {
    let mut value = [0u8; libc::PATH_MAX as usize];
    // SAFETY: F_GETPATH writes at most PATH_MAX bytes to this live buffer.
    if unsafe { libc::fcntl(fd, libc::F_GETPATH, value.as_mut_ptr()) } != 0 {
        return Err(ShellError::Denied);
    }
    let end = value
        .iter()
        .position(|b| *b == 0)
        .ok_or(ShellError::Denied)?;
    let path = std::str::from_utf8(&value[..end]).map_err(|_| ShellError::Denied)?;
    if !path.starts_with('/') || path.chars().any(char::is_control) {
        return Err(ShellError::Denied);
    }
    Ok(path.into())
}
/// All allocations, actions, attributes and duplicates are prepared before spawn.
pub(super) fn launch(
    executable: &Path,
    args: &[String],
    env: &[String],
    mappings: &[(RawFd, RawFd)],
    cwd: Option<RawFd>,
    group: bool,
) -> Result<libc::pid_t, ShellError> {
    let executable =
        CString::new(executable.as_os_str().as_encoded_bytes()).map_err(|_| ShellError::Invalid)?;
    let args: Vec<CString> = args
        .iter()
        .map(|s| CString::new(s.as_bytes()).map_err(|_| ShellError::Invalid))
        .collect::<Result<_, _>>()?;
    let env: Vec<CString> = env
        .iter()
        .map(|s| CString::new(s.as_bytes()).map_err(|_| ShellError::Invalid))
        .collect::<Result<_, _>>()?;
    let mut argv: Vec<*mut libc::c_char> = args.iter().map(|s| s.as_ptr().cast_mut()).collect();
    argv.push(std::ptr::null_mut());
    let mut envp: Vec<*mut libc::c_char> = env.iter().map(|s| s.as_ptr().cast_mut()).collect();
    envp.push(std::ptr::null_mut());
    let mut duplicates = Vec::new();
    for (source, target) in mappings {
        // SAFETY: source is retained by caller. Duplicate above all fixed targets
        // prevents sequential dup2 actions from clobbering later source FDs.
        let fd = unsafe { libc::fcntl(*source, libc::F_DUPFD_CLOEXEC, 20) };
        if fd < 0 {
            return Err(ShellError::Io);
        }
        duplicates.push((unsafe { OwnedFd::from_raw_fd(fd) }, *target));
    }
    // SAFETY: opaque objects initialized by their native constructors.
    let mut raw = unsafe { std::mem::zeroed() };
    ok(unsafe { libc::posix_spawn_file_actions_init(&mut raw) })?;
    let mut actions = SpawnActions(raw);
    if let Some(fd) = cwd {
        ok(unsafe { posix_spawn_file_actions_addfchdir(&mut actions.0, fd) })?;
    }
    for (source, target) in &duplicates {
        ok(unsafe {
            libc::posix_spawn_file_actions_adddup2(&mut actions.0, source.as_raw_fd(), *target)
        })?;
    }
    if group {
        for fd in 3..=9 {
            ok(unsafe { libc::posix_spawn_file_actions_addclose(&mut actions.0, fd) })?;
        }
    }
    let mut raw = unsafe { std::mem::zeroed() };
    ok(unsafe { libc::posix_spawnattr_init(&mut raw) })?;
    let mut attrs = SpawnAttributes(raw);
    let flags = libc::POSIX_SPAWN_CLOEXEC_DEFAULT
        | if group {
            libc::POSIX_SPAWN_SETPGROUP
        } else {
            0
        };
    ok(unsafe { libc::posix_spawnattr_setflags(&mut attrs.0, flags as i16) })?;
    if group {
        ok(unsafe { libc::posix_spawnattr_setpgroup(&mut attrs.0, 0) })?;
    }
    let mut pid = 0;
    // SAFETY: strings and terminated pointer arrays remain alive, actions/attrs
    // are initialized, and pid is a complete output location.
    ok(unsafe {
        libc::posix_spawn(
            &mut pid,
            executable.as_ptr(),
            &actions.0,
            &attrs.0,
            argv.as_ptr(),
            envp.as_ptr(),
        )
    })?;
    Ok(pid)
}
