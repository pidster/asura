//! Program identity (security-policy.md, program identity in PL2): the
//! Security framework static code check of a file at its canonical path,
//! with its team ID, signing ID, cdhash and platform flag. Every call here
//! blocks on file I/O and runs only on a platform worker thread (the
//! policy owner's attribute reader), never on the reactor. A failure never
//! fails a decision: it gives `identity_known = false` and empty strings.
use crate::project_identity::{WalkMode, canonical_path, lexically_canonical};
use core_foundation::base::{CFType, TCFType};
use core_foundation::data::CFData;
use core_foundation::number::CFNumber;
use core_foundation::string::CFString;
use core_foundation::url::CFURL;
use std::collections::HashMap;
use std::ffi::c_void;

/// The longest identity string kept; a longer one is left empty.
const MAX_FIELD: usize = 128;
/// Cache entries (security-policy.md, limits: program identity cache).
pub const IDENTITY_CACHE: usize = 1024;

/// The code-signing identity of one program file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CodeIdentity {
    /// The canonical path; the input path when it could not be
    /// canonicalised but is lexically canonical; else empty.
    pub path: String,
    pub identity_known: bool,
    pub platform: bool,
    pub team_id: String,
    pub signing_id: String,
    /// Lowercase hex.
    pub cdhash: String,
}
impl CodeIdentity {
    fn unknown(path: String) -> Self {
        Self {
            path,
            identity_known: false,
            platform: false,
            team_id: String::new(),
            signing_id: String::new(),
            cdhash: String::new(),
        }
    }
}

/// The file facts that key the cache and detect a change during a check.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Fingerprint {
    pub device: u64,
    pub inode: u64,
    pub size: i64,
    pub mtime: i64,
    pub mtime_nsec: i64,
}

/// The fingerprint of the file at `path`, walked canonically with
/// `O_NOFOLLOW`. `None` when the path has no canonical form.
pub fn fingerprint(path: &str) -> Option<Fingerprint> {
    let canonical = canonical_path(path, WalkMode::Read, None).ok()?;
    fingerprint_of(&canonical.path)
}
fn fingerprint_of(canonical: &str) -> Option<Fingerprint> {
    use std::os::unix::fs::MetadataExt;
    // The canonical spelling has no symbolic link, so `symlink_metadata`
    // reads the same object the walk opened.
    let meta = std::fs::symlink_metadata(canonical).ok()?;
    if meta.file_type().is_symlink() {
        return None;
    }
    Some(Fingerprint {
        device: meta.dev(),
        inode: meta.ino(),
        size: i64::try_from(meta.size()).ok()?,
        mtime: meta.mtime(),
        mtime_nsec: meta.mtime_nsec(),
    })
}

type SecStaticCodeRef = *const c_void;
type CFDictionaryRef = *const c_void;
type CFStringRef = *const c_void;
const SEC_CS_DEFAULT_FLAGS: u32 = 0;
/// `kSecCSSigningInformation`.
const SEC_CS_SIGNING_INFORMATION: u32 = 1 << 1;
/// `kSecCSDoNotValidateResources`.
const SEC_CS_DO_NOT_VALIDATE_RESOURCES: u32 = 1 << 2;

#[link(name = "Security", kind = "framework")]
unsafe extern "C" {
    fn SecStaticCodeCreateWithPath(
        path: *const c_void,
        flags: u32,
        code: *mut SecStaticCodeRef,
    ) -> i32;
    fn SecStaticCodeCheckValidity(
        code: SecStaticCodeRef,
        flags: u32,
        requirement: *const c_void,
    ) -> i32;
    fn SecCodeCopySigningInformation(
        code: SecStaticCodeRef,
        flags: u32,
        information: *mut CFDictionaryRef,
    ) -> i32;
    static kSecCodeInfoIdentifier: CFStringRef;
    static kSecCodeInfoTeamIdentifier: CFStringRef;
    static kSecCodeInfoUnique: CFStringRef;
    static kSecCodeInfoPlatformIdentifier: CFStringRef;
}
#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFDictionaryGetValue(dict: CFDictionaryRef, key: *const c_void) -> *const c_void;
    fn CFRelease(value: *const c_void);
}

/// Owns one +1 Core Foundation reference and releases it on drop.
struct Owned(*const c_void);
impl Drop for Owned {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: the pointer is a +1 reference returned by a Create or
            // Copy function, released exactly once here.
            unsafe { CFRelease(self.0) };
        }
    }
}

/// The value of `key` in a signing information dictionary, retained.
fn value(dict: &Owned, key: CFStringRef) -> Option<CFType> {
    // SAFETY: dict is a live CFDictionary and key a framework constant;
    // CFDictionaryGetValue returns a borrowed reference or null.
    let raw = unsafe { CFDictionaryGetValue(dict.0, key) };
    if raw.is_null() {
        return None;
    }
    // SAFETY: raw is a live, borrowed CF object; get rule retains it.
    Some(unsafe { CFType::wrap_under_get_rule(raw) })
}
fn string_field(dict: &Owned, key: CFStringRef) -> String {
    value(dict, key)
        .and_then(|v| v.downcast::<CFString>())
        .map(|s| s.to_string())
        .filter(|s| s.len() <= MAX_FIELD && !s.chars().any(char::is_control))
        .unwrap_or_default()
}

