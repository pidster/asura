use crate::{Error, Result, checked, effective_uid, last_error};
use std::ffi::{CStr, CString};
use std::fs::{DirBuilder, File};
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::DirBuilderExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

const LOCK: &CStr = c"owner.lock";
const SOCKET: &CStr = c"control.sock";
const ACL_EXTENDED: i32 = 0x100;
unsafe extern "C" {
    fn acl_get_fd_np(fd: i32, kind: i32) -> *mut libc::c_void;
    fn acl_get_link_np(path: *const libc::c_char, kind: i32) -> *mut libc::c_void;
    fn acl_get_entry(acl: *mut libc::c_void, entry_id: i32, entry: *mut *mut libc::c_void) -> i32;
    fn acl_get_tag_type(entry: *mut libc::c_void, tag: *mut i32) -> i32;
    fn acl_get_permset_mask_np(entry: *mut libc::c_void, mask: *mut u64) -> i32;
    fn acl_get_qualifier(entry: *mut libc::c_void) -> *mut libc::c_void;
    fn acl_free(pointer: *mut libc::c_void) -> i32;
    fn mbr_uid_to_uuid(uid: libc::uid_t, uuid: *mut u8) -> i32;
}
struct AclAllocation(*mut libc::c_void);
impl Drop for AclAllocation {
    fn drop(&mut self) {
        // SAFETY: this allocation was returned by an ACL allocation function.
        unsafe {
            acl_free(self.0);
        }
    }
}
pub(crate) fn validate_acl(fd: RawFd, owner: libc::uid_t) -> Result<()> {
    // SAFETY: borrowed open descriptor, fixed supported ACL type.
    let acl = unsafe { acl_get_fd_np(fd, ACL_EXTENDED) };
    validate_acl_allocation(acl, owner)
}
fn validate_acl_allocation(acl: *mut libc::c_void, owner: libc::uid_t) -> Result<()> {
    if acl.is_null() {
        // Darwin reports an absent extended ACL as ENOENT even for a valid open
        // descriptor. Callers validate the object independently; named socket
        // inspection also rechecks its identity after this query.
        return if io::Error::last_os_error().raw_os_error() == Some(libc::ENOENT) {
            Ok(())
        } else {
            Err(Error::UnsafeRuntime)
        };
    }
    let acl = AclAllocation(acl);
    let mut owner_uuid = [0u8; 16];
    // SAFETY: UUID output holds exactly 16 bytes.
    if unsafe { mbr_uid_to_uuid(owner, owner_uuid.as_mut_ptr()) } != 0 {
        return Err(Error::UnsafeRuntime);
    }
    let mut selector = 0;
    for _ in 0..170 {
        let mut entry = std::ptr::null_mut();
        // SAFETY: ACL remains alive and entry output is initialized.
        let result = unsafe { acl_get_entry(acl.0, selector, &mut entry) };
        if result == -1 {
            // Darwin returns -1 with EINVAL at end of iteration.
            if io::Error::last_os_error().raw_os_error() == Some(libc::EINVAL) {
                return Ok(());
            }
            return Err(Error::UnsafeRuntime);
        }
        if result != 0 || entry.is_null() {
            return Err(Error::UnsafeRuntime);
        }
        selector = -1;
        let mut tag = 0;
        let mut permissions = 0;
        // SAFETY: entry belongs to the retained ACL; both outputs have SDK types.
        if unsafe { acl_get_tag_type(entry, &mut tag) } != 0
            || unsafe { acl_get_permset_mask_np(entry, &mut permissions) } != 0
        {
            return Err(Error::UnsafeRuntime);
        }
        if tag == 1 && permissions != 0 {
            // SAFETY: ACL qualifier allocation is a UUID for extended entries.
            let qualifier = unsafe { acl_get_qualifier(entry) };
            if qualifier.is_null() {
                return Err(Error::UnsafeRuntime);
            }
            let qualifier = AclAllocation(qualifier);
            // SAFETY: Darwin extended ACL qualifier contains 16 UUID bytes.
            let uuid = unsafe { std::slice::from_raw_parts(qualifier.0.cast::<u8>(), 16) };
            if uuid != owner_uuid {
                return Err(Error::UnsafeRuntime);
            }
        } else if tag != 1 && tag != 2 {
            return Err(Error::UnsafeRuntime);
        }
    }
    Err(Error::UnsafeRuntime)
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Identity {
    device: libc::dev_t,
    inode: libc::ino_t,
}
fn identity_error(error: Error) -> Error {
    match error {
        Error::Absent => Error::UnsafeRuntime,
        other => other,
    }
}
pub(crate) fn identity(stat: &libc::stat) -> Identity {
    Identity {
        device: stat.st_dev,
        inode: stat.st_ino,
    }
}
pub(crate) fn stat_fd(fd: RawFd) -> Result<libc::stat> {
    // SAFETY: zeroed stat is a valid output buffer.
    let mut stat = unsafe { std::mem::zeroed() };
    // SAFETY: stat is writable and fd is borrowed.
    checked(unsafe { libc::fstat(fd, &mut stat) })?;
    Ok(stat)
}
pub(crate) fn stat_at(fd: RawFd, name: &CStr) -> Result<libc::stat> {
    // SAFETY: zeroed stat is valid output and name is terminated.
    let mut stat = unsafe { std::mem::zeroed() };
    // SAFETY: does not follow the final symlink; parent descriptor is retained.
    checked(unsafe { libc::fstatat(fd, name.as_ptr(), &mut stat, libc::AT_SYMLINK_NOFOLLOW) })?;
    Ok(stat)
}
pub(crate) fn open_at(fd: RawFd, name: &CStr, flags: i32, mode: libc::mode_t) -> Result<OwnedFd> {
    // SAFETY: name is terminated, flags specify whether mode is consumed.
    let result = unsafe {
        libc::openat(
            fd,
            name.as_ptr(),
            flags | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK,
            mode as libc::c_int,
        )
    };
    if result < 0 {
        let e = io::Error::last_os_error();
        return Err(
            if [Some(libc::ELOOP), Some(libc::ENOTDIR)].contains(&e.raw_os_error()) {
                Error::UnsafeRuntime
            } else {
                e.into()
            },
        );
    }
    // SAFETY: successful open returns a new descriptor owned exclusively here.
    Ok(unsafe { OwnedFd::from_raw_fd(result) })
}
pub(crate) fn valid_stat(
    stat: &libc::stat,
    uid: libc::uid_t,
    kind: libc::mode_t,
    mode: Option<libc::mode_t>,
) -> Result<()> {
    if stat.st_uid != uid
        || stat.st_mode & libc::S_IFMT != kind
        || stat.st_mode & 0o022 != 0
        || mode.is_some_and(|m| stat.st_mode & 0o7777 != m)
        || (kind == libc::S_IFREG && stat.st_nlink != 1)
    {
        return Err(Error::UnsafeRuntime);
    }
    Ok(())
}
struct Directory {
    fd: OwnedFd,
    name: CString,
    id: Identity,
    uid: libc::uid_t,
    private: bool,
}
struct RuntimeInner {
    directories: Vec<Directory>,
    path: PathBuf,
    uid: libc::uid_t,
}
#[derive(Clone)]
pub struct RuntimeDirectory(Arc<RuntimeInner>);
fn account_home(uid: libc::uid_t) -> Result<PathBuf> {
    let mut buffer = vec![0u8; 64 * 1024];
    // SAFETY: passwd is an output POD; pointers are consumed while buffer remains alive.
    let mut password: libc::passwd = unsafe { std::mem::zeroed() };
    let mut result = std::ptr::null_mut();
    // SAFETY: all output pointers refer to writable buffers of declared capacity.
    let status = unsafe {
        libc::getpwuid_r(
            uid,
            &mut password,
            buffer.as_mut_ptr().cast(),
            buffer.len(),
            &mut result,
        )
    };
    if status != 0 || result.is_null() || password.pw_dir.is_null() {
        return Err(Error::UnsafeRuntime);
    }
    // SAFETY: successful getpwuid_r supplies a NUL-terminated string within buffer.
    let home = unsafe { CStr::from_ptr(password.pw_dir) }
        .to_str()
        .map_err(|_| Error::UnsafeRuntime)?;
    Ok(PathBuf::from(home))
}
impl RuntimeDirectory {
    /// Managed model assets associated with this retained runtime identity.
    /// This is a path projection only: it performs no IO or availability check.
    pub fn model_assets_path(&self) -> PathBuf {
        self.0
            .path
            .parent()
            .expect("runtime path has managed parent")
            .join("data/models")
    }

    pub(crate) fn model_root(&self) -> Result<(OwnedFd, PathBuf, libc::uid_t)> {
        self.validate()?;
        let directory = self.0.directories.last().ok_or(Error::UnsafeRuntime)?;
        // SAFETY: duplicate a retained validated runtime descriptor with exclusive ownership.
        let raw = unsafe { libc::fcntl(directory.fd.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 5) };
        if raw < 0 {
            return Err(last_error());
        }
        // SAFETY: successful duplication returns a new owned descriptor.
        Ok((
            unsafe { OwnedFd::from_raw_fd(raw) },
            self.0.path.clone(),
            self.0.uid,
        ))
    }

    pub(crate) fn authority_root(&self) -> Result<(OwnedFd, libc::uid_t)> {
        self.validate()?;
        let directory = &self.0.directories[self.0.directories.len() - 2];
        // SAFETY: duplicate retained validated .asura directory; new FD has exclusive ownership.
        let raw = unsafe { libc::fcntl(directory.fd.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 0) };
        if raw < 0 {
            return Err(last_error());
        }
        Ok((unsafe { OwnedFd::from_raw_fd(raw) }, self.0.uid))
    }
    pub fn account(create: bool) -> Result<Self> {
        let uid = effective_uid()?;
        Self::open_home(&account_home(uid)?, uid, create, false)
    }
    #[cfg(any(test, feature = "test-support"))]
    pub fn scratch(path: &Path, create: bool) -> Result<Self> {
        Self::open_home(path, effective_uid()?, create, true)
    }
    fn open_home(home: &Path, uid: libc::uid_t, create: bool, scratch: bool) -> Result<Self> {
        if !home.is_absolute() || home.to_str().is_none() {
            return Err(Error::UnsafeRuntime);
        }
        let path = home.join(".asura/run");
        if path.join("control.sock").as_os_str().as_bytes().len() > 103 {
            return Err(Error::UnsafeRuntime);
        }
        let mut directories = Vec::new();
        if scratch {
            let name =
                CString::new(home.as_os_str().as_bytes()).map_err(|_| Error::UnsafeRuntime)?;
            let fd = open_at(libc::AT_FDCWD, &name, libc::O_RDONLY | libc::O_DIRECTORY, 0)?;
            let stat = stat_fd(fd.as_raw_fd())?;
            valid_stat(&stat, uid, libc::S_IFDIR, Some(0o700))?;
            validate_acl(fd.as_raw_fd(), uid)?;
            directories.push(Directory {
                fd,
                name,
                id: identity(&stat),
                uid,
                private: true,
            });
        } else {
            let fd = open_at(libc::AT_FDCWD, c"/", libc::O_RDONLY | libc::O_DIRECTORY, 0)?;
            let stat = stat_fd(fd.as_raw_fd())?;
            valid_stat(&stat, 0, libc::S_IFDIR, None)?;
            validate_acl(fd.as_raw_fd(), 0)?;
            directories.push(Directory {
                fd,
                name: CString::new("/").unwrap(),
                id: identity(&stat),
                uid: 0,
                private: false,
            });
            let components: Vec<_> = home.components().collect();
            for (index, component) in components.iter().enumerate().skip(1) {
                let Component::Normal(name) = component else {
                    return Err(Error::UnsafeRuntime);
                };
                let owner = if index + 1 == components.len() {
                    uid
                } else {
                    0
                };
                Self::append(&mut directories, name.as_bytes(), owner, false, false)?;
            }
        }
        // SAFETY: statfs output initialized; filesystem queried through retained home FD.
        let mut filesystem: libc::statfs = unsafe { std::mem::zeroed() };
        // SAFETY: output points to one complete statfs.
        checked(unsafe {
            libc::fstatfs(directories.last().unwrap().fd.as_raw_fd(), &mut filesystem)
        })?;
        // SAFETY: fixed kernel f_fstypename is terminated for a successful statfs.
        let fs_type = unsafe { CStr::from_ptr(filesystem.f_fstypename.as_ptr()) };
        if fs_type != c"apfs" || filesystem.f_flags & libc::MNT_LOCAL as u32 == 0 {
            return Err(Error::UnsafeRuntime);
        }
        Self::append(&mut directories, b".asura", uid, true, create)?;
        Self::append(&mut directories, b"run", uid, true, create)?;
        let runtime = Self(Arc::new(RuntimeInner {
            directories,
            path,
            uid,
        }));
        runtime.validate()?;
        Ok(runtime)
    }
    fn append(
        chain: &mut Vec<Directory>,
        name: &[u8],
        uid: libc::uid_t,
        private: bool,
        create: bool,
    ) -> Result<()> {
        let name = CString::new(name).map_err(|_| Error::UnsafeRuntime)?;
        let parent = chain.last().unwrap().fd.as_raw_fd();
        let fd = match open_at(parent, &name, libc::O_RDONLY | libc::O_DIRECTORY, 0) {
            Err(Error::Absent) if create => {
                // SAFETY: fixed child name under retained validated parent, restrictive mode.
                let result = unsafe { libc::mkdirat(parent, name.as_ptr(), 0o700) };
                if result != 0 && io::Error::last_os_error().raw_os_error() != Some(libc::EEXIST) {
                    return Err(last_error());
                }
                open_at(parent, &name, libc::O_RDONLY | libc::O_DIRECTORY, 0)?
            }
            result => result?,
        };
        let stat = stat_fd(fd.as_raw_fd())?;
        valid_stat(&stat, uid, libc::S_IFDIR, private.then_some(0o700))?;
        validate_acl(fd.as_raw_fd(), uid)?;
        chain.push(Directory {
            fd,
            name,
            id: identity(&stat),
            uid,
            private,
        });
        Ok(())
    }
    fn fd(&self) -> RawFd {
        self.0.directories.last().unwrap().fd.as_raw_fd()
    }
    pub fn validate(&self) -> Result<()> {
        for (i, directory) in self.0.directories.iter().enumerate() {
            let parent = if i == 0 {
                libc::AT_FDCWD
            } else {
                self.0.directories[i - 1].fd.as_raw_fd()
            };
            let named = stat_at(parent, &directory.name).map_err(identity_error)?;
            let held = stat_fd(directory.fd.as_raw_fd())?;
            if identity(&named) != directory.id || identity(&held) != directory.id {
                return Err(Error::UnsafeRuntime);
            }
            valid_stat(
                &held,
                directory.uid,
                libc::S_IFDIR,
                directory.private.then_some(0o700),
            )?;
            validate_acl(directory.fd.as_raw_fd(), directory.uid)?;
        }
        Ok(())
    }
    fn lock(&self, create: bool) -> Result<OwnedFd> {
        self.validate()?;
        let fd = if create {
            match open_at(
                self.fd(),
                LOCK,
                libc::O_RDWR | libc::O_CREAT | libc::O_EXCL,
                0o600,
            ) {
                Err(Error::Io(ref e)) if e.raw_os_error() == Some(libc::EEXIST) => {
                    open_at(self.fd(), LOCK, libc::O_RDWR, 0)?
                }
                result => result?,
            }
        } else {
            open_at(self.fd(), LOCK, libc::O_RDONLY, 0)?
        };
        self.validate_lock(fd.as_raw_fd())?;
        Ok(fd)
    }
    fn validate_lock(&self, fd: RawFd) -> Result<()> {
        self.validate()?;
        let held = stat_fd(fd)?;
        let named = stat_at(self.fd(), LOCK).map_err(identity_error)?;
        if identity(&held) != identity(&named) {
            return Err(Error::UnsafeRuntime);
        }
        valid_stat(&held, self.0.uid, libc::S_IFREG, Some(0o600))?;
        validate_acl(fd, self.0.uid)
    }
    fn endpoint(&self) -> Result<Identity> {
        self.validate()?;
        let stat = stat_at(self.fd(), SOCKET)?;
        valid_stat(&stat, self.0.uid, libc::S_IFSOCK, Some(0o600))?;
        let path = CString::new(self.0.path.join("control.sock").as_os_str().as_bytes())
            .map_err(|_| Error::UnsafeRuntime)?;
        // SAFETY: terminated validated path; link variant does not follow final aliases.
        let acl = unsafe { acl_get_link_np(path.as_ptr(), ACL_EXTENDED) };
        validate_acl_allocation(acl, self.0.uid)?;
        self.validate()?;
        if identity(&stat_at(self.fd(), SOCKET).map_err(identity_error)?) != identity(&stat) {
            return Err(Error::UnsafeRuntime);
        }
        Ok(identity(&stat))
    }
    pub fn acquire_owner(&self) -> Result<OwnerGuard> {
        let lock = self.lock(true)?;
        // SAFETY: nonblocking lock on exclusively owned descriptor; no PID signalling.
        if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            let error = io::Error::last_os_error();
            return Err(if error.raw_os_error() == Some(libc::EWOULDBLOCK) {
                Error::OwnerBusy
            } else {
                error.into()
            });
        }
        self.validate_lock(lock.as_raw_fd())?;
        Ok(OwnerGuard {
            runtime: self.clone(),
            lock: Arc::new(OwnerLock(lock)),
            endpoint: None,
        })
    }
    pub fn capture_lock(&self) -> Result<LockWitness> {
        Ok(LockWitness {
            runtime: self.clone(),
            lock: self.lock(false)?,
        })
    }
    pub fn connect(&self) -> Result<AuthenticatedStream> {
        let before = self.endpoint()?;
        // SAFETY: fixed local stream socket creates one new descriptor.
        let raw = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) };
        if raw < 0 {
            return Err(last_error());
        }
        // SAFETY: successful socket result is exclusively owned here.
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        crate::nonblocking(fd.as_raw_fd())?;
        // SAFETY: only set descriptor inheritance, preserving owned lifetime.
        checked(unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC) })?;
        // SAFETY: sockaddr_un is a plain kernel input record, initialized before call.
        let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
        let path = self.0.path.join("control.sock");
        let bytes = path.as_os_str().as_bytes();
        if bytes.len() >= address.sun_path.len() {
            return Err(Error::UnsafeRuntime);
        }
        address.sun_family = libc::AF_UNIX as libc::sa_family_t;
        address.sun_len = (2 + bytes.len() + 1) as u8;
        for (destination, source) in address.sun_path.iter_mut().zip(bytes) {
            *destination = *source as libc::c_char;
        }
        // SAFETY: complete initialized address of sun_len bytes; socket is already nonblocking.
        checked(unsafe {
            libc::connect(
                fd.as_raw_fd(),
                (&address as *const libc::sockaddr_un).cast(),
                address.sun_len as libc::socklen_t,
            )
        })?;
        let stream = UnixStream::from(fd);
        if self.endpoint()? != before {
            return Err(Error::UnsafeRuntime);
        }
        AuthenticatedStream::from_stream(stream)
    }
}
pub struct LockWitness {
    runtime: RuntimeDirectory,
    lock: OwnedFd,
}
impl LockWitness {
    pub fn validate(&self) -> Result<()> {
        self.runtime.validate_lock(self.lock.as_raw_fd())
    }
    pub fn owner_released(&self) -> Result<bool> {
        self.validate()?;
        // SAFETY: nonblocking probe using retained descriptor; successful probe is immediately released.
        if unsafe { libc::flock(self.lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            checked(unsafe { libc::flock(self.lock.as_raw_fd(), libc::LOCK_UN) })?;
            self.validate()?;
            return Ok(true);
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::EWOULDBLOCK) {
            Ok(false)
        } else {
            Err(error.into())
        }
    }
}
// Shared only by the logical owner and its validation witnesses. File-description
// clones held transiently by a spawning child do not extend intentional ownership.
struct OwnerLock(OwnedFd);
impl AsRawFd for OwnerLock {
    fn as_raw_fd(&self) -> RawFd {
        self.0.as_raw_fd()
    }
}
impl Drop for OwnerLock {
    fn drop(&mut self) {
        // SAFETY: final shared owner retains this descriptor throughout the unlock.
        // LOCK_UN is nonblocking; OwnedFd closes it immediately after this destructor.
        let _ = unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN) };
    }
}
pub struct OwnerGuard {
    runtime: RuntimeDirectory,
    lock: Arc<OwnerLock>,
    endpoint: Option<Identity>,
}
/// Immutable authority witness. Its shared descriptor retains the owner lock.
#[derive(Clone)]
pub struct OwnerValidation {
    runtime: RuntimeDirectory,
    lock: Arc<OwnerLock>,
    endpoint: Option<Identity>,
}
impl OwnerValidation {
    pub fn validate(&self) -> Result<()> {
        self.runtime.validate_lock(self.lock.as_raw_fd())?;
        if let Some(id) = self.endpoint
            && self.runtime.endpoint().map_err(identity_error)? != id
        {
            return Err(Error::UnsafeRuntime);
        }
        Ok(())
    }
}
impl OwnerGuard {
    /// Clones retained descriptors only; no filesystem observation runs here.
    pub fn validation_snapshot(&self) -> OwnerValidation {
        OwnerValidation {
            runtime: self.runtime.clone(),
            lock: self.lock.clone(),
            endpoint: self.endpoint,
        }
    }
    pub fn validate(&self) -> Result<()> {
        self.validation_snapshot().validate()
    }
    pub fn bind(&mut self) -> Result<UnixListener> {
        self.validate()?;
        if self.endpoint.is_some() {
            return Err(Error::Unavailable);
        }
        match self.runtime.endpoint() {
            Ok(_) => {
                self.runtime.validate_lock(self.lock.as_raw_fd())?;
                // SAFETY: the sole lock winner removes only a validated stale socket.
                checked(unsafe { libc::unlinkat(self.runtime.fd(), SOCKET.as_ptr(), 0) })?;
            }
            Err(Error::Absent) => (),
            Err(error) => return Err(error),
        }
        // The retained 0700 parent prevents another user reaching the newly
        // created socket before its final mode is set. Never alter umask here:
        // it is process-global and would affect unrelated worker file creation.
        let listener = UnixListener::bind(self.runtime.0.path.join("control.sock"))?;
        self.runtime.validate_lock(self.lock.as_raw_fd())?;
        let created = stat_at(self.runtime.fd(), SOCKET)?;
        if created.st_uid != self.runtime.0.uid
            || created.st_mode & libc::S_IFMT != libc::S_IFSOCK
            || created.st_nlink != 1
        {
            return Err(Error::UnsafeRuntime);
        }
        let created_identity = identity(&created);
        // SAFETY: retained validated private directory and fixed terminated name.
        // Only the socket just created above is chmodded; symlinks are not followed.
        checked(unsafe {
            libc::fchmodat(
                self.runtime.fd(),
                SOCKET.as_ptr(),
                0o600,
                libc::AT_SYMLINK_NOFOLLOW,
            )
        })?;
        if self.runtime.endpoint()? != created_identity {
            return Err(Error::UnsafeRuntime);
        }
        listener.set_nonblocking(true)?;
        self.endpoint = Some(self.runtime.endpoint()?);
        self.validate()?;
        Ok(listener)
    }
    pub fn remove_endpoint(&mut self) -> Result<()> {
        self.validate()?;
        if self.endpoint.is_some() {
            // SAFETY: validate checked named endpoint against our recorded identity.
            checked(unsafe { libc::unlinkat(self.runtime.fd(), SOCKET.as_ptr(), 0) })?;
            self.endpoint = None;
        }
        Ok(())
    }
}
pub struct AuthenticatedStream(UnixStream);
impl AuthenticatedStream {
    pub fn from_stream(stream: UnixStream) -> Result<Self> {
        let expected = effective_uid()?;
        let mut uid = 0;
        let mut gid = 0;
        // SAFETY: socket fd is retained and both credential outputs are writable.
        checked(unsafe { libc::getpeereid(stream.as_raw_fd(), &mut uid, &mut gid) })?;
        if uid != expected {
            return Err(Error::UnsafeRuntime);
        }
        stream.set_nonblocking(true)?;
        Ok(Self(stream))
    }
    pub fn stream(&self) -> &UnixStream {
        &self.0
    }
    pub fn stream_mut(&mut self) -> &mut UnixStream {
        &mut self.0
    }
    pub fn into_stream(self) -> UnixStream {
        self.0
    }
}

