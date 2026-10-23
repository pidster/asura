//! The fixed path lists (execution-path-guard.md, "Path lists"; EPG-E3):
//! no configuration key, no input from the model or the project. A path is
//! project-relative with `/` separators. The depth of a pattern is the number
//! of directory components before its anchor (`build.rs` at the root has
//! depth 0).
//!
//! The implicit-execution names are the ones the `model-1` Seatbelt profile
//! denies (`asura-platform/src/exec_cmd/guardian.rs`, `IMPLICIT_HOOKS` and
//! `IMPLICIT_FILES`, EPG-P); [`IMPLICIT_HOOKS`] and [`IMPLICIT_FILES`] must stay
//! equal to them.

/// The hook directory: the directory and every file under it.
pub(crate) const IMPLICIT_HOOKS: &str = ".git/hooks";
/// The implicit-execution files.
pub(crate) const IMPLICIT_FILES: [&str; 6] = [
    ".git/config",
    ".gitattributes",
    ".gitmodules",
    ".envrc",
    ".cargo/config",
    ".cargo/config.toml",
];
/// Directories the walks never enter (`.git` only as listed).
pub(crate) const SKIPPED: [&str; 6] = [".git", "target", "node_modules", ".build", "dist", "build"];
/// The depth bound of the implicit list.
pub(crate) const IMPLICIT_DEPTH: usize = 8;
/// The depth bound of the build and test list, and the levels kept below a
/// test directory.
pub(crate) const BUILD_DEPTH: usize = 4;
/// Entries one walk visits.
pub(crate) const MAX_ENTRIES: usize = 20_000;
/// Build and test paths one walk keeps, in byte order.
pub(crate) const MAX_BUILD_PATHS: usize = 1_024;
/// Implicit-execution paths one walk keeps, in byte order.
pub(crate) const MAX_IMPLICIT_PATHS: usize = 256;
/// Bytes of one hashed file; a larger file is `too_large`.
pub(crate) const MAX_HASHED_BYTES: u64 = 1024 * 1024;

/// Build files, by exact name.
const BUILD_FILES: [&str; 28] = [
    "build.rs",
    "Cargo.toml",
    "Makefile",
    "makefile",
    "GNUmakefile",
    "CMakeLists.txt",
    "package.json",
    "Package.swift",
    "pyproject.toml",
    "setup.py",
    "setup.cfg",
    "tox.ini",
    "noxfile.py",
    "justfile",
    "Justfile",
    "Rakefile",
    "Gemfile",
    "build.gradle",
    "build.gradle.kts",
    "settings.gradle",
    "settings.gradle.kts",
    "gradlew",
    "pom.xml",
    "go.mod",
    "rust-toolchain",
    "rust-toolchain.toml",
    "conftest.py",
    "pytest.ini",
];
/// Build files and test configuration by suffix or prefix.
const BUILD_SUFFIXES: [&str; 2] = [".mk", ".cmake"];
const TEST_CONFIG_PREFIXES: [&str; 2] = ["jest.config.", "vitest.config."];
/// Test code by name.
const TEST_SUFFIXES: [&str; 6] = [
    "_test.go", "_test.py", ".test.js", ".test.ts", ".spec.js", ".spec.ts",
];
const TEST_PREFIXES: [&str; 1] = ["test_"];
/// Test directories: every file under one, up to [`BUILD_DEPTH`] levels.
const TEST_DIRECTORIES: [&str; 4] = ["tests", "test", "__tests__", "spec"];

/// Which list a walk collects.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum List {
    Implicit,
    BuildTest,
}

fn parts(path: &str) -> Option<Vec<&str>> {
    let parts: Vec<&str> = path.split('/').collect();
    parts
        .iter()
        .all(|p| !p.is_empty() && *p != "." && *p != "..")
        .then_some(parts)
}

