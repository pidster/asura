//! LC-U7 and LC-U8 (llamacpp-backend.md) in private scratch homes.
use super::*;
use std::os::unix::fs::DirBuilderExt;

struct Scratch(PathBuf);
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
fn scratch(label: &str) -> (Scratch, Paths) {
    let root = PathBuf::from(format!(
        "/private/tmp/asura-llama-{label}-{:x}",
        u128::from_ne_bytes(asura_platform::random_id())
    ));
    fs::DirBuilder::new().mode(0o700).create(&root).unwrap();
    let runtime = asura_platform::RuntimeDirectory::scratch(&root, true).unwrap();
    let paths = Paths::of(&runtime);
    (Scratch(root), paths)
}
fn file(dir: &Path, name: &str, bytes: &[u8]) -> PathBuf {
    let path = dir.join(name);
    fs::write(&path, bytes).unwrap();
    path
}

#[test]
fn paths_follow_the_asura_home() {
    let (root, paths) = scratch("paths");
    assert_eq!(paths.models, root.0.join(".asura/data/models/llamacpp"));
    assert_eq!(paths.data, root.0.join(".asura/data/llamacpp"));
    assert_eq!(paths.logs, root.0.join(".asura/logs"));
}

#[test]
fn names_follow_the_router_rule() {
    for name in ["Ternary-Bonsai-2-2B-Q2_0", "a", "m.v1_x"] {
        assert!(valid_name(name), "{name}");
    }
    for name in ["", ".hidden", "with space", "a/b", "é", &"a".repeat(65)] {
        assert!(!valid_name(name), "{name}");
    }
}

