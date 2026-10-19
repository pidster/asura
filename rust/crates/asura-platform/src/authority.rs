//! Descriptor-relative, read-only installation inspection. No repair or initialization.
use crate::runtime::{identity, open_at, stat_at, stat_fd, valid_stat, validate_acl};
use crate::{Error, RuntimeDirectory};
use std::ffi::{CStr, CString};
use std::io;
use std::os::fd::{AsRawFd, IntoRawFd, OwnedFd, RawFd};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AuthorityError {
    Unsafe,
    Changed,
    Limit,
    Cancelled,
    Io,
}
impl std::fmt::Display for AuthorityError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for AuthorityError {}
fn map(error: Error) -> AuthorityError {
    match error {
        Error::UnsafeRuntime => AuthorityError::Unsafe,
        Error::Absent => AuthorityError::Changed,
        Error::Io(e) if e.kind() == io::ErrorKind::PermissionDenied => AuthorityError::Unsafe,
        _ => AuthorityError::Io,
    }
}
/// Inspection and replay reads have no time bound (J1-0); only cancel stops them.
fn active(cancel: &AtomicBool) -> Result<(), AuthorityError> {
    if cancel.load(Ordering::Acquire) {
        Err(AuthorityError::Cancelled)
    } else {
        Ok(())
    }
}
struct DirectoryStream(*mut libc::DIR);
impl Drop for DirectoryStream {
    fn drop(&mut self) {
        // SAFETY: this wrapper uniquely owns the stream and transferred descriptor.
        unsafe {
            libc::closedir(self.0);
        }
    }
}
fn census(fd: RawFd, cancel: &AtomicBool, limit: usize) -> Result<Vec<CString>, AuthorityError> {
    let fresh = open_at(fd, c".", libc::O_RDONLY | libc::O_DIRECTORY, 0).map_err(map)?;
    let raw = fresh.into_raw_fd();
    // SAFETY: fdopendir takes the fresh descriptor on success; failure closes it below.
    let stream = unsafe { libc::fdopendir(raw) };
    if stream.is_null() {
        // SAFETY: failed fdopendir left our descriptor owned here.
        unsafe {
            libc::close(raw);
        }
        return Err(AuthorityError::Io);
    }
    let stream = DirectoryStream(stream);
    let mut names = Vec::new();
    loop {
        active(cancel)?;
        // SAFETY: Darwin errno is thread-local; reset before readdir's EOF/error distinction.
        unsafe {
            *libc::__error() = 0;
        }
        // SAFETY: stream remains uniquely owned and open; entry copied before next call.
        let entry = unsafe { libc::readdir(stream.0) };
        if entry.is_null() {
            if io::Error::last_os_error().raw_os_error() != Some(0) {
                return Err(AuthorityError::Io);
            }
            break;
        }
        // SAFETY: readdir returns a terminated d_name inside the live entry.
        let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) };
        if [c".", c".."].contains(&name) {
            continue;
        }
        if names.len() == limit {
            return Err(AuthorityError::Limit);
        }
        names.push(name.to_owned());
    }
    Ok(names)
}
struct Node {
    fd: OwnedFd,
    parent: Option<usize>,
    name: CString,
    before: libc::stat,
    kind: libc::mode_t,
    mode: Option<libc::mode_t>,
    content: bool,
}
struct Inspection {
    runtime: RuntimeDirectory,
    uid: libc::uid_t,
    nodes: Vec<Node>,
}
fn stamp(stat: &libc::stat) -> (libc::off_t, i64, i64, i64, i64) {
    (
        stat.st_size,
        stat.st_mtime,
        stat.st_mtime_nsec,
        stat.st_ctime,
        stat.st_ctime_nsec,
    )
}
impl Inspection {
    fn validate(&self) -> Result<(), AuthorityError> {
        self.runtime
            .validate()
            .map_err(|_| AuthorityError::Changed)?;
        for node in &self.nodes {
            let held = stat_fd(node.fd.as_raw_fd()).map_err(map)?;
            if identity(&held) != identity(&node.before)
                || (node.content && stamp(&held) != stamp(&node.before))
            {
                return Err(AuthorityError::Changed);
            }
            valid_stat(&held, self.uid, node.kind, node.mode).map_err(map)?;
            validate_acl(node.fd.as_raw_fd(), self.uid).map_err(map)?;
            if let Some(parent) = node.parent {
                let named = stat_at(self.nodes[parent].fd.as_raw_fd(), &node.name).map_err(map)?;
                if identity(&named) != identity(&node.before)
                    || (node.content && stamp(&named) != stamp(&node.before))
                {
                    return Err(AuthorityError::Changed);
                }
                valid_stat(&named, self.uid, node.kind, node.mode).map_err(map)?;
            }
        }
        Ok(())
    }
    fn open(
        &mut self,
        parent: usize,
        name: &CStr,
        kind: libc::mode_t,
        mode: Option<libc::mode_t>,
        content: bool,
    ) -> Result<usize, AuthorityError> {
        let fd = open_at(
            self.nodes[parent].fd.as_raw_fd(),
            name,
            libc::O_RDONLY
                | if kind == libc::S_IFDIR {
                    libc::O_DIRECTORY
                } else {
                    0
                },
            0,
        )
        .map_err(map)?;
        let before = stat_fd(fd.as_raw_fd()).map_err(map)?;
        valid_stat(&before, self.uid, kind, mode).map_err(map)?;
        validate_acl(fd.as_raw_fd(), self.uid).map_err(map)?;
        let index = self.nodes.len();
        self.nodes.push(Node {
            fd,
            parent: Some(parent),
            name: name.to_owned(),
            before,
            kind,
            mode,
            content,
        });
        Ok(index)
    }
}
pub struct AuthorityScan {
    pub source: AuthoritySource,
    pub witness: AuthorityWitness,
}
#[derive(Clone)]
pub struct AuthorityWitness(Arc<Inspection>);
impl AuthorityWitness {
    pub fn validate(&self) -> Result<(), AuthorityError> {
        self.0.validate()
    }
}
pub enum AuthoritySource {
    RuntimeOnly,
    Remnants,
    UnsupportedLayout,
    Journal(AuthorityReader),
}
pub struct AuthorityReader {
    inspection: Arc<Inspection>,
    unknown: bool,
}
impl AuthorityReader {
    pub fn has_unknown_root_content(&self) -> bool {
        self.unknown
    }
    pub fn validate(&self) -> Result<(), AuthorityError> {
        self.inspection.validate()
    }
    /// A read-only view of the journal control directory for replay. It
    /// never writes and takes no lock; the inspection witness detects any
    /// change made while it reads.
    pub fn directory(
        &self,
        cancelled: Arc<AtomicBool>,
    ) -> Result<crate::JournalDirectory, AuthorityError> {
        active(&cancelled)?;
        self.validate()?;
        crate::JournalDirectory::open_read_only(self.inspection.runtime.clone(), cancelled).map_err(
            |error| match error {
                crate::JournalError::Cancelled => AuthorityError::Cancelled,
                crate::JournalError::Unsafe => AuthorityError::Unsafe,
                crate::JournalError::Absent => AuthorityError::Changed,
                crate::JournalError::Limit => AuthorityError::Limit,
                _ => AuthorityError::Io,
            },
        )
    }
}
impl RuntimeDirectory {
    pub fn authority_source(
        &self,
        cancelled: &AtomicBool,
    ) -> Result<AuthorityScan, AuthorityError> {
        active(cancelled)?;
        let (root, uid) = self.authority_root().map_err(map)?;
        let before = stat_fd(root.as_raw_fd()).map_err(map)?;
        let mut inspection = Inspection {
            runtime: self.clone(),
            uid,
            nodes: vec![Node {
                fd: root,
                parent: None,
                name: c".asura".to_owned(),
                before,
                kind: libc::S_IFDIR,
                mode: Some(0o700),
                content: true,
            }],
        };
        let names = census(inspection.nodes[0].fd.as_raw_fd(), cancelled, 64)?;
        let mut evidence = false;
        let mut unknown = false;
        let mut state = None;
        for name in &names {
            active(cancelled)?;
            let non_authority_directory = [c"run", c"logs"].contains(&name.as_c_str());
            let finder_metadata = name.as_c_str() == c".DS_Store";
            let non_authority = non_authority_directory || finder_metadata;
            let named = [
                c"config.yaml",
                c"db",
                c"data",
                c"sessions",
                c"tmp",
                c"state",
                // J1 retained exec_cmd output (tool job controller, payload I/O).
                c"payloads",
            ]
            .contains(&name.as_c_str());
            evidence |= !non_authority && name.as_c_str() != c"config.yaml";
            unknown |= !non_authority && !named;
            let entry = stat_at(inspection.nodes[0].fd.as_raw_fd(), name).map_err(map)?;
            let kind = entry.st_mode & libc::S_IFMT;
            if ![libc::S_IFDIR, libc::S_IFREG].contains(&kind)
                || (non_authority_directory && kind != libc::S_IFDIR)
                || (finder_metadata && kind != libc::S_IFREG)
                || (named && name.as_c_str() != c"config.yaml" && kind != libc::S_IFDIR)
                || (name.as_c_str() == c"config.yaml" && kind != libc::S_IFREG)
            {
                return Err(AuthorityError::Unsafe);
            }
            let mode = if non_authority {
                None
            } else if kind == libc::S_IFDIR {
                Some(0o700)
            } else {
                Some(0o600)
            };
            let node = inspection.open(0, name, kind, mode, name.as_c_str() == c"state")?;
            if name.as_c_str() == c"state" {
                state = Some(node);
            }
        }
        let result = if let Some(state) = state {
            let control_stat = match stat_at(inspection.nodes[state].fd.as_raw_fd(), c"control") {
                Ok(stat) => Some(stat),
                Err(Error::Absent) => None,
                Err(error) => return Err(map(error)),
            };
            if control_stat.is_none() {
                AuthoritySource::Remnants
            } else {
                let control =
                    inspection.open(state, c"control", libc::S_IFDIR, Some(0o700), true)?;
                // The journal names are checked by the read-only census;
                // only the first entry decides between remnants and a journal.
                let first = census(
                    inspection.nodes[control].fd.as_raw_fd(),
                    cancelled,
                    crate::journal_io::MAX_CENSUS,
                )
                .map(|names| names.into_iter().next());
                match first {
                    Ok(None) => AuthoritySource::Remnants,
                    Ok(Some(name)) if crate::JournalName::parse(name.to_bytes()).is_none() => {
                        AuthoritySource::UnsupportedLayout
                    }
                    Ok(Some(_)) => {
                        inspection.validate()?;
                        active(cancelled)?;
                        let inspection = Arc::new(inspection);
                        return Ok(AuthorityScan {
                            witness: AuthorityWitness(Arc::clone(&inspection)),
                            source: AuthoritySource::Journal(AuthorityReader {
                                inspection,
                                unknown,
                            }),
                        });
                    }
                    Err(error) => return Err(error),
                }
            }
        } else if evidence {
            AuthoritySource::Remnants
        } else {
            AuthoritySource::RuntimeOnly
        };
        inspection.validate()?;
        active(cancelled)?;
        Ok(AuthorityScan {
            source: result,
            witness: AuthorityWitness(Arc::new(inspection)),
        })
    }
}
