//! Person execution profiles (person-command-mode.md, platform changes).
//!
//! The service side resolves the variable roots of the `person-1` Seatbelt
//! profile into directory descriptors (FDs 12–19) and encodes the person
//! environment descriptor frame (FD 11). The guardian side reads and checks
//! that frame and generates the person profile from fixed literals and its
//! own pinned descriptor paths. Command text and client paths never enter a
//! profile. Every call here that touches the filesystem or account database
//! runs on the slot setup thread or in the guardian, never on the reactor.
use super::{ExecCmdError, Profile, guardian, spawn};
use crate::runtime::{open_at, stat_fd};
use std::ffi::{CStr, CString, OsStr};
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use zeroize::Zeroizing;

/// Most root descriptors (FDs 12–19).
pub const MAX_ROOTS: usize = 8;
/// Most cache-list roots (PCR10).
pub const MAX_CACHES: usize = 5;
/// Most environment entries (OD4).
pub const MAX_ENTRIES: usize = 1024;
/// Longest name (OD4).
pub const MAX_NAME_BYTES: usize = 1024;
/// Names and values together (OD4).
pub const MAX_ENVIRONMENT_BYTES: usize = 40_960;
/// The largest frame: 4 + 1 + 8 + 2 + 1,024 × 6 + 40,960.
pub const MAX_FRAME_BYTES: usize = 4 + 1 + MAX_ROOTS + 2 + MAX_ENTRIES * 6 + MAX_ENVIRONMENT_BYTES;
/// The first root descriptor; roots take FDs 12 to 12 + count − 1.
pub(super) const FIRST_ROOT_FD: RawFd = 12;
/// The person environment descriptor.
pub(super) const FRAME_FD: RawFd = 11;
/// The PCR10 cache list, relative to the home, one component at a time.
const CACHE_LIST: [&[&str]; MAX_CACHES] = [
    &[".cargo"],
    &[".rustup"],
    &[".cache"],
    &["Library", "Caches"],
    &[".npm"],
];
/// Longest PATH the setup accepts (PC2).
const MAX_PATH_BYTES: usize = 4096;
/// Most PATH entries (PC2).
const MAX_PATH_ENTRIES: usize = 64;
/// Longest locale value (PC3).
const MAX_LOCALE_BYTES: usize = 64;
/// Longest `SSH_AUTH_SOCK` (PC3; `sun_path` holds 104 bytes with its NUL).
const MAX_SOCKET_BYTES: usize = 103;
/// Locale names a client may send (PC3).
const LOCALE_NAMES: [&str; 8] = [
    "LANG",
    "LC_ALL",
    "LC_COLLATE",
    "LC_CTYPE",
    "LC_MESSAGES",
    "LC_MONETARY",
    "LC_NUMERIC",
    "LC_TIME",
];
/// Names the setup worker sets for `person-1`.
const SERVICE_NAMES: [&str; 7] = [
    "PATH",
    "HOME",
    "TMPDIR",
    "USER",
    "LOGNAME",
    "TERM",
    "GIT_TERMINAL_PROMPT",
];
const SSH_AUTH_SOCK: &str = "SSH_AUTH_SOCK";
/// Prefixes only the service sets.
const RESERVED_PREFIXES: [&[u8]; 2] = [b"ASURA_SECRET_", b"ASURA_JOB_"];
/// The model roots, also read and exec roots of the person profile.
pub(super) const MODEL_ROOTS: [&str; 7] = [
    "/bin",
    "/usr/bin",
    "/sbin",
    "/usr/sbin",
    "/usr/lib",
    "/usr/share",
    "/System",
];
/// The fixed system roots of the person profile. A missing one is omitted.
pub(super) const FIXED_ROOTS: [&str; 5] = [
    "/Library",
    "/Applications",
    "/opt/homebrew",
    "/usr/local",
    "/private/etc",
];
/// The link `xcode-select -p` reads, then its fallback.
const DEVELOPER_LINK: &str = "/var/db/xcode_select_link";
const COMMAND_LINE_TOOLS: &str = "/Library/Developer/CommandLineTools";

/// The role of one root descriptor in the FD 11 root table.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RootRole {
    /// 1: the account home (required).
    Home = 1,
    /// 2: the person's `$TMPDIR` (required).
    TempDir = 2,
    /// 3: the developer directory outside the fixed roots.
    Developer = 3,
    /// 4: one existing cache-list directory.
    Cache = 4,
}
impl RootRole {
    fn from_byte(byte: u8) -> Option<Self> {
        match byte {
            1 => Some(Self::Home),
            2 => Some(Self::TempDir),
            3 => Some(Self::Developer),
            4 => Some(Self::Cache),
            _ => None,
        }
    }
}

