//! TCR-U06–U09 and refinements TCR-D4, TCR-D19–D22. Fixture secrets are
//! assembled at run time so no token-shaped literal is committed.
use super::*;
use crate::secrets::{RefusalCode, detection, rewrite_exec_cmd_call};

fn rules() -> &'static RuleSet {
    detection().expect("rule set self-check")
}
fn github(seed: &str) -> String {
    let mut body = String::new();
    while body.len() < 36 {
        body.push_str(seed);
    }
    body.truncate(36);
    ["ghp_", &body].concat()
}
fn plain(name: &str, text: &str) -> RedactedArguments {
    redact_arguments(
        rules(),
        &ArgumentsInput {
            name: Some(name),
            arguments: Some(text),
            exec_cmd: None,
            exec_cmd_name: false,
        },
    )
}
fn holds(out: &RedactedArguments, value: &str) -> bool {
    out.text.contains(value) || out.name.as_deref().is_some_and(|n| n.contains(value))
}

/// TCR-U06: a secret in a key, a value, the name and an escaped value.
#[test]
fn tcr_u06_key_value_name_and_escaped_value_hold_no_secret() {
    let token = github("aB3");
    let key = plain("memory", &format!("{{\"{token}\":\"x\"}}"));
    assert!(!holds(&key, &token));
    assert_eq!(key.form, ArgumentForm::Verbatim);
    assert_eq!(key.text, "{\"$ASURA_SECRET_1\":\"x\"}");
    assert_eq!(key.number_kinds, vec![SecretKind::GithubToken]);

    let value = plain("memory", &format!("{{\"body\": \"note {token}\"}}"));
    assert!(!holds(&value, &token));
    assert_eq!(value.form, ArgumentForm::Verbatim);
    assert_eq!(value.text, "{\"body\": \"note $ASURA_SECRET_1\"}");

    let name = plain(&format!("tool {token}"), "{}");
    assert_eq!(name.name.as_deref(), Some("tool $ASURA_SECRET_1"));
    assert!(!holds(&name, &token));

    // `\u0067` is `g`: the raw text has no detectable token, the decoded value has.
    let escaped = format!("{{\"body\":\"\\u0067{}\"}}", &token[1..]);
    let out = plain("memory", &escaped);
    assert_eq!(out.form, ArgumentForm::Canonical);
    assert!(!holds(&out, &token) && !holds(&out, &token[1..]));
    assert_eq!(out.text, "{\"body\":\"$ASURA_SECRET_1\"}");
}

/// TCR-U07: unparsed text whose escape hides a secret is not retained.
#[test]
fn tcr_u07_unparsed_escape_hiding_a_secret_is_not_retained() {
    let token = github("Qz7");
    let text = format!("{{\"body\": \"\\u0067{}\"", &token[1..]);
    let out = plain("memory", &text);
    assert_eq!(
        out.form,
        ArgumentForm::NotRetained(NotRetained::UnverifiableEscape)
    );
    assert!(out.text.is_empty());
    // Unparsed text without an escape is kept, redacted.
    let out = plain("memory", &format!("not json {token} n"));
    assert_eq!(out.form, ArgumentForm::Unparsed);
    assert_eq!(out.text, "not json $ASURA_SECRET_1 n");
    // Any match in the unescaped copy withholds the text, even one the raw
    // scan also found (design: "Any match in that copy withholds the text").
    let out = plain("memory", &format!("not json {token} \\n"));
    assert_eq!(
        out.form,
        ArgumentForm::NotRetained(NotRetained::UnverifiableEscape)
    );
    // An escape with no hidden match is kept.
    let out = plain("memory", "not json \\n \\u0041");
    assert_eq!(out.form, ArgumentForm::Unparsed);
    assert_eq!(out.text, "not json \\n \\u0041");
}

