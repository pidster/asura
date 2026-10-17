//! Unit tests SDU-01–SDU-12. Fixture secrets are assembled at run time so no
//! token-shaped literal is committed.
use super::rewrite::{Refusal, RefusalCode, display_redacted, rewrite};
use super::rules::{Candidate, merge};
use super::stream::{is_body_line, scan_spans};
use super::*;

fn rules() -> &'static RuleSet {
    detection().expect("rule set self-check")
}
fn cat(parts: &[&str]) -> String {
    parts.concat()
}
fn kinds(text: &str) -> Vec<&'static str> {
    scan_spans(rules(), text.as_bytes())
        .0
        .iter()
        .map(|s| s.kind.as_str())
        .collect()
}
fn values(text: &str) -> Vec<String> {
    scan_spans(rules(), text.as_bytes())
        .0
        .iter()
        .map(|s| text[s.start as usize..s.end as usize].to_string())
        .collect()
}
fn run(job: &mut JobRedaction, stream: Stream, chunks: &[&[u8]]) -> Vec<u8> {
    let mut out = Vec::new();
    for chunk in chunks {
        out.extend(job.scan(stream, chunk).into_bytes());
    }
    out.extend(job.finish(stream).into_bytes());
    out
}
fn redact(chunks: &[&[u8]]) -> (Vec<u8>, StreamStats) {
    let mut job = JobRedaction::new(rules(), &[]);
    let out = run(&mut job, Stream::Stdout, chunks);
    (out, job.stats(Stream::Stdout).clone())
}
fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}
fn begin(label: &str) -> String {
    cat(&["-----BEGIN ", label, "PRIVATE", " KEY-----"])
}
fn end(label: &str) -> String {
    cat(&["-----END ", label, "PRIVATE", " KEY-----"])
}
fn body_line(seed: &str) -> String {
    cat(&["MIIE", "owIBAAKCAQEA", &seed.repeat(13)])
}

fn github() -> String {
    cat(&["ghp_", &"aB3".repeat(12)])
}
fn aws() -> String {
    cat(&["AKIA", "IOSFODNN7", "QWERTYU"])
}
fn stripe() -> String {
    cat(&["sk_", "live_", &"x1Y2".repeat(6)])
}
fn anthropic() -> String {
    cat(&["sk-", "ant-", &"Zq9_".repeat(8)])
}

#[test]
fn sdu_01_every_expression_compiles_and_the_digest_is_stable() {
    let set = RuleSet::compile().expect("self-check");
    assert_eq!(set.rule_count(), 47);
    assert_eq!(set.expression_count(), 46);
    assert_eq!(set.revision(), RULE_SET_REVISION);
    assert_eq!(set.digest_hex(), RULE_SET_DIGEST);
    assert_eq!(RuleSet::compile().unwrap().digest(), set.digest());
    assert_eq!(rules().digest_hex(), RULE_SET_DIGEST);
    for rule in &set.rules {
        regex::bytes::RegexBuilder::new(&rule.pattern)
            .unicode(false)
            .size_limit(rules::EXPRESSION_SIZE_LIMIT)
            .dfa_size_limit(rules::DFA_CACHE_LIMIT)
            .build()
            .unwrap_or_else(|e| panic!("{}: {e}", rule.id));
    }
    let ids: Vec<_> = set.rules.iter().map(|r| r.id).collect();
    let expected: Vec<_> = (1..=47).map(|n| format!("SR-{n:02}")).collect();
    assert_eq!(ids, expected);
    assert!(rewrite::rewrite_command("true").is_ok());
}

