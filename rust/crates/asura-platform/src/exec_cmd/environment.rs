//! The allow-list environment of person commands
//! (`docs/designs/person-command-environment.md`, owner decision ENV1).
//!
//! One table serves every user: the TUI builds the terminal variables of a
//! submit from it, the service checks the submit against it, the kind 31
//! codec checks recorded entries, the configuration owner checks declared
//! names, and the setup thread and the guardian check the FD 11 frame.
//! Pure functions over bytes; no I/O. Values never appear in an error, a
//! reason or debug text.
use crate::secrets::environment::Finding;

/// How a terminal value is checked (value classes).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Class {
    /// 1–64 bytes of ASCII letters, digits, `_ - . @`.
    Locale,
    /// 1–128 bytes of ASCII letters, digits, `_ - . @ + : /`.
    Word,
    /// 1–512 bytes of UTF-8, no NUL or control character, starting with `/`.
    Path,
    /// 1–2,048 bytes of UTF-8, no NUL or control character, at most 64
    /// entries separated by `:`, each starting with `/`.
    PathList,
    /// 1–103 bytes starting with `/`, no NUL.
    Socket,
}

/// Where a terminal name belongs in the person-facing explanation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Source {
    Base,
    Toolchain,
}

/// One terminal table entry: its name code (1-based table index), name,
/// value class and explanation group.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TerminalName {
    pub code: u8,
    pub name: &'static str,
    pub class: Class,
    pub source: Source,
}

const fn t(code: u8, name: &'static str, class: Class, source: Source) -> TerminalName {
    TerminalName {
        code,
        name,
        class,
        source,
    }
}

use Class::{Locale, Path, PathList, Socket, Word};
use Source::{Base, Toolchain};

/// Most terminal names, and the highest name code.
pub const TERMINAL_COUNT: usize = 39;

/// The terminal table (person-command-environment.md, terminal table).
/// Codes 1–9 keep the PC3 codes. A new name appends a code; a removed code
/// is never reused.
pub const TERMINAL: [TerminalName; TERMINAL_COUNT] = [
    t(1, "LANG", Locale, Base),
    t(2, "LC_ALL", Locale, Base),
    t(3, "LC_COLLATE", Locale, Base),
    t(4, "LC_CTYPE", Locale, Base),
    t(5, "LC_MESSAGES", Locale, Base),
    t(6, "LC_MONETARY", Locale, Base),
    t(7, "LC_NUMERIC", Locale, Base),
    t(8, "LC_TIME", Locale, Base),
    t(9, "SSH_AUTH_SOCK", Socket, Base),
    t(10, "TZ", Word, Base),
    t(11, "XDG_CONFIG_HOME", Path, Base),
    t(12, "XDG_CACHE_HOME", Path, Base),
    t(13, "XDG_DATA_HOME", Path, Base),
    t(14, "XDG_STATE_HOME", Path, Base),
    t(15, "CARGO_HOME", Path, Toolchain),
    t(16, "RUSTUP_HOME", Path, Toolchain),
    t(17, "RUSTUP_TOOLCHAIN", Word, Toolchain),
    t(18, "CARGO_TARGET_DIR", Path, Toolchain),
    t(19, "DEVELOPER_DIR", Path, Toolchain),
    t(20, "SDKROOT", Path, Toolchain),
    t(21, "TOOLCHAINS", Word, Toolchain),
    t(22, "HOMEBREW_PREFIX", Path, Toolchain),
    t(23, "HOMEBREW_CELLAR", Path, Toolchain),
    t(24, "HOMEBREW_REPOSITORY", Path, Toolchain),
    t(25, "GOPATH", PathList, Toolchain),
    t(26, "GOROOT", Path, Toolchain),
    t(27, "GOMODCACHE", Path, Toolchain),
    t(28, "GOCACHE", Path, Toolchain),
    t(29, "NVM_DIR", Path, Toolchain),
    t(30, "VOLTA_HOME", Path, Toolchain),
    t(31, "PNPM_HOME", Path, Toolchain),
    t(32, "BUN_INSTALL", Path, Toolchain),
    t(33, "PYENV_ROOT", Path, Toolchain),
    t(34, "VIRTUAL_ENV", Path, Toolchain),
    t(35, "CONDA_PREFIX", Path, Toolchain),
    t(36, "JAVA_HOME", Path, Toolchain),
    t(37, "RBENV_ROOT", Path, Toolchain),
    t(38, "GEM_HOME", Path, Toolchain),
    t(39, "GEM_PATH", PathList, Toolchain),
];

