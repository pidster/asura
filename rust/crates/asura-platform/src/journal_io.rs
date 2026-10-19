//! Descriptor-held ordinary-file journal IO. Call only on the storage worker.
use crate::runtime::{identity, open_at, stat_at, stat_fd, valid_stat, validate_acl};
use crate::{Error, Result, RuntimeDirectory, checked};
use std::ffi::{CStr, CString};
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::fs::FileExt;
use std::sync::atomic::{AtomicBool, Ordering};

/// A segment file holds at most 8 MiB (segmented journal).
pub const SEGMENT_BYTES: u64 = 8 * 1024 * 1024;
/// Census bound: 131,072 segment files (1 TiB / 8 MiB) plus 256 derived and lock entries.
pub const MAX_CENSUS: usize = 131_328;
const CENSUS_BATCH: usize = 128;
/// Open descriptors kept by one journal directory.
const OPEN_DESCRIPTORS: usize = 12;

/// The only names the control directory may hold. Any other name is
/// `unsupported_layout`; `slot-0.log` included.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum JournalName {
    /// `seg-H.log`: format-1 frames from sequence H. Authority, never deleted.
    Segment(u64),
    /// `ckpt-H.chk`: checkpoint at sequence H.
    Checkpoint(u64),
    /// `run-F-L.idx`: index run covering frames F to L.
    Run(u64, u64),
    /// `ckpt-H.tmp`
    CheckpointTmp(u64),
    /// `run-F-L.tmp`
    RunTmp(u64, u64),
    /// `lock`: holds the directory-wide writer `flock`.
    Lock,
}
fn hex16(text: &str) -> Option<u64> {
    (text.len() == 16
        && text
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)))
    .then(|| u64::from_str_radix(text, 16).ok())
    .flatten()
}
impl JournalName {
    pub fn parse(name: &[u8]) -> Option<Self> {
        let name = std::str::from_utf8(name).ok()?;
        if name == "lock" {
            return Some(Self::Lock);
        }
        if let Some(rest) = name.strip_prefix("seg-") {
            return Some(Self::Segment(hex16(rest.strip_suffix(".log")?)?));
        }
        if let Some(rest) = name.strip_prefix("ckpt-") {
            if let Some(h) = rest.strip_suffix(".chk") {
                return Some(Self::Checkpoint(hex16(h)?));
            }
            return Some(Self::CheckpointTmp(hex16(rest.strip_suffix(".tmp")?)?));
        }
        let rest = name.strip_prefix("run-")?;
        let (pair, tmp) = if let Some(pair) = rest.strip_suffix(".idx") {
            (pair, false)
        } else {
            (rest.strip_suffix(".tmp")?, true)
        };
        let (first, last) = pair.split_once('-')?;
        let (first, last) = (hex16(first)?, hex16(last)?);
        Some(if tmp {
            Self::RunTmp(first, last)
        } else {
            Self::Run(first, last)
        })
    }
    pub fn text(&self) -> String {
        match self {
            Self::Segment(h) => format!("seg-{h:016x}.log"),
            Self::Checkpoint(h) => format!("ckpt-{h:016x}.chk"),
            Self::Run(f, l) => format!("run-{f:016x}-{l:016x}.idx"),
            Self::CheckpointTmp(h) => format!("ckpt-{h:016x}.tmp"),
            Self::RunTmp(f, l) => format!("run-{f:016x}-{l:016x}.tmp"),
            Self::Lock => "lock".into(),
        }
    }
    fn c_name(&self) -> CString {
        CString::new(self.text()).expect("fixed journal names hold no NUL")
    }
    pub fn is_tmp(&self) -> bool {
        matches!(self, Self::CheckpointTmp(_) | Self::RunTmp(..))
    }
    /// Derived files: checkpoints, runs and their tmp files.
    pub fn is_derived(&self) -> bool {
        !matches!(self, Self::Segment(_) | Self::Lock)
    }
}