/// (line, Wisp kinds). Each expectation was checked against Wisp's
/// `SecretScanner.scan(_, categories: [.secret])` at commit 4305d2c.
fn rule_vectors() -> Vec<(String, Vec<&'static str>)> {
    let hash = cat(&["$2b$12$", &"Ab9.".repeat(13), "x"]);
    let hex = "8c1f4a7e".repeat(4);
    let mut v: Vec<(String, Vec<&'static str>)> = vec![
        // SecretScannerTests.credentialsNamedByTheirHeaderFlagOrKeyAreFound
        (
            cat(&["Authorization: Basic ", "ZGVwbG95OmNvcm4tZmllbGQtOQ=="]),
            vec!["auth-header"],
        ),
        (
            cat(&[
                "curl -H \"Authorization: Bearer ",
                &"Fq7Lm2".repeat(4),
                "\"",
            ]),
            vec!["auth-header"],
        ),
        (
            cat(&["Cookie: PHPSESSID=", "r4v9t3b8y2c6h1d5"]),
            vec!["session-cookie"],
        ),
        (cat(&["db_pass: ", "kX9mQ2vL"]), vec!["assigned-secret"]),
        (
            cat(&["spring.datasource.password=", "Orion#2026"]),
            vec!["assigned-secret"],
        ),
        (
            cat(&["X-Api-Key: ", "live_4569a774f8a6"]),
            vec!["assigned-secret"],
        ),
        (
            cat(&["mysql -u root -p", "S3cretRoot shop"]),
            vec!["cli-password"],
        ),
        (
            cat(&["curl -u admin:", "Sup3rS3cret https://ci.example.com/api"]),
            vec!["cli-password"],
        ),
        (
            cat(&["docker login -u ci -p ", "Pat-7731-lark ghcr.example.com"]),
            vec!["cli-password"],
        ),
        (
            cat(&["password_hash = \"", &hash, "\""]),
            vec!["password-hash"],
        ),
        (
            cat(&[
                "https://acct.blob.core.windows.net/q3.csv?sv=2024&sig=",
                "CXXBK56qXT7zZfWZbmMm",
            ]),
            vec!["url-secret"],
        ),
        (
            begin("PGP ").replace(" KEY-----", " KEY BLOCK-----"),
            vec!["private-key"],
        ),
        // assignmentsNeedARealLookingValue
        (
            cat(&["password = \"", "Tr0ub4dor", "&3x\""]),
            vec!["assigned-secret"],
        ),
        (
            cat(&["export GITHUB_TOKEN=", "abc123def456ghi"]),
            vec!["env-secret"],
        ),
        // aURLPasswordIsASecretNotAnEmailAddress
        (
            cat(&["mongodb://backup:", "dumpIt88@mongo.example.com:27017"]),
            vec!["url-password"],
        ),
        (
            cat(&["redis://:", "hush-hush@cache.example.com:6379"]),
            vec!["url-password"],
        ),
        // overlappingMatchesKeepTheFirstAndLongest
        (anthropic(), vec!["anthropic-key"]),
        // aCredentialWordInASentenceIsNotAKeyUnlessItsValueReadsAsACredential
        (cat(&["token: ", &github()]), vec!["github-token"]),
        (
            cat(&["API_KEY=", "sk-", &"Ab3d".repeat(6)]),
            vec!["openai-key"],
        ),
        (
            cat(&["password: \"", "Tr0ub4dor", "&3x\""]),
            vec!["assigned-secret"],
        ),
        (
            cat(&["aws_secret_access_key = ", &"Tq4nVb8Rw2".repeat(4)]),
            vec!["aws-secret-key"],
        ),
        (
            cat(&["{\"api_key\": \"", "Zs4Qm8Vx2Rw7Nb1K", "\"}"]),
            vec!["assigned-secret"],
        ),
        (
            cat(&["  password: ", "UyFCXCpkJHpE"]),
            vec!["assigned-secret"],
        ),
        (cat(&["admin_password: ", "admin"]), vec!["assigned-secret"]),
        (
            cat(&["data: { token: ", "ZXhhbXBsZS1zZXJ2aWNl", " }"]),
            vec!["assigned-secret"],
        ),
        (
            cat(&["account=deploy service=github password=", "Maple&Rye5"]),
            vec!["assigned-secret"],
        ),
        (
            cat(&["please use the following password: ", "wdpfe*Lv0%R"]),
            vec!["assigned-secret"],
        ),
        (
            cat(&["VPN shared secret: ", "nightjar-quay-19"]),
            vec!["assigned-secret"],
        ),
        (cat(&["jupyter token: ", &hex]), vec!["assigned-secret"]),
        // One positive per remaining rule.
        (
            cat(&["MII", "EowIBAAK", &"CAQEA7v".repeat(4)]),
            vec!["private-key"],
        ),
        (cat(&["key id ", &aws()]), vec!["aws-access-key"]),
        (github(), vec!["github-token"]),
        (
            cat(&["gl", "pat-", "a1B2c3D4e5F6g7H8i9J0"]),
            vec!["gitlab-token"],
        ),
        (
            cat(&["py", "pi-AgE", &"Ab12".repeat(5)]),
            vec!["pypi-token"],
        ),
        (
            cat(&["dckr", "_pat_", &"Ab12".repeat(6)]),
            vec!["docker-token"],
        ),
        (cat(&["npm", "_", &"Ab12".repeat(8)]), vec!["npm-token"]),
        (
            cat(&["AT", "BB", &"Ab12".repeat(7)]),
            vec!["bitbucket-token"],
        ),
        (cat(&["hvs", ".", &"Ab12".repeat(7)]), vec!["vault-token"]),
        (
            cat(&["FlyV1 ", "fm2_", &"Ab12".repeat(6)]),
            vec!["fly-token"],
        ),
        (
            cat(&["wh", "sec_", &"Ab12".repeat(7)]),
            vec!["webhook-secret"],
        ),
        (
            cat(&["key", "-", &"0123456789abcdef".repeat(2)]),
            vec!["mailgun-key"],
        ),
        (
            cat(&["GOC", "SPX-", &"Ab12".repeat(6)]),
            vec!["google-oauth"],
        ),
        (
            cat(&[
                "DefaultEndpointsProtocol=https;AccountKey=",
                &"Ab12".repeat(11),
            ]),
            vec!["azure-key"],
        ),
        (
            cat(&["xo", "xb-", "1234567890", "-abcdef"]),
            vec!["slack-token"],
        ),
        (
            cat(&[
                "https://hooks.slack.com/services/",
                "T000/B000/",
                &"Ab12".repeat(6),
            ]),
            vec!["slack-webhook"],
        ),
        (
            cat(&[
                "https://discord.com/api/webhooks/",
                "123456/",
                &"Ab12".repeat(6),
            ]),
            vec!["chat-webhook"],
        ),
        (stripe(), vec!["stripe-key"]),
        (
            cat(&["sk-", "proj-", &"Ab12".repeat(6)]),
            vec!["openai-key"],
        ),
        (
            cat(&["AI", "za", "Sy", &"A1b2C3d4E5".repeat(3), "f6g"]),
            vec!["google-api-key"],
        ),
        (
            cat(&[
                "ey",
                "JhbGciOiJIUz",
                ".ey",
                "JzdWIiOiIxMjM",
                ".",
                "SflKxwRJSMeK",
            ]),
            vec!["jwt"],
        ),
        (
            cat(&["the header was Bearer ", &"Fq7Lm2".repeat(4)]),
            vec!["auth-header"],
        ),
        (
            cat(&["tool --api-key=", "Zq9xW2vB7"]),
            vec!["assigned-secret"],
        ),
        (cat(&["tool --token ", "Zq9xW2vB7"]), vec!["cli-password"]),
        (
            cat(&["redis-cli -h host -a ", "S3cr3tPw9"]),
            vec!["cli-password"],
        ),
        (
            cat(&["htpasswd -b /etc/htpasswd admin ", "Pa55w0rdX"]),
            vec!["cli-password"],
        ),
        (
            cat(&["echo ", "Hunter2Pw", " | sudo -S apt update"]),
            vec!["cli-password"],
        ),
        (
            cat(&["sudo -S <<< ", "Hunter2Pw", " true"]),
            vec!["cli-password"],
        ),
        (
            cat(&["aws configure set aws_secret_access_key ", "Tq4nVb8Rw2x"]),
            vec!["cli-password"],
        ),
        (
            cat(&["machine api.example.com login deploy password ", "Pw9xYz12"]),
            vec!["netrc-password"],
        ),
        (
            cat(&["WIFI:S:home;T:WPA;P:", "Pw9xYz12", ";;"]),
            vec!["wifi-password"],
        ),
    ];
    // lookAlikesOfNamedCredentialsAreLeftAlone and the prose cases.
    for line in [
        r#"print("\(ok ? "pass" : "FAIL")")"#,
        "--- PASS: TestGo (0.00s)",
        "the model's pass: names and numbers",
        "echo ${TOKEN:+yes}",
        "token_count = 42",
        "password: String",
        "let secret = await store.read()",
        r#"let token = "ghp_" + String(repeating: "a", count: 36)"#,
        "password = os.environ.get(\"DB\")",
        "arn:aws:secretsmanager:us-east-1:000000000000:secret:prod/db-AbCdEf",
        "PUBLIC_KEY_PATH=/etc/wisp/pub.pem",
        r#"export OPENAI_API_KEY="sk-...""#,
        "-e POSTGRES_PASSWORD=postgres",
        "token: ghp_****abcd",
        "docker run -p 5432:5432 postgres:16",
        "git push --token-file ~/.token",
        r#"mock_api_key = "test-key""#,
        "only its attention layers per token: every `full_attention_interval`th layer",
        "kept once and once per drafted token: 748.125 MiB, as measured at both windows",
        "Tokens come to at four bytes a token: their text, and each call's name",
        "load: control-looking token: 128247 '</s>' was not control-type",
        "the cache costs per token: 32KiB",
        "per request: about 2 s",
        "the token: a word",
        "key: value pairs are read as such",
        "a shared secret: nothing more than a word",
        "the password:  everything after it",
        r#"password = "changeme123""#,
        r#"api_key: "${API_KEY}""#,
        r#"token = "xxxxxxxxxxxx""#,
        r#"secret = "<your-secret-here>""#,
        "DB_PASSWORD=$DB_PASSWORD",
    ] {
        v.push((line.to_string(), vec![]));
    }
    v
}