/// The names the slot setup thread sets itself, PATH first (PATH comes from
/// the terminal but is sanitized and recorded on its own, kind 31 field 11).
pub const ASURA_SET: [&str; 8] = [
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "SHELL",
    "TMPDIR",
    "TERM",
    "GIT_TERMINAL_PROMPT",
];
/// Prefixes only the service sets; no other source may use them.
pub const RESERVED_PREFIXES: [&str; 2] = ["ASURA_SECRET_", "ASURA_JOB_"];
/// Declared names may not start with this.
const ASURA_PREFIX: &str = "ASURA_";

/// The PC2 PATH bounds.
pub const MAX_PATH_BYTES: usize = 4_096;
pub const MAX_PATH_ENTRIES: usize = 64;
/// The largest terminal values together: 8 × 64 + 103 + 3 × 128 + 25 × 512
/// + 2 × 2,048.
pub const MAX_TERMINAL_VALUE_BYTES: usize = 17_895;
/// Declared variables per project.
pub const MAX_DECLARED: usize = 32;
/// Projects with declarations.
pub const MAX_DECLARED_PROJECTS: usize = 256;
/// Bytes of one declared name, and of any FD 11 name.
pub const MAX_NAME_BYTES: usize = 64;
/// Bytes of one declared value.
pub const MAX_DECLARED_VALUE_BYTES: usize = 1_024;
/// Names and values of one project's declarations together.
pub const MAX_DECLARED_BYTES: usize = 4_096;

/// The terminal table entry named `name`.
pub fn terminal(name: &[u8]) -> Option<&'static TerminalName> {
    TERMINAL.iter().find(|t| t.name.as_bytes() == name)
}
/// The terminal table entry with `code`.
pub fn by_code(code: u8) -> Option<&'static TerminalName> {
    TERMINAL.get(usize::from(code).checked_sub(1)?)
}

fn control_free(value: &[u8]) -> bool {
    std::str::from_utf8(value).is_ok_and(|text| !text.chars().any(char::is_control))
}

/// Whether `value` passes `class`.
pub fn class_valid(class: Class, value: &[u8]) -> bool {
    match class {
        Class::Locale => {
            (1..=64).contains(&value.len())
                && value
                    .iter()
                    .all(|b| b.is_ascii_alphanumeric() || b"_-.@".contains(b))
        }
        Class::Word => {
            (1..=128).contains(&value.len())
                && value
                    .iter()
                    .all(|b| b.is_ascii_alphanumeric() || b"_-.@+:/".contains(b))
        }
        Class::Path => (1..=512).contains(&value.len()) && value[0] == b'/' && control_free(value),
        Class::PathList => {
            (1..=2_048).contains(&value.len())
                && control_free(value)
                && value.split(|b| *b == b':').count() <= 64
                && value
                    .split(|b| *b == b':')
                    .all(|e| e.first() == Some(&b'/'))
        }
        Class::Socket => {
            (1..=103).contains(&value.len()) && value[0] == b'/' && !value.contains(&0)
        }
    }
}

/// The PC2 PATH sanitizer: at most 4,096 bytes of UTF-8, no NUL, at most 64
/// entries before dropping; empty and relative entries dropped, order kept.
/// `None` is `path_invalid`.
pub fn sanitize_path(raw: &[u8]) -> Option<String> {
    if raw.len() > MAX_PATH_BYTES || raw.contains(&0) {
        return None;
    }
    let text = std::str::from_utf8(raw).ok()?;
    if text.split(':').count() > MAX_PATH_ENTRIES {
        return None;
    }
    let kept: Vec<&str> = text.split(':').filter(|e| e.starts_with('/')).collect();
    if kept.is_empty() {
        return None;
    }
    Some(kept.join(":"))
}