/// One `NAME=VALUE` environment entry, zeroed on drop.
struct Entry {
    name_len: usize,
    entry: Zeroizing<Vec<u8>>,
}
impl Entry {
    fn new(name: &[u8], value: &[u8]) -> Self {
        let mut entry = Zeroizing::new(Vec::with_capacity(name.len() + 1 + value.len()));
        entry.extend_from_slice(name);
        entry.push(b'=');
        entry.extend_from_slice(value);
        Self {
            name_len: name.len(),
            entry,
        }
    }
    fn name(&self) -> &[u8] {
        &self.entry[..self.name_len]
    }
    fn value(&self) -> &[u8] {
        &self.entry[self.name_len + 1..]
    }
}

/// The data of one person job: root descriptors and environment entries.
/// It travels to the guardian as FDs 12–19 and the FD 11 frame; nothing
/// here enters the guardian's own environment or argv. No `Debug`: values
/// may hold tokens from a terminal environment.
pub struct PersonSetup {
    profile: Profile,
    roots: Vec<(RootRole, OwnedFd)>,
    entries: Vec<Entry>,
}
impl PersonSetup {
    /// `person-1` setup for a `!` command (PC2, PC3). `path` is the
    /// service-sanitized PATH; `client` holds the PC3 client variables
    /// (locale names and `SSH_AUTH_SOCK`). Resolves HOME, USER and LOGNAME
    /// from the account database, `$TMPDIR` with `confstr`, the developer
    /// directory and the existing cache-list directories. Blocking: slot
    /// setup thread only; it checks `cancel` between steps.
    ///
    /// A missing home or `$TMPDIR` fails with `Unavailable`
    /// (`sandbox_unavailable`); an invalid PATH or client variable with
    /// `Invalid`; a cancel with `Cancelled`.
    pub fn sandboxed(
        path: &[u8],
        client: &[(Vec<u8>, Vec<u8>)],
        cancel: &AtomicBool,
    ) -> Result<Self, ExecCmdError> {
        let (user, home) = account()?;
        check(cancel)?;
        let tmpdir = user_temp_dir()?;
        check(cancel)?;
        let developer = developer_target(Path::new(DEVELOPER_LINK), Path::new(COMMAND_LINE_TOOLS));
        Self::resolve(
            Account {
                user,
                home,
                tmpdir,
                developer,
            },
            path,
            client,
            cancel,
        )
    }
    /// `person-1` setup with fixture account values, so tests never use
    /// the account's real home or temporary directory.
    #[cfg(feature = "test-support")]
    pub fn sandboxed_for_test(
        user: &str,
        home: &Path,
        tmpdir: &Path,
        developer: Option<&Path>,
        path: &[u8],
        client: &[(Vec<u8>, Vec<u8>)],
    ) -> Result<Self, ExecCmdError> {
        Self::resolve(
            Account {
                user: user.into(),
                home: home.to_path_buf(),
                tmpdir: tmpdir.as_os_str().as_bytes().to_vec(),
                developer: match developer {
                    Some(path) => Developer::Root(path.to_path_buf()),
                    None => Developer::Omitted,
                },
            },
            path,
            client,
            &AtomicBool::new(false),
        )
    }
    fn resolve(
        account: Account,
        path: &[u8],
        client: &[(Vec<u8>, Vec<u8>)],
        cancel: &AtomicBool,
    ) -> Result<Self, ExecCmdError> {
        check_path(path)?;
        let client = check_client(client)?;
        let home_text = account.home.as_os_str().as_bytes().to_vec();
        let home = open_directory(&account.home).ok_or(ExecCmdError::Unavailable)?;
        check(cancel)?;
        let tmpdir = open_directory(Path::new(OsStr::from_bytes(&account.tmpdir)))
            .ok_or(ExecCmdError::Unavailable)?;
        check(cancel)?;
        let mut roots = vec![(RootRole::Home, home), (RootRole::TempDir, tmpdir)];
        if let Developer::Root(target) = &account.developer
            && let Some(fd) = open_directory(target)
            && let Ok(resolved) = spawn::path(fd.as_raw_fd())
            && !covered(&resolved)
        {
            roots.push((RootRole::Developer, fd));
        }
        check(cancel)?;
        for components in CACHE_LIST {
            if let Some(fd) = cache_directory(roots[0].1.as_raw_fd(), components) {
                roots.push((RootRole::Cache, fd));
            }
        }
        check(cancel)?;
        let mut entries = vec![
            Entry::new(b"PATH", path),
            Entry::new(b"HOME", &home_text),
            Entry::new(b"TMPDIR", &account.tmpdir),
            Entry::new(b"USER", account.user.as_bytes()),
            Entry::new(b"LOGNAME", account.user.as_bytes()),
            Entry::new(b"TERM", b"dumb"),
            Entry::new(b"GIT_TERMINAL_PROMPT", b"0"),
        ];
        for (name, value) in client {
            // A socket path that does not name a socket is dropped; the
            // command still runs (PC3).
            if name == SSH_AUTH_SOCK.as_bytes() && !is_socket(value) {
                continue;
            }
            entries.push(Entry::new(name, value));
        }
        let setup = Self {
            profile: Profile::Person1,
            roots,
            entries,
        };
        setup.check_bounds()?;
        Ok(setup)
    }
    /// `person-outside-1` setup for a `!!` command: the TUI launch
    /// environment, form checked only (PCR21, OD4). Each name 1–1,024 bytes
    /// with no `=` and no NUL, each value without NUL, at most 1,024
    /// entries and 40,960 bytes, no duplicate and no `ASURA_SECRET_` or
    /// `ASURA_JOB_` name. Anything else is `Invalid`.
    pub fn outside(launch: &[(Vec<u8>, Vec<u8>)]) -> Result<Self, ExecCmdError> {
        let entries: Vec<Entry> = launch
            .iter()
            .map(|(name, value)| Entry::new(name, value))
            .collect();
        check_entries(
            entries.iter().map(|e| (e.name(), e.value())),
            Profile::PersonOutside1,
        )?;
        Ok(Self {
            profile: Profile::PersonOutside1,
            roots: Vec::new(),
            entries,
        })
    }
    /// The profile this setup belongs to.
    pub fn profile(&self) -> Profile {
        self.profile
    }
    /// The root table roles, in FD order (12 onwards).
    pub fn roles(&self) -> Vec<RootRole> {
        self.roots.iter().map(|(role, _)| *role).collect()
    }
    /// The environment names, in frame order. Names only; values never
    /// leave this type except in the frame.
    pub fn names(&self) -> Vec<String> {
        self.entries
            .iter()
            .map(|e| String::from_utf8_lossy(e.name()).into_owned())
            .collect()
    }
    pub(super) fn root_fds(&self) -> impl Iterator<Item = RawFd> + '_ {
        self.roots.iter().map(|(_, fd)| fd.as_raw_fd())
    }
    fn check_bounds(&self) -> Result<(), ExecCmdError> {
        check_roles(&self.roles(), self.profile)?;
        check_entries(
            self.entries.iter().map(|e| (e.name(), e.value())),
            self.profile,
        )
    }
    /// The FD 11 frame (big-endian): u32 length of the rest; u8 root
    /// count and one role byte per root; u16 entry count; each entry u16
    /// name length, name, u32 value length, value. Zeroed on drop.
    pub(super) fn frame(&self) -> Zeroizing<Vec<u8>> {
        let size: usize = 1
            + self.roots.len()
            + 2
            + self
                .entries
                .iter()
                .map(|e| 6 + e.entry.len() - 1)
                .sum::<usize>();
        let mut frame = Zeroizing::new(Vec::with_capacity(4 + size));
        frame.extend_from_slice(&(size as u32).to_be_bytes());
        frame.push(self.roots.len() as u8);
        frame.extend(self.roots.iter().map(|(role, _)| *role as u8));
        frame.extend_from_slice(&(self.entries.len() as u16).to_be_bytes());
        for entry in &self.entries {
            frame.extend_from_slice(&(entry.name().len() as u16).to_be_bytes());
            frame.extend_from_slice(entry.name());
            frame.extend_from_slice(&(entry.value().len() as u32).to_be_bytes());
            frame.extend_from_slice(entry.value());
        }
        frame
    }
}

