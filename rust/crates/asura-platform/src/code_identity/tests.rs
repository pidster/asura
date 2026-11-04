//! PL-N1 (security-policy.md, PL2 validation map): program identity of
//! system, Homebrew and team-signed binaries on the host, each compared with
//! `codesign -dvvv` (read only). No file is written.
use super::*;

/// `Identifier`, `TeamIdentifier` (empty for "not set") and `CDHash` from
/// `codesign -dvvv`, and whether it printed a platform identifier.
fn codesign(path: &str) -> (String, String, String, bool) {
    let out = std::process::Command::new("/usr/bin/codesign")
        .args(["-dvvv", path])
        .output()
        .expect("codesign runs");
    let text = String::from_utf8_lossy(&out.stderr).into_owned();
    let field = |key: &str| {
        text.lines()
            .find_map(|l| l.strip_prefix(key))
            .unwrap_or_default()
            .trim()
            .to_owned()
    };
    let team = field("TeamIdentifier=");
    (
        field("Identifier="),
        if team == "not set" {
            String::new()
        } else {
            team
        },
        field("CDHash="),
        text.lines().any(|l| l.starts_with("Platform identifier=")),
    )
}

fn compare(path: &str) -> CodeIdentity {
    let identity = read_identity(path);
    let (signing, team, cdhash, platform) = codesign(path);
    assert!(identity.identity_known, "{path}: {identity:?}");
    assert_eq!(identity.path, path);
    assert_eq!(identity.signing_id, signing, "{path}");
    assert_eq!(identity.team_id, team, "{path}");
    assert_eq!(identity.cdhash, cdhash, "{path}");
    assert_eq!(identity.platform, platform, "{path}");
    identity
}

#[test]
fn pl_n1_system_binaries_are_platform_code() {
    let sh = compare("/bin/sh");
    assert!(sh.platform && sh.team_id.is_empty());
    assert_eq!(sh.signing_id, "com.apple.sh");
    assert_eq!(sh.cdhash.len(), 40);
    let shim = compare(concat!("/usr/bin/", "git"));
    assert!(shim.platform && shim.team_id.is_empty());
    assert_eq!(
        shim.signing_id,
        "com.apple.dt.xcode_select.tool-shim-public"
    );
}

#[test]
fn pl_n1_homebrew_binary_is_ad_hoc_and_its_link_is_refused() {
    let link = "/opt/homebrew/bin/rg";
    assert!(
        std::path::Path::new(link).exists(),
        "GAP: no Homebrew ripgrep at {link} on this host; PL-N1 needs one"
    );
    let target = std::fs::canonicalize(link).unwrap();
    let identity = compare(target.to_str().unwrap());
    assert!(!identity.platform && identity.team_id.is_empty());
    assert!(!identity.signing_id.is_empty());
    // The symbolic link spelling has no canonical form: no identity.
    let through_link = read_identity(link);
    assert!(!through_link.identity_known);
    assert_eq!(through_link.path, link);
}

#[test]
fn pl_n1_team_signed_binary_has_its_team() {
    let path = "/Applications/Xcode.app/Contents/MacOS/Xcode";
    assert!(
        std::path::Path::new(path).exists(),
        "GAP: no Xcode at {path} on this host; PL-N1 needs a team-signed binary"
    );
    let identity = compare(path);
    assert!(!identity.platform);
    assert_eq!(identity.team_id, "59GAB85EFG");
    assert_eq!(identity.signing_id, "com.apple.dt.Xcode");
}

#[test]
fn unsigned_or_missing_files_have_no_identity() {
    let dir = std::env::temp_dir().join(format!("asura-code-identity-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = std::fs::canonicalize(&dir).unwrap().join("plain.sh");
    std::fs::write(&file, b"#!/bin/sh\necho hi\n").unwrap();
    let identity = read_identity(file.to_str().unwrap());
    assert!(!identity.identity_known && identity.signing_id.is_empty());
    assert_eq!(identity.path, file.to_str().unwrap());
    let missing = read_identity("/bin/asura-no-such-program");
    assert!(!missing.identity_known);
    assert_eq!(missing.path, "/bin/asura-no-such-program");
    assert_eq!(read_identity("bin/../sh").path, "");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn cache_reads_once_and_reports_changes() {
    let mut cache = IdentityCache::new();
    let (first, changed) = cache.read("/bin/sh");
    assert!(changed && first.identity_known);
    assert_eq!(cache.len(), 1);
    let (second, changed) = cache.read("/bin/sh");
    assert!(!changed);
    assert_eq!(first, second);
    assert_eq!(cache.len(), 1);
    assert!(fingerprint("/bin/sh").is_some());
    assert!(fingerprint("/tmp").is_some(), "the /private alias walks");
}