/// Journal directory failures. `NoSpace` reports whether the file size is
/// unchanged after `ENOSPC` (no byte of the write is in the file).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JournalError {
    /// Unknown or `slot-0.log` names in the control directory.
    UnsupportedLayout,
    /// Wrong owner, mode, link count, ACL, file type or a changed identity.
    Unsafe,
    /// Another writer holds the `lock` file.
    OwnerBusy,
    NoSpace {
        unchanged: bool,
    },
    /// A held or requested file is absent.
    Absent,
    /// Census over its bound, a bad request or a non-APFS volume.
    Limit,
    Cancelled,
    Io,
}
impl std::fmt::Display for JournalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for JournalError {}
fn map_error(error: Error) -> JournalError {
    match error {
        Error::UnsafeRuntime => JournalError::Unsafe,
        Error::OwnerBusy => JournalError::OwnerBusy,
        Error::Absent => JournalError::Absent,
        Error::Io(e) if e.raw_os_error() == Some(libc::ENOSPC) => {
            JournalError::NoSpace { unchanged: false }
        }
        Error::Io(e) if e.kind() == std::io::ErrorKind::PermissionDenied => JournalError::Unsafe,
        _ => JournalError::Io,
    }
}
type JResult<T> = std::result::Result<T, JournalError>;

/// One entry of the control-directory census.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct JournalEntry {
    pub name: JournalName,
    pub bytes: u64,
}

fn directory(parent: &OwnedFd, name: &CStr, create: bool) -> Result<OwnedFd> {
    match open_at(
        parent.as_raw_fd(),
        name,
        libc::O_RDONLY | libc::O_DIRECTORY,
        0,
    ) {
        Ok(fd) => Ok(fd),
        Err(Error::Absent) if create => {
            // SAFETY: parent is held and name is a fixed terminated component.
            checked(unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), 0o700) })?;
            // SAFETY: fsync borrows the held parent descriptor.
            checked(unsafe { libc::fsync(parent.as_raw_fd()) })?;
            open_at(
                parent.as_raw_fd(),
                name,
                libc::O_RDONLY | libc::O_DIRECTORY,
                0,
            )
        }
        Err(error) => Err(error),
    }
}
struct Node {
    fd: OwnedFd,
    name: CString,
    before: libc::stat,
}
struct Held {
    name: JournalName,
    file: std::fs::File,
    identity: crate::runtime::Identity,
    writable: bool,
}
struct DirectoryStream(*mut libc::DIR);
impl Drop for DirectoryStream {
    fn drop(&mut self) {
        // SAFETY: this wrapper uniquely owns the stream and its descriptor.
        unsafe {
            libc::closedir(self.0);
        }
    }
}

