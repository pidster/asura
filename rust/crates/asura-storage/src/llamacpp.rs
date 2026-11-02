//! Files of Asura's llama.cpp server (llamacpp-backend.md): the binary
//! approval record, the model folder of GGUF links and the server log.
//!
//! Every function here does blocking file IO. Callers run it on their own
//! worker: the one-shot CLI commands, the serve step and the status sampler
//! thread. None runs on a reactor. Each function is bounded: a folder holds
//! at most [`MAX_LINKS`] links, an approved binary is read to
//! [`MAX_BINARY_BYTES`], and the record is read to [`MAX_RECORD_BYTES`].
use sha2::{Digest, Sha256};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

/// Links in the model folder.
pub const MAX_LINKS: usize = 64;
/// The longest model name (file stem).
pub const MAX_NAME_BYTES: usize = 64;
/// The largest GGUF file Asura links.
pub const MAX_MODEL_BYTES: u64 = 1 << 40;
/// The largest binary Asura approves and hashes.
pub const MAX_BINARY_BYTES: u64 = 256 << 20;
/// The approval record is a few lines.
pub const MAX_RECORD_BYTES: u64 = 16_384;
/// The log moves aside at a start past this size.
pub const LOG_ROTATE_BYTES: u64 = 8 << 20;
const RECORD: &str = "server.json";

/// The paths of one Asura home.
#[derive(Clone, Debug)]
pub struct Paths {
    /// `~/.asura/data/models/llamacpp`
    pub models: PathBuf,
    /// `~/.asura/data/llamacpp`
    pub data: PathBuf,
    /// `~/.asura/logs`
    pub logs: PathBuf,
}
impl Paths {
    /// Path projection only; no IO.
    pub fn of(runtime: &asura_platform::RuntimeDirectory) -> Self {
        let models = runtime.model_assets_path();
        let data_root = models
            .parent()
            .expect("data/models has a parent")
            .to_path_buf();
        let home = data_root.parent().expect("data has a parent").to_path_buf();
        Self {
            models: models.join("llamacpp"),
            data: data_root.join("llamacpp"),
            logs: home.join("logs"),
        }
    }
}

/// A model name: 1 to 64 bytes of `[A-Za-z0-9._-]`, not starting with `.`.
pub fn valid_name(name: &str) -> bool {
    (1..=MAX_NAME_BYTES).contains(&name.len())
        && !name.starts_with('.')
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

/// Why a file was not linked.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AddError {
    /// The file name does not end in `.gguf` or its stem breaks the name rule.
    Name,
    /// Not a regular readable file.
    NotRegular,
    /// Larger than [`MAX_MODEL_BYTES`].
    TooLarge,
    /// The first four bytes are not `GGUF`.
    NotGguf,
    /// A link of that name exists.
    Exists,
    /// The folder already holds [`MAX_LINKS`] links.
    Full,
    /// The folder could not be read or written.
    Io,
}
impl AddError {
    pub fn code(self) -> &'static str {
        match self {
            Self::Name => "llamacpp_model_name_invalid",
            Self::NotRegular => "llamacpp_model_not_a_file",
            Self::TooLarge => "llamacpp_model_too_large",
            Self::NotGguf => "llamacpp_model_not_gguf",
            Self::Exists => "llamacpp_model_exists",
            Self::Full => "llamacpp_models_full",
            Self::Io => "llamacpp_models_io",
        }
    }
}

fn private_dir(path: &Path) -> io::Result<()> {
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(path)
}

/// Links `file` into the folder as `<name>.gguf`, pointing to its real
/// path. Returns the name. The file is never copied or moved.
pub fn add(paths: &Paths, file: &Path) -> Result<String, AddError> {
    let stem = file
        .file_name()
        .and_then(|n| n.to_str())
        .and_then(|n| n.strip_suffix(".gguf"))
        .filter(|stem| valid_name(stem))
        .ok_or(AddError::Name)?
        .to_owned();
    let real = fs::canonicalize(file).map_err(|_| AddError::NotRegular)?;
    let metadata = fs::metadata(&real).map_err(|_| AddError::NotRegular)?;
    if !metadata.is_file() {
        return Err(AddError::NotRegular);
    }
    if metadata.len() > MAX_MODEL_BYTES {
        return Err(AddError::TooLarge);
    }
    let mut magic = [0u8; 4];
    File::open(&real)
        .and_then(|mut f| f.read_exact(&mut magic))
        .map_err(|_| AddError::NotGguf)?;
    if &magic != b"GGUF" {
        return Err(AddError::NotGguf);
    }
    private_dir(&paths.models).map_err(|_| AddError::Io)?;
    if list(paths).map_err(|_| AddError::Io)?.len() >= MAX_LINKS {
        return Err(AddError::Full);
    }
    let link = paths.models.join(format!("{stem}.gguf"));
    match std::os::unix::fs::symlink(&real, &link) {
        Ok(()) => Ok(stem),
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Err(AddError::Exists),
        Err(_) => Err(AddError::Io),
    }
}

