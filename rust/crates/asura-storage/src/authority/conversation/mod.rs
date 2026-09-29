//! Pure format-1 conversation authority codec and replay. Call from the bounded storage worker.
//! No method writes, flushes, repairs, starts a model, or proves durable commit.
mod codec;
mod state;
mod types;

pub use codec::{capitalize_project_name, decode_record, encode_frame, request_digest};
pub use state::{Replay, replay};
pub use types::*;
