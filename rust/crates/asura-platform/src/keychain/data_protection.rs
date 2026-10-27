//! The production backend: the data-protection keychain with the Asura
//! access group (`docs/designs/release-signing.md`, team Keychain store,
//! packet SG3).
//!
//! Every dictionary is built by [`pairs`], which always sets the
//! data-protection flag, the access group, `kSecAttrSynchronizable` false
//! and `kSecUseAuthenticationUI` fail (KC-U1). The access group comes from
//! the process's own signed entitlements ([`entitled_group`]); a process
//! without a valid group never builds a dictionary.
use super::{Calls, Failure, LABEL, MISSING_ENTITLEMENT};
use core_foundation::array::CFArray;
use core_foundation::base::{CFAllocatorRef, CFTypeRef, kCFAllocatorDefault};
use core_foundation::base::{CFType, TCFType};
use core_foundation::boolean::CFBoolean;
use core_foundation::data::CFData;
use core_foundation::dictionary::CFDictionary;
use core_foundation::error::CFErrorRef;
use core_foundation::number::CFNumber;
use core_foundation::string::CFString;
use core_foundation::string::CFStringRef;
use security_framework_sys::access_control::kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly;
use security_framework_sys::item::{
    kSecAttrAccessGroup, kSecAttrAccount, kSecAttrLabel, kSecAttrService, kSecAttrSynchronizable,
    kSecClass, kSecClassGenericPassword, kSecMatchLimit, kSecReturnAttributes, kSecReturnData,
    kSecUseAuthenticationUI, kSecValueData,
};
use security_framework_sys::keychain_item::{SecItemAdd, SecItemCopyMatching, SecItemDelete};
use std::ffi::c_void;
use zeroize::Zeroizing;

const ERR_SEC_INTERACTION_NOT_ALLOWED: i32 = -25_308;

/// The application identifier suffixes the access group check accepts:
/// the product and the qualification bundle's test App ID.
pub const PRODUCT_SUFFIX: &str = "org.pidster.asura";
pub const TEST_SUFFIX: &str = "org.pidster.asura.test";

// Selected (engineering, SG3): `security-framework-sys` 2.17.0 does not
// declare these (it declares `kSecUseDataProtectionKeychain` only behind a
// feature that KQ3's minimal feature set leaves off). Each is read or called
// only in this module.
#[link(name = "Security", kind = "framework")]
unsafe extern "C" {
    pub(crate) static kSecUseDataProtectionKeychain: CFStringRef;
    pub(crate) static kSecUseAuthenticationUIFail: CFStringRef;
    pub(crate) static kSecAttrAccessible: CFStringRef;
    static kSecMatchLimitOne: CFStringRef;
    fn SecTaskCreateFromSelf(allocator: CFAllocatorRef) -> CFTypeRef;
    fn SecTaskCopyValueForEntitlement(
        task: CFTypeRef,
        entitlement: CFStringRef,
        error: *mut CFErrorRef,
    ) -> CFTypeRef;
}

/// A Security framework string constant as a `CFString`.
///
/// # Safety
///
/// `constant` must be a valid `CFStringRef` for the process lifetime, such
/// as a Security framework constant.
pub(crate) unsafe fn key(constant: CFStringRef) -> CFString {
    // SAFETY: the caller passes a valid constant; the get rule retains it.
    unsafe { CFString::wrap_under_get_rule(constant) }
}

/// One entitlement value, as the access group check sees it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Value {
    Absent,
    String(String),
    Strings(Vec<String>),
    /// Any other type, or an array holding a non-string.
    Other,
}

/// The three entitlements the check reads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Entitlements {
    pub application_identifier: Value,
    pub access_groups: Value,
    pub team_identifier: Value,
}

impl Entitlements {
    /// True when the process claims any of the three entitlements.
    pub fn any(&self) -> bool {
        [
            &self.application_identifier,
            &self.access_groups,
            &self.team_identifier,
        ]
        .iter()
        .any(|v| **v != Value::Absent)
    }
}

/// SG-F7: the one access group the store may name, or `None`
/// (`not_signed`).
pub(crate) fn access_group(entitlements: &Entitlements) -> Option<String> {
    let Value::String(application) = &entitlements.application_identifier else {
        return None;
    };
    let (team, bundle) = application.split_once('.')?;
    let team_valid = team.len() == 10
        && team
            .bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit());
    if !team_valid || (bundle != PRODUCT_SUFFIX && bundle != TEST_SUFFIX) {
        return None;
    }
    let Value::Strings(groups) = &entitlements.access_groups else {
        return None;
    };
    if groups.as_slice() != std::slice::from_ref(application) {
        return None;
    }
    match &entitlements.team_identifier {
        Value::Absent => {}
        Value::String(t) if t == team => {}
        _ => return None,
    }
    Some(application.clone())
}

