//! Pinned project directory identity; no symlink traversal.
use crate::runtime::{Identity, open_at, stat_at, stat_fd};
use crate::{Error, Result};
use std::ffi::CString;
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::{Component, Path};
use std::sync::Arc;

/// The Asura home and every directory identity that contains it under some
/// spelling. A project may not equal, lie inside, or contain the home.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StateBoundary(Arc<BoundaryInner>);
#[derive(Debug, PartialEq, Eq)]
struct BoundaryInner {
    home: Identity,
    ancestors: Vec<Identity>,
}
impl StateBoundary {
    pub(crate) fn new(home: Identity, ancestors: impl IntoIterator<Item = Identity>) -> Self {
        let mut ancestors: Vec<_> = ancestors.into_iter().filter(|id| *id != home).collect();
        ancestors.sort_by_key(|id| (id.device, id.inode));
        ancestors.dedup();
        Self(Arc::new(BoundaryInner { home, ancestors }))
    }
    /// Matches no directory. Test-only, for fixtures with no Asura home.
    #[cfg(any(test, feature = "test-support"))]
    pub fn detached_for_test() -> Self {
        Self::new(
            Identity {
                device: 0,
                inode: 0,
            },
            [],
        )
    }
    fn is_home(&self, id: Identity) -> bool {
        self.0.home == id
    }
    fn contains_home(&self, id: Identity) -> bool {
        self.0.ancestors.contains(&id)
    }
}
pub struct ProjectIdentity {
    location: String,
    held: OwnedFd,
    boundary: StateBoundary,
    pub device: u64,
    pub inode: u64,
}
/// Walks from `/` without following links. With a boundary, any component that
/// is the Asura home, or a final directory that contains it, is rejected.
fn open_location(
    location: &str,
    boundary: Option<&StateBoundary>,
) -> Result<(OwnedFd, libc::stat)> {
    if location.len() > 4096
        || !location.starts_with('/')
        || (location.len() > 1 && location.ends_with('/'))
        || location.contains("//")
    {
        return Err(Error::UnsafeRuntime);
    }
    let inside = |fd: &OwnedFd| -> Result<libc::stat> {
        let stat = stat_fd(fd.as_raw_fd())?;
        if boundary.is_some_and(|b| b.is_home(crate::runtime::identity(&stat))) {
            return Err(Error::OverlapsAsuraState);
        }
        Ok(stat)
    };
    let mut held = open_at(libc::AT_FDCWD, c"/", libc::O_RDONLY | libc::O_DIRECTORY, 0)?;
    let mut stat = inside(&held)?;
    for part in Path::new(location).components() {
        match part {
            Component::RootDir => {}
            Component::Normal(name) => {
                let name =
                    CString::new(name.as_encoded_bytes()).map_err(|_| Error::UnsafeRuntime)?;
                held = open_at(
                    held.as_raw_fd(),
                    &name,
                    libc::O_RDONLY | libc::O_DIRECTORY,
                    0,
                )?;
                stat = inside(&held)?;
            }
            _ => return Err(Error::UnsafeRuntime),
        }
    }
    // Path components normalize dots; reject their spelling independently.
    if location.split('/').any(|part| part == "." || part == "..") {
        return Err(Error::UnsafeRuntime);
    }
    if boundary.is_some_and(|b| b.contains_home(crate::runtime::identity(&stat))) {
        return Err(Error::OverlapsAsuraState);
    }
    Ok((held, stat))
}
/// The guardian's identity pin for its inherited descriptors. It applies no
/// boundary; the guardian checks overlap separately against FD 10.
pub(crate) fn pinned_identity(location: &str) -> Result<(u64, u64)> {
    let (_, stat) = open_location(location, None)?;
    Ok((stat.st_dev as u64, stat.st_ino))
}
impl ProjectIdentity {
    pub fn open(location: &str, boundary: &StateBoundary) -> Result<Self> {
        let (held, stat) = open_location(location, Some(boundary))?;
        Ok(Self {
            location: location.into(),
            held,
            boundary: boundary.clone(),
            device: stat.st_dev as u64,
            inode: stat.st_ino,
        })
    }
    pub(crate) fn process_location(&self) -> &str {
        &self.location
    }
    pub(crate) fn boundary(&self) -> &StateBoundary {
        &self.boundary
    }
    pub(crate) fn process_directory(&self) -> Result<OwnedFd> {
        self.validate()?;
        self.held.try_clone().map_err(Error::from)
    }
    pub fn validate(&self) -> Result<()> {
        let current = Self::open(&self.location, &self.boundary)?;
        let held = stat_fd(self.held.as_raw_fd())?;
        if current.device != self.device || current.inode != self.inode || held.st_nlink == 0 {
            return Err(Error::UnsafeRuntime);
        }
        Ok(())
    }
}

/// A bounded observation. The continuation offset is measured in original bytes.
#[derive(Debug, Eq, PartialEq)]
pub struct ProjectPage {
    pub text: String,
    pub next_offset: u64,
    pub truncated: bool,
}

#[derive(Debug, Eq, PartialEq)]
pub enum ProjectReadError {
    InvalidArguments,
    Cancelled,
    UnsafePath,
    Changed,
    NotText,
    Limit,
    Unavailable,
}

