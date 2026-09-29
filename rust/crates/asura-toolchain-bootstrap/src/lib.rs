//! Read-only validation of the repository's Protobuf tool lock.
//! Acceptance validates declarations, not downloaded bytes or executable tools.
#![forbid(unsafe_code)]

pub mod archive;
pub mod cache;
pub mod preparation;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fmt;

pub const MAX_LOCK_BYTES: usize = 16 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorCode {
    InputTooLarge,
    InvalidUtf8,
    InvalidJson,
    Unsupported,
    InvalidField,
    HostMismatch,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LockError {
    pub code: ErrorCode,
    pub field: &'static str,
}
impl fmt::Display for LockError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}: {}", self.code, self.field)
    }
}
impl std::error::Error for LockError {}
fn error(code: ErrorCode, field: &'static str) -> LockError {
    LockError { code, field }
}
fn require(ok: bool, field: &'static str) -> Result<(), LockError> {
    if ok {
        Ok(())
    } else {
        Err(error(ErrorCode::InvalidField, field))
    }
}

/// Observed host identity. Validation compares all fields to the lock declaration.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct HostToolchain {
    pub os: String,
    pub os_major: u32,
    pub arch: String,
    pub xcode_build: String,
    pub swift_version: String,
    pub rust_version: String,
    pub cargo_version: String,
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawLock {
    format_version: u32,
    host: HostToolchain,
    protoc: RawProtoc,
    swift_protobuf: RawSwift,
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawProtoc {
    version: String,
    url: String,
    sha256: String,
    archive_kind: String,
    allowed_top_level: Vec<String>,
    required_member: String,
    expected_tool_version: String,
    redirect_hosts: Vec<String>,
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSwift {
    version: String,
    url: String,
    sha256: String,
    archive_kind: String,
    top_level: String,
    source_commit: String,
    expected_tool_version: String,
    redirect_hosts: Vec<String>,
}
#[derive(Debug)]
pub struct ToolchainLock {
    host: HostToolchain,
    protoc: ProtocLock,
    swift_protobuf: SwiftProtobufLock,
    digest: [u8; 32],
}
#[derive(Debug)]
pub struct ProtocLock(RawProtoc);
#[derive(Debug)]
pub struct SwiftProtobufLock(RawSwift);
macro_rules! archive_accessors {
    () => {
        pub fn version(&self) -> &str {
            &self.0.version
        }
        pub fn url(&self) -> &str {
            &self.0.url
        }
        pub fn sha256(&self) -> &str {
            &self.0.sha256
        }
        pub fn archive_kind(&self) -> &str {
            &self.0.archive_kind
        }
        pub fn expected_tool_version(&self) -> &str {
            &self.0.expected_tool_version
        }
        pub fn redirect_hosts(&self) -> &[String] {
            &self.0.redirect_hosts
        }
    };
}
impl ProtocLock {
    archive_accessors!();
    pub fn allowed_top_level(&self) -> &[String] {
        &self.0.allowed_top_level
    }
    pub fn required_member(&self) -> &str {
        &self.0.required_member
    }
}
impl SwiftProtobufLock {
    archive_accessors!();
    pub fn top_level(&self) -> &str {
        &self.0.top_level
    }
    pub fn source_commit(&self) -> &str {
        &self.0.source_commit
    }
}
impl ToolchainLock {
    pub fn host(&self) -> &HostToolchain {
        &self.host
    }
    pub fn protoc(&self) -> &ProtocLock {
        &self.protoc
    }
    pub fn swift_protobuf(&self) -> &SwiftProtobufLock {
        &self.swift_protobuf
    }
    pub fn digest(&self) -> &[u8; 32] {
        &self.digest
    }
    pub fn validate_host(&self, observed: &HostToolchain) -> Result<(), LockError> {
        if observed == &self.host {
            Ok(())
        } else {
            Err(error(ErrorCode::HostMismatch, "host"))
        }
    }
}
fn version(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 32
        && value.split('.').count() <= 4
        && value
            .split('.')
            .all(|p| !p.is_empty() && p.len() <= 9 && p.bytes().all(|b| b.is_ascii_digit()))
}
fn component(value: &str) -> bool {
    !value.is_empty()
        && value != "."
        && value != ".."
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}
fn url(value: &str) -> bool {
    value.len() <= 2048
        && value
            .strip_prefix("https://github.com/")
            .is_some_and(|path| path.split('/').all(component))
}
fn hex(value: &str, size: usize) -> bool {
    value.len() == size
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn archive(
    version_value: &str,
    expected: &str,
    url_value: &str,
    hash: &str,
    redirects: &[String],
    allowed_redirect: &str,
    field: &'static str,
) -> Result<(), LockError> {
    require(
        version(version_value)
            && version_value == expected
            && url(url_value)
            && hex(hash, 64)
            && redirects.len() == 1
            && redirects[0] == allowed_redirect,
        field,
    )
}

/// Validate a bounded UTF-8 JSON declaration. No files, network or processes are used.
pub fn parse_lock(bytes: &[u8]) -> Result<ToolchainLock, LockError> {
    if bytes.len() > MAX_LOCK_BYTES {
        return Err(error(ErrorCode::InputTooLarge, "lock"));
    }
    std::str::from_utf8(bytes).map_err(|_| error(ErrorCode::InvalidUtf8, "lock"))?;
    let raw: RawLock =
        serde_json::from_slice(bytes).map_err(|_| error(ErrorCode::InvalidJson, "lock"))?;
    if raw.format_version != 1 {
        return Err(error(ErrorCode::Unsupported, "format_version"));
    }
    let host = &raw.host;
    if host.os != "macos" || host.os_major != 27 || host.arch != "aarch64" {
        return Err(error(ErrorCode::Unsupported, "host"));
    }
    require(
        !host.xcode_build.is_empty()
            && host.xcode_build.len() <= 32
            && host.xcode_build.bytes().all(|b| b.is_ascii_alphanumeric())
            && version(&host.swift_version)
            && version(&host.rust_version)
            && version(&host.cargo_version),
        "host",
    )?;
    let p = &raw.protoc;
    archive(
        &p.version,
        &p.expected_tool_version,
        &p.url,
        &p.sha256,
        &p.redirect_hosts,
        "release-assets.githubusercontent.com",
        "protoc",
    )?;
    require(
        p.archive_kind == "zip"
            && p.required_member == "bin/protoc"
            && p.allowed_top_level.len() == 3
            && ["bin", "include", "readme.txt"]
                .iter()
                .all(|name| p.allowed_top_level.iter().any(|item| item == name)),
        "protoc.members",
    )?;
    let s = &raw.swift_protobuf;
    archive(
        &s.version,
        &s.expected_tool_version,
        &s.url,
        &s.sha256,
        &s.redirect_hosts,
        "codeload.github.com",
        "swift_protobuf",
    )?;
    require(
        s.archive_kind == "tar.gz" && component(&s.top_level) && hex(&s.source_commit, 40),
        "swift_protobuf.members",
    )?;
    Ok(ToolchainLock {
        host: raw.host,
        protoc: ProtocLock(raw.protoc),
        swift_protobuf: SwiftProtobufLock(raw.swift_protobuf),
        digest: Sha256::digest(bytes).into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    const FIXTURE: &[u8] = include_bytes!("../tests/fixtures/lock.json");
    fn changed(path: &str, value: Value) -> Vec<u8> {
        let mut root: Value = serde_json::from_slice(FIXTURE).unwrap();
        *root.pointer_mut(path).unwrap() = value;
        serde_json::to_vec(&root).unwrap()
    }
    #[test]
    fn exact_bytes_define_identity() {
        let first = parse_lock(FIXTURE).unwrap();
        let mut spaced = FIXTURE.to_vec();
        spaced.push(b' ');
        assert_ne!(first.digest(), parse_lock(&spaced).unwrap().digest());
        assert_eq!(
            first.digest().as_slice(),
            Sha256::digest(FIXTURE).as_slice()
        );
    }
    #[test]
    fn reviewed_identities_are_data() {
        let mut root: Value = serde_json::from_slice(FIXTURE).unwrap();
        root["protoc"]["version"] = json!("37.1");
        root["protoc"]["expected_tool_version"] = json!("37.1");
        root["protoc"]["url"] = json!("https://github.com/example/project/new.zip");
        root["protoc"]["sha256"] = json!("a".repeat(64));
        root["swift_protobuf"]["source_commit"] = json!("b".repeat(40));
        let lock = parse_lock(&serde_json::to_vec(&root).unwrap()).unwrap();
        assert_eq!(lock.protoc().version(), "37.1");
        assert_eq!(lock.swift_protobuf().source_commit(), "b".repeat(40));
    }
    #[test]
    fn bounded_input_and_encoding() {
        assert_eq!(
            parse_lock(&vec![b' '; MAX_LOCK_BYTES + 1])
                .unwrap_err()
                .code,
            ErrorCode::InputTooLarge
        );
        let mut boundary = FIXTURE.to_vec();
        boundary.resize(MAX_LOCK_BYTES, b' ');
        assert!(parse_lock(&boundary).is_ok());
        assert_eq!(
            parse_lock(&[0xff]).unwrap_err().code,
            ErrorCode::InvalidUtf8
        );
        for bytes in [b"".as_slice(), b"null", b"[]", b"{}", b"{}{}"] {
            assert!(parse_lock(bytes).is_err());
        }
        let mut trailing = FIXTURE.to_vec();
        trailing.extend_from_slice(b"null");
        assert_eq!(
            parse_lock(&trailing).unwrap_err().code,
            ErrorCode::InvalidJson
        );
    }
    #[test]
    fn strict_fields_at_every_object() {
        let fixture: Value = serde_json::from_slice(FIXTURE).unwrap();
        for object in ["", "/host", "/protoc", "/swift_protobuf"] {
            let keys: Vec<_> = fixture
                .pointer(object)
                .unwrap()
                .as_object()
                .unwrap()
                .keys()
                .cloned()
                .collect();
            for key in keys {
                let mut missing = fixture.clone();
                missing
                    .pointer_mut(object)
                    .unwrap()
                    .as_object_mut()
                    .unwrap()
                    .remove(&key);
                assert!(
                    parse_lock(&serde_json::to_vec(&missing).unwrap()).is_err(),
                    "missing {object}/{key}"
                );
                let mut wrong = fixture.clone();
                wrong.pointer_mut(object).unwrap()[&key] = Value::Null;
                assert!(
                    parse_lock(&serde_json::to_vec(&wrong).unwrap()).is_err(),
                    "null {object}/{key}"
                );
                let encoded = serde_json::to_string(&fixture).unwrap();
                let original = serde_json::to_string(fixture.pointer(object).unwrap()).unwrap();
                let value = &fixture.pointer(object).unwrap()[&key];
                let duplicate_object = format!("{{\"{key}\":{value},{}", &original[1..]);
                let duplicate = encoded.replacen(&original, &duplicate_object, 1);
                assert!(
                    parse_lock(duplicate.as_bytes()).is_err(),
                    "duplicate {object}/{key}"
                );
            }
            let mut unknown = fixture.clone();
            unknown.pointer_mut(object).unwrap()["unknown"] = json!(1);
            assert!(parse_lock(&serde_json::to_vec(&unknown).unwrap()).is_err());
        }
    }
    #[test]
    fn rejects_invalid_declarations() {
        let cases = [
            ("/format_version", json!(2)),
            ("/host/os", json!("linux")),
            ("/host/os_major", json!(28)),
            ("/host/arch", json!("x86_64")),
            ("/host/xcode_build", json!("")),
            ("/host/xcode_build", json!("a".repeat(33))),
            ("/host/xcode_build", json!("27 A")),
            ("/host/swift_version", json!("6..4")),
            ("/host/rust_version", json!("1.2.3.4.5")),
            ("/host/cargo_version", json!("1234567890")),
            ("/protoc/archive_kind", json!("tar.gz")),
            ("/protoc/required_member", json!("../protoc")),
            (
                "/protoc/allowed_top_level",
                json!(["bin", "bin", "readme.txt"]),
            ),
            ("/protoc/expected_tool_version", json!("99")),
            ("/protoc/sha256", json!("A".repeat(64))),
            ("/protoc/sha256", json!("a".repeat(63))),
            ("/swift_protobuf/sha256", json!("g".repeat(64))),
            ("/protoc/redirect_hosts", json!([])),
            (
                "/protoc/redirect_hosts",
                json!([
                    "release-assets.githubusercontent.com",
                    "release-assets.githubusercontent.com"
                ]),
            ),
            ("/swift_protobuf/redirect_hosts", json!(["evil.example"])),
            ("/swift_protobuf/archive_kind", json!("zip")),
            ("/swift_protobuf/top_level", json!("../source")),
            ("/swift_protobuf/source_commit", json!("a".repeat(39))),
            ("/swift_protobuf/source_commit", json!("A".repeat(40))),
        ];
        for (path, value) in cases {
            assert!(parse_lock(&changed(path, value)).is_err(), "{path}");
        }
    }
    #[test]
    fn url_and_scalar_boundaries() {
        for value in [
            "",
            "http://github.com/a",
            "https://github.com.evil/a",
            "https://user@github.com/a",
            "https://github.com:443/a",
            "https://github.com/",
            "https://github.com/a//b",
            "https://github.com/../a",
            "https://github.com/a/./b",
            "https://github.com/a?x",
            "https://github.com/a#x",
            "https://github.com/%2e",
            "https://github.com/a\\b",
            "https://github.com/é",
            "https://github.com/a b",
        ] {
            assert!(!url(value), "{value}");
            assert!(parse_lock(&changed("/protoc/url", json!(value))).is_err());
        }
        let boundary = format!("https://github.com/{}", "a".repeat(2029));
        assert_eq!(boundary.len(), 2048);
        assert!(url(&boundary));
        assert!(!url(&(boundary + "a")));
        for value in [
            "",
            ".1",
            "1.",
            "a",
            "1.2.3.4.5",
            "1234567890",
            "123456789.123456789.123456789.12345",
        ] {
            assert!(!version(value));
        }
        assert!(version("123456789.123456789.123456789.12"));
        for value in ["", ".", "..", "a/b", "a\\b"] {
            assert!(!component(value));
        }
    }
    #[test]
    fn errors_do_not_disclose_input() {
        let err = parse_lock(b"{\"secret-token\":true}").unwrap_err();
        assert_eq!(err.to_string(), "InvalidJson: lock");
        assert!(!format!("{err:?}").contains("secret"));
    }
}
