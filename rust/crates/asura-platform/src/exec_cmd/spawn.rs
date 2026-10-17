use super::ExecCmdError;
use crate::startup::{SpawnActions, SpawnAttributes};
use std::ffi::CString;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::Path;
use zeroize::Zeroizing;
unsafe extern "C" {
    fn posix_spawn_file_actions_addfchdir(
        actions: *mut libc::posix_spawn_file_actions_t,
        fd: i32,
    ) -> i32;
}
fn ok(n: i32) -> Result<(), ExecCmdError> {
    if n == 0 {
        Ok(())
    } else {
        Err(ExecCmdError::Io)
    }
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
pub(super) fn nonblocking(fd: RawFd) -> Result<(), ExecCmdError> {
    // SAFETY: inspect and update flags on the caller-owned descriptor.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        Err(ExecCmdError::Io)
    } else {
        Ok(())
    }
}
pub(super) fn pipe(nonblocking_write: bool) -> Result<(OwnedFd, OwnedFd), ExecCmdError> {
    let (read, write) = crate::startup::pipe().map_err(|_| ExecCmdError::Io)?;
    nonblocking(read.as_raw_fd())?;
    if nonblocking_write {
        nonblocking(write.as_raw_fd())?;
    }
    Ok((read, write))
}
pub(super) fn monotonic_ns() -> Result<u64, ExecCmdError> {
    // SAFETY: initialized output passed to the fixed monotonic clock.
    let mut t: libc::timespec = unsafe { std::mem::zeroed() };
    if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut t) } != 0
        || t.tv_sec < 0
        || t.tv_nsec < 0
    {
        return Err(ExecCmdError::Io);
    }
    (t.tv_sec as u64)
        .checked_mul(1_000_000_000)
        .and_then(|n| n.checked_add(t.tv_nsec as u64))
        .ok_or(ExecCmdError::Invalid)
}
pub(super) fn path(fd: RawFd) -> Result<String, ExecCmdError> {
    let mut value = [0u8; libc::PATH_MAX as usize];
    // SAFETY: F_GETPATH writes at most PATH_MAX bytes to this live buffer.
    if unsafe { libc::fcntl(fd, libc::F_GETPATH, value.as_mut_ptr()) } != 0 {
        return Err(ExecCmdError::Denied);
    }
    let end = value
        .iter()
        .position(|b| *b == 0)
        .ok_or(ExecCmdError::Denied)?;
    let path = std::str::from_utf8(&value[..end]).map_err(|_| ExecCmdError::Denied)?;
    if !path.starts_with('/') || path.chars().any(char::is_control) {
        return Err(ExecCmdError::Denied);
    }
    Ok(path.into())
}
/// One NUL-terminated environment entry in memory that is zeroed on drop.
/// `CString` clears only its first byte when dropped, so it cannot hold a
/// secret variable.
fn env_entry(entry: &[u8]) -> Result<Zeroizing<Vec<u8>>, ExecCmdError> {
    if entry.contains(&0) {
        return Err(ExecCmdError::Invalid);
    }
    let mut owned = Zeroizing::new(Vec::with_capacity(entry.len() + 1));
    owned.extend_from_slice(entry);
    owned.push(0);
    Ok(owned)
}
/// All allocations, actions, attributes and duplicates are prepared before spawn.
/// Environment entries may hold secret variables: their copies here are zeroed
/// when this function returns, whether or not the spawn succeeded.
pub(super) fn launch(
    executable: &Path,
    args: &[String],
    env: &[&[u8]],
    mappings: &[(RawFd, RawFd)],
    cwd: Option<RawFd>,
    group: bool,
) -> Result<libc::pid_t, ExecCmdError> {
    let executable = CString::new(executable.as_os_str().as_encoded_bytes())
        .map_err(|_| ExecCmdError::Invalid)?;
    let args: Vec<CString> = args
        .iter()
        .map(|s| CString::new(s.as_bytes()).map_err(|_| ExecCmdError::Invalid))
        .collect::<Result<_, _>>()?;
    let env: Vec<Zeroizing<Vec<u8>>> = env
        .iter()
        .map(|entry| env_entry(entry))
        .collect::<Result<_, _>>()?;
    let mut argv: Vec<*mut libc::c_char> = args.iter().map(|s| s.as_ptr().cast_mut()).collect();
    argv.push(std::ptr::null_mut());
    // Each entry is NUL-terminated by env_entry and is neither moved nor
    // resized before `env` drops at return, so these pointers stay valid.
    let mut envp: Vec<*mut libc::c_char> = env
        .iter()
        .map(|s| s.as_ptr().cast::<libc::c_char>().cast_mut())
        .collect();
    envp.push(std::ptr::null_mut());
    let mut duplicates = Vec::new();
    for (source, target) in mappings {
        // SAFETY: source is retained by caller. Duplicate above all fixed targets
        // prevents sequential dup2 actions from clobbering later source FDs.
        let fd = unsafe { libc::fcntl(*source, libc::F_DUPFD_CLOEXEC, 20) };
        if fd < 0 {
            return Err(ExecCmdError::Io);
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