/// The FD 11 name form: 1–64 bytes, `[A-Za-z_][A-Za-z0-9_]*`.
pub fn name_form(name: &[u8]) -> bool {
    (1..=MAX_NAME_BYTES).contains(&name.len())
        && (name[0].is_ascii_alphabetic() || name[0] == b'_')
        && name.iter().all(|b| b.is_ascii_alphanumeric() || *b == b'_')
}
/// Whether `name` starts with a reserved prefix.
pub fn reserved(name: &[u8]) -> bool {
    RESERVED_PREFIXES
        .iter()
        .any(|p| name.starts_with(p.as_bytes()))
}
/// A declared name: the name form, not on the terminal table, not an
/// Asura-set name and not starting with `ASURA_`.
pub fn declared_name_valid(name: &[u8]) -> bool {
    name_form(name)
        && terminal(name).is_none()
        && !ASURA_SET.iter().any(|n| n.as_bytes() == name)
        && !name.starts_with(ASURA_PREFIX.as_bytes())
}
/// A declared value: 1–1,024 bytes of UTF-8 with no NUL or control
/// character. The scanner check is separate.
pub fn declared_value_valid(value: &[u8]) -> bool {
    (1..=MAX_DECLARED_VALUE_BYTES).contains(&value.len()) && control_free(value)
}
/// One project's declarations: at most 32, each valid, names unique and in
/// increasing byte order, at most 4,096 bytes of names and values.
pub fn declared_valid<'a>(entries: impl IntoIterator<Item = (&'a [u8], &'a [u8])>) -> bool {
    let mut last: Option<&[u8]> = None;
    let mut count = 0;
    let mut bytes = 0usize;
    for (name, value) in entries {
        count += 1;
        bytes += name.len() + value.len();
        if count > MAX_DECLARED
            || bytes > MAX_DECLARED_BYTES
            || !declared_name_valid(name)
            || !declared_value_valid(value)
            || last.is_some_and(|l| l >= name)
        {
            return false;
        }
        last = Some(name);
    }
    true
}

/// Why a captured variable is not passed. Never holds a value.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NotPassed {
    /// The environment scanner flagged it.
    Flagged(Finding),
    /// A terminal table name whose value fails its class.
    Invalid,
}

/// The terminal variables of one submit, built from a captured
/// environment (PE-F1). No `Debug`: it holds values.
#[derive(Clone, Default)]
pub struct Built {
    /// The sanitized PATH, or `None` (`path_invalid`).
    pub path: Option<String>,
    /// Passed terminal variables in name code order: code and value.
    pub passed: Vec<(u8, String)>,
    /// Terminal table names that are set but not passed, with the reason.
    pub not_passed: Vec<(&'static str, NotPassed)>,
}

/// Builds the terminal variables from `captured` (name, value pairs as the
/// process environment holds them). `flagged` answers the scanner's finding
/// for one name; a flagged variable is never passed.
pub fn build(captured: &[(Vec<u8>, Vec<u8>)], flagged: &dyn Fn(&[u8]) -> Option<Finding>) -> Built {
    let value_of = |name: &str| {
        captured
            .iter()
            .find(|(n, _)| n.as_slice() == name.as_bytes())
            .map(|(_, v)| v.as_slice())
    };
    let mut built = Built {
        path: value_of("PATH").and_then(sanitize_path),
        ..Built::default()
    };
    for entry in &TERMINAL {
        let Some(value) = value_of(entry.name) else {
            continue;
        };
        if let Some(finding) = flagged(entry.name.as_bytes()) {
            built
                .not_passed
                .push((entry.name, NotPassed::Flagged(finding)));
        } else if !class_valid(entry.class, value) {
            built.not_passed.push((entry.name, NotPassed::Invalid));
        } else if let Ok(text) = std::str::from_utf8(value) {
            built.passed.push((entry.code, text.to_owned()));
        } else {
            built.not_passed.push((entry.name, NotPassed::Invalid));
        }
    }
    built
}

/// The FD 11 entry check shared by the setup thread and the guardian (FD 11
/// frame): at most `max_entries` and `max_bytes`; each name in the name
/// form, once, not reserved; no NUL in a value; a terminal table name's
/// value passes its class; PATH, HOME and TMPDIR present.
pub fn frame_entries_valid<'a>(
    entries: impl Iterator<Item = (&'a [u8], &'a [u8])>,
    max_entries: usize,
    max_bytes: usize,
) -> bool {
    let mut names: Vec<&[u8]> = Vec::new();
    let mut total = 0usize;
    for (name, value) in entries {
        total += name.len() + value.len();
        if names.len() == max_entries
            || total > max_bytes
            || !name_form(name)
            || reserved(name)
            || value.contains(&0)
            || names.contains(&name)
            || terminal(name).is_some_and(|t| !class_valid(t.class, value))
        {
            return false;
        }
        names.push(name);
    }
    ["PATH", "HOME", "TMPDIR"]
        .iter()
        .all(|n| names.contains(&n.as_bytes()))
}

