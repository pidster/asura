//! Real encoded archives and public file workflows; no tool is executed.
#![forbid(unsafe_code)]
#[cfg(not(target_os = "macos"))]
compile_error!("archive qualification requires macOS network confinement");
use asura_toolchain_bootstrap::{
    archive::{
        ArchiveErrorKind, ArchiveSelection, MAX_COMPRESSED_BYTES, MAX_EXTRACTED_BYTES,
        extract_archive,
    },
    parse_lock,
};
use flate2::{Compression, write::GzEncoder};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{Cursor, Read, Write},
    os::unix::fs::{PermissionsExt, symlink},
    path::{Path, PathBuf},
    process::Command,
    sync::atomic::{AtomicUsize, Ordering},
    thread,
    time::{Duration, Instant},
};
use zip::write::SimpleFileOptions;
static NEXT: AtomicUsize = AtomicUsize::new(0);
fn scratch() -> PathBuf {
    let p = std::env::temp_dir().canonicalize().unwrap().join(format!(
        "asura-archive-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::SeqCst)
    ));
    fs::create_dir(&p).unwrap();
    p
}
fn lock_bytes(bytes: &[u8], swift: bool) -> Vec<u8> {
    let mut lock: serde_json::Value =
        serde_json::from_slice(include_bytes!("fixtures/lock.json")).unwrap();
    lock[if swift { "swift_protobuf" } else { "protoc" }]["sha256"] =
        format!("{:x}", Sha256::digest(bytes)).into();
    serde_json::to_vec(&lock).unwrap()
}
fn zip_fixture(entries: &[(&str, &[u8], u32)]) -> Vec<u8> {
    let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
    for (name, body, mode) in entries {
        let options = SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored)
            .unix_permissions(*mode);
        if name.ends_with('/') {
            writer.add_directory(*name, options).unwrap();
        } else {
            writer.start_file(*name, options).unwrap();
            writer.write_all(body).unwrap();
        }
    }
    writer.finish().unwrap().into_inner()
}
fn tar_fixture(entries: &[(&str, u8, &[u8])]) -> Vec<u8> {
    let mut builder = tar::Builder::new(Vec::new());
    for (name, kind, body) in entries {
        let mut h = tar::Header::new_ustar();
        h.set_mode(0o755);
        h.set_size(body.len() as u64);
        h.set_entry_type(tar::EntryType::new(*kind));
        h.as_mut_bytes()[..100].fill(0);
        h.as_mut_bytes()[..name.len()].copy_from_slice(name.as_bytes());
        h.set_cksum();
        builder.append(&h, *body).unwrap();
    }
    gzip(&builder.into_inner().unwrap())
}
fn gzip(bytes: &[u8]) -> Vec<u8> {
    let mut e = GzEncoder::new(Vec::new(), Compression::fast());
    e.write_all(bytes).unwrap();
    e.finish().unwrap()
}
fn reject(bytes: &[u8], swift: bool) -> ArchiveErrorKind {
    let root = scratch();
    let target = root.join("stage");
    let lock = parse_lock(&lock_bytes(bytes, swift)).unwrap();
    let error = extract_archive(
        bytes,
        if swift {
            ArchiveSelection::SwiftProtobuf(lock.swift_protobuf())
        } else {
            ArchiveSelection::Protoc(lock.protoc())
        },
        &target,
    )
    .unwrap_err();
    assert!(!error.cleanup_failed);
    assert!(!target.exists());
    fs::remove_dir(root).unwrap();
    error.kind
}
#[test]
fn valid_zip_preserves_bytes_and_restricts_modes() {
    let bytes = zip_fixture(&[
        ("bin/protoc", b"binary", 0o777),
        ("bin/", b"", 0o777),
        ("readme.txt", b"readme", 0o666),
    ]);
    let lock = parse_lock(&lock_bytes(&bytes, false)).unwrap();
    let root = scratch();
    let target = root.join("stage");
    let extracted =
        extract_archive(&bytes, ArchiveSelection::Protoc(lock.protoc()), &target).unwrap();
    assert_eq!(extracted.path(), target);
    assert_eq!(extracted.file_count(), 2);
    assert_eq!(extracted.payload_bytes(), 12);
    assert_eq!(fs::read(target.join("bin/protoc")).unwrap(), b"binary");
    for (path, mode) in [
        (target.clone(), 0o700),
        (target.join("bin"), 0o700),
        (target.join("bin/protoc"), 0o700),
        (target.join("readme.txt"), 0o600),
    ] {
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o7777,
            mode
        );
    }
    fs::remove_dir_all(root).unwrap();
}
#[test]
fn valid_tar_accepts_only_locked_leading_comment() {
    let commit = parse_lock(include_bytes!("fixtures/lock.json"))
        .unwrap()
        .swift_protobuf()
        .source_commit()
        .to_owned();
    let comment = format!("52 comment={commit}\n");
    let bytes = tar_fixture(&[
        ("pax_global_header", b'g', comment.as_bytes()),
        ("swift-protobuf-1.38.1/", b'5', b""),
        ("swift-protobuf-1.38.1/source", b'0', b"swift"),
    ]);
    let lock = parse_lock(&lock_bytes(&bytes, true)).unwrap();
    let root = scratch();
    let target = root.join("stage");
    let result = extract_archive(
        &bytes,
        ArchiveSelection::SwiftProtobuf(lock.swift_protobuf()),
        &target,
    )
    .unwrap();
    assert_eq!(result.file_count(), 1);
    assert_eq!(result.payload_bytes(), 5);
    assert_eq!(
        fs::read(target.join("swift-protobuf-1.38.1/source")).unwrap(),
        b"swift"
    );
    assert!(!target.join("pax_global_header").exists());
    fs::remove_dir_all(root).unwrap();
    let bad = tar_fixture(&[
        (
            "pax_global_header",
            b'g',
            b"52 comment=aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n",
        ),
        ("swift-protobuf-1.38.1/a", b'0', b"x"),
    ]);
    assert_eq!(reject(&bad, true), ArchiveErrorKind::InvalidMember);
    let repeated = tar_fixture(&[
        ("pax_global_header", b'g', comment.as_bytes()),
        ("pax_global_header", b'g', comment.as_bytes()),
        ("swift-protobuf-1.38.1/a", b'0', b"x"),
    ]);
    assert_eq!(reject(&repeated, true), ArchiveErrorKind::InvalidMember);
}
#[test]
fn checksum_and_compressed_bound_precede_mutation() {
    let root = scratch();
    let target = root.join("stage");
    let lock = parse_lock(include_bytes!("fixtures/lock.json")).unwrap();
    assert_eq!(
        extract_archive(b"wrong", ArchiveSelection::Protoc(lock.protoc()), &target)
            .unwrap_err()
            .kind,
        ArchiveErrorKind::DigestMismatch
    );
    assert_eq!(
        extract_archive(
            &vec![0; MAX_COMPRESSED_BYTES + 1],
            ArchiveSelection::Protoc(lock.protoc()),
            &target
        )
        .unwrap_err()
        .kind,
        ArchiveErrorKind::CompressedLimit
    );
    assert!(fs::read_dir(&root).unwrap().next().is_none());
    fs::remove_dir(root).unwrap();
}
#[test]
fn unsafe_members_and_missing_executable_fail() {
    for name in [
        "/outside",
        "bin/../outside",
        "bin/./protoc",
        "bin//protoc",
        "bin\\protoc",
        "outside/file",
        "readme.txt/file",
    ] {
        assert!(matches!(
            reject(&zip_fixture(&[(name, b"data", 0o755)]), false),
            ArchiveErrorKind::InvalidPath | ArchiveErrorKind::InvalidMember
        ));
    }
    assert_eq!(
        reject(&zip_fixture(&[("readme.txt", b"data", 0o644)]), false),
        ArchiveErrorKind::MissingExecutable
    );
    assert_eq!(
        reject(&zip_fixture(&[("bin/protoc", b"data", 0o644)]), false),
        ArchiveErrorKind::MissingExecutable
    );
    for kind in *b"12346Lxg" {
        assert_eq!(
            reject(
                &tar_fixture(&[("swift-protobuf-1.38.1/entry", kind, b"")]),
                true
            ),
            ArchiveErrorKind::InvalidMember
        );
    }
    assert_eq!(
        reject(&tar_fixture(&[("outside", b'0', b"bad")]), true),
        ArchiveErrorKind::InvalidMember
    );
}
#[test]
fn duplicates_and_prefix_conflicts_fail_without_partial_stage() {
    let mut duplicate = zip_fixture(&[
        ("bin/protoc", b"first", 0o755),
        ("bin/second", b"other", 0o755),
    ]);
    for start in 0..duplicate.len() - 10 {
        if &duplicate[start..start + 10] == b"bin/second" {
            duplicate[start..start + 10].copy_from_slice(b"bin/protoc");
        }
    }
    assert_eq!(reject(&duplicate, false), ArchiveErrorKind::InvalidArchive);
    let prefix = zip_fixture(&[
        ("bin/child/a", b"first", 0o644),
        ("bin/child", b"other", 0o755),
    ]);
    assert_eq!(reject(&prefix, false), ArchiveErrorKind::Collision);
    let duplicate_tar = tar_fixture(&[
        ("swift-protobuf-1.38.1/a", b'0', b"1"),
        ("swift-protobuf-1.38.1/a", b'0', b"2"),
    ]);
    assert_eq!(reject(&duplicate_tar, true), ArchiveErrorKind::Collision);
    let aliases = zip_fixture(&[
        ("bin/protoc", b"first", 0o755),
        ("bin/PROTOC", b"other", 0o755),
    ]);
    let root = scratch();
    fs::write(root.join("case"), b"a").unwrap();
    if root.join("CASE").exists() {
        assert_eq!(reject(&aliases, false), ArchiveErrorKind::Collision);
    } else {
        let lock = parse_lock(&lock_bytes(&aliases, false)).unwrap();
        assert_eq!(
            extract_archive(
                &aliases,
                ArchiveSelection::Protoc(lock.protoc()),
                &root.join("stage")
            )
            .unwrap()
            .file_count(),
            2
        );
    }
    fs::remove_dir_all(root).unwrap();
}
#[test]
fn corrupt_zip_and_gzip_and_tar_tail_fail() {
    let valid = zip_fixture(&[("bin/protoc", b"unique-body", 0o755)]);
    let mut corrupt = valid.clone();
    let index = corrupt
        .windows(11)
        .position(|w| w == b"unique-body")
        .unwrap();
    corrupt[index] ^= 1;
    assert_eq!(reject(&corrupt, false), ArchiveErrorKind::InvalidArchive);
    assert_eq!(
        reject(&valid[..valid.len() - 1], false),
        ArchiveErrorKind::InvalidArchive
    );
    let tar = tar_fixture(&[("swift-protobuf-1.38.1/a", b'0', b"data")]);
    let mut corrupt = tar.clone();
    let last = corrupt.len() - 8;
    corrupt[last] ^= 1;
    assert_eq!(reject(&corrupt, true), ArchiveErrorKind::InvalidArchive);
    let mut trailing = tar.clone();
    trailing.push(1);
    assert_eq!(reject(&trailing, true), ArchiveErrorKind::InvalidArchive);
    let mut concatenated = tar.clone();
    concatenated.extend_from_slice(&tar);
    assert_eq!(
        reject(&concatenated, true),
        ArchiveErrorKind::InvalidArchive
    );
    let mut raw = Vec::new();
    flate2::read::GzDecoder::new(&tar[..])
        .read_to_end(&mut raw)
        .unwrap();
    raw.extend_from_slice(b"not-zero");
    assert_eq!(reject(&gzip(&raw), true), ArchiveErrorKind::InvalidArchive);
}
#[test]
fn zip_special_types_encryption_and_hidden_records_fail() {
    let bytes = zip_fixture(&[("bin/protoc", b"x", 0o755)]);
    let central = bytes.windows(4).position(|w| w == b"PK\x01\x02").unwrap();
    for filetype in [0o120777u32, 0o010600, 0o020600] {
        let mut modified = bytes.clone();
        modified[central + 38..central + 42].copy_from_slice(&(filetype << 16).to_le_bytes());
        assert_eq!(reject(&modified, false), ArchiveErrorKind::InvalidMember);
    }
    let mut encrypted = bytes.clone();
    encrypted[central + 8] |= 1;
    assert_eq!(reject(&encrypted, false), ArchiveErrorKind::InvalidArchive);
    let mut extra = bytes.clone();
    let end = bytes.len() - 22;
    extra.splice(end..end, bytes[central..end].iter().copied());
    let eocd = extra.len() - 22;
    extra[eocd + 12..eocd + 16]
        .copy_from_slice(&((end - central) * 2u32 as usize).to_le_bytes()[..4]);
    assert_eq!(reject(&extra, false), ArchiveErrorKind::InvalidArchive);
}
#[test]
fn existing_or_linked_destinations_are_preserved() {
    let bytes = zip_fixture(&[("bin/protoc", b"data", 0o755)]);
    let lock = parse_lock(&lock_bytes(&bytes, false)).unwrap();
    let root = scratch();
    let existing = root.join("existing");
    fs::create_dir(&existing).unwrap();
    fs::write(existing.join("keep"), b"preserve").unwrap();
    assert_eq!(
        extract_archive(&bytes, ArchiveSelection::Protoc(lock.protoc()), &existing)
            .unwrap_err()
            .kind,
        ArchiveErrorKind::UnsafeDestination
    );
    symlink(&existing, root.join("alias")).unwrap();
    assert_eq!(
        extract_archive(
            &bytes,
            ArchiveSelection::Protoc(lock.protoc()),
            &root.join("alias/stage")
        )
        .unwrap_err()
        .kind,
        ArchiveErrorKind::UnsafeDestination
    );
    assert_eq!(fs::read(existing.join("keep")).unwrap(), b"preserve");
    assert!(!existing.join("stage").exists());
    fs::remove_dir_all(root).unwrap();
}
#[test]
fn expansion_limits_are_enforced() {
    let mut compressed = GzEncoder::new(Vec::new(), Compression::fast());
    std::io::copy(
        &mut std::io::repeat(0).take(MAX_EXTRACTED_BYTES + 1),
        &mut compressed,
    )
    .unwrap();
    let bytes = compressed.finish().unwrap();
    assert!(bytes.len() < MAX_COMPRESSED_BYTES);
    assert_eq!(reject(&bytes, true), ArchiveErrorKind::ExtractedLimit);
    let mut writer = zip::ZipWriter::new(Cursor::new(Vec::new()));
    writer
        .start_file(
            "bin/protoc",
            SimpleFileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated)
                .unix_permissions(0o755),
        )
        .unwrap();
    std::io::copy(
        &mut std::io::repeat(0).take(MAX_EXTRACTED_BYTES + 1),
        &mut writer,
    )
    .unwrap();
    let bytes = writer.finish().unwrap().into_inner();
    assert_eq!(reject(&bytes, false), ArchiveErrorKind::ExtractedLimit);
}
#[test]
fn archive_file_child() {
    let Some(root) = std::env::var_os("ASURA_ARCHIVE_TEST_ROOT") else {
        assert!(parse_lock(include_bytes!("fixtures/lock.json")).is_ok());
        return;
    };
    assert!(std::net::TcpListener::bind("127.0.0.1:0").is_err());
    let root = Path::new(&root);
    let bytes = fs::read(root.join("archive.zip")).unwrap();
    let lock = parse_lock(&fs::read(root.join("lock.json")).unwrap()).unwrap();
    let output = root.join("stage");
    if std::env::var_os("ASURA_ARCHIVE_REJECT").is_some() {
        assert!(extract_archive(&bytes, ArchiveSelection::Protoc(lock.protoc()), &output).is_err());
        assert!(!output.exists());
    } else {
        let result =
            extract_archive(&bytes, ArchiveSelection::Protoc(lock.protoc()), &output).unwrap();
        assert_eq!(result.file_count(), 1);
        assert_eq!(
            fs::read(output.join("bin/protoc")).unwrap(),
            b"file-workflow"
        );
    }
}
#[test]
fn file_workflow_under_network_denial() {
    for rejected in [false, true] {
        let root = scratch();
        let bytes = zip_fixture(&[(
            if rejected { "../outside" } else { "bin/protoc" },
            b"file-workflow",
            0o755,
        )]);
        fs::write(root.join("archive.zip"), &bytes).unwrap();
        fs::write(root.join("lock.json"), lock_bytes(&bytes, false)).unwrap();
        let mut command = Command::new("/usr/bin/sandbox-exec");
        command
            .args(["-p", "(version 1)(allow default)(deny network*)"])
            .arg(std::env::current_exe().unwrap())
            .args(["--exact", "archive_file_child", "--nocapture"])
            .env("ASURA_ARCHIVE_TEST_ROOT", &root)
            .env_remove("ASURA_ARCHIVE_REJECT");
        if rejected {
            command.env("ASURA_ARCHIVE_REJECT", "1");
        }
        let mut child = command.spawn().unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                assert!(status.success());
                break;
            }
            assert!(Instant::now() < deadline, "driver must settle test group");
            thread::sleep(Duration::from_millis(10));
        }
        fs::remove_dir_all(root).unwrap();
    }
}

