//! Descriptor-held policy text files (security-policy.md, PL3 policy text
//! store). Every operation runs on the policy admission worker, never on
//! the reactor. The only caller is the storage policy text store, which
//! owns digest naming and checks.
//!
//! Layout: `~/.asura/policy/admitted/<64 hex>`, directories 0700, files
//! 0600, opened through the validated Asura home descriptor with no
//! symbolic link followed. The directories are inside `~/.asura`, which
//! every exec_cmd profile denies (SI3).
use crate::runtime::{open_at, stat_at, stat_fd, valid_stat, validate_acl};
use crate::{Error, RuntimeDirectory, checked};
use std::ffi::{CStr, CString};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

/// A policy text file operation's failure.
#[derive(Debug)]
pub enum PolicyTextError {
    /// The file does not exist.
    Missing,
    /// A symbolic link, another owner, a wrong mode or type, or a foreign
    /// name: the text store is unavailable.
    Unsafe,
    /// `ENOSPC` or `EDQUOT`: the volume is full.
    NoSpace,
    /// The file is longer than the caller's bound.
    TooLarge,
    Io(io::Error),
}
impl PolicyTextError {
    fn from_io(error: io::Error) -> Self {
        match error.raw_os_error() {
            Some(libc::ENOSPC | libc::EDQUOT) => Self::NoSpace,
            Some(libc::ENOENT) => Self::Missing,
            Some(libc::ELOOP | libc::ENOTDIR) => Self::Unsafe,
            _ => Self::Io(error),
        }
    }
    fn last() -> Self {
        Self::from_io(io::Error::last_os_error())
    }
}
impl From<Error> for PolicyTextError {
    fn from(error: Error) -> Self {
        match error {
            Error::Absent => Self::Missing,
            Error::UnsafeRuntime => Self::Unsafe,
            Error::Io(error) => Self::from_io(error),
            _ => Self::Io(io::Error::other("policy text runtime")),
        }
    }
}
type Result<T> = std::result::Result<T, PolicyTextError>;

/// A text name: 64 lowercase hex digits.
fn valid_name(name: &[u8]) -> bool {
    name.len() == 64 && name.iter().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}
fn c_name(name: &str) -> Result<CString> {
    if !valid_name(name.as_bytes()) {
        return Err(PolicyTextError::Unsafe);
    }
    CString::new(name).map_err(|_| PolicyTextError::Unsafe)
}

fn directory(parent: &OwnedFd, name: &CStr, create: bool, uid: libc::uid_t) -> Result<OwnedFd> {
    let open = || {
        open_at(
            parent.as_raw_fd(),
            name,
            libc::O_RDONLY | libc::O_DIRECTORY,
            0,
        )
    };
    let fd = match open() {
        Ok(fd) => fd,
        Err(Error::Absent) if create => {
            // SAFETY: parent is held and name is a fixed terminated component.
            match checked(unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), 0o700) }) {
                Ok(()) | Err(Error::Io(_)) => {}
                Err(error) => return Err(error.into()),
            }
            // SAFETY: fsync borrows the held parent descriptor.
            checked(unsafe { libc::fsync(parent.as_raw_fd()) })?;
            open()?
        }
        Err(error) => return Err(error.into()),
    };
    let stat = stat_fd(fd.as_raw_fd())?;
    valid_stat(&stat, uid, libc::S_IFDIR, Some(0o700)).map_err(|_| PolicyTextError::Unsafe)?;
    validate_acl(fd.as_raw_fd(), uid).map_err(|_| PolicyTextError::Unsafe)?;
    Ok(fd)
}