/// Removes the link `name` only. Returns where its file stays, if known.
pub fn remove(paths: &Paths, name: &str) -> io::Result<Option<PathBuf>> {
    if !valid_name(name) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "name"));
    }
    let link = paths.models.join(format!("{name}.gguf"));
    let metadata = fs::symlink_metadata(&link)?;
    if !metadata.file_type().is_symlink() {
        // Asura never removes a file it did not link.
        return Err(io::Error::new(io::ErrorKind::InvalidInput, "not a link"));
    }
    let target = fs::read_link(&link).ok();
    fs::remove_file(&link)?;
    Ok(target)
}

/// One link in the folder.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Link {
    pub name: String,
    pub target: Option<PathBuf>,
    /// The target's size; `None` for a broken link.
    pub size: Option<u64>,
}

/// The links in the folder, sorted by name. A missing folder is empty.
/// Entries that are not `<valid name>.gguf` links are skipped. More than
/// [`MAX_LINKS`] links is an error.
pub fn list(paths: &Paths) -> io::Result<Vec<Link>> {
    let entries = match fs::read_dir(&paths.models) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };
    let mut links = Vec::new();
    for (index, entry) in entries.enumerate() {
        if index > 4 * MAX_LINKS {
            return Err(io::Error::other("model folder holds too many entries"));
        }
        let entry = entry?;
        let Some(name) = entry
            .file_name()
            .to_str()
            .and_then(|n| n.strip_suffix(".gguf"))
            .filter(|n| valid_name(n))
            .map(str::to_owned)
        else {
            continue;
        };
        if !entry.file_type()?.is_symlink() {
            continue;
        }
        let target = fs::read_link(entry.path()).ok();
        let size = fs::metadata(entry.path())
            .ok()
            .filter(|m| m.is_file())
            .map(|m| m.len());
        links.push(Link { name, target, size });
        if links.len() > MAX_LINKS {
            return Err(io::Error::other("model folder holds too many links"));
        }
    }
    links.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(links)
}

/// The person's approval of one `llama-server` binary.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Approval {
    /// The path as given; a Homebrew link stays the link.
    pub path: PathBuf,
    /// The real path it resolved to when approved.
    pub real_path: PathBuf,
    pub size: u64,
    pub sha256: [u8; 32],
}
impl Approval {
    pub fn digest_hex(&self) -> String {
        hex(&self.sha256)
    }
}
pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Why a binary is not approved, or does not match its approval.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ApprovalError {
    /// No regular executable file at the path.
    NotExecutable,
    /// Larger than [`MAX_BINARY_BYTES`].
    TooLarge,
    /// Not a Mach-O file.
    NotMachO,
    /// Owned by someone other than the person or root, or writable by others.
    Ownership,
    /// The record or the binary could not be read or written.
    Io,
}
impl ApprovalError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::NotExecutable => "llamacpp_server_not_executable",
            Self::TooLarge => "llamacpp_server_too_large",
            Self::NotMachO => "llamacpp_server_not_macho",
            Self::Ownership => "llamacpp_server_ownership",
            Self::Io => "llamacpp_server_io",
        }
    }
}

const MACH_O: [[u8; 4]; 4] = [
    [0xfe, 0xed, 0xfa, 0xcf],
    [0xcf, 0xfa, 0xed, 0xfe],
    [0xca, 0xfe, 0xba, 0xbe],
    [0xbe, 0xba, 0xfe, 0xca],
];

/// Resolves and hashes `path` under the approval rules.
pub fn inspect(path: &Path) -> Result<Approval, ApprovalError> {
    let real = fs::canonicalize(path).map_err(|_| ApprovalError::NotExecutable)?;
    let mut file = File::open(&real).map_err(|_| ApprovalError::NotExecutable)?;
    let metadata = file.metadata().map_err(|_| ApprovalError::Io)?;
    if !metadata.is_file() || metadata.permissions().mode() & 0o111 == 0 {
        return Err(ApprovalError::NotExecutable);
    }
    if metadata.len() > MAX_BINARY_BYTES {
        return Err(ApprovalError::TooLarge);
    }
    let uid = asura_platform::account_uid().map_err(|_| ApprovalError::Ownership)?;
    if (metadata.uid() != uid && metadata.uid() != 0) || metadata.mode() & 0o022 != 0 {
        return Err(ApprovalError::Ownership);
    }
    let mut hasher = Sha256::new();
    let mut magic = [0u8; 4];
    let mut buffer = vec![0u8; 64 * 1024];
    let mut total = 0u64;
    loop {
        let read = file.read(&mut buffer).map_err(|_| ApprovalError::Io)?;
        if read == 0 {
            break;
        }
        if total < 4 {
            let take = (4 - total as usize).min(read);
            magic[total as usize..total as usize + take].copy_from_slice(&buffer[..take]);
        }
        total += read as u64;
        if total > MAX_BINARY_BYTES {
            return Err(ApprovalError::TooLarge);
        }
        hasher.update(&buffer[..read]);
    }
    if total < 4 || !MACH_O.contains(&magic) {
        return Err(ApprovalError::NotMachO);
    }
    Ok(Approval {
        path: path.to_path_buf(),
        real_path: real,
        size: total,
        sha256: hasher.finalize().into(),
    })
}

