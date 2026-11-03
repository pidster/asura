//! LC-U9 (llamacpp-backend.md): the server plan, the environment filter, the
//! key pipe, the agent file and the `launchctl` calls with a fake runner.
use super::*;
use std::cell::RefCell;
use std::io::Read;

fn plan() -> ServerPlan {
    ServerPlan {
        binary: "/opt/homebrew/bin/llama-server".into(),
        models: "/Users/p/.asura/data/models/llamacpp".into(),
        port: 8934,
        context_length: 65_536,
        slots: 1,
        models_loaded: 2,
        idle_seconds: 600,
    }
}

#[test]
fn arguments_follow_the_settings() {
    let args: Vec<String> = plan()
        .arguments(5)
        .into_iter()
        .map(|a| a.into_string().unwrap())
        .collect();
    assert_eq!(
        args.join(" "),
        "--models-dir /Users/p/.asura/data/models/llamacpp --models-max 2 --ctx-size 65536 \
         --parallel 1 --sleep-idle-seconds 600 --no-ui --host 127.0.0.1 --port 8934 \
         --api-key-file /dev/fd/5 --cors-origins asura-no-browser-origin \
         --no-cors-credentials --cors-methods GET --cors-headers Content-Type"
    );
    let never = ServerPlan {
        idle_seconds: 0,
        ..plan()
    };
    let args: Vec<_> = never.arguments(3);
    assert!(!args.iter().any(|a| a == "--sleep-idle-seconds"));
    assert!(
        !args.iter().any(|a| a == "--api-key"),
        "the key is never an argument"
    );
}

/// LCD16: CORS is closed whatever the settings. The origin is not a valid
/// serialised origin (no scheme), so no page's origin can equal it; it is
/// not `*`, `null` or `localhost`, which let pages read replies.
#[test]
fn every_plan_closes_cors() {
    for plan in [
        plan(),
        ServerPlan {
            idle_seconds: 0,
            port: 1024,
            slots: 16,
            ..plan()
        },
    ] {
        let args: Vec<String> = plan
            .arguments(3)
            .into_iter()
            .map(|a| a.into_string().unwrap())
            .collect();
        let value = |flag: &str| {
            let at = args.iter().position(|a| a == flag).unwrap();
            assert_eq!(args.iter().filter(|a| *a == flag).count(), 1, "{flag} once");
            args[at + 1].clone()
        };
        let origin = value("--cors-origins");
        assert!(!origin.contains("://") && !origin.contains(','), "{origin}");
        assert!(!["*", "null", "localhost", ""].contains(&origin.as_str()));
        assert_eq!(value("--cors-methods"), "GET");
        assert_eq!(value("--cors-headers"), "Content-Type");
        assert!(args.iter().any(|a| a == "--no-cors-credentials"));
        assert!(!args.iter().any(|a| a == "--cors-credentials"));
    }
}

/// LCD6: inherited `LLAMA_*` variables never reach the server; the start's
/// own key is the one `LLAMA_*` variable it receives, once.
#[test]
fn llama_variables_never_reach_the_server_except_the_start_key() {
    let key = new_key();
    let kept = environment(
        [
            ("PATH".into(), "/usr/bin".into()),
            // An inherited key would let another key open the server.
            ("LLAMA_API_KEY".into(), "inherited".into()),
            ("LLAMA_ARG_HOST".into(), "0.0.0.0".into()),
            ("LLAMA_ARG_PORT".into(), "1".into()),
            // Would reopen CORS on the router and its model processes (LCD16).
            ("LLAMA_ARG_CORS_ORIGINS".into(), "*".into()),
            ("LLAMA_ARG_CORS_CREDENTIALS".into(), "1".into()),
            ("HOME".into(), "/Users/p".into()),
        ],
        &key,
    )
    .unwrap();
    let pairs: Vec<(&str, &str)> = kept
        .iter()
        .map(|(n, v)| (n.to_str().unwrap(), v.to_str().unwrap()))
        .collect();
    assert_eq!(
        pairs,
        [
            ("PATH", "/usr/bin"),
            ("HOME", "/Users/p"),
            (KEY_VARIABLE, std::str::from_utf8(&key).unwrap()),
        ]
    );
    assert_eq!(KEY_VARIABLE, "LLAMA_API_KEY");
}

#[test]
fn a_malformed_key_never_reaches_the_environment() {
    for bad in [&b""[..], b"short", &[b'g'; 64][..], &[b'a'; 65][..]] {
        assert!(environment([("PATH".into(), "/usr/bin".into())], bad).is_err());
    }
}

/// The key is never an argument: the plan names only the pipe (LCD6), and
/// `--no-ui` is always present; the router passes it on as `--no-webui`
/// (P25).
#[test]
fn the_plan_keeps_the_ui_off_and_the_key_out_of_the_arguments() {
    let key = new_key();
    for plan in [
        plan(),
        ServerPlan {
            idle_seconds: 0,
            ..plan()
        },
    ] {
        let args: Vec<String> = plan
            .arguments(9)
            .into_iter()
            .map(|a| a.into_string().unwrap())
            .collect();
        assert_eq!(args.iter().filter(|a| *a == "--no-ui").count(), 1);
        assert!(!args.iter().any(|a| a == "--ui" || a == "--webui"));
        assert!(!args.iter().any(|a| a == "--api-key"));
        assert!(
            !args
                .iter()
                .any(|a| a.contains(std::str::from_utf8(&key).unwrap()))
        );
        let at = args.iter().position(|a| a == "--api-key-file").unwrap();
        assert_eq!(args[at + 1], "/dev/fd/9");
    }
}