fn check(cancel: &AtomicBool) -> Result<(), ExecCmdError> {
    if cancel.load(Ordering::Acquire) {
        Err(ExecCmdError::Cancelled)
    } else {
        Ok(())
    }
}

/// The account values the setup worker resolves.
struct Account {
    user: String,
    home: PathBuf,
    tmpdir: Vec<u8>,
    developer: Developer,
}

/// The account name and home of the effective UID (`getpwuid_r`).
fn account() -> Result<(String, PathBuf), ExecCmdError> {
    let mut buffer = vec![0u8; 64 * 1024];
    // SAFETY: passwd is plain output data; zero is a valid bit pattern.
    let mut entry: libc::passwd = unsafe { std::mem::zeroed() };
    let mut result = std::ptr::null_mut();
    // SAFETY: every output pointer refers to writable memory of the stated
    // capacity; geteuid has no preconditions.
    let status = unsafe {
        libc::getpwuid_r(
            libc::geteuid(),
            &mut entry,
            buffer.as_mut_ptr().cast(),
            buffer.len(),
            &mut result,
        )
    };
    if status != 0 || result.is_null() || entry.pw_dir.is_null() || entry.pw_name.is_null() {
        return Err(ExecCmdError::Unavailable);
    }
    // SAFETY: a successful getpwuid_r supplies NUL-terminated strings inside
    // `buffer`, which is alive here.
    let (name, home) = unsafe { (CStr::from_ptr(entry.pw_name), CStr::from_ptr(entry.pw_dir)) };
    let name = name.to_str().map_err(|_| ExecCmdError::Unavailable)?;
    if name.is_empty() || home.to_bytes().first() != Some(&b'/') {
        return Err(ExecCmdError::Unavailable);
    }
    Ok((
        name.into(),
        PathBuf::from(OsStr::from_bytes(home.to_bytes())),
    ))
}