#[test]
fn sdu_02_per_rule_positives_and_negatives_match_wisp() {
    for (line, expected) in rule_vectors() {
        assert_eq!(kinds(&line), expected, "{line}");
    }
    // findsKnownCredentialShapesAndMasksThem, scanned as a stream of lines.
    let text = [
        cat(&["token: ", &github()]),
        cat(&["aws_access_key_id = ", &aws()]),
        cat(&["STRIPE=", &stripe()]),
        cat(&["let key = \"", &anthropic(), "\""]),
        cat(&[
            "postgres://app:",
            "hunter2",
            "-prod",
            "@db.internal:5432/app",
        ]),
        begin("RSA "),
        "MIIEowIBAAKCAQEA".to_string(),
        end("RSA "),
    ]
    .join("\n");
    assert_eq!(
        kinds(&text),
        [
            "github-token",
            "aws-access-key",
            "stripe-key",
            "anthropic-key",
            "url-password",
            "private-key"
        ]
    );
    let found = values(&text);
    assert_eq!(found[4], cat(&["hunter2", "-prod"]));
    assert!(found[5].ends_with(&end("RSA ")) && found[5].starts_with(&begin("RSA ")));
    assert_eq!(
        values(&cat(&[
            "mongodb://backup:",
            "dumpIt88@mongo.example.com:27017"
        ])),
        ["dumpIt88"]
    );
    let r = rules();
    assert!(r.reads_as_credential(b"nightjar-quay-19") && r.reads_as_credential(b"3fea6af1"));
    for value in ["every", "748.125", "32KiB", "2ms", "50%", "128247", "admin"] {
        assert!(!r.reads_as_credential(value.as_bytes()), "{value}");
    }
    for value in [
        "$ASURA_SECRET_1",
        "aaaa",
        "abab",
        "my-dummy-key",
        "Changeme1",
        "root",
        "<x>",
        "{{ s }}",
        "%TOKEN%",
        "a\u{2026}b",
    ] {
        assert!(rules::is_placeholder(value.as_bytes()), "{value}");
    }
    assert!(!rules::is_placeholder(b"Tq7-harbour-2"));
    for value in [
        "String",
        "?value!",
        "os.environ",
        "config/app.yaml",
        "-x",
        "~/k",
        "await",
    ] {
        assert!(r.is_code(value.as_bytes()), "{value}");
    }
    assert!(!r.is_code(b"kX9mQ2vL"));
}

/// Construct rewrites L1, A1, P1 and L2 at their boundaries; each expectation
/// was checked against the Swift expression.
#[test]
fn sdu_03_construct_rewrite_boundaries() {
    let pad = |n: usize| " ".repeat(n);
    let cases: Vec<(String, Vec<&str>)> = vec![
        // L1: line start, after `:`, after a letter, after punctuation.
        ("token: abcdefgh".into(), vec!["assigned-secret"]),
        ("x:token: abcdefgh".into(), vec![]),
        ("xtoken: abcdefgh".into(), vec![]),
        ("{token: abcdefgh}".into(), vec!["assigned-secret"]),
        ("a.db_pass= abcdefgh".into(), vec!["assigned-secret"]),
        // L2: 16 blanks after a letter is sentence position (needs letters and
        // digits); 17 blanks is key position; a tab counts as a blank.
        (cat(&["a", &pad(16), "token: abcdefgh"]), vec![]),
        (
            cat(&["a", &pad(16), "token: abcd1234"]),
            vec!["assigned-secret"],
        ),
        (
            cat(&["a", &pad(17), "token: abcdefgh"]),
            vec!["assigned-secret"],
        ),
        ("a\ttoken: abcdefgh".into(), vec![]),
        (", token: abcdefgh".into(), vec!["assigned-secret"]),
        // P1: a call or subscript is not a value.
        ("token = attach(x)".into(), vec![]),
        ("token = keys[0]".into(), vec![]),
        ("token = abcdefgh".into(), vec!["assigned-secret"]),
        ("token: abcdefgh(".into(), vec![]),
        // A1: a quoted value continued by `+` or `[` is code.
        ("token = \"abcd\" + rest".into(), vec![]),
        ("token = \"abcd\"[0]".into(), vec![]),
        ("token = \"abcd\" ;".into(), vec!["assigned-secret"]),
        ("token = \"abcd\"".into(), vec!["assigned-secret"]),
    ];
    for (line, expected) in cases {
        assert_eq!(kinds(&line), expected, "{line:?}");
    }
    assert!(rules::sentence_position(b"a token", 2));
    assert!(!rules::sentence_position(b"token", 0));
}

#[test]
fn sdu_04_overlap_merge_keeps_no_candidate_byte() {
    use SecretKind::*;
    let rule = |s, e, order, kind| Candidate {
        start: s,
        end: e,
        kind,
        order,
        literal: None,
    };
    // Nested: the outer span survives with its kind.
    let m = merge(vec![rule(5, 8, 3, Jwt), rule(2, 10, 9, CliPassword)]);
    assert_eq!(
        (m.len(), m[0].start, m[0].end, m[0].kind),
        (1, 2, 10, CliPassword)
    );
    // Partial: union, earlier candidate's kind.
    let m = merge(vec![
        rule(6, 12, 1, PrivateKey),
        rule(2, 8, 30, AssignedSecret),
    ]);
    assert_eq!(
        (m.len(), m[0].start, m[0].end, m[0].kind),
        (1, 2, 12, AssignedSecret)
    );
    // Adjacent spans stay separate.
    assert_eq!(merge(vec![rule(0, 4, 1, Jwt), rule(4, 8, 2, Jwt)]).len(), 2);
    // Identical: table order picks the kind; a literal of the same range keeps its number.
    let m = merge(vec![
        rule(0, 4, 22, OpenaiKey),
        rule(0, 4, 21, AnthropicKey),
    ]);
    assert_eq!(m[0].kind, AnthropicKey);
    let m = merge(vec![
        rule(0, 4, 22, OpenaiKey),
        Candidate::literal(0, 4, CliPassword, 3),
    ]);
    assert_eq!(
        (m[0].kind, m[0].literal_number(), m[0].from_rule),
        (CliPassword, Some(3), true)
    );
    // A literal extended by a rule is no longer the literal's value.
    let m = merge(vec![
        rule(0, 9, 22, OpenaiKey),
        Candidate::literal(2, 6, CliPassword, 3),
    ]);
    assert_eq!((m[0].start, m[0].end, m[0].literal_number()), (0, 9, None));
    // Wisp kept the overlapping loser's bytes; the union replaces them.
    let line = cat(&[
        "postgres://u:",
        "p4ssw0rd-x",
        "@h/db?password=",
        "p4ssw0rd-xyz",
    ]);
    let (out, _) = redact(&[line.as_bytes()]);
    assert_eq!(
        text(&out),
        "postgres://u:$ASURA_SECRET_1@h/db?password=$ASURA_SECRET_2"
    );
}

