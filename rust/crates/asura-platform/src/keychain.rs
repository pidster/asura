//! macOS Keychain calls for the durable secret store
//! (`docs/designs/secret-store.md`, packets SS1-K and SS1-K2;
//! `docs/designs/release-signing.md`, team Keychain store, packet SG3).
//!
//! Every function that takes a [`Keychain`] or a [`Target`] makes IPC calls
//! to securityd and can wait without a bound. Call them only on the secret
//! store worker thread (AE-01, AE-02); the reactor, control handlers and
//! render paths must never call them. This module adds no queue, thread or
//! time bound: the worker in `asura-service::secret_store` owns those.
//!
//! Production ([`Target::Default`]) uses the data-protection keychain with
//! the access group read from the process's own entitlements; every call
//! names it ([`data_protection`]). A process without a valid group gets
//! [`Failure::NotSigned`] and makes no Keychain item call.
//!
//! The test-only scratch backend (`Target::Scratch`, `test-support`) uses a
//! scratch keychain file. Each of its item calls is preceded in the same
//! call by the target pre-check, `SecKeychainGetStatus` on the target
//! reference. A non-zero pre-check returns [`Failure::Unavailable`] and the
//! item call is not made, because an add against a missing keychain
//! reference falls through to the default keychain (probe SS-P2 (d)).
pub mod data_protection;
pub mod session;

use security_framework::os::macos::keychain::SecKeychain;
use std::sync::OnceLock;
use zeroize::Zeroizing;

#[cfg(feature = "test-support")]
use core_foundation::base::TCFType;
#[cfg(feature = "test-support")]
use core_foundation::data::CFData;
#[cfg(feature = "test-support")]
use security_framework::item::{
    ItemAddOptions, ItemAddValue, ItemClass, ItemSearchOptions, Limit, Location, SearchResult,
};
#[cfg(feature = "test-support")]
use security_framework_sys::base::SecKeychainRef;
#[cfg(feature = "test-support")]
use std::path::{Path, PathBuf};
#[cfg(feature = "test-support")]
use std::sync::Arc;

/// Service attribute prefix; the installation ID follows in lowercase hex.
pub const SERVICE_PREFIX: &str = "asura.secret-store.";
/// Account attribute prefix; the input ID follows in lowercase hex.
pub const ACCOUNT_PREFIX: &str = "input.";
/// Label of every secret item.
pub const LABEL: &str = "Asura queued command secrets";
/// Largest item data the store writes: the largest payload of the design.
/// 1 + 32 + 1 + 8,241 + 32 = 8,307 (secret-store.md, payload; ENV1 removed
/// the launch environment).
pub const MAX_ITEM_BYTES: usize = 8_307;

/// Most results one list asks for: the 256-item limit plus one. A listing of
/// this many results is truncated and means "more than 256 items".
pub const LIST_BOUND: usize = 257;

/// `SecKeychainGetStatus` flag: the keychain is unlocked.
pub const STATUS_UNLOCKED: u32 = 0x1;

const ERR_SEC_SUCCESS: i32 = 0;
#[cfg(any(test, feature = "test-support"))]
const ERR_SEC_INVALID_OWNER_EDIT: i32 = -25_244;
#[cfg(any(test, feature = "test-support"))]
const ERR_SEC_AUTH_FAILED: i32 = -25_293;
const ERR_SEC_DUPLICATE_ITEM: i32 = -25_299;
const ERR_SEC_ITEM_NOT_FOUND: i32 = -25_300;
#[cfg(any(test, feature = "test-support"))]
const ERR_SEC_INTERACTION_NOT_ALLOWED: i32 = -25_308;
/// `errSecMissingEntitlement`: the process is not entitled to the group it
/// named (KC-P1, KC-P4). Also the status a failed access group check
/// reports, although that check made no Keychain call.
pub const MISSING_ENTITLEMENT: i32 = -34_018;
/// `errSecInternal`. Reported when a search succeeds with a result shape the
/// request cannot produce, so the reply is `unavailable` with a real code.
pub const UNEXPECTED_RESULT_STATUS: i32 = -26_276;

