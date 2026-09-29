//! Metadata-only footprint measurement on a retained storage worker.
use super::*;
use std::{
    os::fd::{FromRawFd, IntoRawFd},
    sync::atomic::{AtomicBool, Ordering},
};
fn active(deadline: Instant, cancel: &AtomicBool) -> Result<()> {
    if cancel.load(Ordering::Acquire) || Instant::now() >= deadline {
        Err(Error::Deadline)
    } else {
        Ok(())
    }
}
struct Stream(*mut libc::DIR);
impl Drop for Stream {
    fn drop(&mut self) {
        // SAFETY: uniquely owns successful fdopendir allocation.
        unsafe {
            libc::closedir(self.0);
        }
    }
}
struct Budget {
    entries: usize,
    directories: usize,
    bytes: u64,
}
fn walk(
    fd: &OwnedFd,
    uid: libc::uid_t,
    depth: usize,
    budget: &mut Budget,
    deadline: Instant,
    cancel: &AtomicBool,
) -> Result<()> {
    active(deadline, cancel)?;
    budget.directories += 1;
    if depth > 8 || budget.directories > 64 {
        return Err(Error::Unavailable);
    }
    let before = stat_fd(fd.as_raw_fd())?;
    valid_stat(&before, uid, libc::S_IFDIR, None)?;
    validate_acl(fd.as_raw_fd(), uid)?;
    let reader =
        open_at(fd.as_raw_fd(), c".", libc::O_RDONLY | libc::O_DIRECTORY, 0)?.into_raw_fd();
    // SAFETY: new directory descriptor transfers ownership only on success.
    let raw = unsafe { libc::fdopendir(reader) };
    if raw.is_null() {
        let error = crate::last_error();
        unsafe {
            drop(OwnedFd::from_raw_fd(reader));
        }
        return Err(error);
    }
    let stream = Stream(raw);
    loop {
        active(deadline, cancel)?;
        // SAFETY: errno is thread-local and stream is uniquely held through iteration.
        unsafe {
            *libc::__error() = 0;
        }
        let entry = unsafe { libc::readdir(stream.0) };
        if entry.is_null() {
            if unsafe { *libc::__error() } != 0 {
                return Err(crate::last_error());
            }
            break;
        }
        let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) };
        if name == c"." || name == c".." {
            continue;
        }
        budget.entries += 1;
        if budget.entries > 20_000 {
            return Err(Error::Unavailable);
        }
        let named = stat_at(fd.as_raw_fd(), name)?;
        match named.st_mode & libc::S_IFMT {
            libc::S_IFREG => {
                valid_stat(&named, uid, libc::S_IFREG, None)?;
                let child = open_at(fd.as_raw_fd(), name, libc::O_RDONLY, 0)?;
                let held = stat_fd(child.as_raw_fd())?;
                valid_stat(&held, uid, libc::S_IFREG, None)?;
                validate_acl(child.as_raw_fd(), uid)?;
                if identity(&held) != identity(&named) || held.st_size < 0 {
                    return Err(Error::UnsafeRuntime);
                }
                budget.bytes = budget
                    .bytes
                    .checked_add(held.st_size as u64)
                    .ok_or(Error::Unavailable)?;
                if identity(&stat_at(fd.as_raw_fd(), name)?) != identity(&held) {
                    return Err(Error::UnsafeRuntime);
                }
            }
            libc::S_IFDIR => {
                let child = open_at(fd.as_raw_fd(), name, libc::O_RDONLY | libc::O_DIRECTORY, 0)?;
                if identity(&stat_fd(child.as_raw_fd())?) != identity(&named) {
                    return Err(Error::UnsafeRuntime);
                }
                walk(&child, uid, depth + 1, budget, deadline, cancel)?;
                if identity(&stat_at(fd.as_raw_fd(), name)?) != identity(&named) {
                    return Err(Error::UnsafeRuntime);
                }
            }
            _ => return Err(Error::UnsafeRuntime),
        }
    }
    if identity(&stat_fd(fd.as_raw_fd())?) != identity(&before) {
        return Err(Error::UnsafeRuntime);
    }
    active(deadline, cancel)
}
impl DatabaseDirectory {
    pub fn stored_bytes(&self, deadline: Instant, cancel: &AtomicBool) -> Result<u64> {
        active(deadline, cancel)?;
        self.validate()?;
        let mut budget = Budget {
            entries: 0,
            directories: 0,
            bytes: 0,
        };
        walk(&self.directory, self.uid, 0, &mut budget, deadline, cancel)?;
        self.validate()?;
        Ok(budget.bytes)
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        os::unix::fs::{DirBuilderExt, OpenOptionsExt, symlink},
        path::PathBuf,
        time::Duration,
    };
    struct Scratch(PathBuf);
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn fixture() -> (Scratch, DatabaseDirectory) {
        let s = Scratch(
            format!(
                "/private/tmp/asura-size-{:x}",
                u128::from_ne_bytes(crate::random_id())
            )
            .into(),
        );
        fs::DirBuilder::new().mode(0o700).create(&s.0).unwrap();
        let d =
            DatabaseDirectory::open(RuntimeDirectory::scratch(&s.0, true).unwrap(), true).unwrap();
        (s, d)
    }
    fn file(path: &std::path::Path, size: u64) {
        fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
            .unwrap()
            .set_len(size)
            .unwrap();
    }
    fn deadline() -> Instant {
        Instant::now() + Duration::from_secs(2)
    }
    #[test]
    fn stored_footprint_counts_files_and_rejects_links_specials_and_deadlines() {
        let (s, d) = fixture();
        let db = s.0.join(".asura/db");
        fs::DirBuilder::new()
            .mode(0o700)
            .create(db.join("nested"))
            .unwrap();
        file(&db.join("one"), 12);
        file(&db.join("nested/two"), 31);
        let cancel = AtomicBool::new(false);
        assert_eq!(d.stored_bytes(deadline(), &cancel).unwrap(), 43);
        assert!(matches!(
            d.stored_bytes(Instant::now(), &cancel),
            Err(Error::Deadline)
        ));
        assert!(matches!(
            d.stored_bytes(deadline(), &AtomicBool::new(true)),
            Err(Error::Deadline)
        ));
        symlink("one", db.join("alias")).unwrap();
        assert!(d.stored_bytes(deadline(), &cancel).is_err());
        fs::remove_file(db.join("alias")).unwrap();
        fs::hard_link(db.join("one"), db.join("alias")).unwrap();
        assert!(d.stored_bytes(deadline(), &cancel).is_err());
        fs::remove_file(db.join("alias")).unwrap();
        // SAFETY: isolated fixture descriptor and fixed child name, never user storage.
        assert_eq!(
            unsafe { libc::mkfifoat(d.directory.as_raw_fd(), c"fifo".as_ptr(), 0o600) },
            0
        );
        assert!(d.stored_bytes(deadline(), &cancel).is_err());
        fs::remove_file(db.join("fifo")).unwrap();
        fs::rename(&db, s.0.join("saved")).unwrap();
        fs::DirBuilder::new().mode(0o700).create(&db).unwrap();
        assert!(d.stored_bytes(deadline(), &cancel).is_err());
    }
    #[test]
    fn stored_footprint_directory_and_depth_limits_are_enforced() {
        let (s, d) = fixture();
        let db = s.0.join(".asura/db");
        for n in 0..64 {
            fs::DirBuilder::new()
                .mode(0o700)
                .create(db.join(format!("d{n}")))
                .unwrap();
        }
        assert!(matches!(
            d.stored_bytes(deadline(), &AtomicBool::new(false)),
            Err(Error::Unavailable)
        ));
        let (s, d) = fixture();
        let mut path = s.0.join(".asura/db");
        for _ in 0..9 {
            path.push("nested");
            fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
        }
        assert!(matches!(
            d.stored_bytes(deadline(), &AtomicBool::new(false)),
            Err(Error::Unavailable)
        ));
    }
}
