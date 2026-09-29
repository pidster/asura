//! Typed memory reads and bounded evidence serialization; no database access here.
use super::*;
use asura_storage::memory::{self, ReadResult};
fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
fn failure(call: &tools::Call, status: u8, text: &str) -> journal::ToolResult {
    journal::ToolResult {
        operation: call.operation,
        generation: call.generation,
        ordinal: call.ordinal,
        status,
        text: text.into(),
        next_offset: None,
        truncated: false,
    }
}
pub(super) fn encode(call: &tools::Call, value: memory::Result<ReadResult>) -> journal::ToolResult {
    let mut result = failure(call, 1, "");
    let value = match value {
        Ok(value) => value,
        Err(error) => {
            return match error {
                memory::Error::InvalidInput => failure(call, 3, ""),
                memory::Error::LimitExceeded => failure(call, 7, ""),
                memory::Error::Cancelled => failure(call, 6, ""),
                memory::Error::Timeout => failure(call, 5, ""),
                memory::Error::NotFound => failure(call, 4, "memory_note_not_found"),
                memory::Error::Busy => failure(call, 4, "memory_busy"),
                _ => failure(call, 4, "memory_unavailable"),
            };
        }
    };
    use std::fmt::Write;
    match (&call.arguments, value) {
        (tools::Arguments::MemoryListNotes { limit, .. }, ReadResult::Page(page))
            if page.notes.len() <= *limit as usize =>
        {
            let _ = writeln!(
                result.text,
                "next={}",
                page.next
                    .map(|id| hex(&id.bytes()))
                    .unwrap_or_else(|| "none".into())
            );
            for note in page.notes {
                if note.preview.len() > 256 {
                    return failure(call, 7, "");
                }
                let preview: String = note.preview.chars().flat_map(char::escape_debug).collect();
                let _ = writeln!(
                    result.text,
                    "version={} object={} sha256={} preview=\"{}\"",
                    hex(&note.version_id.bytes()),
                    hex(&note.object_id.bytes()),
                    hex(&note.body_sha256),
                    preview
                );
            }
        }
        (tools::Arguments::MemoryGetNote { offset, limit, .. }, ReadResult::Note(note)) => {
            let Ok(start) = usize::try_from(*offset) else {
                return failure(call, 3, "");
            };
            if start > note.body.len() || !note.body.is_char_boundary(start) {
                return failure(call, 3, "");
            }
            let mut end = start.saturating_add(*limit as usize).min(note.body.len());
            while !note.body.is_char_boundary(end) {
                end -= 1;
            }
            if end == start && start < note.body.len() {
                return failure(call, 7, "");
            }
            result.text = note.body[start..end].into();
            result.next_offset = Some(end as u64);
            result.truncated = end < note.body.len();
        }
        (tools::Arguments::MemoryNoteSources { .. }, ReadResult::Sources(mut notes))
            if notes.len() <= 32 =>
        {
            notes.sort_by_key(|note| note.version_id.bytes());
            let _ = writeln!(result.text, "sources={}", notes.len());
            for note in notes {
                let _ = writeln!(
                    result.text,
                    "version={} object={} sha256={}",
                    hex(&note.version_id.bytes()),
                    hex(&note.object_id.bytes()),
                    hex(&note.body_sha256)
                );
            }
        }
        (tools::Arguments::MemoryListNotes { .. }, ReadResult::Page(_))
        | (tools::Arguments::MemoryNoteSources { .. }, ReadResult::Sources(_)) => {
            return failure(call, 7, "");
        }
        _ => return failure(call, 4, "memory_unavailable"),
    }
    if result.text.len() > tools::MAX_RESULT_BYTES {
        failure(call, 7, "")
    } else {
        result
    }
}
pub(super) fn writer_error(call: &tools::Call, error: writer::Error) -> journal::ToolResult {
    match error {
        writer::Error::Busy => failure(call, 4, "memory_busy"),
        writer::Error::Deadline => failure(call, 5, ""),
        writer::Error::Cancelled => failure(call, 6, ""),
        writer::Error::Invalid => failure(call, 3, ""),
        writer::Error::Limit => failure(call, 7, ""),
        _ => failure(call, 4, "memory_unavailable"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn id(n: u8) -> memory::Id {
        memory::Id::new([n; 16]).unwrap()
    }
    fn call(arguments: tools::Arguments) -> tools::Call {
        tools::Call {
            operation: [1; 16],
            generation: 1,
            ordinal: 1,
            arguments,
        }
    }
    fn note(body: &str) -> memory::Note {
        memory::Note {
            binding: memory::Binding {
                installation_id: id(1),
                graph_id: id(2),
                init_operation_id: id(3),
            },
            context_id: id(4),
            object_id: id(2),
            version_id: id(3),
            operation_id: id(4),
            body: body.into(),
            body_sha256: [5; 32],
        }
    }
    #[test]
    fn get_pages_preserve_utf8_and_exact_offsets() {
        for (offset, limit, status, text, next, truncated) in [
            (0, 1, 7, "", None, false),
            (0, 2, 1, "é", Some(2), true),
            (1, 2, 3, "", None, false),
            (2, 3, 1, "abc", Some(5), false),
            (5, 1, 1, "", Some(5), false),
            (6, 1, 3, "", None, false),
        ] {
            let result = encode(
                &call(tools::Arguments::MemoryGetNote {
                    version: "03".repeat(16),
                    offset,
                    limit,
                }),
                Ok(ReadResult::Note(note("éabc"))),
            );
            assert_eq!(
                (
                    result.status,
                    result.text.as_str(),
                    result.next_offset,
                    result.truncated
                ),
                (status, text, next, truncated)
            );
        }
    }
    #[test]
    fn list_escapes_evidence_and_sources_are_sorted() {
        let page = memory::NotePage {
            notes: vec![memory::NoteSummary {
                object_id: id(2),
                version_id: id(3),
                operation_id: id(4),
                body_sha256: [5; 32],
                preview: "line\n\"\\\u{1b}".into(),
            }],
            next: Some(id(3)),
        };
        let result = encode(
            &call(tools::Arguments::MemoryListNotes {
                after: None,
                limit: 8,
            }),
            Ok(ReadResult::Page(page)),
        );
        assert_eq!(result.status, 1);
        assert_eq!(result.text.lines().count(), 2);
        assert!(!result.text.contains('\u{1b}'));
        assert!(result.text.contains("preview=\"line\\n\\\"\\\\\\u{1b}\""));
        assert_eq!(result.next_offset, None);
        let mut second = note("second");
        second.version_id = id(1);
        let sources = encode(
            &call(tools::Arguments::MemoryNoteSources {
                version: "03".repeat(16),
            }),
            Ok(ReadResult::Sources(vec![note("first"), second])),
        );
        assert!(
            sources
                .text
                .lines()
                .nth(1)
                .unwrap()
                .starts_with(&format!("version={}", "01".repeat(16)))
        );
    }
    #[test]
    fn memory_failures_are_fixed_and_do_not_disclose_data() {
        let call = call(tools::Arguments::MemoryNoteSources {
            version: "03".repeat(16),
        });
        for (error, status, text) in [
            (memory::Error::NotFound, 4, "memory_note_not_found"),
            (memory::Error::InvalidRecord, 4, "memory_unavailable"),
            (memory::Error::Busy, 4, "memory_busy"),
            (memory::Error::Timeout, 5, ""),
            (memory::Error::Cancelled, 6, ""),
            (memory::Error::LimitExceeded, 7, ""),
        ] {
            let result = encode(&call, Err(error));
            assert_eq!((result.status, result.text.as_str()), (status, text));
        }
        assert_eq!(
            encode(&call, Ok(ReadResult::Sources(vec![note("x"); 33]))).status,
            7
        );
    }
}
