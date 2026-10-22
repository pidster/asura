//! macOS Keychain calls for the durable secret store
//! (`docs/designs/secret-store.md`, packets SS1-K and SS1-K2).
//!
//! Every function that takes a [`Keychain`] or a [`Target`] makes IPC calls
//! to securityd and can wait without a bound. Call them only on the secret
//! store worker thread (AE-01, AE-02); the reactor, control handlers and
//! render paths must never call them. This module adds no queue, thread or
//! time bound: the worker in `asura-service::secret_store` owns those.
//!
//! Each item call (add, get, delete, list) is preceded in the same call by
//! the target pre-check, `SecKeychainGetStatus` on the target reference. A
//! non-zero pre-check returns [`Failure::Unavailable`] and the item call is
//! not made, because an add against a missing keychain reference falls
//! through to the default keychain (probe SS-P2 (d)).
use core_foundation::base::TCFType;
use core_foundation::data::CFData;
use security_framework::item::{
    ItemAddOptions, ItemAddValue, ItemClass, ItemSearchOptions, Limit, Location, SearchResult,
};
use security_framework::os::macos::keychain::SecKeychain;
use security_framework_sys::base::SecKeychainRef;
use std::sync::{Arc, OnceLock};
use zeroize::Zeroizing;

#[cfg(feature = "test-support")]
use std::path::{Path, PathBuf};

/// Service attribute prefix; the installation ID follows in lowercase hex.
pub const SERVICE_PREFIX: &str = "asura.secret-store.";
/// Account attribute prefix; the input ID follows in lowercase hex.
pub const ACCOUNT_PREFIX: &str = "input.";
/// Label of every secret item.
pub const LABEL: &str = "Asura queued command secrets";
/// Largest item data the store writes: the largest payload of the design.
pub const MAX_ITEM_BYTES: usize = 55_413;

/// Most results one list asks for: the 256-item limit plus one. A listing of
/// this many results is truncated and means "more than 256 items".
pub const LIST_BOUND: usize = 257;

/// `SecKeychainGetStatus` flag: the keychain is unlocked.
pub const STATUS_UNLOCKED: u32 = 0x1;

const ERR_SEC_SUCCESS: i32 = 0;
const ERR_SEC_INVALID_OWNER_EDIT: i32 = -25_244;
const ERR_SEC_AUTH_FAILED: i32 = -25_293;
const ERR_SEC_DUPLICATE_ITEM: i32 = -25_299;
const ERR_SEC_ITEM_NOT_FOUND: i32 = -25_300;
const ERR_SEC_INTERACTION_NOT_ALLOWED: i32 = -25_308;
/// `errSecInternal`. Reported when a search succeeds with a result shape the
/// request cannot produce, so the reply is `unavailable` with a real code.
pub const UNEXPECTED_RESULT_STATUS: i32 = -26_276;

// Selected (engineering, 2026-10-22): security-framework 3.7.0 and
// security-framework-sys 2.17.0 do not expose these three functions. Each is
// called from exactly one wrapper below. The API is deprecated in Apple's
// headers; Rust declarations emit no deprecation warning.
unsafe extern "C" {
    fn SecKeychainGetStatus(keychain: SecKeychainRef, status: *mut u32) -> i32;
    #[cfg(feature = "test-support")]
    fn SecKeychainLock(keychain: SecKeychainRef) -> i32;
    #[cfg(feature = "test-support")]
    fn SecKeychainDelete(keychain: SecKeychainRef) -> i32;
}

/// The target pre-check: `SecKeychainGetStatus` on `keychain`.
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

/// Why a Keychain call did not give a result.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Failure {
    /// The target keychain is locked.
    Locked,
    /// The keychain is unlocked but refused this binary access to the item.
    Denied,
    /// The target or the Security framework failed. `item_call` is false when
    /// the open or the pre-check failed and no item call was made.
    Unavailable { status: i32, item_call: bool },
    /// The request broke a bound or a name rule; no Keychain call was made.
    Invalid,
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
    /// The default keychain from `SecKeychainCopyDefault`, normally the login
    /// keychain. The only production target.
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
/// process lifetime. A call that would show a dialog then fails instead of
/// waiting for a person. [`Keychain::open`] calls this first, so no item call
/// can run with interaction allowed.
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
    reference: SecKeychain,
}

impl std::fmt::Debug for Keychain {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Keychain")
    }
}

impl Keychain {
    /// Disables user interaction, then takes the target reference. Opening
    /// does not prove the keychain exists: every later call pre-checks it.
    pub fn open(target: &Target) -> Result<Self, Failure> {
        disable_user_interaction()?;
        let reference = match target {
            Target::Default => {
                // The platform's own unit tests must never reach the
                // person's default keychain.
                if cfg!(test) {
                    return Err(Failure::Invalid);
                }
                SecKeychain::default()
            }
            #[cfg(feature = "test-support")]
            Target::Scratch { path, .. } => {
                scratch::check_path(path)?;
                SecKeychain::open(path)
            }
        }
        .map_err(|error| Failure::Unavailable {
            status: error.code(),
            item_call: false,
        })?;
        Ok(Self { reference })
    }

    /// The target pre-check alone. Returns the status flags.
    pub fn status(&self) -> Result<u32, Failure> {
        keychain_status(&self.reference).map_err(|status| Failure::Unavailable {
            status,
            item_call: false,
        })
    }

    /// Pre-checks, then adds one generic-password item of 1 to
    /// [`MAX_ITEM_BYTES`] bytes.
    pub fn add(&self, key: &ItemKey, data: &[u8]) -> Result<Added, Failure> {
        add_with(self, key, data)
    }

