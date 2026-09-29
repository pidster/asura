//! Metadata-only audit file adapter. The service owns its sole blocking worker.
mod types;
pub use types::*;
mod store;
pub use store::{AuditStore, Opened};

#[cfg(test)]
mod tests;
