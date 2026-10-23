//! The bounded walk, fingerprints and file digests (execution-path-guard.md,
//! "Path lists" and "Mechanism"). Blocking: `lstat`, `readdir`, `open`,
//! `read` and SHA-256 run only on the path guard worker and the slot setup
//! and cleanup threads (AE-02), never on the reactor. No time bound: a walk
//! ends by completion, its cancel flag or an I/O error (AE-04).
//!
//! The walk starts at the project root, never follows a symlink (`lstat`
//! only; a symlink is a listed entry, never a directory), never crosses a
//! mount (a directory on another device is not entered), and skips the
//! [`SKIPPED`](super::lists::SKIPPED) directories. A directory is entered by
//! path; its identity is checked before and after its listing, and a
//! directory replaced in between is not listed (the walk reports
//! `overflow`). This narrows, but does not close, a rename race by a
//! concurrent writer; the guard is a warning, not a boundary.
use super::lists::{self, List, MAX_ENTRIES, MAX_HASHED_BYTES};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};

/// Device, inode, size, modification time in nanoseconds and file type of
/// one path, from `lstat` (no symlink followed).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Fingerprint {
    pub device: u64,
    pub inode: u64,
    pub size: u64,
    pub mtime_ns: i128,
    /// `st_mode & S_IFMT`.
    pub kind: u32,
}
impl Fingerprint {
    pub(crate) fn of(m: &std::fs::Metadata) -> Self {
        Self {
            device: m.dev(),
            inode: m.ino(),
            size: m.size(),
            mtime_ns: i128::from(m.mtime()) * 1_000_000_000 + i128::from(m.mtime_nsec()),
            kind: m.mode() & 0o170_000,
        }
    }
}

/// One walk's kept paths, in byte order of the project-relative path, and
/// whether a bound was passed.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Walk {
    pub paths: BTreeMap<String, Fingerprint>,
    /// More entries than [`MAX_ENTRIES`], more listed paths than the list's
    /// bound, a path over 1,024 bytes or not UTF-8, or a directory replaced
    /// during its listing.
    pub overflow: bool,
}

/// Why a walk produced no result.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WalkError {
    Cancelled,
    /// The root is not a directory, or could not be read.
    Unavailable,
}

fn cancelled(cancel: &AtomicBool) -> Result<(), WalkError> {
    if cancel.load(Ordering::Acquire) {
        Err(WalkError::Cancelled)
    } else {
        Ok(())
    }
}

/// The paths kept per list.
pub(crate) fn bound(list: List) -> usize {
    match list {
        List::Implicit => lists::MAX_IMPLICIT_PATHS,
        List::BuildTest => lists::MAX_BUILD_PATHS,
    }
}

fn same_directory(a: &std::fs::Metadata, b: &std::fs::Metadata) -> bool {
    a.is_dir() && b.is_dir() && a.dev() == b.dev() && a.ino() == b.ino()
}

/// Walks `root` for the files of `list`. Directory entries are visited in
/// byte order of their names, depth first; at most [`MAX_ENTRIES`] entries
/// are visited, and the first [`bound`] listed paths in byte order are kept.
pub(crate) fn walk(root: &Path, list: List, cancel: &AtomicBool) -> Result<Walk, WalkError> {
    let top = std::fs::symlink_metadata(root).map_err(|_| WalkError::Unavailable)?;
    if !top.is_dir() {
        return Err(WalkError::Unavailable);
    }
    let device = top.dev();
    let mut out = Walk::default();
    let mut visited = 0usize;
    // Directories still to list: relative path and the identity seen.
    let mut stack: Vec<(String, std::fs::Metadata)> = vec![(String::new(), top)];
    while let Some((relative, seen)) = stack.pop() {
        cancelled(cancel)?;
        let full = if relative.is_empty() {
            root.to_path_buf()
        } else {
            root.join(&relative)
        };
        let listing = match std::fs::read_dir(&full) {
            Ok(listing) => listing,
            Err(_) if relative.is_empty() => return Err(WalkError::Unavailable),
            Err(_) => {
                out.overflow = true;
                continue;
            }
        };
        let mut names = Vec::new();
        for entry in listing {
            cancelled(cancel)?;
            let Ok(entry) = entry else {
                out.overflow = true;
                break;
            };
            match entry.file_name().into_string() {
                Ok(name) => names.push(name),
                // Not UTF-8: not recordable; changes there are not detected.
                Err(_) => out.overflow = true,
            }
            if names.len() > MAX_ENTRIES {
                break;
            }
        }
        // The directory listed must still be the one entered.
        match std::fs::symlink_metadata(&full) {
            Ok(after) if same_directory(&seen, &after) => {}
            _ if relative.is_empty() => return Err(WalkError::Unavailable),
            _ => {
                out.overflow = true;
                continue;
            }
        }
        names.sort_unstable_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
        let mut directories = Vec::new();
        for name in names {
            cancelled(cancel)?;
            visited += 1;
            if visited > MAX_ENTRIES {
                out.overflow = true;
                stack.clear();
                directories.clear();
                break;
            }
            let path = if relative.is_empty() {
                name
            } else {
                format!("{relative}/{name}")
            };
            let Ok(meta) = std::fs::symlink_metadata(root.join(&path)) else {
                // Removed since the listing.
                continue;
            };
            if meta.is_dir() {
                let parts: Vec<&str> = path.split('/').collect();
                if meta.dev() == device && lists::enters(list, &parts) {
                    directories.push((path, meta));
                }
                continue;
            }
            if !lists::listed(list, &path) {
                continue;
            }
            if path.len() > lists_path_bytes() {
                out.overflow = true;
                continue;
            }
            out.paths.insert(path, Fingerprint::of(&meta));
        }
        // Depth first in byte order: the stack pops the first name next.
        directories.reverse();
        stack.extend(directories);
    }
    let keep = bound(list);
    if out.paths.len() > keep {
        out.overflow = true;
        let cut = out.paths.keys().nth(keep).cloned().expect("past the bound");
        out.paths.split_off(&cut);
    }
    Ok(out)
}

