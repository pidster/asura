use super::*;
use asura_storage::authority::conversation::{ToolFault as F, ToolOperation as O};

fn fault(name: Option<&str>, text: Option<&str>) -> F {
    parse_call(name, text).arguments.unwrap_err()
}
const ID: &str = "0123456789abcdef0123456789abcdef";

/// TCR-U04: each fault code 1-15 from a fixed input.
#[test]
fn tcr_u04_each_fault_from_a_fixed_input() {
    // Depth 33 is over the 32-level limit.
    let deep = format!("{}1{}", "[".repeat(33), "]".repeat(33));
    let cases: [(Option<&str>, Option<&str>, F); 15] = [
        (Some("project"), Some("{\"command\":"), F::InvalidJson),
        (Some("project"), Some(deep.as_str()), F::JsonLimit),
        (Some("project"), Some("[1]"), F::NotObject),
        (
            Some("service"),
            Some("{\"command\":\"status\",\"command\":\"tools\"}"),
            F::DuplicateKey,
        ),
        (
            Some("service"),
            Some("{\"command\":\"reboot\"}"),
            F::UnknownCommand,
        ),
        (
            Some("project"),
            Some("{\"command\":\"read_file\"}"),
            F::MissingField,
        ),
        (
            Some("service"),
            Some("{\"command\":\"status\",\"limit\":2}"),
            F::UnexpectedField,
        ),
        (
            Some("project"),
            Some("{\"command\":\"read_file\",\"path\":\"a\",\"offset\":\"zero\"}"),
            F::WrongType,
        ),
        (
            Some("service"),
            Some("{\"command\":\"audit\",\"limit\":17}"),
            F::OutOfRange,
        ),
        (
            Some("project"),
            Some("{\"command\":\"read_file\",\"path\":\"../secret\"}"),
            F::UnsafePath,
        ),
        (
            Some("memory"),
            Some("{\"command\":\"get_note\",\"version\":\"ABC\"}"),
            F::InvalidVersionId,
        ),
        (Some("project"), None, F::ArgumentsOverLimit),
        (None, Some("{}"), F::NameOverLimit),
        (
            Some("exec_cmd"),
            Some("{\"command\":\"a\\u0000b\"}"),
            F::InvalidText,
        ),
        (Some("delete_everything"), Some("{}"), F::UnregisteredName),
    ];
    for (index, (name, text, expected)) in cases.into_iter().enumerate() {
        assert_eq!(fault(name, text), expected, "case {index}");
        assert_eq!(expected as u8, index as u8 + 1);
    }
    // A name with a control character is invalid text, not unregistered.
    assert_eq!(fault(Some("pro\nject"), Some("{}")), F::InvalidText);
}

/// TCR-U04: the first fault in table order wins when two apply.
#[test]
fn tcr_u04_lowest_fault_code_wins() {
    // Invalid JSON (1) before an unregistered name (15).
    assert_eq!(fault(Some("nope"), Some("{")), F::InvalidJson);
    // Not an object (3) before an over-limit name (13).
    assert_eq!(fault(None, Some("true")), F::NotObject);
    // Missing field (6) before unexpected field (7) and wrong type (8).
    assert_eq!(
        fault(
            Some("project"),
            Some("{\"command\":\"read_file\",\"extra\":1,\"offset\":\"x\"}")
        ),
        F::MissingField
    );
    // Unexpected field (7) before out of range (9) and unsafe path (10).
    assert_eq!(
        fault(
            Some("project"),
            Some("{\"command\":\"read_file\",\"path\":\"/etc\",\"limit\":0,\"x\":1}")
        ),
        F::UnexpectedField
    );
    // Out of range (9) before unsafe path (10).
    assert_eq!(
        fault(
            Some("project"),
            Some("{\"command\":\"read_file\",\"path\":\"/etc\",\"limit\":0}")
        ),
        F::OutOfRange
    );
    // Arguments over limit (12) before name over limit (13).
    assert_eq!(fault(None, None), F::ArgumentsOverLimit);
    // Unknown command leaves no operation; missing command too.
    let parsed = parse_call(Some("memory"), Some("{\"command\":\"forget\"}"));
    assert_eq!(parsed.operation, None);
    assert_eq!(parse_call(Some("service"), Some("{}")).operation, None);
    assert_eq!(parse_call(Some("exec_cmd"), Some("{}")).operation, None);
    // A known command keeps its operation when a later field is invalid.
    let parsed = parse_call(
        Some("project"),
        Some("{\"command\":\"read_file\",\"path\":\"..\"}"),
    );
    assert_eq!(
        (parsed.operation, parsed.arguments),
        (Some(O::ReadFile), Err(F::UnsafePath))
    );
}

