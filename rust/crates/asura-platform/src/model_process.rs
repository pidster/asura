//! Owned native model process. Blocking preparation runs only on its caller's worker.
use crate::startup::{SpawnActions, SpawnAttributes, duplicate_for_spawn, spawn_result};
use crate::{Error, Result, RuntimeDirectory, checked, last_error, nonblocking};
use sha2::{Digest, Sha256};
use std::{
    ffi::{CStr, CString},
    fs::File,
    io::{Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd, RawFd},
        unix::{ffi::OsStrExt, fs::MetadataExt, net::UnixStream},
    },
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
};

/// The SwiftPM resource bundle that `swift-transformers` `Hub` reads through
/// `Bundle.module` (BN1). It must sit beside the running helper.
pub const HUB_BUNDLE_NAME: &str = "swift-transformers_Hub.bundle";
const HUB_BUNDLE_FILES: usize = 16;
const HUB_BUNDLE_DIRECTORIES: usize = 16;
const HUB_BUNDLE_DEPTH: usize = 4;
const HUB_BUNDLE_FILE_BYTES: u64 = 1024 * 1024;
const HUB_BUNDLE_TOTAL_BYTES: u64 = 1024 * 1024;

/// The one directory that holds `mlx.metallib` and the Hub bundle for
/// `helper` (release signing, SG1). Inside `<name>.app/Contents/MacOS/` it is
/// `<name>.app/Contents/Resources`; for any other layout (development package,
/// test binaries) it is the helper's own directory. Only the path is
/// inspected: there is no search and no second location.
pub fn package_resource_directory(helper: &Path) -> PathBuf {
    let directory = helper.parent().unwrap_or(Path::new(""));
    let contents = directory
        .parent()
        .filter(|_| directory.file_name() == Some("MacOS".as_ref()));
    let bundle = contents
        .filter(|contents| contents.file_name() == Some("Contents".as_ref()))
        .and_then(Path::parent)
        .filter(|bundle| bundle.extension() == Some("app".as_ref()));
    match (contents, bundle) {
        (Some(contents), Some(_)) => contents.join("Resources"),
        _ => directory.to_path_buf(),
    }
}

/// Package resources copied beside the helper. An absent digest copies nothing.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PackageResources {
    /// `mlx.metallib` SHA-256 (MLX selections).
    pub metallib: Option<[u8; 32]>,
    /// `hub_bundle_sha256` (MLX and Core AI selections).
    pub hub_bundle: Option<[u8; 32]>,
}

struct PinnedCopy {
    directory: OwnedFd,
    name: CString,
    file: File,
}
impl PinnedCopy {
    /// The retained name still refers to the retained inode, unchanged.
    fn still_named(&self) -> Result<bool> {
        let named = stat_at(self.directory.as_raw_fd(), &self.name)?;
        let held = self.file.metadata()?;
        Ok(fingerprint(&held)
            == (
                named.st_dev as u64,
                named.st_ino,
                named.st_size as u64,
                named.st_mtime,
                named.st_mtime_nsec,
                named.st_ctime,
                named.st_ctime_nsec,
            ))
    }
}
impl Drop for PinnedCopy {
    fn drop(&mut self) {
        // SAFETY: inspect the name without following links and compare the retained inode.
        let mut named: libc::stat = unsafe { std::mem::zeroed() };
        if unsafe {
            libc::fstatat(
                self.directory.as_raw_fd(),
                self.name.as_ptr(),
                &mut named,
                libc::AT_SYMLINK_NOFOLLOW,
            )
        } == 0
            && self
                .file
                .metadata()
                .is_ok_and(|held| held.dev() == named.st_dev as u64 && held.ino() == named.st_ino)
        {
            // SAFETY: unlink only the still-owned name in the retained private directory.
            unsafe {
                libc::unlinkat(self.directory.as_raw_fd(), self.name.as_ptr(), 0);
            }
        }
    }
}

fn fingerprint(m: &std::fs::Metadata) -> (u64, u64, u64, i64, i64, i64, i64) {
    (
        m.dev(),
        m.ino(),
        m.len(),
        m.mtime(),
        m.mtime_nsec(),
        m.ctime(),
        m.ctime_nsec(),
    )
}

/// No-follow `fstatat` of one name in a retained directory.
fn stat_at(directory: RawFd, name: &CStr) -> Result<libc::stat> {
    // SAFETY: zeroed plain-data output; terminated name; no link following.
    let mut named: libc::stat = unsafe { std::mem::zeroed() };
    checked(unsafe {
        libc::fstatat(
            directory,
            name.as_ptr(),
            &mut named,
            libc::AT_SYMLINK_NOFOLLOW,
        )
    })?;
    Ok(named)
}

/// Open one source name read-only without following a final symlink.
fn open_source(directory: RawFd, name: &CStr) -> Result<File> {
    // SAFETY: terminated name and no-follow final component; successful FD is newly owned.
    let raw = unsafe {
        libc::openat(
            directory,
            name.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
        )
    };
    if raw < 0 {
        return Err(last_error());
    }
    // SAFETY: openat returned a new exclusively owned descriptor.
    Ok(unsafe { File::from_raw_fd(raw) })
}

#[allow(clippy::too_many_arguments)]
fn verified_copy(
    directory: OwnedFd,
    name: CString,
    source_path: &Path,
    expected: [u8; 32],
    uid: libc::uid_t,
    executable: bool,
    maximum_bytes: u64,
    check: &impl Fn() -> Result<()>,
) -> Result<PinnedCopy> {
    let source_path =
        CString::new(source_path.as_os_str().as_bytes()).map_err(|_| Error::Unavailable)?;
    let source = open_source(libc::AT_FDCWD, &source_path)?;
    let (pinned, digest) = pinned_copy(
        directory,
        name,
        source,
        uid,
        executable,
        false,
        maximum_bytes,
        check,
    )?;
    if digest != expected {
        return Err(Error::UnsafeRuntime);
    }
    Ok(pinned)
}