// Selected (engineering, 2026-10-22): security-framework 3.7.0 and
// security-framework-sys 2.17.0 do not expose these three functions. Each is
// called from exactly one wrapper below. The API is deprecated in Apple's
// headers; Rust declarations emit no deprecation warning. Since SG3 only the
// test-only scratch backend uses them.
#[cfg(feature = "test-support")]
unsafe extern "C" {
    fn SecKeychainGetStatus(keychain: SecKeychainRef, status: *mut u32) -> i32;
    fn SecKeychainLock(keychain: SecKeychainRef) -> i32;
    fn SecKeychainDelete(keychain: SecKeychainRef) -> i32;
}

/// The target pre-check: `SecKeychainGetStatus` on `keychain`.
#[cfg(feature = "test-support")]
fn keychain_status(keychain: &SecKeychain) -> Result<u32, i32> {
    let mut flags = 0u32;
    // SAFETY: the reference comes from a live `SecKeychain` borrowed for the
    // whole call, so it is a valid `SecKeychainRef`. `flags` is a local `u32`
    // the call writes once; no pointer outlives the call.
    let code = unsafe { SecKeychainGetStatus(keychain.as_concrete_TypeRef(), &mut flags) };
    if code == ERR_SEC_SUCCESS {
        Ok(flags)
    } else {
        Err(code)
    }
}

#[cfg(feature = "test-support")]
fn keychain_lock(keychain: &SecKeychain) -> i32 {
    // SAFETY: the reference comes from a live `SecKeychain` borrowed for the
    // whole call. The call takes no other pointer.
    unsafe { SecKeychainLock(keychain.as_concrete_TypeRef()) }
}

#[cfg(feature = "test-support")]
fn keychain_delete(keychain: &SecKeychain) -> i32 {
    // SAFETY: the reference comes from a live `SecKeychain` borrowed for the
    // whole call. Deleting the file does not free the reference; the caller's
    // `SecKeychain` still releases it on drop.
    unsafe { SecKeychainDelete(keychain.as_concrete_TypeRef()) }
}

/// Why a Keychain call did not give a result. Every variant but
/// [`Failure::Invalid`] has a raw `OSStatus` ([`Failure::status`]) that the
/// worker records as field evidence.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Failure {
    /// The keychain is locked: -25308 on the data-protection keychain
    /// (before first unlock; unverified), or the scratch unlock-bit rule.
    Locked { status: i32 },
    /// The scratch keychain is unlocked but refused this binary access to
    /// the item. The data-protection backend never returns it.
    Denied { status: i32 },
    /// The process is not entitled to the access group: a call returned
    /// -34018, or the access group check failed and no call was made
    /// (`item_call` false). Terminal for the process.
    NotSigned { item_call: bool },
    /// The target or the Security framework failed. `item_call` is false when
    /// the open or the pre-check failed and no item call was made.
    Unavailable { status: i32, item_call: bool },
    /// The request broke a bound or a name rule; no Keychain call was made.
    Invalid,
}

impl Failure {
    /// The raw `OSStatus` of the failure; `None` for [`Failure::Invalid`].
    pub fn status(&self) -> Option<i32> {
        match *self {
            Self::Locked { status } | Self::Denied { status } => Some(status),
            Self::NotSigned { .. } => Some(MISSING_ENTITLEMENT),
            Self::Unavailable { status, .. } => Some(status),
            Self::Invalid => None,
        }
    }
}

/// Reply to [`Keychain::add`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Added {
    Stored,
    /// An item with this service and account exists (`errSecDuplicateItem`).
    Duplicate,
}

/// Reply to [`Keychain::get`].
#[derive(Debug, PartialEq, Eq)]
pub enum Found {
    Data(Zeroizing<Vec<u8>>),
    Missing,
}

/// Reply to [`Keychain::delete`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Removed {
    Deleted,
    Absent,
}

/// Reply to [`Keychain::list`]: the input IDs of this installation's items,
/// from at most [`LIST_BOUND`] results.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Listing {
    pub inputs: Vec<[u8; 16]>,
    /// Items under this service whose account is not `input.<32 hex>`.
    pub unrecognized: usize,
    /// [`LIST_BOUND`] results returned, so more items may exist than listed.
    pub truncated: bool,
}

/// One secret item's service and account attributes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ItemKey {
    pub installation: [u8; 16],
    pub input: [u8; 16],
}