/// Records the approval of `path`; returns the new record and the one it
/// replaced. The record is written to a private temporary file and renamed.
pub fn approve(paths: &Paths, path: &Path) -> Result<(Approval, Option<Approval>), ApprovalError> {
    let approval = inspect(path)?;
    let text = path.to_str().zip(approval.real_path.to_str());
    let Some((given, real)) = text.filter(|(a, b)| !a.contains('\n') && !b.contains('\n')) else {
        return Err(ApprovalError::NotExecutable);
    };
    let previous = record(paths).ok().flatten();
    let body = serde_json::json!({
        "path": given,
        "realPath": real,
        "size": approval.size,
        "sha256": approval.digest_hex(),
    })
    .to_string();
    private_dir(&paths.data).map_err(|_| ApprovalError::Io)?;
    let temporary = paths
        .data
        .join(format!(".{RECORD}.{}", hex(&asura_platform::random_id())));
    let written = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)?;
        file.write_all(body.as_bytes())?;
        file.sync_all()?;
        fs::rename(&temporary, paths.data.join(RECORD))
    })();
    if written.is_err() {
        let _ = fs::remove_file(&temporary);
        return Err(ApprovalError::Io);
    }
    Ok((approval, previous))
}

/// The recorded approval, if any.
pub fn record(paths: &Paths) -> io::Result<Option<Approval>> {
    let file = match File::open(paths.data.join(RECORD)) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
    };
    let mut text = String::new();
    file.take(MAX_RECORD_BYTES + 1).read_to_string(&mut text)?;
    if text.len() as u64 > MAX_RECORD_BYTES {
        return Err(io::Error::other("approval record too large"));
    }
    let invalid = || io::Error::new(io::ErrorKind::InvalidData, "approval record");
    let value: serde_json::Value = serde_json::from_str(&text).map_err(|_| invalid())?;
    let field = |name: &str| value.get(name).and_then(|v| v.as_str()).ok_or_else(invalid);
    let digest = field("sha256")?;
    let mut sha256 = [0u8; 32];
    if digest.len() != 64 {
        return Err(invalid());
    }
    for (i, byte) in sha256.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&digest[2 * i..2 * i + 2], 16).map_err(|_| invalid())?;
    }
    Ok(Some(Approval {
        path: field("path")?.into(),
        real_path: field("realPath")?.into(),
        size: value
            .get("size")
            .and_then(|v| v.as_u64())
            .ok_or_else(invalid)?,
        sha256,
    }))
}

/// The approval state at a start or a status sample.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ApprovalState {
    Unset,
    Approved(Approval),
    /// The path resolves to another file, or the file changed.
    Changed {
        record: Approval,
        current: Approval,
    },
    /// The path no longer resolves to an acceptable binary.
    Missing(Approval),
}

/// Checks the approved path against its record (LC-F3). Hashes the file.
pub fn check(paths: &Paths) -> io::Result<ApprovalState> {
    let Some(record) = record(paths)? else {
        return Ok(ApprovalState::Unset);
    };
    Ok(match inspect(&record.path) {
        Ok(current)
            if current.real_path == record.real_path
                && current.size == record.size
                && current.sha256 == record.sha256 =>
        {
            ApprovalState::Approved(record)
        }
        Ok(current) => ApprovalState::Changed { record, current },
        Err(_) => ApprovalState::Missing(record),
    })
}

/// Opens the server log for appending, moving it to `llamacpp.log.1` first
/// when it is past [`LOG_ROTATE_BYTES`].
pub fn open_log(paths: &Paths) -> io::Result<File> {
    private_dir(&paths.logs)?;
    let log = paths.logs.join("llamacpp.log");
    if fs::symlink_metadata(&log).is_ok_and(|m| m.is_file() && m.len() > LOG_ROTATE_BYTES) {
        fs::rename(&log, paths.logs.join("llamacpp.log.1"))?;
    }
    asura_platform::open_private_append(&paths.logs, "llamacpp.log").map_err(io::Error::other)
}

#[cfg(test)]
mod tests;