impl ProjectIdentity {
    /// Blocking OS calls: the caller must isolate this on its retained IO worker.
    pub fn read_project_page(
        &self,
        relative: &str,
        offset: u64,
        limit: usize,
        cancelled: &std::sync::atomic::AtomicBool,
    ) -> std::result::Result<ProjectPage, ProjectReadError> {
        use std::os::unix::fs::FileExt;
        project_check(cancelled)?;
        if limit == 0
            || limit > 16_384
            || offset > i64::MAX as u64
            || offset
                .checked_add(limit as u64)
                .is_none_or(|end| end > i64::MAX as u64)
        {
            return Err(ProjectReadError::InvalidArguments);
        }
        self.validate().map_err(|_| ProjectReadError::Changed)?;
        let parts = project_parts(relative)?;
        let mut parent = self
            .held
            .try_clone()
            .map_err(|_| ProjectReadError::Unavailable)?;
        for part in &parts[..parts.len() - 1] {
            project_check(cancelled)?;
            parent = open_at(
                parent.as_raw_fd(),
                part,
                libc::O_RDONLY | libc::O_DIRECTORY,
                0,
            )
            .map_err(|_| ProjectReadError::UnsafePath)?;
        }
        project_check(cancelled)?;
        let target = open_at(parent.as_raw_fd(), parts.last().unwrap(), libc::O_RDONLY, 0)
            .map_err(|_| ProjectReadError::UnsafePath)?;
        let before = stat_fd(target.as_raw_fd()).map_err(|_| ProjectReadError::Unavailable)?;
        if before.st_mode & libc::S_IFMT != libc::S_IFREG || before.st_size < 0 {
            return Err(ProjectReadError::UnsafePath);
        }
        let file = std::fs::File::from(target);
        // Read one additional byte to distinguish a complete page from truncation.
        // FileExt avoids changing a shared descriptor offset.
        let mut bytes = vec![0; limit + 1];
        let mut used = 0;
        while used < bytes.len() {
            project_check(cancelled)?;
            let count = match file.read_at(&mut bytes[used..], offset + used as u64) {
                Ok(count) => count,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => return Err(ProjectReadError::Unavailable),
            };
            if count == 0 {
                break;
            }
            used += count;
        }
        bytes.truncate(used);
        project_check(cancelled)?;
        let after = stat_fd(file.as_raw_fd()).map_err(|_| ProjectReadError::Unavailable)?;
        let current_parent = self.reopen_project_directory(&parts[..parts.len() - 1], cancelled)?;
        let named = stat_at(current_parent.as_raw_fd(), parts.last().unwrap())
            .map_err(|_| ProjectReadError::Changed)?;
        if before.st_dev != named.st_dev
            || before.st_ino != named.st_ino
            || before.st_dev != after.st_dev
            || before.st_ino != after.st_ino
            || before.st_size != after.st_size
            || before.st_mtime != after.st_mtime
            || before.st_mtime_nsec != after.st_mtime_nsec
            || after.st_nlink == 0
        {
            return Err(ProjectReadError::Changed);
        }
        self.validate().map_err(|_| ProjectReadError::Changed)?;
        project_check(cancelled)?;
        let mut truncated = bytes.len() > limit;
        bytes.truncate(limit);
        if bytes.contains(&0) {
            return Err(ProjectReadError::NotText);
        }
        let end = match std::str::from_utf8(&bytes) {
            Ok(_) => bytes.len(),
            Err(error) if error.error_len().is_none() && truncated => {
                if error.valid_up_to() == 0 {
                    return Err(ProjectReadError::Limit);
                }
                truncated = true;
                error.valid_up_to()
            }
            Err(_) => return Err(ProjectReadError::NotText),
        };
        bytes.truncate(end);
        let text = String::from_utf8(bytes).map_err(|_| ProjectReadError::NotText)?;
        Ok(ProjectPage {
            next_offset: offset + text.len() as u64,
            text,
            truncated,
        })
    }
}