fn mixed_stream() -> Vec<u8> {
    let mut s = Vec::new();
    for line in [
        cat(&["token: ", &github(), "\r\n"]),
        cat(&["aws_access_key_id = ", &aws(), "\n"]),
        "progress 10%\rprogress 20%\r".to_string(),
        cat(&[
            &begin("RSA "),
            "\n",
            &body_line("q9Zx"),
            "\n",
            &body_line("Lm2Q"),
            "\n",
            &end("RSA "),
            "\n",
        ]),
        cat(&["STRIPE=", &stripe(), " again ", &stripe(), "\n"]),
        cat(&["mysql -u root -p", "S3cretRoot", " shop\n"]),
        cat(&[
            &begin(""),
            "\n",
            &body_line("Ab12"),
            "\n",
            "prose follows\n",
        ]),
    ] {
        s.extend_from_slice(line.as_bytes());
    }
    s.extend_from_slice(&[0xff, 0xfe, b'\n', 0x80]);
    s.extend_from_slice(cat(&["x ", &stripe()]).as_bytes());
    s
}

#[test]
fn sdu_05_chunk_split_at_every_offset_matches_one_chunk() {
    let stream = mixed_stream();
    let (whole, stats) = redact(&[&stream]);
    assert_eq!(stats.replacements, 8, "{}", text(&whole));
    for split in 0..=stream.len() {
        let (out, s) = redact(&[&stream[..split], &stream[split..]]);
        assert_eq!(out, whole, "split at {split}");
        assert_eq!(s, stats, "split at {split}");
    }
    let mut seed = 7u64;
    for _ in 0..400 {
        seed = seed
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let a = (seed >> 33) as usize % (stream.len() + 1);
        let b = a + (seed >> 13) as usize % (stream.len() + 1 - a);
        let (out, _) = redact(&[&stream[..a], &stream[a..b], &stream[b..]]);
        assert_eq!(out, whole, "split at {a} and {b}");
    }
    // Byte at a time.
    let singles: Vec<&[u8]> = stream.chunks(1).collect();
    assert_eq!(redact(&singles).0, whole);
}

#[test]
fn sdu_06_private_key_blocks() {
    let block = cat(&[
        &begin("RSA "),
        "\n",
        &body_line("q9Zx"),
        "\n",
        &end("RSA "),
        "\n",
    ]);
    // BEGIN, body and END in separate chunks: one placeholder.
    let parts: Vec<&str> = block.split_inclusive('\n').collect();
    let chunks: Vec<&[u8]> = parts.iter().map(|p| p.as_bytes()).collect();
    let (out, stats) = redact(&chunks);
    assert_eq!(text(&out), "$ASURA_SECRET_1\n");
    assert_eq!(stats.kind_count(SecretKind::PrivateKey), 1);
    // CRLF line ends.
    let crlf = block.replace('\n', "\r\n");
    assert_eq!(text(&redact(&[crlf.as_bytes()]).0), "$ASURA_SECRET_1\r\n");
    // An escaped one-line JSON key matches within the line.
    let json = cat(&[
        "{\"private_key\":\"",
        &begin(""),
        "\\n",
        &body_line("Ab12"),
        "\\n",
        &end(""),
        "\\n\"}\n",
    ]);
    // The quoted assigned-secret value merges with the block, `\n` included.
    assert_eq!(
        text(&redact(&[json.as_bytes()]).0),
        "{\"private_key\":\"$ASURA_SECRET_1\"}\n"
    );
    let bare = cat(&[
        "k ",
        &begin(""),
        "\\n",
        &body_line("Ab12"),
        "\\n",
        &end(""),
        "\\n tail\n",
    ]);
    assert_eq!(
        text(&redact(&[bare.as_bytes()]).0),
        "k $ASURA_SECRET_1\\n tail\n"
    );
    // Diff-prefixed lines.
    let diff = cat(&[
        "+",
        &begin("OPENSSH "),
        "\n+",
        &body_line("q9Zx"),
        "\n+",
        &end("OPENSSH "),
        "\n context\n",
    ]);
    assert_eq!(
        text(&redact(&[diff.as_bytes()]).0),
        "+$ASURA_SECRET_1\n context\n"
    );
    // Unterminated at EOF: replaced whole, BEGIN to last body line.
    let open = cat(&["key:\n", &begin("EC "), "\n", &body_line("q9Zx"), "\n"]);
    assert_eq!(
        text(&redact(&[open.as_bytes()]).0),
        "key:\n$ASURA_SECRET_1\n"
    );
    let open = cat(&[&begin("EC "), "\n", &body_line("q9Zx")]);
    assert_eq!(text(&redact(&[open.as_bytes()]).0), "$ASURA_SECRET_1");
    // Unterminated before prose: block replaced, prose scanned normally.
    let prose = cat(&[
        &begin(""),
        "\n",
        &body_line("q9Zx"),
        "\n\nthe build failed\n",
    ]);
    // An empty line is a body line, so the blank line belongs to the block.
    assert_eq!(
        text(&redact(&[prose.as_bytes()]).0),
        "$ASURA_SECRET_1\nthe build failed\n"
    );
    // A BEGIN after text releases the prefix; a mismatched END does not close.
    let mismatch = cat(&[
        "note ",
        &begin("RSA "),
        "\n",
        &body_line("q9Zx"),
        "\n",
        &end("EC "),
        "\nok\n",
    ]);
    assert_eq!(
        text(&redact(&[mismatch.as_bytes()]).0),
        "note $ASURA_SECRET_1\n-----END EC PRIVATE KEY-----\nok\n"
    );
    // Headers are body lines; the same block twice has one number.
    let headed = cat(&[
        &begin("RSA "),
        "\nProc-Type: 4,ENCRYPTED\nDEK-Info: AES-128-CBC,00FF\n\n",
        &body_line("q9Zx"),
        "\n",
        &end("RSA "),
        "\n",
    ]);
    let twice = cat(&[&headed, &headed]);
    assert_eq!(
        text(&redact(&[twice.as_bytes()]).0),
        "$ASURA_SECRET_1\n$ASURA_SECRET_1\n"
    );
    for line in [
        "",
        "\n",
        "  +AbC=\n",
        "Proc-Type: 4,ENCRYPTED\n",
        "\"MIIE/x+=\",\n",
    ] {
        assert!(is_body_line(line.as_bytes()), "{line:?}");
    }
    for line in ["the build failed\n", "1:x\n", &"A".repeat(129)] {
        assert!(!is_body_line(line.as_bytes()), "{line:?}");
    }
}

fn marker(bytes: u64, reason: &str) -> String {
    format!("[ASURA_WITHHELD {bytes} bytes: {reason}]")
}