#[test]
fn the_key_pipe_holds_the_key_and_refuses_other_shapes() {
    let key = new_key();
    assert_eq!(key.len(), KEY_HEX_BYTES);
    assert!(
        key.iter()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    );
    assert_ne!(*key, *new_key());
    let read = key_pipe(&key).unwrap();
    let mut text = Vec::new();
    File::from(read).read_to_end(&mut text).unwrap();
    assert_eq!(&text[..64], &key[..]);
    assert_eq!(text[64], b'\n');
    for bad in [&b"short"[..], &[b'g'; 64][..], &[b'a'; 65][..]] {
        assert!(key_pipe(bad).is_err());
    }
}

#[test]
fn the_agent_file_restarts_after_a_crash_only() {
    let text = agent_plist(Path::new("/Applications/Asura.app/Contents/MacOS/asura")).unwrap();
    for needle in [
        "<string>org.pidster.asura.llamacpp</string>",
        "<string>/Applications/Asura.app/Contents/MacOS/asura</string>",
        "<string>llamacpp</string>",
        "<string>serve</string>",
        "<key>SuccessfulExit</key>\n\t\t<false/>",
        "<key>RunAtLoad</key>\n\t<true/>",
        "<integer>30</integer>",
    ] {
        assert!(text.contains(needle), "{needle}");
    }
    assert!(!text.contains("EnvironmentVariables"));
    assert!(agent_plist(Path::new("relative/asura")).is_err());
    assert!(agent_plist(Path::new("/a<b>/asura")).is_err());
    assert_eq!(
        agent_path(Path::new("/Users/p")),
        PathBuf::from("/Users/p/Library/LaunchAgents/org.pidster.asura.llamacpp.plist")
    );
}

/// A scripted `launchctl`: `loaded` answers `print`; every call is kept.
struct Fake {
    loaded: RefCell<bool>,
    calls: RefCell<Vec<String>>,
    fail: Option<&'static str>,
}
impl Fake {
    fn new(loaded: bool) -> Self {
        Self {
            loaded: RefCell::new(loaded),
            calls: RefCell::default(),
            fail: None,
        }
    }
}
impl Launchctl for Fake {
    fn run(&self, args: &[&OsStr]) -> io::Result<(i32, String)> {
        let line = args
            .iter()
            .map(|a| a.to_str().unwrap())
            .collect::<Vec<_>>()
            .join(" ");
        self.calls.borrow_mut().push(line.clone());
        if self.fail.is_some_and(|f| line.starts_with(f)) {
            return Ok((5, String::new()));
        }
        match args[0].to_str().unwrap() {
            "print" if *self.loaded.borrow() => Ok((
                0,
                "gui/501/org.pidster.asura.llamacpp = {\n\tstate = running\n\tpid = 4242\n\tlast exit code = 0\n}\n".into(),
            )),
            "print" => Ok((113, String::new())),
            "bootstrap" => {
                *self.loaded.borrow_mut() = true;
                Ok((0, String::new()))
            }
            "bootout" => {
                *self.loaded.borrow_mut() = false;
                Ok((0, String::new()))
            }
            _ => Ok((0, String::new())),
        }
    }
}

fn scratch_home() -> PathBuf {
    let path = PathBuf::from(format!(
        "/private/tmp/asura-agent-{:x}",
        u128::from_ne_bytes(crate::random_id())
    ));
    std::fs::create_dir(&path).unwrap();
    path
}

#[test]
fn start_registers_once_then_restarts_and_stop_removes() {
    let home = scratch_home();
    let fake = Fake::new(false);
    let program = Path::new("/Applications/Asura.app/Contents/MacOS/asura");
    assert_eq!(agent_state(&fake, 501).unwrap(), AgentState::NotLoaded);
    assert_eq!(
        start(&fake, 501, &home, program).unwrap(),
        Started::Registered
    );
    let file = agent_path(&home);
    assert!(std::fs::read_to_string(&file).unwrap().contains("llamacpp"));
    assert_eq!(
        agent_state(&fake, 501).unwrap(),
        AgentState::Loaded {
            pid: Some(4242),
            last_exit: Some(0)
        }
    );
    assert_eq!(
        start(&fake, 501, &home, program).unwrap(),
        Started::Restarted
    );
    assert!(stop(&fake, 501, &home).unwrap());
    assert!(!file.exists());
    assert!(!stop(&fake, 501, &home).unwrap(), "nothing left to stop");
    let calls = fake.calls.borrow().clone();
    assert_eq!(
        calls
            .iter()
            .filter(|c| !c.starts_with("print"))
            .cloned()
            .collect::<Vec<_>>(),
        [
            format!("bootstrap gui/501 {}", file.display()),
            "kickstart -k gui/501/org.pidster.asura.llamacpp".into(),
            "bootout gui/501/org.pidster.asura.llamacpp".into(),
        ]
    );
    std::fs::remove_dir_all(&home).unwrap();
}

#[test]
fn launchctl_failures_are_reported() {
    let home = scratch_home();
    let mut fake = Fake::new(false);
    fake.fail = Some("bootstrap");
    let program = Path::new("/Applications/Asura.app/Contents/MacOS/asura");
    assert!(start(&fake, 501, &home, program).is_err());
    let mut loaded = Fake::new(true);
    loaded.fail = Some("kickstart");
    assert!(restart(&loaded, 501).is_err());
    std::fs::remove_dir_all(&home).unwrap();
}
