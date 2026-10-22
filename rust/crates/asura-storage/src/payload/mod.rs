//! Retained job output payloads: identities keyed by job ID, the quota
//! ledger, staging, text pages, the person pin hold and delete, and
//! recovery. The only caller of platform payload I/O.
mod id;
mod pages;
mod recovery;
mod store;
pub use asura_platform::payload_io::PayloadError;
pub use id::{ObjectKind, job_payloads, payload_id};
pub use pages::{
    Origin, PAGE_LIMIT, PAGE_QUEUE, Page, PagePool, PageReply, PageRequest, PageState, TEXT_LIMIT,
    Ticket, read_page,
};
pub use recovery::{
    FACT_BATCH, Known, RecoveryError, RecoveryReport, known_calls, recover, recover_known,
    recover_with_facts,
};
pub use store::{
    Capture, DEFAULT_QUOTA, DeleteError, Ledger, Location, MAX_EXPIRY_PER_ATTEMPT, MAX_QUOTA, MIB,
    MIN_QUOTA, PayloadId, PayloadState, PayloadStore, RECOVERY_RESERVE, RESERVATION_STEP, Short,
    SpaceListener, StreamIndex, Victim, effective_quota,
};