/// Resolve the effective account home independently of environment overrides.
pub fn account_home_path() -> Result<PathBuf> {
    account_home(effective_uid()?)
}

/// Open a caller-selected private append file without following final aliases.
pub fn open_private_append(directory: &Path, filename: &str) -> Result<File> {
    if filename.is_empty()
        || filename.len() > 255
        || filename.contains('/')
        || matches!(filename, "." | "..")
    {
        return Err(Error::UnsafeRuntime);
    }
    let filename = CString::new(filename).map_err(|_| Error::UnsafeRuntime)?;
    let uid = effective_uid()?;
    DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(directory)?;
    let name = CString::new(directory.as_os_str().as_bytes()).map_err(|_| Error::UnsafeRuntime)?;
    let parent = open_at(libc::AT_FDCWD, &name, libc::O_RDONLY | libc::O_DIRECTORY, 0)?;
    let parent_stat = stat_fd(parent.as_raw_fd())?;
    valid_stat(&parent_stat, uid, libc::S_IFDIR, None)?;
    let file = open_at(
        parent.as_raw_fd(),
        &filename,
        libc::O_WRONLY | libc::O_APPEND | libc::O_CREAT,
        0o600,
    )?;
    let held = stat_fd(file.as_raw_fd())?;
    valid_stat(&held, uid, libc::S_IFREG, Some(0o600))?;
    let named = stat_at(parent.as_raw_fd(), &filename).map_err(identity_error)?;
    let parent_named = stat_at(libc::AT_FDCWD, &name).map_err(identity_error)?;
    if identity(&held) != identity(&named) || identity(&parent_stat) != identity(&parent_named) {
        return Err(Error::UnsafeRuntime);
    }
    valid_stat(&named, uid, libc::S_IFREG, Some(0o600))?;
    valid_stat(&parent_named, uid, libc::S_IFDIR, None)?;
    Ok(File::from(file))
}

