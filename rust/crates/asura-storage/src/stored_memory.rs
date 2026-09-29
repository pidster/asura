//! Canonical physical stored-memory footprint adapter; no additional DB engine owner.
use asura_platform::{DatabaseDirectory, RuntimeDirectory};
use std::{
    sync::atomic::{AtomicBool, Ordering},
    time::Instant,
};
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SizeError {
    Timeout,
    Cancelled,
    Unsafe,
    Unavailable,
    Limit,
}
pub fn stored_bytes(
    runtime: RuntimeDirectory,
    deadline: Instant,
    cancel: &AtomicBool,
) -> Result<u64, SizeError> {
    if cancel.load(Ordering::Acquire) {
        return Err(SizeError::Cancelled);
    }
    if Instant::now() >= deadline {
        return Err(SizeError::Timeout);
    }
    let measure = || {
        let directory = DatabaseDirectory::open(runtime, false)?;
        directory.stored_bytes(deadline, cancel)
    };
    measure().map_err(|error| match error {
        asura_platform::Error::Deadline if cancel.load(Ordering::Acquire) => SizeError::Cancelled,
        asura_platform::Error::Deadline => SizeError::Timeout,
        asura_platform::Error::UnsafeRuntime => SizeError::Unsafe,
        asura_platform::Error::Unavailable => SizeError::Limit,
        _ => SizeError::Unavailable,
    })
}
