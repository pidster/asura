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
    Deadline,
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
        deadline: std::time::Instant,
        cancelled: &std::sync::atomic::AtomicBool,
    ) -> std::result::Result<ProjectPage, ProjectReadError> {
        use std::os::unix::fs::FileExt;
        project_check(deadline, cancelled)?;
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
            project_check(deadline, cancelled)?;
            parent = open_at(
                parent.as_raw_fd(),
                part,
                libc::O_RDONLY | libc::O_DIRECTORY,
                0,
            )
            .map_err(|_| ProjectReadError::UnsafePath)?;
        }
        project_check(deadline, cancelled)?;
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
            project_check(deadline, cancelled)?;
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
        project_check(deadline, cancelled)?;
        let after = stat_fd(file.as_raw_fd()).map_err(|_| ProjectReadError::Unavailable)?;
        let current_parent =
            self.reopen_project_directory(&parts[..parts.len() - 1], deadline, cancelled)?;
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
        project_check(deadline, cancelled)?;
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
        deadline: std::time::Instant,
        cancelled: &std::sync::atomic::AtomicBool,
    ) -> std::result::Result<ProjectListing, ProjectReadError> {
        use std::os::fd::IntoRawFd;
        project_check(deadline, cancelled)?;
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
                project_check(deadline, cancelled)?;
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
            project_check(deadline, cancelled)?;
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
        project_check(deadline, cancelled)?;
        let parts = if relative == "." {
            Vec::new()
        } else {
            project_parts(relative)?
        };
        let current = self.reopen_project_directory(&parts, deadline, cancelled)?;
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
        project_check(deadline, cancelled)?;
        names.sort();
        Ok(ProjectListing { names, truncated })
    }
}

impl ProjectIdentity {
    fn reopen_project_directory(
        &self,
        parts: &[CString],
        deadline: std::time::Instant,
        cancelled: &std::sync::atomic::AtomicBool,
    ) -> std::result::Result<OwnedFd, ProjectReadError> {
        let mut directory = self
            .held
            .try_clone()
            .map_err(|_| ProjectReadError::Unavailable)?;
        for part in parts {
            project_check(deadline, cancelled)?;
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

fn project_check(
    deadline: std::time::Instant,
    cancelled: &std::sync::atomic::AtomicBool,
) -> std::result::Result<(), ProjectReadError> {
    if cancelled.load(std::sync::atomic::Ordering::Acquire) {
        return Err(ProjectReadError::Cancelled);
    }
    if std::time::Instant::now() >= deadline {
        return Err(ProjectReadError::Deadline);
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
    use std::{
        sync::atomic::AtomicBool,
        time::{Duration, Instant},
    };
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
        let deadline = Instant::now() + Duration::from_secs(2);
        let page = project
            .read_project_page("sample", 0, 4, deadline, &cancel)
            .unwrap();
        assert_eq!(
            page,
            ProjectPage {
                text: "abc".into(),
                next_offset: 3,
                truncated: true
            }
        );
        let page = project
            .read_project_page("sample", 3, 16, deadline, &cancel)
            .unwrap();
        assert_eq!(
            page,
            ProjectPage {
                text: "édef".into(),
                next_offset: 8,
                truncated: false
            }
        );
        assert_eq!(
            project.read_project_page("sample", 3, 1, deadline, &cancel),
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
        let deadline = Instant::now() + Duration::from_secs(2);
        assert_eq!(
            project.read_project_page("../sample", 0, 4, deadline, &cancel),
            Err(ProjectReadError::InvalidArguments)
        );
        for name in ["link", "fifo"] {
            assert_eq!(
                project.read_project_page(name, 0, 4, deadline, &cancel),
                Err(ProjectReadError::UnsafePath)
            );
        }
        cancel.store(true, std::sync::atomic::Ordering::Release);
        assert_eq!(
            project.read_project_page("sample", 0, 4, deadline, &cancel),
            Err(ProjectReadError::Cancelled)
        );
        cancel.store(false, std::sync::atomic::Ordering::Release);
        assert_eq!(
            project.read_project_page("sample", 0, 4, Instant::now(), &cancel),
            Err(ProjectReadError::Deadline)
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
        let deadline = Instant::now() + Duration::from_secs(2);
        let listing = project
            .list_project_directory(".", deadline, &cancel)
            .unwrap();
        assert_eq!(listing.names.len(), 128);
        assert!(listing.truncated);
        assert_eq!(
            project.list_project_directory("outside", deadline, &cancel),
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
        let deadline = Instant::now() + Duration::from_secs(2);
        assert_eq!(
            project.read_project_page("binary", 0, 16, deadline, &cancel),
            Err(ProjectReadError::NotText)
        );
        std::fs::rename(&root, scratch.0.join("old")).unwrap();
        std::fs::create_dir(&root).unwrap();
        std::fs::write(root.join("binary"), "replacement").unwrap();
        assert_eq!(
            project.read_project_page("binary", 0, 16, deadline, &cancel),
            Err(ProjectReadError::Changed)
        );
        assert_eq!(
            project.list_project_directory(".", deadline, &cancel),
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