/// The largest recordable path (kind 32 field 6).
fn lists_path_bytes() -> usize {
    asura_storage::authority::conversation::MAX_GUARD_PATH
}

/// The paths whose fingerprint differs between two snapshots: added,
/// removed or changed, each once, in byte order.
pub(crate) fn changed(before: &Walk, after: &Walk) -> Vec<String> {
    let mut out: Vec<String> = before
        .paths
        .iter()
        .filter(|(path, print)| after.paths.get(*path) != Some(print))
        .map(|(path, _)| path.clone())
        .collect();
    out.extend(
        after
            .paths
            .keys()
            .filter(|path| !before.paths.contains_key(*path))
            .cloned(),
    );
    out.sort_unstable_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
    out.dedup();
    out
}

// `open(2)` flags (std exposes no constants; the values are the platform
// ABI's).
#[cfg(target_os = "macos")]
const O_NONBLOCK: i32 = 0x0004;
#[cfg(target_os = "macos")]
const O_NOFOLLOW: i32 = 0x0100;
#[cfg(target_os = "linux")]
const O_NONBLOCK: i32 = 0o4000;
#[cfg(target_os = "linux")]
const O_NOFOLLOW: i32 = 0o400000;

/// The content state of one implicit-execution file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FileState {
    /// SHA-256 of the bytes of a regular file of at most 1 MiB, or of the
    /// target of a symlink.
    Digest([u8; 32]),
    TooLarge,
    Missing,
    /// Not a regular file or symlink, a component that is not a plain
    /// directory, a file replaced while read, or an I/O error.
    Unreadable,
}

/// SHA-256 of a symlink's target: the content a program that follows the
/// link sees depends on it, so an approval covers the target name.
fn link_digest(target: &std::path::Path) -> [u8; 32] {
    use std::os::unix::ffi::OsStrExt;
    let mut hash = Sha256::new();
    hash.update(b"asura.path-guard.symlink\0");
    hash.update(target.as_os_str().as_bytes());
    hash.finalize().into()
}

/// The content state of `relative` under `root`. Each directory component
/// must be a plain directory on the root's device (no symlink followed); a
/// regular file is read only after `lstat`, and its open descriptor must be
/// the same file; at most 1 MiB is read.
pub(crate) fn digest(root: &Path, relative: &str, cancel: &AtomicBool) -> FileState {
    if cancel.load(Ordering::Acquire) {
        return FileState::Unreadable;
    }
    let Ok(top) = std::fs::symlink_metadata(root) else {
        return FileState::Unreadable;
    };
    let parts: Vec<&str> = relative.split('/').collect();
    if !top.is_dir()
        || parts
            .iter()
            .any(|p| p.is_empty() || *p == "." || *p == "..")
    {
        return FileState::Unreadable;
    }
    let mut at = root.to_path_buf();
    for part in &parts[..parts.len() - 1] {
        at.push(part);
        match std::fs::symlink_metadata(&at) {
            Ok(m) if m.is_dir() && m.dev() == top.dev() => {}
            Ok(_) => return FileState::Unreadable,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return FileState::Missing,
            Err(_) => return FileState::Unreadable,
        }
    }
    at.push(parts[parts.len() - 1]);
    let meta = match std::fs::symlink_metadata(&at) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return FileState::Missing,
        Err(_) => return FileState::Unreadable,
    };
    if meta.file_type().is_symlink() {
        return match std::fs::read_link(&at) {
            Ok(target) => FileState::Digest(link_digest(&target)),
            Err(_) => FileState::Unreadable,
        };
    }
    if !meta.is_file() {
        return FileState::Unreadable;
    }
    if meta.size() > MAX_HASHED_BYTES {
        return FileState::TooLarge;
    }
    // Nonblocking and no-follow: a FIFO or a symlink swapped in after the
    // `lstat` neither blocks the open nor redirects it.
    let opened = {
        use std::os::unix::fs::OpenOptionsExt;
        std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(O_NONBLOCK | O_NOFOLLOW)
            .open(&at)
    };
    let Ok(file) = opened else {
        return FileState::Unreadable;
    };
    match file.metadata() {
        Ok(opened) if opened.dev() == meta.dev() && opened.ino() == meta.ino() => {}
        _ => return FileState::Unreadable,
    }
    let mut hash = Sha256::new();
    let mut read = 0u64;
    let mut buffer = vec![0u8; 64 * 1024];
    let mut file = file.take(MAX_HASHED_BYTES + 1);
    loop {
        if cancel.load(Ordering::Acquire) {
            return FileState::Unreadable;
        }
        match file.read(&mut buffer) {
            Ok(0) => break,
            Ok(n) => {
                read += n as u64;
                if read > MAX_HASHED_BYTES {
                    return FileState::TooLarge;
                }
                hash.update(&buffer[..n]);
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => return FileState::Unreadable,
        }
    }
    FileState::Digest(hash.finalize().into())
}
