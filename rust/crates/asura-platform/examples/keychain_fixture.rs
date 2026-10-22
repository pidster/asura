//! SS-I7 fixture binary (`docs/designs/secret-store.md`, test-support only).
//!
//! `keychain_fixture <scratch keychain path> <password> <service> <account>`
//! writes one generic-password item into an existing scratch keychain. It is a
//! different executable from the test that runs it, so the item's default ACL
//! does not trust the test binary. The write goes through
//! `asura_platform::keychain`, so it pre-checks the target first and never
//! writes to a missing keychain. Prints one reply word; exits 0 on `stored`.
use asura_platform::keychain::{Added, ItemKey, Keychain, Target};
use std::path::PathBuf;
use zeroize::Zeroizing;

/// The data every fixture item holds.
const FIXTURE_DATA: &[u8] = b"asura keychain fixture item";

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let [path, password, service, account] = args.as_slice() else {
        eprintln!("usage: keychain_fixture <keychain path> <password> <service> <account>");
        std::process::exit(2);
    };
    let Some(key) = ItemKey::parse(service, account) else {
        eprintln!("service or account does not follow the item contract");
        std::process::exit(2);
    };
    let target = Target::Scratch {
        path: PathBuf::from(path),
        password: Zeroizing::new(password.clone()),
    };
    let reply = Keychain::open(&target).and_then(|keychain| keychain.add(&key, FIXTURE_DATA));
    match reply {
        Ok(Added::Stored) => println!("stored"),
        Ok(Added::Duplicate) => {
            println!("duplicate");
            std::process::exit(1);
        }
        Err(failure) => {
            println!("{failure:?}");
            std::process::exit(1);
        }
    }
}