/// TCR-U05: every valid command of the four tools gives the same typed
/// arguments as the former helper defaults and service conversion.
#[test]
fn tcr_u05_valid_commands_match_the_former_typed_path() {
    let cases: Vec<(&str, String, O, Arguments)> = vec![
        (
            "project",
            "{\"command\":\"read_file\",\"path\":\"src/main.rs\"}".into(),
            O::ReadFile,
            Arguments::ReadFile {
                path: "src/main.rs".into(),
                offset: 0,
                limit: 4096,
            },
        ),
        (
            "project",
            "{\"command\":\"read_file\",\"path\":\"資料/a.rs\",\"offset\":7,\"limit\":16384}"
                .into(),
            O::ReadFile,
            Arguments::ReadFile {
                path: "資料/a.rs".into(),
                offset: 7,
                limit: 16_384,
            },
        ),
        (
            "project",
            "{\"command\":\"list_directory\"}".into(),
            O::ListDirectory,
            Arguments::ListDirectory { path: ".".into() },
        ),
        (
            "project",
            "{\"command\":\"list_directory\",\"path\":\"src\",\"offset\":null}".into(),
            O::ListDirectory,
            Arguments::ListDirectory { path: "src".into() },
        ),
        (
            "service",
            "{\"command\":\"status\"}".into(),
            O::Status,
            Arguments::ObserveStatus,
        ),
        (
            "service",
            "{\"command\":\"tools\"}".into(),
            O::Tools,
            Arguments::ListTools,
        ),
        (
            "service",
            "{\"command\":\"audit\"}".into(),
            O::Audit,
            Arguments::ReadAudit { limit: 16 },
        ),
        (
            "service",
            "{\"command\":\"audit\",\"limit\":1}".into(),
            O::Audit,
            Arguments::ReadAudit { limit: 1 },
        ),
        (
            "memory",
            "{\"command\":\"list_notes\"}".into(),
            O::ListNotes,
            Arguments::MemoryListNotes {
                after: None,
                limit: 8,
            },
        ),
        (
            "memory",
            format!("{{\"command\":\"list_notes\",\"after\":\"{ID}\",\"limit\":2}}"),
            O::ListNotes,
            Arguments::MemoryListNotes {
                after: Some(ID.into()),
                limit: 2,
            },
        ),
        (
            "memory",
            format!("{{\"command\":\"get_note\",\"version\":\"{ID}\"}}"),
            O::GetNote,
            Arguments::MemoryGetNote {
                version: ID.into(),
                offset: 0,
                limit: 4096,
            },
        ),
        (
            "memory",
            format!("{{\"command\":\"note_sources\",\"version\":\"{ID}\"}}"),
            O::NoteSources,
            Arguments::MemoryNoteSources { version: ID.into() },
        ),
        (
            "memory",
            "{\"command\":\"create_note\",\"body\":\" exact\\nbody \"}".into(),
            O::CreateNote,
            Arguments::MemoryCreateNote {
                body: " exact\nbody ".into(),
                source_version: None,
            },
        ),
        (
            "memory",
            format!("{{\"command\":\"create_note\",\"body\":\"b\",\"source_version\":\"{ID}\"}}"),
            O::CreateNote,
            Arguments::MemoryCreateNote {
                body: "b".into(),
                source_version: Some(ID.into()),
            },
        ),
        (
            "exec_cmd",
            "{\"command\":\"printf hello\"}".into(),
            O::ExecCmd,
            Arguments::ExecCmd {
                command: "printf hello".into(),
                cwd: ".".into(),
            },
        ),
        (
            "exec_cmd",
            "{\"cwd\":\"src/sub\",\"command\":\"ls\"}".into(),
            O::ExecCmd,
            Arguments::ExecCmd {
                command: "ls".into(),
                cwd: "src/sub".into(),
            },
        ),
    ];
    for (name, text, op, expected) in cases {
        let parsed = parse_call(Some(name), Some(&text));
        assert_eq!(parsed.operation, Some(op), "{text}");
        assert_eq!(parsed.arguments.as_ref(), Ok(&expected), "{text}");
        // Every parsed call passes the executor's own argument guard.
        assert_eq!(expected.validate(), Ok(()), "{text}");
        assert_eq!(parsed.tool.map(|t| t.name()), Some(name));
    }
    // The registry names are the storage names replay uses.
    assert_eq!(
        crate::tools::REGISTRY
            .iter()
            .map(|d| d.name)
            .collect::<Vec<_>>(),
        asura_storage::authority::conversation::TOOL_NAMES
    );
}