fn named_build_or_test(name: &str) -> bool {
    BUILD_FILES.contains(&name)
        || BUILD_SUFFIXES
            .iter()
            .any(|s| name.len() > s.len() && name.ends_with(s))
        || TEST_CONFIG_PREFIXES
            .iter()
            .any(|p| name.len() > p.len() && name.starts_with(p))
        || TEST_SUFFIXES
            .iter()
            .any(|s| name.len() > s.len() && name.ends_with(s))
        || TEST_PREFIXES
            .iter()
            .any(|p| name.len() > p.len() + 3 && name.starts_with(p) && name.ends_with(".py"))
}

/// Whether `path` names a file in the build and test list.
pub(crate) fn build_or_test(path: &str) -> bool {
    let Some(parts) = parts(path) else {
        return false;
    };
    let (name, directories) = parts.split_last().expect("one part");
    if directories.iter().any(|d| SKIPPED.contains(d)) {
        return false;
    }
    if directories.len() <= BUILD_DEPTH && named_build_or_test(name) {
        return true;
    }
    // Under a test directory at depth up to 4, up to 4 levels below it.
    directories.iter().enumerate().any(|(depth, d)| {
        depth <= BUILD_DEPTH
            && TEST_DIRECTORIES.contains(d)
            && parts.len() - 1 - depth <= BUILD_DEPTH
    })
}

/// Whether `path` names a file in the implicit-execution list: a file
/// under [`IMPLICIT_HOOKS`] or one of [`IMPLICIT_FILES`], anchored at depth
/// up to [`IMPLICIT_DEPTH`] outside the skipped directories.
pub(crate) fn implicit(path: &str) -> bool {
    let Some(parts) = parts(path) else {
        return false;
    };
    let n = parts.len();
    let hooks: Vec<&str> = IMPLICIT_HOOKS.split('/').collect();
    let anchor = IMPLICIT_FILES
        .iter()
        .find_map(|file| {
            let file: Vec<&str> = file.split('/').collect();
            (n >= file.len() && parts[n - file.len()..] == file[..]).then(|| n - file.len())
        })
        .or_else(|| {
            (0..n.saturating_sub(hooks.len())).find(|&i| parts[i..i + hooks.len()] == hooks[..])
        });
    anchor.is_some_and(|anchor| {
        anchor <= IMPLICIT_DEPTH && !parts[..anchor].iter().any(|d| SKIPPED.contains(d))
    })
}

/// Whether `path` names a file in `list`.
pub(crate) fn listed(list: List, path: &str) -> bool {
    match list {
        List::Implicit => implicit(path),
        List::BuildTest => build_or_test(path),
    }
}

/// Whether the walk of `list` enters the directory `path` (its components):
/// the pruning rule that keeps each walk inside its depth bounds. Every file
/// a listed predicate accepts lies under a directory this accepts.
pub(crate) fn enters(list: List, path: &[&str]) -> bool {
    if path.is_empty() {
        return true;
    }
    let index = path.len() - 1;
    match list {
        List::Implicit => {
            if let Some(git) = path.iter().position(|p| *p == ".git") {
                // `.git` itself, then only its `hooks` subtree.
                return git <= IMPLICIT_DEPTH
                    && !path[..git].iter().any(|d| SKIPPED.contains(d))
                    && (index == git || path[git + 1] == "hooks");
            }
            if path.iter().any(|d| SKIPPED.contains(d)) {
                return false;
            }
            if let Some(cargo) = path.iter().position(|p| *p == ".cargo") {
                return cargo == index && cargo <= IMPLICIT_DEPTH;
            }
            // A plain directory: its files are at depth `index + 1`.
            index < IMPLICIT_DEPTH
        }
        List::BuildTest => {
            if path.iter().any(|d| SKIPPED.contains(d)) {
                return false;
            }
            // The deepest test directory within the bound gives the most
            // levels below it.
            let test = path
                .iter()
                .enumerate()
                .rev()
                .find(|(depth, d)| *depth <= BUILD_DEPTH && TEST_DIRECTORIES.contains(d))
                .map(|(depth, _)| depth);
            match test {
                // Files below the test directory up to 4 levels.
                Some(depth) => index - depth < BUILD_DEPTH,
                None => index < BUILD_DEPTH,
            }
        }
    }
}