impl ItemKey {
    pub fn service(&self) -> String {
        service_name(&self.installation)
    }
    pub fn account(&self) -> String {
        format!("{ACCOUNT_PREFIX}{}", hex(&self.input))
    }
    /// Parses attributes written under the item contract.
    pub fn parse(service: &str, account: &str) -> Option<Self> {
        Some(Self {
            installation: parse_hex_id(service.strip_prefix(SERVICE_PREFIX)?)?,
            input: parse_account(account)?,
        })
    }
}

pub fn service_name(installation: &[u8; 16]) -> String {
    format!("{SERVICE_PREFIX}{}", hex(installation))
}

fn hex(id: &[u8; 16]) -> String {
    id.iter().map(|b| format!("{b:02x}")).collect()
}

fn parse_account(account: &str) -> Option<[u8; 16]> {
    parse_hex_id(account.strip_prefix(ACCOUNT_PREFIX)?)
}

fn parse_hex_id(text: &str) -> Option<[u8; 16]> {
    let bytes = text.as_bytes();
    if bytes.len() != 32 {
        return None;
    }
    let digit = |c: u8| match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        _ => None,
    };
    let mut id = [0u8; 16];
    let (pairs, _) = bytes.as_chunks::<2>();
    for (byte, [high, low]) in id.iter_mut().zip(pairs) {
        *byte = (digit(*high)? << 4) | digit(*low)?;
    }
    Some(id)
}

/// The keychain the secret store uses.
#[derive(Clone)]
pub enum Target {
    /// The data-protection keychain with the access group from this
    /// process's entitlements. The only production target.
    Default,
    /// A scratch keychain file made by [`scratch::create`]. Test builds only.
    #[cfg(feature = "test-support")]
    Scratch {
        path: PathBuf,
        password: Zeroizing<String>,
    },
}

impl std::fmt::Debug for Target {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Default => f.write_str("Target::Default"),
            #[cfg(feature = "test-support")]
            Self::Scratch { path, .. } => f
                .debug_struct("Target::Scratch")
                .field("path", path)
                .finish_non_exhaustive(),
        }
    }
}

static INTERACTION: OnceLock<i32> = OnceLock::new();

/// Disables Keychain user interaction for the whole process, once, for the
/// process lifetime. A file-keychain call that would show a dialog then
/// fails instead of waiting for a person; every data-protection dictionary
/// also sets `kSecUseAuthenticationUIFail`. [`Keychain::open`] calls this
/// first, so no item call can run with interaction allowed.
pub fn disable_user_interaction() -> Result<(), Failure> {
    let code = *INTERACTION.get_or_init(|| match SecKeychain::disable_user_interaction() {
        Ok(lock) => {
            // The lock re-enables interaction on drop; the design holds it
            // for the process lifetime, so it is never dropped.
            std::mem::forget(lock);
            ERR_SEC_SUCCESS
        }
        Err(error) => error.code(),
    });
    if code == ERR_SEC_SUCCESS {
        Ok(())
    } else {
        Err(Failure::Unavailable {
            status: code,
            item_call: false,
        })
    }
}

/// An open target keychain. Owned by the secret store worker.
pub struct Keychain {
    backend: Backend,
}

enum Backend {
    DataProtection(data_protection::DataProtection),
    #[cfg(feature = "test-support")]
    File(FileKeychain),
}

impl std::fmt::Debug for Keychain {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Keychain")
    }
}

impl Keychain {
    /// Disables user interaction, then opens the target. For
    /// [`Target::Default`] it reads the access group from the entitlements
    /// (SG-F7): without a valid group it returns [`Failure::NotSigned`] and
    /// no Keychain item call can follow. A scratch open does not prove the
    /// keychain exists: every later call pre-checks it.
    pub fn open(target: &Target) -> Result<Self, Failure> {
        disable_user_interaction()?;
        let backend = match target {
            Target::Default => {
                let group = data_protection::entitled_group()
                    .ok_or(Failure::NotSigned { item_call: false })?;
                // The platform's own unit tests must never reach the
                // data-protection keychain, even from a signed binary.
                if cfg!(test) {
                    return Err(Failure::Invalid);
                }
                Backend::DataProtection(data_protection::DataProtection::new(group))
            }
            #[cfg(feature = "test-support")]
            Target::Scratch { path, .. } => {
                scratch::check_path(path)?;
                let reference = SecKeychain::open(path).map_err(|error| Failure::Unavailable {
                    status: error.code(),
                    item_call: false,
                })?;
                Backend::File(FileKeychain { reference })
            }
        };
        Ok(Self { backend })
    }