/// `confstr(_CS_DARWIN_USER_TEMP_DIR)`: the person's per-user temporary
/// directory, as the person's own `TMPDIR` names it.
fn user_temp_dir() -> Result<Vec<u8>, ExecCmdError> {
    let mut buffer = vec![0u8; libc::PATH_MAX as usize];
    // SAFETY: the buffer holds `len` writable bytes; confstr writes at most
    // that many, NUL included.
    let needed = unsafe {
        libc::confstr(
            libc::_CS_DARWIN_USER_TEMP_DIR,
            buffer.as_mut_ptr().cast(),
            buffer.len(),
        )
    };
    if needed <= 1 || needed > buffer.len() {
        return Err(ExecCmdError::Unavailable);
    }
    buffer.truncate(needed - 1);
    if buffer.first() != Some(&b'/') || buffer.contains(&0) {
        return Err(ExecCmdError::Unavailable);
    }
    Ok(buffer)
}

/// The developer directory decision (PC-U52).
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum Developer {
    /// Inside a fixed root: no descriptor is needed.
    Covered,
    /// An existing absolute target outside the fixed roots: role 3.
    Root(PathBuf),
    /// Missing, relative or unreadable: omitted; the command still runs.
    Omitted,
}
/// Select the developer directory as `xcode-select -p` does, without
/// running a process: the target of `link`, or else `command_line_tools`
/// when that directory exists.
pub(super) fn developer_target(link: &Path, command_line_tools: &Path) -> Developer {
    match std::fs::read_link(link) {
        Ok(target) => classify_developer(&target),
        Err(_) if command_line_tools.is_dir() => classify_developer(command_line_tools),
        Err(_) => Developer::Omitted,
    }
}
pub(super) fn classify_developer(target: &Path) -> Developer {
    if !target.is_absolute() {
        return Developer::Omitted;
    }
    match target.to_str() {
        Some(text) if covered(text) => Developer::Covered,
        _ if target.is_dir() => Developer::Root(target.to_path_buf()),
        _ => Developer::Omitted,
    }
}
/// Whether a resolved path lies under a model or fixed root.
fn covered(path: &str) -> bool {
    MODEL_ROOTS
        .iter()
        .chain(FIXED_ROOTS.iter())
        .any(|root| guardian::within(path, root))
}

/// Open a directory read-only. `None` when it is missing or not a directory.
fn open_directory(path: &Path) -> Option<OwnedFd> {
    let path = CString::new(path.as_os_str().as_bytes()).ok()?;
    open_at(libc::AT_FDCWD, &path, libc::O_RDONLY | libc::O_DIRECTORY, 0).ok()
}
/// One cache-list directory under the home, opened one component at a
/// time without following a symlink, so it cannot resolve outside the
/// home. `None` when it is missing, a symlink or not a directory.
fn cache_directory(home: RawFd, components: &[&str]) -> Option<OwnedFd> {
    let mut held: Option<OwnedFd> = None;
    for component in components {
        let name = CString::new(*component).ok()?;
        let parent = held.as_ref().map_or(home, AsRawFd::as_raw_fd);
        held = Some(
            open_at(
                parent,
                &name,
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW,
                0,
            )
            .ok()?,
        );
    }
    held
}
/// `lstat` says the path names a socket.
fn is_socket(path: &[u8]) -> bool {
    let Ok(path) = CString::new(path) else {
        return false;
    };
    // SAFETY: plain output data; zero is a valid bit pattern.
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: a NUL-terminated path and a complete output location.
    unsafe {
        libc::lstat(path.as_ptr(), &mut st) == 0 && st.st_mode & libc::S_IFMT == libc::S_IFSOCK
    }
}