/// Immediate directory names only; symlink entries are observations, never followed.
#[derive(Debug, Eq, PartialEq)]
pub struct ProjectListing {
    pub names: Vec<String>,
    pub truncated: bool,
}
impl ProjectIdentity {
    /// Run on the same isolated IO worker as read_project_page.
    pub fn list_project_directory(
        &self,
        relative: &str,
        cancelled: &std::sync::atomic::AtomicBool,
    ) -> std::result::Result<ProjectListing, ProjectReadError> {
        use std::os::fd::IntoRawFd;
        project_check(cancelled)?;
        self.validate().map_err(|_| ProjectReadError::Changed)?;
        // Open a fresh descriptor: dup would share the directory stream offset.
        let mut directory = open_at(
            self.held.as_raw_fd(),
            c".",
            libc::O_RDONLY | libc::O_DIRECTORY,
            0,
        )
        .map_err(|_| ProjectReadError::Unavailable)?;
        if relative != "." {
            for part in project_parts(relative)? {
                project_check(cancelled)?;
                directory = open_at(
                    directory.as_raw_fd(),
                    &part,
                    libc::O_RDONLY | libc::O_DIRECTORY,
                    0,
                )
                .map_err(|_| ProjectReadError::UnsafePath)?;
            }
        }
        let before = stat_fd(directory.as_raw_fd()).map_err(|_| ProjectReadError::Unavailable)?;
        let raw = directory.into_raw_fd();
        // SAFETY: raw is an exclusively owned directory descriptor. fdopendir
        // takes ownership only on success; the failure path closes it below.
        let stream = unsafe { libc::fdopendir(raw) };
        if stream.is_null() {
            // SAFETY: ownership was not transferred on fdopendir failure.
            unsafe {
                libc::close(raw);
            }
            return Err(ProjectReadError::Unavailable);
        }
        struct DirectoryStream(*mut libc::DIR);
        impl Drop for DirectoryStream {
            fn drop(&mut self) {
                // SAFETY: this guard exclusively owns the live stream.
                unsafe {
                    libc::closedir(self.0);
                }
            }
        }
        let stream = DirectoryStream(stream);
        let mut names = Vec::new();
        let mut bytes = 0usize;
        let mut truncated = false;
        loop {
            project_check(cancelled)?;
            // SAFETY: __error points to thread-local errno. Reset distinguishes
            // end-of-directory from an actual readdir error.
            unsafe {
                *libc::__error() = 0;
            }
            // SAFETY: stream is exclusively held until the guard drops. Copy
            // each name before the next readdir invalidates the returned entry.
            let entry = unsafe { libc::readdir(stream.0) };
            if entry.is_null() {
                if std::io::Error::last_os_error().raw_os_error() != Some(0) {
                    return Err(ProjectReadError::Unavailable);
                }
                break;
            }
            // SAFETY: a successful readdir returns a live NUL-terminated name.
            let name = unsafe { std::ffi::CStr::from_ptr((*entry).d_name.as_ptr()) };
            let name = name.to_str().map_err(|_| ProjectReadError::NotText)?;
            if name == "." || name == ".." {
                continue;
            }
            if name.chars().any(char::is_control) {
                return Err(ProjectReadError::NotText);
            }
            if names.len() == 128 || bytes + name.len() + 1 > 16_384 {
                truncated = true;
                break;
            }
            bytes += name.len() + 1;
            names.push(name.to_owned());
        }
        self.validate().map_err(|_| ProjectReadError::Changed)?;
        project_check(cancelled)?;
        let parts = if relative == "." {
            Vec::new()
        } else {
            project_parts(relative)?
        };
        let current = self.reopen_project_directory(&parts, cancelled)?;
        let named = stat_fd(current.as_raw_fd()).map_err(|_| ProjectReadError::Changed)?;
        let after = stat_fd(raw).map_err(|_| ProjectReadError::Unavailable)?;
        if named.st_dev != before.st_dev
            || named.st_ino != before.st_ino
            || before.st_mtime != after.st_mtime
            || before.st_mtime_nsec != after.st_mtime_nsec
            || after.st_nlink == 0
        {
            return Err(ProjectReadError::Changed);
        }
        project_check(cancelled)?;
        names.sort();
        Ok(ProjectListing { names, truncated })
    }
}

impl ProjectIdentity {
    fn reopen_project_directory(
        &self,
        parts: &[CString],
        cancelled: &std::sync::atomic::AtomicBool,
    ) -> std::result::Result<OwnedFd, ProjectReadError> {
        let mut directory = self
            .held
            .try_clone()
            .map_err(|_| ProjectReadError::Unavailable)?;
        for part in parts {
            project_check(cancelled)?;
            directory = open_at(
                directory.as_raw_fd(),
                part,
                libc::O_RDONLY | libc::O_DIRECTORY,
                0,
            )
            .map_err(|_| ProjectReadError::Changed)?;
        }
        Ok(directory)
    }
}

// ----- Canonical paths (security-policy.md, PL2 canonical paths) -----

/// Why a path has no canonical form (PL2 canonical paths, reason codes).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PathRefusal {
    NotCanonical,
    Symlink,
    OutsideRoot,
    HardLink,
    Unavailable,
}
impl PathRefusal {
    /// The stable reason code.
    pub fn reason(self) -> &'static str {
        match self {
            PathRefusal::NotCanonical => "path_not_canonical",
            PathRefusal::Symlink => "path_symlink",
            PathRefusal::OutsideRoot => "path_outside_root",
            PathRefusal::HardLink => "path_hard_link",
            PathRefusal::Unavailable => "path_unavailable",
        }
    }
}
/// What the caller will do with the path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WalkMode {
    /// Every component must exist.
    Read,
    /// The final component may be missing; its parent must exist.
    Create,
    /// Every component must exist, and a regular file must have one link.
    MediatedWrite,
}
/// A canonical path: the stored spelling, walked again and compared by
/// identity. `rel` is set for a walk under a root (empty never: the root
/// itself is not a `rel` path). `device` and `inode` are the final object's,
/// or the parent's when the final component is missing (`exists` false).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CanonicalPath {
    pub path: String,
    pub rel: Option<String>,
    pub exists: bool,
    pub nlink: u64,
    pub device: u64,
    pub inode: u64,
    /// A walked directory, or the final object, is the Asura home.
    pub in_asura_home: bool,
}

/// W1 bounds of an input path.
const MAX_PATH_BYTES: usize = 1024;
const MAX_COMPONENTS: usize = 64;
/// The fixed `/private` aliases of the sealed system volume (step 2).
const ALIASES: [&str; 3] = ["tmp", "var", "etc"];

/// Step 1 for the components of a path (without its leading `/`).
fn lexical_components(text: &str) -> std::result::Result<Vec<CString>, PathRefusal> {
    if text.is_empty() {
        return Ok(Vec::new());
    }
    let parts: Vec<&str> = text.split('/').collect();
    if parts.len() > MAX_COMPONENTS {
        return Err(PathRefusal::NotCanonical);
    }
    parts
        .into_iter()
        .map(|part| {
            if part.is_empty() || part == "." || part == ".." {
                return Err(PathRefusal::NotCanonical);
            }
            CString::new(part).map_err(|_| PathRefusal::NotCanonical)
        })
        .collect()
}
fn lexical_input(text: &str) -> std::result::Result<(), PathRefusal> {
    if text.len() > MAX_PATH_BYTES || text.chars().any(char::is_control) {
        return Err(PathRefusal::NotCanonical);
    }
    Ok(())
}
/// Whether `path` is an absolute path in lexically canonical form.
pub fn lexically_canonical(path: &str) -> bool {
    lexical_input(path).is_ok()
        && (path == "/" || (path.starts_with('/') && lexical_components(&path[1..]).is_ok()))
}