    /// Pre-checks, then reads one item's data into a zeroizing buffer.
    pub fn get(&self, key: &ItemKey) -> Result<Found, Failure> {
        get_with(self, key)
    }

    /// Pre-checks, then deletes one item.
    pub fn delete(&self, key: &ItemKey) -> Result<Removed, Failure> {
        delete_with(self, key)
    }

    /// Pre-checks, then lists the items of one installation, at most
    /// [`LIST_BOUND`] of them. Items of other installations are never listed.
    pub fn list(&self, installation: &[u8; 16]) -> Result<Listing, Failure> {
        list_with(self, installation)
    }

    fn search(&self, service: &str) -> ItemSearchOptions {
        let mut options = ItemSearchOptions::new();
        options
            .keychains(std::slice::from_ref(&self.reference))
            .class(ItemClass::generic_password())
            .service(service);
        options
    }
}

/// The Security framework calls behind the pre-check and status mapping.
/// [`Keychain`] is the one production implementation; unit tests substitute
/// a recording fixture to check call order and mapping.
pub(crate) trait Calls {
    fn status(&self) -> Result<u32, i32>;
    fn add(&self, service: &str, account: &str, data: &[u8]) -> Result<(), i32>;
    fn copy(&self, service: &str, account: &str) -> Result<Zeroizing<Vec<u8>>, i32>;
    fn delete(&self, service: &str, account: &str) -> Result<(), i32>;
    /// The account attributes of at most `max` items under `service`.
    fn accounts(&self, service: &str, max: usize) -> Result<Vec<String>, i32>;
}

impl Calls for Keychain {
    fn status(&self) -> Result<u32, i32> {
        keychain_status(&self.reference)
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

/// Runs the target pre-check; a non-zero status prevents the item call.
fn precheck<C: Calls + ?Sized>(calls: &C) -> Result<(), Failure> {
    calls
        .status()
        .map(drop)
        .map_err(|status| Failure::Unavailable {
            status,
            item_call: false,
        })
}

/// Maps a failed item call's status under the revised table.
fn classify<C: Calls + ?Sized>(calls: &C, code: i32) -> Failure {
    match code {
        ERR_SEC_AUTH_FAILED | ERR_SEC_INTERACTION_NOT_ALLOWED => match calls.status() {
            Ok(flags) if flags & STATUS_UNLOCKED == 0 => Failure::Locked,
            Ok(_) => Failure::Denied,
            Err(status) => Failure::Unavailable {
                status,
                item_call: true,
            },
        },
        ERR_SEC_INVALID_OWNER_EDIT => Failure::Denied,
        status => Failure::Unavailable {
            status,
            item_call: true,
        },
    }
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
        Err(code) => Err(classify(calls, code)),
    }
}

pub(crate) fn get_with<C: Calls + ?Sized>(calls: &C, key: &ItemKey) -> Result<Found, Failure> {
    precheck(calls)?;
    match calls.copy(&key.service(), &key.account()) {
        Ok(data) => Ok(Found::Data(data)),
        // Missing only because the pre-check returned 0 in this call.
        Err(ERR_SEC_ITEM_NOT_FOUND) => Ok(Found::Missing),
        Err(code) => Err(classify(calls, code)),
    }
}

pub(crate) fn delete_with<C: Calls + ?Sized>(calls: &C, key: &ItemKey) -> Result<Removed, Failure> {
    precheck(calls)?;
    match calls.delete(&key.service(), &key.account()) {
        Ok(()) => Ok(Removed::Deleted),
        Err(ERR_SEC_ITEM_NOT_FOUND) => Ok(Removed::Absent),
        Err(code) => Err(classify(calls, code)),
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
        Err(code) => Err(classify(calls, code)),
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

    fn open_checked(target: &Target) -> Result<(Keychain, &Zeroizing<String>), Failure> {
        let Target::Scratch { password, .. } = target else {
            return Err(Failure::Invalid);
        };
        let keychain = Keychain::open(target)?;
        keychain.status()?;
        Ok((keychain, password))
    }

    /// Locks a scratch keychain (`SecKeychainLock`) after a pre-check.
    pub fn lock(target: &Target) -> Result<(), Failure> {
        let (keychain, _) = open_checked(target)?;
        match keychain_lock(&keychain.reference) {
            ERR_SEC_SUCCESS => Ok(()),
            status => Err(Failure::Unavailable {
                status,
                item_call: false,
            }),
        }
    }

    /// Unlocks a scratch keychain with its password after a pre-check.
    pub fn unlock(target: &Target) -> Result<(), Failure> {
        let (keychain, password) = open_checked(target)?;
        let mut reference = keychain.reference.clone();
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
        match keychain_delete(&keychain.reference) {
            ERR_SEC_SUCCESS => Ok(()),
            status => Err(Failure::Unavailable {
                status,
                item_call: false,
            }),
        }
    }

    /// Adds one item under `service_name(installation)` with any non-empty
    /// `account`, so tests can write accounts outside the item contract
    /// (SS-W9). Opens only a scratch target and uses the same pre-check, add
    /// and status mapping as [`Keychain::add`].
    pub fn add_account(
        target: &Target,
        installation: &[u8; 16],
        account: &str,
        data: &[u8],
    ) -> Result<Added, Failure> {
        let Target::Scratch { .. } = target else {
            return Err(Failure::Invalid);
        };
        if account.is_empty() || account.contains('\0') {
            return Err(Failure::Invalid);
        }
        let keychain = Keychain::open(target)?;
        add_raw_with(&keychain, &service_name(installation), account, data)
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
