//! Secret detection against the committed Wisp fixtures (SDU-15, SDM-01) and
//! the throughput and rewrite measurements (SDM-02, SDM-03). Measurements are
//! reported, never enforced at run time; run them in release mode with
//! `cargo test --release -p asura-platform --test secrets_fixtures -- --ignored --nocapture`.
#![cfg(target_os = "macos")]
use asura_platform::secrets::{JobRedaction, Stream, detection, rewrite::rewrite};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

fn fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/secrets")
}

struct Line {
    number: usize,
    label: String,
    text: String,
    hard_negative: bool,
}

/// Keeps `secret` and `none` lines, strips U+200B, and marks the lines after
/// `# slice: hard negatives`.
fn load(name: &str) -> Vec<Line> {
    let text = std::fs::read_to_string(fixtures().join(name)).expect("fixture");
    let mut slice = false;
    let mut lines = Vec::new();
    for (i, raw) in text.lines().enumerate() {
        if raw.starts_with('#') {
            slice |= raw == "# slice: hard negatives";
            continue;
        }
        let (label, body) = raw.split_once('\t').expect("label and text");
        if label == "secret" || label == "none" {
            lines.push(Line {
                number: i + 1,
                label: label.into(),
                text: body.replace('\u{200B}', ""),
                hard_negative: slice,
            });
        }
    }
    lines
}

/// Wisp's rule labels at the pinned commit, keyed by file and line.
fn wisp_labels() -> BTreeMap<(String, usize), String> {
    let text = std::fs::read_to_string(fixtures().join("wisp-rule-labels.tsv")).expect("labels");
    text.lines()
        .filter(|l| !l.starts_with('#'))
        .map(|l| {
            let f: Vec<_> = l.split('\t').collect();
            ((f[0].to_string(), f[1].parse().unwrap()), f[2].to_string())
        })
        .collect()
}

/// The port's label for one line: scanned as a one-line stream.
fn label(text: &str) -> &'static str {
    let mut job = JobRedaction::new(detection().expect("rules"), &[]);
    let _ = job.scan(Stream::Stdout, text.as_bytes());
    let _ = job.scan(Stream::Stdout, b"\n");
    let _ = job.finish(Stream::Stdout);
    if job.stats(Stream::Stdout).replacements > 0 {
        "secret"
    } else {
        "none"
    }
}

#[test]
fn sdu_15_loader_keeps_secret_and_none_lines_and_strips_the_mark() {
    for (name, secret, none) in [
        ("train.tsv", 164, 225),
        ("dev.tsv", 25, 44),
        ("test.tsv", 211, 216),
    ] {
        let lines = load(name);
        assert_eq!(
            lines.iter().filter(|l| l.label == "secret").count(),
            secret,
            "{name}"
        );
        assert_eq!(
            lines.iter().filter(|l| l.label == "none").count(),
            none,
            "{name}"
        );
        assert!(lines.iter().all(|l| !l.text.contains('\u{200B}')), "{name}");
        let raw = std::fs::read_to_string(fixtures().join(name)).unwrap();
        assert!(raw.contains('\u{200B}'), "{name} keeps the mark on disk");
        assert!(
            raw.lines().all(|l| l.starts_with('#')
                || l.starts_with("secret\t")
                || l.starts_with("none\t"))
        );
    }
    let hard: Vec<_> = load("test.tsv")
        .into_iter()
        .filter(|l| l.hard_negative)
        .collect();
    assert_eq!(hard.len(), 119);
    assert!(hard.iter().all(|l| l.label == "none"));
}

/// Lines whose label differs from Wisp's, each with its recorded cause
/// (G1 ASCII semantics or the union overlap rule). The test set is never used
/// to choose a rule.
const DIVERGENCES: &[(&str, usize, &str)] = &[];

#[test]
fn sdm_01_parity_with_wisp_rule_labels() {
    let wisp = wisp_labels();
    let mut divergent = Vec::new();
    for name in ["train.tsv", "dev.tsv", "test.tsv"] {
        let lines = load(name);
        let (mut tp, mut fp, mut hard_fp, mut secrets) = (0, 0, 0, 0);
        for line in &lines {
            let ours = label(&line.text);
            let theirs = &wisp[&(name.to_string(), line.number)];
            if ours != theirs {
                divergent.push((name, line.number, ours, theirs.clone()));
            }
            secrets += usize::from(line.label == "secret");
            tp += usize::from(line.label == "secret" && ours == "secret");
            fp += usize::from(line.label == "none" && ours == "secret");
            hard_fp += usize::from(line.hard_negative && ours == "secret");
        }
        let precision = tp as f64 / (tp + fp).max(1) as f64;
        println!(
            "SDM-01 {name}: recall {tp}/{secrets} ({:.1}%), false alarms {fp}/{}, precision {precision:.3}, hard-negative false alarms {hard_fp}/{}",
            100.0 * tp as f64 / secrets as f64,
            lines.len() - secrets,
            lines.iter().filter(|l| l.hard_negative).count(),
        );
        if name == "test.tsv" {
            assert!(tp >= 67, "test recall {tp} below Wisp's 67 of 211");
        }
    }
    for (name, number, ours, theirs) in &divergent {
        println!("SDM-01 divergence {name}:{number}: port {ours}, Wisp {theirs}");
    }
    let listed: Vec<_> = DIVERGENCES.iter().map(|(f, n, _)| (*f, *n)).collect();
    let found: Vec<_> = divergent.iter().map(|(f, n, _, _)| (*f, *n)).collect();
    assert_eq!(
        found, listed,
        "every divergence must be recorded with its cause"
    );
}