fn value(raw: CFTypeRef) -> Value {
    if raw.is_null() {
        return Value::Absent;
    }
    // SAFETY: a non-null return of `SecTaskCopyValueForEntitlement` follows
    // the create rule; wrapping it releases it once on drop.
    let value = unsafe { CFType::wrap_under_create_rule(raw) };
    if let Some(text) = value.downcast::<CFString>() {
        return Value::String(text.to_string());
    }
    let Some(array) = value.downcast::<CFArray>() else {
        return Value::Other;
    };
    let mut strings = Vec::new();
    for item in array.iter() {
        // SAFETY: the array owns its elements for this borrow; the get rule
        // retains the element for the wrapper's lifetime.
        let item = unsafe { CFType::wrap_under_get_rule(*item) };
        match item.downcast::<CFString>() {
            Some(text) => strings.push(text.to_string()),
            None => return Value::Other,
        }
    }
    Value::Strings(strings)
}

/// Reads the process's own entitlements. Blocking boundary: the secret
/// store worker thread only.
pub(crate) fn read_entitlements() -> Entitlements {
    // SAFETY: `kCFAllocatorDefault` is the default allocator; the call
    // returns a new task object (create rule) or null.
    let task = unsafe { SecTaskCreateFromSelf(kCFAllocatorDefault) };
    if task.is_null() {
        return Entitlements {
            application_identifier: Value::Other,
            access_groups: Value::Other,
            team_identifier: Value::Other,
        };
    }
    // SAFETY: `task` is the non-null object created above; wrapping it under
    // the create rule releases it once, after the three reads below.
    let task = unsafe { CFType::wrap_under_create_rule(task) };
    let read = |name: &str| {
        let name = CFString::new(name);
        // SAFETY: `task` and `name` are live for the call; a null error
        // pointer is allowed. The result follows the create rule and `value`
        // takes ownership of it.
        value(unsafe {
            SecTaskCopyValueForEntitlement(
                task.as_CFTypeRef(),
                name.as_concrete_TypeRef(),
                std::ptr::null_mut(),
            )
        })
    };
    Entitlements {
        application_identifier: read("com.apple.application-identifier"),
        access_groups: read("keychain-access-groups"),
        team_identifier: read("com.apple.developer.team-identifier"),
    }
}

/// The access group of this process (SG-F7), or `None`.
pub fn entitled_group() -> Option<String> {
    access_group(&read_entitlements())
}

/// True when this process claims any Keychain entitlement. Tests use it to
/// prove that an unsigned binary can make no change.
pub fn has_keychain_entitlements() -> bool {
    read_entitlements().any()
}

/// One call kind's dictionary content.
#[derive(Clone, Copy)]
pub(crate) enum Op<'a> {
    Add {
        service: &'a str,
        account: &'a str,
        data: &'a CFData,
    },
    Copy {
        service: &'a str,
        account: &'a str,
    },
    Delete {
        service: &'a str,
        account: &'a str,
    },
    /// Attributes of at most `max` items, of one service or of the group.
    List {
        service: Option<&'a str>,
        max: i64,
    },
}

/// KC-U1: the one dictionary builder. Every call kind gets the class, the
/// data-protection flag, the access group, synchronizable false and UI
/// fail; none gets `kSecUseKeychain`.
pub(crate) fn pairs(group: &str, op: Op<'_>) -> Vec<(CFString, CFType)> {
    // SAFETY (each `key` and constant read below): the statics are Security
    // framework constants, valid for the process lifetime.
    let mut pairs = unsafe {
        vec![
            (key(kSecClass), key(kSecClassGenericPassword).as_CFType()),
            (
                key(kSecUseDataProtectionKeychain),
                CFBoolean::true_value().as_CFType(),
            ),
            (key(kSecAttrAccessGroup), CFString::new(group).as_CFType()),
            (
                key(kSecAttrSynchronizable),
                CFBoolean::false_value().as_CFType(),
            ),
            (
                key(kSecUseAuthenticationUI),
                key(kSecUseAuthenticationUIFail).as_CFType(),
            ),
        ]
    };
    let mut item = |service: &str, account: &str| {
        // SAFETY: Security framework constants, as above.
        unsafe {
            pairs.push((key(kSecAttrService), CFString::new(service).as_CFType()));
            pairs.push((key(kSecAttrAccount), CFString::new(account).as_CFType()));
        }
    };
    match op {
        Op::Add {
            service,
            account,
            data,
        } => {
            item(service, account);
            // SAFETY: Security framework constants, as above.
            unsafe {
                pairs.push((key(kSecAttrLabel), CFString::new(LABEL).as_CFType()));
                pairs.push((
                    key(kSecAttrAccessible),
                    key(kSecAttrAccessibleAfterFirstUnlockThisDeviceOnly).as_CFType(),
                ));
                pairs.push((key(kSecValueData), data.as_CFType()));
            }
        }
        Op::Copy { service, account } => {
            item(service, account);
            // SAFETY: Security framework constants, as above.
            unsafe {
                pairs.push((key(kSecReturnData), CFBoolean::true_value().as_CFType()));
                pairs.push((key(kSecMatchLimit), key(kSecMatchLimitOne).as_CFType()));
            }
        }
        Op::Delete { service, account } => item(service, account),
        Op::List { service, max } => {
            // SAFETY: Security framework constants, as above.
            unsafe {
                if let Some(service) = service {
                    pairs.push((key(kSecAttrService), CFString::new(service).as_CFType()));
                }
                pairs.push((
                    key(kSecReturnAttributes),
                    CFBoolean::true_value().as_CFType(),
                ));
                pairs.push((key(kSecMatchLimit), CFNumber::from(max).as_CFType()));
            }
        }
    }
    pairs
}