/// The sanitized PATH form (PC2): 1–4096 bytes, no NUL, at most 64
/// entries, each absolute.
pub(super) fn check_path(path: &[u8]) -> Result<(), ExecCmdError> {
    let entries: Vec<&[u8]> = path.split(|b| *b == b':').collect();
    if path.is_empty()
        || path.len() > MAX_PATH_BYTES
        || path.contains(&0)
        || entries.len() > MAX_PATH_ENTRIES
        || entries.iter().any(|e| e.first() != Some(&b'/'))
    {
        return Err(ExecCmdError::Invalid);
    }
    Ok(())
}
/// The PC3 client variables: locale names with short plain values and
/// `SSH_AUTH_SOCK` with an absolute path; no name twice.
pub(super) fn check_client(
    client: &[(Vec<u8>, Vec<u8>)],
) -> Result<&[(Vec<u8>, Vec<u8>)], ExecCmdError> {
    let mut seen: Vec<&[u8]> = Vec::new();
    for (name, value) in client {
        if seen.contains(&name.as_slice()) {
            return Err(ExecCmdError::Invalid);
        }
        seen.push(name);
        let locale = LOCALE_NAMES.iter().any(|n| n.as_bytes() == name.as_slice());
        let valid = if locale {
            !value.is_empty()
                && value.len() <= MAX_LOCALE_BYTES
                && value
                    .iter()
                    .all(|b| b.is_ascii_alphanumeric() || b"_-.@".contains(b))
        } else if name == SSH_AUTH_SOCK.as_bytes() {
            value.first() == Some(&b'/') && value.len() <= MAX_SOCKET_BYTES && !value.contains(&0)
        } else {
            false
        };
        if !valid {
            return Err(ExecCmdError::Invalid);
        }
    }
    Ok(client)
}
/// The role table: at most 8; `person-1` has home then `$TMPDIR` first,
/// each once, at most one developer root and at most five caches;
/// `person-outside-1` has none.
pub(super) fn check_roles(roles: &[RootRole], profile: Profile) -> Result<(), ExecCmdError> {
    let count = |role| roles.iter().filter(|r| **r == role).count();
    let valid = match profile {
        Profile::Person1 => {
            roles.len() <= MAX_ROOTS
                && roles.first() == Some(&RootRole::Home)
                && roles.get(1) == Some(&RootRole::TempDir)
                && count(RootRole::Home) == 1
                && count(RootRole::TempDir) == 1
                && count(RootRole::Developer) <= 1
                && count(RootRole::Cache) <= MAX_CACHES
        }
        Profile::PersonOutside1 => roles.is_empty(),
        Profile::Model1 => false,
    };
    if valid {
        Ok(())
    } else {
        Err(ExecCmdError::Invalid)
    }
}
/// The entry checks shared by the setup and the guardian (OD4 and the FD 11
/// guardian entry checks).
pub(super) fn check_entries<'a>(
    entries: impl Iterator<Item = (&'a [u8], &'a [u8])>,
    profile: Profile,
) -> Result<(), ExecCmdError> {
    let mut names: Vec<&[u8]> = Vec::new();
    let mut total = 0usize;
    for (name, value) in entries {
        total += name.len() + value.len();
        if names.len() == MAX_ENTRIES
            || name.is_empty()
            || name.len() > MAX_NAME_BYTES
            || total > MAX_ENVIRONMENT_BYTES
            || name.contains(&b'=')
            || name.contains(&0)
            || value.contains(&0)
            || names.contains(&name)
            || RESERVED_PREFIXES.iter().any(|p| name.starts_with(p))
        {
            return Err(ExecCmdError::Invalid);
        }
        if profile == Profile::Person1
            && !SERVICE_NAMES
                .iter()
                .chain(LOCALE_NAMES.iter())
                .chain(std::iter::once(&SSH_AUTH_SOCK))
                .any(|n| n.as_bytes() == name)
        {
            return Err(ExecCmdError::Invalid);
        }
        names.push(name);
    }
    if profile == Profile::Person1
        && !["PATH", "HOME", "TMPDIR"]
            .iter()
            .all(|n| names.contains(&n.as_bytes()))
    {
        return Err(ExecCmdError::Invalid);
    }
    if profile == Profile::Model1 {
        return Err(ExecCmdError::Invalid);
    }
    Ok(())
}