#[test]
fn empty_swift_tree_and_incomplete_terminator_are_rejected() {
    assert_eq!(
        reject(&tar_fixture(&[]), true),
        ArchiveErrorKind::InvalidMember
    );
    let bytes = tar_fixture(&[("swift-protobuf-1.38.1/a", b'0', b"x")]);
    let mut raw = Vec::new();
    flate2::read::GzDecoder::new(&bytes[..])
        .read_to_end(&mut raw)
        .unwrap();
    raw.truncate(1536); // Header, padded one-byte payload, only one zero record.
    assert_eq!(reject(&gzip(&raw), true), ArchiveErrorKind::InvalidArchive);
}

#[test]
fn zip_directory_crc_and_local_name_are_validated() {
    let bytes = zip_fixture(&[("bin/", b"", 0o755), ("bin/protoc", b"x", 0o755)]);
    let central = bytes.windows(4).position(|w| w == b"PK\x01\x02").unwrap();
    let mut bad_crc = bytes.clone();
    bad_crc[central + 16] ^= 1;
    assert_eq!(reject(&bad_crc, false), ArchiveErrorKind::InvalidArchive);
    let mut bad_local = bytes;
    bad_local[30] = b'x';
    assert_eq!(reject(&bad_local, false), ArchiveErrorKind::InvalidArchive);
}