/// The key a dictionary must never carry (KC-U1).
#[cfg(test)]
pub(crate) fn file_keychain_key() -> CFString {
    // SAFETY: a Security framework constant, valid for the process lifetime.
    unsafe { key(security_framework_sys::item::kSecUseKeychain) }
}

/// The data-protection status table (SG3, KC-P1, KC-P4).
pub(crate) fn classify(code: i32) -> Failure {
    match code {
        ERR_SEC_INTERACTION_NOT_ALLOWED => Failure::Locked { status: code },
        MISSING_ENTITLEMENT => Failure::NotSigned { item_call: true },
        status => Failure::Unavailable {
            status,
            item_call: true,
        },
    }
}

/// The service and account attributes of one listed item.
pub(crate) type Attributes = (Option<String>, Option<String>);

/// The production backend: one access group read from the entitlements.
pub(crate) struct DataProtection {
    group: String,
}

impl DataProtection {
    pub(crate) fn new(group: String) -> Self {
        Self { group }
    }

    pub(crate) fn group(&self) -> &str {
        &self.group
    }

    fn dictionary(&self, op: Op<'_>) -> CFDictionary<CFString, CFType> {
        CFDictionary::from_CFType_pairs(&pairs(&self.group, op))
    }

    /// `SecItemCopyMatching` for the attributes of at most `max` items:
    /// (service, account) of each.
    pub(crate) fn attributes(
        &self,
        service: Option<&str>,
        max: usize,
    ) -> Result<Vec<Attributes>, i32> {
        let max = i64::try_from(max).map_err(|_| super::UNEXPECTED_RESULT_STATUS)?;
        let query = self.dictionary(Op::List { service, max });
        let mut result: CFTypeRef = std::ptr::null();
        // SAFETY: `query` is a live dictionary; `result` is a local the call
        // sets to a new object (create rule) on success, or leaves null.
        let code = unsafe { SecItemCopyMatching(query.as_concrete_TypeRef(), &mut result) };
        if code != 0 {
            if !result.is_null() {
                // SAFETY: a returned object follows the create rule.
                drop(unsafe { CFType::wrap_under_create_rule(result) });
            }
            return Err(code);
        }
        if result.is_null() {
            return Err(super::UNEXPECTED_RESULT_STATUS);
        }
        // SAFETY: as above; the wrapper releases the result once.
        let result = unsafe { CFType::wrap_under_create_rule(result) };
        let array = result
            .downcast::<CFArray>()
            .ok_or(super::UNEXPECTED_RESULT_STATUS)?;
        // SAFETY: Security framework constants, valid for the process.
        let (service_key, account_key) = unsafe { (key(kSecAttrService), key(kSecAttrAccount)) };
        let mut found = Vec::with_capacity(array.len() as usize);
        for entry in array.iter() {
            // SAFETY: the array owns its elements for this borrow; the get
            // rule retains the element for the wrapper's lifetime.
            let entry = unsafe { CFType::wrap_under_get_rule(*entry) };
            let attributes = entry
                .downcast::<CFDictionary>()
                .ok_or(super::UNEXPECTED_RESULT_STATUS)?;
            let text = |name: &CFString| {
                attributes
                    .find(name.as_concrete_TypeRef().cast::<c_void>())
                    .map(|raw| {
                        // SAFETY: the dictionary owns the value for this
                        // borrow; the get rule retains it for the wrapper.
                        unsafe { CFType::wrap_under_get_rule(*raw) }
                    })
                    .and_then(|v| v.downcast::<CFString>())
                    .map(|s| s.to_string())
            };
            found.push((text(&service_key), text(&account_key)));
        }
        Ok(found)
    }
}