/// The held `~/.asura/policy/admitted` directory.
pub struct PolicyTextDirectory {
    uid: libc::uid_t,
    admitted: OwnedFd,
}
impl PolicyTextDirectory {
    /// Holds `~/.asura/policy/admitted` (mode 0700), creating `policy/`
    /// and `admitted/` when `create` is set.
    pub fn open(runtime: &RuntimeDirectory, create: bool) -> Result<Self> {
        let (asura, uid) = runtime.authority_root()?;
        let policy = directory(&asura, c"policy", create, uid)?;
        let admitted = directory(&policy, c"admitted", create, uid)?;
        Ok(Self { uid, admitted })
    }
    /// Writes `bytes` as `name` (64 hex): an exclusive temporary name, the
    /// bytes, `fsync`, `rename` without replacing, `fsync` of the
    /// directory. An existing regular file of that name is kept; the
    /// caller checks its digest.
    pub fn write(&self, name: &str, bytes: &[u8]) -> Result<()> {
        let target = c_name(name)?;
        match self.stat_regular(&target) {
            Ok(_) => return Ok(()),
            Err(PolicyTextError::Missing) => {}
            Err(error) => return Err(error),
        }
        let suffix: String = crate::random_id()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let temporary =
            CString::new(format!(".tmp-{suffix}")).map_err(|_| PolicyTextError::Unsafe)?;
        // SAFETY: held directory and a terminated name; the new descriptor is owned below.
        let raw = unsafe {
            libc::openat(
                self.admitted.as_raw_fd(),
                temporary.as_ptr(),
                libc::O_CREAT | libc::O_EXCL | libc::O_WRONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o600 as libc::c_int,
            )
        };
        if raw < 0 {
            return Err(PolicyTextError::last());
        }
        // SAFETY: a successful open returns a new descriptor owned exclusively here.
        let mut file = std::fs::File::from(unsafe { OwnedFd::from_raw_fd(raw) });
        let written = file
            .write_all(bytes)
            .and_then(|()| file.sync_all())
            .map_err(PolicyTextError::from_io);
        drop(file);
        let renamed = written.and_then(|()| {
            // SAFETY: both names are terminated components of the held directory.
            let result = unsafe {
                libc::renameatx_np(
                    self.admitted.as_raw_fd(),
                    temporary.as_ptr(),
                    self.admitted.as_raw_fd(),
                    target.as_ptr(),
                    libc::RENAME_EXCL,
                )
            };
            if result != 0 {
                return Err(PolicyTextError::last());
            }
            Ok(())
        });
        match renamed {
            Ok(()) => {}
            Err(error) => {
                // SAFETY: remove only the temporary name in the held directory.
                unsafe { libc::unlinkat(self.admitted.as_raw_fd(), temporary.as_ptr(), 0) };
                // A concurrent writer installed the same name: keep it.
                if matches!(&error, PolicyTextError::Io(e) if e.raw_os_error() == Some(libc::EEXIST))
                {
                    return self.stat_regular(&target).map(|_| ());
                }
                return Err(error);
            }
        }
        // SAFETY: fsync borrows the held directory.
        if unsafe { libc::fsync(self.admitted.as_raw_fd()) } != 0 {
            return Err(PolicyTextError::last());
        }
        Ok(())
    }
    fn stat_regular(&self, name: &CStr) -> Result<libc::stat> {
        let stat = stat_at(self.admitted.as_raw_fd(), name)?;
        valid_stat(&stat, self.uid, libc::S_IFREG, Some(0o600))
            .map_err(|_| PolicyTextError::Unsafe)?;
        Ok(stat)
    }
    /// Reads `name` (64 hex) when it holds at most `max` bytes.
    pub fn read(&self, name: &str, max: usize) -> Result<Vec<u8>> {
        let name = c_name(name)?;
        let fd = open_at(self.admitted.as_raw_fd(), &name, libc::O_RDONLY, 0)?;
        let stat = stat_fd(fd.as_raw_fd())?;
        valid_stat(&stat, self.uid, libc::S_IFREG, Some(0o600))
            .map_err(|_| PolicyTextError::Unsafe)?;
        if stat.st_size as u64 > max as u64 {
            return Err(PolicyTextError::TooLarge);
        }
        let mut bytes = Vec::new();
        std::fs::File::from(fd)
            .take(max as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(PolicyTextError::from_io)?;
        if bytes.len() > max {
            return Err(PolicyTextError::TooLarge);
        }
        Ok(bytes)
    }
    /// The number of stored texts. A foreign name other than a leftover
    /// temporary file is `Unsafe`.
    pub fn count(&self) -> Result<usize> {
        let fd = open_at(
            self.admitted.as_raw_fd(),
            c".",
            libc::O_RDONLY | libc::O_DIRECTORY,
            0,
        )?;
        // SAFETY: fdopendir takes ownership of the descriptor on success.
        let dir = unsafe { libc::fdopendir(fd.as_raw_fd()) };
        if dir.is_null() {
            return Err(PolicyTextError::last());
        }
        std::mem::forget(fd);
        struct Dir(*mut libc::DIR);
        impl Drop for Dir {
            fn drop(&mut self) {
                // SAFETY: the stream is open and owned here; closedir closes its descriptor.
                unsafe { libc::closedir(self.0) };
            }
        }
        let dir = Dir(dir);
        let mut count = 0;
        loop {
            // SAFETY: the stream is open; errno distinguishes end from failure.
            unsafe { *libc::__error() = 0 };
            let entry = unsafe { libc::readdir(dir.0) };
            if entry.is_null() {
                // SAFETY: errno read after readdir on this thread.
                if unsafe { *libc::__error() } != 0 {
                    return Err(PolicyTextError::last());
                }
                return Ok(count);
            }
            // SAFETY: readdir returns a valid entry with a terminated name.
            let raw = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
            if raw == b"." || raw == b".." || raw.starts_with(b".tmp-") {
                continue;
            }
            if !valid_name(raw) {
                return Err(PolicyTextError::Unsafe);
            }
            count += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    struct Home(std::path::PathBuf);
    impl Home {
        fn new() -> Self {
            let name: String = crate::random_id()
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect();
            let path = std::path::PathBuf::from("/private/tmp").join(format!("as-pt-{name}"));
            fs::create_dir(&path).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
            Self(fs::canonicalize(path).unwrap())
        }
        fn admitted(&self) -> std::path::PathBuf {
            self.0.join(".asura/policy/admitted")
        }
    }
    impl Drop for Home {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    #[test]
    fn write_read_reuse_and_count() {
        let home = Home::new();
        let runtime = RuntimeDirectory::scratch(&home.0, true).unwrap();
        assert!(matches!(
            PolicyTextDirectory::open(&runtime, false),
            Err(PolicyTextError::Missing)
        ));
        let texts = PolicyTextDirectory::open(&runtime, true).unwrap();
        assert_eq!(texts.count().unwrap(), 0);
        texts.write(A, b"permit").unwrap();
        // An existing name is kept, never replaced.
        texts.write(A, b"other").unwrap();
        assert_eq!(texts.read(A, 64).unwrap(), b"permit");
        assert!(matches!(texts.read(A, 3), Err(PolicyTextError::TooLarge)));
        assert!(matches!(texts.read(B, 64), Err(PolicyTextError::Missing)));
        assert!(matches!(
            texts.write("not-hex", b"x"),
            Err(PolicyTextError::Unsafe)
        ));
        assert_eq!(texts.count().unwrap(), 1);
        for path in [home.0.join(".asura/policy"), home.admitted()] {
            assert_eq!(
                fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
        let mode = fs::metadata(home.admitted().join(A))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        // No temporary name is left behind.
        assert_eq!(fs::read_dir(home.admitted()).unwrap().count(), 1);
        // Reopening finds the same text.
        let again = PolicyTextDirectory::open(&runtime, false).unwrap();
        assert_eq!(again.read(A, 64).unwrap(), b"permit");
    }

    #[test]
    fn symlinks_and_foreign_names_are_refused() {
        let home = Home::new();
        let runtime = RuntimeDirectory::scratch(&home.0, true).unwrap();
        let texts = PolicyTextDirectory::open(&runtime, true).unwrap();
        std::os::unix::fs::symlink("/etc/hosts", home.admitted().join(B)).unwrap();
        assert!(matches!(
            texts.read(B, 1 << 20),
            Err(PolicyTextError::Unsafe)
        ));
        assert!(matches!(texts.write(B, b"x"), Err(PolicyTextError::Unsafe)));
        fs::remove_file(home.admitted().join(B)).unwrap();
        fs::write(home.admitted().join("stray"), b"x").unwrap();
        assert!(matches!(texts.count(), Err(PolicyTextError::Unsafe)));
        fs::remove_file(home.admitted().join("stray")).unwrap();
        // A symbolic link in place of the directory is refused.
        fs::remove_dir(home.admitted()).unwrap();
        std::os::unix::fs::symlink("/private/tmp", home.admitted()).unwrap();
        assert!(matches!(
            PolicyTextDirectory::open(&runtime, true),
            Err(PolicyTextError::Unsafe)
        ));
    }

    #[test]
    fn a_wrong_mode_is_refused() {
        let home = Home::new();
        let runtime = RuntimeDirectory::scratch(&home.0, true).unwrap();
        let texts = PolicyTextDirectory::open(&runtime, true).unwrap();
        texts.write(A, b"permit").unwrap();
        fs::set_permissions(home.admitted().join(A), fs::Permissions::from_mode(0o644)).unwrap();
        assert!(matches!(texts.read(A, 64), Err(PolicyTextError::Unsafe)));
    }
}
