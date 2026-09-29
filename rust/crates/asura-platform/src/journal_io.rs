//! Descriptor-held ordinary-file journal IO. Call only on the storage worker.
use crate::runtime::{identity, open_at, stat_at, stat_fd, valid_stat, validate_acl};
use crate::{Error, Result, RuntimeDirectory, checked};
use std::ffi::{CStr, CString};
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::fs::FileExt;
use std::time::Instant;

const LIMIT: usize = 8 * 1024 * 1024;
struct Node {
    fd: OwnedFd,
    name: CString,
    before: libc::stat,
}
pub struct JournalFile {
    runtime: RuntimeDirectory,
    uid: libc::uid_t,
    nodes: Vec<Node>,
    size: usize,
    fenced: bool,
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
impl JournalFile {
    pub fn open(runtime: RuntimeDirectory, create: bool) -> Result<Self> {
        let (root, uid) = runtime.authority_root()?;
        // SAFETY: initialized POD is a valid fstatfs output buffer.
        let mut fs: libc::statfs = unsafe { std::mem::zeroed() };
        // SAFETY: root is held and fs is writable.
        checked(unsafe { libc::fstatfs(root.as_raw_fd(), &mut fs) })?;
        // SAFETY: successful fstatfs supplies a terminated filesystem name.
        if unsafe { CStr::from_ptr(fs.f_fstypename.as_ptr()) } != c"apfs"
            || fs.f_flags & libc::MNT_LOCAL as u32 == 0
        {
            return Err(Error::Unavailable);
        }
        let mut nodes = vec![Node {
            before: stat_fd(root.as_raw_fd())?,
            fd: root,
            name: c".asura".to_owned(),
        }];
        for name in [c"state", c"control"] {
            let fd = directory(&nodes.last().expect("root").fd, name, create)?;
            let before = stat_fd(fd.as_raw_fd())?;
            valid_stat(&before, uid, libc::S_IFDIR, Some(0o700))?;
            validate_acl(fd.as_raw_fd(), uid)?;
            nodes.push(Node {
                fd,
                name: name.to_owned(),
                before,
            });
        }
        let parent = nodes.last().expect("control").fd.as_raw_fd();
        let fd = open_at(
            parent,
            c"slot-0.log",
            libc::O_RDWR
                | if create {
                    libc::O_CREAT | libc::O_EXCL
                } else {
                    0
                },
            0o600,
        )?;
        // SAFETY: flock applies a nonblocking exclusive lock to our held file.
        if unsafe { libc::flock(fd.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            return Err(Error::OwnerBusy);
        }
        let before = stat_fd(fd.as_raw_fd())?;
        valid_stat(&before, uid, libc::S_IFREG, Some(0o600))?;
        validate_acl(fd.as_raw_fd(), uid)?;
        if create {
            // SAFETY: parent is a retained directory descriptor.
            checked(unsafe { libc::fsync(parent) })?;
        }
        let size = usize::try_from(before.st_size).map_err(|_| Error::UnsafeRuntime)?;
        if size > LIMIT {
            return Err(Error::Unavailable);
        }
        nodes.push(Node {
            fd,
            name: c"slot-0.log".to_owned(),
            before,
        });
        let result = Self {
            runtime,
            uid,
            nodes,
            size,
            fenced: false,
        };
        result.validate()?;
        Ok(result)
    }
    pub fn validate(&self) -> Result<()> {
        self.runtime.validate()?;
        for (i, node) in self.nodes.iter().enumerate() {
            let held = stat_fd(node.fd.as_raw_fd())?;
            let file = i + 1 == self.nodes.len();
            valid_stat(
                &held,
                self.uid,
                if file { libc::S_IFREG } else { libc::S_IFDIR },
                Some(if file { 0o600 } else { 0o700 }),
            )?;
            validate_acl(node.fd.as_raw_fd(), self.uid)?;
            if identity(&held) != identity(&node.before)
                || (file
                    && (held.st_size != self.size as i64
                        || held.st_mtime != node.before.st_mtime
                        || held.st_mtime_nsec != node.before.st_mtime_nsec))
            {
                return Err(Error::UnsafeRuntime);
            }
            if i > 0
                && identity(&stat_at(self.nodes[i - 1].fd.as_raw_fd(), &node.name)?)
                    != identity(&held)
            {
                return Err(Error::UnsafeRuntime);
            }
        }
        Ok(())
    }
    pub fn read_all(&self, deadline: Instant) -> Result<Vec<u8>> {
        self.validate()?;
        let fd = self.nodes.last().expect("file").fd.try_clone()?;
        let file = std::fs::File::from(fd);
        let mut bytes = Vec::with_capacity(self.size);
        while bytes.len() < self.size {
            if Instant::now() >= deadline {
                return Err(Error::Deadline);
            }
            let mut chunk = [0; 65536];
            let wanted = chunk.len().min(self.size - bytes.len());
            let n = file.read_at(&mut chunk[..wanted], bytes.len() as u64)?;
            if n == 0 {
                return Err(Error::UnsafeRuntime);
            }
            bytes.extend_from_slice(&chunk[..n]);
        }
        self.validate()?;
        Ok(bytes)
    }
    pub fn append(&mut self, expected: usize, bytes: &[u8], deadline: Instant) -> Result<()> {
        if self.fenced
            || expected != self.size
            || bytes.is_empty()
            || bytes.len() > 65536
            || self.size.checked_add(bytes.len()).is_none_or(|n| n > LIMIT)
        {
            return Err(Error::Unavailable);
        }
        self.validate()?;
        self.fenced = true;
        let node = self.nodes.last_mut().expect("file");
        let file = std::fs::File::from(node.fd.try_clone()?);
        let mut written = 0;
        while written < bytes.len() {
            if Instant::now() >= deadline {
                return Err(Error::Deadline);
            }
            match file.write_at(&bytes[written..], (self.size + written) as u64) {
                Ok(0) => return Err(Error::Unavailable),
                Ok(n) => written += n,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e.into()),
            }
        }
        // SAFETY: fcntl full-sync borrows the held regular-file descriptor.
        checked(unsafe { libc::fcntl(node.fd.as_raw_fd(), libc::F_FULLFSYNC) })?;
        if Instant::now() >= deadline {
            return Err(Error::Deadline);
        }
        node.before = stat_fd(node.fd.as_raw_fd())?;
        self.size += bytes.len();
        self.validate()?;
        self.fenced = false;
        Ok(())
    }
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