/// Copy one validated source into an exclusive new name and return the
/// SHA-256 of the re-read copy. The source must stay unchanged during the copy.
#[allow(clippy::too_many_arguments)]
fn pinned_copy(
    directory: OwnedFd,
    name: CString,
    mut source: File,
    uid: libc::uid_t,
    executable: bool,
    allow_empty: bool,
    maximum_bytes: u64,
    check: &impl Fn() -> Result<()>,
) -> Result<(PinnedCopy, [u8; 32])> {
    let before = source.metadata()?;
    if !before.is_file()
        || before.uid() != uid
        || before.mode() & 0o022 != 0
        || (executable && before.mode() & 0o111 == 0)
        || (!allow_empty && before.len() == 0)
        || before.len() > maximum_bytes
    {
        return Err(Error::UnsafeRuntime);
    }
    // SAFETY: create one exclusive file under validated retained directory, never follow links.
    let raw = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDWR | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            if executable { 0o700 } else { 0o600 } as libc::c_uint,
        )
    };
    if raw < 0 {
        return Err(last_error());
    }
    let mut pinned = PinnedCopy {
        directory,
        name,
        file: unsafe { File::from_raw_fd(raw) },
    };
    let mut hasher = Sha256::new();
    let mut total = 0u64;
    let mut buffer = [0u8; 65536];
    loop {
        check()?;
        let n = source.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        total += n as u64;
        if total > before.len() {
            return Err(Error::UnsafeRuntime);
        }
        hasher.update(&buffer[..n]);
        pinned.file.write_all(&buffer[..n])?;
    }
    let source_digest = <[u8; 32]>::from(hasher.finalize());
    if total != before.len() || fingerprint(&source.metadata()?) != fingerprint(&before) {
        return Err(Error::UnsafeRuntime);
    }
    // Re-read the copied inode, rather than trusting the bytes supplied to write.
    use std::io::{Seek, SeekFrom};
    pinned.file.seek(SeekFrom::Start(0))?;
    let mut copied = Sha256::new();
    loop {
        check()?;
        let n = pinned.file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        copied.update(&buffer[..n]);
    }
    if <[u8; 32]>::from(copied.finalize()) != source_digest {
        return Err(Error::UnsafeRuntime);
    }
    // Exec must not retain a writable handle to its text image. Reopen the
    // verified inode read-only and verify its identity before dropping the writer.
    let executable_handle = open_source(pinned.directory.as_raw_fd(), &pinned.name)?;
    if fingerprint(&executable_handle.metadata()?) != fingerprint(&pinned.file.metadata()?) {
        return Err(Error::UnsafeRuntime);
    }
    pinned.file = executable_handle;
    Ok((pinned, source_digest))
}

struct PackageDirectory {
    parent: OwnedFd,
    name: CString,
    directory: OwnedFd,
}
impl PackageDirectory {
    /// Create one exclusive private directory under a retained parent.
    fn create(parent: OwnedFd, name: CString) -> Result<Self> {
        // SAFETY: create one exclusive directory under a retained private parent.
        checked(unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), 0o700) })?;
        // SAFETY: open only the new directory, without following a replacement symlink.
        let raw = unsafe {
            libc::openat(
                parent.as_raw_fd(),
                name.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if raw < 0 {
            let error = last_error();
            // SAFETY: remove the just-created name only if it is still an empty directory.
            unsafe {
                libc::unlinkat(parent.as_raw_fd(), name.as_ptr(), libc::AT_REMOVEDIR);
            }
            return Err(error);
        }
        Ok(Self {
            parent,
            name,
            // SAFETY: successful open returns a newly owned descriptor.
            directory: unsafe { OwnedFd::from_raw_fd(raw) },
        })
    }
    /// The retained name still refers to the retained directory.
    fn still_named(&self) -> Result<bool> {
        let named = stat_at(self.parent.as_raw_fd(), &self.name)?;
        let held = File::from(self.directory.try_clone()?).metadata()?;
        Ok(held.dev() == named.st_dev as u64 && held.ino() == named.st_ino)
    }
}
impl Drop for PackageDirectory {
    fn drop(&mut self) {
        // SAFETY: compare held directory identity to its no-follow parent entry.
        let mut held: libc::stat = unsafe { std::mem::zeroed() };
        let mut named: libc::stat = unsafe { std::mem::zeroed() };
        if unsafe { libc::fstat(self.directory.as_raw_fd(), &mut held) } == 0
            && unsafe {
                libc::fstatat(
                    self.parent.as_raw_fd(),
                    self.name.as_ptr(),
                    &mut named,
                    libc::AT_SYMLINK_NOFOLLOW,
                )
            } == 0
            && held.st_dev == named.st_dev
            && held.st_ino == named.st_ino
        {
            // SAFETY: remove matching directory only if empty; unknown entries survive.
            unsafe {
                libc::unlinkat(
                    self.parent.as_raw_fd(),
                    self.name.as_ptr(),
                    libc::AT_REMOVEDIR,
                );
            }
        }
    }
}

/// A verified copy of a resource bundle. Dropping it removes owned files
/// first, then owned directories deepest first; unknown entries survive.
struct PinnedTree {
    files: Vec<PinnedCopy>,
    directories: Vec<PackageDirectory>,
}
impl PinnedTree {
    fn still_named(&self) -> Result<bool> {
        for directory in &self.directories {
            if !directory.still_named()? {
                return Ok(false);
            }
        }
        for file in &self.files {
            if !file.still_named()? {
                return Ok(false);
            }
        }
        Ok(true)
    }
}
impl Drop for PinnedTree {
    fn drop(&mut self) {
        self.files.clear();
        while let Some(directory) = self.directories.pop() {
            drop(directory);
        }
    }
}

/// The `hub_bundle_sha256` encoding: SHA-256 over one line per file, sorted by
/// relative path bytes: 64 lowercase hex digits, two spaces, the path, a line
/// feed. These bytes equal `shasum -a 256` output over the sorted paths.
pub fn bundle_digest(files: &mut [(String, [u8; 32])]) -> [u8; 32] {
    files.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
    let mut digest = Sha256::new();
    for (path, hash) in files.iter() {
        for byte in hash {
            digest.update(format!("{byte:02x}").as_bytes());
        }
        digest.update(b"  ");
        digest.update(path.as_bytes());
        digest.update(b"\n");
    }
    digest.finalize().into()
}

/// List one directory's names through a duplicate of its retained descriptor.
/// At most `limit` names are accepted; more fail the walk.
fn directory_names(directory: &OwnedFd, limit: usize) -> Result<Vec<CString>> {
    let listed = directory.try_clone()?;
    // SAFETY: fdopendir takes ownership of the duplicated descriptor on success.
    let raw = listed.into_raw_fd();
    let stream = unsafe { libc::fdopendir(raw) };
    if stream.is_null() {
        let error = last_error();
        // SAFETY: fdopendir failed, so the duplicate is still ours to close.
        drop(unsafe { OwnedFd::from_raw_fd(raw) });
        return Err(error);
    }
    struct Stream(*mut libc::DIR);
    impl Drop for Stream {
        fn drop(&mut self) {
            // SAFETY: close the stream (and its descriptor) exactly once.
            unsafe {
                libc::closedir(self.0);
            }
        }
    }
    let stream = Stream(stream);
    // SAFETY: rewind the shared offset so a reused descriptor lists every entry.
    unsafe { libc::rewinddir(stream.0) };
    let mut names = Vec::new();
    loop {
        // SAFETY: errno is thread-local; clear it to tell the end from an error.
        unsafe { *libc::__error() = 0 };
        // SAFETY: a valid open stream; the entry stays valid until the next call.
        let entry = unsafe { libc::readdir(stream.0) };
        if entry.is_null() {
            if unsafe { *libc::__error() } != 0 {
                return Err(last_error());
            }
            return Ok(names);
        }
        // SAFETY: d_name is a terminated array inside the live entry.
        let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) };
        if name.to_bytes() == b"." || name.to_bytes() == b".." {
            continue;
        }
        if names.len() == limit {
            return Err(Error::UnsafeRuntime);
        }
        names.push(name.to_owned());
    }
}