#[test]
fn sdu_07_long_lines_release_to_the_last_delimiter() {
    let limit = CARRY_LIMIT;
    // 64 KiB − 1 content bytes and a terminator: an ordinary line.
    let line = cat(&[&"a".repeat(limit - 1), "\n"]);
    let (out, stats) = redact(&[line.as_bytes()]);
    assert_eq!(out, line.as_bytes());
    assert_eq!(stats.loss_span_count, 0);
    // Exactly 64 KiB of content: scanned and released up to the last
    // delimiter; nothing after it remains, so no marker.
    let line = cat(&[&"a".repeat(limit - 1), " ", "\n"]);
    let (out, stats) = redact(&[line.as_bytes()]);
    assert_eq!(out, line.as_bytes());
    assert_eq!(stats.withheld_bytes, 0);
    // 64 KiB + 1: the word after the last space is withheld to the terminator.
    let line = cat(&["head ", &"b".repeat(limit - 4), "\nnext\n"]);
    let (out, stats) = redact(&[line.as_bytes()]);
    assert_eq!(
        text(&out),
        cat(&[
            "head ",
            &marker((limit - 4) as u64, "line_over_limit"),
            "\nnext\n"
        ])
    );
    assert_eq!(stats.loss_spans[0].stored_offset, 5);
    assert_eq!(stats.loss_spans[0].withheld_bytes, (limit - 4) as u64);
    // No delimiter at all: everything withheld.
    let line = "c".repeat(limit + 10);
    let (out, stats) = redact(&[line.as_bytes()]);
    assert_eq!(text(&out), marker((limit + 10) as u64, "line_over_limit"));
    assert_eq!(stats.stored_bytes, out.len() as u64);
    // A token straddling the cut is withheld whole, never partly stored.
    let token = github();
    let mut line = "d ".repeat((limit - 10) / 2);
    line.push_str(&token);
    line.push_str(" tail\n");
    let (out, _) = redact(&[line.as_bytes()]);
    let out = text(&out);
    assert!(!out.contains(&token[..10]), "token prefix stored");
    assert!(out.ends_with(&cat(&[
        &marker(
            (line.len() - 1 - line.find(&token).unwrap()) as u64,
            "line_over_limit"
        ),
        "\n"
    ])));
    // A secret before the cut is replaced; the rest of the line withheld.
    let mut line = cat(&["STRIPE=", &stripe(), " "]);
    line.push_str(&"e".repeat(limit));
    let (out, _) = redact(&[line.as_bytes()]);
    assert_eq!(
        text(&out),
        cat(&[
            "STRIPE=$ASURA_SECRET_1 ",
            &marker(limit as u64, "line_over_limit")
        ])
    );
}

#[test]
fn sdu_08_key_block_over_the_limit() {
    let line = cat(&[&body_line("q9Zx"), "\n"]);
    let mut block = cat(&[&begin("RSA "), "\n"]);
    while block.len() < CARRY_LIMIT + 1000 {
        block.push_str(&line);
    }
    let total = block.len() + end("RSA ").len();
    let input = cat(&[&block, &end("RSA "), "\nafter ", &github(), "\n"]);
    let (out, stats) = redact(&[input.as_bytes()]);
    assert_eq!(
        text(&out),
        cat(&[
            &marker(total as u64, "key_block_over_limit"),
            "\nafter $ASURA_SECRET_1\n"
        ])
    );
    assert_eq!(stats.loss_spans[0].reason, LossReason::KeyBlockOverLimit);
    // Closed by a non-body line: the last body line's terminator is stored.
    let input = cat(&[&block, "the end\n"]);
    let (out, _) = redact(&[input.as_bytes()]);
    assert_eq!(
        text(&out),
        cat(&[
            &marker(block.len() as u64 - 1, "key_block_over_limit"),
            "\nthe end\n"
        ])
    );
    // Same in one-byte chunks.
    let singles: Vec<&[u8]> = input.as_bytes().chunks(1).collect();
    assert_eq!(
        text(&redact(&singles).0),
        cat(&[
            &marker(block.len() as u64 - 1, "key_block_over_limit"),
            "\nthe end\n"
        ])
    );
    // An over-long line inside a block is withheld with it.
    let input = cat(&[
        &begin(""),
        "\n",
        &"Q".repeat(CARRY_LIMIT),
        "\n",
        &line,
        "all done.\n",
    ]);
    let (out, _) = redact(&[input.as_bytes()]);
    let withheld = input.len() - "all done.\n".len() - 1;
    assert_eq!(
        text(&out),
        cat(&[
            &marker(withheld as u64, "key_block_over_limit"),
            "\nall done.\n"
        ])
    );
    // Stream end inside the overflow closes the marker.
    let (out, _) = redact(&[block.as_bytes()]);
    assert_eq!(
        text(&out),
        cat(&[
            &marker(block.len() as u64 - 1, "key_block_over_limit"),
            "\n"
        ])
    );
}

#[test]
fn sdu_09_binary_and_invalid_utf8_are_kept() {
    let mut input = vec![0x00, 0xff, 0xc3, b' '];
    input.extend_from_slice(github().as_bytes());
    input.extend_from_slice(&[b' ', 0x80, 0xfe, b'\n', 0x1b, b'[', b'0', b'm']);
    input.extend_from_slice(cat(&[" password=\u{e9}", "Tq7-harbour-2", "\u{e9}"]).as_bytes());
    let (out, stats) = redact(&[&input]);
    let mut expected = vec![0x00, 0xff, 0xc3, b' '];
    expected.extend_from_slice(b"$ASURA_SECRET_1");
    expected.extend_from_slice(&[b' ', 0x80, 0xfe, b'\n', 0x1b, b'[', b'0', b'm']);
    expected.extend_from_slice(b" password=$ASURA_SECRET_2");
    assert_eq!(out, expected);
    assert_eq!(stats.observed_bytes, input.len() as u64);
    assert_eq!(stats.stored_bytes, expected.len() as u64);
    // Pseudo-random binary with no secret passes through unchanged.
    let mut seed = 0x2545_f491_4f6c_dd1du64;
    let noise: Vec<u8> = (0..200_000)
        .map(|_| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            if seed.is_multiple_of(512) {
                b'\n'
            } else {
                (seed as u8) | 0x80
            }
        })
        .collect();
    assert_eq!(redact(&[&noise]).0.len(), noise.len());
}