/// How the service's frame write ended. Each end closes the write end.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum FrameWrite {
    Complete,
    /// The guardian closed FD 11 (`EPIPE`): it exited or refused setup.
    PeerClosed,
    Cancelled,
    Failed,
}
/// Write the whole frame to the nonblocking write end, polling for
/// writability at the guardian's 100 ms pacing and checking the setup
/// cancel flag between polls. No time bound applies: the write ends on
/// completion, guardian exit or cancel. Slot setup thread only.
pub(super) fn write_frame(fd: OwnedFd, frame: &[u8], cancel: &AtomicBool) -> FrameWrite {
    let mut sent = 0;
    while sent < frame.len() {
        if cancel.load(Ordering::Acquire) {
            return FrameWrite::Cancelled;
        }
        let rest = &frame[sent..];
        // SAFETY: a bounded write from a live buffer to the owned
        // nonblocking pipe; SIGPIPE is off for this descriptor.
        let n = unsafe { libc::write(fd.as_raw_fd(), rest.as_ptr().cast(), rest.len()) };
        if n > 0 {
            sent += n as usize;
            continue;
        }
        match std::io::Error::last_os_error().raw_os_error() {
            Some(libc::EINTR) => continue,
            Some(libc::EAGAIN) => {
                let mut pfd = libc::pollfd {
                    fd: fd.as_raw_fd(),
                    events: libc::POLLOUT,
                    revents: 0,
                };
                // SAFETY: one live pollfd; a paced wait of at most 100 ms.
                unsafe {
                    libc::poll(&mut pfd, 1, 100);
                }
            }
            Some(libc::EPIPE) => return FrameWrite::PeerClosed,
            _ => return FrameWrite::Failed,
        }
    }
    FrameWrite::Complete
}

