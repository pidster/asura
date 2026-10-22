//! The store's only route to the platform (`docs/designs/secret-store.md`,
//! worker seam for tests). The port carries platform types, so the store has
//! no second status table and no raw `OSStatus` path.
use asura_platform::keychain::{Added, Failure, Found, ItemKey, Keychain, Listing, Removed};

/// The four item calls the worker makes. Production forwards to
/// [`Keychain`]; unit tests use a scripted port; integration tests use the
/// gated port (`test-support`).
pub(crate) trait KeychainPort: Send + 'static {
    fn add(&self, key: &ItemKey, data: &[u8]) -> Result<Added, Failure>;
    fn get(&self, key: &ItemKey) -> Result<Found, Failure>;
    fn delete(&self, key: &ItemKey) -> Result<Removed, Failure>;
    fn list(&self, installation: &[u8; 16]) -> Result<Listing, Failure>;
}

impl KeychainPort for Keychain {
    fn add(&self, key: &ItemKey, data: &[u8]) -> Result<Added, Failure> {
        Keychain::add(self, key, data)
    }
    fn get(&self, key: &ItemKey) -> Result<Found, Failure> {
        Keychain::get(self, key)
    }
    fn delete(&self, key: &ItemKey) -> Result<Removed, Failure> {
        Keychain::delete(self, key)
    }
    fn list(&self, installation: &[u8; 16]) -> Result<Listing, Failure> {
        Keychain::list(self, installation)
    }
}

#[cfg(feature = "test-support")]
pub use gated::{Call, Gate};

/// The gated port for integration tests (SS-W4, SS-W5, SS-W8): the real
/// scratch [`Keychain`] behind a gate that can hold a call until the test
/// releases it, or fail one named call once. Every other call reaches the
/// real scratch keychain, with its own target pre-check.
#[cfg(feature = "test-support")]
mod gated {
    use super::*;
    use std::sync::{Arc, Condvar, Mutex, MutexGuard};

    /// One port call, as the gate records it.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum Call {
        Add,
        Get,
        Delete,
        List,
    }

    /// The status a gated failure reports (`errSecNotAvailable`).
    pub const GATED_STATUS: i32 = -25_291;

    #[derive(Default)]
    struct State {
        held: bool,
        blocked: usize,
        fail_once: Option<Call>,
        calls: Vec<Call>,
    }

    /// Test control over the gated port. Clones share one gate.
    #[derive(Clone, Default)]
    pub struct Gate(Arc<(Mutex<State>, Condvar)>);

    impl Gate {
        fn state(&self) -> MutexGuard<'_, State> {
            self.0.0.lock().unwrap_or_else(|e| e.into_inner())
        }
        /// Later calls wait before reaching the Keychain until [`Gate::release`].
        pub fn hold(&self) {
            self.state().held = true;
        }
        /// Lets every held and later call proceed.
        pub fn release(&self) {
            self.state().held = false;
            self.0.1.notify_all();
        }
        /// The next call of `call` returns `Failure::Unavailable` without a
        /// Keychain call.
        pub fn fail_once(&self, call: Call) {
            self.state().fail_once = Some(call);
        }
        /// Waits until a call is held at the gate. Ends when the worker
        /// reaches the gate; the test owns that closure.
        pub fn wait_blocked(&self) {
            let mut state = self.state();
            while state.blocked == 0 {
                state = self.0.1.wait(state).unwrap_or_else(|e| e.into_inner());
            }
        }
        /// Every call the port received, in order.
        pub fn calls(&self) -> Vec<Call> {
            self.state().calls.clone()
        }
        fn enter(&self, call: Call) -> Option<Failure> {
            let mut state = self.state();
            state.calls.push(call);
            state.blocked += 1;
            self.0.1.notify_all();
            while state.held {
                state = self.0.1.wait(state).unwrap_or_else(|e| e.into_inner());
            }
            state.blocked -= 1;
            (state.fail_once == Some(call)).then(|| {
                state.fail_once = None;
                Failure::Unavailable {
                    status: GATED_STATUS,
                    item_call: false,
                }
            })
        }
    }

    pub(crate) struct GatedPort {
        pub keychain: Keychain,
        pub gate: Gate,
    }

    impl KeychainPort for GatedPort {
        fn add(&self, key: &ItemKey, data: &[u8]) -> Result<Added, Failure> {
            self.gate
                .enter(Call::Add)
                .map_or_else(|| self.keychain.add(key, data), Err)
        }
        fn get(&self, key: &ItemKey) -> Result<Found, Failure> {
            self.gate
                .enter(Call::Get)
                .map_or_else(|| self.keychain.get(key), Err)
        }
        fn delete(&self, key: &ItemKey) -> Result<Removed, Failure> {
            self.gate
                .enter(Call::Delete)
                .map_or_else(|| self.keychain.delete(key), Err)
        }
        fn list(&self, installation: &[u8; 16]) -> Result<Listing, Failure> {
            self.gate
                .enter(Call::List)
                .map_or_else(|| self.keychain.list(installation), Err)
        }
    }
}

#[cfg(feature = "test-support")]
pub(crate) use gated::GatedPort;