#[test]
fn sdu_10_numbering_is_per_job_and_stable() {
    let command = rewrite(rules(), &cat(&["mysql -u root -p", "S3cretRoot", " shop"])).unwrap();
    assert_eq!(command.variables.len(), 1);
    let mut job = JobRedaction::new(rules(), &command.variables);
    let a = run(
        &mut job,
        Stream::Stdout,
        &[cat(&["echo S3cret", "Root\nSTRIPE=", &stripe(), "\n"]).as_bytes()],
    );
    let b = run(
        &mut job,
        Stream::Stderr,
        &[cat(&["dup ", &stripe(), " and ", "S3cretRoot", "\n"]).as_bytes()],
    );
    assert_eq!(text(&a), "echo $ASURA_SECRET_1\nSTRIPE=$ASURA_SECRET_2\n");
    assert_eq!(text(&b), "dup $ASURA_SECRET_2 and $ASURA_SECRET_1\n");
    assert_eq!(job.stats(Stream::Stdout).literal_replacements, 1);
    // A propagated value is replaced bare, without the context its rule needed.
    let mut job = JobRedaction::new(rules(), &[]);
    let value = cat(&["Tq7-", "harbour-2"]);
    let out = run(
        &mut job,
        Stream::Stdout,
        &[cat(&["password=", &value, "\nlater ", &value, "\n"]).as_bytes()],
    );
    assert_eq!(
        text(&out),
        "password=$ASURA_SECRET_1\nlater $ASURA_SECRET_1\n"
    );
    // Bytes released before the value was first found are not changed.
    let mut job = JobRedaction::new(rules(), &[]);
    let out = run(
        &mut job,
        Stream::Stdout,
        &[cat(&["early ", &value, "\npassword=", &value, "\n"]).as_bytes()],
    );
    assert_eq!(
        text(&out),
        cat(&["early ", &value, "\npassword=$ASURA_SECRET_1\n"])
    );
    // Past 16,384 numbers, new values are `$ASURA_SECRET_0`; known values keep theirs.
    let mut job = JobRedaction::new(rules(), &[]);
    let mut input = String::new();
    for n in 0..=MAX_NUMBERS {
        input.push_str(&format!("password=Pw{n:06}x\n"));
    }
    input.push_str("password=Pw000000x\n");
    let out = text(&run(&mut job, Stream::Stdout, &[input.as_bytes()]));
    let lines: Vec<_> = out.lines().collect();
    assert_eq!(lines[0], "password=$ASURA_SECRET_1");
    assert_eq!(
        lines[MAX_NUMBERS as usize - 1],
        "password=$ASURA_SECRET_16384"
    );
    assert_eq!(lines[MAX_NUMBERS as usize], "password=$ASURA_SECRET_0");
    assert_eq!(lines[MAX_NUMBERS as usize + 1], "password=$ASURA_SECRET_1");
    assert_eq!(job.stats(Stream::Stdout).unnumbered_replacements, 1);
    let (summary, _) = job.settle();
    assert_eq!(summary.distinct_numbers, MAX_NUMBERS);
    assert_eq!(summary.numbers.len(), MAX_SUMMARY_NUMBERS);
    // Numbers are independent per job.
    let mut other = JobRedaction::new(rules(), &[]);
    let out = run(&mut other, Stream::Stdout, &[b"password=Pw016000x\n"]);
    assert_eq!(text(&out), "password=$ASURA_SECRET_1\n");
    // A multi-line literal becomes one placeholder per line piece.
    let block = cat(&[&begin(""), "\n", &body_line("q9Zx"), "\n", &end(""), "\n"]);
    let mut job = JobRedaction::new(rules(), &[]);
    let out = run(
        &mut job,
        Stream::Stdout,
        &[cat(&[&block, "copy: ", &body_line("q9Zx"), "\n"]).as_bytes()],
    );
    assert_eq!(text(&out), "$ASURA_SECRET_1\ncopy: $ASURA_SECRET_1\n");
}

#[test]
fn detector_error_and_flush_fail_closed() {
    let mut job = JobRedaction::new(rules(), &[]);
    let _ = job.scan(Stream::Stdout, b"partial ");
    let out = job.detector_error(Stream::Stdout, 100);
    assert_eq!(text(out.as_bytes()), marker(108, "detector_error"));
    assert!(job.stats(Stream::Stdout).degraded);
    let out = job.scan(Stream::Stdout, b"fine\n");
    assert_eq!(out.as_bytes(), b"fine\n");
    let _ = job.finish(Stream::Stdout);
    let out = job.scan(Stream::Stdout, b"late\n");
    assert_eq!(text(out.as_bytes()), marker(5, "detector_error"));
    let (summary, tails) = job.settle();
    assert!(tails[0].is_empty() && tails[1].is_empty());
    assert_eq!(summary.stdout.loss_span_count, 2);
    assert!(format!("{summary:?}").contains("loss_spans"));
}

fn values_of(r: &rewrite::Rewrite) -> Vec<(u8, String)> {
    r.variables
        .iter()
        .map(|v| (v.number(), String::from_utf8(v.value().to_vec()).unwrap()))
        .collect()
}

#[test]
fn sdu_11_rewrite_per_form() {
    let token = cat(&["Fq7Lm2", &"Fq7Lm2".repeat(3)]);
    let pw = cat(&["Pat-7731", "-lark"]);
    let gh = github();
    let cases: Vec<(String, String, Vec<String>)> = vec![
        // Span inside double quotes.
        (
            cat(&["curl -H \"Authorization: Bearer ", &token, "\" https://x"]),
            "curl -H \"Authorization: Bearer ${ASURA_SECRET_1}\" https://x".into(),
            vec![token.clone()],
        ),
        // Whole unquoted word.
        (
            cat(&["docker login -u ci -p ", &pw, " ghcr.example.com"]),
            "docker login -u ci -p \"$ASURA_SECRET_1\" ghcr.example.com".into(),
            vec![pw.clone()],
        ),
        // Whole double-quoted word.
        (
            cat(&["docker login -u ci -p \"", &pw, "\" ghcr.example.com"]),
            "docker login -u ci -p \"${ASURA_SECRET_1}\" ghcr.example.com".into(),
            vec![pw.clone()],
        ),
        // Here-string word.
        (
            cat(&["sudo -S <<< ", &pw, " true"]),
            "sudo -S <<< \"$ASURA_SECRET_1\" true".into(),
            vec![pw.clone()],
        ),
        // SO1: inside an unquoted word, at its end.
        (
            cat(&["mysql -u root -p", "S3cretRoot", " shop"]),
            "mysql -u root -p\"$ASURA_SECRET_1\" shop".into(),
            vec![cat(&["S3cret", "Root"])],
        ),
        (
            cat(&["tool --password=", &pw]),
            "tool --password=\"$ASURA_SECRET_1\"".into(),
            vec![pw.clone()],
        ),
        (
            cat(&["curl -u admin:", "Sup3rS3cret", " https://x"]),
            "curl -u admin:\"$ASURA_SECRET_1\" https://x".into(),
            vec![cat(&["Sup3r", "S3cret"])],
        ),
        // SO1: assignments stay assignments.
        (
            cat(&["export GITHUB_TOKEN=", &gh]),
            "export GITHUB_TOKEN=\"$ASURA_SECRET_1\"".into(),
            vec![gh.clone()],
        ),
        (
            cat(&["GITHUB_TOKEN=", &gh, " gh auth status"]),
            "GITHUB_TOKEN=\"$ASURA_SECRET_1\" gh auth status".into(),
            vec![gh.clone()],
        ),
        // SO1: in the middle of a word.
        (
            cat(&["git clone https://u:", "hunter2x", "@example.com/r.git"]),
            "git clone https://u:\"$ASURA_SECRET_1\"@example.com/r.git".into(),
            vec![cat(&["hunter", "2x"])],
        ),
        // Equal values twice share one number; a second value gets the next.
        (
            cat(&["a --token ", &pw, " && b --token ", &pw, " && c --secret ", &gh]),
            "a --token \"$ASURA_SECRET_1\" && b --token \"$ASURA_SECRET_1\" && c --secret \"$ASURA_SECRET_2\"".into(),
            vec![pw.clone(), gh.clone()],
        ),
    ];
    for (command, expected, vars) in cases {
        let result = rewrite(rules(), &command).unwrap_or_else(|r| panic!("{command}: {r}"));
        assert_eq!(result.command, expected);
        let expected_vars: Vec<(u8, String)> = vars
            .into_iter()
            .enumerate()
            .map(|(i, v)| (i as u8 + 1, v))
            .collect();
        assert_eq!(values_of(&result), expected_vars, "{command}");
        // The shell value of each word is unchanged once the variables expand.
        let original: Vec<_> = crate::exec_cmd::split::tokens(&command)
            .into_iter()
            .map(|w| w.value)
            .collect();
        let mut expanded: Vec<_> = crate::exec_cmd::split::tokens(&result.command)
            .into_iter()
            .map(|w| w.value)
            .collect();
        for word in &mut expanded {
            for v in &result.variables {
                let value = String::from_utf8(v.value().to_vec()).unwrap();
                *word = word
                    .replace(&format!("${{{}}}", v.name()), &value)
                    .replace(&format!("${}", v.name()), &value);
            }
        }
        assert_eq!(expanded, original, "{command}");
        assert!(
            !format!("{result:?}").contains(&expected_vars_text(&result)),
            "debug shows a value"
        );
    }
    // No secret: unchanged.
    let plain = rewrite(rules(), "cargo test --workspace").unwrap();
    assert_eq!(
        (plain.command.as_str(), plain.variables.len()),
        ("cargo test --workspace", 0)
    );
    // A private key in double quotes across lines.
    let key = cat(&[
        "printf '%s' \"",
        &begin(""),
        "\n",
        &body_line("q9Zx"),
        "\n",
        &end(""),
        "\" > key.pem",
    ]);
    let result = rewrite(rules(), &key).unwrap();
    assert_eq!(
        result.command,
        "printf '%s' \"${ASURA_SECRET_1}\" > key.pem"
    );
    assert_eq!(result.variables[0].kind(), SecretKind::PrivateKey);
    assert_eq!(
        display_redacted(rules(), &cat(&["x --token ", &pw])),
        "x --token $ASURA_SECRET_1"
    );
}
fn expected_vars_text(r: &rewrite::Rewrite) -> String {
    r.variables
        .first()
        .map(|v| String::from_utf8(v.value().to_vec()).unwrap())
        .unwrap_or_else(|| "\u{0}".into())
}