/// The checked FD 11 frame inside the guardian. Entries are `NAME=VALUE`,
/// zeroed on drop.
pub(super) struct Frame {
    pub roles: Vec<RootRole>,
    pub entries: Vec<Zeroizing<Vec<u8>>>,
}
/// Read the whole frame from FD 11, then its EOF. `stopped` reports lease
/// input, which refuses the job before the command exists. The read ends
/// on the frame and EOF, a short frame, trailing bytes, or lease input;
/// the service closes its write end on completion, cancel or loss.
pub(super) fn read_frame(
    fd: RawFd,
    lease: RawFd,
    stopped: &mut dyn FnMut() -> bool,
) -> Result<Zeroizing<Vec<u8>>, ExecCmdError> {
    spawn::nonblocking(fd)?;
    let mut frame = Zeroizing::new(Vec::with_capacity(4));
    // The header length is known once 4 bytes arrived; until then exactly
    // the header is read.
    let mut want: Option<usize> = None;
    let mut chunk = Zeroizing::new(vec![0u8; 4096]);
    loop {
        // After the header, one byte past the frame proves EOF or finds
        // trailing bytes.
        let room = match want {
            None => 4 - frame.len(),
            Some(want) => (want + 1 - frame.len()).min(chunk.len()),
        };
        // SAFETY: bounded read into a live buffer of at least `room` bytes.
        let n = unsafe { libc::read(fd, chunk.as_mut_ptr().cast(), room) };
        if n > 0 {
            frame.extend_from_slice(&chunk[..n as usize]);
            match want {
                None if frame.len() == 4 => {
                    let rest =
                        u32::from_be_bytes([frame[0], frame[1], frame[2], frame[3]]) as usize;
                    if rest > MAX_FRAME_BYTES - 4 {
                        return Err(ExecCmdError::Invalid);
                    }
                    want = Some(4 + rest);
                    frame.reserve_exact(rest);
                }
                Some(want) if frame.len() > want => return Err(ExecCmdError::Invalid),
                _ => {}
            }
            continue;
        }
        if n == 0 {
            return if want == Some(frame.len()) {
                Ok(frame)
            } else {
                Err(ExecCmdError::Invalid)
            };
        }
        if !spawn::would_block() {
            return Err(ExecCmdError::Invalid);
        }
        if stopped() {
            return Err(ExecCmdError::Cancelled);
        }
        let mut pfds = [
            libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: lease,
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        // SAFETY: two live pollfds; a paced wait of at most 100 ms.
        unsafe {
            libc::poll(pfds.as_mut_ptr(), 2, 100);
        }
    }
}
/// Parse and check a frame for `profile` (PC-U50). Any fault is `Invalid`,
/// which the guardian sends as setup-failed reason 5.
pub(super) fn parse_frame(frame: &[u8], profile: Profile) -> Result<Frame, ExecCmdError> {
    let mut cursor = Cursor(frame);
    let rest = cursor.u32()? as usize;
    if rest != cursor.0.len() || rest > MAX_FRAME_BYTES - 4 {
        return Err(ExecCmdError::Invalid);
    }
    let count = usize::from(cursor.u8()?);
    if count > MAX_ROOTS {
        return Err(ExecCmdError::Invalid);
    }
    let roles = (0..count)
        .map(|_| {
            cursor
                .u8()
                .and_then(|b| RootRole::from_byte(b).ok_or(ExecCmdError::Invalid))
        })
        .collect::<Result<Vec<_>, _>>()?;
    check_roles(&roles, profile)?;
    let entries = usize::from(cursor.u16()?);
    if entries > MAX_ENTRIES {
        return Err(ExecCmdError::Invalid);
    }
    let mut parsed = Vec::with_capacity(entries);
    for _ in 0..entries {
        let name_len = usize::from(cursor.u16()?);
        let name = cursor.take(name_len)?;
        let value_len = cursor.u32()? as usize;
        if value_len > MAX_ENVIRONMENT_BYTES {
            return Err(ExecCmdError::Invalid);
        }
        let value = cursor.take(value_len)?;
        parsed.push(Entry::new(name, value));
    }
    if !cursor.0.is_empty() {
        return Err(ExecCmdError::Invalid);
    }
    check_entries(parsed.iter().map(|e| (e.name(), e.value())), profile)?;
    Ok(Frame {
        roles,
        entries: parsed.into_iter().map(|e| e.entry).collect(),
    })
}
struct Cursor<'a>(&'a [u8]);
impl<'a> Cursor<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], ExecCmdError> {
        if n > self.0.len() {
            return Err(ExecCmdError::Invalid);
        }
        let (head, tail) = self.0.split_at(n);
        self.0 = tail;
        Ok(head)
    }
    fn u8(&mut self) -> Result<u8, ExecCmdError> {
        Ok(self.take(1)?[0])
    }
    fn u16(&mut self) -> Result<u16, ExecCmdError> {
        let b = self.take(2)?;
        Ok(u16::from_be_bytes([b[0], b[1]]))
    }
    fn u32(&mut self) -> Result<u32, ExecCmdError> {
        let b = self.take(4)?;
        Ok(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }
}

/// Validate the root descriptors FDs 12 to 12 + count − 1 as FDs 7–9 are
/// validated: open read-only directories with links. Home, `$TMPDIR` and
/// caches must belong to the effective UID; a developer directory may be
/// system owned, since the profile only reads it. Every FD from 12 + count
/// to 19 must be absent. Returns the pinned paths in role order.
pub(super) fn root_paths(roles: &[RootRole]) -> Result<Vec<(RootRole, String)>, ExecCmdError> {
    let mut paths = Vec::with_capacity(roles.len());
    for (index, role) in roles.iter().enumerate() {
        let fd = FIRST_ROOT_FD + index as RawFd;
        let st = stat_fd(fd).map_err(|_| ExecCmdError::Denied)?;
        // SAFETY: inspect the access mode of the inherited descriptor.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        // SAFETY: geteuid has no preconditions.
        let owner = *role == RootRole::Developer || st.st_uid == unsafe { libc::geteuid() };
        if flags < 0
            || flags & libc::O_ACCMODE != libc::O_RDONLY
            || st.st_mode & libc::S_IFMT != libc::S_IFDIR
            || st.st_nlink == 0
            || !owner
        {
            return Err(ExecCmdError::Denied);
        }
        paths.push((*role, guardian::pinned_path(fd)?));
    }
    absent(FIRST_ROOT_FD + roles.len() as RawFd..=FIRST_ROOT_FD + MAX_ROOTS as RawFd - 1)?;
    Ok(paths)
}
/// Every descriptor in `fds` is closed.
pub(super) fn absent(fds: std::ops::RangeInclusive<RawFd>) -> Result<(), ExecCmdError> {
    for fd in fds {
        // SAFETY: F_GETFD only reads the descriptor flags.
        if unsafe { libc::fcntl(fd, libc::F_GETFD) } >= 0 {
            return Err(ExecCmdError::Denied);
        }
    }
    Ok(())
}

