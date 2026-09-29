//! Pinned project directory identity; no symlink traversal.
use crate::runtime::{open_at, stat_at, stat_fd};
use crate::{Error, Result};
use std::ffi::CString;
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::{Component, Path};
pub struct ProjectIdentity {
    location: String,
    held: OwnedFd,
    pub device: u64,
    pub inode: u64,
}
impl ProjectIdentity {
    pub fn open(location: &str) -> Result<Self> {
        if location.len() > 4096
            || !location.starts_with('/')
            || (location.len() > 1 && location.ends_with('/'))
            || location.contains("//")
        {
            return Err(Error::UnsafeRuntime);
        }
        let mut held = open_at(libc::AT_FDCWD, c"/", libc::O_RDONLY | libc::O_DIRECTORY, 0)?;
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
                }
                _ => return Err(Error::UnsafeRuntime),
            }
        }
        // Path components normalize dots; reject their spelling independently.
        if location.split('/').any(|part| part == "." || part == "..") {
            return Err(Error::UnsafeRuntime);
        }
        let stat = stat_fd(held.as_raw_fd())?;
        Ok(Self {
            location: location.into(),
            held,
            device: stat.st_dev as u64,
            inode: stat.st_ino,
        })
    }
    pub(crate) fn process_location(&self) -> &str {
        &self.location
    }
    pub(crate) fn process_directory(&self) -> Result<OwnedFd> {
        self.validate()?;
        self.held.try_clone().map_err(Error::from)
    }
    pub fn validate(&self) -> Result<()> {
        let current = Self::open(&self.location)?;
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
            ProjectIdentity::open(self.0.to_str().unwrap()).unwrap()
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
        let project = ProjectIdentity::open(root.to_str().unwrap()).unwrap();
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