/// SDU-11 with the real shell: `/bin/sh` expands each rewritten command, with
/// its secret variables in the environment, to the same words as the original.
#[test]
fn sdu_11_rewritten_words_keep_their_shell_values_in_sh() {
    use std::os::unix::ffi::OsStrExt;
    let rules = detection().expect("rules");
    let token = ["Fq7Lm2", &"Fq7Lm2".repeat(3)].concat();
    let gh = ["gh", "p_", &"aB3".repeat(12)].concat();
    let pw = ["Pat-7731", "-lark"].concat();
    let commands = [
        ["curl -H \"Authorization: Bearer ", &token, "\" https://x"].concat(),
        ["docker login -u ci -p ", &pw, " ghcr.example.com"].concat(),
        ["docker login -u ci -p \"", &pw, "\" ghcr.example.com"].concat(),
        ["mysql -u root -p", "S3cretRoot", " shop"].concat(),
        ["tool --password=", &pw, " *.none"].concat(),
        ["curl -u admin:", "Sup3rS3cret", " https://x"].concat(),
        ["export GITHUB_TOKEN=", &gh].concat(),
        ["GITHUB_TOKEN=", &gh, " gh auth status"].concat(),
        ["git clone https://u:", "hunter2x", "@example.com/r.git"].concat(),
        ["a --token ", &pw, " b --token \"", &pw, "\" --secret ", &gh].concat(),
        ["echo pre\"post\"--token=", &pw].concat(),
    ];
    let words = |command: &str, vars: &[(String, Vec<u8>)]| {
        let mut sh = std::process::Command::new("/bin/sh");
        sh.env_clear()
            .env("PATH", "/usr/bin:/bin")
            .arg("-c")
            .arg(format!("set -- {command}; printf '%s\\0' \"$@\""));
        for (name, value) in vars {
            sh.env(name, std::ffi::OsStr::from_bytes(value));
        }
        let out = sh.output().expect("sh");
        assert!(out.status.success(), "{command}");
        out.stdout
    };
    for command in commands {
        let result = rewrite(rules, &command).unwrap_or_else(|r| panic!("{command}: {r}"));
        assert!(!result.variables.is_empty(), "{command}");
        assert!(
            !result.command.contains(&pw)
                && !result.command.contains(&gh)
                && !result.command.contains(&token)
        );
        let vars: Vec<_> = result
            .variables
            .iter()
            .map(|v| (v.name(), v.value().to_vec()))
            .collect();
        assert_eq!(
            words(&result.command, &vars),
            words(&command, &[]),
            "{command} -> {}",
            result.command
        );
    }
    // A here-string word: compare what the command reads. A function stands
    // in for sudo so the detected form runs without privilege.
    let original = ["sudo() { cat; }; sudo -S <<< ", &pw].concat();
    let result = rewrite(rules, &original).unwrap();
    assert_eq!(
        result.command,
        "sudo() { cat; }; sudo -S <<< \"$ASURA_SECRET_1\""
    );
    let read = |command: &str, vars: &[(String, Vec<u8>)]| {
        let mut sh = std::process::Command::new("/bin/sh");
        sh.env_clear()
            .env("PATH", "/usr/bin:/bin")
            .arg("-c")
            .arg(command);
        for (name, value) in vars {
            sh.env(name, std::ffi::OsStr::from_bytes(value));
        }
        sh.output().expect("sh").stdout
    };
    let vars: Vec<_> = result
        .variables
        .iter()
        .map(|v| (v.name(), v.value().to_vec()))
        .collect();
    assert_eq!(read(&result.command, &vars), read(&original, &[]));
    assert_eq!(read(&original, &[]), [pw.as_bytes(), b"\n"].concat());
}

fn percentile(sorted: &[Duration], p: f64) -> Duration {
    sorted[((sorted.len() - 1) as f64 * p).round() as usize]
}

/// Deterministic pseudo-random bytes.
fn noise(len: usize, mut seed: u64) -> Vec<u8> {
    (0..len)
        .map(|_| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed as u8
        })
        .collect()
}

fn fill_512k(unit: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(512 * 1024);
    while out.len() < 512 * 1024 {
        let take = unit.len().min(512 * 1024 - out.len());
        out.extend_from_slice(&unit[..take]);
    }
    out
}

