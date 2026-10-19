//! Canonical kind 10 and 11 journals for tests in other crates. Never used at runtime.
use super::conversation::{
    ActiveBinding, FrameContext, PendingInit, Record, Request, encode_frame, request_digest,
};

/// Installation `0x11…`, initialization request `0x22…`, graph `0x44…`.
/// Returns kind 10 alone, or kind 10 followed by its kind 11 binding.
pub fn initialization_journal(bound: bool) -> Vec<u8> {
    let mut bytes = Vec::new();
    let mut records = vec![Record::PendingInit(PendingInit {
        request: [0x22; 16],
        digest: request_digest(Request::Initialize { mode: 1 }).expect("valid mode"),
        mode: 1,
        configuration_revision: 1,
        configuration_digest: [0x33; 32],
        graph: [0x44; 16],
    })];
    if bound {
        records.push(Record::ActiveBinding(ActiveBinding {
            request: [0x22; 16],
            generation: 1,
            graph: [0x44; 16],
            configuration_digest: [0x33; 32],
        }));
    }
    let mut prior_digest = [0; 32];
    for (index, record) in records.iter().enumerate() {
        let sequence = index as u64 + 1;
        let frame = encode_frame(
            &FrameContext {
                installation_id: [0x11; 16],
                transition_id: [sequence as u8; 16],
                sequence,
                prior_digest,
                expected_revision: sequence - 1,
                owner_generation: 1,
            },
            record,
        )
        .expect("valid fixture frame");
        prior_digest.copy_from_slice(&frame[frame.len() - 40..frame.len() - 8]);
        bytes.extend(frame);
    }
    bytes
}

/// Every segment file of a scratch home's journal, concatenated in order.
/// Replaying the result as one stream gives locators into these bytes.
pub fn journal_bytes(home: &std::path::Path) -> std::io::Result<Vec<u8>> {
    let control = home.join(".asura/state/control");
    let mut names: Vec<_> = std::fs::read_dir(&control)?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.starts_with("seg-") && name.ends_with(".log"))
        .collect();
    names.sort();
    let mut bytes = Vec::new();
    for name in names {
        bytes.extend(std::fs::read(control.join(name))?);
    }
    Ok(bytes)
}
