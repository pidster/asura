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
                yield_seconds: 10,
            },
        ),
        (
            "exec_cmd",
            "{\"cwd\":\"src/sub\",\"command\":\"ls\"}".into(),
            O::ExecCmd,
            Arguments::ExecCmd {
                command: "ls".into(),
                cwd: "src/sub".into(),
                yield_seconds: 10,
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

const JOB: &str = "7f3a09c2e4b15d68a0c3f1e2d4b5a697";
fn job_id() -> asura_storage::authority::conversation::Id {
    super::job_handle(JOB).unwrap()
}
fn job(text: &str) -> Parsed {
    parse_call(Some("job"), Some(text))
}
fn job_fault(text: &str) -> F {
    job(text).arguments.unwrap_err()
}

/// J3T-01 (unit): exec_cmd `yield_seconds` 0-30, default 10; the command
/// text rule (fault 14 for NUL or a control character other than tab and
/// newline); every other field bound.
#[test]
fn j3t_01_exec_cmd_yield_seconds_and_command_text() {
    let exec = |text: &str| parse_call(Some("exec_cmd"), Some(text));
    for (text, expected) in [
        (r#"{"command":"ls"}"#, 10),
        (r#"{"command":"ls","yield_seconds":0}"#, 0),
        (r#"{"command":"ls","yield_seconds":30}"#, 30),
        (r#"{"command":"ls","yield_seconds":null}"#, 10),
    ] {
        let parsed = exec(text);
        assert_eq!(parsed.operation, Some(O::ExecCmd));
        let arguments = parsed.arguments.unwrap();
        assert_eq!(
            arguments,
            Arguments::ExecCmd {
                command: "ls".into(),
                cwd: ".".into(),
                yield_seconds: expected,
            },
            "{text}"
        );
        assert_eq!(arguments.validate(), Ok(()));
    }
    for (text, expected) in [
        (r#"{"command":"ls","yield_seconds":31}"#, F::OutOfRange),
        (r#"{"command":"ls","yield_seconds":-1}"#, F::OutOfRange),
        (r#"{"command":"ls","yield_seconds":1.5}"#, F::WrongType),
        (r#"{"command":"ls","yield_seconds":"10"}"#, F::WrongType),
        (r#"{"command":"ls","yield":10}"#, F::UnexpectedField),
        (r#"{"cwd":"."}"#, F::MissingField),
        (r#"{"command":7}"#, F::WrongType),
        (r#"{"command":""}"#, F::OutOfRange),
        (r#"{"command":"a\rb"}"#, F::InvalidText),
        (r#"{"command":"a\u001bb"}"#, F::InvalidText),
        (r#"{"command":"a\u007fb"}"#, F::InvalidText),
        (r#"{"command":"ls","cwd":"../x"}"#, F::UnsafePath),
    ] {
        assert_eq!(fault(Some("exec_cmd"), Some(text)), expected, "{text}");
    }
    // Tab and newline are kept.
    let parsed = exec(r#"{"command":"printf 'a\tb'\nls"}"#);
    assert!(parsed.arguments.is_ok());
    // The lowest code wins: unexpected field (7) before out of range (9).
    assert_eq!(
        fault(
            Some("exec_cmd"),
            Some(r#"{"command":"ls","yield_seconds":99,"x":1}"#)
        ),
        F::UnexpectedField
    );
}

/// J3T-01 (unit): every valid `job` command, with its defaults.
#[test]
fn j3t_01_job_commands_parse_to_typed_arguments() {
    use asura_storage::authority::conversation::OutputStream as S;
    let id = job_id();
    let cases: Vec<(String, O, JobArguments)> = vec![
        (
            r#"{"command":"list"}"#.into(),
            O::JobList,
            JobArguments::List { before: None },
        ),
        (
            r#"{"command":"list","before":1832}"#.into(),
            O::JobList,
            JobArguments::List { before: Some(1832) },
        ),
        (
            format!(r#"{{"command":"list","before":{}}}"#, u64::MAX),
            O::JobList,
            JobArguments::List {
                before: Some(u64::MAX),
            },
        ),
        (
            format!(r#"{{"command":"status","job":"{JOB}"}}"#),
            O::JobStatus,
            JobArguments::Status { job: id },
        ),
        (
            format!(r#"{{"command":"read","job":"{JOB}","stream":"stdout"}}"#),
            O::JobRead,
            JobArguments::Read {
                job: id,
                stream: S::Stdout,
                offset: None,
                limit: 8192,
            },
        ),
        (
            format!(
                r#"{{"command":"read","job":"{JOB}","stream":"stderr","offset":9223372036854775807,"limit":1}}"#
            ),
            O::JobRead,
            JobArguments::Read {
                job: id,
                stream: S::Stderr,
                offset: Some(i64::MAX as u64),
                limit: 1,
            },
        ),
        (
            format!(
                r#"{{"command":"read","job":"{JOB}","stream":"stdout","offset":0,"limit":8192}}"#
            ),
            O::JobRead,
            JobArguments::Read {
                job: id,
                stream: S::Stdout,
                offset: Some(0),
                limit: 8192,
            },
        ),
        (
            format!(r#"{{"command":"wait","job":"{JOB}"}}"#),
            O::JobWait,
            JobArguments::Wait {
                job: id,
                seconds: None,
            },
        ),
        (
            format!(r#"{{"command":"wait","job":"{JOB}","seconds":1}}"#),
            O::JobWait,
            JobArguments::Wait {
                job: id,
                seconds: Some(1),
            },
        ),
        (
            format!(r#"{{"command":"wait","job":"{JOB}","seconds":3600}}"#),
            O::JobWait,
            JobArguments::Wait {
                job: id,
                seconds: Some(3600),
            },
        ),
        (
            format!(r#"{{"command":"cancel","job":"{JOB}"}}"#),
            O::JobCancel,
            JobArguments::Cancel { job: id },
        ),
        (
            format!(r#"{{"command":"note","job":"{JOB}","text":"a"}}"#),
            O::JobNote,
            JobArguments::Note {
                job: id,
                text: "a".into(),
            },
        ),
        (
            format!(
                r#"{{"command":"note","job":"{JOB}","text":"{}"}}"#,
                "é".repeat(1024)
            ),
            O::JobNote,
            JobArguments::Note {
                job: id,
                text: "é".repeat(1024),
            },
        ),
    ];
    for (text, op, expected) in cases {
        let parsed = job(&text);
        assert_eq!(parsed.tool.map(|t| t.name()), Some("job"));
        assert_eq!(parsed.operation, Some(op), "{text}");
        assert_eq!(expected.operation(), op);
        let arguments = Arguments::Job(expected.clone());
        assert_eq!(parsed.arguments.as_ref(), Ok(&arguments), "{text}");
        assert_eq!(arguments.validate(), Ok(()), "{text}");
        assert_eq!(expected.job(), (op != O::JobList).then_some(id), "{text}");
    }
}

/// J3T-01 (unit): every field bound and fault of each `job` command,
/// including fault 16 `invalid_job_id` and `unexpected_field` per command.
#[test]
fn j3t_01_job_faults_per_field_and_command() {
    let id = format!(r#""job":"{JOB}""#);
    let cases: Vec<(String, F)> = vec![
        // command
        ("{}".into(), F::MissingField),
        (r#"{"command":"kill"}"#.into(), F::UnknownCommand),
        (r#"{"command":"List"}"#.into(), F::UnknownCommand),
        (r#"{"command":1}"#.into(), F::WrongType),
        // job: missing, wrong type, malformed (16)
        (r#"{"command":"status"}"#.into(), F::MissingField),
        (r#"{"command":"status","job":7}"#.into(), F::WrongType),
        (r#"{"command":"status","job":""}"#.into(), F::InvalidJobId),
        (
            r#"{"command":"status","job":"7F3A09C2E4B15D68A0C3F1E2D4B5A697"}"#.into(),
            F::InvalidJobId,
        ),
        (
            r#"{"command":"status","job":"7f3a09c2e4b15d68a0c3f1e2d4b5a69"}"#.into(),
            F::InvalidJobId,
        ),
        (
            r#"{"command":"status","job":"7f3a09c2e4b15d68a0c3f1e2d4b5a6970"}"#.into(),
            F::InvalidJobId,
        ),
        (
            r#"{"command":"status","job":"7f3a09c2e4b15d68a0c3f1e2d4b5a69g"}"#.into(),
            F::InvalidJobId,
        ),
        (
            r#"{"command":"cancel","job":"00000000000000000000000000000000"}"#.into(),
            F::InvalidJobId,
        ),
        (
            r#"{"command":"wait","job":"../00000000000000000000000000000"}"#.into(),
            F::InvalidJobId,
        ),
        (
            r#"{"command":"read","stream":"stdout"}"#.into(),
            F::MissingField,
        ),
        (r#"{"command":"wait"}"#.into(), F::MissingField),
        (r#"{"command":"cancel"}"#.into(), F::MissingField),
        (r#"{"command":"note","text":"a"}"#.into(), F::MissingField),
        // list: before >= 1
        (r#"{"command":"list","before":0}"#.into(), F::OutOfRange),
        (r#"{"command":"list","before":-1}"#.into(), F::OutOfRange),
        (
            r#"{"command":"list","before":18446744073709551616}"#.into(),
            F::OutOfRange,
        ),
        (r#"{"command":"list","before":"1"}"#.into(), F::WrongType),
        // read: stream, offset, limit
        (format!(r#"{{"command":"read",{id}}}"#), F::MissingField),
        (
            format!(r#"{{"command":"read",{id},"stream":"stdin"}}"#),
            F::OutOfRange,
        ),
        (
            format!(r#"{{"command":"read",{id},"stream":1}}"#),
            F::WrongType,
        ),
        (
            format!(r#"{{"command":"read",{id},"stream":"stdout","offset":9223372036854775808}}"#),
            F::OutOfRange,
        ),
        (
            format!(r#"{{"command":"read",{id},"stream":"stdout","offset":-1}}"#),
            F::OutOfRange,
        ),
        (
            format!(r#"{{"command":"read",{id},"stream":"stdout","offset":0.5}}"#),
            F::WrongType,
        ),
        (
            format!(r#"{{"command":"read",{id},"stream":"stdout","limit":0}}"#),
            F::OutOfRange,
        ),
        (
            format!(r#"{{"command":"read",{id},"stream":"stdout","limit":8193}}"#),
            F::OutOfRange,
        ),
        (
            format!(r#"{{"command":"read",{id},"stream":"stdout","limit":true}}"#),
            F::WrongType,
        ),
        // wait: seconds 1-3600
        (
            format!(r#"{{"command":"wait",{id},"seconds":0}}"#),
            F::OutOfRange,
        ),
        (
            format!(r#"{{"command":"wait",{id},"seconds":3601}}"#),
            F::OutOfRange,
        ),
        (
            format!(r#"{{"command":"wait",{id},"seconds":"5"}}"#),
            F::WrongType,
        ),
        // note: text 1-2048 bytes, no NUL
        (format!(r#"{{"command":"note",{id}}}"#), F::MissingField),
        (
            format!(r#"{{"command":"note",{id},"text":""}}"#),
            F::OutOfRange,
        ),
        (
            format!(r#"{{"command":"note",{id},"text":"{}"}}"#, "a".repeat(2049)),
            F::OutOfRange,
        ),
        (
            format!(r#"{{"command":"note",{id},"text":"a\u0000b"}}"#),
            F::InvalidText,
        ),
        (
            format!(r#"{{"command":"note",{id},"text":["a"]}}"#),
            F::WrongType,
        ),
    ];
    for (text, expected) in &cases {
        assert_eq!(job_fault(text), *expected, "{text}");
    }
    assert!(
        job(&format!(
            r#"{{"command":"note",{id},"text":"{}"}}"#,
            "a".repeat(2048)
        ))
        .arguments
        .is_ok()
    );
    // A field the chosen command does not take is `unexpected_field`, for
    // every command and every foreign field.
    let fields = [
        ("job", format!(r#""{JOB}""#)),
        ("stream", r#""stdout""#.into()),
        ("offset", "0".into()),
        ("limit", "1".into()),
        ("seconds", "1".into()),
        ("text", r#""a""#.into()),
        ("before", "1".into()),
        ("cwd", r#"".""#.into()),
    ];
    let takes: [(&str, &[&str]); 6] = [
        ("list", &["before"]),
        ("status", &["job"]),
        ("read", &["job", "stream", "offset", "limit"]),
        ("wait", &["job", "seconds"]),
        ("cancel", &["job"]),
        ("note", &["job", "text"]),
    ];
    for (command, own) in takes {
        let mut members = vec![format!(r#""command":"{command}""#)];
        members.extend(
            fields
                .iter()
                .filter(|(name, _)| own.contains(name))
                .map(|(name, value)| format!(r#""{name}":{value}"#)),
        );
        let valid = format!("{{{}}}", members.join(","));
        assert!(job(&valid).arguments.is_ok(), "{valid}");
        for (name, value) in fields.iter().filter(|(name, _)| !own.contains(name)) {
            let text = valid.replacen('{', &format!(r#"{{"{name}":{value},"#), 1);
            let parsed = job(&text);
            assert_eq!(parsed.arguments, Err(F::UnexpectedField), "{text}");
            // The command is known, so the operation stays recorded.
            assert!(parsed.operation.is_some(), "{text}");
            // `null` is an absent member, never unexpected.
            let text = valid.replacen('{', &format!(r#"{{"{name}":null,"#), 1);
            assert!(job(&text).arguments.is_ok(), "{text}");
        }
    }
    // The lowest code wins: missing field (6), unexpected field (7) and out
    // of range (9) before invalid job ID (16).
    assert_eq!(
        job_fault(r#"{"command":"read","job":"x"}"#),
        F::MissingField
    );
    assert_eq!(
        job_fault(r#"{"command":"cancel","job":"x","text":"a"}"#),
        F::UnexpectedField
    );
    assert_eq!(
        job_fault(r#"{"command":"wait","job":"x","seconds":0}"#),
        F::OutOfRange
    );
    // An unknown or missing command leaves no operation.
    assert_eq!(job(r#"{"command":"kill"}"#).operation, None);
    assert_eq!(job("{}").operation, None);
    // A known command keeps its operation when the handle is malformed.
    let parsed = job(r#"{"command":"status","job":"x"}"#);
    assert_eq!(
        (parsed.operation, parsed.arguments),
        (Some(O::JobStatus), Err(F::InvalidJobId))
    );
}

/// J3-E8: a handle is exactly 32 lowercase hexadecimal digits, not all
/// zero, and reads back as the job ID.
#[test]
fn job_handle_reads_32_lowercase_hex_digits() {
    let id = job_id();
    assert_eq!(id[0], 0x7f);
    assert_eq!(id[15], 0x97);
    assert_eq!(crate::job_text::handle(&id), JOB);
    for bad in [
        "",
        "00000000000000000000000000000000",
        "7F3A09C2E4B15D68A0C3F1E2D4B5A697",
        "+f3a09c2e4b15d68a0c3f1e2d4b5a697",
        " 7f3a09c2e4b15d68a0c3f1e2d4b5a69",
        "7f3a09c2e4b15d68a0c3f1e2d4b5a69é",
    ] {
        assert_eq!(super::job_handle(bad), None, "{bad:?}");
    }
    assert_eq!(
        super::job_handle("00000000000000000000000000000001").map(|id| id[15]),
        Some(1)
    );
}