#[test]
fn sdu_12_refusal_per_form_never_shows_the_value() {
    let pw = cat(&["Pat-7731", "-lark"]);
    let gh = github();
    use RefusalCode::*;
    let mut cases: Vec<(String, RefusalCode, Option<usize>)> = vec![
        (cat(&["echo '", &gh, "'"]), SecretSingleQuoted, Some(2)),
        (cat(&["echo $'", &gh, "'"]), SecretAnsiQuoted, Some(2)),
        (
            cat(&["machine h login u password Pw9x\"", "yz12 tail\""]),
            SecretMixedQuoting,
            Some(6),
        ),
        (
            cat(&["machine h login u password 'Pw9x'", "yz12abc"]),
            SecretMixedQuoting,
            Some(6),
        ),
        (
            cat(&["tool --password=", "pa*", "ss9x"]),
            SecretWithShellSyntax,
            Some(2),
        ),
        (
            cat(&["tool --password=", "pa~", "ss9x"]),
            SecretWithShellSyntax,
            Some(2),
        ),
        (
            cat(&["tool --password=\\", &pw]),
            SecretWithShellSyntax,
            Some(2),
        ),
        (
            cat(&["tool {a,b}--password=", &pw]),
            SecretWithShellSyntax,
            Some(2),
        ),
        (cat(&["echo $", &gh]), SecretWithShellSyntax, Some(2)),
        (
            cat(&["echo \"$", &gh, "\""]),
            SecretWithShellSyntax,
            Some(2),
        ),
        (
            cat(&["tool --password=", &pw, ">out"]),
            SecretWithShellSyntax,
            Some(2),
        ),
        (
            cat(&["echo \"machine h login u password Pw9x$", "yz12 tail\""]),
            SecretWithShellSyntax,
            Some(2),
        ),
        (
            cat(&["echo \"machine h login u password Pw9x\\", "yz12 tail\""]),
            SecretWithShellSyntax,
            Some(2),
        ),
        (
            cat(&["echo \"machine h login u password Pw9x`", "yz12 tail\""]),
            UnparsedCommand,
            Some(2),
        ),
        (
            cat(&["echo $(cat --token ", &pw, ")"]),
            SecretInSubstitution,
            Some(4),
        ),
        (
            cat(&["echo `tool --token ", &pw, "`"]),
            SecretInSubstitution,
            Some(4),
        ),
        (
            cat(&["(tool --token ", &pw, ")"]),
            SecretInSubstitution,
            Some(3),
        ),
        (
            cat(&["sh -c \"git push https://u:", "hunter2x", "@host\""]),
            SecretInInnerScript,
            Some(3),
        ),
        (
            cat(&["sudo bash -lc \"tool --token ", &pw, "\""]),
            SecretInInnerScript,
            Some(4),
        ),
        (
            cat(&["eval tool --token ", &pw]),
            SecretInInnerScript,
            Some(4),
        ),
        (
            cat(&["if sh -c \"tool --token ", &pw, "\"; then :; fi"]),
            SecretInInnerScript,
            Some(4),
        ),
        (
            cat(&["find . -exec sh -c \"tool --token ", &pw, "\" \\;"]),
            SecretInInnerScript,
            Some(6),
        ),
        (
            cat(&["cat <<EOF\npassword=", &pw, "\nEOF"]),
            SecretInHeredoc,
            None,
        ),
        (
            cat(&["cat <<-'EOF' > f\n\ttoken: ", &gh, "\n\tEOF\necho done"]),
            SecretInHeredoc,
            None,
        ),
        (
            cat(&["echo \"unclosed --token ", &pw]),
            UnparsedCommand,
            Some(2),
        ),
        (
            cat(&["echo $(tool --token ", &pw]),
            UnparsedCommand,
            Some(4),
        ),
        (
            cat(&["tool --token ", &pw, " <<"]),
            UnparsedCommand,
            Some(3),
        ),
        ("echo $ASURA_SECRET_3".into(), PlaceholderNotAvailable, None),
        (
            "echo \"${ASURA_SECRET_12}\"".into(),
            PlaceholderNotAvailable,
            None,
        ),
    ];
    let mut many = String::from("tool");
    for n in 0..17 {
        many.push_str(&format!(" --token Pw{n:02}xyz9"));
    }
    cases.push((many, TooManySecrets, None));
    let mut wrong = Vec::new();
    for (command, code, word) in cases {
        let refusal = rewrite(rules(), &command).expect_err(&command);
        if (refusal.code, refusal.word) != (code, word) {
            wrong.push(format!(
                "{command:?}: {:?} {:?}",
                refusal.code, refusal.word
            ));
        }
        let message = refusal.to_string();
        for secret in [pw.as_str(), gh.as_str(), "hunter2x", "Pw00xyz9"] {
            assert!(!message.contains(secret) && !format!("{refusal:?}").contains(secret));
        }
        assert!(message.starts_with("Command refused: "));
    }
    assert!(wrong.is_empty(), "{wrong:#?}");
    // 16 distinct values are allowed.
    let mut sixteen = String::from("tool");
    for n in 0..16 {
        sixteen.push_str(&format!(" --token Pw{n:02}xyz9"));
    }
    assert_eq!(rewrite(rules(), &sixteen).unwrap().variables.len(), 16);
    // `$ASURA_SECRET_` without a number names nothing.
    assert!(rewrite(rules(), "echo $ASURA_SECRET_X").is_ok());
    let refusal = Refusal {
        code: SecretSingleQuoted,
        kind: Some(SecretKind::CliPassword),
        word: Some(4),
    };
    assert_eq!(
        refusal.to_string(),
        "Command refused: a `cli-password` secret in word 4: it is single-quoted. Use double quotes so Asura can pass it as a variable."
    );
    let unavailable = rewrite::rewrite_with(Err(SelfCheckError::Digest), "true").unwrap_err();
    assert_eq!(unavailable.code.as_str(), "secret_detection_unavailable");
}

