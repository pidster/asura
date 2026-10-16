//! Canonical physical stored-memory footprint adapter; no additional DB engine owner.
use asura_platform::{DatabaseDirectory, RuntimeDirectory};
use std::sync::atomic::{AtomicBool, Ordering};
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SizeError {
    /// Retired for new samples (owner, 2026-10-16); kept for wire mapping.
    Timeout,
    Cancelled,
    Unsafe,
    Unavailable,
    Limit,
}
/// Measure the stored-memory footprint. No time bound (owner, 2026-10-16): the
/// sample ends on completion, an error, the entry limit or cancel; the status
/// pane shows the size as measuring until it returns.
pub fn stored_bytes(runtime: RuntimeDirectory, cancel: &AtomicBool) -> Result<u64, SizeError> {
    if cancel.load(Ordering::Acquire) {
        return Err(SizeError::Cancelled);
    }
    let measure = || {
        let directory = DatabaseDirectory::open(runtime, false)?;
        directory.stored_bytes(cancel)
    };
    measure().map_err(|error| match error {
        // The footprint walk reports cancel as `Deadline`.
        asura_platform::Error::Deadline => SizeError::Cancelled,
        asura_platform::Error::UnsafeRuntime => SizeError::Unsafe,
        asura_platform::Error::Unavailable => SizeError::Limit,
        _ => SizeError::Unavailable,
    })
}
