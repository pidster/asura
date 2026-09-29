//! Owned native model process. Blocking preparation runs only on its caller's worker.
use crate::startup::{SpawnActions, SpawnAttributes, duplicate_for_spawn, spawn_result};
use crate::{Error, Result, RuntimeDirectory, checked, last_error, nonblocking};
use sha2::{Digest, Sha256};
use std::{
    ffi::CString,
    fs::File,
    io::{Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::{ffi::OsStrExt, fs::MetadataExt, net::UnixStream},
    },
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
    time::Instant,
};

struct PinnedCopy {
    directory: OwnedFd,
    name: CString,
    file: File,
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
    // SAFETY: terminated path and no-follow final component; successful FD is newly owned.
    let raw = unsafe {
        libc::open(
            source_path.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
        )
    };
    if raw < 0 {
        return Err(last_error());
    }
    let mut source = unsafe { File::from_raw_fd(raw) };
    let before = source.metadata()?;
    if !before.is_file()
        || before.uid() != uid
        || before.mode() & 0o022 != 0
        || (executable && before.mode() & 0o111 == 0)
        || before.len() == 0
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
            if executable { 0o700 } else { 0o600 },
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
    let fingerprint = |m: &std::fs::Metadata| {
        (
            m.dev(),
            m.ino(),
            m.len(),
            m.mtime(),
            m.mtime_nsec(),
            m.ctime(),
            m.ctime_nsec(),
        )
    };
    if total != before.len()
        || <[u8; 32]>::from(hasher.finalize()) != expected
        || fingerprint(&source.metadata()?) != fingerprint(&before)
    {
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
    if <[u8; 32]>::from(copied.finalize()) != expected {
        return Err(Error::UnsafeRuntime);
    }
    // Exec must not retain a writable handle to its text image. Reopen the
    // verified inode read-only and verify its identity before dropping the writer.
    // SAFETY: retained directory and terminated name; no symlink following.
    let raw = unsafe {
        libc::openat(
            pinned.directory.as_raw_fd(),
            pinned.name.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
        )
    };
    if raw < 0 {
        return Err(last_error());
    }
    // SAFETY: openat returned a new exclusively owned descriptor.
    let executable_handle = unsafe { File::from_raw_fd(raw) };
    if fingerprint(&executable_handle.metadata()?) != fingerprint(&pinned.file.metadata()?) {
        return Err(Error::UnsafeRuntime);
    }
    pinned.file = executable_handle;
    Ok(pinned)
}

struct PackageDirectory {
    parent: OwnedFd,
    name: CString,
    directory: OwnedFd,
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
    _package: Option<PackageDirectory>,
}
impl ModelProcess {
    /// Called only from the single bounded preparation worker. `helper` is the fixed package sibling.
    pub fn spawn(
        runtime: &RuntimeDirectory,
        helper: &Path,
        expected: [u8; 32],
        deadline: Instant,
        cancelled: &AtomicBool,
    ) -> Result<Self> {
        Self::spawn_with_resource(runtime, helper, expected, None, deadline, cancelled)
    }

    /// MLX requires the package sibling resource; other providers pass no digest.
    pub fn spawn_with_resource(
        runtime: &RuntimeDirectory,
        helper: &Path,
        expected: [u8; 32],
        resource_digest: Option<[u8; 32]>,
        deadline: Instant,
        cancelled: &AtomicBool,
    ) -> Result<Self> {
        let check = || {
            if cancelled.load(Ordering::Acquire) || Instant::now() >= deadline {
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
        let (directory, root, name) = if resource_digest.is_some() {
            // SAFETY: create one exclusive directory under a retained private root.
            checked(unsafe { libc::mkdirat(directory.as_raw_fd(), name.as_ptr(), 0o700) })?;
            // SAFETY: open only the new directory, without following a replacement symlink.
            let raw = unsafe {
                libc::openat(
                    directory.as_raw_fd(),
                    name.as_ptr(),
                    libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                )
            };
            if raw < 0 {
                return Err(last_error());
            }
            // SAFETY: successful open returns a newly owned descriptor.
            let child = unsafe { OwnedFd::from_raw_fd(raw) };
            let copied = child.try_clone()?;
            let child_root = root.join(name.to_str().unwrap());
            package = Some(PackageDirectory {
                parent: directory,
                name,
                directory: child,
            });
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
        let resource = if let Some(digest) = resource_digest {
            Some(verified_copy(
                pinned.directory.try_clone()?,
                CString::new("mlx.metallib").unwrap(),
                &helper.with_file_name("mlx.metallib"),
                digest,
                uid,
                false,
                64 * 1024 * 1024,
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
    use std::{os::unix::fs::PermissionsExt, path::PathBuf, time::Duration};
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
                Instant::now() + Duration::from_secs(2),
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
        let child = ModelProcess::spawn_with_resource(
            &fixture.runtime,
            &fixture.helper,
            fixture.digest,
            Some(Sha256::digest(resource).into()),
            Instant::now() + Duration::from_secs(2),
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
                ModelProcess::spawn_with_resource(
                    &fixture.runtime,
                    &fixture.helper,
                    fixture.digest,
                    Some([0; 32]),
                    Instant::now() + Duration::from_secs(2),
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
            ModelProcess::spawn_with_resource(
                &fixture.runtime,
                &fixture.helper,
                fixture.digest,
                Some([0; 32]),
                Instant::now() + Duration::from_secs(2),
                &AtomicBool::new(false),
            )
            .is_err()
        );
        assert_eq!(fixture.copies(), 0);
        let child = ModelProcess::spawn_with_resource(
            &fixture.runtime,
            &fixture.helper,
            fixture.digest,
            None,
            Instant::now() + Duration::from_secs(2),
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
        let child = ModelProcess::spawn_with_resource(
            &fixture.runtime,
            &fixture.helper,
            fixture.digest,
            Some(Sha256::digest(resource).into()),
            Instant::now() + Duration::from_secs(2),
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
                Instant::now() + Duration::from_secs(2),
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
                Instant::now() + Duration::from_secs(2),
                &AtomicBool::new(true)
            )
            .is_err()
        );
        assert_eq!(fixture.copies(), 0);
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
