//! Descriptor-held payload file primitives (J1, G14). Every operation runs on
//! a capture, page-read or recovery worker, never on the reactor. The only
//! caller is the storage payload store.
//!
//! Layout: `~/.asura/payloads/{staging,objects}/<32 hex>`, directories 0700,
//! files 0600, on a local APFS volume. The directories are inside `~/.asura`,
//! which every shell profile denies (SI3).
use crate::runtime::{open_at, stat_at, stat_fd, valid_stat, validate_acl};
use crate::{Error, RuntimeDirectory, checked};
use std::ffi::{CStr, CString};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

pub type PayloadId = [u8; 16];
/// Names per listing batch.
pub const LIST_BATCH: usize = 128;

/// A payload file operation's failure.
#[derive(Debug)]
pub enum PayloadError {
    /// `ENOSPC` or `EDQUOT`: the volume is full (R6).
    NoSpace,
    /// The file already exists (exclusive create or install).
    Exists,
    /// The file does not exist.
    Absent,
    /// A symbolic link, another owner, a wrong mode or type, or a foreign
    /// name: the payload store becomes unavailable.
    Unsafe,
    /// The volume is not local APFS.
    Unsupported,
    Io(io::Error),
}
impl PayloadError {
    fn from_io(error: io::Error) -> Self {
        match error.raw_os_error() {
            Some(libc::ENOSPC | libc::EDQUOT) => Self::NoSpace,
            Some(libc::EEXIST) => Self::Exists,
            Some(libc::ENOENT) => Self::Absent,
            Some(libc::ELOOP | libc::ENOTDIR) => Self::Unsafe,
            _ => Self::Io(error),
        }
    }
    fn last() -> Self {
        Self::from_io(io::Error::last_os_error())
    }
}
impl From<Error> for PayloadError {
    fn from(error: Error) -> Self {
        match error {
            Error::Absent => Self::Absent,
            Error::UnsafeRuntime => Self::Unsafe,
            Error::Unavailable => Self::Unsupported,
            Error::Io(error) => Self::from_io(error),
            _ => Self::Io(io::Error::other("payload runtime")),
        }
    }
}
type Result<T> = std::result::Result<T, PayloadError>;

/// The two payload directories.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PayloadDirectory {
    Staging,
    Objects,
}

/// One listed file: its ID and size. Validation already passed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PayloadEntry {
    pub id: PayloadId,
    pub bytes: u64,
}

fn name(id: PayloadId) -> CString {
    let text: String = id.iter().map(|b| format!("{b:02x}")).collect();
    CString::new(text).expect("hex has no NUL")
}
fn parse(name: &[u8]) -> Option<PayloadId> {
    if name.len() != 32
        || !name
            .iter()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(b))
    {
        return None;
    }
    let mut id = [0; 16];
    for (i, byte) in id.iter_mut().enumerate() {
        let text = std::str::from_utf8(&name[i * 2..i * 2 + 2]).ok()?;
        *byte = u8::from_str_radix(text, 16).ok()?;
    }
    (id != [0; 16]).then_some(id)
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
    valid_stat(&stat, uid, libc::S_IFDIR, Some(0o700)).map_err(|_| PayloadError::Unsafe)?;
    validate_acl(fd.as_raw_fd(), uid).map_err(|_| PayloadError::Unsafe)?;
    Ok(fd)
}