/// Opens one component with `O_NOFOLLOW`. A symbolic link gives `Symlink`;
/// a missing component `Unavailable` with `missing` set.
fn open_component(
    dir: &OwnedFd,
    name: &CString,
    directory: bool,
) -> std::result::Result<OwnedFd, (PathRefusal, bool)> {
    let flags = libc::O_RDONLY | if directory { libc::O_DIRECTORY } else { 0 };
    match open_at(dir.as_raw_fd(), name, flags, 0) {
        Ok(fd) => Ok(fd),
        // The kind of the entry, without following it: a symbolic link, an
        // entry that cannot be opened, or no entry at all.
        Err(_) => match stat_at(dir.as_raw_fd(), name) {
            Ok(stat) if stat.st_mode & libc::S_IFMT == libc::S_IFLNK => {
                Err((PathRefusal::Symlink, false))
            }
            Ok(_) => Err((PathRefusal::Unavailable, false)),
            Err(_) => Err((PathRefusal::Unavailable, true)),
        },
    }
}

/// `fcntl(F_GETPATH)`: the kernel's spelling of an open descriptor.
fn descriptor_path(fd: &OwnedFd) -> std::result::Result<String, PathRefusal> {
    let mut buffer = vec![0u8; libc::PATH_MAX as usize + 1];
    // SAFETY: F_GETPATH writes at most MAXPATHLEN (PATH_MAX) bytes and a NUL
    // into the buffer, which holds PATH_MAX + 1 bytes; fd is borrowed open.
    let result = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETPATH, buffer.as_mut_ptr()) };
    if result < 0 {
        return Err(PathRefusal::Unavailable);
    }
    let end = buffer
        .iter()
        .position(|b| *b == 0)
        .ok_or(PathRefusal::Unavailable)?;
    buffer.truncate(end);
    String::from_utf8(buffer).map_err(|_| PathRefusal::Unavailable)
}

/// One walk's result: the final (or parent) descriptor's stat and spelling.
struct Walked {
    spelling: String,
    stat: libc::stat,
    exists: bool,
    in_home: bool,
}

/// Steps 3 and 4 below `start` (a directory descriptor) through `parts`.
fn walk(
    start: OwnedFd,
    parts: &[CString],
    mode: WalkMode,
    boundary: Option<&StateBoundary>,
) -> std::result::Result<Walked, PathRefusal> {
    let home =
        |stat: &libc::stat| boundary.is_some_and(|b| b.is_home(crate::runtime::identity(stat)));
    let mut held = start;
    let mut stat = stat_fd(held.as_raw_fd()).map_err(|_| PathRefusal::Unavailable)?;
    let mut in_home = home(&stat);
    for (index, part) in parts.iter().enumerate() {
        let last = index + 1 == parts.len();
        match open_component(&held, part, !last) {
            Ok(next) => {
                held = next;
                stat = stat_fd(held.as_raw_fd()).map_err(|_| PathRefusal::Unavailable)?;
                in_home |= home(&stat);
            }
            Err((PathRefusal::Unavailable, true)) if last && mode == WalkMode::Create => {
                let parent = descriptor_path(&held)?;
                let name = part.to_str().map_err(|_| PathRefusal::NotCanonical)?;
                let spelling = if parent == "/" {
                    format!("/{name}")
                } else {
                    format!("{parent}/{name}")
                };
                return Ok(Walked {
                    spelling,
                    stat,
                    exists: false,
                    in_home,
                });
            }
            Err((refusal, _)) => return Err(refusal),
        }
    }
    Ok(Walked {
        spelling: descriptor_path(&held)?,
        stat,
        exists: true,
        in_home,
    })
}
fn open_root() -> std::result::Result<OwnedFd, PathRefusal> {
    open_at(libc::AT_FDCWD, c"/", libc::O_RDONLY | libc::O_DIRECTORY, 0)
        .map_err(|_| PathRefusal::Unavailable)
}
/// The components of an absolute path after the step 2 alias.
fn absolute_parts(path: &str) -> std::result::Result<Vec<CString>, PathRefusal> {
    lexical_input(path)?;
    if !path.starts_with('/') {
        return Err(PathRefusal::NotCanonical);
    }
    let mut parts = lexical_components(&path[1..])?;
    if parts
        .first()
        .is_some_and(|first| ALIASES.iter().any(|a| first.as_bytes() == a.as_bytes()))
    {
        parts.insert(0, c"private".to_owned());
    }
    Ok(parts)
}
/// Step 4: the spelling walked again from `/` gives the same identity.
fn confirm(walked: &Walked, mode: WalkMode) -> std::result::Result<(), PathRefusal> {
    let parts = absolute_parts(&walked.spelling).map_err(|_| PathRefusal::Unavailable)?;
    let again = walk(open_root()?, &parts, mode, None).map_err(|_| PathRefusal::Unavailable)?;
    if again.exists != walked.exists
        || again.stat.st_dev != walked.stat.st_dev
        || again.stat.st_ino != walked.stat.st_ino
        || again.spelling != walked.spelling
    {
        return Err(PathRefusal::Unavailable);
    }
    Ok(())
}
fn finish(
    walked: Walked,
    rel: Option<String>,
    mode: WalkMode,
) -> std::result::Result<CanonicalPath, PathRefusal> {
    let regular = walked.stat.st_mode & libc::S_IFMT == libc::S_IFREG;
    if mode == WalkMode::MediatedWrite && walked.exists && regular && walked.stat.st_nlink > 1 {
        return Err(PathRefusal::HardLink);
    }
    Ok(CanonicalPath {
        path: walked.spelling,
        rel,
        exists: walked.exists,
        nlink: u64::from(walked.stat.st_nlink),
        device: walked.stat.st_dev as u64,
        inode: walked.stat.st_ino,
        in_asura_home: walked.in_home,
    })
}