impl Calls for DataProtection {
    fn classify(&self, code: i32) -> Failure {
        classify(code)
    }
    fn add(&self, service: &str, account: &str, data: &[u8]) -> Result<(), i32> {
        // CF uses this zeroizing buffer without copying and drops it when the
        // request dictionary is released.
        let buffer = std::sync::Arc::new(Zeroizing::new(data.to_vec()));
        let data = CFData::from_arc(buffer);
        let attributes = self.dictionary(Op::Add {
            service,
            account,
            data: &data,
        });
        // SAFETY: `attributes` is a live dictionary; a null result pointer
        // asks for no returned object.
        let code = unsafe { SecItemAdd(attributes.as_concrete_TypeRef(), std::ptr::null_mut()) };
        if code == 0 { Ok(()) } else { Err(code) }
    }
    fn copy(&self, service: &str, account: &str) -> Result<Zeroizing<Vec<u8>>, i32> {
        let query = self.dictionary(Op::Copy { service, account });
        let mut result: CFTypeRef = std::ptr::null();
        // SAFETY: `query` is a live dictionary; `result` is a local the call
        // sets to a new object (create rule) on success, or leaves null.
        let code = unsafe { SecItemCopyMatching(query.as_concrete_TypeRef(), &mut result) };
        let owned = (!result.is_null()).then(|| {
            // SAFETY: a returned object follows the create rule; the
            // wrapper releases it once.
            unsafe { CFType::wrap_under_create_rule(result) }
        });
        if code != 0 {
            return Err(code);
        }
        // The CFData itself is released without zeroizing (SS-T8).
        owned
            .and_then(|v| v.downcast::<CFData>())
            .map(|data| Zeroizing::new(data.bytes().to_vec()))
            .ok_or(super::UNEXPECTED_RESULT_STATUS)
    }
    fn delete(&self, service: &str, account: &str) -> Result<(), i32> {
        let query = self.dictionary(Op::Delete { service, account });
        // SAFETY: `query` is a live dictionary.
        let code = unsafe { SecItemDelete(query.as_concrete_TypeRef()) };
        if code == 0 { Ok(()) } else { Err(code) }
    }
    fn accounts(&self, service: &str, max: usize) -> Result<Vec<String>, i32> {
        self.attributes(Some(service), max)?
            .into_iter()
            .map(|(_, account)| account.ok_or(super::UNEXPECTED_RESULT_STATUS))
            .collect()
    }
}

/// The signed qualification run's purge of the test group (SGQ6,
/// `release-signing.md#signed-qualification-run`). Test-support only.
#[cfg(feature = "test-support")]
pub mod qualification {
    use super::*;

    /// Why the purge stopped.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum PurgeError {
        /// This process's group is not a test group: nothing was called.
        NotTestGroup,
        /// A Keychain call failed with this status.
        Status(i32),
        /// The final listing still found items.
        NotEmpty,
    }

    /// The selected group when it is the test group, else `None`.
    pub fn test_group() -> Option<String> {
        entitled_group().filter(|g| g.ends_with(&format!(".{TEST_SUFFIX}")))
    }

    /// Deletes every generic-password item of the test group, by service
    /// and account, then lists the group: returns the number deleted when
    /// the final listing is empty. Refuses with no call unless the selected
    /// group is the test group. Repeats only while a full listing (257)
    /// lost an item in its pass, so each repeat shrinks a finite set.
    pub fn purge_test_group() -> Result<usize, PurgeError> {
        let group = test_group().ok_or(PurgeError::NotTestGroup)?;
        let backend = DataProtection::new(group);
        let bound = super::super::LIST_BOUND;
        let mut deleted = 0;
        loop {
            let items = match backend.attributes(None, bound) {
                Ok(items) => items,
                Err(-25_300) => return Ok(deleted),
                Err(code) => return Err(PurgeError::Status(code)),
            };
            let mut pass = 0;
            for (service, account) in &items {
                let (Some(service), Some(account)) = (service, account) else {
                    return Err(PurgeError::Status(super::super::UNEXPECTED_RESULT_STATUS));
                };
                match Calls::delete(&backend, service, account) {
                    Ok(()) => pass += 1,
                    Err(-25_300) => {}
                    Err(code) => return Err(PurgeError::Status(code)),
                }
            }
            deleted += pass;
            if items.len() < bound || pass == 0 {
                // One more listing proves the group empty.
                return match backend.attributes(None, bound) {
                    Err(-25_300) => Ok(deleted),
                    Ok(_) => Err(PurgeError::NotEmpty),
                    Err(code) => Err(PurgeError::Status(code)),
                };
            }
        }
    }
}