fn corpus() -> Vec<(&'static str, Vec<u8>)> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let mut build_log = Vec::new();
    for i in 0..400 {
        build_log.extend_from_slice(
            format!(
                "   Compiling crate-{i} v0.{i}.3 (/work/src/crate-{i})\nwarning: unused variable `x{i}`\n  --> src/lib.rs:{i}:9\n   |\n{i:>4} |     let x{i} = compute({i});\n   |         ^^ help: if this is intentional, prefix it with an underscore\n"
            )
            .as_bytes(),
        );
    }
    let mut diff = Vec::new();
    for file in [
        "src/secrets/stream.rs",
        "src/secrets/rules.rs",
        "src/exec_cmd/split.rs",
        "src/exec_cmd.rs",
    ] {
        let text = std::fs::read(root.join(file)).unwrap_or_default();
        diff.extend_from_slice(
            format!("diff --git a/{file} b/{file}\n--- a/{file}\n+++ b/{file}\n@@ -1,1 +1,1 @@\n")
                .as_bytes(),
        );
        for line in text.split(|&b| b == b'\n') {
            diff.push(b'+');
            diff.extend_from_slice(line);
            diff.push(b'\n');
        }
    }
    let lock = std::fs::read(root.join("../../../Cargo.lock")).unwrap_or_default();
    let mut minified = Vec::new();
    for i in 0..4000 {
        minified.extend_from_slice(format!("function f{i}(a,b){{return a.map(function(c){{return c*{i}+b}})}};var v{i}=f{i}([1,2,3],{i});").as_bytes());
    }
    let mut adversarial = Vec::new();
    for _ in 0..200 {
        adversarial.extend_from_slice(b"x");
        adversarial.extend_from_slice(&b"token: ".repeat(400));
        adversarial.push(b'\n');
    }
    let mut adversarial_long = b"x".to_vec();
    adversarial_long.extend_from_slice(&b" token: a.token:".repeat(40_000));
    vec![
        ("build log", fill_512k(&build_log)),
        ("git log -p", fill_512k(&diff)),
        ("lockfile", fill_512k(&lock)),
        ("minified JS", fill_512k(&minified)),
        ("binary", noise(512 * 1024, 0x9e37_79b9_7f4a_7c15)),
        ("adversarial token: lines", fill_512k(&adversarial)),
        (
            "adversarial 64 KiB token: lines",
            fill_512k(&adversarial_long),
        ),
    ]
}

#[test]
#[ignore = "measurement; run in release mode"]
fn sdm_02_scan_throughput_per_512_kib() {
    let rules = detection().expect("rules");
    for (name, input) in corpus() {
        assert!(!input.is_empty(), "{name}");
        let mut times = Vec::new();
        for _ in 0..30 {
            let mut job = JobRedaction::new(rules, &[]);
            let start = Instant::now();
            let mut stored = 0;
            for chunk in input.chunks(64 * 1024) {
                stored += job.scan(Stream::Stdout, chunk).len();
            }
            stored += job.finish(Stream::Stdout).len();
            times.push(start.elapsed());
            std::hint::black_box(stored);
        }
        times.sort();
        println!(
            "SDM-02 {name}: p50 {:?} p95 {:?} worst {:?}",
            percentile(&times, 0.5),
            percentile(&times, 0.95),
            times.last().unwrap()
        );
    }
}

#[test]
#[ignore = "measurement; run in release mode"]
fn sdm_03_rewrite_time_for_8192_byte_commands() {
    let rules = detection().expect("rules");
    let token = ["gh", "p_", &"aB3".repeat(12)].concat();
    let mut ordinary = String::from("curl -sS -H \"Authorization: Bearer ");
    ordinary.push_str(&token);
    ordinary.push_str("\" https://api.example.com/v1/items");
    while ordinary.len() < 8150 {
        ordinary.push_str(" --data-urlencode q=item");
    }
    let mut adversarial = String::from("x");
    while adversarial.len() < 8180 {
        adversarial.push_str(" token: a");
    }
    let mut quoted = String::from("echo");
    while quoted.len() < 8150 {
        quoted.push_str(" \"a'b\" $(c) `d`");
    }
    let mut many = String::from("tool");
    let mut n = 0;
    while many.len() < 8100 {
        many.push_str(&format!(" --password=Pw{n:04}x9"));
        n += 1;
    }
    for (name, command) in [
        ("ordinary", ordinary),
        ("adversarial token:", adversarial),
        ("quoting", quoted),
        ("many secrets", many),
    ] {
        assert!(command.len() <= 8192, "{name}");
        let mut times = Vec::new();
        for _ in 0..50 {
            let start = Instant::now();
            let result = rewrite(rules, &command);
            times.push(start.elapsed());
            std::hint::black_box(result.is_ok());
        }
        times.sort();
        println!(
            "SDM-03 {name} ({} bytes): p50 {:?} worst {:?}",
            command.len(),
            percentile(&times, 0.5),
            times.last().unwrap()
        );
    }
}