/// The canonical form of an absolute path (PL2-F1). Blocking file system
/// calls: platform worker threads only.
pub fn canonical_path(
    path: &str,
    mode: WalkMode,
    boundary: Option<&StateBoundary>,
) -> std::result::Result<CanonicalPath, PathRefusal> {
    let parts = absolute_parts(path)?;
    let walked = walk(open_root()?, &parts, mode, boundary)?;
    confirm(&walked, mode)?;
    finish(walked, None, mode)
}
/// The canonical spelling of a registered root whose identity must equal
/// `(device, inode)`; another identity is `OutsideRoot`. Blocking.
pub fn canonical_root(
    location: &str,
    device: u64,
    inode: u64,
) -> std::result::Result<String, PathRefusal> {
    let root = canonical_path(location, WalkMode::Read, None)?;
    if root.device != device || root.inode != inode {
        return Err(PathRefusal::OutsideRoot);
    }
    Ok(root.path)
}
impl ProjectIdentity {
    /// The canonical form of `rel` under this retained root (PL2-F1, step
    /// 5). Blocking file system calls: platform worker threads only.
    pub fn canonical_under(
        &self,
        rel: &str,
        mode: WalkMode,
    ) -> std::result::Result<CanonicalPath, PathRefusal> {
        lexical_input(rel)?;
        if rel.starts_with('/') {
            return Err(PathRefusal::NotCanonical);
        }
        let parts = lexical_components(rel)?;
        if parts.is_empty() {
            return Err(PathRefusal::NotCanonical);
        }
        // The root's current spelling and identity.
        let held = self
            .held
            .try_clone()
            .map_err(|_| PathRefusal::Unavailable)?;
        let root_spelling = descriptor_path(&held)?;
        let root = canonical_path(&root_spelling, WalkMode::Read, None)
            .map_err(|_| PathRefusal::OutsideRoot)?;
        if root.device != self.device || root.inode != self.inode {
            return Err(PathRefusal::OutsideRoot);
        }
        let walked = walk(held, &parts, mode, Some(&self.boundary))?;
        confirm(&walked, mode)?;
        let prefix = if root.path == "/" {
            "/".to_owned()
        } else {
            format!("{}/", root.path)
        };
        let rest = walked
            .spelling
            .strip_prefix(&prefix)
            .filter(|rest| !rest.is_empty())
            .ok_or(PathRefusal::OutsideRoot)?
            .to_owned();
        finish(walked, Some(rest), mode)
    }
}

/// Project reads have no time bound; only the owner's cancel stops them.
fn project_check(
    cancelled: &std::sync::atomic::AtomicBool,
) -> std::result::Result<(), ProjectReadError> {
    if cancelled.load(std::sync::atomic::Ordering::Acquire) {
        return Err(ProjectReadError::Cancelled);
    }
    Ok(())
}
fn project_parts(relative: &str) -> std::result::Result<Vec<CString>, ProjectReadError> {
    if relative.is_empty()
        || relative.len() > 1024
        || relative.chars().any(|c| c.is_control() || c == '\\')
    {
        return Err(ProjectReadError::InvalidArguments);
    }
    relative
        .split('/')
        .map(|part| {
            if part.is_empty() || part == "." || part == ".." {
                return Err(ProjectReadError::InvalidArguments);
            }
            CString::new(part).map_err(|_| ProjectReadError::InvalidArguments)
        })
        .collect()
}