/// The descriptor-held `state/control` directory of the segmented journal.
/// Every file is opened relative to it with `O_NOFOLLOW`; names come only
/// from [`JournalName`]. Call only on the journal worker (writer) or the
/// inspection worker (read-only); every method may block.
pub struct JournalDirectory {
    runtime: RuntimeDirectory,
    uid: libc::uid_t,
    nodes: Vec<Node>,
    _lock: Option<OwnedFd>,
    writable: bool,
    held: Vec<Held>,
    cancel: std::sync::Arc<AtomicBool>,
}
impl JournalDirectory {
    /// Opens the control directory for the single writer and takes the
    /// `lock` file's exclusive `flock` (`OwnerBusy` when held elsewhere).
    /// `create` makes `state/control` and `lock` when absent.
    pub fn open(
        runtime: RuntimeDirectory,
        create: bool,
        cancel: std::sync::Arc<AtomicBool>,
    ) -> JResult<Self> {
        let mut directory = Self::open_nodes(runtime, create, cancel)?;
        let control = directory.control();
        // The lock file is not authority: it is created when absent.
        let lock = match open_at(control, c"lock", libc::O_RDWR, 0) {
            Ok(fd) => fd,
            Err(Error::Absent) => {
                let fd = open_at(
                    control,
                    c"lock",
                    libc::O_RDWR | libc::O_CREAT | libc::O_EXCL,
                    0o600,
                )
                .map_err(map_error)?;
                // SAFETY: control is a retained directory descriptor.
                checked(unsafe { libc::fsync(control) }).map_err(map_error)?;
                fd
            }
            Err(error) => return Err(map_error(error)),
        };
        // SAFETY: nonblocking exclusive lock on our held regular file.
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err(JournalError::OwnerBusy);
        }
        let stat = stat_fd(lock.as_raw_fd()).map_err(map_error)?;
        valid_stat(&stat, directory.uid, libc::S_IFREG, Some(0o600)).map_err(map_error)?;
        validate_acl(lock.as_raw_fd(), directory.uid).map_err(map_error)?;
        directory._lock = Some(lock);
        directory.writable = true;
        directory.validate()?;
        Ok(directory)
    }
    /// Opens the control directory read-only, without the lock; it never writes.
    pub fn open_read_only(
        runtime: RuntimeDirectory,
        cancel: std::sync::Arc<AtomicBool>,
    ) -> JResult<Self> {
        Self::open_nodes(runtime, false, cancel)
    }
    fn open_nodes(
        runtime: RuntimeDirectory,
        create: bool,
        cancel: std::sync::Arc<AtomicBool>,
    ) -> JResult<Self> {
        let (root, uid) = runtime.authority_root().map_err(map_error)?;
        // SAFETY: initialized POD is a valid fstatfs output buffer.
        let mut fs: libc::statfs = unsafe { std::mem::zeroed() };
        // SAFETY: root is held and fs is writable.
        checked(unsafe { libc::fstatfs(root.as_raw_fd(), &mut fs) }).map_err(map_error)?;
        // SAFETY: successful fstatfs supplies a terminated filesystem name.
        if unsafe { CStr::from_ptr(fs.f_fstypename.as_ptr()) } != c"apfs"
            || fs.f_flags & libc::MNT_LOCAL as u32 == 0
        {
            return Err(JournalError::Limit);
        }
        let mut nodes = vec![Node {
            before: stat_fd(root.as_raw_fd()).map_err(map_error)?,
            fd: root,
            name: c".asura".to_owned(),
        }];
        for name in [c"state", c"control"] {
            let fd = directory(&nodes.last().expect("root").fd, name, create).map_err(map_error)?;
            let before = stat_fd(fd.as_raw_fd()).map_err(map_error)?;
            valid_stat(&before, uid, libc::S_IFDIR, Some(0o700)).map_err(map_error)?;
            validate_acl(fd.as_raw_fd(), uid).map_err(map_error)?;
            nodes.push(Node {
                fd,
                name: name.to_owned(),
                before,
            });
        }
        let directory = Self {
            runtime,
            uid,
            nodes,
            _lock: None,
            writable: false,
            held: Vec::new(),
            cancel,
        };
        directory.validate()?;
        Ok(directory)
    }
    fn control(&self) -> std::os::fd::RawFd {
        self.nodes.last().expect("control").fd.as_raw_fd()
    }
    /// Checks the runtime and the held directory chain.
    pub fn validate(&self) -> JResult<()> {
        self.runtime.validate().map_err(map_error)?;
        for (i, node) in self.nodes.iter().enumerate() {
            let held = stat_fd(node.fd.as_raw_fd()).map_err(map_error)?;
            valid_stat(&held, self.uid, libc::S_IFDIR, Some(0o700)).map_err(map_error)?;
            validate_acl(node.fd.as_raw_fd(), self.uid).map_err(map_error)?;
            if identity(&held) != identity(&node.before) {
                return Err(JournalError::Unsafe);
            }
            if i > 0
                && identity(
                    &stat_at(self.nodes[i - 1].fd.as_raw_fd(), &node.name).map_err(map_error)?,
                ) != identity(&held)
            {
                return Err(JournalError::Unsafe);
            }
        }
        Ok(())
    }
    fn active(&self) -> JResult<()> {
        if self.cancel.load(Ordering::Acquire) {
            Err(JournalError::Cancelled)
        } else {
            Ok(())
        }
    }
    /// Lists and checks every control-directory entry: only journal names,
    /// regular files of this account, mode 0600, one link and no extended
    /// ACL. Reads names in batches of 128 and at most 131,328 entries.
    pub fn census(&mut self) -> JResult<Vec<JournalEntry>> {
        self.active()?;
        self.validate()?;
        let fresh = open_at(self.control(), c".", libc::O_RDONLY | libc::O_DIRECTORY, 0)
            .map_err(map_error)?;
        let raw = std::os::fd::IntoRawFd::into_raw_fd(fresh);
        // SAFETY: fdopendir takes the fresh descriptor on success; failure closes it below.
        let stream = unsafe { libc::fdopendir(raw) };
        if stream.is_null() {
            // SAFETY: failed fdopendir left our descriptor owned here.
            unsafe {
                libc::close(raw);
            }
            return Err(JournalError::Io);
        }
        let stream = DirectoryStream(stream);
        let mut names = Vec::new();
        loop {
            if names.len() % CENSUS_BATCH == 0 {
                self.active()?;
            }
            // SAFETY: Darwin errno is thread-local; reset before readdir's EOF/error distinction.
            unsafe {
                *libc::__error() = 0;
            }
            // SAFETY: stream remains uniquely owned and open; entry copied before next call.
            let entry = unsafe { libc::readdir(stream.0) };
            if entry.is_null() {
                if std::io::Error::last_os_error().raw_os_error() != Some(0) {
                    return Err(JournalError::Io);
                }
                break;
            }
            // SAFETY: readdir returns a terminated d_name inside the live entry.
            let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) };
            if [c".", c".."].contains(&name) {
                continue;
            }
            let parsed =
                JournalName::parse(name.to_bytes()).ok_or(JournalError::UnsupportedLayout)?;
            if names.len() == MAX_CENSUS {
                return Err(JournalError::Limit);
            }
            names.push(parsed);
        }
        drop(stream);
        names.sort();
        let mut entries = Vec::with_capacity(names.len());
        for (count, name) in names.into_iter().enumerate() {
            if count % CENSUS_BATCH == 0 {
                self.active()?;
            }
            let c_name = name.c_name();
            let named = stat_at(self.control(), &c_name).map_err(map_error)?;
            valid_stat(&named, self.uid, libc::S_IFREG, Some(0o600)).map_err(map_error)?;
            let fd = open_at(self.control(), &c_name, libc::O_RDONLY, 0).map_err(map_error)?;
            let held = stat_fd(fd.as_raw_fd()).map_err(map_error)?;
            validate_acl(fd.as_raw_fd(), self.uid).map_err(map_error)?;
            if identity(&held) != identity(&named) || held.st_size < 0 {
                return Err(JournalError::Unsafe);
            }
            entries.push(JournalEntry {
                name,
                bytes: held.st_size as u64,
            });
        }
        self.validate()?;
        Ok(entries)
    }
    /// The held descriptor for `name`, opening it on first use. At most 12
    /// descriptors stay open; the least recently used closes first.
    fn held(&mut self, name: JournalName, write: bool) -> JResult<&Held> {
        if let Some(position) = self
            .held
            .iter()
            .position(|h| h.name == name && (h.writable || !write))
        {
            let entry = self.held.remove(position);
            self.held.push(entry);
            return Ok(self.held.last().expect("held"));
        }
        if write && !self.writable {
            return Err(JournalError::Unsafe);
        }
        let c_name = name.c_name();
        let fd = open_at(
            self.control(),
            &c_name,
            if write { libc::O_RDWR } else { libc::O_RDONLY },
            0,
        )
        .map_err(map_error)?;
        let stat = stat_fd(fd.as_raw_fd()).map_err(map_error)?;
        valid_stat(&stat, self.uid, libc::S_IFREG, Some(0o600)).map_err(map_error)?;
        validate_acl(fd.as_raw_fd(), self.uid).map_err(map_error)?;
        if identity(&stat_at(self.control(), &c_name).map_err(map_error)?) != identity(&stat) {
            return Err(JournalError::Unsafe);
        }
        self.held.retain(|h| h.name != name);
        if self.held.len() >= OPEN_DESCRIPTORS {
            self.held.remove(0);
        }
        self.held.push(Held {
            name,
            file: std::fs::File::from(fd),
            identity: identity(&stat),
            writable: write,
        });
        Ok(self.held.last().expect("held"))
    }
    fn forget(&mut self, name: JournalName) {
        self.held.retain(|h| h.name != name);
    }
    /// Size of a held file, with its identity rechecked.
    pub fn size(&mut self, name: JournalName) -> JResult<u64> {
        let held = self.held(name, false)?;
        let stat = stat_fd(held.file.as_raw_fd()).map_err(map_error)?;
        if identity(&stat) != held.identity {
            return Err(JournalError::Unsafe);
        }
        Ok(stat.st_size as u64)
    }
    /// Reads exactly `buffer.len()` bytes at `offset`. A short file is
    /// `Absent` (the bytes are not there). No time bound; cancel stops it.
    pub fn read_at(&mut self, name: JournalName, offset: u64, buffer: &mut [u8]) -> JResult<()> {
        gate::read_point(&self.runtime, &self.cancel, self.writable)
            .map_err(|_| JournalError::Cancelled)?;
        self.active()?;
        let held = self.held(name, false)?;
        let mut done = 0;
        while done < buffer.len() {
            match held.file.read_at(&mut buffer[done..], offset + done as u64) {
                Ok(0) => return Err(JournalError::Absent),
                Ok(n) => done += n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => return Err(JournalError::Io),
            }
        }
        Ok(())
    }
    /// Creates a new file with `O_CREAT|O_EXCL`, mode 0600, and makes its
    /// directory entry durable. A segment file gets an `F_PREALLOCATE`
    /// request for 8 MiB without changing its size (a hint; its failure is
    /// ignored because the quota checks free space first).
    pub fn create(&mut self, name: JournalName) -> JResult<()> {
        if !self.writable || name == JournalName::Lock {
            return Err(JournalError::Unsafe);
        }
        self.active()?;
        let c_name = name.c_name();
        let fd = open_at(
            self.control(),
            &c_name,
            libc::O_RDWR | libc::O_CREAT | libc::O_EXCL,
            0o600,
        )
        .map_err(map_error)?;
        if let JournalName::Segment(_) = name {
            let mut request = libc::fstore_t {
                fst_flags: libc::F_ALLOCATECONTIG,
                fst_posmode: libc::F_PEOFPOSMODE,
                fst_offset: 0,
                fst_length: SEGMENT_BYTES as libc::off_t,
                fst_bytesalloc: 0,
            };
            // SAFETY: F_PREALLOCATE reads and writes this live fstore_t.
            if unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_PREALLOCATE, &mut request) } != 0 {
                request.fst_flags = libc::F_ALLOCATEALL;
                // SAFETY: as above, without the contiguous request.
                let _ = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_PREALLOCATE, &mut request) };
            }
        }
        drop(fd);
        self.sync_directory()?;
        Ok(())
    }
    fn sync_directory(&self) -> JResult<()> {
        // SAFETY: control is a retained directory descriptor.
        checked(unsafe { libc::fsync(self.control()) }).map_err(map_error)?;
        Ok(())
    }
    /// Writes all bytes at `offset` of a derived tmp file (no sync).
    pub fn write_at(&mut self, name: JournalName, offset: u64, bytes: &[u8]) -> JResult<()> {
        if !name.is_tmp() {
            return Err(JournalError::Unsafe);
        }
        let held = self.held(name, true)?;
        let target = HeldFile {
            file: &held.file,
            fault: None,
        };
        let mut written = 0;
        while written < bytes.len() {
            match target.write_at(&bytes[written..], offset + written as u64) {
                Ok(0) => return Err(JournalError::Io),
                Ok(n) => written += n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) if e.raw_os_error() == Some(libc::ENOSPC) => {
                    return Err(JournalError::NoSpace { unchanged: false });
                }
                Err(_) => return Err(JournalError::Io),
            }
        }
        Ok(())
    }
    /// `F_FULLFSYNC` of a held file.
    pub fn sync(&mut self, name: JournalName) -> JResult<()> {
        let held = self.held(name, true)?;
        HeldFile {
            file: &held.file,
            fault: None,
        }
        .full_sync()
        .map_err(map_error)
    }
    /// Appends one frame at `expected`, the current end of the segment
    /// file, then `F_FULLFSYNC`. The write and sync have no time bound
    /// (J1-0). `ENOSPC` reports whether the size is unchanged.
    pub fn append(&mut self, segment: u64, expected: u64, bytes: &[u8]) -> JResult<()> {
        if bytes.is_empty() || bytes.len() > 65536 || expected + bytes.len() as u64 > SEGMENT_BYTES
        {
            return Err(JournalError::Limit);
        }
        self.validate()?;
        let name = JournalName::Segment(segment);
        if self.size(name)? != expected {
            return Err(JournalError::Unsafe);
        }
        // The held descriptor still names the file at its path.
        let named = stat_at(self.control(), &name.c_name()).map_err(map_error)?;
        if Some(identity(&named)) != self.held.last().map(|h| h.identity) {
            return Err(JournalError::Unsafe);
        }
        let fault = gate::append_point(&self.runtime);
        let held = self.held(name, true)?;
        if fault == Some(gate::Fault::NoSpace) {
            return Err(JournalError::NoSpace { unchanged: true });
        }
        let result = write_and_sync(
            &HeldFile {
                file: &held.file,
                fault,
            },
            expected as usize,
            bytes,
        );
        if let Err(error) = result {
            let enospc = matches!(&error, Error::Io(e) if e.raw_os_error() == Some(libc::ENOSPC));
            if enospc {
                let size = stat_fd(held.file.as_raw_fd()).map_err(map_error)?.st_size as u64;
                return Err(JournalError::NoSpace {
                    unchanged: size == expected,
                });
            }
            return Err(map_error(error));
        }
        if self.size(name)? != expected + bytes.len() as u64 {
            return Err(JournalError::Unsafe);
        }
        self.validate()
    }
    /// Renames a durable tmp file to its final name and syncs the directory.
    pub fn publish(&mut self, from: JournalName, to: JournalName) -> JResult<()> {
        let valid = matches!(
            (from, to),
            (JournalName::CheckpointTmp(a), JournalName::Checkpoint(b)) if a == b
        ) || matches!(
            (from, to),
            (JournalName::RunTmp(a, b), JournalName::Run(c, d)) if a == c && b == d
        );
        if !self.writable || !valid {
            return Err(JournalError::Unsafe);
        }
        self.active()?;
        self.forget(from);
        self.forget(to);
        let (from_c, to_c) = (from.c_name(), to.c_name());
        // SAFETY: both names are fixed journal names relative to the held control directory.
        checked(unsafe {
            libc::renameat(
                self.control(),
                from_c.as_ptr(),
                self.control(),
                to_c.as_ptr(),
            )
        })
        .map_err(map_error)?;
        self.sync_directory()
    }
    /// Removes a derived file and syncs the directory. Never removes a
    /// segment file or the lock.
    pub fn remove(&mut self, name: JournalName) -> JResult<()> {
        if !self.writable || !name.is_derived() {
            return Err(JournalError::Unsafe);
        }
        self.forget(name);
        let c_name = name.c_name();
        // SAFETY: fixed derived journal name relative to the held control directory.
        match checked(unsafe { libc::unlinkat(self.control(), c_name.as_ptr(), 0) }) {
            Ok(_) | Err(Error::Absent) => {}
            Err(error) => return Err(map_error(error)),
        }
        self.sync_directory()
    }
    /// Free bytes on the journal's volume, from `fstatfs` on the held directory.
    pub fn free_bytes(&mut self) -> JResult<u64> {
        if let Some(bytes) = gate::free_bytes(&self.runtime) {
            return Ok(bytes);
        }
        // SAFETY: initialized POD is a valid fstatfs output buffer.
        let mut fs: libc::statfs = unsafe { std::mem::zeroed() };
        // SAFETY: control is held and fs is writable.
        checked(unsafe { libc::fstatfs(self.control(), &mut fs) }).map_err(map_error)?;
        Ok(fs.f_bavail.saturating_mul(u64::from(fs.f_bsize)))
    }
    /// A test hold or crash point; inert without the `test-support` feature.
    pub fn point(&self, point: gate::Point) -> JResult<()> {
        gate::named_point(&self.runtime, point, &self.cancel).map_err(|_| JournalError::Cancelled)
    }
}