/// TCR-U08: exec_cmd stores the SL3 rewrite in `command`; numbers 1..k are
/// the variables; a refused call with a secret in cwd keeps no value.
#[test]
fn tcr_u08_exec_cmd_rewrite_and_refusal() {
    let token = github("Mn4");
    let raw = format!("curl -H \"Authorization: Bearer {token}\" x");
    let rewrite = rewrite_exec_cmd_call(&raw, ".").expect("rewrite");
    let text = crate::secrets::json::canonical(&Value::Object(vec![
        ("command".into(), Value::String(raw.clone())),
        ("cwd".into(), Value::String(".".into())),
    ]));
    let out = redact_arguments(
        rules(),
        &ArgumentsInput {
            name: Some("exec_cmd"),
            arguments: Some(&text),
            exec_cmd: Some(ExecCmdCommand::Rewritten {
                command: &rewrite.command,
                variables: &rewrite.variables,
            }),
            exec_cmd_name: true,
        },
    );
    assert_eq!(out.form, ArgumentForm::Canonical);
    let mut expected = String::from("{\"command\":");
    crate::secrets::json::write_canonical_string(&mut expected, &rewrite.command);
    expected.push_str(",\"cwd\":\".\"}");
    assert_eq!(out.text, expected);
    assert!(out.text.contains("ASURA_SECRET_1"));
    assert_eq!(
        out.number_kinds,
        rewrite
            .variables
            .iter()
            .map(|v| v.kind())
            .collect::<Vec<_>>()
    );
    assert!(!holds(&out, &token));

    let cwd = format!("dir/{token}");
    let refusal = rewrite_exec_cmd_call("ls", &cwd).unwrap_err();
    assert_eq!(refusal.code, RefusalCode::SecretInCwd);
    let text = format!("{{\"command\":\"ls\",\"cwd\":\"{cwd}\"}}");
    let out = redact_arguments(
        rules(),
        &ArgumentsInput {
            name: Some("exec_cmd"),
            arguments: Some(&text),
            exec_cmd: Some(ExecCmdCommand::Refused),
            exec_cmd_name: true,
        },
    );
    assert_eq!(
        out.text,
        "{\"command\":\"ls\",\"cwd\":\"dir/$ASURA_SECRET_1\"}"
    );
    assert!(!holds(&out, &token) && !out.command_cut);
}

/// TCR-U09: one number per value; verbatim without secrets; the 257th
/// distinct value is unnumbered.
#[test]
fn tcr_u09_numbering_verbatim_and_unnumbered_overflow() {
    let token = github("Kp2");
    let out = plain(
        "memory",
        &format!("{{\"a\":\"{token}\", \"b\":[\"x {token}\"]}}"),
    );
    assert_eq!(
        out.text,
        "{\"a\":\"$ASURA_SECRET_1\", \"b\":[\"x $ASURA_SECRET_1\"]}"
    );
    assert_eq!(out.number_kinds.len(), 1);

    let raw = "{ \"command\" : \"read_file\",\n  \"path\": \"src/main.rs\", \"limit\": 1.0e3 }";
    let out = plain("project", raw);
    assert_eq!(out.form, ArgumentForm::Verbatim);
    assert_eq!(out.text, raw);
    assert!(out.number_kinds.is_empty() && out.unnumbered == 0);

    let values: Vec<String> = (0..257).map(|i| github(&format!("{i:03}Zq"))).collect();
    let members: Vec<String> = values
        .iter()
        .enumerate()
        .map(|(i, v)| format!("\"k{i}\":\"{v}\""))
        .collect();
    let out = plain("memory", &format!("{{{}}}", members.join(",")));
    assert_eq!(out.number_kinds.len(), MAX_ARGUMENT_NUMBERS);
    assert_eq!(out.unnumbered, 1);
    assert!(out.text.ends_with("\"k256\":\"$ASURA_SECRET_0\"}"));
    assert!(values.iter().all(|v| !holds(&out, v)));
    let tokens = placeholder_tokens(&out.text);
    assert_eq!(tokens.max, 256);
    assert_eq!(tokens.distinct.len(), 256);
}

/// TCR-D19: a rule that needs the key never matches the value alone; the
/// whole-text scan finds it, and the value is replaced everywhere.
#[test]
fn tcr_d19_context_dependent_value_is_replaced() {
    let secret = "Tr0ub4dorXq9";
    let alone = plain("memory", &format!("{{\"body\":\"{secret}\"}}"));
    assert!(alone.text.contains(secret), "no context, no detection");
    let out = plain(
        "memory",
        &format!("{{\"password\": \"{secret}\", \"body\": \"again {secret}\"}}"),
    );
    assert!(!holds(&out, secret), "{}", out.text);
    assert_eq!(out.number_kinds, vec![SecretKind::AssignedSecret]);
    assert_eq!(
        out.text,
        "{\"password\": \"$ASURA_SECRET_1\", \"body\": \"again $ASURA_SECRET_1\"}"
    );
}

/// TCR-D4/D20: model-written tokens become unnumbered and are counted;
/// token rules.
#[test]
fn tcr_d4_d20_model_tokens_and_token_scanner() {
    let out = plain(
        "memory",
        "{\"body\":\"use $ASURA_SECRET_7 and ${ASURA_SECRET_12} not ASURA_SECRET_3\"}",
    );
    assert_eq!(
        out.text,
        "{\"body\":\"use $ASURA_SECRET_0 and ${ASURA_SECRET_0} not ASURA_SECRET_3\"}"
    );
    assert_eq!(out.unnumbered, 2);
    assert!(out.number_kinds.is_empty());
    let name = plain("x$ASURA_SECRET_05", "{}");
    assert_eq!(name.name.as_deref(), Some("x$ASURA_SECRET_0"));
    assert_eq!(name.unnumbered, 1);

    let t = placeholder_tokens("$ASURA_SECRET_2 ${ASURA_SECRET_1} $ASURA_SECRET_0 $ASURA_SECRET_");
    assert_eq!(
        (t.max, t.distinct.clone(), t.zero, t.malformed),
        (2, vec![1, 2], 1, false)
    );
    assert!(placeholder_tokens("$ASURA_SECRET_01").malformed);
    assert!(placeholder_tokens("$ASURA_SECRET_99999999999").malformed);
    assert_eq!(
        placeholder_tokens("ASURA_SECRET_4 {ASURA_SECRET_4}"),
        PlaceholderTokens::default()
    );
}