    /// The access group every call names; `None` for a scratch keychain.
    pub fn group(&self) -> Option<&str> {
        match &self.backend {
            Backend::DataProtection(backend) => Some(backend.group()),
            #[cfg(feature = "test-support")]
            Backend::File(_) => None,
        }
    }

    fn calls(&self) -> &dyn Calls {
        match &self.backend {
            Backend::DataProtection(backend) => backend,
            #[cfg(feature = "test-support")]
            Backend::File(backend) => backend,
        }
    }

    /// The scratch target pre-check alone. Returns the status flags.
    #[cfg(feature = "test-support")]
    pub fn status(&self) -> Result<u32, Failure> {
        let Backend::File(file) = &self.backend else {
            return Err(Failure::Invalid);
        };
        keychain_status(&file.reference).map_err(|status| Failure::Unavailable {
            status,
            item_call: false,
        })
    }

    /// Adds one generic-password item of 1 to [`MAX_ITEM_BYTES`] bytes.
    pub fn add(&self, key: &ItemKey, data: &[u8]) -> Result<Added, Failure> {
        add_with(self.calls(), key, data)
    }

    /// Reads one item's data into a zeroizing buffer.
    pub fn get(&self, key: &ItemKey) -> Result<Found, Failure> {
        get_with(self.calls(), key)
    }

    /// Deletes one item.
    pub fn delete(&self, key: &ItemKey) -> Result<Removed, Failure> {
        delete_with(self.calls(), key)
    }

    /// Lists the items of one installation, at most [`LIST_BOUND`] of them.
    /// Items of other installations are never listed.
    pub fn list(&self, installation: &[u8; 16]) -> Result<Listing, Failure> {
        list_with(self.calls(), installation)
    }
}

/// The Security framework calls behind the status mapping, and for the
/// scratch backend the pre-check. `DataProtection` and the scratch
/// `FileKeychain` implement it; unit tests substitute a recording fixture
/// to check call order and mapping.
pub(crate) trait Calls {
    /// The scratch backend's target pre-check. The data-protection backend
    /// has none.
    fn precheck(&self) -> Result<(), i32> {
        Ok(())
    }
    /// This backend's status mapping for a failed item call.
    fn classify(&self, code: i32) -> Failure;
    fn add(&self, service: &str, account: &str, data: &[u8]) -> Result<(), i32>;
    fn copy(&self, service: &str, account: &str) -> Result<Zeroizing<Vec<u8>>, i32>;
    fn delete(&self, service: &str, account: &str) -> Result<(), i32>;
    /// The account attributes of at most `max` items under `service`.
    fn accounts(&self, service: &str, max: usize) -> Result<Vec<String>, i32>;
}

/// The scratch backend's status table: -25293 and -25308 map by the unlock
/// bit of a second status call (`status`); -25244 is denied.
#[cfg(any(test, feature = "test-support"))]
pub(crate) fn classify_file(code: i32, status: impl FnOnce() -> Result<u32, i32>) -> Failure {
    match code {
        ERR_SEC_AUTH_FAILED | ERR_SEC_INTERACTION_NOT_ALLOWED => match status() {
            Ok(flags) if flags & STATUS_UNLOCKED == 0 => Failure::Locked { status: code },
            Ok(_) => Failure::Denied { status: code },
            Err(status) => Failure::Unavailable {
                status,
                item_call: true,
            },
        },
        ERR_SEC_INVALID_OWNER_EDIT => Failure::Denied { status: code },
        status => Failure::Unavailable {
            status,
            item_call: true,
        },
    }
}

/// A scratch keychain file (`test-support`).
#[cfg(feature = "test-support")]
pub(crate) struct FileKeychain {
    reference: SecKeychain,
}

#[cfg(feature = "test-support")]
impl FileKeychain {
    fn search(&self, service: &str) -> ItemSearchOptions {
        let mut options = ItemSearchOptions::new();
        options
            .keychains(std::slice::from_ref(&self.reference))
            .class(ItemClass::generic_password())
            .service(service);
        options
    }
}