/// The two blocking calls of one append. A fake implements this in unit tests.
trait AppendTarget {
    fn write_at(&self, bytes: &[u8], offset: u64) -> std::io::Result<usize>;
    fn full_sync(&self) -> Result<()>;
}
struct HeldFile<'a> {
    file: &'a std::fs::File,
    fault: Option<gate::Fault>,
}
impl AppendTarget for HeldFile<'_> {
    fn write_at(&self, bytes: &[u8], offset: u64) -> std::io::Result<usize> {
        if self.fault == Some(gate::Fault::Write) {
            return Err(std::io::Error::from_raw_os_error(libc::EIO));
        }
        self.file.write_at(bytes, offset)
    }
    fn full_sync(&self) -> Result<()> {
        if self.fault == Some(gate::Fault::Sync) {
            return Err(std::io::Error::from_raw_os_error(libc::EIO).into());
        }
        // SAFETY: fcntl full-sync borrows the held regular-file descriptor.
        checked(unsafe { libc::fcntl(self.file.as_raw_fd(), libc::F_FULLFSYNC) })?;
        Ok(())
    }
}
/// Writes the complete frame, then syncs it. Short writes advance the same
/// offset and EINTR retries; no timer ends the loop.
fn write_and_sync(target: &impl AppendTarget, offset: usize, bytes: &[u8]) -> Result<()> {
    let mut written = 0;
    while written < bytes.len() {
        match target.write_at(&bytes[written..], (offset + written) as u64) {
            Ok(0) => return Err(Error::Unavailable),
            Ok(n) => written += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.into()),
        }
    }
    target.full_sync()
}