/// Whether a client-supplied entry (a terminal or declared variable) may
/// reach the setup: a terminal table name whose value passes its class, or a
/// valid declared name and value.
pub fn client_entry_valid(name: &[u8], value: &[u8]) -> bool {
    match terminal(name) {
        Some(t) => class_valid(t.class, value),
        None => declared_name_valid(name) && declared_value_valid(value),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// PE-U1: the table.
    #[test]
    fn pe_u1_terminal_table() {
        for (index, entry) in TERMINAL.iter().enumerate() {
            assert_eq!(usize::from(entry.code), index + 1);
            assert!(name_form(entry.name.as_bytes()), "{}", entry.name);
            assert!(!ASURA_SET.contains(&entry.name));
            assert!(!reserved(entry.name.as_bytes()));
            assert_eq!(by_code(entry.code), Some(entry));
            assert_eq!(terminal(entry.name.as_bytes()), Some(entry));
            assert!(!declared_name_valid(entry.name.as_bytes()));
        }
        let mut names: Vec<_> = TERMINAL.iter().map(|t| t.name).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), TERMINAL_COUNT);
        // Codes 1-9 keep the PC3 codes.
        let pc3 = [
            "LANG",
            "LC_ALL",
            "LC_COLLATE",
            "LC_CTYPE",
            "LC_MESSAGES",
            "LC_MONETARY",
            "LC_NUMERIC",
            "LC_TIME",
            "SSH_AUTH_SOCK",
        ];
        for (index, name) in pc3.iter().enumerate() {
            assert_eq!(TERMINAL[index].name, *name);
        }
        assert_eq!(by_code(0), None);
        assert_eq!(by_code(40), None);
        // The value bound matches the classes.
        let total: usize = TERMINAL
            .iter()
            .map(|t| match t.class {
                Class::Locale => 64,
                Class::Word => 128,
                Class::Path => 512,
                Class::PathList => 2_048,
                Class::Socket => 103,
            })
            .sum();
        assert_eq!(total, MAX_TERMINAL_VALUE_BYTES);
        for name in ASURA_SET {
            assert!(!declared_name_valid(name.as_bytes()));
        }
    }

    /// PE-U2: value classes at their bounds.
    #[test]
    fn pe_u2_value_classes() {
        assert!(class_valid(Class::Locale, b"en_GB.UTF-8"));
        assert!(class_valid(Class::Locale, &[b'a'; 64]));
        assert!(!class_valid(Class::Locale, &[b'a'; 65]));
        assert!(!class_valid(Class::Locale, b""));
        assert!(!class_valid(Class::Locale, b"en GB"));
        assert!(!class_valid(Class::Locale, b"C/x"));
        assert!(class_valid(Class::Word, b"America/New_York"));
        assert!(class_valid(
            Class::Word,
            b"com.apple.dt.toolchain.Metal.32023.921.5"
        ));
        assert!(class_valid(Class::Word, &[b'w'; 128]));
        assert!(!class_valid(Class::Word, &[b'w'; 129]));
        assert!(!class_valid(Class::Word, b"a b"));
        let mut long = vec![b'/'];
        long.extend([b'p'; 511]);
        assert!(class_valid(Class::Path, &long));
        long.push(b'p');
        assert!(!class_valid(Class::Path, &long));
        assert!(!class_valid(Class::Path, b"relative/path"));
        assert!(!class_valid(Class::Path, b"/a\nb"));
        assert!(!class_valid(Class::Path, b"/a\0b"));
        assert!(!class_valid(Class::Path, b"/\xff"));
        assert!(class_valid(Class::PathList, b"/a:/b"));
        assert!(!class_valid(Class::PathList, b"/a::/b"));
        assert!(!class_valid(Class::PathList, b"/a:b"));
        let many = vec!["/x"; 65].join(":");
        assert!(!class_valid(Class::PathList, many.as_bytes()));
        let enough = vec!["/x"; 64].join(":");
        assert!(class_valid(Class::PathList, enough.as_bytes()));
        assert!(class_valid(Class::Socket, b"/tmp/agent.sock"));
        let mut socket = vec![b'/'];
        socket.extend([b's'; 103]);
        assert!(!class_valid(Class::Socket, &socket));
        assert!(!class_valid(Class::Socket, b"tmp/agent.sock"));
        // PATH sanitizer.
        assert_eq!(sanitize_path(b"/a::b:./c:/d").as_deref(), Some("/a:/d"));
        assert_eq!(sanitize_path(b"::rel"), None);
        assert_eq!(sanitize_path(b"/a\0:/b"), None);
        assert_eq!(sanitize_path(&[b'/'; 4097]), None);
        assert_eq!(sanitize_path(vec!["/x"; 65].join(":").as_bytes()), None);
    }

    /// PE-U3: building from a synthetic captured environment.
    #[test]
    fn pe_u3_build_terminal_variables() {
        let captured: Vec<(Vec<u8>, Vec<u8>)> = [
            ("PATH", "/usr/bin::rel:/bin"),
            ("LANG", "en_GB.UTF-8"),
            ("LC_TIME", "bad value"),
            ("CARGO_HOME", "/Users/me/.cargo"),
            ("UNLISTED_MARKER", "on"),
            ("HOME", "/Users/elsewhere"),
            ("RUSTUP_HOME", "/Users/me/.rustup"),
        ]
        .iter()
        .map(|(n, v)| (n.as_bytes().to_vec(), v.as_bytes().to_vec()))
        .collect();
        let flagged = |name: &[u8]| (name == b"RUSTUP_HOME").then_some(Finding::HighEntropy);
        let built = build(&captured, &flagged);
        assert_eq!(built.path.as_deref(), Some("/usr/bin:/bin"));
        let codes: Vec<u8> = built.passed.iter().map(|(c, _)| *c).collect();
        assert_eq!(codes, [1, 15]);
        assert_eq!(
            built.not_passed,
            [
                ("LC_TIME", NotPassed::Invalid),
                ("RUSTUP_HOME", NotPassed::Flagged(Finding::HighEntropy))
            ]
        );
        // No reason or debug text holds a value.
        let text = format!("{:?}", built.not_passed);
        assert!(!text.contains("bad value") && !text.contains(".rustup"));
        assert!(build(&[], &|_| None).path.is_none());
    }

    /// Declared names and values, and the FD 11 entry check.
    #[test]
    fn declared_and_frame_rules() {
        assert!(declared_name_valid(b"RUST_LOG"));
        assert!(declared_name_valid(b"_X1"));
        assert!(!declared_name_valid(b"1X"));
        assert!(!declared_name_valid(b"A-B"));
        assert!(!declared_name_valid(b"ASURA_ANYTHING"));
        assert!(!declared_name_valid(b"HOME"));
        assert!(!declared_name_valid(&[b'A'; 65]));
        assert!(declared_value_valid(b"debug"));
        assert!(!declared_value_valid(b""));
        assert!(!declared_value_valid(b"a\tb"));
        assert!(!declared_value_valid(&[b'v'; 1025]));
        let ok: Vec<(&[u8], &[u8])> = vec![(b"A", b"1"), (b"B", b"2")];
        assert!(declared_valid(ok.iter().copied()));
        let unordered: Vec<(&[u8], &[u8])> = vec![(b"B", b"1"), (b"A", b"2")];
        assert!(!declared_valid(unordered.iter().copied()));
        let names: Vec<String> = (0..33).map(|i| format!("V{i:02}")).collect();
        let many: Vec<(&[u8], &[u8])> = names.iter().map(|n| (n.as_bytes(), &b"1"[..])).collect();
        assert!(!declared_valid(many.iter().copied()));
        assert!(declared_valid(many[..32].iter().copied()));
        let big = vec![b'v'; 1_024];
        let bulky: Vec<(&[u8], &[u8])> = names[..4]
            .iter()
            .map(|n| (n.as_bytes(), big.as_slice()))
            .collect();
        assert!(!declared_valid(bulky.iter().copied()));

        let base: Vec<(&[u8], &[u8])> = vec![
            (b"PATH", b"/bin"),
            (b"HOME", b"/h"),
            (b"TMPDIR", b"/t"),
            (b"RUST_LOG", b"debug"),
        ];
        assert!(frame_entries_valid(base.iter().copied(), 128, 40_960));
        for bad in [
            (&b"ASURA_JOB_1"[..], &b"1"[..]),
            (b"ASURA_SECRET_1", b"1"),
            (b"BAD-NAME", b"1"),
            (b"LANG", b"bad value"),
            (b"X", b"a\0b"),
            (b"HOME", b"/dup"),
        ] {
            let mut entries = base.clone();
            entries.push(bad);
            assert!(!frame_entries_valid(entries.iter().copied(), 128, 40_960));
        }
        assert!(!frame_entries_valid(base[1..].iter().copied(), 128, 40_960));
        assert!(!frame_entries_valid(base.iter().copied(), 3, 40_960));
        assert!(client_entry_valid(b"LANG", b"C"));
        assert!(!client_entry_valid(b"HOME", b"/x"));
        assert!(client_entry_valid(b"RUST_LOG", b"debug"));
    }
}