/// Copy the resource bundle `source` (a directory path) into `package` as
/// `HUB_BUNDLE_NAME`, within the BN1 limits, and require its digest.
fn copy_bundle(
    package: &OwnedFd,
    source: &Path,
    expected: [u8; 32],
    uid: libc::uid_t,
    check: &impl Fn() -> Result<()>,
) -> Result<PinnedTree> {
    let open_directory = |parent: RawFd, name: &CStr| -> Result<OwnedFd> {
        // SAFETY: terminated name, no-follow, directory only; successful FD is newly owned.
        let raw = unsafe {
            libc::openat(
                parent,
                name.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if raw < 0 {
            return Err(last_error());
        }
        // SAFETY: openat returned a new exclusively owned descriptor.
        let directory = unsafe { OwnedFd::from_raw_fd(raw) };
        let held = File::from(directory.try_clone()?).metadata()?;
        if !held.is_dir() || held.uid() != uid || held.mode() & 0o022 != 0 {
            return Err(Error::UnsafeRuntime);
        }
        Ok(directory)
    };
    check()?;
    let source_path =
        CString::new(source.as_os_str().as_bytes()).map_err(|_| Error::Unavailable)?;
    let root = open_directory(libc::AT_FDCWD, &source_path)?;
    let mut tree = PinnedTree {
        files: Vec::new(),
        directories: vec![PackageDirectory::create(
            package.try_clone()?,
            CString::new(HUB_BUNDLE_NAME).unwrap(),
        )?],
    };
    let mut hashes = Vec::new();
    let mut total = 0u64;
    let mut directories = 0usize;
    // (source directory, index of its copy in tree.directories, relative prefix, depth)
    let mut pending = vec![(root, 0usize, String::new(), 0usize)];
    while let Some((source_directory, copy, prefix, depth)) = pending.pop() {
        check()?;
        let names = directory_names(&source_directory, HUB_BUNDLE_FILES + HUB_BUNDLE_DIRECTORIES)?;
        for name in names {
            check()?;
            let text = name
                .to_str()
                .ok()
                .filter(|text| !text.chars().any(|c| c.is_control() || c == '\\'))
                .ok_or(Error::UnsafeRuntime)?;
            let path = format!("{prefix}{text}");
            if depth + 1 > HUB_BUNDLE_DEPTH {
                return Err(Error::UnsafeRuntime);
            }
            let named = stat_at(source_directory.as_raw_fd(), &name)?;
            match named.st_mode & libc::S_IFMT {
                libc::S_IFDIR => {
                    directories += 1;
                    if directories > HUB_BUNDLE_DIRECTORIES {
                        return Err(Error::UnsafeRuntime);
                    }
                    let child = open_directory(source_directory.as_raw_fd(), &name)?;
                    let held = File::from(child.try_clone()?).metadata()?;
                    if held.dev() != named.st_dev as u64 || held.ino() != named.st_ino {
                        return Err(Error::UnsafeRuntime);
                    }
                    let parent = tree.directories[copy].directory.try_clone()?;
                    tree.directories
                        .push(PackageDirectory::create(parent, name)?);
                    pending.push((
                        child,
                        tree.directories.len() - 1,
                        format!("{path}/"),
                        depth + 1,
                    ));
                }
                libc::S_IFREG => {
                    if hashes.len() == HUB_BUNDLE_FILES {
                        return Err(Error::UnsafeRuntime);
                    }
                    let file = open_source(source_directory.as_raw_fd(), &name)?;
                    let held = file.metadata()?;
                    if held.dev() != named.st_dev as u64 || held.ino() != named.st_ino {
                        return Err(Error::UnsafeRuntime);
                    }
                    total += held.len();
                    if total > HUB_BUNDLE_TOTAL_BYTES {
                        return Err(Error::UnsafeRuntime);
                    }
                    let (pinned, hash) = pinned_copy(
                        tree.directories[copy].directory.try_clone()?,
                        name,
                        file,
                        uid,
                        false,
                        true,
                        HUB_BUNDLE_FILE_BYTES,
                        check,
                    )?;
                    tree.files.push(pinned);
                    hashes.push((path, hash));
                }
                _ => return Err(Error::UnsafeRuntime),
            }
        }
    }
    if bundle_digest(&mut hashes) != expected {
        return Err(Error::UnsafeRuntime);
    }
    Ok(tree)
}

pub struct ModelProcess {
    pid: libc::pid_t,
    reaped: bool,
    pub channel: UnixStream,
    diagnostics: File,
    diagnostic_tail: Vec<u8>,
    diagnostics_closed: bool,
    pub discarded_diagnostic_bytes: u64,
    _copy: PinnedCopy,
    _resource: Option<PinnedCopy>,
    _bundle: Option<PinnedTree>,
    _package: Option<PackageDirectory>,
}
impl ModelProcess {
    /// Called only from the single bounded preparation worker. `helper` is the fixed package sibling.
    /// Preparation has no time bound (owner, 2026-10-16): it ends on
    /// completion, a verification or spawn failure, or `cancelled`.
    pub fn spawn(
        runtime: &RuntimeDirectory,
        helper: &Path,
        expected: [u8; 32],
        cancelled: &AtomicBool,
    ) -> Result<Self> {
        Self::spawn_with_resources(
            runtime,
            helper,
            expected,
            PackageResources::default(),
            cancelled,
        )
    }

    /// MLX requires `mlx.metallib`; MLX and Core AI require the Hub bundle.
    /// Each resource is read from [`package_resource_directory`] of `helper`
    /// and copied beside the helper copy in one private package directory.
    pub fn spawn_with_resources(
        runtime: &RuntimeDirectory,
        helper: &Path,
        expected: [u8; 32],
        resources: PackageResources,
        cancelled: &AtomicBool,
    ) -> Result<Self> {
        // Cancellation is reported as `Error::Deadline` (stable mapping).
        let check = || {
            if cancelled.load(Ordering::Acquire) {
                Err(Error::Deadline)
            } else {
                Ok(())
            }
        };
        check()?;
        let (directory, root, uid) = runtime.model_root()?;
        let suffix: String = crate::random_id()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let name = CString::new(format!("model-{suffix}")).unwrap();
        let mut package = None;
        let (directory, root, name) =
            if resources.metallib.is_some() || resources.hub_bundle.is_some() {
                let child_root = root.join(name.to_str().unwrap());
                let created = PackageDirectory::create(directory, name)?;
                let copied = created.directory.try_clone()?;
                package = Some(created);
                (copied, child_root, CString::new("asura-model").unwrap())
            } else {
                (directory, root, name)
            };
        let pinned = verified_copy(
            directory,
            name,
            helper,
            expected,
            uid,
            true,
            128 * 1024 * 1024,
            &check,
        )?;
        let resource_directory = package_resource_directory(helper);
        let resource = if let Some(digest) = resources.metallib {
            Some(verified_copy(
                pinned.directory.try_clone()?,
                CString::new("mlx.metallib").unwrap(),
                &resource_directory.join("mlx.metallib"),
                digest,
                uid,
                false,
                64 * 1024 * 1024,
                &check,
            )?)
        } else {
            None
        };
        let bundle = if let Some(digest) = resources.hub_bundle {
            Some(copy_bundle(
                &pinned.directory,
                &resource_directory.join(HUB_BUNDLE_NAME),
                digest,
                uid,
                &check,
            )?)
        } else {
            None
        };
        let executable = root.join(pinned.name.to_str().unwrap());
        let path =
            CString::new(executable.as_os_str().as_bytes()).map_err(|_| Error::Unavailable)?;
        let (channel, child_channel) = UnixStream::pair()?;
        channel.set_nonblocking(true)?;
        let no_sigpipe: libc::c_int = 1;
        // SAFETY: a valid owned socket and initialized integer option of the stated size.
        checked(unsafe {
            libc::setsockopt(
                channel.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_NOSIGPIPE,
                (&no_sigpipe as *const libc::c_int).cast(),
                std::mem::size_of_val(&no_sigpipe) as libc::socklen_t,
            )
        })?;
        let socket = duplicate_for_spawn(child_channel.as_raw_fd())?;
        let (read, write) = crate::startup::pipe()?;
        nonblocking(read.as_raw_fd())?;
        let output = duplicate_for_spawn(write.as_raw_fd())?;
        // SAFETY: opaque actions are initialized before use or destruction.
        let mut actions = unsafe { std::mem::zeroed() };
        spawn_result(unsafe { libc::posix_spawn_file_actions_init(&mut actions) })?;
        let mut actions = SpawnActions(actions);
        // SAFETY: retained descriptors and terminated strings outlive spawn; CLOEXEC closes all others.
        unsafe {
            spawn_result(libc::posix_spawn_file_actions_addopen(
                &mut actions.0,
                0,
                c"/dev/null".as_ptr(),
                libc::O_RDONLY,
                0,
            ))?;
            for fd in [1, 2] {
                spawn_result(libc::posix_spawn_file_actions_adddup2(
                    &mut actions.0,
                    output.as_raw_fd(),
                    fd,
                ))?;
            }
            spawn_result(libc::posix_spawn_file_actions_adddup2(
                &mut actions.0,
                socket.as_raw_fd(),
                3,
            ))?;
            spawn_result(libc::posix_spawn_file_actions_addclose(
                &mut actions.0,
                output.as_raw_fd(),
            ))?;
            spawn_result(libc::posix_spawn_file_actions_addclose(
                &mut actions.0,
                socket.as_raw_fd(),
            ))?;
        }
        // SAFETY: initialize opaque spawn attributes before use or destruction.
        let mut attrs = unsafe { std::mem::zeroed() };
        spawn_result(unsafe { libc::posix_spawnattr_init(&mut attrs) })?;
        let mut attrs = SpawnAttributes(attrs);
        spawn_result(unsafe {
            libc::posix_spawnattr_setflags(&mut attrs.0, libc::POSIX_SPAWN_CLOEXEC_DEFAULT as i16)
        })?;
        runtime.validate()?;
        check()?;
        if fingerprint(&std::fs::symlink_metadata(&executable)?)
            != fingerprint(&pinned.file.metadata()?)
        {
            return Err(Error::UnsafeRuntime);
        }
        if let Some(resource) = &resource
            && fingerprint(&std::fs::symlink_metadata(root.join("mlx.metallib"))?)
                != fingerprint(&resource.file.metadata()?)
        {
            return Err(Error::UnsafeRuntime);
        }
        if let Some(bundle) = &bundle
            && !bundle.still_named()?
        {
            return Err(Error::UnsafeRuntime);
        }
        let mut argv = [path.as_ptr().cast_mut(), std::ptr::null_mut()];
        let mut env = [
            c"LANG=C".as_ptr().cast_mut(),
            c"LC_ALL=C".as_ptr().cast_mut(),
            std::ptr::null_mut(),
        ];
        let mut pid = 0;
        // SAFETY: all pointers refer to live initialized spawn state and null-terminated arrays.
        spawn_result(unsafe {
            libc::posix_spawn(
                &mut pid,
                path.as_ptr(),
                &actions.0,
                &attrs.0,
                argv.as_mut_ptr(),
                env.as_mut_ptr(),
            )
        })?;
        Ok(Self {
            pid,
            reaped: false,
            channel,
            diagnostics: File::from(read),
            diagnostic_tail: Vec::new(),
            diagnostics_closed: false,
            discarded_diagnostic_bytes: 0,
            _copy: pinned,
            _resource: resource,
            _bundle: bundle,
            _package: package,
        })
    }
    pub fn try_reap(&mut self) -> Result<bool> {
        if self.reaped {
            return Ok(true);
        }
        let mut status = 0;
        // SAFETY: wait only on our unreaped exact child, without blocking.
        let result = unsafe { libc::waitpid(self.pid, &mut status, libc::WNOHANG) };
        if result == self.pid {
            self.reaped = true;
            Ok(true)
        } else if result == 0 {
            Ok(false)
        } else {
            Err(last_error())
        }
    }
    pub fn terminate(&self, force: bool) -> Result<()> {
        if self.reaped {
            return Ok(());
        }
        // SAFETY: an unreaped child PID cannot be recycled; never signal an externally discovered PID.
        let result =
            unsafe { libc::kill(self.pid, if force { libc::SIGKILL } else { libc::SIGTERM }) };
        if result < 0 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
            return Ok(());
        }
        checked(result)
    }
    pub fn diagnostics_fd(&self) -> Option<std::os::fd::RawFd> {
        (!self.diagnostics_closed).then(|| self.diagnostics.as_raw_fd())
    }
    /// Consume complete private failure records, never vendor descriptions.
    pub fn take_failure_diagnostics(&mut self) -> Vec<(&'static str, i64)> {
        let Some(end) = self.diagnostic_tail.iter().rposition(|byte| *byte == b'\n') else {
            return Vec::new();
        };
        let results = self.diagnostic_tail[..=end]
            .split(|byte| *byte == b'\n')
            .filter_map(parse_failure_diagnostic)
            .take(2)
            .collect();
        self.diagnostic_tail.drain(..=end);
        results
    }
    pub fn poll_diagnostics(&mut self) -> Result<()> {
        let mut remaining = 65536;
        let mut bytes = [0u8; 4096];
        while remaining > 0 {
            match self.diagnostics.read(&mut bytes) {
                Ok(0) => {
                    self.diagnostics_closed = true;
                    break;
                }
                Ok(n) => {
                    remaining -= n;
                    self.diagnostic_tail.extend_from_slice(&bytes[..n]);
                    if self.diagnostic_tail.len() > 65536 {
                        let excess = self.diagnostic_tail.len() - 65536;
                        self.diagnostic_tail.drain(..excess);
                        self.discarded_diagnostic_bytes += excess as u64;
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => break,
                Err(e) => return Err(e.into()),
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        os::unix::fs::PermissionsExt,
        path::PathBuf,
        time::{Duration, Instant},
    };
    struct Fixture {
        root: PathBuf,
        runtime: RuntimeDirectory,
        helper: PathBuf,
        digest: [u8; 32],
    }
    impl Fixture {
        fn new(script: &[u8]) -> Self {
            let name: String = crate::random_id()
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect();
            let root = PathBuf::from(format!("/private/tmp/asura-model-{}", &name[..12]));
            std::fs::create_dir(&root).unwrap();
            std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
            let runtime = RuntimeDirectory::scratch(&root, true).unwrap();
            let helper = root.join("asura-model");
            std::fs::write(&helper, script).unwrap();
            std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o700)).unwrap();
            Self {
                root,
                runtime,
                helper,
                digest: Sha256::digest(script).into(),
            }
        }
        fn spawn(&self) -> ModelProcess {
            ModelProcess::spawn(
                &self.runtime,
                &self.helper,
                self.digest,
                &AtomicBool::new(false),
            )
            .unwrap()
        }
        fn copies(&self) -> usize {
            std::fs::read_dir(self.root.join(".asura/run"))
                .unwrap()
                .filter_map(|v| v.ok())
                .filter(|v| v.file_name().to_string_lossy().starts_with("model-"))
                .count()
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }
    struct OwnedTestChild(Option<ModelProcess>);
    impl Drop for OwnedTestChild {
        fn drop(&mut self) {
            if let Some(child) = self.0.as_mut() {
                let _ = child.terminate(true);
                let deadline = Instant::now() + Duration::from_secs(2);
                while Instant::now() < deadline {
                    if child.try_reap().unwrap_or(false) {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(5));
                }
                panic!("test helper did not reap");
            }
        }
    }
    #[test]
    fn resource_is_verified_colocated_and_removed_after_reap() {
        let fixture = Fixture::new(b"#!/bin/sh\n/bin/cat \"${0%/*}/mlx.metallib\"\n");
        let resource = b"verified-metal-resource";
        std::fs::write(fixture.helper.with_file_name("mlx.metallib"), resource).unwrap();
        let child = ModelProcess::spawn_with_resources(
            &fixture.runtime,
            &fixture.helper,
            fixture.digest,
            metal(Sha256::digest(resource).into()),
            &AtomicBool::new(false),
        )
        .unwrap();
        let mut owned = OwnedTestChild(Some(child));
        assert_eq!(fixture.copies(), 1);
        let deadline = Instant::now() + Duration::from_secs(2);
        while !owned.0.as_mut().unwrap().try_reap().unwrap() {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        }
        owned.0.as_mut().unwrap().poll_diagnostics().unwrap();
        assert_eq!(owned.0.as_ref().unwrap().diagnostic_tail, resource);
        owned.0.take();
        assert_eq!(fixture.copies(), 0);
    }
    #[test]
    fn missing_or_tampered_resource_never_spawns_and_cleans_owned_copies() {
        let fixture = Fixture::new(b"#!/bin/sh\nexit 0\n");
        for present in [false, true] {
            if present {
                std::fs::write(fixture.helper.with_file_name("mlx.metallib"), b"tampered").unwrap();
            }
            assert!(
                ModelProcess::spawn_with_resources(
                    &fixture.runtime,
                    &fixture.helper,
                    fixture.digest,
                    metal([0; 32]),
                    &AtomicBool::new(false)
                )
                .is_err()
            );
            assert_eq!(fixture.copies(), 0);
        }
    }
    #[test]
    fn absent_mlx_resource_does_not_disable_executable_only_providers() {
        let fixture = Fixture::new(b"#!/bin/sh\nexit 0\n");
        assert!(
            ModelProcess::spawn_with_resources(
                &fixture.runtime,
                &fixture.helper,
                fixture.digest,
                metal([0; 32]),
                &AtomicBool::new(false),
            )
            .is_err()
        );
        assert_eq!(fixture.copies(), 0);
        let child = ModelProcess::spawn_with_resources(
            &fixture.runtime,
            &fixture.helper,
            fixture.digest,
            PackageResources::default(),
            &AtomicBool::new(false),
        )
        .unwrap();
        let mut owned = OwnedTestChild(Some(child));
        let deadline = Instant::now() + Duration::from_secs(2);
        while !owned.0.as_mut().unwrap().try_reap().unwrap() {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        }
        owned.0.take();
        assert_eq!(fixture.copies(), 0);
    }

    #[test]
    fn package_cleanup_preserves_replacement_resource() {
        let fixture = Fixture::new(b"#!/bin/sh\nexit 0\n");
        let resource = b"verified";
        std::fs::write(fixture.helper.with_file_name("mlx.metallib"), resource).unwrap();
        let child = ModelProcess::spawn_with_resources(
            &fixture.runtime,
            &fixture.helper,
            fixture.digest,
            metal(Sha256::digest(resource).into()),
            &AtomicBool::new(false),
        )
        .unwrap();
        let mut owned = OwnedTestChild(Some(child));
        let directory = std::fs::read_dir(fixture.root.join(".asura/run"))
            .unwrap()
            .flatten()
            .find(|entry| entry.file_name().to_string_lossy().starts_with("model-"))
            .unwrap()
            .path();
        let target = directory.join("mlx.metallib");
        std::fs::remove_file(&target).unwrap();
        std::fs::write(&target, b"replacement").unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while !owned.0.as_mut().unwrap().try_reap().unwrap() {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        }
        owned.0.take();
        assert_eq!(std::fs::read(target).unwrap(), b"replacement");
        assert!(!directory.join("asura-model").exists());
    }
    #[test]
    fn verified_copy_exits_and_is_removed_after_reap() {
        let fixture = Fixture::new(b"#!/bin/sh\nexit 0\n");
        let mut owned = OwnedTestChild(Some(fixture.spawn()));
        assert_eq!(fixture.copies(), 1);
        let deadline = Instant::now() + Duration::from_secs(2);
        while !owned.0.as_mut().unwrap().try_reap().unwrap() {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        }
        owned.0.take();
        assert_eq!(fixture.copies(), 0);
    }
    #[test]
    fn kill_targets_owned_child_and_reaps() {
        let fixture = Fixture::new(b"#!/bin/sh\nexec /bin/sleep 60\n");
        let mut owned = OwnedTestChild(Some(fixture.spawn()));
        owned.0.as_ref().unwrap().terminate(true).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while !owned.0.as_mut().unwrap().try_reap().unwrap() {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        }
        owned.0.take();
        assert_eq!(fixture.copies(), 0);
    }
    #[test]
    fn bad_hash_and_cancel_leave_no_copy_or_child() {
        let fixture = Fixture::new(b"#!/bin/sh\nexit 0\n");
        assert!(
            ModelProcess::spawn(
                &fixture.runtime,
                &fixture.helper,
                [0; 32],
                &AtomicBool::new(false)
            )
            .is_err()
        );
        assert_eq!(fixture.copies(), 0);
        assert!(
            ModelProcess::spawn(
                &fixture.runtime,
                &fixture.helper,
                fixture.digest,
                &AtomicBool::new(true)
            )
            .is_err()
        );
        assert_eq!(fixture.copies(), 0);
    }

    /// Directories and (path, bytes) files of one bundle fixture.
    type BundleCase<'a> = (Vec<&'a str>, Vec<(&'a str, &'a [u8])>);
    fn metal(digest: [u8; 32]) -> PackageResources {
        PackageResources {
            metallib: Some(digest),
            hub_bundle: None,
        }
    }
    fn hub(digest: [u8; 32]) -> PackageResources {
        PackageResources {
            metallib: None,
            hub_bundle: Some(digest),
        }
    }
    /// Replace the bundle beside the fixture helper; returns its digest.
    fn write_bundle(fixture: &Fixture, directories: &[&str], files: &[(&str, &[u8])]) -> [u8; 32] {
        let root = fixture.helper.with_file_name(HUB_BUNDLE_NAME);
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir(&root).unwrap();
        for directory in directories {
            std::fs::create_dir_all(root.join(directory)).unwrap();
        }
        let mut hashes = Vec::new();
        for (path, bytes) in files {
            let target = root.join(path);
            std::fs::create_dir_all(target.parent().unwrap()).unwrap();
            std::fs::write(&target, bytes).unwrap();
            hashes.push((path.to_string(), Sha256::digest(bytes).into()));
        }
        bundle_digest(&mut hashes)
    }
    /// Loop closure is the reap itself; there is no time bound.
    fn reap(mut owned: OwnedTestChild) {
        while !owned.0.as_mut().unwrap().try_reap().unwrap() {
            std::thread::sleep(Duration::from_millis(5));
        }
        owned.0.take();
    }

    #[test]
    fn hub_bundle_digest_matches_the_shasum_form() {
        // `find . -type f | sed 's|^\./||' | LC_ALL=C sort | tr '\n' '\0' |
        //  xargs -0 shasum -a 256 | shasum -a 256` over the same three files.
        let mut files = vec![
            (
                "Contents/_CodeSignature/CodeSignature".to_string(),
                Sha256::digest(b"").into(),
            ),
            (
                "Contents/Info.plist".to_string(),
                Sha256::digest(b"plist").into(),
            ),
            (
                "Contents/Resources/a.json".to_string(),
                Sha256::digest(b"{}").into(),
            ),
        ];
        let digest: String = bundle_digest(&mut files)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        assert_eq!(
            digest,
            "6ba61b27db375727414dab22591c72357e5fcb3c5e93a97dbed9f59aa39d4242"
        );
        // Order is by path bytes: `R` sorts before `_`.
        assert_eq!(files[1].0, "Contents/Resources/a.json");
    }

    #[test]
    fn hub_bundle_is_verified_colocated_and_removed_after_reap() {
        let fixture = Fixture::new(
            b"#!/bin/sh\n/bin/cat \"${0%/*}/swift-transformers_Hub.bundle/Contents/Resources/a.json\"\n",
        );
        let digest = write_bundle(
            &fixture,
            &[],
            &[
                ("Contents/Info.plist", b"plist"),
                ("Contents/Resources/a.json", b"{\"tokenizer\":1}"),
                ("Contents/_CodeSignature/CodeSignature", b""),
            ],
        );
        let child = ModelProcess::spawn_with_resources(
            &fixture.runtime,
            &fixture.helper,
            fixture.digest,
            hub(digest),
            &AtomicBool::new(false),
        )
        .unwrap();
        let mut owned = OwnedTestChild(Some(child));
        assert_eq!(fixture.copies(), 1);
        while !owned.0.as_mut().unwrap().try_reap().unwrap() {
            std::thread::sleep(Duration::from_millis(5));
        }
        owned.0.as_mut().unwrap().poll_diagnostics().unwrap();
        assert_eq!(
            owned.0.as_ref().unwrap().diagnostic_tail,
            b"{\"tokenizer\":1}"
        );
        owned.0.take();
        assert_eq!(fixture.copies(), 0);
    }

    #[test]
    fn package_resource_directory_is_contents_resources_only_inside_an_app_bundle() {
        // SG-V9 unit: one resource directory per layout, decided by the path.
        for (helper, expected) in [
            (
                "/opt/homebrew/Cellar/asura/0.1.0/libexec/Asura.app/Contents/MacOS/asura-model",
                "/opt/homebrew/Cellar/asura/0.1.0/libexec/Asura.app/Contents/Resources",
            ),
            (
                "/private/tmp/out/Asura.app/Contents/MacOS/asura-model",
                "/private/tmp/out/Asura.app/Contents/Resources",
            ),
            (
                "/Applications/Other.app/Contents/MacOS/asura-model",
                "/Applications/Other.app/Contents/Resources",
            ),
            // Development package and test binaries: beside the helper.
            ("/repo/target/debug/asura-model", "/repo/target/debug"),
            (
                "/repo/.build/current-package/asura-model",
                "/repo/.build/current-package",
            ),
            // Not a bundle: a `MacOS` directory outside `Contents`, a
            // `Contents` directory outside an `.app`, or a lower-case name.
            ("/x/Asura.app/MacOS/asura-model", "/x/Asura.app/MacOS"),
            (
                "/x/Asura/Contents/MacOS/asura-model",
                "/x/Asura/Contents/MacOS",
            ),
            (
                "/x/Asura.app/contents/MacOS/asura-model",
                "/x/Asura.app/contents/MacOS",
            ),
            (
                "/x/Asura.app/Contents/macos/asura-model",
                "/x/Asura.app/Contents/macos",
            ),
            (
                "/x/Asura.apps/Contents/MacOS/asura-model",
                "/x/Asura.apps/Contents/MacOS",
            ),
            ("/Contents/MacOS/asura-model", "/Contents/MacOS"),
            ("asura-model", ""),
        ] {
            assert_eq!(
                package_resource_directory(Path::new(helper)),
                PathBuf::from(expected),
                "{helper}"
            );
        }
    }

    /// An `Asura.app` layout under the fixture root: the fixture helper is
    /// copied to `Contents/MacOS/asura-model`; returns (helper, Resources).
    fn app_layout(fixture: &Fixture) -> (PathBuf, PathBuf) {
        let contents = fixture.root.join("Asura.app/Contents");
        let resources = contents.join("Resources");
        std::fs::create_dir_all(contents.join("MacOS")).unwrap();
        std::fs::create_dir(&resources).unwrap();
        let helper = contents.join("MacOS/asura-model");
        std::fs::copy(&fixture.helper, &helper).unwrap();
        (helper, resources)
    }

    #[test]
    fn app_bundle_resources_are_read_from_contents_resources() {
        // SG-V9 integration: the copy step finds `Resources/mlx.metallib` and
        // the Hub bundle; decoys beside the helper inside the bundle are ignored.
        let fixture = Fixture::new(
            b"#!/bin/sh\n/bin/cat \"${0%/*}/mlx.metallib\" \"${0%/*}/swift-transformers_Hub.bundle/Contents/Resources/a.json\"\n",
        );
        let (helper, resources) = app_layout(&fixture);
        let metal_bytes = b"bundle-metal;";
        std::fs::write(resources.join("mlx.metallib"), metal_bytes).unwrap();
        std::fs::write(helper.with_file_name("mlx.metallib"), b"decoy-metal;").unwrap();
        let digest = write_bundle(
            &fixture,
            &[],
            &[
                ("Contents/Info.plist", b"plist"),
                ("Contents/Resources/a.json", b"bundle-hub"),
            ],
        );
        std::fs::rename(
            fixture.helper.with_file_name(HUB_BUNDLE_NAME),
            resources.join(HUB_BUNDLE_NAME),
        )
        .unwrap();
        write_bundle(
            &fixture,
            &[],
            &[
                ("Contents/Info.plist", b"plist"),
                ("Contents/Resources/a.json", b"decoy-hub"),
            ],
        );
        std::fs::rename(
            fixture.helper.with_file_name(HUB_BUNDLE_NAME),
            helper.with_file_name(HUB_BUNDLE_NAME),
        )
        .unwrap();
        let child = ModelProcess::spawn_with_resources(
            &fixture.runtime,
            &helper,
            fixture.digest,
            PackageResources {
                metallib: Some(Sha256::digest(metal_bytes).into()),
                hub_bundle: Some(digest),
            },
            &AtomicBool::new(false),
        )
        .unwrap();
        let mut owned = OwnedTestChild(Some(child));
        assert_eq!(fixture.copies(), 1);
        while !owned.0.as_mut().unwrap().try_reap().unwrap() {
            std::thread::sleep(Duration::from_millis(5));
        }
        owned.0.as_mut().unwrap().poll_diagnostics().unwrap();
        assert_eq!(
            owned.0.as_ref().unwrap().diagnostic_tail,
            b"bundle-metal;bundle-hub"
        );
        owned.0.take();
        assert_eq!(fixture.copies(), 0);
    }

    #[test]
    fn app_bundle_without_contents_resources_never_spawns() {
        // SG-V9 integration: resources only beside the helper inside a bundle
        // are absent; preparation fails before spawn and leaves no copy.
        let fixture = Fixture::new(b"#!/bin/sh\nexit 0\n");
        let (helper, _resources) = app_layout(&fixture);
        let metal_bytes = b"beside-only";
        std::fs::write(helper.with_file_name("mlx.metallib"), metal_bytes).unwrap();
        let digest = write_bundle(&fixture, &[], &[("Contents/Info.plist", b"plist")]);
        std::fs::rename(
            fixture.helper.with_file_name(HUB_BUNDLE_NAME),
            helper.with_file_name(HUB_BUNDLE_NAME),
        )
        .unwrap();
        for resources in [metal(Sha256::digest(metal_bytes).into()), hub(digest)] {
            let result = ModelProcess::spawn_with_resources(
                &fixture.runtime,
                &helper,
                fixture.digest,
                resources,
                &AtomicBool::new(false),
            );
            assert!(result.is_err(), "{resources:?} must not spawn");
            assert_eq!(fixture.copies(), 0);
        }
    }

    #[test]
    fn hub_bundle_limits_are_accepted_at_the_bound() {
        let fixture = Fixture::new(b"#!/bin/sh\nexit 0\n");
        let sixteen: Vec<String> = (0..16).map(|n| format!("Contents/f{n:02}")).collect();
        let sixteen_files: Vec<(&str, &[u8])> = sixteen
            .iter()
            .map(|p| (p.as_str(), b"x".as_slice()))
            .collect();
        let fifteen_directories: Vec<String> = (0..15).map(|n| format!("d{n:02}")).collect();
        let fifteen_directories: Vec<&str> =
            fifteen_directories.iter().map(String::as_str).collect();
        let mebibyte = vec![7u8; 1024 * 1024];
        let half = vec![8u8; 512 * 1024];
        let cases: Vec<BundleCase> = vec![
            (vec![], sixteen_files),
            // The root holds 15 directories plus `a`: 16 in all.
            (fifteen_directories, vec![("a/x", b"x")]),
            (vec![], vec![("a/b/c/d", b"deep")]),
            (vec![], vec![("one", mebibyte.as_slice())]),
            (
                vec![],
                vec![("one", half.as_slice()), ("two", half.as_slice())],
            ),
            (vec![], vec![("empty", b"")]),
        ];
        for (directories, files) in cases {
            let digest = write_bundle(&fixture, &directories, &files);
            let child = ModelProcess::spawn_with_resources(
                &fixture.runtime,
                &fixture.helper,
                fixture.digest,
                hub(digest),
                &AtomicBool::new(false),
            )
            .unwrap_or_else(|error| panic!("{files:?}: {error}"));
            reap(OwnedTestChild(Some(child)));
            assert_eq!(fixture.copies(), 0);
        }
    }

    #[test]
    fn hub_bundle_over_a_limit_or_unsafe_never_spawns_and_cleans_owned_copies() {
        use std::os::unix::fs::symlink;
        let fixture = Fixture::new(b"#!/bin/sh\nexit 0\n");
        let bundle = fixture.helper.with_file_name(HUB_BUNDLE_NAME);
        let seventeen: Vec<String> = (0..17).map(|n| format!("Contents/f{n:02}")).collect();
        let seventeen_files: Vec<(&str, &[u8])> = seventeen
            .iter()
            .map(|p| (p.as_str(), b"x".as_slice()))
            .collect();
        let sixteen_directories: Vec<String> = (0..16).map(|n| format!("d{n:02}")).collect();
        let sixteen_directories: Vec<&str> =
            sixteen_directories.iter().map(String::as_str).collect();
        let over = vec![7u8; 1024 * 1024 + 1];
        let large = vec![8u8; 512 * 1024];
        let attempt = |resources: PackageResources| {
            let result = ModelProcess::spawn_with_resources(
                &fixture.runtime,
                &fixture.helper,
                fixture.digest,
                resources,
                &AtomicBool::new(false),
            );
            assert!(result.is_err());
            assert_eq!(fixture.copies(), 0);
        };
        // Each case is within every other limit; the digest is the true one.
        let cases: Vec<(&str, BundleCase)> = vec![
            ("17 files", (vec![], seventeen_files)),
            ("17 directories", (sixteen_directories, vec![("a/x", b"x")])),
            ("depth 5", (vec![], vec![("a/b/c/d/e", b"deep")])),
            ("1 MiB + 1 file", (vec![], vec![("one", over.as_slice())])),
            (
                "1 MiB + 1 total",
                (
                    vec![],
                    vec![
                        ("one", large.as_slice()),
                        ("two", large.as_slice()),
                        ("three", b"x"),
                    ],
                ),
            ),
            ("control name", (vec![], vec![("a\nb", b"x")])),
            ("backslash name", (vec![], vec![("a\\b", b"x")])),
        ];
        for (label, (directories, files)) in cases {
            let digest = write_bundle(&fixture, &directories, &files);
            eprintln!("case {label}");
            attempt(hub(digest));
        }
        // Wrong digest and an absent bundle.
        let digest = write_bundle(&fixture, &[], &[("a", b"x")]);
        let mut wrong = digest;
        wrong[0] ^= 1;
        attempt(hub(wrong));
        std::fs::remove_dir_all(&bundle).unwrap();
        attempt(hub(digest));
        // A symlink, at the root and inside the bundle.
        let digest = write_bundle(&fixture, &[], &[("a", b"x")]);
        symlink("a", bundle.join("link")).unwrap();
        attempt(hub(digest));
        let real = fixture.root.join("real.bundle");
        std::fs::rename(&bundle, &real).unwrap();
        std::fs::remove_file(real.join("link")).unwrap();
        symlink(&real, &bundle).unwrap();
        attempt(hub(digest));
        std::fs::remove_file(&bundle).unwrap();
        // Group-writable file and directory.
        let digest = write_bundle(&fixture, &[], &[("a/x", b"x")]);
        std::fs::set_permissions(bundle.join("a/x"), std::fs::Permissions::from_mode(0o664))
            .unwrap();
        attempt(hub(digest));
        std::fs::set_permissions(bundle.join("a/x"), std::fs::Permissions::from_mode(0o644))
            .unwrap();
        std::fs::set_permissions(bundle.join("a"), std::fs::Permissions::from_mode(0o775)).unwrap();
        attempt(hub(digest));
        // A copied MLX resource is cleaned when the bundle then fails.
        let digest = write_bundle(&fixture, &[], &[("a", b"x")]);
        std::fs::write(fixture.helper.with_file_name("mlx.metallib"), b"m").unwrap();
        let mut wrong = digest;
        wrong[0] ^= 1;
        attempt(PackageResources {
            metallib: Some(Sha256::digest(b"m").into()),
            hub_bundle: Some(wrong),
        });
    }

    #[test]
    fn hub_bundle_cleanup_preserves_unknown_entries() {
        let fixture = Fixture::new(b"#!/bin/sh\nexec /bin/sleep 60\n");
        let digest = write_bundle(
            &fixture,
            &[],
            &[
                ("Contents/Info.plist", b"plist"),
                ("Contents/Resources/a", b"a"),
            ],
        );
        let child = ModelProcess::spawn_with_resources(
            &fixture.runtime,
            &fixture.helper,
            fixture.digest,
            hub(digest),
            &AtomicBool::new(false),
        )
        .unwrap();
        let owned = OwnedTestChild(Some(child));
        let directory = std::fs::read_dir(fixture.root.join(".asura/run"))
            .unwrap()
            .flatten()
            .find(|entry| entry.file_name().to_string_lossy().starts_with("model-"))
            .unwrap()
            .path();
        let copied = directory.join(HUB_BUNDLE_NAME);
        std::fs::write(copied.join("Contents/unknown"), b"unknown").unwrap();
        // A replaced owned name is unknown too.
        std::fs::remove_file(copied.join("Contents/Resources/a")).unwrap();
        std::fs::write(copied.join("Contents/Resources/a"), b"replacement").unwrap();
        owned.0.as_ref().unwrap().terminate(true).unwrap();
        reap(owned);
        assert_eq!(
            std::fs::read(copied.join("Contents/unknown")).unwrap(),
            b"unknown"
        );
        assert_eq!(
            std::fs::read(copied.join("Contents/Resources/a")).unwrap(),
            b"replacement"
        );
        assert!(!copied.join("Contents/Info.plist").exists());
        assert!(!directory.join("asura-model").exists());
    }

    #[test]
    fn cancel_during_hub_bundle_copy_cleans_owned_copies() {
        use std::cell::Cell;
        let fixture = Fixture::new(b"#!/bin/sh\nexit 0\n");
        let chunk = vec![9u8; 200 * 1024];
        let digest = write_bundle(
            &fixture,
            &[],
            &[
                ("Contents/Info.plist", b"plist"),
                ("Contents/Resources/large", chunk.as_slice()),
                ("Contents/_CodeSignature/CodeSignature", b""),
            ],
        );
        let package = fixture.root.join("package");
        std::fs::create_dir(&package).unwrap();
        let held = OwnedFd::from(File::open(&package).unwrap());
        let uid = unsafe { libc::geteuid() };
        let source = fixture.helper.with_file_name(HUB_BUNDLE_NAME);
        let mut cancelled_at = 0;
        for limit in 1.. {
            let calls = Cell::new(0);
            let check = || {
                calls.set(calls.get() + 1);
                if calls.get() > limit {
                    Err(Error::Deadline)
                } else {
                    Ok(())
                }
            };
            match copy_bundle(&held, &source, digest, uid, &check) {
                Err(Error::Deadline) => {
                    cancelled_at += 1;
                    assert_eq!(std::fs::read_dir(&package).unwrap().count(), 0);
                }
                Err(error) => panic!("unexpected failure: {error}"),
                Ok(tree) => {
                    drop(tree);
                    assert_eq!(std::fs::read_dir(&package).unwrap().count(), 0);
                    break;
                }
            }
        }
        // Cancellation was observed at every check point before completion,
        // including within the multi-chunk file copy.
        assert!(cancelled_at > 8, "{cancelled_at}");
    }
}

fn parse_failure_diagnostic(line: &[u8]) -> Option<(&'static str, i64)> {
    if line.len() >= 256 {
        return None;
    }
    let line = std::str::from_utf8(line).ok()?;
    let rest = line.strip_prefix("asura_model_failure ")?;
    let (kind, code) = rest.split_once(' ')?;
    let kind = match kind {
        "context_limit" => "context_limit",
        "rate_limited" => "rate_limited",
        "guardrail" => "guardrail",
        "refusal" => "refusal",
        "unsupported_capability" => "unsupported_capability",
        "unsupported_transcript" => "unsupported_transcript",
        "unsupported_guide" => "unsupported_guide",
        "unsupported_language" => "unsupported_language",
        "timeout" => "timeout",
        "unknown_sdk" => "unknown_sdk",
        "cancelled" => "cancelled",
        "provider_cancelled" => "provider_cancelled",
        "task_cancelled" => "task_cancelled",
        "backend_failure" => "backend_failure",
        "unclassified" => "unclassified",
        "generated_content" => "generated_content",
        "generated_content_empty" => "generated_content_empty",
        "generated_content_oversized" => "generated_content_oversized",
        "generated_content_object" => "generated_content_object",
        "generated_content_array" => "generated_content_array",
        "generated_content_scalar" => "generated_content_scalar",
        "generated_content_invalid_json" => "generated_content_invalid_json",
        "decoding" => "decoding",
        "tool_callback" => "tool_callback",
        "session_state" => "session_state",
        "helper_protocol" => "helper_protocol",
        "helper_limit" => "helper_limit",
        "helper_closed" => "helper_closed",
        "helper_unavailable" => "helper_unavailable",
        "helper_context" => "helper_context",
        "helper_timeout" => "helper_timeout",
        _ => return None,
    };
    Some((kind, code.parse().ok()?))
}
#[cfg(test)]
mod failure_diagnostic_tests {
    use super::parse_failure_diagnostic;
    #[test]
    fn logs_only_allowlisted_class_and_numeric_code() {
        assert_eq!(
            parse_failure_diagnostic(b"asura_model_failure rate_limited 42"),
            Some(("rate_limited", 42))
        );
        for bad in [
            b"provider prompt secret".as_slice(),
            b"asura_model_failure secret 42",
            b"asura_model_failure timeout 42 secret",
            b"asura_model_failure timeout secret",
        ] {
            assert_eq!(parse_failure_diagnostic(bad), None);
        }
    }
}
