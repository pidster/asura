//! File and database persistence through bounded canonical adapters.
#![forbid(unsafe_code)]
pub mod authority;

pub mod audit;
pub mod config;
pub mod logs;
#[cfg(feature = "embedded-memory")]
pub mod memory;

pub mod sensors;

pub mod stored_memory;
