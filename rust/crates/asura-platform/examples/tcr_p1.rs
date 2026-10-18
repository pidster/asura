//! TCR-P1 measurement (`docs/designs/tool-call-records.md`): parse and redact
//! 64,000 bytes of worst-case argument JSON. Acceptance: 99th percentile at
//! most 5 ms per call on the development host, measured in a release build:
//! `cargo run --release -p asura-platform --example tcr_p1`. Exits nonzero on a
//! miss. A figure for acceptance, never a runtime limit. Fixture secrets are
//! assembled at run time.
use asura_platform::secrets::{self, ArgumentsInput, MAX_ARGUMENT_BYTES, json};
use std::time::{Duration, Instant};

fn token(seed: usize) -> String {
    let mut body = format!("{seed:04}Qx7Lm");
    while body.len() < 36 {
        body.push_str("Zk3Wv");
    }
    body.truncate(36);
    ["ghp_", &body].concat()
}

/// A 64,000-byte object: one member nested to depth 32, then many members
/// whose strings hold escapes, context-dependent assignments and tokens.
fn worst_case() -> String {
    let mut text = String::from("{\"deep\":");
    text.push_str(&"[".repeat(31));
    text.push_str("\"x\"");
    text.push_str(&"]".repeat(31));
    let mut i = 0;
    loop {
        let member = match i % 4 {
            0 => format!(
                ",\"k{i}\":\"line\\n\\t\\\"quoted\\\" \\u00e9 {}\"",
                token(i % 200)
            ),
            1 => format!(",\"password_{i}\":\"Tr0ub4dor{i:05}\""),
            2 => format!(",\"n{i}\":12345.678e-3"),
            _ => format!(",\"s{i}\":\"plain text with no secret at all {i}\""),
        };
        if text.len() + member.len() + 1 > MAX_ARGUMENT_BYTES {
            break;
        }
        text.push_str(&member);
        i += 1;
    }
    while text.len() + 1 < MAX_ARGUMENT_BYTES {
        text.push(' ');
    }
    text.push('}');
    text
}

fn main() {
    let rules = secrets::detection().expect("rule set self-check");
    let text = worst_case();
    assert_eq!(text.len(), MAX_ARGUMENT_BYTES);
    json::parse(&text).expect("fixture parses");
    let input = ArgumentsInput {
        name: Some("memory"),
        arguments: Some(&text),
        exec_cmd: None,
        exec_cmd_name: false,
    };
    let first = secrets::redact_arguments(rules, &input);
    println!(
        "fixture: {} bytes, form {:?}, stored {} bytes, {} numbers, {} unnumbered",
        text.len(),
        first.form,
        first.text.len(),
        first.number_kinds.len(),
        first.unnumbered
    );
    let runs = 1000;
    let mut samples: Vec<Duration> = Vec::with_capacity(runs);
    for _ in 0..runs {
        let started = Instant::now();
        let parsed = json::parse(&text);
        let redacted = secrets::redact_arguments(rules, &input);
        samples.push(started.elapsed());
        let _ = std::hint::black_box((parsed, redacted));
    }
    samples.sort();
    let at = |q: f64| samples[((runs as f64 * q).ceil() as usize).min(runs) - 1];
    let ms = |d: Duration| d.as_secs_f64() * 1000.0;
    let p99 = at(0.99);
    println!(
        "TCR-P1 runs={runs} p50={:.3}ms p99={:.3}ms max={:.3}ms (acceptance p99 <= 5ms, release build)",
        ms(at(0.50)),
        ms(p99),
        ms(samples[runs - 1])
    );
    if p99 > Duration::from_millis(5) {
        eprintln!("TCR-P1 MISS: p99 above 5 ms");
        std::process::exit(1);
    }
}