/// The static code check and signing fields of a canonical path.
fn check(canonical: &str) -> Option<(bool, String, String, String)> {
    let url = CFURL::from_path(canonical, false)?;
    let mut code: SecStaticCodeRef = std::ptr::null();
    // SAFETY: url is a live CFURL; code is a valid out pointer that receives
    // a +1 reference on success.
    let status = unsafe {
        SecStaticCodeCreateWithPath(
            url.as_concrete_TypeRef().cast(),
            SEC_CS_DEFAULT_FLAGS,
            &mut code,
        )
    };
    if status != 0 || code.is_null() {
        return None;
    }
    let code = Owned(code);
    // SAFETY: code is a live SecStaticCode; a null requirement checks the
    // signature itself only.
    let valid = unsafe {
        SecStaticCodeCheckValidity(code.0, SEC_CS_DO_NOT_VALIDATE_RESOURCES, std::ptr::null())
    };
    if valid != 0 {
        return None;
    }
    let mut info: CFDictionaryRef = std::ptr::null();
    // SAFETY: code is live; info receives a +1 dictionary on success.
    let status =
        unsafe { SecCodeCopySigningInformation(code.0, SEC_CS_SIGNING_INFORMATION, &mut info) };
    if status != 0 || info.is_null() {
        return None;
    }
    let info = Owned(info);
    // SAFETY: the framework constants are immutable CFStrings for the life
    // of the process.
    let (identifier, team, unique, platform) = unsafe {
        (
            kSecCodeInfoIdentifier,
            kSecCodeInfoTeamIdentifier,
            kSecCodeInfoUnique,
            kSecCodeInfoPlatformIdentifier,
        )
    };
    let cdhash = value(&info, unique)
        .and_then(|v| v.downcast::<CFData>())
        .map(|d| {
            d.bytes()
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        })
        .filter(|s| s.len() <= MAX_FIELD)
        .unwrap_or_default();
    let platform = value(&info, platform)
        .and_then(|v| v.downcast::<CFNumber>())
        .and_then(|n| n.to_i64())
        .is_some_and(|n| n != 0);
    Some((
        platform,
        string_field(&info, team),
        string_field(&info, identifier),
        cdhash,
    ))
}

/// The identity of the program at `path`: canonical walk, static code
/// check, signing fields, and the fingerprint taken before and after the
/// check. Never fails; blocking.
pub fn read_identity(path: &str) -> CodeIdentity {
    let canonical = match canonical_path(path, WalkMode::Read, None) {
        Ok(c) if c.exists => c.path,
        _ => {
            let kept = if lexically_canonical(path) {
                path.to_owned()
            } else {
                String::new()
            };
            return CodeIdentity::unknown(kept);
        }
    };
    let Some(before) = fingerprint_of(&canonical) else {
        return CodeIdentity::unknown(canonical);
    };
    let checked = check(&canonical);
    if fingerprint_of(&canonical) != Some(before) {
        return CodeIdentity::unknown(canonical);
    }
    match checked {
        Some((platform, team_id, signing_id, cdhash)) => CodeIdentity {
            path: canonical,
            identity_known: true,
            platform,
            team_id: if platform { String::new() } else { team_id },
            signing_id,
            cdhash,
        },
        None => CodeIdentity::unknown(canonical),
    }
}

/// Identities by fingerprint, at most [`IDENTITY_CACHE`] entries, least
/// recently used first out; and the last result per path, for the change
/// flag. Owned by one platform worker thread.
pub struct IdentityCache {
    entries: HashMap<Fingerprint, (CodeIdentity, u64)>,
    last: HashMap<String, (CodeIdentity, u64)>,
    tick: u64,
}
impl Default for IdentityCache {
    fn default() -> Self {
        Self::new()
    }
}
impl IdentityCache {
    pub fn new() -> Self {
        Self {
            entries: HashMap::new(),
            last: HashMap::new(),
            tick: 0,
        }
    }
    /// The identity of `path`, and whether it differs from the last result
    /// for this path (true on the first read). A changed fingerprint reads
    /// the signature again. Blocking.
    pub fn read(&mut self, path: &str) -> (CodeIdentity, bool) {
        self.tick += 1;
        let tick = self.tick;
        let identity = match fingerprint(path) {
            Some(print) => match self.entries.get_mut(&print) {
                Some((identity, used)) => {
                    *used = tick;
                    identity.clone()
                }
                _ => {
                    let identity = read_identity(path);
                    if identity.identity_known {
                        evict(&mut self.entries);
                        self.entries.insert(print, (identity.clone(), tick));
                    }
                    identity
                }
            },
            None => read_identity(path),
        };
        let changed = self
            .last
            .get(path)
            .is_none_or(|(last, _)| *last != identity);
        evict(&mut self.last);
        self.last.insert(path.to_owned(), (identity.clone(), tick));
        (identity, changed)
    }
    /// Entries held (tests and bounds).
    pub fn len(&self) -> usize {
        self.entries.len()
    }
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}
/// Removes the least recently used entry when the map is full.
fn evict<K: Clone + Eq + std::hash::Hash>(map: &mut HashMap<K, (CodeIdentity, u64)>) {
    if map.len() < IDENTITY_CACHE {
        return;
    }
    if let Some(oldest) = map
        .iter()
        .min_by_key(|(_, (_, used))| *used)
        .map(|(key, _)| key.clone())
    {
        map.remove(&oldest);
    }
}

#[cfg(test)]
mod tests;