/// The paths a person profile is generated from. Each comes from a pinned
/// descriptor of the guardian, or is a fixed literal; none is client text.
pub(super) struct PersonPaths<'a> {
    pub project: &'a str,
    pub scratch: &'a str,
    pub asura: &'a str,
    pub home: &'a str,
    pub tmpdir: &'a str,
    pub developer: Option<&'a str>,
    pub caches: Vec<&'a str>,
    /// The fixed system roots that exist.
    pub fixed: Vec<&'a str>,
}
impl<'a> PersonPaths<'a> {
    /// Sort the pinned root paths by role.
    pub fn from_roots(
        roots: &'a [(RootRole, String)],
        project: &'a str,
        scratch: &'a str,
        asura: &'a str,
    ) -> Result<Self, ExecCmdError> {
        let one = |role| {
            roots
                .iter()
                .find(|(r, _)| *r == role)
                .map(|(_, path)| path.as_str())
        };
        Ok(Self {
            project,
            scratch,
            asura,
            home: one(RootRole::Home).ok_or(ExecCmdError::Invalid)?,
            tmpdir: one(RootRole::TempDir).ok_or(ExecCmdError::Invalid)?,
            developer: one(RootRole::Developer),
            caches: roots
                .iter()
                .filter(|(role, _)| *role == RootRole::Cache)
                .map(|(_, path)| path.as_str())
                .collect(),
            fixed: existing_fixed_roots(),
        })
    }
}
/// The fixed system roots present on this host.
fn existing_fixed_roots() -> Vec<&'static str> {
    FIXED_ROOTS
        .iter()
        .copied()
        .filter(|root| Path::new(root).is_dir())
        .collect()
}
/// The `person-1` Seatbelt profile (person-command-mode.md, person Seatbelt
/// profile). `(deny default)`; reads and execution under the model roots,
/// the fixed system roots, home, `$TMPDIR`, the developer directory and
/// the caches; writes to the project, `$TMPDIR`, the existing caches, the
/// job's own scratch (OD1) and `/dev/null`; network allowed except Unix
/// sockets under the Asura home; no `mach-lookup` (probe PC-P1 found none
/// needed). The Asura home deny comes last, then the scratch re-allow, so
/// a home, cache or `$TMPDIR` root that contains it never reopens it.
pub(super) fn seatbelt(paths: &PersonPaths) -> Result<String, ExecCmdError> {
    let quote = guardian::quote;
    let mut reads: Vec<&str> = MODEL_ROOTS.to_vec();
    reads.extend(paths.fixed.iter().copied());
    reads.extend([paths.project, paths.scratch, paths.home, paths.tmpdir]);
    reads.extend(paths.developer);
    reads.extend(paths.caches.iter().copied());
    let mut filters = String::new();
    for root in &reads {
        filters.push_str(&format!(" (subpath {})", quote(root)?));
    }
    let mut writes = String::new();
    for root in [paths.project, paths.scratch, paths.tmpdir]
        .into_iter()
        .chain(paths.caches.iter().copied())
    {
        writes.push_str(&format!(" (subpath {})", quote(root)?));
    }
    Ok(format!(
        "(version 1)(deny default)\
         (allow file-read* file-test-existence (literal \"/\"))\
         (allow file-read* (literal \"/private/var/select/sh\"))\
         (allow file-read-metadata (literal \"/private\") (literal \"/private/var\") (literal \"/private/var/select\"))\
         (allow file-read-metadata (literal \"/var\") (literal \"/etc\") (literal \"/tmp\"))\
         (allow file-read* (literal \"/var/db/xcode_select_link\") (literal \"/private/var/db/xcode_select_link\") (literal \"/private/var/select/developer_dir\"))\
         (allow process-fork)\
         (allow process-exec file-map-executable file-read*{filters})\
         (allow file-read-metadata{metadata})\
         (allow file-write*{writes})\
         (allow sysctl-read)\
         (allow file-read* file-write* (literal \"/dev/null\"))\
         (allow file-read* (literal \"/dev/random\") (literal \"/dev/urandom\"))\
         (allow network*)\
         {state}\
         (deny network* (remote unix-socket (subpath {asura})))",
        metadata = guardian::ancestor_literals(&reads)?,
        state = guardian::state_rules(paths.asura, paths.scratch)?,
        asura = quote(paths.asura)?,
    ))
}
