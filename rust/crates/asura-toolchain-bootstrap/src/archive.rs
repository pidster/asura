//! Check locked bytes and extract only approved members into new private staging.
use crate::{ProtocLock, SwiftProtobufLock};
use flate2::bufread::GzDecoder;
use sha2::{Digest, Sha256};
use std::{
    collections::HashSet,
    fs::{self, DirBuilder, OpenOptions},
    io::{Cursor, Read, Write},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
    path::{Component, Path, PathBuf},
};

pub const MAX_COMPRESSED_BYTES: usize = 32 * 1024 * 1024;
pub const MAX_EXTRACTED_BYTES: u64 = 256 * 1024 * 1024;
#[derive(Clone, Copy)]
pub enum ArchiveSelection<'a> {
    Protoc(&'a ProtocLock),
    SwiftProtobuf(&'a SwiftProtobufLock),
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchiveErrorKind {
    CompressedLimit,
    DigestMismatch,
    InvalidArchive,
    InvalidPath,
    InvalidMember,
    Collision,
    MissingExecutable,
    ExtractedLimit,
    UnsafeDestination,
    Io,
}
#[derive(Debug)]
pub struct ArchiveError {
    pub kind: ArchiveErrorKind,
    pub cleanup_failed: bool,
}
impl std::fmt::Display for ArchiveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}; cleanup_failed={}", self.kind, self.cleanup_failed)
    }
}
impl std::error::Error for ArchiveError {}
fn err(kind: ArchiveErrorKind) -> ArchiveError {
    ArchiveError {
        kind,
        cleanup_failed: false,
    }
}
type Result<T> = std::result::Result<T, ArchiveError>;
#[derive(Debug)]
pub struct ExtractedArchive {
    path: PathBuf,
    files: u64,
    bytes: u64,
}
impl ExtractedArchive {
    pub fn path(&self) -> &Path {
        &self.path
    }
    pub fn file_count(&self) -> u64 {
        self.files
    }
    pub fn payload_bytes(&self) -> u64 {
        self.bytes
    }
}
fn member_path(raw: &[u8], directory: bool) -> Result<String> {
    let name = std::str::from_utf8(raw).map_err(|_| err(ArchiveErrorKind::InvalidPath))?;
    if name.contains(['\0', '\\']) || name.starts_with('/') {
        return Err(err(ArchiveErrorKind::InvalidPath));
    }
    let name = if directory {
        name.strip_suffix('/').unwrap_or(name)
    } else {
        name
    };
    if name
        .split('/')
        .any(|p| p.is_empty() || p == "." || p == "..")
    {
        return Err(err(ArchiveErrorKind::InvalidPath));
    }
    Ok(name.to_owned())
}
fn destination_parent(destination: &Path) -> Result<()> {
    if !destination.is_absolute() || destination.file_name().is_none() {
        return Err(err(ArchiveErrorKind::UnsafeDestination));
    }
    let mut checked = PathBuf::new();
    for part in destination
        .parent()
        .ok_or_else(|| err(ArchiveErrorKind::UnsafeDestination))?
        .components()
    {
        match part {
            Component::RootDir | Component::Normal(_) => checked.push(part),
            _ => return Err(err(ArchiveErrorKind::UnsafeDestination)),
        }
        let metadata =
            fs::symlink_metadata(&checked).map_err(|_| err(ArchiveErrorKind::UnsafeDestination))?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err(err(ArchiveErrorKind::UnsafeDestination));
        }
    }
    match fs::symlink_metadata(destination) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        _ => Err(err(ArchiveErrorKind::UnsafeDestination)),
    }
}
struct Stage<'a> {
    root: &'a Path,
    selection: ArchiveSelection<'a>,
    explicit: HashSet<String>,
    directories: HashSet<String>,
    files: u64,
    bytes: u64,
    executable: bool,
}
impl Stage<'_> {
    fn directory(&mut self, name: &str) -> Result<()> {
        if self.directories.contains(name) {
            return Ok(());
        }
        DirBuilder::new()
            .mode(0o700)
            .create(self.root.join(name))
            .map_err(|e| {
                err(if e.kind() == std::io::ErrorKind::AlreadyExists {
                    ArchiveErrorKind::Collision
                } else {
                    ArchiveErrorKind::Io
                })
            })?;
        fs::set_permissions(self.root.join(name), fs::Permissions::from_mode(0o700))
            .map_err(|_| err(ArchiveErrorKind::Io))?;
        self.directories.insert(name.to_owned());
        Ok(())
    }
    fn member(
        &mut self,
        raw: &[u8],
        directory: bool,
        executable: bool,
        size: u64,
        reader: &mut dyn Read,
    ) -> Result<()> {
        let name = member_path(raw, directory)?;
        let top = name.split('/').next().unwrap();
        let allowed = match self.selection {
            ArchiveSelection::Protoc(lock) => {
                lock.allowed_top_level().iter().any(|v| v == top)
                    && (top != "readme.txt" || (name == top && !directory))
                    && (!matches!(top, "bin" | "include") || name != top || directory)
            }
            ArchiveSelection::SwiftProtobuf(lock) => {
                top == lock.top_level() && (name != top || directory)
            }
        };
        if !allowed {
            return Err(err(ArchiveErrorKind::InvalidMember));
        }
        if !self.explicit.insert(name.clone()) {
            return Err(err(ArchiveErrorKind::Collision));
        }
        if directory && size != 0 {
            return Err(err(ArchiveErrorKind::InvalidMember));
        }
        if self
            .bytes
            .checked_add(size)
            .is_none_or(|v| v > MAX_EXTRACTED_BYTES)
        {
            return Err(err(ArchiveErrorKind::ExtractedLimit));
        }
        let components: Vec<_> = name.split('/').collect();
        for i in 1..components.len() {
            self.directory(&components[..i].join("/"))?;
        }
        if directory {
            let mut unexpected = [0u8; 1];
            if reader
                .read(&mut unexpected)
                .map_err(|_| err(ArchiveErrorKind::InvalidArchive))?
                != 0
            {
                return Err(err(ArchiveErrorKind::InvalidMember));
            }
            return self.directory(&name);
        }
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(if executable { 0o700 } else { 0o600 })
            .open(self.root.join(&name))
            .map_err(|e| {
                err(if e.kind() == std::io::ErrorKind::AlreadyExists {
                    ArchiveErrorKind::Collision
                } else {
                    ArchiveErrorKind::Io
                })
            })?;
        file.set_permissions(fs::Permissions::from_mode(if executable {
            0o700
        } else {
            0o600
        }))
        .map_err(|_| err(ArchiveErrorKind::Io))?;
        let mut count = 0u64;
        let mut buffer = [0u8; 16384];
        loop {
            let n = reader
                .read(&mut buffer)
                .map_err(|_| err(ArchiveErrorKind::InvalidArchive))?;
            if n == 0 {
                break;
            }
            count = count
                .checked_add(n as u64)
                .ok_or_else(|| err(ArchiveErrorKind::ExtractedLimit))?;
            self.bytes = self
                .bytes
                .checked_add(n as u64)
                .ok_or_else(|| err(ArchiveErrorKind::ExtractedLimit))?;
            if self.bytes > MAX_EXTRACTED_BYTES || count > size {
                return Err(err(ArchiveErrorKind::ExtractedLimit));
            }
            file.write_all(&buffer[..n])
                .map_err(|_| err(ArchiveErrorKind::Io))?;
        }
        if count != size {
            return Err(err(ArchiveErrorKind::InvalidArchive));
        }
        self.files += 1;
        if let ArchiveSelection::Protoc(lock) = self.selection
            && name == lock.required_member()
            && executable
        {
            self.executable = true;
        }
        Ok(())
    }
}
fn u16le(bytes: &[u8], offset: usize) -> Result<usize> {
    Ok(u16::from_le_bytes(
        bytes
            .get(offset..offset + 2)
            .ok_or_else(|| err(ArchiveErrorKind::InvalidArchive))?
            .try_into()
            .unwrap(),
    ) as usize)
}
fn u32le(bytes: &[u8], offset: usize) -> Result<usize> {
    Ok(u32::from_le_bytes(
        bytes
            .get(offset..offset + 4)
            .ok_or_else(|| err(ArchiveErrorKind::InvalidArchive))?
            .try_into()
            .unwrap(),
    ) as usize)
}
fn zip_extra(bytes: &[u8], mut cursor: usize, end: usize) -> Result<()> {
    while cursor < end {
        if cursor + 4 > end || u16le(bytes, cursor)? == 1 {
            return Err(err(ArchiveErrorKind::InvalidArchive));
        }
        cursor = cursor
            .checked_add(4 + u16le(bytes, cursor + 2)?)
            .filter(|&next| next <= end)
            .ok_or_else(|| err(ArchiveErrorKind::InvalidArchive))?;
    }
    Ok(())
}
fn zip_inventory(bytes: &[u8], count: usize, start: u64) -> Result<()> {
    let end = (0..bytes.len().saturating_sub(21))
        .rev()
        .find(|&p| {
            bytes.get(p..p + 4) == Some(b"PK\x05\x06")
                && u16le(bytes, p + 20).is_ok_and(|n| p + 22 + n == bytes.len())
        })
        .ok_or_else(|| err(ArchiveErrorKind::InvalidArchive))?;
    let declared = u16le(bytes, end + 10)?;
    let offset = u32le(bytes, end + 16)?;
    let size = u32le(bytes, end + 12)?;
    if u16le(bytes, end + 4)? != 0
        || u16le(bytes, end + 6)? != 0
        || u16le(bytes, end + 8)? != declared
        || declared == 65535
        || declared != count
        || offset as u64 != start
        || offset.checked_add(size) != Some(end)
    {
        return Err(err(ArchiveErrorKind::InvalidArchive));
    }
    let mut cursor = offset;
    let mut physical = 0;
    while cursor < end {
        if bytes.get(cursor..cursor + 4) != Some(b"PK\x01\x02") || cursor + 46 > end {
            return Err(err(ArchiveErrorKind::InvalidArchive));
        }
        let name = u16le(bytes, cursor + 28)?;
        let extra = u16le(bytes, cursor + 30)?;
        let comment = u16le(bytes, cursor + 32)?;
        let next = cursor
            .checked_add(46 + name + extra + comment)
            .filter(|&n| n <= end)
            .ok_or_else(|| err(ArchiveErrorKind::InvalidArchive))?;
        if u16le(bytes, cursor + 8)? & 1 != 0
            || u16le(bytes, cursor + 34)? != 0
            || [20, 24, 42]
                .iter()
                .any(|&p| u32le(bytes, cursor + p).ok() == Some(u32::MAX as usize))
        {
            return Err(err(ArchiveErrorKind::InvalidArchive));
        }
        zip_extra(bytes, cursor + 46 + name, cursor + 46 + name + extra)?;
        physical += 1;
        cursor = next;
    }
    if physical != declared {
        return Err(err(ArchiveErrorKind::InvalidArchive));
    }
    Ok(())
}
fn extract_zip(bytes: &[u8], stage: &mut Stage<'_>) -> Result<()> {
    let mut zip = zip::ZipArchive::new(Cursor::new(bytes))
        .map_err(|_| err(ArchiveErrorKind::InvalidArchive))?;
    zip_inventory(bytes, zip.len(), zip.central_directory_start())?;
    if zip.offset() != 0 {
        return Err(err(ArchiveErrorKind::InvalidArchive));
    }
    let central_start = zip.central_directory_start() as usize;
    for i in 0..zip.len() {
        let mut entry = zip
            .by_index(i)
            .map_err(|_| err(ArchiveErrorKind::InvalidArchive))?;
        let local = usize::try_from(entry.header_start())
            .map_err(|_| err(ArchiveErrorKind::InvalidArchive))?;
        if bytes.get(local..local + 4) != Some(b"PK\x03\x04") || local + 30 > central_start {
            return Err(err(ArchiveErrorKind::InvalidArchive));
        }
        let name_size = u16le(bytes, local + 26)?;
        let extra_size = u16le(bytes, local + 28)?;
        let body = local
            .checked_add(30 + name_size + extra_size)
            .filter(|&end| end <= central_start)
            .ok_or_else(|| err(ArchiveErrorKind::InvalidArchive))?;
        if bytes.get(local + 30..local + 30 + name_size) != Some(entry.name_raw())
            || u16le(bytes, local + 6)? & 1 != 0
            || [18, 22]
                .iter()
                .any(|&offset| u32le(bytes, local + offset).ok() == Some(u32::MAX as usize))
            || (body as u64)
                .checked_add(entry.compressed_size())
                .is_none_or(|end| end > central_start as u64)
        {
            return Err(err(ArchiveErrorKind::InvalidArchive));
        }
        zip_extra(bytes, local + 30 + name_size, body)?;
        let directory = entry.is_dir();
        let mode = entry
            .unix_mode()
            .unwrap_or(if directory { 0o40700 } else { 0o100600 });
        if entry.encrypted() || mode & 0o170000 != if directory { 0o040000 } else { 0o100000 } {
            return Err(err(ArchiveErrorKind::InvalidMember));
        }
        let path = entry.name_raw().to_vec();
        let size = entry.size();
        stage.member(&path, directory, mode & 0o111 != 0, size, &mut entry)?;
    }
    if !stage.executable {
        return Err(err(ArchiveErrorKind::MissingExecutable));
    }
    Ok(())
}
fn extract_tar(bytes: &[u8], stage: &mut Stage<'_>, lock: &SwiftProtobufLock) -> Result<()> {
    let mut decoder = GzDecoder::new(Cursor::new(bytes));
    let mut tar_bytes = Vec::new();
    decoder
        .by_ref()
        .take(MAX_EXTRACTED_BYTES + 1)
        .read_to_end(&mut tar_bytes)
        .map_err(|_| err(ArchiveErrorKind::InvalidArchive))?;
    if tar_bytes.len() as u64 > MAX_EXTRACTED_BYTES {
        return Err(err(ArchiveErrorKind::ExtractedLimit));
    }
    if decoder.into_inner().position() != bytes.len() as u64 {
        return Err(err(ArchiveErrorKind::InvalidArchive));
    }
    let mut archive = tar::Archive::new(Cursor::new(&tar_bytes));
    for (index, item) in archive
        .entries()
        .map_err(|_| err(ArchiveErrorKind::InvalidArchive))?
        .raw(true)
        .enumerate()
    {
        let mut entry = item.map_err(|_| err(ArchiveErrorKind::InvalidArchive))?;
        let kind = entry.header().entry_type();
        let path = entry.path_bytes().into_owned();
        let size = entry.size();
        if kind.is_pax_global_extensions()
            && index == 0
            && path == b"pax_global_header"
            && size == 52
        {
            let mut payload = Vec::new();
            entry
                .read_to_end(&mut payload)
                .map_err(|_| err(ArchiveErrorKind::InvalidArchive))?;
            if payload != format!("52 comment={}\n", lock.source_commit()).as_bytes() {
                return Err(err(ArchiveErrorKind::InvalidMember));
            }
            continue;
        }
        if !kind.is_file() && !kind.is_dir() {
            return Err(err(ArchiveErrorKind::InvalidMember));
        }
        let executable = entry
            .header()
            .mode()
            .map_err(|_| err(ArchiveErrorKind::InvalidArchive))?
            & 0o111
            != 0;
        stage.member(&path, kind.is_dir(), executable, size, &mut entry)?;
    }
    let consumed = archive.into_inner().position() as usize;
    if consumed < 512
        || tar_bytes
            .get(consumed - 512..)
            .is_none_or(|tail| tail.len() < 1024 || tail.iter().any(|&b| b != 0))
    {
        return Err(err(ArchiveErrorKind::InvalidArchive));
    }
    if stage.files == 0 {
        return Err(err(ArchiveErrorKind::InvalidMember));
    }
    Ok(())
}
/// Verify before mutation. The caller owns the real parent and the worktree lock.
pub fn extract_archive(
    bytes: &[u8],
    selection: ArchiveSelection<'_>,
    destination: &Path,
) -> Result<ExtractedArchive> {
    if bytes.len() > MAX_COMPRESSED_BYTES {
        return Err(err(ArchiveErrorKind::CompressedLimit));
    }
    let expected = match selection {
        ArchiveSelection::Protoc(lock) => lock.sha256(),
        ArchiveSelection::SwiftProtobuf(lock) => lock.sha256(),
    };
    let digest = format!("{:x}", Sha256::digest(bytes));
    if digest != expected {
        return Err(err(ArchiveErrorKind::DigestMismatch));
    }
    destination_parent(destination)?;
    DirBuilder::new()
        .mode(0o700)
        .create(destination)
        .map_err(|_| err(ArchiveErrorKind::UnsafeDestination))?;
    let mut stage = Stage {
        root: destination,
        selection,
        explicit: HashSet::new(),
        directories: HashSet::new(),
        files: 0,
        bytes: 0,
        executable: false,
    };
    let result = fs::set_permissions(destination, fs::Permissions::from_mode(0o700))
        .map_err(|_| err(ArchiveErrorKind::Io))
        .and_then(|()| match selection {
            ArchiveSelection::Protoc(_) => extract_zip(bytes, &mut stage),
            ArchiveSelection::SwiftProtobuf(lock) => extract_tar(bytes, &mut stage, lock),
        });
    match result {
        Ok(()) => Ok(ExtractedArchive {
            path: destination.to_owned(),
            files: stage.files,
            bytes: stage.bytes,
        }),
        Err(mut error) => {
            error.cleanup_failed = fs::remove_dir_all(destination).is_err();
            Err(error)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn original_paths_are_checked() {
        for name in [
            b"".as_slice(),
            b"/absolute",
            b"a/../b",
            b"a/./b",
            b"a//b",
            b"a\\b",
            b"a\0b",
            b"a//",
            b"\xff",
        ] {
            assert!(member_path(name, true).is_err());
        }
        assert_eq!(member_path(b"tree/sub/", true).unwrap(), "tree/sub");
        assert!(member_path(b"tree/", false).is_err());
    }
}
