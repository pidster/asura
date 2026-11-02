//! LC-U14 (llamacpp-backend.md): arguments and the person commands in a
//! scratch home with a fake `launchctl`; no agent is ever registered.
use super::*;
use std::cell::RefCell;
use std::ffi::OsStr;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};

#[test]
fn arguments_parse() {
    assert_eq!(
        parse(&["approve", "/x"]),
        Some(Command::Approve("/x".into()))
    );
    assert_eq!(
        parse(&["add", "/m.gguf"]),
        Some(Command::Add("/m.gguf".into()))
    );
    assert_eq!(parse(&["remove", "m"]), Some(Command::Remove("m".into())));
    for (words, command) in [
        (["list"], Command::List),
        (["start"], Command::Start),
        (["stop"], Command::Stop),
        (["status"], Command::Status),
        (["serve"], Command::Serve),
    ] {
        assert_eq!(parse(&words), Some(command));
    }
    for words in [
        &[][..],
        &["approve"],
        &["add", "a", "b"],
        &["serve", "--home", "/tmp"],
        &["run"],
    ] {
        assert_eq!(parse(words), None, "{words:?}");
    }
    assert!(!USAGE.contains("serve"), "serve stays hidden");
}

struct Fake {
    loaded: RefCell<bool>,
    calls: RefCell<Vec<String>>,
}
impl Launchctl for Fake {
    fn run(&self, args: &[&OsStr]) -> std::io::Result<(i32, String)> {
        let line: Vec<_> = args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        self.calls.borrow_mut().push(line.join(" "));
        Ok(match line[0].as_str() {
            "print" if *self.loaded.borrow() => (0, "\tpid = 77\n".into()),
            "print" => (113, String::new()),
            "bootstrap" => {
                *self.loaded.borrow_mut() = true;
                (0, String::new())
            }
            "bootout" => {
                *self.loaded.borrow_mut() = false;
                (0, String::new())
            }
            _ => (0, String::new()),
        })
    }
}

struct Scratch(PathBuf);
impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn person_commands_in_a_scratch_home() {
    let root = Scratch(PathBuf::from(format!(
        "/private/tmp/asura-llama-cli-{:x}",
        u128::from_ne_bytes(asura_platform::random_id())
    )));
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&root.0)
        .unwrap();
    let runtime = RuntimeDirectory::scratch(&root.0, true).unwrap();
    let fake = Fake {
        loaded: RefCell::new(false),
        calls: RefCell::default(),
    };
    let program = Path::new("/Applications/Asura.app/Contents/MacOS/asura");
    let call = |command| {
        let mut out = Vec::new();
        let status = run(command, &runtime, 501, program, &fake, &mut out);
        (status, String::from_utf8(out).unwrap())
    };
    // Start needs the mode, an approval and a model.
    let (status, text) = call(Command::Start);
    assert_eq!(status, 1);
    assert!(text.contains("is not asura"), "{text}");
    asura_storage::config::execute(
        runtime.clone(),
        "providers.llamacpp.server",
        Some("asura"),
        &AtomicBool::new(false),
    )
    .unwrap();
    let (status, text) = call(Command::Start);
    assert_eq!(status, 1);
    assert!(text.contains("server_not_approved"), "{text}");
    let binary = root.0.join("llama-server");
    std::fs::copy("/bin/ls", &binary).unwrap();
    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();
    let (status, text) = call(Command::Approve(binary.display().to_string()));
    assert_eq!(status, 0, "{text}");
    assert!(text.starts_with("Approved "), "{text}");
    let (status, text) = call(Command::Start);
    assert_eq!(status, 1);
    assert!(text.contains("no_models"), "{text}");
    let model = root.0.join("Tiny.gguf");
    std::fs::write(&model, b"GGUF....").unwrap();
    let (status, text) = call(Command::Add(model.display().to_string()));
    assert_eq!((status, text.lines().count()), (0, 1), "{text}");
    let (status, text) = call(Command::List);
    assert_eq!((status, text.as_str()), (0, "llamacpp:Tiny  8 bytes\n"));
    let (status, text) = call(Command::Start);
    assert_eq!(status, 0, "{text}");
    let agent = root
        .0
        .join("Library/LaunchAgents/org.pidster.asura.llamacpp.plist");
    assert!(
        std::fs::read_to_string(&agent)
            .unwrap()
            .contains("<string>serve</string>")
    );
    // Adding a model restarts a loaded server, and says so.
    let other = root.0.join("Other.gguf");
    std::fs::write(&other, b"GGUF").unwrap();
    let (_, text) = call(Command::Add(other.display().to_string()));
    assert!(text.contains("Restarting"), "{text}");
    let (status, text) = call(Command::Status);
    assert_eq!(status, 0);
    assert!(
        text.contains("Mode: asura")
            && text.contains("running, pid 77")
            && text.contains("Models: 2"),
        "{text}"
    );
    // An upgrade changes the binary: start refuses with both digests.
    let mut bytes = std::fs::read(&binary).unwrap();
    bytes.push(1);
    std::fs::write(&binary, bytes).unwrap();
    *fake.loaded.borrow_mut() = false;
    let (status, text) = call(Command::Start);
    assert_eq!(status, 1);
    assert!(text.contains("server_changed"), "{text}");
    let (status, text) = call(Command::Remove("Tiny".into()));
    assert_eq!(status, 0);
    assert!(
        model.exists() && text.contains("The file stays at"),
        "{text}"
    );
    let (status, text) = call(Command::Stop);
    assert_eq!(status, 0);
    assert!(!agent.exists(), "{text}");
    let calls = fake.calls.borrow();
    assert!(calls.iter().any(|c| c.starts_with("bootstrap gui/501 ")));
    assert!(
        calls
            .iter()
            .any(|c| c == "kickstart -k gui/501/org.pidster.asura.llamacpp")
    );
}