/// Size, range and identifier bounds stay what the typed path enforced.
#[test]
fn bounds_match_the_former_wire_and_service_rules() {
    let body = |n: usize| {
        format!(
            "{{\"command\":\"create_note\",\"body\":\"{}\"}}",
            "a".repeat(n)
        )
    };
    assert!(
        parse_call(Some("memory"), Some(&body(16_384)))
            .arguments
            .is_ok()
    );
    assert_eq!(fault(Some("memory"), Some(&body(16_385))), F::OutOfRange);
    assert_eq!(fault(Some("memory"), Some(&body(0))), F::OutOfRange);
    let command = |n: usize| format!("{{\"command\":\"{}\"}}", "a".repeat(n));
    assert!(
        parse_call(Some("exec_cmd"), Some(&command(8192)))
            .arguments
            .is_ok()
    );
    assert_eq!(fault(Some("exec_cmd"), Some(&command(8193))), F::OutOfRange);
    assert_eq!(fault(Some("exec_cmd"), Some(&command(0))), F::OutOfRange);
    for cwd in ["", "/tmp", "..", "a/../b"] {
        let text = format!("{{\"command\":\"ls\",\"cwd\":\"{cwd}\"}}");
        assert_eq!(fault(Some("exec_cmd"), Some(&text)), F::UnsafePath, "{cwd}");
    }
    for limit in ["0", "9"] {
        let text = format!("{{\"command\":\"list_notes\",\"limit\":{limit}}}");
        assert_eq!(fault(Some("memory"), Some(&text)), F::OutOfRange);
    }
    for (value, expected) in [
        ("-1", F::OutOfRange),
        ("1.5", F::WrongType),
        ("1e3", F::WrongType),
        ("18446744073709551616", F::OutOfRange),
        ("true", F::WrongType),
    ] {
        let text = format!("{{\"command\":\"read_file\",\"path\":\"a\",\"offset\":{value}}}");
        assert_eq!(fault(Some("project"), Some(&text)), expected, "{value}");
    }
    let text = "{\"command\":\"read_file\",\"path\":\"a\",\"offset\":9223372036854775807}";
    assert_eq!(fault(Some("project"), Some(text)), F::OutOfRange);
    for version in ["", "00000000000000000000000000000000", "../../private"] {
        let text = format!("{{\"command\":\"note_sources\",\"version\":\"{version}\"}}");
        assert_eq!(fault(Some("memory"), Some(&text)), F::InvalidVersionId);
    }
    // list_directory takes no paging members.
    assert_eq!(
        fault(
            Some("project"),
            Some("{\"command\":\"list_directory\",\"limit\":1}")
        ),
        F::UnexpectedField
    );
}
