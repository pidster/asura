use super::{ExecCmdError, ExecCmdFailure, hex};
use crate::RuntimeDirectory;
use crate::runtime::{open_at, stat_at, stat_fd, validate_acl};
use std::ffi::{CStr, CString};
use std::os::fd::{AsRawFd, IntoRawFd, OwnedFd};
use std::sync::atomic::AtomicBool;
use std::time::Instant;
/// Retained token. Dropping it never performs filesystem work.
pub struct ExecCmdCleanup {
    parent: OwnedFd,
    pub(super) directory: OwnedFd,
    name: CString,
    device: u64,
    inode: u64,
    done: bool,
    validated: bool,
}
impl ExecCmdCleanup {
    pub(super) fn create(runtime: &RuntimeDirectory, id: [u8; 16]) -> Result<Self, ExecCmdFailure> {
        let (root, uid) = runtime.authority_root().map_err(|_| ExecCmdError::Denied)?;
        // SAFETY: retained validated parent, fixed single component and private mode.
        if unsafe { libc::mkdirat(root.as_raw_fd(), c"tmp".as_ptr(), 0o700) } != 0
            && std::io::Error::last_os_error().raw_os_error() != Some(libc::EEXIST)
        {
            return Err(ExecCmdError::Io.into());
        }
        let parent = open_at(
            root.as_raw_fd(),
            c"tmp",
            libc::O_RDONLY | libc::O_DIRECTORY,
            0,
        )
        .map_err(|_| ExecCmdError::Denied)?;
        let stat = stat_fd(parent.as_raw_fd()).map_err(|_| ExecCmdError::Io)?;
        if stat.st_uid != uid
            || stat.st_mode & 0o777 != 0o700
            || validate_acl(parent.as_raw_fd(), uid).is_err()
        {
            return Err(ExecCmdError::Denied.into());
        }
        let name =
            CString::new(format!("exec-cmd-{}", hex(id))).map_err(|_| ExecCmdError::Invalid)?;
        let placeholder = parent.try_clone().map_err(|_| ExecCmdError::Io)?;
        // Allocate retained fallback ownership before the first scratch effect.
        let mut token = Self {
            parent,
            directory: placeholder,
            name,
            device: 0,
            inode: 0,
            done: false,
            validated: false,
        };
        if unsafe { libc::mkdirat(token.parent.as_raw_fd(), token.name.as_ptr(), 0o700) } != 0 {
            return Err(ExecCmdError::Denied.into());
        }
        let opened = open_at(
            token.parent.as_raw_fd(),
            &token.name,
            libc::O_RDONLY | libc::O_DIRECTORY,
            0,
        );
        match opened {
            Ok(directory) => token.directory = directory,
            Err(_) => {
                return Err(ExecCmdFailure {
                    reason: ExecCmdError::CleanupUnconfirmed,
                    cleanup: Some(token),
                });
            }
        }
        let stat = match stat_fd(token.directory.as_raw_fd()) {
            Ok(stat) => stat,
            Err(_) => {
                return Err(ExecCmdFailure {
                    reason: ExecCmdError::CleanupUnconfirmed,
                    cleanup: Some(token),
                });
            }
        };
        token.device = stat.st_dev as u64;
        token.inode = stat.st_ino;
        token.validated = true;
        Ok(token)
    }
    /// Blocking bounded descriptor traversal, only on the retained cleanup worker.
    pub fn run(&mut self, deadline: Instant, cancel: &AtomicBool) -> Result<(), ExecCmdError> {
        if !self.validated {
            return Err(ExecCmdError::CleanupUnconfirmed);
        }
        if self.done {
            return Ok(());
        }
        wind_down(deadline, cancel)?;
        let named = stat_at(self.parent.as_raw_fd(), &self.name)
            .map_err(|_| ExecCmdError::CleanupUnconfirmed)?;
        if named.st_dev as u64 != self.device || named.st_ino != self.inode {
            return Err(ExecCmdError::CleanupUnconfirmed);
        }
        let mut remaining = 4096;
        remove_contents(
            self.directory.as_raw_fd(),
            0,
            &mut remaining,
            deadline,
            cancel,
        )?;
        wind_down(deadline, cancel)?;
        let named = stat_at(self.parent.as_raw_fd(), &self.name)
            .map_err(|_| ExecCmdError::CleanupUnconfirmed)?;
        if named.st_dev as u64 != self.device || named.st_ino != self.inode {
            return Err(ExecCmdError::CleanupUnconfirmed);
        }
        // SAFETY: identity checked retained parent/name, directory-only removal.
        if unsafe {
            libc::unlinkat(
                self.parent.as_raw_fd(),
                self.name.as_ptr(),
                libc::AT_REMOVEDIR,
            )
        } != 0
        {
            return Err(ExecCmdError::CleanupUnconfirmed);
        }
        self.done = true;
        Ok(())
    }
}
struct Directory(*mut libc::DIR);
impl Drop for Directory {
    fn drop(&mut self) {
        // SAFETY: exclusive ownership from successful fdopendir.
        unsafe {
            libc::closedir(self.0);
        }
    }
}
/// Cleanup is Asura's own operation. Past its owner-approved wind-down
/// bound the scratch stays for recovery and cleanup reports unconfirmed.
fn wind_down(deadline: Instant, cancel: &AtomicBool) -> Result<(), ExecCmdError> {
    if cancel.load(std::sync::atomic::Ordering::Acquire) {
        Err(ExecCmdError::Cancelled)
    } else if Instant::now() >= deadline {
        Err(ExecCmdError::CleanupUnconfirmed)
    } else {
        Ok(())
    }
}
fn remove_contents(
    fd: i32,
    depth: usize,
    remaining: &mut usize,
    deadline: Instant,
    cancel: &AtomicBool,
) -> Result<(), ExecCmdError> {
    if depth > 32 {
        return Err(ExecCmdError::CleanupUnconfirmed);
    }
    let held =
        open_at(fd, c".", libc::O_RDONLY | libc::O_DIRECTORY, 0).map_err(|_| ExecCmdError::Io)?;
    let raw = held.into_raw_fd();
    // SAFETY: transfer exclusive descriptor to directory stream on success.
    let stream = unsafe { libc::fdopendir(raw) };
    if stream.is_null() {
        unsafe {
            libc::close(raw);
        }
        return Err(ExecCmdError::Io);
    }
    let stream = Directory(stream);
    loop {
        wind_down(deadline, cancel)?;
        // SAFETY: thread-local errno; live exclusively owned stream.
        unsafe {
            *libc::__error() = 0;
        }
        let entry = unsafe { libc::readdir(stream.0) };
        if entry.is_null() {
            return if std::io::Error::last_os_error().raw_os_error() == Some(0) {
                Ok(())
            } else {
                Err(ExecCmdError::Io)
            };
        }
        let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) };
        if name == c"." || name == c".." {
            continue;
        }
        if *remaining == 0 {
            return Err(ExecCmdError::CleanupUnconfirmed);
        }
        *remaining -= 1;
        let before = stat_at(fd, name).map_err(|_| ExecCmdError::Io)?;
        let directory = before.st_mode & libc::S_IFMT == libc::S_IFDIR;
        if directory {
            let child = open_at(fd, name, libc::O_RDONLY | libc::O_DIRECTORY, 0)
                .map_err(|_| ExecCmdError::Denied)?;
            let held = stat_fd(child.as_raw_fd()).map_err(|_| ExecCmdError::Io)?;
            if held.st_dev != before.st_dev || held.st_ino != before.st_ino {
                return Err(ExecCmdError::CleanupUnconfirmed);
            }
            remove_contents(child.as_raw_fd(), depth + 1, remaining, deadline, cancel)?;
        }
        wind_down(deadline, cancel)?;
        let after = stat_at(fd, name).map_err(|_| ExecCmdError::Io)?;
        if before.st_dev != after.st_dev || before.st_ino != after.st_ino {
            return Err(ExecCmdError::CleanupUnconfirmed);
        }
        // SAFETY: no-follow stat and parent-relative unlink; symlinks removed, never traversed.
        if unsafe {
            libc::unlinkat(
                fd,
                name.as_ptr(),
                if directory { libc::AT_REMOVEDIR } else { 0 },
            )
        } != 0
        {
            return Err(ExecCmdError::Io);
        }
    }
}