/// The held payload directories.
pub struct PayloadRoot {
    uid: libc::uid_t,
    root: OwnedFd,
    staging: OwnedFd,
    objects: OwnedFd,
    #[cfg(any(test, feature = "test-support"))]
    run: std::path::PathBuf,
}
impl PayloadRoot {
    /// Holds `~/.asura/payloads`, `staging/` and `objects/` (mode 0700) on a
    /// local APFS volume, creating them when `create` is set.
    pub fn open(runtime: &RuntimeDirectory, create: bool) -> Result<Self> {
        let (asura, uid) = runtime.authority_root()?;
        // SAFETY: initialized POD is a valid fstatfs output buffer.
        let mut fs: libc::statfs = unsafe { std::mem::zeroed() };
        // SAFETY: asura is held and fs is writable.
        checked(unsafe { libc::fstatfs(asura.as_raw_fd(), &mut fs) })?;
        // SAFETY: successful fstatfs supplies a terminated filesystem name.
        if unsafe { CStr::from_ptr(fs.f_fstypename.as_ptr()) } != c"apfs"
            || fs.f_flags & libc::MNT_LOCAL as u32 == 0
        {
            return Err(PayloadError::Unsupported);
        }
        let root = directory(&asura, c"payloads", create, uid)?;
        let staging = directory(&root, c"staging", create, uid)?;
        let objects = directory(&root, c"objects", create, uid)?;
        Ok(Self {
            uid,
            root,
            staging,
            objects,
            #[cfg(any(test, feature = "test-support"))]
            run: runtime.run_path().to_owned(),
        })
    }
    fn held(&self, directory: PayloadDirectory) -> &OwnedFd {
        match directory {
            PayloadDirectory::Staging => &self.staging,
            PayloadDirectory::Objects => &self.objects,
        }
    }
    /// Exclusive create of `staging/<id>`, mode 0600, append-only.
    pub fn create_staging(&self, id: PayloadId) -> Result<StagingFile> {
        #[cfg(any(test, feature = "test-support"))]
        if let Some(error) = gate::point(&self.run, gate::Point::Create) {
            return Err(error);
        }
        let file = name(id);
        // SAFETY: held directory, terminated name; the new descriptor is owned below.
        let raw = unsafe {
            libc::openat(
                self.staging.as_raw_fd(),
                file.as_ptr(),
                libc::O_CREAT
                    | libc::O_EXCL
                    | libc::O_WRONLY
                    | libc::O_APPEND
                    | libc::O_NOFOLLOW
                    | libc::O_CLOEXEC,
                0o600 as libc::c_int,
            )
        };
        if raw < 0 {
            return Err(PayloadError::last());
        }
        // SAFETY: a successful open returns a new descriptor owned exclusively here.
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        Ok(StagingFile {
            fd,
            bytes: 0,
            #[cfg(any(test, feature = "test-support"))]
            run: self.run.clone(),
        })
    }
    /// Renames `staging/<id>` to `objects/<id>` without replacing an existing
    /// object (`RENAME_EXCL`), then syncs `objects/`.
    pub fn install(&self, id: PayloadId) -> Result<()> {
        #[cfg(any(test, feature = "test-support"))]
        if let Some(error) = gate::point(&self.run, gate::Point::Install) {
            return Err(error);
        }
        let file = name(id);
        // SAFETY: both directories are held; names are terminated components.
        let result = unsafe {
            libc::renameatx_np(
                self.staging.as_raw_fd(),
                file.as_ptr(),
                self.objects.as_raw_fd(),
                file.as_ptr(),
                libc::RENAME_EXCL,
            )
        };
        if result != 0 {
            return Err(PayloadError::last());
        }
        // SAFETY: fsync borrows the held objects directory.
        if unsafe { libc::fsync(self.objects.as_raw_fd()) } != 0 {
            return Err(PayloadError::last());
        }
        Ok(())
    }
    fn open_file(&self, directory: PayloadDirectory, id: PayloadId) -> Result<PayloadFile> {
        let fd = open_at(
            self.held(directory).as_raw_fd(),
            &name(id),
            libc::O_RDONLY,
            0,
        )?;
        let stat = stat_fd(fd.as_raw_fd())?;
        valid_stat(&stat, self.uid, libc::S_IFREG, Some(0o600))
            .map_err(|_| PayloadError::Unsafe)?;
        Ok(PayloadFile {
            fd,
            bytes: stat.st_size as u64,
        })
    }
    /// Read-only open of an installed object; checks a regular file, owner and mode.
    pub fn open_object(&self, id: PayloadId) -> Result<PayloadFile> {
        self.open_file(PayloadDirectory::Objects, id)
    }
    /// Read-only open of a staging file; checks a regular file, owner and mode.
    pub fn open_staging(&self, id: PayloadId) -> Result<PayloadFile> {
        self.open_file(PayloadDirectory::Staging, id)
    }
    fn remove(&self, directory: PayloadDirectory, id: PayloadId) -> Result<()> {
        // SAFETY: held directory and a terminated name; unlinkat never follows.
        if unsafe { libc::unlinkat(self.held(directory).as_raw_fd(), name(id).as_ptr(), 0) } != 0 {
            return Err(PayloadError::last());
        }
        Ok(())
    }
    pub fn remove_object(&self, id: PayloadId) -> Result<()> {
        self.remove(PayloadDirectory::Objects, id)
    }
    pub fn remove_staging(&self, id: PayloadId) -> Result<()> {
        self.remove(PayloadDirectory::Staging, id)
    }
    /// Free bytes available to this user on the payload volume (`fstatfs`).
    pub fn free_bytes(&self) -> Result<u64> {
        #[cfg(any(test, feature = "test-support"))]
        if let Some(bytes) = gate::free_override(&self.run) {
            return Ok(bytes);
        }
        // SAFETY: initialized POD is a valid fstatfs output buffer.
        let mut fs: libc::statfs = unsafe { std::mem::zeroed() };
        // SAFETY: the root is held and fs is writable.
        if unsafe { libc::fstatfs(self.root.as_raw_fd(), &mut fs) } != 0 {
            return Err(PayloadError::last());
        }
        Ok(fs.f_bavail.saturating_mul(u64::from(fs.f_bsize)))
    }
    /// One batch of at most [`LIST_BATCH`] files after `after`, in increasing
    /// ID order. Each listed file is validated; a symbolic link, another
    /// owner, a non-regular file or a foreign name is `Unsafe`.
    pub fn list(
        &self,
        directory: PayloadDirectory,
        after: Option<PayloadId>,
    ) -> Result<Vec<PayloadEntry>> {
        // A fresh open file description: its directory offset is independent.
        let fd = open_at(
            self.held(directory).as_raw_fd(),
            c".",
            libc::O_RDONLY | libc::O_DIRECTORY,
            0,
        )?;
        // SAFETY: fdopendir takes ownership of the descriptor on success.
        let dir = unsafe { libc::fdopendir(fd.as_raw_fd()) };
        if dir.is_null() {
            return Err(PayloadError::last());
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
        let mut names: std::collections::BTreeSet<PayloadId> = std::collections::BTreeSet::new();
        loop {
            // SAFETY: the stream is open; errno distinguishes end from failure.
            unsafe { *libc::__error() = 0 };
            let entry = unsafe { libc::readdir(dir.0) };
            if entry.is_null() {
                // SAFETY: errno read after readdir on this thread.
                if unsafe { *libc::__error() } != 0 {
                    return Err(PayloadError::last());
                }
                break;
            }
            // SAFETY: readdir returns a valid entry with a terminated name.
            let raw = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
            if raw == b"." || raw == b".." {
                continue;
            }
            let id = parse(raw).ok_or(PayloadError::Unsafe)?;
            if after.is_some_and(|after| id <= after) {
                continue;
            }
            names.insert(id);
            if names.len() > LIST_BATCH {
                names.pop_last();
            }
        }
        let mut result = Vec::with_capacity(names.len());
        for id in names {
            let stat = stat_at(self.held(directory).as_raw_fd(), &name(id))?;
            valid_stat(&stat, self.uid, libc::S_IFREG, Some(0o600))
                .map_err(|_| PayloadError::Unsafe)?;
            result.push(PayloadEntry {
                id,
                bytes: stat.st_size as u64,
            });
        }
        Ok(result)
    }
}

/// An open staging file. Appends only; dropping closes it.
pub struct StagingFile {
    fd: OwnedFd,
    bytes: u64,
    #[cfg(any(test, feature = "test-support"))]
    run: std::path::PathBuf,
}
impl StagingFile {
    /// Writes all bytes or returns an error. `ENOSPC` maps to `NoSpace`.
    /// After an error the file holds a prefix of the bytes; `len` counts
    /// only bytes the kernel accepted.
    pub fn append(&mut self, mut bytes: &[u8]) -> Result<()> {
        #[cfg(any(test, feature = "test-support"))]
        if let Some(error) = gate::point(&self.run, gate::Point::Write) {
            return Err(error);
        }
        while !bytes.is_empty() {
            // SAFETY: owned descriptor and a valid bounded buffer.
            let n = unsafe { libc::write(self.fd.as_raw_fd(), bytes.as_ptr().cast(), bytes.len()) };
            if n < 0 {
                let error = io::Error::last_os_error();
                if error.raw_os_error() == Some(libc::EINTR) {
                    continue;
                }
                return Err(PayloadError::from_io(error));
            }
            if n == 0 {
                return Err(PayloadError::NoSpace);
            }
            self.bytes += n as u64;
            bytes = &bytes[n as usize..];
        }
        Ok(())
    }
    /// `fsync` of the file (not `F_FULLFSYNC`; see the J1 proof limits).
    pub fn sync(&self) -> Result<()> {
        #[cfg(any(test, feature = "test-support"))]
        if let Some(error) = gate::point(&self.run, gate::Point::Sync) {
            return Err(error);
        }
        // SAFETY: fsync borrows the owned descriptor.
        if unsafe { libc::fsync(self.fd.as_raw_fd()) } != 0 {
            return Err(PayloadError::last());
        }
        Ok(())
    }
    /// Bytes the kernel accepted.
    pub fn len(&self) -> u64 {
        self.bytes
    }
    pub fn is_empty(&self) -> bool {
        self.bytes == 0
    }
}

/// A read-only payload file.
pub struct PayloadFile {
    fd: OwnedFd,
    bytes: u64,
}
impl PayloadFile {
    /// One `pread` at `offset`; returns the bytes read (0 at end).
    pub fn read_at(&self, offset: u64, buffer: &mut [u8]) -> Result<usize> {
        loop {
            // SAFETY: owned descriptor and a valid writable buffer.
            let n = unsafe {
                libc::pread(
                    self.fd.as_raw_fd(),
                    buffer.as_mut_ptr().cast(),
                    buffer.len(),
                    offset as libc::off_t,
                )
            };
            if n >= 0 {
                return Ok(n as usize);
            }
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::EINTR) {
                return Err(PayloadError::from_io(error));
            }
        }
    }
    /// The size at open.
    pub fn len(&self) -> u64 {
        self.bytes
    }
    pub fn is_empty(&self) -> bool {
        self.bytes == 0
    }
    /// The current size (`fstat`).
    pub fn current_len(&self) -> Result<u64> {
        Ok(stat_fd(self.fd.as_raw_fd())?.st_size as u64)
    }
}

