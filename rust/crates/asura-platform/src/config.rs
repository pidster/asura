//! Descriptor-relative bounded private-file operations. Call only from an isolated worker.
use crate::runtime::{identity, open_at, stat_at, stat_fd, valid_stat, validate_acl};
use crate::{Error, RuntimeDirectory, random_id};
use std::ffi::CString;
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, OwnedFd};
use std::sync::atomic::{AtomicBool, Ordering};

/// The whole-file bound of a private configuration file: 64 KiB
/// (execution-setting.md, configuration; ES-E6). It holds the largest
/// `execution.projects` mapping of 1,024 overrides beside the other keys.
/// A read past it, or a replacement larger than it, is `TooLarge`.
pub const CONFIG_FILE_LIMIT: usize = 64 * 1024;
const LIMIT: usize = CONFIG_FILE_LIMIT;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PrivateFileError {
    UnsafeOrIo,
    /// The owner cancelled the operation. These operations have no time
    /// bound (owner, 2026-10-16); the name is kept for wire-stable callers.
    CancelledOrExpired,
    TooLarge,
    Read,
    Changed,
    Write,
    Sync,
    Replace,
    OutcomeUnconfirmed,
}
type Result<T> = std::result::Result<T, PrivateFileError>;
fn error(_: Error) -> PrivateFileError {
    PrivateFileError::UnsafeOrIo
}
fn active(cancel: &AtomicBool) -> Result<()> {
    if cancel.load(Ordering::Acquire) {
        Err(PrivateFileError::CancelledOrExpired)
    } else {
        Ok(())
    }
}
fn same(a: &libc::stat, b: &libc::stat) -> bool {
    identity(a) == identity(b)
        && a.st_size == b.st_size
        && a.st_mtime == b.st_mtime
        && a.st_mtime_nsec == b.st_mtime_nsec
        && a.st_ctime == b.st_ctime
        && a.st_ctime_nsec == b.st_ctime_nsec
        && a.st_mode == b.st_mode
        && a.st_nlink == b.st_nlink
}
/// Retains the validated parent and the exact snapshot that a mutation may replace.
pub struct PrivateFileSnapshot {
    runtime: RuntimeDirectory,
    name: CString,
    parent: OwnedFd,
    uid: libc::uid_t,
    stamp: Option<libc::stat>,
    pub bytes: Vec<u8>,
}
impl PrivateFileSnapshot {
    /// Whether the validated snapshot came from an existing file, including an empty one.
    pub fn existed(&self) -> bool {
        self.stamp.is_some()
    }