#[test]
fn add_links_gguf_files_and_refuses_the_rest() {
    let (root, paths) = scratch("add");
    let source = root.0.join("source");
    fs::create_dir(&source).unwrap();
    let model = file(&source, "Model-A.gguf", b"GGUF\x03\x00\x00\x00rest");
    assert_eq!(add(&paths, &model), Ok("Model-A".into()));
    let link = paths.models.join("Model-A.gguf");
    assert!(
        fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert_eq!(
        fs::read_link(&link).unwrap(),
        fs::canonicalize(&model).unwrap()
    );
    assert_eq!(
        fs::read(&model).unwrap().len(),
        12,
        "the file is never moved"
    );
    assert_eq!(add(&paths, &model), Err(AddError::Exists));
    let other = root.0.join("other");
    fs::create_dir(&other).unwrap();
    let clash = file(&other, "Model-A.gguf", b"GGUFxx");
    assert_eq!(add(&paths, &clash), Err(AddError::Exists));
    assert_eq!(
        add(&paths, &file(&source, "not.bin", b"GGUF")),
        Err(AddError::Name)
    );
    assert_eq!(
        add(&paths, &file(&source, ".x.gguf", b"GGUF")),
        Err(AddError::Name)
    );
    assert_eq!(
        add(&paths, &file(&source, "fake.gguf", b"ZZZZ")),
        Err(AddError::NotGguf)
    );
    assert_eq!(
        add(&paths, &file(&source, "short.gguf", b"GG")),
        Err(AddError::NotGguf)
    );
    assert_eq!(
        add(&paths, &source.join("missing.gguf")),
        Err(AddError::NotRegular)
    );
    fs::create_dir(source.join("dir.gguf")).unwrap();
    assert_eq!(
        add(&paths, &source.join("dir.gguf")),
        Err(AddError::NotRegular)
    );
    // The 64-link bound.
    for i in 1..MAX_LINKS {
        let path = file(&source, &format!("m{i}.gguf"), b"GGUF");
        add(&paths, &path).unwrap();
    }
    assert_eq!(list(&paths).unwrap().len(), MAX_LINKS);
    let extra = file(&source, "extra.gguf", b"GGUF");
    assert_eq!(add(&paths, &extra), Err(AddError::Full));
}

#[test]
fn list_reports_broken_links_and_remove_touches_links_only() {
    let (root, paths) = scratch("list");
    assert!(
        list(&paths).unwrap().is_empty(),
        "a missing folder is empty"
    );
    let model = file(&root.0, "Keep.gguf", b"GGUF1234");
    let gone = file(&root.0, "Gone.gguf", b"GGUF");
    add(&paths, &model).unwrap();
    add(&paths, &gone).unwrap();
    fs::remove_file(&gone).unwrap();
    // A regular file in the folder is not a link Asura made.
    file(&paths.models, "Planted.gguf", b"GGUF");
    let links = list(&paths).unwrap();
    assert_eq!(
        links
            .iter()
            .map(|l| (l.name.as_str(), l.size))
            .collect::<Vec<_>>(),
        [("Gone", None), ("Keep", Some(8))]
    );
    assert_eq!(
        remove(&paths, "Keep").unwrap(),
        Some(fs::canonicalize(&model).unwrap())
    );
    assert!(model.exists(), "removing a link keeps its file");
    assert!(remove(&paths, "Planted").is_err());
    assert!(paths.models.join("Planted.gguf").exists());
    assert!(remove(&paths, "../x").is_err());
    assert_eq!(
        remove(&paths, "Keep").unwrap_err().kind(),
        io::ErrorKind::NotFound
    );
}

#[test]
fn approval_records_and_checks_the_binary() {
    let (root, paths) = scratch("approve");
    assert_eq!(check(&paths).unwrap(), ApprovalState::Unset);
    // A copy of a system Mach-O executable in the scratch home.
    let binary = root.0.join("llama-server");
    fs::copy("/bin/ls", &binary).unwrap();
    fs::set_permissions(&binary, fs::Permissions::from_mode(0o755)).unwrap();
    let link = root.0.join("link-to-server");
    std::os::unix::fs::symlink(&binary, &link).unwrap();
    let (approval, previous) = approve(&paths, &link).unwrap();
    assert!(previous.is_none());
    assert_eq!(approval.path, link);
    assert_eq!(approval.real_path, fs::canonicalize(&binary).unwrap());
    let mode = fs::metadata(paths.data.join("server.json"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);
    assert_eq!(record(&paths).unwrap(), Some(approval.clone()));
    assert_eq!(
        check(&paths).unwrap(),
        ApprovalState::Approved(approval.clone())
    );
    // An upgrade changes the digest: refused until approved again.
    let mut bytes = fs::read(&binary).unwrap();
    bytes.push(0);
    fs::write(&binary, &bytes).unwrap();
    let ApprovalState::Changed {
        record: old,
        current,
    } = check(&paths).unwrap()
    else {
        panic!("a changed binary is not approved");
    };
    assert_eq!(old, approval);
    assert_ne!(current.sha256, approval.sha256);
    let (again, replaced) = approve(&paths, &link).unwrap();
    assert_eq!(replaced, Some(approval));
    assert_eq!(
        check(&paths).unwrap(),
        ApprovalState::Approved(again.clone())
    );
    fs::remove_file(&binary).unwrap();
    assert_eq!(check(&paths).unwrap(), ApprovalState::Missing(again));
}

#[test]
fn approval_refuses_what_is_not_a_private_mach_o_executable() {
    let (root, paths) = scratch("refuse");
    let script = file(&root.0, "script", b"#!/bin/sh\necho hi\n");
    fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(approve(&paths, &script), Err(ApprovalError::NotMachO));
    let plain = root.0.join("plain");
    fs::copy("/bin/ls", &plain).unwrap();
    fs::set_permissions(&plain, fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(approve(&paths, &plain), Err(ApprovalError::NotExecutable));
    fs::set_permissions(&plain, fs::Permissions::from_mode(0o777)).unwrap();
    assert_eq!(approve(&paths, &plain), Err(ApprovalError::Ownership));
    assert_eq!(
        approve(&paths, &root.0.join("absent")),
        Err(ApprovalError::NotExecutable)
    );
    assert!(record(&paths).unwrap().is_none(), "nothing is recorded");
}

#[test]
fn the_log_moves_aside_past_its_bound() {
    let (_root, paths) = scratch("log");
    let mut log = open_log(&paths).unwrap();
    log.write_all(&vec![b'x'; (LOG_ROTATE_BYTES + 1) as usize])
        .unwrap();
    drop(log);
    let mut log = open_log(&paths).unwrap();
    log.write_all(b"new\n").unwrap();
    assert_eq!(fs::read(paths.logs.join("llamacpp.log")).unwrap(), b"new\n");
    assert_eq!(
        fs::metadata(paths.logs.join("llamacpp.log.1"))
            .unwrap()
            .len(),
        LOG_ROTATE_BYTES + 1
    );
}