#[cfg(feature = "test-support")]
impl Calls for FileKeychain {
    fn precheck(&self) -> Result<(), i32> {
        keychain_status(&self.reference).map(drop)
    }
    fn classify(&self, code: i32) -> Failure {
        classify_file(code, || keychain_status(&self.reference))
    }
    fn add(&self, service: &str, account: &str, data: &[u8]) -> Result<(), i32> {
        // CF uses this zeroizing buffer without copying and drops it when the
        // request dictionary is released.
        let buffer = Arc::new(Zeroizing::new(data.to_vec()));
        let mut options = ItemAddOptions::new(ItemAddValue::Data {
            class: ItemClass::generic_password(),
            data: CFData::from_arc(buffer),
        });
        options
            .set_service(service)
            .set_account_name(account)
            .set_label(LABEL)
            .set_location(Location::FileKeychain(self.reference.clone()));
        options.add().map_err(|error| error.code())
    }
    fn copy(&self, service: &str, account: &str) -> Result<Zeroizing<Vec<u8>>, i32> {
        let results = self
            .search(service)
            .account(account)
            .load_data(true)
            .limit(1i64)
            .search()
            .map_err(|error| error.code())?;
        // Every returned buffer is zeroized on drop, whatever its shape.
        let count = results.len();
        let mut found = None;
        for result in results {
            if let SearchResult::Data(data) = result {
                found = Some(Zeroizing::new(data));
            }
        }
        match found {
            Some(data) if count == 1 => Ok(data),
            _ => Err(UNEXPECTED_RESULT_STATUS),
        }
    }
    fn delete(&self, service: &str, account: &str) -> Result<(), i32> {
        self.search(service)
            .account(account)
            .delete()
            .map_err(|error| error.code())
    }
    fn accounts(&self, service: &str, max: usize) -> Result<Vec<String>, i32> {
        let limit = i64::try_from(max).map_err(|_| UNEXPECTED_RESULT_STATUS)?;
        let results = self
            .search(service)
            .load_attributes(true)
            .limit(Limit::Max(limit))
            .search()
            .map_err(|error| error.code())?;
        results
            .iter()
            .map(|result| {
                result
                    .simplify_dict()
                    .and_then(|mut attributes| attributes.remove("acct"))
                    .ok_or(UNEXPECTED_RESULT_STATUS)
            })
            .collect()
    }
}

/// Runs the backend's pre-check; a non-zero status prevents the item call.
fn precheck<C: Calls + ?Sized>(calls: &C) -> Result<(), Failure> {
    calls.precheck().map_err(|status| Failure::Unavailable {
        status,
        item_call: false,
    })
}

pub(crate) fn add_with<C: Calls + ?Sized>(
    calls: &C,
    key: &ItemKey,
    data: &[u8],
) -> Result<Added, Failure> {
    add_raw_with(calls, &key.service(), &key.account(), data)
}

/// The one add path: bounds, pre-check, add and status mapping. Production
/// reaches it only through [`add_with`] with an [`ItemKey`]'s attributes.
fn add_raw_with<C: Calls + ?Sized>(
    calls: &C,
    service: &str,
    account: &str,
    data: &[u8],
) -> Result<Added, Failure> {
    if data.is_empty() || data.len() > MAX_ITEM_BYTES {
        return Err(Failure::Invalid);
    }
    precheck(calls)?;
    match calls.add(service, account, data) {
        Ok(()) => Ok(Added::Stored),
        Err(ERR_SEC_DUPLICATE_ITEM) => Ok(Added::Duplicate),
        Err(code) => Err(calls.classify(code)),
    }
}

pub(crate) fn get_with<C: Calls + ?Sized>(calls: &C, key: &ItemKey) -> Result<Found, Failure> {
    precheck(calls)?;
    match calls.copy(&key.service(), &key.account()) {
        Ok(data) => Ok(Found::Data(data)),
        // Missing only because the call named an entitled group (KC-P4), or
        // the scratch pre-check returned 0 in this call.
        Err(ERR_SEC_ITEM_NOT_FOUND) => Ok(Found::Missing),
        Err(code) => Err(calls.classify(code)),
    }
}

pub(crate) fn delete_with<C: Calls + ?Sized>(calls: &C, key: &ItemKey) -> Result<Removed, Failure> {
    precheck(calls)?;
    match calls.delete(&key.service(), &key.account()) {
        Ok(()) => Ok(Removed::Deleted),
        Err(ERR_SEC_ITEM_NOT_FOUND) => Ok(Removed::Absent),
        Err(code) => Err(calls.classify(code)),
    }
}