/// Retains the validated database directory while the embedded engine owns it.
pub struct DatabaseDirectory {
    runtime: RuntimeDirectory,
    root: OwnedFd,
    directory: OwnedFd,
    uid: libc::uid_t,
    before: libc::stat,
    path: std::path::PathBuf,
}
impl DatabaseDirectory {
    pub fn open(runtime: RuntimeDirectory, create: bool) -> Result<Self> {
        let (root, uid) = runtime.authority_root()?;
        let directory = directory(&root, c"db", create)?;
        let before = stat_fd(directory.as_raw_fd())?;
        valid_stat(&before, uid, libc::S_IFDIR, Some(0o700))?;
        validate_acl(directory.as_raw_fd(), uid)?;
        let mut path = [0i8; libc::PATH_MAX as usize];
        // SAFETY: F_GETPATH writes at most PATH_MAX bytes into this live array.
        checked(unsafe { libc::fcntl(directory.as_raw_fd(), libc::F_GETPATH, path.as_mut_ptr()) })?;
        // SAFETY: successful F_GETPATH supplies a terminated pathname.
        let path = unsafe { CStr::from_ptr(path.as_ptr()) }
            .to_str()
            .map_err(|_| Error::UnsafeRuntime)?
            .into();
        let result = Self {
            runtime,
            root,
            directory,
            uid,
            before,
            path,
        };
        result.validate()?;
        Ok(result)
    }
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }
    pub fn validate(&self) -> Result<()> {
        self.runtime.validate()?;
        let held = stat_fd(self.directory.as_raw_fd())?;
        valid_stat(&held, self.uid, libc::S_IFDIR, Some(0o700))?;
        validate_acl(self.directory.as_raw_fd(), self.uid)?;
        if identity(&held) != identity(&self.before)
            || identity(&stat_at(self.root.as_raw_fd(), c"db")?) != identity(&held)
        {
            return Err(Error::UnsafeRuntime);
        }
        Ok(())
    }
}

mod footprint;
pub(crate) mod gate;
#[cfg(test)]
mod tests;
