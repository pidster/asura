//! Descriptor-relative audit files. Invoke only on the service's isolated audit worker.
use crate::runtime::{identity, open_at, stat_at, stat_fd, valid_stat, validate_acl};
use crate::{Error, Result, RuntimeDirectory, checked};
use std::{
    ffi::{CStr, CString},
    fs::File,
    os::{
        fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd},
        unix::fs::FileExt,
    },
    sync::atomic::{AtomicBool, Ordering},
    time::Instant,
};
const ACTIVE: &CStr = c"audit.jsonl";
fn check(deadline: Instant, cancel: &AtomicBool) -> Result<()> {
    if cancel.load(Ordering::Acquire) || Instant::now() >= deadline {
        Err(Error::Deadline)
    } else {
        Ok(())
    }
}
fn same_file(a: &libc::stat, b: &libc::stat) -> bool {
    identity(a) == identity(b)
        && a.st_size == b.st_size
        && a.st_mtime == b.st_mtime
        && a.st_mtime_nsec == b.st_mtime_nsec
        && a.st_ctime == b.st_ctime
        && a.st_ctime_nsec == b.st_ctime_nsec
}
pub struct AuditDirectory {
    runtime: RuntimeDirectory,
    root: OwnedFd,
    directory: OwnedFd,
    uid: libc::uid_t,
    before: libc::stat,
}
#[derive(Clone)]
pub struct AuditArchive {
    pub name: String,
    pub created: (i64, i64),
    before: libc::stat,
}
pub struct AuditFile {
    file: File,
    before: libc::stat,
    writable: bool,
    fenced: bool,
}
pub fn audit_archive_name(name: &str) -> bool {
    let Some(rest) = name
        .strip_prefix("audit.")
        .and_then(|s| s.strip_suffix(".jsonl"))
    else {
        return false;
    };
    let Some((epoch, sequence)) = rest.split_once('.') else {
        return false;
    };
    epoch.len() == 32
        && epoch
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        && epoch.bytes().any(|b| b != b'0')
        && sequence.len() == 20
        && sequence.bytes().all(|b| b.is_ascii_digit())
        && sequence.parse::<u64>().is_ok()
}
impl AuditDirectory {
    pub fn open(
        runtime: RuntimeDirectory,
        create: bool,
        deadline: Instant,
        cancel: &AtomicBool,
    ) -> Result<Option<Self>> {
        check(deadline, cancel)?;
        let (root, uid) = runtime.authority_root()?;
        let directory = match open_at(
            root.as_raw_fd(),
            c"logs",
            libc::O_RDONLY | libc::O_DIRECTORY,
            0,
        ) {
            Ok(fd) => fd,
            Err(Error::Absent) if !create => return Ok(None),
            Err(Error::Absent) => {
                check(deadline, cancel)?;
                // SAFETY: fixed child name under the retained validated .asura descriptor.
                checked(unsafe { libc::mkdirat(root.as_raw_fd(), c"logs".as_ptr(), 0o700) })?;
                // SAFETY: borrowed directory descriptor; synchronizes its new child entry.
                checked(unsafe { libc::fsync(root.as_raw_fd()) })?;
                open_at(
                    root.as_raw_fd(),
                    c"logs",
                    libc::O_RDONLY | libc::O_DIRECTORY,
                    0,
                )?
            }
            Err(e) => return Err(e),
        };
        let before = stat_fd(directory.as_raw_fd())?;
        let result = Self {
            runtime,
            root,
            directory,
            uid,
            before,
        };
        result.validate()?;
        check(deadline, cancel)?;
        Ok(Some(result))
    }
    pub fn validate(&self) -> Result<()> {
        self.runtime.validate()?;
        let stat = stat_fd(self.directory.as_raw_fd())?;
        valid_stat(&stat, self.uid, libc::S_IFDIR, Some(0o700))?;
        validate_acl(self.directory.as_raw_fd(), self.uid)?;
        if identity(&stat) != identity(&self.before)
            || identity(&stat_at(self.root.as_raw_fd(), c"logs")?) != identity(&stat)
        {
            return Err(Error::UnsafeRuntime);
        }
        Ok(())
    }
    pub fn open_active(
        &self,
        writable: bool,
        create: bool,
        deadline: Instant,
        cancel: &AtomicBool,
    ) -> Result<Option<AuditFile>> {
        check(deadline, cancel)?;
        self.validate()?;
        if create && !writable {
            return Err(Error::UnsafeRuntime);
        }
        let flags = if writable {
            libc::O_RDWR
        } else {
            libc::O_RDONLY
        } | if create {
            libc::O_CREAT | libc::O_EXCL
        } else {
            0
        };
        #[cfg(test)]
        fault("create")?;
        let fd = match open_at(self.directory.as_raw_fd(), ACTIVE, flags, 0o600) {
            Ok(fd) => fd,
            Err(Error::Absent) if !create => return Ok(None),
            Err(e) => return Err(e),
        };
        let before = stat_fd(fd.as_raw_fd())?;
        valid_stat(&before, self.uid, libc::S_IFREG, Some(0o600))?;
        validate_acl(fd.as_raw_fd(), self.uid)?;
        if writable {
            // SAFETY: nonblocking advisory exclusive lock applies only to our regular file.
            if unsafe { libc::flock(fd.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
                return Err(Error::OwnerBusy);
            }
        }
        let file = AuditFile {
            file: File::from(fd),
            before,
            writable,
            fenced: false,
        };
        file.validate(self)?;
        if create {
            self.sync(deadline, cancel)?;
        }
        check(deadline, cancel)?;
        Ok(Some(file))
    }
    pub fn sync(&self, deadline: Instant, cancel: &AtomicBool) -> Result<()> {
        check(deadline, cancel)?;
        self.validate()?;
        #[cfg(test)]
        fault("directory_sync")?;
        // SAFETY: fsync borrows a held, validated directory descriptor.
        checked(unsafe { libc::fsync(self.directory.as_raw_fd()) })?;
        check(deadline, cancel)
    }
    pub fn seal(
        &self,
        mut file: AuditFile,
        name: &str,
        deadline: Instant,
        cancel: &AtomicBool,
    ) -> Result<AuditArchive> {
        if !audit_archive_name(name) {
            return Err(Error::UnsafeRuntime);
        }
        check(deadline, cancel)?;
        file.sync(self, deadline, cancel)?;
        file.validate(self)?;
        let name_c = CString::new(name).map_err(|_| Error::UnsafeRuntime)?;
        // SAFETY: both fixed/validated single-component names are relative to the retained directory.
        // RENAME_EXCL atomically refuses to overwrite any existing archive.
        #[cfg(test)]
        fault("rename")?;
        checked(unsafe {
            libc::renameatx_np(
                self.directory.as_raw_fd(),
                ACTIVE.as_ptr(),
                self.directory.as_raw_fd(),
                name_c.as_ptr(),
                libc::RENAME_EXCL,
            )
        })?;
        #[cfg(test)]
        fault("after_rename")?;
        let current = stat_fd(file.file.as_raw_fd())?;
        if identity(&current) != identity(&file.before)
            || identity(&stat_at(self.directory.as_raw_fd(), &name_c)?) != identity(&current)
        {
            return Err(Error::UnsafeRuntime);
        }
        self.sync(deadline, cancel)?;
        Ok(AuditArchive {
            name: name.into(),
            created: (current.st_birthtime, current.st_birthtime_nsec),
            before: current,
        })
    }
    pub fn archives(&self, deadline: Instant, cancel: &AtomicBool) -> Result<Vec<AuditArchive>> {
        self.validate()?;
        check(deadline, cancel)?;
        let fd = open_at(
            self.directory.as_raw_fd(),
            c".",
            libc::O_RDONLY | libc::O_DIRECTORY,
            0,
        )?;
        let raw = fd.into_raw_fd();
        // SAFETY: fdopendir takes exclusive ownership on success; close it ourselves on failure.
        let stream = unsafe { libc::fdopendir(raw) };
        if stream.is_null() {
            let error = crate::last_error();
            // SAFETY: fdopendir did not take ownership on failure.
            unsafe {
                drop(OwnedFd::from_raw_fd(raw));
            }
            return Err(error);
        }
        struct DirectoryStream(*mut libc::DIR);
        impl Drop for DirectoryStream {
            fn drop(&mut self) {
                // SAFETY: this guard owns the successful fdopendir result.
                unsafe {
                    libc::closedir(self.0);
                }
            }
        }
        let stream = DirectoryStream(stream);
        let mut result = Vec::new();
        let mut examined = 0usize;
        loop {
            check(deadline, cancel)?;
            // SAFETY: errno is thread-local; readdir uses a valid uniquely owned directory stream.
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
            examined += 1;
            if examined > 20_000 {
                return Err(Error::Unavailable);
            }
            // SAFETY: successful readdir returns a terminated name until the next call.
            let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) };
            let Ok(text) = name.to_str() else { continue };
            if !audit_archive_name(text) {
                continue;
            }
            if result.len() == 10_001 {
                return Err(Error::Unavailable);
            }
            let fd = open_at(self.directory.as_raw_fd(), name, libc::O_RDONLY, 0)?;
            let before = stat_fd(fd.as_raw_fd())?;
            valid_stat(&before, self.uid, libc::S_IFREG, Some(0o600))?;
            validate_acl(fd.as_raw_fd(), self.uid)?;
            if !same_file(&before, &stat_at(self.directory.as_raw_fd(), name)?) {
                return Err(Error::UnsafeRuntime);
            }
            result.push(AuditArchive {
                name: text.into(),
                created: (before.st_birthtime, before.st_birthtime_nsec),
                before,
            });
        }
        self.validate()?;
        result.sort_by(|a, b| a.created.cmp(&b.created).then_with(|| a.name.cmp(&b.name)));
        Ok(result)
    }
    pub fn remove(
        &self,
        archive: &AuditArchive,
        deadline: Instant,
        cancel: &AtomicBool,
    ) -> Result<()> {
        check(deadline, cancel)?;
        self.validate()?;
        if !audit_archive_name(&archive.name) {
            return Err(Error::UnsafeRuntime);
        }
        let name = CString::new(archive.name.as_str()).map_err(|_| Error::UnsafeRuntime)?;
        let fd = open_at(self.directory.as_raw_fd(), &name, libc::O_RDONLY, 0)?;
        let stat = stat_fd(fd.as_raw_fd())?;
        valid_stat(&stat, self.uid, libc::S_IFREG, Some(0o600))?;
        validate_acl(fd.as_raw_fd(), self.uid)?;
        if !same_file(&stat, &archive.before)
            || !same_file(&stat, &stat_at(self.directory.as_raw_fd(), &name)?)
        {
            return Err(Error::UnsafeRuntime);
        }
        #[cfg(test)]
        fault("prune")?;
        // SAFETY: validated archive component is relative to the held validated parent; no recursive removal.
        checked(unsafe { libc::unlinkat(self.directory.as_raw_fd(), name.as_ptr(), 0) })?;
        self.sync(deadline, cancel)
    }
}
impl AuditFile {
    pub fn len(&self) -> u64 {
        self.before.st_size.max(0) as u64
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    pub fn validate(&self, dir: &AuditDirectory) -> Result<()> {
        if self.fenced {
            return Err(Error::Unavailable);
        }
        dir.validate()?;
        let stat = stat_fd(self.file.as_raw_fd())?;
        valid_stat(&stat, dir.uid, libc::S_IFREG, Some(0o600))?;
        validate_acl(self.file.as_raw_fd(), dir.uid)?;
        if !same_file(&stat, &self.before)
            || !same_file(&stat, &stat_at(dir.directory.as_raw_fd(), ACTIVE)?)
        {
            return Err(Error::UnsafeRuntime);
        }
        Ok(())
    }
    pub fn read_range(
        &self,
        dir: &AuditDirectory,
        offset: u64,
        length: usize,
        deadline: Instant,
        cancel: &AtomicBool,
    ) -> Result<Vec<u8>> {
        check(deadline, cancel)?;
        self.validate(dir)?;
        if length > 256 * 1024
            || offset
                .checked_add(length as u64)
                .is_none_or(|end| end > self.len())
        {
            return Err(Error::Unavailable);
        }
        let mut bytes = vec![0; length];
        let mut read = 0;
        while read < length {
            check(deadline, cancel)?;
            let n = self
                .file
                .read_at(&mut bytes[read..], offset + read as u64)?;
            if n == 0 {
                return Err(Error::UnsafeRuntime);
            }
            read += n;
        }
        self.validate(dir)?;
        Ok(bytes)
    }
    pub fn truncate(
        &mut self,
        dir: &AuditDirectory,
        length: u64,
        deadline: Instant,
        cancel: &AtomicBool,
    ) -> Result<()> {
        if !self.writable || length > self.len() {
            return Err(Error::UnsafeRuntime);
        }
        check(deadline, cancel)?;
        self.validate(dir)?;
        self.fenced = true;
        self.file.set_len(length)?;
        #[cfg(test)]
        fault("file_sync")?;
        self.file.sync_all()?;
        self.before = stat_fd(self.file.as_raw_fd())?;
        self.fenced = false;
        self.validate(dir)?;
        check(deadline, cancel)
    }
    pub fn append(
        &mut self,
        dir: &AuditDirectory,
        bytes: &[u8],
        deadline: Instant,
        cancel: &AtomicBool,
    ) -> Result<()> {
        if !self.writable || bytes.is_empty() || bytes.len() > 65536 {
            return Err(Error::Unavailable);
        }
        check(deadline, cancel)?;
        self.validate(dir)?;
        let offset = self.len();
        self.fenced = true;
        let mut at = 0;
        while at < bytes.len() {
            check(deadline, cancel)?;
            let n = self.file.write_at(&bytes[at..], offset + at as u64)?;
            if n == 0 {
                return Err(Error::Unavailable);
            }
            at += n;
        }
        self.before = stat_fd(self.file.as_raw_fd())?;
        if self.len() != offset + bytes.len() as u64 {
            return Err(Error::UnsafeRuntime);
        }
        self.fenced = false;
        self.validate(dir)?;
        check(deadline, cancel)
    }
    pub fn sync(
        &mut self,
        dir: &AuditDirectory,
        deadline: Instant,
        cancel: &AtomicBool,
    ) -> Result<()> {
        if !self.writable {
            return Err(Error::UnsafeRuntime);
        }
        check(deadline, cancel)?;
        self.validate(dir)?;
        self.fenced = true;
        #[cfg(test)]
        fault("file_sync")?;
        self.file.sync_all()?;
        self.before = stat_fd(self.file.as_raw_fd())?;
        self.fenced = false;
        self.validate(dir)?;
        check(deadline, cancel)
    }
}

#[cfg(test)]
thread_local! { static FAULT: std::cell::Cell<Option<&'static str>> = const { std::cell::Cell::new(None) }; }
#[cfg(test)]
fn fault(stage: &'static str) -> Result<()> {
    FAULT.with(|selected| {
        if selected.get() == Some(stage) {
            selected.set(None);
            Err(Error::Unavailable)
        } else {
            Ok(())
        }
    })
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, os::unix::fs::DirBuilderExt, path::PathBuf, time::Duration};
    struct Scratch(PathBuf);
    impl Scratch {
        fn new() -> Self {
            let path = PathBuf::from(format!(
                "/private/tmp/asura-aio-{:x}",
                u128::from_ne_bytes(crate::random_id())
            ));
            fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
            Self(path)
        }
        fn dir(&self) -> AuditDirectory {
            AuditDirectory::open(
                RuntimeDirectory::scratch(&self.0, true).unwrap(),
                true,
                deadline(),
                &AtomicBool::new(false),
            )
            .unwrap()
            .unwrap()
        }
    }
    impl Drop for Scratch {
        fn drop(&mut self) {
            FAULT.with(|v| v.set(None));
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    fn deadline() -> Instant {
        Instant::now() + Duration::from_secs(10)
    }
    fn name() -> String {
        format!("audit.{}.{:020}.jsonl", "01".repeat(16), 1)
    }
    #[test]
    fn fifo_open_is_nonblocking_and_rejected() {
        let scratch = Scratch::new();
        let dir = scratch.dir();
        // SAFETY: test owns this isolated directory and creates one fixed FIFO component.
        assert_eq!(
            unsafe { libc::mkfifoat(dir.directory.as_raw_fd(), ACTIVE.as_ptr(), 0o600) },
            0
        );
        let start = Instant::now();
        assert!(
            dir.open_active(false, false, deadline(), &AtomicBool::new(false))
                .is_err()
        );
        assert!(start.elapsed() < Duration::from_secs(1));
    }
    #[test]
    fn sync_and_rename_faults_preserve_recoverable_identity_and_bytes() {
        for stage in ["file_sync", "rename", "after_rename", "directory_sync"] {
            let scratch = Scratch::new();
            let dir = scratch.dir();
            let cancel = AtomicBool::new(false);
            let mut file = dir
                .open_active(true, true, deadline(), &cancel)
                .unwrap()
                .unwrap();
            file.append(&dir, b"record\n", deadline(), &cancel).unwrap();
            FAULT.with(|v| v.set(Some(stage)));
            assert!(dir.seal(file, &name(), deadline(), &cancel).is_err());
            let archives = dir.archives(deadline(), &cancel).unwrap();
            let active = dir.open_active(false, false, deadline(), &cancel).unwrap();
            assert_eq!(archives.len() + usize::from(active.is_some()), 1);
            let path = scratch.0.join(".asura/logs").join(if active.is_some() {
                "audit.jsonl".to_owned()
            } else {
                name()
            });
            assert_eq!(fs::read(path).unwrap(), b"record\n");
        }
    }
    #[test]
    fn create_and_prune_faults_never_overwrite_or_remove_other_files() {
        let scratch = Scratch::new();
        let dir = scratch.dir();
        let cancel = AtomicBool::new(false);
        FAULT.with(|v| v.set(Some("create")));
        assert!(dir.open_active(true, true, deadline(), &cancel).is_err());
        assert!(!scratch.0.join(".asura/logs/audit.jsonl").exists());
        let mut file = dir
            .open_active(true, true, deadline(), &cancel)
            .unwrap()
            .unwrap();
        file.append(&dir, b"one\n", deadline(), &cancel).unwrap();
        let archive = dir.seal(file, &name(), deadline(), &cancel).unwrap();
        let _active = dir.open_active(true, true, deadline(), &cancel).unwrap();
        FAULT.with(|v| v.set(Some("prune")));
        assert!(dir.remove(&archive, deadline(), &cancel).is_err());
        assert_eq!(dir.archives(deadline(), &cancel).unwrap().len(), 1);
        FAULT.with(|v| v.set(Some("directory_sync")));
        assert!(dir.remove(&archive, deadline(), &cancel).is_err());
        assert!(dir.archives(deadline(), &cancel).unwrap().is_empty());
        assert!(scratch.0.join(".asura/logs/audit.jsonl").exists());
    }
    #[test]
    fn foreign_read_acl_is_rejected_without_changing_file() {
        unsafe extern "C" {
            fn acl_init(count: i32) -> *mut libc::c_void;
            fn acl_create_entry(acl: *mut *mut libc::c_void, entry: *mut *mut libc::c_void) -> i32;
            fn acl_set_tag_type(entry: *mut libc::c_void, tag: i32) -> i32;
            fn acl_set_qualifier(entry: *mut libc::c_void, qualifier: *const libc::c_void) -> i32;
            fn acl_set_permset_mask_np(entry: *mut libc::c_void, mask: u64) -> i32;
            fn acl_set_fd_np(fd: i32, acl: *mut libc::c_void, kind: i32) -> i32;
            fn acl_free(acl: *mut libc::c_void) -> i32;
            fn mbr_gid_to_uuid(gid: libc::gid_t, uuid: *mut u8) -> i32;
        }
        let scratch = Scratch::new();
        let dir = scratch.dir();
        let cancel = AtomicBool::new(false);
        let mut file = dir
            .open_active(true, true, deadline(), &cancel)
            .unwrap()
            .unwrap();
        file.append(&dir, b"private\n", deadline(), &cancel)
            .unwrap();
        // SAFETY: SDK ACL functions receive owned allocations and correctly sized
        // outputs. The only modified descriptor is this test's private scratch file.
        unsafe {
            let mut acl = acl_init(1);
            assert!(!acl.is_null());
            let mut entry = std::ptr::null_mut();
            let mut everyone = [0u8; 16];
            assert_eq!(mbr_gid_to_uuid(12, everyone.as_mut_ptr()), 0);
            assert_eq!(acl_create_entry(&mut acl, &mut entry), 0);
            assert_eq!(acl_set_tag_type(entry, 1), 0);
            assert_eq!(acl_set_qualifier(entry, everyone.as_ptr().cast()), 0);
            assert_eq!(acl_set_permset_mask_np(entry, 2), 0);
            assert_eq!(acl_set_fd_np(file.file.as_raw_fd(), acl, 0x100), 0);
            assert_eq!(acl_free(acl), 0);
        }
        drop(file);
        assert!(matches!(
            dir.open_active(false, false, deadline(), &cancel),
            Err(Error::UnsafeRuntime)
        ));
        assert_eq!(
            fs::read(scratch.0.join(".asura/logs/audit.jsonl")).unwrap(),
            b"private\n"
        );
    }
    #[test]
    fn archive_and_directory_enumeration_caps_preserve_entries() {
        use std::os::unix::fs::OpenOptionsExt;
        let scratch = Scratch::new();
        let dir = scratch.dir();
        let path = scratch.0.join(".asura/logs");
        let cancel = AtomicBool::new(false);
        // Real descriptor enumeration exercises both independent numeric bounds.
        for n in 0..10_002 {
            let name = format!("audit.{}.{n:020}.jsonl", "01".repeat(16));
            fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(path.join(name))
                .unwrap();
        }
        assert!(matches!(
            dir.archives(deadline(), &cancel),
            Err(Error::Unavailable)
        ));
        assert_eq!(fs::read_dir(&path).unwrap().count(), 10_002);
        for entry in fs::read_dir(&path).unwrap() {
            fs::remove_file(entry.unwrap().path()).unwrap();
        }
        for n in 0..20_001 {
            fs::File::create(path.join(format!("unrelated-{n}"))).unwrap();
        }
        assert!(matches!(
            dir.archives(deadline(), &cancel),
            Err(Error::Unavailable)
        ));
        assert_eq!(fs::read_dir(&path).unwrap().count(), 20_001);
    }
}