pub(crate) fn list_with<C: Calls + ?Sized>(
    calls: &C,
    installation: &[u8; 16],
) -> Result<Listing, Failure> {
    precheck(calls)?;
    match calls.accounts(&service_name(installation), LIST_BOUND) {
        // More results than the match limit is a shape the request cannot
        // produce.
        Ok(accounts) if accounts.len() > LIST_BOUND => Err(Failure::Unavailable {
            status: UNEXPECTED_RESULT_STATUS,
            item_call: true,
        }),
        Ok(accounts) => {
            let mut listing = Listing {
                truncated: accounts.len() == LIST_BOUND,
                ..Listing::default()
            };
            for account in accounts {
                match parse_account(&account) {
                    Some(input) => listing.inputs.push(input),
                    None => listing.unrecognized += 1,
                }
            }
            Ok(listing)
        }
        // A search with no match returns -25300, not an empty list.
        Err(ERR_SEC_ITEM_NOT_FOUND) => Ok(Listing::default()),
        Err(code) => Err(calls.classify(code)),
    }
}

/// Scratch keychain files for tests (SSR6). Never the login keychain.
#[cfg(feature = "test-support")]
pub mod scratch {
    use super::*;
    use security_framework::os::macos::keychain::CreateOptions;

    /// Rejects anything but an absolute `.keychain` path outside the
    /// person's `~/Library/Keychains` directory.
    pub(super) fn check_path(path: &Path) -> Result<(), Failure> {
        let text = path.to_str().ok_or(Failure::Invalid)?;
        let in_library = std::env::var_os("HOME")
            .map(|home| path.starts_with(Path::new(&home).join("Library/Keychains")))
            .unwrap_or(false);
        if !path.is_absolute()
            || !text.ends_with(".keychain")
            || text.contains('\0')
            || in_library
            || path.starts_with("/Library")
        {
            return Err(Failure::Invalid);
        }
        Ok(())
    }

    /// Creates a new scratch keychain file with `password`. The file must not
    /// exist. `SecKeychainCreate` does not add it to the search list (SS-P1).
    pub fn create(path: &Path, password: &str) -> Result<Target, Failure> {
        check_path(path)?;
        if password.is_empty() || std::fs::symlink_metadata(path).is_ok() {
            return Err(Failure::Invalid);
        }
        disable_user_interaction()?;
        CreateOptions::new()
            .password(password)
            .create(path)
            .map_err(|error| Failure::Unavailable {
                status: error.code(),
                item_call: false,
            })?;
        Ok(Target::Scratch {
            path: path.to_path_buf(),
            password: Zeroizing::new(password.to_owned()),
        })
    }

    fn open_checked(target: &Target) -> Result<(SecKeychain, &Zeroizing<String>), Failure> {
        let Target::Scratch { password, .. } = target else {
            return Err(Failure::Invalid);
        };
        let keychain = Keychain::open(target)?;
        keychain.status()?;
        let Backend::File(file) = keychain.backend else {
            return Err(Failure::Invalid);
        };
        Ok((file.reference, password))
    }

    /// Locks a scratch keychain (`SecKeychainLock`) after a pre-check.
    pub fn lock(target: &Target) -> Result<(), Failure> {
        let (keychain, _) = open_checked(target)?;
        match keychain_lock(&keychain) {
            ERR_SEC_SUCCESS => Ok(()),
            status => Err(Failure::Unavailable {
                status,
                item_call: false,
            }),
        }
    }

    /// Unlocks a scratch keychain with its password after a pre-check.
    pub fn unlock(target: &Target) -> Result<(), Failure> {
        let (mut reference, password) = open_checked(target)?;
        reference
            .unlock(Some(password.as_str()))
            .map_err(|error| Failure::Unavailable {
                status: error.code(),
                item_call: false,
            })
    }

    /// Deletes a scratch keychain file (`SecKeychainDelete`) after a
    /// pre-check. Stop every worker on the target first (testing rule 2).
    pub fn delete(target: &Target) -> Result<(), Failure> {
        let (keychain, _) = open_checked(target)?;
        match keychain_delete(&keychain) {
            ERR_SEC_SUCCESS => Ok(()),
            status => Err(Failure::Unavailable {
                status,
                item_call: false,
            }),
        }
    }