#[cfg(test)]
mod owner_snapshot_tests {
    use super::*;
    use std::fs;
    struct Scratch(PathBuf);
    impl Scratch {
        fn new() -> Self {
            let path = PathBuf::from(format!(
                "/private/tmp/asura-witness-{:x}",
                u128::from_ne_bytes(crate::random_id())
            ));
            DirBuilder::new().mode(0o700).create(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    #[test]
    fn snapshot_pins_lock_and_rejects_replaced_endpoint() {
        let root = Scratch::new();
        let runtime = RuntimeDirectory::scratch(&root.0, true).unwrap();
        let mut owner = runtime.acquire_owner().unwrap();
        let listener = owner.bind().unwrap();
        let snapshot = owner.validation_snapshot();
        snapshot.validate().unwrap();
        drop(owner);
        assert!(matches!(runtime.acquire_owner(), Err(Error::OwnerBusy)));
        fs::remove_file(root.0.join(".asura/run/control.sock")).unwrap();
        assert!(snapshot.validate().is_err());
        drop(snapshot);
        assert!(runtime.acquire_owner().is_ok());
        drop(listener);
    }
    #[test]
    fn snapshot_preserves_runtime_chain_validation() {
        let root = Scratch::new();
        let runtime = RuntimeDirectory::scratch(&root.0, true).unwrap();
        let owner = runtime.acquire_owner().unwrap();
        let snapshot = owner.validation_snapshot();
        fs::rename(root.0.join(".asura/run"), root.0.join(".asura/moved")).unwrap();
        assert!(snapshot.validate().is_err());
    }
    #[test]
    fn model_assets_path_uses_retained_runtime_root_without_reopening_it() {
        let root = Scratch::new();
        let runtime = RuntimeDirectory::scratch(&root.0, true).unwrap();
        let expected = root.0.join(".asura/data/models");
        assert_eq!(runtime.model_assets_path(), expected);
        assert!(!expected.exists(), "projection must not create directories");
        fs::rename(root.0.join(".asura/run"), root.0.join(".asura/moved")).unwrap();
        assert_eq!(runtime.model_assets_path(), expected);
    }
    #[test]
    fn final_witness_unlocks_even_with_inherited_open_description() {
        let root = Scratch::new();
        let runtime = RuntimeDirectory::scratch(&root.0, true).unwrap();
        let owner = runtime.acquire_owner().unwrap();
        // dup shares the exact open-file description, as inheritance before exec does.
        let inherited = owner.lock.0.try_clone().unwrap();
        let snapshot = owner.validation_snapshot();
        let witness = runtime.capture_lock().unwrap();
        drop(owner);
        assert!(
            !witness.owner_released().unwrap(),
            "validation witness must retain authority"
        );
        snapshot.validate().unwrap();
        drop(snapshot);
        assert!(
            witness.owner_released().unwrap(),
            "final logical owner explicitly releases shared description"
        );
        let next = runtime.acquire_owner().unwrap();
        drop(inherited);
        assert!(
            !witness.owner_released().unwrap(),
            "old description cannot unlock new owner"
        );
        drop(next);
        assert!(witness.owner_released().unwrap());
    }
}