/// TCR-D22: a refused command over 8,192 bytes is cut without splitting a
/// placeholder; numbers stay contiguous after the cut.
#[test]
fn tcr_d22_refused_command_cut_never_splits_a_token() {
    let first = github("Wx1");
    let second = github("Yv8");
    // The first placeholder starts at byte 8,185 and so straddles 8,192.
    let pad = "a".repeat(8179);
    let command = format!("echo {pad} {first} {second}");
    let text = format!("{{\"command\":\"{command}\"}}");
    let out = redact_arguments(
        rules(),
        &ArgumentsInput {
            name: Some("exec_cmd"),
            arguments: Some(&text),
            exec_cmd: Some(ExecCmdCommand::Refused),
            exec_cmd_name: true,
        },
    );
    assert!(out.command_cut);
    let Value::Object(members) = crate::secrets::json::parse(&out.text).unwrap() else {
        panic!("object");
    };
    let Value::String(stored) = &members[0].1 else {
        panic!("string");
    };
    assert_eq!(stored, &format!("echo {pad} "));
    assert!(out.number_kinds.is_empty(), "cut numbers are dropped");
    assert!(!holds(&out, &first) && !holds(&out, &second));
}

/// TCR-D21 mechanism: a string whose scan withheld bytes is stored whole as
/// `[withheld]`. (A real withheld scan needs more than 64 KiB of key block;
/// see the handback proof limit.)
#[test]
fn tcr_d21_withheld_string_is_replaced_whole() {
    let values = Values::default();
    let redactor = Redactor {
        matcher: None,
        values: &values,
    };
    let mut numbers = Numbers::new();
    let mut emit = Emit {
        redactor: &redactor,
        withheld: vec![false, true].into_iter(),
        exec_cmd: None,
        command_cut: false,
    };
    let out = emit.value(
        &Value::Object(vec![("k".into(), Value::String("secretish".into()))]),
        &mut numbers,
        true,
    );
    assert_eq!(
        out,
        Value::Object(vec![("k".into(), Value::String(WITHHELD_TEXT.into()))])
    );
}

#[test]
fn oversized_and_absent_text_are_not_retained() {
    let big = format!("\"{}\"", "a".repeat(MAX_ARGUMENT_BYTES - 1));
    assert_eq!(
        plain("memory", &big).form,
        ArgumentForm::NotRetained(NotRetained::Oversized)
    );
    let ok = format!("\"{}\"", "a".repeat(MAX_ARGUMENT_BYTES - 2));
    assert_eq!(plain("memory", &ok).form, ArgumentForm::Verbatim);
    let absent = redact_arguments(
        rules(),
        &ArgumentsInput {
            name: None,
            arguments: None,
            exec_cmd: None,
            exec_cmd_name: false,
        },
    );
    assert_eq!(
        absent.form,
        ArgumentForm::NotRetained(NotRetained::Oversized)
    );
    assert!(absent.name.is_none() && absent.text.is_empty());
    // Expansion past 64,000 bytes after redaction is also oversized.
    let body = format!("--token Xk9qZ {}", "Xk9qZ ".repeat(10_600));
    let text = format!("{{\"body\":\"{body}\"}}");
    assert!(text.len() <= MAX_ARGUMENT_BYTES);
    let out = plain("memory", &text);
    assert_eq!(out.form, ArgumentForm::NotRetained(NotRetained::Oversized));
    assert!(out.text.is_empty() && out.number_kinds.is_empty());
    let name = plain(&"n".repeat(MAX_NAME_BYTES), "{}");
    assert_eq!(name.name.map(|n| n.len()), Some(MAX_NAME_BYTES));
}

#[test]
fn exec_cmd_name_always_stores_canonical_and_unescape_is_best_effort() {
    let out = redact_arguments(
        rules(),
        &ArgumentsInput {
            name: Some("exec_cmd"),
            arguments: Some("{ \"command\" : \"ls\" }"),
            exec_cmd: None,
            exec_cmd_name: true,
        },
    );
    assert_eq!(out.form, ArgumentForm::Canonical);
    assert_eq!(out.text, "{\"command\":\"ls\"}");
    assert_eq!(unescape("a\\u0041\\ud83d\\ude00\\q\\"), "aA😀\\q\\");
}