/// Payload fault points for tests (J1T-09, J1T-20, J1T-22). Without the
/// `test-support` feature nothing is compiled.
#[cfg(any(test, feature = "test-support"))]
#[cfg_attr(not(feature = "test-support"), allow(dead_code))]
pub mod gate {
    use super::PayloadError;
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub enum Point {
        Create,
        Write,
        Sync,
        Install,
    }
    /// An injected failure for the next operation at a point.
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub enum Fault {
        /// `ENOSPC`.
        NoSpace,
        /// `EIO`.
        Io,
    }
    struct Entry {
        run: PathBuf,
        point: Point,
        fault: Fault,
        remaining: u32,
    }
    static FAULTS: Mutex<Vec<Entry>> = Mutex::new(Vec::new());
    static FREE: Mutex<Vec<(PathBuf, u64)>> = Mutex::new(Vec::new());

    fn run_path(home: &Path) -> PathBuf {
        home.join(".asura/run")
    }
    /// Fails the next `count` operations at `point` for the scratch `home`.
    pub fn inject(home: &Path, point: Point, fault: Fault, count: u32) {
        FAULTS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(Entry {
                run: run_path(home),
                point,
                fault,
                remaining: count,
            });
    }
    /// Reports `bytes` as the volume's free space for `home` (None clears).
    pub fn set_free_bytes(home: &Path, bytes: Option<u64>) {
        let mut free = FREE.lock().unwrap_or_else(|e| e.into_inner());
        let run = run_path(home);
        free.retain(|(path, _)| *path != run);
        if let Some(bytes) = bytes {
            free.push((run, bytes));
        }
    }
    /// Child-process faults: while `HOME/payload-fault-<point>` exists, every
    /// operation at that point fails with `EIO`, or with `ENOSPC` when the
    /// file holds `enospc`.
    fn file_fault(run: &Path, point: Point) -> Option<PayloadError> {
        let home = run.parent()?.parent()?;
        let label = match point {
            Point::Create => "create",
            Point::Write => "write",
            Point::Sync => "sync",
            Point::Install => "install",
        };
        let content = std::fs::read(home.join(format!("payload-fault-{label}"))).ok()?;
        Some(if content.starts_with(b"enospc") {
            PayloadError::NoSpace
        } else {
            PayloadError::Io(std::io::Error::from_raw_os_error(libc::EIO))
        })
    }
    pub(super) fn point(run: &Path, point: Point) -> Option<PayloadError> {
        let mut faults = FAULTS.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(entry) = faults
            .iter_mut()
            .find(|e| e.run == run && e.point == point && e.remaining > 0)
        {
            entry.remaining -= 1;
            return Some(match entry.fault {
                Fault::NoSpace => PayloadError::NoSpace,
                Fault::Io => PayloadError::Io(std::io::Error::from_raw_os_error(libc::EIO)),
            });
        }
        drop(faults);
        file_fault(run, point)
    }
    pub(super) fn free_override(run: &Path) -> Option<u64> {
        if let Some(bytes) = FREE
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .find(|(path, _)| path == run)
            .map(|(_, bytes)| *bytes)
        {
            return Some(bytes);
        }
        let home = run.parent()?.parent()?;
        std::fs::read_to_string(home.join("payload-free-bytes"))
            .ok()?
            .trim()
            .parse()
            .ok()
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
            let path = std::path::PathBuf::from("/private/tmp").join(format!("as-pl-{name}"));
            fs::create_dir(&path).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
            Self(fs::canonicalize(path).unwrap())
        }
    }
    impl Drop for Home {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn root(home: &Home) -> PayloadRoot {
        let runtime = RuntimeDirectory::scratch(&home.0, true).unwrap();
        PayloadRoot::open(&runtime, true).unwrap()
    }
    #[test]
    fn staging_append_sync_install_read_and_remove() {
        let home = Home::new();
        let root = root(&home);
        let id = [7; 16];
        let mut file = root.create_staging(id).unwrap();
        assert!(matches!(root.create_staging(id), Err(PayloadError::Exists)));
        file.append(b"hello ").unwrap();
        file.append(b"world").unwrap();
        file.sync().unwrap();
        assert_eq!(file.len(), 11);
        drop(file);
        let staged = root.open_staging(id).unwrap();
        let mut buffer = [0u8; 32];
        assert_eq!(staged.read_at(6, &mut buffer).unwrap(), 5);
        assert_eq!(&buffer[..5], b"world");
        root.install(id).unwrap();
        assert!(matches!(root.open_staging(id), Err(PayloadError::Absent)));
        let object = root.open_object(id).unwrap();
        assert_eq!(object.len(), 11);
        // RENAME_EXCL: an existing object is never replaced.
        root.create_staging(id).unwrap().append(b"x").unwrap();
        assert!(matches!(root.install(id), Err(PayloadError::Exists)));
        root.remove_staging(id).unwrap();
        root.remove_object(id).unwrap();
        assert!(matches!(root.remove_object(id), Err(PayloadError::Absent)));
        assert!(root.free_bytes().unwrap() > 0);
        let mode = fs::metadata(home.0.join(".asura/payloads/staging"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700);
    }
    #[test]
    fn listing_is_batched_ordered_and_rejects_unsafe_entries() {
        let home = Home::new();
        let root = root(&home);
        let mut ids: Vec<PayloadId> = (1..=200u16)
            .map(|n| {
                let mut id = [0; 16];
                id[14..].copy_from_slice(&n.to_be_bytes());
                id
            })
            .collect();
        for id in &ids {
            root.create_staging(*id).unwrap().append(b"ab").unwrap();
        }
        ids.sort();
        let first = root.list(PayloadDirectory::Staging, None).unwrap();
        assert_eq!(first.len(), LIST_BATCH);
        assert!(
            first
                .iter()
                .zip(&ids)
                .all(|(e, id)| e.id == *id && e.bytes == 2)
        );
        let second = root
            .list(PayloadDirectory::Staging, Some(first.last().unwrap().id))
            .unwrap();
        assert_eq!(second.len(), 200 - LIST_BATCH);
        assert!(
            root.list(PayloadDirectory::Objects, None)
                .unwrap()
                .is_empty()
        );
        // Unsafe entries in a batch: a symbolic link, a foreign name, a wrong mode.
        let objects = home.0.join(".asura/payloads/objects");
        std::os::unix::fs::symlink("/etc/hosts", objects.join("f".repeat(32))).unwrap();
        assert!(matches!(
            root.list(PayloadDirectory::Objects, None),
            Err(PayloadError::Unsafe)
        ));
        assert!(matches!(
            root.open_object(parse(&b"f".repeat(32)).unwrap()),
            Err(PayloadError::Unsafe)
        ));
        fs::remove_file(objects.join("f".repeat(32))).unwrap();
        fs::write(objects.join("foreign"), b"x").unwrap();
        assert!(matches!(
            root.list(PayloadDirectory::Objects, None),
            Err(PayloadError::Unsafe)
        ));
        fs::remove_file(objects.join("foreign")).unwrap();
        let staging = home.0.join(".asura/payloads/staging");
        let wide = staging.join("e".repeat(32));
        fs::write(&wide, b"x").unwrap();
        fs::set_permissions(&wide, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(matches!(
            root.list(PayloadDirectory::Staging, Some(ids[199])),
            Err(PayloadError::Unsafe)
        ));
        assert!(matches!(
            root.open_staging(parse(&b"e".repeat(32)).unwrap()),
            Err(PayloadError::Unsafe)
        ));
    }
    #[test]
    fn injected_faults_map_to_no_space_and_io() {
        let home = Home::new();
        let root = root(&home);
        gate::inject(&home.0, gate::Point::Write, gate::Fault::NoSpace, 1);
        let mut file = root.create_staging([3; 16]).unwrap();
        assert!(matches!(file.append(b"x"), Err(PayloadError::NoSpace)));
        file.append(b"y").unwrap();
        gate::inject(&home.0, gate::Point::Sync, gate::Fault::Io, 1);
        assert!(matches!(file.sync(), Err(PayloadError::Io(_))));
        gate::set_free_bytes(&home.0, Some(5));
        assert_eq!(root.free_bytes().unwrap(), 5);
        gate::set_free_bytes(&home.0, None);
    }
}