    /// Adds one item under `service_name(installation)` with any non-empty
    /// `account`, so tests can write accounts outside the item contract
    /// (SS-W9). Opens a scratch target, or `Target::Default` only when this
    /// process's selected access group is the test group (SG5, SS-I10 on
    /// the data-protection keychain), and uses the same pre-check, add and
    /// status mapping as [`Keychain::add`].
    pub fn add_account(
        target: &Target,
        installation: &[u8; 16],
        account: &str,
        data: &[u8],
    ) -> Result<Added, Failure> {
        match target {
            Target::Scratch { .. } => {}
            Target::Default if super::data_protection::qualification::test_group().is_some() => {}
            Target::Default => return Err(Failure::Invalid),
        }
        if account.is_empty() || account.contains('\0') {
            return Err(Failure::Invalid);
        }
        let keychain = Keychain::open(target)?;
        add_raw_with(keychain.calls(), &service_name(installation), account, data)
    }

    /// The target of an existing scratch keychain file (no call is made).
    pub fn target(path: &Path, password: &str) -> Target {
        Target::Scratch {
            path: path.to_path_buf(),
            password: Zeroizing::new(password.to_owned()),
        }
    }

    /// The service attributes of the generic items in a scratch keychain,
    /// at most [`LIST_BOUND`], attributes only, never item data. A PTY
    /// journey counts the Asura items with it, so no `security` command
    /// runs. An empty keychain is an empty list.
    pub fn services(target: &Target) -> Result<Vec<String>, Failure> {
        let (keychain, _) = open_checked(target)?;
        let limit = i64::try_from(LIST_BOUND).map_err(|_| Failure::Invalid)?;
        let results = ItemSearchOptions::new()
            .keychains(std::slice::from_ref(&keychain))
            .class(ItemClass::generic_password())
            .load_attributes(true)
            .limit(Limit::Max(limit))
            .search();
        let results = match results {
            Ok(results) => results,
            Err(error) if error.code() == ERR_SEC_ITEM_NOT_FOUND => return Ok(Vec::new()),
            Err(error) => {
                return Err(Failure::Unavailable {
                    status: error.code(),
                    item_call: true,
                });
            }
        };
        Ok(results
            .iter()
            .filter_map(|result| result.simplify_dict()?.remove("svce"))
            .collect())
    }

    /// The data every fixture item holds.
    pub const FIXTURE_DATA: &[u8] = b"asura keychain fixture item";

    /// The body of every keychain fixture binary (SS-I7, SS-W3, SS-W9).
    ///
    /// `<fixture> <scratch keychain path> <password> <service> <account>`
    /// writes one generic-password item into an existing scratch keychain.
    /// The binary is a different executable from the test that runs it, so
    /// the item's default ACL does not trust the test binary. An
    /// `input.<32 hex>` account is written by [`Keychain::add`], any other
    /// account by [`add_account`]; both pre-check the target first and never
    /// write to a missing keychain. Prints one reply word: `stored` exits 0,
    /// `duplicate` or a failure exits 1, a usage error exits 2.
    pub fn fixture_main() -> std::process::ExitCode {
        use std::process::ExitCode;
        let args: Vec<String> = std::env::args().skip(1).collect();
        let [path, password, service, account] = args.as_slice() else {
            eprintln!("usage: <fixture> <keychain path> <password> <service> <account>");
            return ExitCode::from(2);
        };
        let Some(installation) = service.strip_prefix(SERVICE_PREFIX).and_then(parse_hex_id) else {
            eprintln!("service does not follow the item contract");
            return ExitCode::from(2);
        };
        let target = Target::Scratch {
            path: PathBuf::from(path),
            password: Zeroizing::new(password.clone()),
        };
        let reply = match ItemKey::parse(service, account) {
            Some(key) => {
                Keychain::open(&target).and_then(|keychain| keychain.add(&key, FIXTURE_DATA))
            }
            None => add_account(&target, &installation, account, FIXTURE_DATA),
        };
        match reply {
            Ok(Added::Stored) => {
                println!("stored");
                ExitCode::SUCCESS
            }
            Ok(Added::Duplicate) => {
                println!("duplicate");
                ExitCode::from(1)
            }
            Err(failure) => {
                println!("{failure:?}");
                ExitCode::from(1)
            }
        }
    }
}

#[cfg(test)]
mod tests;