#[test]
fn sdu_12_residual_secret_is_refused() {
    // A rule without the placeholder predicate matches the inserted name;
    // a match wholly inside inserted text is not residual.
    let ok = rewrite(rules(), &cat(&["machine h login u password ", "Pw9xYz12"])).unwrap();
    assert_eq!(ok.command, "machine h login u password \"$ASURA_SECRET_1\"");
    let netrc = "machine h login u password \"$ASURA_SECRET_1\"";
    let name = netrc.find('"').unwrap() as u64;
    assert!(
        rewrite::verify(
            rules(),
            netrc,
            std::slice::from_ref(&(name..netrc.len() as u64))
        )
        .is_ok()
    );
    // Any matched byte outside the inserted text refuses.
    let partial = cat(&["x --token ", "Pw9xYz12"]);
    let refusal = rewrite::verify(rules(), &partial, std::slice::from_ref(&(10..14))).unwrap_err();
    assert_eq!(
        (refusal.code, refusal.word),
        (RefusalCode::ResidualSecret, Some(3))
    );
    let left = cat(&["x --token Pw9xYz12 --secret ", "\"$ASURA_SECRET_1\""]);
    let refusal = rewrite::verify(rules(), &left, std::slice::from_ref(&(27..44))).unwrap_err();
    assert_eq!(
        (refusal.code, refusal.kind),
        (RefusalCode::ResidualSecret, Some(SecretKind::CliPassword))
    );
}
/// The credential-name value bounds equal the regex captures: a debug
/// assertion in the matcher checks each match on generated lines.
#[test]
fn named_rule_values_equal_the_capture_groups() {
    const PARTS: &[&str] = &[
        "token",
        "db_pass",
        "X-Api-Key",
        "secret_key",
        "a.",
        ":",
        "=",
        " ",
        "\t",
        "\"",
        "'",
        "abcd1234",
        "Tq7-harbour",
        "(",
        "[",
        "+",
        ",",
        "x",
        "{",
        "}",
        "ab:cd=ef",
        "_hash",
        "password",
    ];
    let mut seed = 0x1234_5678_9abc_def0u64;
    let mut matched = 0;
    for _ in 0..20_000 {
        let mut line = String::new();
        for _ in 0..12 {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            line.push_str(PARTS[(seed % PARTS.len() as u64) as usize]);
        }
        matched += kinds(&line).len();
    }
    assert!(matched > 500, "{matched}");
}

/// J1T-03 unit (G6): a rewrite of exactly 8,192 bytes is admitted and one of
/// 8,193 bytes is refused with `rewritten_command_too_long`, whatever the
/// original length; the refusal holds no value.
#[test]
fn j1t_03_rewritten_command_length_limit() {
    use super::rewrite::{MAX_REWRITTEN_COMMAND_BYTES, rewrite_exec_cmd_call_with};
    let value = cat(&["Sup3r", "S3cret"]);
    let unit = cat(&["curl -u admin:", &value, " x;"]);
    let build = |rewritten_len: usize| {
        let units = unit.repeat(10);
        // Each occurrence grows by 6 bytes: 11 value bytes become 17.
        let filler = rewritten_len - 60 - units.len() - "echo ".len();
        cat(&[&units, "echo ", &"f".repeat(filler)])
    };
    let fits = build(MAX_REWRITTEN_COMMAND_BYTES);
    assert!(fits.len() < MAX_REWRITTEN_COMMAND_BYTES);
    let rewrite = rewrite_exec_cmd_call_with(Ok(rules()), &fits, ".").unwrap();
    assert_eq!(rewrite.command.len(), MAX_REWRITTEN_COMMAND_BYTES);
    assert_eq!(rewrite.variables.len(), 1);
    let over = build(MAX_REWRITTEN_COMMAND_BYTES + 1);
    assert!(over.len() <= MAX_REWRITTEN_COMMAND_BYTES);
    let refusal = rewrite_exec_cmd_call_with(Ok(rules()), &over, ".").unwrap_err();
    assert_eq!(refusal.code, RefusalCode::RewrittenCommandTooLong);
    assert_eq!(refusal.code.as_str(), "rewritten_command_too_long");
    assert!(!refusal.to_string().contains(&value));
}
/// J1 cwd scan: a detected span in the working directory refuses with
/// `secret_in_cwd`; a failed self-check refuses first.
#[test]
fn j1_cwd_secret_and_detection_unavailable_refuse() {
    use super::rewrite::rewrite_exec_cmd_call_with;
    let gh = github();
    let refusal = rewrite_exec_cmd_call_with(Ok(rules()), "ls", &cat(&["dir/", &gh])).unwrap_err();
    assert_eq!(refusal.code, RefusalCode::SecretInCwd);
    assert_eq!(refusal.code.as_str(), "secret_in_cwd");
    assert!(!refusal.to_string().contains(&gh));
    assert!(rewrite_exec_cmd_call_with(Ok(rules()), "ls", "src/bin").is_ok());
    let unavailable =
        rewrite_exec_cmd_call_with(Err(super::SelfCheckError::Digest), "ls", ".").unwrap_err();
    assert_eq!(unavailable.code, RefusalCode::DetectionUnavailable);
    // The command refusal comes before the cwd scan.
    let quoted = cat(&["echo '", &gh, "'"]);
    let refusal = rewrite_exec_cmd_call_with(Ok(rules()), &quoted, &gh).unwrap_err();
    assert_eq!(refusal.code, RefusalCode::SecretSingleQuoted);
}
