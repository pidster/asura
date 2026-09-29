//! Strict cache inventories and bounded filesystem operations for PB0.3.
//! The caller owns the worktree lock. This module never starts a child process.
use crate::{HostToolchain, ToolchainLock};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    fs::{self, DirBuilder, File, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    path::{Component, Path},
};

pub const MAX_MANIFEST: usize = 16 * 1024 * 1024;
pub const MAX_FILES: usize = 65_536;
pub const MAX_TOTAL: u64 = 2 * 1024 * 1024 * 1024;
pub const REGISTRY: &str = "registry+https://github.com/rust-lang/crates.io-index";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CacheError {
    InvalidInput,
    UnsafePath,
    Limit,
    MissingInput,
    DigestMismatch,
    Incomplete,
    Io,
    CleanupRequired,
    CleanupAfter(Box<CacheError>),
    Archive {
        kind: crate::archive::ArchiveErrorKind,
        cleanup_failed: bool,
    },
}
impl std::fmt::Display for CacheError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for CacheError {}
impl From<std::io::Error> for CacheError {
    fn from(_: std::io::Error) -> Self {
        Self::Io
    }
}
pub type Result<T> = std::result::Result<T, CacheError>;

pub fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
pub fn valid_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
pub fn component(value: &str) -> bool {
    !value.is_empty()
        && value != "."
        && value != ".."
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-+".contains(&b))
}
pub fn relative(value: &str) -> bool {
    value.len() <= 4096 && value.split('/').all(path_component)
}
fn path_component(value: &str) -> bool {
    !value.is_empty() && value != "." && value != ".." && !value.contains(['/', '\\', '\0'])
}
pub fn real_directory(path: &Path) -> Result<()> {
    if !path.is_absolute() {
        return Err(CacheError::UnsafePath);
    }
    let mut current = std::path::PathBuf::new();
    for part in path.components() {
        match part {
            Component::RootDir | Component::Normal(_) => current.push(part),
            _ => return Err(CacheError::UnsafePath),
        }
        if !fs::symlink_metadata(&current)?.is_dir() {
            return Err(CacheError::UnsafePath);
        }
    }
    Ok(())
}
pub fn new_directory(path: &Path) -> Result<()> {
    real_directory(path.parent().ok_or(CacheError::UnsafePath)?)?;
    DirBuilder::new().mode(0o700).create(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}
pub fn ensure_directory(path: &Path) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(m) if m.is_dir() => real_directory(path),
        Ok(_) => Err(CacheError::UnsafePath),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => new_directory(path),
        Err(_) => Err(CacheError::Io),
    }
}
pub fn read_file(path: &Path, limit: usize) -> Result<Vec<u8>> {
    real_directory(path.parent().ok_or(CacheError::UnsafePath)?)?;
    let meta = fs::symlink_metadata(path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            CacheError::MissingInput
        } else {
            CacheError::Io
        }
    })?;
    if !meta.is_file() || meta.nlink() != 1 {
        return Err(CacheError::UnsafePath);
    }
    if meta.len() > limit as u64 {
        return Err(CacheError::Limit);
    }
    let mut bytes = Vec::new();
    File::open(path)?
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > limit {
        return Err(CacheError::Limit);
    }
    Ok(bytes)
}
pub fn write_new(path: &Path, bytes: &[u8], executable: bool) -> Result<()> {
    real_directory(path.parent().ok_or(CacheError::UnsafePath)?)?;
    let mode = if executable { 0o700 } else { 0o600 };
    let mut f = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(path)?;
    f.set_permissions(fs::Permissions::from_mode(mode))?;
    f.write_all(bytes)?;
    f.sync_all()?;
    Ok(())
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CargoLock {
    version: u32,
    package: Vec<LockedPackage>,
}
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LockedPackage {
    pub name: String,
    pub version: String,
    pub source: Option<String>,
    pub checksum: Option<String>,
    #[serde(default)]
    pub dependencies: Vec<String>,
}
pub fn packages(bytes: &[u8], metadata: &[u8]) -> Result<Vec<LockedPackage>> {
    if bytes.len() > 1024 * 1024 || metadata.len() > MAX_MANIFEST {
        return Err(CacheError::Limit);
    }
    let lock: CargoLock =
        toml::from_str(std::str::from_utf8(bytes).map_err(|_| CacheError::InvalidInput)?)
            .map_err(|_| CacheError::InvalidInput)?;
    if lock.version != 4 || lock.package.is_empty() || lock.package.len() > 4096 {
        return Err(CacheError::InvalidInput);
    }
    let meta: serde_json::Value =
        serde_json::from_slice(metadata).map_err(|_| CacheError::InvalidInput)?;
    let members = meta["workspace_members"]
        .as_array()
        .ok_or(CacheError::InvalidInput)?;
    let observed = meta["packages"]
        .as_array()
        .ok_or(CacheError::InvalidInput)?;
    let workspace = Path::new(
        meta["workspace_root"]
            .as_str()
            .ok_or(CacheError::InvalidInput)?,
    );
    real_directory(workspace)?;
    let mut ids = BTreeSet::new();
    for p in &lock.package {
        if !component(&p.name)
            || !component(&p.version)
            || !ids.insert((&p.name, &p.version, &p.source))
        {
            return Err(CacheError::InvalidInput);
        }
        match (&p.source, &p.checksum) {
            (Some(source), Some(checksum)) if source == REGISTRY && valid_digest(checksum) => (),
            (None, None) => {
                let valid = observed.iter().any(|m| {
                    m["name"].as_str() == Some(&p.name)
                        && m["version"].as_str() == Some(&p.version)
                        && m["source"].is_null()
                        && members.contains(&m["id"])
                        && m["manifest_path"].as_str().is_some_and(|v| {
                            Path::new(v)
                                .canonicalize()
                                .is_ok_and(|q| q.starts_with(workspace))
                        })
                });
                if !valid {
                    return Err(CacheError::InvalidInput);
                }
            }
            _ => return Err(CacheError::InvalidInput),
        }
    }
    Ok(lock.package)
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct FileRecord {
    pub path: String,
    pub size: u64,
    pub sha256: String,
    pub role: String,
    pub executable: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct PackageRecord {
    pub name: String,
    pub version: String,
    pub source: String,
    pub checksum: String,
    pub archive_path: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArchiveRecord {
    pub id: String,
    pub version: String,
    pub sha256: String,
    pub path: String,
    #[serde(deserialize_with = "required_source_commit")]
    pub source_commit: Option<String>,
}
// Nullable is not optional: the archive contract requires the key even for protoc.
fn required_source_commit<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Option<String>, D::Error> {
    Option::<String>::deserialize(deserializer)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutableRecord {
    pub id: String,
    pub path: String,
    pub sha256: String,
    pub expected_version: String,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub format_version: u32,
    pub kind: String,
    pub tool_lock_sha256: String,
    pub cargo_lock_sha256: String,
    pub host: HostToolchain,
    pub qualification: String,
    pub directories: Vec<String>,
    pub files: Vec<FileRecord>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub archives: Option<Vec<ArchiveRecord>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub executables: Option<Vec<ExecutableRecord>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub generator_build_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub packages: Option<Vec<PackageRecord>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checked_targets: Option<Vec<String>>,
}
fn role(path: &str, kind: &str) -> Result<&'static str> {
    match kind {
        "cargo" if path.starts_with("registry/cache/") && path.ends_with(".crate") => Ok("archive"),
        "cargo" if path.starts_with("registry/index/") => Ok("registry-index"),
        "tools" if path.starts_with("archives/") => Ok("archive"),
        "tools" if path == "protoc/bin/protoc" || path == "bin/protoc-gen-swift" => {
            Ok("executable")
        }
        "tools" if path.starts_with("protoc/") || path.starts_with("swift/source/") => Ok("source"),
        _ => Err(CacheError::InvalidInput),
    }
}
pub fn inventory(root: &Path, kind: &str) -> Result<(Vec<String>, Vec<FileRecord>)> {
    real_directory(root)?;
    let mut directories = Vec::new();
    let mut files = Vec::new();
    let mut pending = vec![root.to_owned()];
    let mut total = 0u64;
    while let Some(dir) = pending.pop() {
        for entry in fs::read_dir(&dir)? {
            let entry = entry?;
            let path = entry.path();
            let name = path
                .strip_prefix(root)
                .map_err(|_| CacheError::UnsafePath)?
                .to_str()
                .ok_or(CacheError::UnsafePath)?
                .to_owned();
            if !relative(&name) {
                return Err(CacheError::UnsafePath);
            }
            if name == "manifest.json" {
                continue;
            }
            let m = fs::symlink_metadata(&path)?;
            if m.is_dir() {
                directories.push(name);
                pending.push(path);
            } else if m.is_file() && m.nlink() == 1 {
                total = total.checked_add(m.len()).ok_or(CacheError::Limit)?;
                if total > MAX_TOTAL {
                    return Err(CacheError::Limit);
                }
                let file_role = role(&name, kind)?;
                let limit = if file_role == "registry-index" {
                    MAX_MANIFEST
                } else {
                    256 * 1024 * 1024
                };
                let bytes = read_file(&path, limit)?;
                files.push(FileRecord {
                    path: name,
                    size: bytes.len() as u64,
                    sha256: digest(&bytes),
                    role: file_role.into(),
                    executable: m.permissions().mode() & 0o100 != 0,
                });
            } else {
                return Err(CacheError::UnsafePath);
            }
            if directories.len() + files.len() > MAX_FILES {
                return Err(CacheError::Limit);
            }
        }
    }
    directories.sort();
    files.sort_by(|a, b| a.path.cmp(&b.path));
    Ok((directories, files))
}
impl Manifest {
    pub fn new(kind: &str, lock: &ToolchainLock, cargo: &[u8], root: &Path) -> Result<Self> {
        let (directories, files) = inventory(root, kind)?;
        Ok(Self {
            format_version: 1,
            kind: kind.into(),
            tool_lock_sha256: lock.digest().iter().map(|v| format!("{v:02x}")).collect(),
            cargo_lock_sha256: digest(cargo),
            host: lock.host().clone(),
            qualification: if kind == "tools" {
                "tools-verified"
            } else {
                "bootstrap-only"
            }
            .into(),
            directories,
            files,
            archives: None,
            executables: None,
            generator_build_path: None,
            packages: None,
            checked_targets: None,
        })
    }
    pub fn write(&self, root: &Path) -> Result<()> {
        let bytes = serde_json::to_vec_pretty(self).map_err(|_| CacheError::InvalidInput)?;
        if bytes.len() > MAX_MANIFEST {
            return Err(CacheError::Limit);
        }
        write_new(&root.join("manifest.json"), &bytes, false)
    }
    pub fn read(root: &Path, kind: &str, lock: &ToolchainLock, cargo: &[u8]) -> Result<Self> {
        Self::read_inner(root, kind, lock, cargo, false)
    }
    pub fn read_for_generator_rebuild(
        root: &Path,
        lock: &ToolchainLock,
        cargo: &[u8],
    ) -> Result<Self> {
        Self::read_inner(root, "tools", lock, cargo, true)
    }
    fn read_inner(
        root: &Path,
        kind: &str,
        lock: &ToolchainLock,
        cargo: &[u8],
        rebuild: bool,
    ) -> Result<Self> {
        let bytes = read_file(&root.join("manifest.json"), MAX_MANIFEST)?;
        let m: Self = serde_json::from_slice(&bytes).map_err(|_| CacheError::InvalidInput)?;
        let expected = Self::new(kind, lock, cargo, root)?;
        if m.format_version != 1
            || m.kind != kind
            || m.tool_lock_sha256 != expected.tool_lock_sha256
            || m.cargo_lock_sha256 != expected.cargo_lock_sha256
            || m.host != expected.host
            || m.qualification != expected.qualification
            || m.files
                .iter()
                .filter(|f| !rebuild || f.path != "bin/protoc-gen-swift")
                .ne(expected
                    .files
                    .iter()
                    .filter(|f| !rebuild || f.path != "bin/protoc-gen-swift"))
            || m.directories
                .iter()
                .filter(|d| !rebuild || d.as_str() != "bin")
                .ne(expected
                    .directories
                    .iter()
                    .filter(|d| !rebuild || d.as_str() != "bin"))
        {
            return Err(CacheError::DigestMismatch);
        }
        match kind {
            "tools"
                if m.archives.as_ref().is_some_and(|v| v.len() == 2)
                    && m.executables.as_ref().is_some_and(|v| v.len() == 2)
                    && m.generator_build_path.is_some()
                    && m.packages.is_none()
                    && m.checked_targets.is_none() => {}
            "cargo"
                if m.packages.is_some()
                    && m.checked_targets.as_deref()
                        == Some(&["asura-toolchain-bootstrap".into()])
                    && m.archives.is_none()
                    && m.executables.is_none()
                    && m.generator_build_path.is_none() => {}
            _ => return Err(CacheError::InvalidInput),
        }
        Ok(m)
    }
}

/// Copy an already inventoried input tree, preserving only owner execute status.
pub fn copy_tree(source: &Path, destination: &Path) -> Result<()> {
    real_directory(source)?;
    new_directory(destination)?;
    let mut pending = vec![(source.to_owned(), destination.to_owned())];
    let mut count = 0usize;
    let mut total = 0u64;
    while let Some((from, to)) = pending.pop() {
        for entry in fs::read_dir(from)? {
            let entry = entry?;
            let m = fs::symlink_metadata(entry.path())?;
            count += 1;
            if count > MAX_FILES {
                return Err(CacheError::Limit);
            }
            let name = entry.file_name();
            let name = name.to_str().ok_or(CacheError::UnsafePath)?;
            if !path_component(name) {
                return Err(CacheError::UnsafePath);
            }
            let target = to.join(name);
            if m.is_dir() {
                new_directory(&target)?;
                pending.push((entry.path(), target));
            } else if m.is_file() && m.nlink() == 1 {
                total = total.checked_add(m.len()).ok_or(CacheError::Limit)?;
                if total > MAX_TOTAL {
                    return Err(CacheError::Limit);
                }
                write_new(
                    &target,
                    &read_file(&entry.path(), 256 * 1024 * 1024)?,
                    m.permissions().mode() & 0o100 != 0,
                )?;
            } else {
                return Err(CacheError::UnsafePath);
            }
        }
    }
    Ok(())
}