#[cfg(test)]
mod read_tests {
    use super::*;
    use std::sync::atomic::AtomicBool;
    static NEXT_SCRATCH: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    struct Scratch(std::path::PathBuf);
    impl Scratch {
        fn new() -> Self {
            let path = std::path::PathBuf::from(format!(
                "/private/tmp/asura-project-read-{}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos(),
                NEXT_SCRATCH.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
        fn identity(&self) -> ProjectIdentity {
            ProjectIdentity::open(
                self.0.to_str().unwrap(),
                &StateBoundary::detached_for_test(),
            )
            .unwrap()
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    #[test]
    fn bounded_file_pages_preserve_utf8_offsets() {
        let scratch = Scratch::new();
        std::fs::write(scratch.0.join("sample"), "abcédef").unwrap();
        let project = scratch.identity();
        let cancel = AtomicBool::new(false);
        let page = project.read_project_page("sample", 0, 4, &cancel).unwrap();
        assert_eq!(
            page,
            ProjectPage {
                text: "abc".into(),
                next_offset: 3,
                truncated: true
            }
        );
        let page = project.read_project_page("sample", 3, 16, &cancel).unwrap();
        assert_eq!(
            page,
            ProjectPage {
                text: "édef".into(),
                next_offset: 8,
                truncated: false
            }
        );
        assert_eq!(
            project.read_project_page("sample", 3, 1, &cancel),
            Err(ProjectReadError::Limit)
        );
    }
    #[test]
    fn project_reads_reject_escape_symlink_fifo_and_cancel() {
        use std::os::unix::fs::symlink;
        let scratch = Scratch::new();
        std::fs::write(scratch.0.join("sample"), "hello").unwrap();
        symlink("sample", scratch.0.join("link")).unwrap();
        let fifo = CString::new(scratch.0.join("fifo").as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: owned scratch path is a live NUL-terminated string; mode is valid.
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        let project = scratch.identity();
        let cancel = AtomicBool::new(false);
        assert_eq!(
            project.read_project_page("../sample", 0, 4, &cancel),
            Err(ProjectReadError::InvalidArguments)
        );
        for name in ["link", "fifo"] {
            assert_eq!(
                project.read_project_page(name, 0, 4, &cancel),
                Err(ProjectReadError::UnsafePath)
            );
        }
        cancel.store(true, std::sync::atomic::Ordering::Release);
        assert_eq!(
            project.read_project_page("sample", 0, 4, &cancel),
            Err(ProjectReadError::Cancelled)
        );
    }
    #[test]
    fn directory_listing_is_bounded_and_does_not_follow_links() {
        use std::os::unix::fs::symlink;
        let scratch = Scratch::new();
        for n in 0..130 {
            std::fs::write(scratch.0.join(format!("item-{n}")), "").unwrap();
        }
        symlink("/private/tmp", scratch.0.join("outside")).unwrap();
        let project = scratch.identity();
        let cancel = AtomicBool::new(false);
        let listing = project.list_project_directory(".", &cancel).unwrap();
        assert_eq!(listing.names.len(), 128);
        assert!(listing.truncated);
        assert_eq!(
            project.list_project_directory("outside", &cancel),
            Err(ProjectReadError::UnsafePath)
        );
    }
    #[test]
    fn project_replacement_and_binary_content_are_not_observations() {
        let scratch = Scratch::new();
        let root = scratch.0.join("project");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("binary"), [0xff, 0, 1]).unwrap();
        let project =
            ProjectIdentity::open(root.to_str().unwrap(), &StateBoundary::detached_for_test())
                .unwrap();
        let cancel = AtomicBool::new(false);
        assert_eq!(
            project.read_project_page("binary", 0, 16, &cancel),
            Err(ProjectReadError::NotText)
        );
        std::fs::rename(&root, scratch.0.join("old")).unwrap();
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("binary"), "replacement").unwrap();
        assert_eq!(
            project.read_project_page("binary", 0, 16, &cancel),
            Err(ProjectReadError::Changed)
        );
        assert_eq!(
            project.list_project_directory(".", &cancel),
            Err(ProjectReadError::Changed)
        );
    }
}

#[cfg(test)]
mod boundary_tests {
    use super::*;
    use crate::runtime::{BOUNDARY_LEVELS, walk_parents};
    fn id(device: i32, inode: u64) -> Identity {
        Identity { device, inode }
    }
    /// SI-U1: the physical walk ends at a self-parent and rejects 65 levels.
    #[test]
    fn walk_stops_at_root_and_rejects_deep_chains() {
        let step = |depth: &u64| Ok((depth - 1, id(1, (depth - 1).max(1))));
        assert_eq!(
            walk_parents(4u64, id(1, 4), step).unwrap(),
            vec![id(1, 3), id(1, 2), id(1, 1)]
        );
        let limit = BOUNDARY_LEVELS as u64;
        assert_eq!(
            walk_parents(limit + 1, id(1, limit + 1), step)
                .unwrap()
                .len(),
            BOUNDARY_LEVELS
        );
        assert!(matches!(
            walk_parents(limit + 2, id(1, limit + 2), step),
            Err(Error::UnsafeRuntime)
        ));
        assert!(matches!(
            walk_parents(4u64, id(1, 4), |_| Err(Error::UnsafeRuntime)),
            Err(Error::UnsafeRuntime)
        ));
    }
    /// SI-U1 and SI-U2: the union of all sources; only equal, inside or
    /// containing identities overlap.
    #[test]
    fn boundary_unites_sources_and_decides_overlap() {
        let chain = [id(1, 2), id(1, 10), id(1, 11)];
        let walk = [id(1, 11), id(1, 2), id(1, 5)];
        let mount = [id(1, 2), id(1, 7), id(1, 8)];
        let boundary = StateBoundary::new(id(1, 20), chain.into_iter().chain(walk).chain(mount));
        assert!(boundary.is_home(id(1, 20)));
        for contains in [2, 5, 7, 8, 10, 11] {
            assert!(boundary.contains_home(id(1, contains)), "{contains}");
        }
        assert!(!boundary.contains_home(id(1, 20)));
        assert!(!boundary.contains_home(id(1, 12)), "sibling");
        assert!(!boundary.contains_home(id(2, 10)), "other device");
        assert!(!boundary.is_home(id(2, 20)), "other device");
        assert_eq!(boundary.0.ancestors.len(), 6);
    }
    struct Home(std::path::PathBuf);
    impl Drop for Home {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn home() -> Home {
        use std::os::unix::fs::PermissionsExt;
        let path = std::path::PathBuf::from(format!(
            "/private/tmp/as-si-{}-{}",
            std::process::id(),
            crate::random_id()[..6]
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        ));
        std::fs::create_dir(&path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        Home(path)
    }
    /// SI-I1 and SI-I2 at the platform owner, with a scratch runtime.
    #[test]
    fn opens_reject_every_overlap_with_the_runtime_home() {
        let home = home();
        let runtime = crate::RuntimeDirectory::scratch(&home.0, true).unwrap();
        let boundary = runtime.state_boundary();
        std::fs::create_dir_all(home.0.join(".asura/tmp")).unwrap();
        std::fs::create_dir(home.0.join("project")).unwrap();
        let sibling = std::path::PathBuf::from(format!("{}-sibling", home.0.display()));
        std::fs::create_dir(&sibling).unwrap();
        let _sibling = Home(sibling.clone());
        let firmlinked = |path: &std::path::Path| {
            std::path::Path::new("/System/Volumes/Data").join(path.strip_prefix("/").unwrap())
        };
        let mut overlapping: Vec<std::path::PathBuf> = [
            home.0.clone(),
            home.0.join(".asura"),
            home.0.join(".asura/tmp"),
            home.0.join(".asura/run"),
            home.0.parent().unwrap().to_owned(),
            "/".into(),
            "/private".into(),
            "/System".into(),
            "/System/Volumes".into(),
            "/System/Volumes/Data".into(),
            "/System/Volumes/Data/private".into(),
        ]
        .into();
        overlapping.push(firmlinked(&home.0.join(".asura")));
        overlapping.push(firmlinked(&home.0));
        for path in overlapping {
            let path = path.to_str().unwrap();
            assert!(
                matches!(
                    ProjectIdentity::open(path, &boundary),
                    Err(Error::OverlapsAsuraState)
                ),
                "{path}"
            );
        }
        for path in [
            home.0.join("project"),
            sibling.clone(),
            firmlinked(&sibling),
        ] {
            let project = ProjectIdentity::open(path.to_str().unwrap(), &boundary).unwrap();
            project.validate().unwrap();
        }
    }
    /// SI-I8: a scratch home behind the `/Users` firmlink is protected under
    /// its `/System/Volumes/Data` spelling. Never touches the account home.
    #[test]
    fn overlap_check_crosses_the_users_firmlink() {
        use std::os::unix::fs::PermissionsExt;
        let shared = std::path::Path::new("/Users/Shared");
        let path = shared.join(format!(
            "as-si-{}-{}",
            std::process::id(),
            crate::random_id()[..6]
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        ));
        if std::fs::create_dir(&path).is_err() {
            eprintln!("SI-I8 unverified: /Users/Shared is not writable on this host");
            return;
        }
        let home = Home(path);
        std::fs::set_permissions(&home.0, std::fs::Permissions::from_mode(0o700)).unwrap();
        let runtime = crate::RuntimeDirectory::scratch(&home.0, true).unwrap();
        let boundary = runtime.state_boundary();
        let data = std::path::Path::new("/System/Volumes/Data");
        let spelled = data.join(home.0.strip_prefix("/").unwrap());
        for path in [
            spelled.clone(),
            spelled.join(".asura"),
            data.join("Users/Shared"),
            data.join("Users"),
            shared.to_owned(),
        ] {
            let path = path.to_str().unwrap();
            assert!(
                matches!(
                    ProjectIdentity::open(path, &boundary),
                    Err(Error::OverlapsAsuraState)
                ),
                "{path}"
            );
        }
        std::fs::create_dir(home.0.join("project")).unwrap();
        ProjectIdentity::open(spelled.join("project").to_str().unwrap(), &boundary).unwrap();
    }
    /// A pinned project replaced by one that overlaps fails revalidation.
    #[test]
    fn revalidation_applies_the_boundary() {
        let home = home();
        let runtime = crate::RuntimeDirectory::scratch(&home.0, true).unwrap();
        let project = home.0.join("project");
        std::fs::create_dir(&project).unwrap();
        let held =
            ProjectIdentity::open(project.to_str().unwrap(), &runtime.state_boundary()).unwrap();
        std::fs::remove_dir(&project).unwrap();
        std::os::unix::fs::symlink(home.0.join(".asura"), &project).unwrap();
        assert!(held.validate().is_err());
    }
}

/// PL-U9 (security-policy.md, PL2 validation map): the canonicaliser in
/// scratch directories under `/private/tmp`; never the Asura home.
#[cfg(test)]
mod canonical_tests {
    use super::*;
    use crate::runtime::Identity;
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    struct Scratch(String);
    impl Scratch {
        fn new() -> Self {
            let path = format!(
                "/private/tmp/asura-canonical-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            );
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
        fn at(&self, rel: &str) -> String {
            format!("{}/{rel}", self.0)
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn read(path: &str) -> std::result::Result<CanonicalPath, PathRefusal> {
        canonical_path(path, WalkMode::Read, None)
    }

    #[test]
    fn lexical_forms_are_refused() {
        let s = Scratch::new();
        std::fs::create_dir(s.at("a")).unwrap();
        for bad in [
            s.at("a/../a"),
            s.at("a/./x"),
            s.at("a//x"),
            s.at("a/"),
            "relative/a".into(),
            s.at("a\u{7}"),
            format!("/{}", "x".repeat(1025)),
            format!("/{}", vec!["d"; 65].join("/")),
        ] {
            assert_eq!(read(&bad), Err(PathRefusal::NotCanonical), "{bad:?}");
        }
        assert!(lexically_canonical("/"));
        assert!(!lexically_canonical("/a/../b"));
        assert_eq!(PathRefusal::NotCanonical.reason(), "path_not_canonical");
        assert_eq!(PathRefusal::HardLink.reason(), "path_hard_link");
    }

    #[test]
    fn symlink_components_are_refused() {
        let s = Scratch::new();
        std::fs::create_dir(s.at("real")).unwrap();
        std::fs::write(s.at("real/f"), b"x").unwrap();
        std::os::unix::fs::symlink("real", s.at("link")).unwrap();
        std::os::unix::fs::symlink("f", s.at("real/flink")).unwrap();
        assert_eq!(read(&s.at("link/f")), Err(PathRefusal::Symlink));
        assert_eq!(read(&s.at("link")), Err(PathRefusal::Symlink));
        assert_eq!(read(&s.at("real/flink")), Err(PathRefusal::Symlink));
        assert_eq!(read(&s.at("real/missing")), Err(PathRefusal::Unavailable));
        assert_eq!(read(&s.at("real/f")).unwrap().path, s.at("real/f"));
    }

    #[test]
    fn aliases_and_firmlinks_give_the_stored_spelling() {
        assert_eq!(read("/tmp").unwrap().path, "/private/tmp");
        assert_eq!(read("/var").unwrap().path, "/private/var");
        assert_eq!(read("/etc").unwrap().path, "/private/etc");
        let users = read("/Users").unwrap();
        let firm = read("/System/Volumes/Data/Users").unwrap();
        assert_eq!(firm.path, "/Users");
        assert_eq!((firm.device, firm.inode), (users.device, users.inode));
        assert_eq!(read("/").unwrap().path, "/");
        assert_eq!(read("/bin/sh").unwrap().path, "/bin/sh");
    }

    #[test]
    fn case_and_normalization_give_the_stored_spelling() {
        let s = Scratch::new();
        let nfc = "Caf\u{e9}";
        std::fs::create_dir(s.at(nfc)).unwrap();
        std::fs::write(s.at(&format!("{nfc}/File.txt")), b"x").unwrap();
        let stored = s.at(&format!("{nfc}/File.txt"));
        for spelling in [
            stored.clone(),
            s.at("CAF\u{c9}/file.TXT"),
            s.at("Cafe\u{301}/File.txt"),
            stored.replacen("/private/tmp", "/tmp", 1),
        ] {
            assert_eq!(read(&spelling).unwrap().path, stored, "{spelling:?}");
        }
    }

    #[test]
    fn hard_links_are_refused_only_for_mediated_writes() {
        let s = Scratch::new();
        std::fs::write(s.at("one"), b"x").unwrap();
        std::fs::hard_link(s.at("one"), s.at("two")).unwrap();
        let path = s.at("two");
        let found = canonical_path(&path, WalkMode::Read, None).unwrap();
        assert_eq!((found.nlink, found.path.as_str()), (2, path.as_str()));
        assert_eq!(
            canonical_path(&path, WalkMode::MediatedWrite, None),
            Err(PathRefusal::HardLink)
        );
        std::fs::write(s.at("single"), b"x").unwrap();
        assert!(canonical_path(&s.at("single"), WalkMode::MediatedWrite, None).is_ok());
    }

    #[test]
    fn create_mode_allows_a_missing_final_component() {
        let s = Scratch::new();
        std::fs::create_dir(s.at("Dir")).unwrap();
        let created = canonical_path(&s.at("dir/New"), WalkMode::Create, None).unwrap();
        assert_eq!(created.path, s.at("Dir/New"));
        assert!(!created.exists);
        assert_eq!(
            canonical_path(&s.at("nodir/New"), WalkMode::Create, None),
            Err(PathRefusal::Unavailable)
        );
        assert_eq!(read(&s.at("Dir/New")), Err(PathRefusal::Unavailable));
    }

    #[test]
    fn under_root_gives_rel_and_refuses_outside() {
        let s = Scratch::new();
        std::fs::create_dir_all(s.at("proj/src")).unwrap();
        std::fs::write(s.at("proj/src/Main.rs"), b"x").unwrap();
        std::fs::create_dir(s.at("other")).unwrap();
        std::os::unix::fs::symlink("../other", s.at("proj/out")).unwrap();
        let root =
            ProjectIdentity::open(&s.at("proj"), &StateBoundary::detached_for_test()).unwrap();
        let file = root.canonical_under("SRC/main.rs", WalkMode::Read).unwrap();
        assert_eq!(file.path, s.at("proj/src/Main.rs"));
        assert_eq!(file.rel.as_deref(), Some("src/Main.rs"));
        assert_eq!(
            root.canonical_under("out/x", WalkMode::Read),
            Err(PathRefusal::Symlink)
        );
        assert_eq!(
            root.canonical_under("../other", WalkMode::Read),
            Err(PathRefusal::NotCanonical)
        );
        assert_eq!(
            root.canonical_under("/etc", WalkMode::Read),
            Err(PathRefusal::NotCanonical)
        );
        // Another directory under the root's registered spelling.
        std::fs::rename(s.at("proj"), s.at("moved")).unwrap();
        std::fs::create_dir(s.at("proj")).unwrap();
        assert_eq!(
            canonical_root(&s.at("proj"), root.device, root.inode),
            Err(PathRefusal::OutsideRoot)
        );
        assert_eq!(
            canonical_root(&s.at("moved"), root.device, root.inode).unwrap(),
            s.at("moved")
        );
        // The retained root moved: its new spelling is the prefix.
        let moved = root.canonical_under("src/Main.rs", WalkMode::Read).unwrap();
        assert_eq!(moved.path, s.at("moved/src/Main.rs"));
    }

    #[test]
    fn asura_home_identity_is_flagged() {
        use std::os::unix::fs::MetadataExt;
        let s = Scratch::new();
        std::fs::create_dir_all(s.at("home/.asura/tmp")).unwrap();
        let stat = std::fs::metadata(s.at("home/.asura")).unwrap();
        let boundary = StateBoundary::new(
            Identity {
                device: stat.dev() as libc::dev_t,
                inode: stat.ino(),
            },
            [],
        );
        let inside =
            canonical_path(&s.at("home/.asura/tmp"), WalkMode::Read, Some(&boundary)).unwrap();
        assert!(inside.in_asura_home);
        let outside = canonical_path(&s.at("home"), WalkMode::Read, Some(&boundary)).unwrap();
        assert!(!outside.in_asura_home);
    }
}