    /// Read a validated snapshot. It has no time bound (J1-0; owner,
    /// 2026-10-16): only the owner's cancel or a real error stops the read.
    pub fn read(runtime: RuntimeDirectory, name: &str, cancel: &AtomicBool) -> Result<Self> {
        active(cancel)?;
        if name.is_empty() || name.len() > 255 || name.contains('/') || matches!(name, "." | "..") {
            return Err(PrivateFileError::UnsafeOrIo);
        }
        let name = CString::new(name).map_err(|_| PrivateFileError::UnsafeOrIo)?;
        let (parent, uid) = runtime.authority_root().map_err(error)?;
        let mut snapshot = Self {
            runtime,
            name,
            parent,
            uid,
            stamp: None,
            bytes: Vec::new(),
        };
        match open_at(
            snapshot.parent.as_raw_fd(),
            &snapshot.name,
            libc::O_RDONLY,
            0,
        ) {
            Err(Error::Absent) => (),
            Err(e) => return Err(error(e)),
            Ok(fd) => {
                let before = stat_fd(fd.as_raw_fd()).map_err(error)?;
                valid_stat(&before, uid, libc::S_IFREG, Some(0o600)).map_err(error)?;
                validate_acl(fd.as_raw_fd(), uid).map_err(error)?;
                if before.st_size < 0 || before.st_size as u64 > LIMIT as u64 {
                    return Err(PrivateFileError::TooLarge);
                }
                let mut file = File::from(fd);
                (&mut file)
                    .take((LIMIT + 1) as u64)
                    .read_to_end(&mut snapshot.bytes)
                    .map_err(|_| PrivateFileError::Read)?;
                if snapshot.bytes.len() > LIMIT {
                    return Err(PrivateFileError::TooLarge);
                }
                if !same(&before, &stat_fd(file.as_raw_fd()).map_err(error)?) {
                    return Err(PrivateFileError::Changed);
                }
                snapshot.stamp = Some(before);
            }
        }
        snapshot.validate()?;
        active(cancel)?;
        Ok(snapshot)
    }
    fn validate(&self) -> Result<()> {
        self.runtime.validate().map_err(error)?;
        match (
            self.stamp.as_ref(),
            stat_at(self.parent.as_raw_fd(), &self.name),
        ) {
            (None, Err(Error::Absent)) => Ok(()),
            (Some(before), Ok(after)) if same(before, &after) => {
                valid_stat(&after, self.uid, libc::S_IFREG, Some(0o600)).map_err(error)?;
                let fd = open_at(self.parent.as_raw_fd(), &self.name, libc::O_RDONLY, 0)
                    .map_err(error)?;
                if !same(before, &stat_fd(fd.as_raw_fd()).map_err(error)?) {
                    return Err(PrivateFileError::Changed);
                }
                validate_acl(fd.as_raw_fd(), self.uid).map_err(error)
            }
            _ => Err(PrivateFileError::Changed),
        }
    }
    /// Atomically replace the snapshot. No time bound; cancel stops it before
    /// the rename, after which the outcome is reported exactly.
    pub fn replace(&self, bytes: &[u8], cancel: &AtomicBool) -> Result<()> {
        if bytes.len() > LIMIT {
            return Err(PrivateFileError::TooLarge);
        }
        active(cancel)?;
        self.validate()?;
        let name = CString::new(format!(
            ".private-{:032x}.tmp",
            u128::from_ne_bytes(random_id())
        ))
        .unwrap();
        let fd = open_at(
            self.parent.as_raw_fd(),
            &name,
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
            0o600,
        )
        .map_err(error)?;
        let mut file = File::from(fd);
        let created = stat_fd(file.as_raw_fd()).map_err(error)?;
        let result = (|| {
            valid_stat(&created, self.uid, libc::S_IFREG, Some(0o600)).map_err(error)?;
            validate_acl(file.as_raw_fd(), self.uid).map_err(error)?;
            file.write_all(bytes).map_err(|_| PrivateFileError::Write)?;
            file.sync_all().map_err(|_| PrivateFileError::Sync)?;
            self.validate()?;
            let ready = stat_fd(file.as_raw_fd()).map_err(error)?;
            valid_stat(&ready, self.uid, libc::S_IFREG, Some(0o600)).map_err(error)?;
            validate_acl(file.as_raw_fd(), self.uid).map_err(error)?;
            if identity(&ready) != identity(&created)
                || !same(
                    &stat_at(self.parent.as_raw_fd(), &name).map_err(error)?,
                    &ready,
                )
            {
                return Err(PrivateFileError::Changed);
            }
            active(cancel)?;
            // SAFETY: both terminated names are relative to the retained, validated directory.
            if unsafe {
                libc::renameat(
                    self.parent.as_raw_fd(),
                    name.as_ptr(),
                    self.parent.as_raw_fd(),
                    self.name.as_ptr(),
                )
            } != 0
            {
                return Err(PrivateFileError::Replace);
            }
            // SAFETY: borrowed retained directory descriptor, no ownership transfer.
            if unsafe { libc::fsync(self.parent.as_raw_fd()) } != 0 {
                return Err(PrivateFileError::OutcomeUnconfirmed);
            }
            Ok(())
        })();
        // Remove only this worker's still-named temporary inode, never another writer's file.
        if let Ok(named) = stat_at(self.parent.as_raw_fd(), &name)
            && identity(&named) == identity(&created)
        {
            // SAFETY: checked exclusive temporary inode under retained directory.
            unsafe {
                libc::unlinkat(self.parent.as_raw_fd(), name.as_ptr(), 0);
            }
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt, symlink};
    struct Scratch(std::path::PathBuf);
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    #[test]
    fn safe_atomic_storage_and_rejection() {
        let root = Scratch(
            format!(
                "/private/tmp/asura-config-{:x}",
                u128::from_ne_bytes(random_id())
            )
            .into(),
        );
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&root.0)
            .unwrap();
        let runtime = RuntimeDirectory::scratch(&root.0, true).unwrap();
        let path = root.0.join(".asura/config.yaml");
        let cancel = AtomicBool::new(false);
        let read = || PrivateFileSnapshot::read(runtime.clone(), "config.yaml", &cancel);
        for name in [
            "",
            ".",
            "..",
            "../config.yaml",
            "nested/config.yaml",
            "nul\0name",
        ] {
            assert!(PrivateFileSnapshot::read(runtime.clone(), name, &cancel).is_err());
            assert!(crate::runtime::open_private_append(&root.0, name).is_err());
        }

        read()
            .unwrap()
            .replace(b"model: example\n", &cancel)
            .unwrap();
        assert_eq!(read().unwrap().bytes, b"model: example\n");
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let old = read().unwrap();
        std::fs::write(&path, b"model: changed\n").unwrap();
        assert!(old.replace(b"model: lost", &cancel).is_err());
        std::fs::hard_link(&path, root.0.join("hardlink")).unwrap();
        assert!(read().is_err());
        std::fs::remove_file(root.0.join("hardlink")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(read().is_err());
        std::fs::remove_file(&path).unwrap();
        symlink(root.0.join("missing"), &path).unwrap();
        assert!(read().is_err());
        std::fs::remove_file(&path).unwrap();
        let fifo = CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: terminated scratch path and fixed private FIFO creation mode.
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        let started = std::time::Instant::now();
        assert!(read().is_err());
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
        std::fs::remove_file(&path).unwrap();
        let absent = read().unwrap();
        absent.replace(b"model: temporary", &cancel).unwrap();
        // ES-02: the bound is 64 KiB. A file of exactly the bound reads;
        // one byte more is `TooLarge`.
        assert_eq!(LIMIT, 65_536);
        std::fs::write(&path, vec![b'x'; LIMIT]).unwrap();
        assert_eq!(read().unwrap().bytes.len(), LIMIT);
        std::fs::write(&path, vec![b'x'; LIMIT + 1]).unwrap();
        assert_eq!(read().err(), Some(PrivateFileError::TooLarge));
        std::fs::remove_file(&path).unwrap();
        // A new file appearing after the absent snapshot must not be overwritten.
        read()
            .unwrap()
            .replace(b"model: appeared", &cancel)
            .unwrap();
        assert!(absent.replace(b"model: lost", &cancel).is_err());
        std::fs::remove_file(&path).unwrap();
        let absent = read().unwrap();
        assert_eq!(
            absent.replace(&vec![b'x'; LIMIT + 1], &cancel),
            Err(PrivateFileError::TooLarge)
        );
        assert!(!path.exists());
        absent.replace(&vec![b'x'; LIMIT], &cancel).unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().len(), LIMIT as u64);
        std::fs::remove_file(&path).unwrap();
        let absent = read().unwrap();
        cancel.store(true, Ordering::Release);
        assert!(absent.replace(b"model: cancelled", &cancel).is_err());
        assert!(!path.exists());
    }
}
