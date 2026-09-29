use super::*;
use std::io::Write;
use std::time::Duration;

fn pair() -> (StartupChild, File) {
    let (read, write) = pipe().unwrap();
    nonblocking(read.as_raw_fd()).unwrap();
    (
        StartupChild {
            pid: 0,
            reader: File::from(read),
            bytes: Vec::new(),
            notice: None,
            reaped: None,
        },
        File::from(write),
    )
}
#[test]
fn exact_notice_cases_and_rejections() {
    for notice in [
        StartupNotice::Bound,
        StartupNotice::OwnerBusy,
        StartupNotice::UnsafeRuntime,
        StartupNotice::Unavailable,
        StartupNotice::InternalFailure,
    ] {
        assert_eq!(StartupNotice::decode(&notice.encode()).unwrap(), notice);
    }
    for bytes in [
        b"ASST\x02\x01\0\0".as_slice(),
        b"ASST\x01\x01\0\x01",
        b"ASST\x01\x04\0\0",
        b"ASST\x01\x03\x01\0",
        b"ASST",
        b"ASST\x01\x01\0\0x",
    ] {
        assert!(matches!(
            StartupNotice::decode(bytes),
            Err(Error::InvalidNotice)
        ));
    }
}
#[test]
fn real_pipe_partial_progress_requires_eof() {
    let (mut child, mut write) = pair();
    let end = Instant::now() + Duration::from_secs(1);
    let bytes = StartupNotice::Bound.encode();
    write.write_all(&bytes[..3]).unwrap();
    assert_eq!(child.poll_notice(end).unwrap(), None);
    write.write_all(&bytes[3..]).unwrap();
    assert_eq!(child.poll_notice(end).unwrap(), None);
    drop(write);
    assert_eq!(child.poll_notice(end).unwrap(), Some(StartupNotice::Bound));
}
#[test]
fn pipe_early_eof_extra_bytes_and_deadline_fail() {
    let (mut child, mut write) = pair();
    write.write_all(b"ASS").unwrap();
    drop(write);
    assert!(matches!(
        child.poll_notice(Instant::now() + Duration::from_secs(1)),
        Err(Error::InvalidNotice)
    ));
    let (mut child, mut write) = pair();
    write.write_all(b"ASST\x01\x01\0\0x").unwrap();
    assert!(matches!(
        child.poll_notice(Instant::now() + Duration::from_secs(1)),
        Err(Error::InvalidNotice)
    ));
    let (mut child, _write) = pair();
    assert!(matches!(
        child.poll_notice(Instant::now()),
        Err(Error::Deadline)
    ));
}
#[test]
fn writable_pipe_is_required_and_notice_writer_closes() {
    let (read, write) = pipe().unwrap();
    assert!(matches!(
        StartupPipe::validate_descriptor(read.as_raw_fd()),
        Err(Error::InvalidNotice)
    ));
    StartupPipe::validate_descriptor(write.as_raw_fd()).unwrap();
    let null = File::open("/dev/null").unwrap();
    assert!(matches!(
        StartupPipe::validate_descriptor(null.as_raw_fd()),
        Err(Error::InvalidNotice)
    ));
    nonblocking(write.as_raw_fd()).unwrap();
    StartupPipe(write).send(StartupNotice::OwnerBusy).unwrap();
    let mut reader = File::from(read);
    let mut bytes = Vec::new();
    reader.read_to_end(&mut bytes).unwrap();
    assert_eq!(
        StartupNotice::decode(&bytes).unwrap(),
        StartupNotice::OwnerBusy
    );
}

#[test]
fn spawn_actions_keep_notice_and_route_only_child_stderr() {
    spawn_action_case(false);
}
#[test]
fn spawn_actions_inherit_reader_without_any_lease_writer() {
    spawn_action_case(true);
}
fn spawn_action_case(owned: bool) {
    use std::os::unix::fs::DirBuilderExt;
    let directory = std::path::PathBuf::from(format!(
        "/private/tmp/asura-log-spawn-{:x}",
        u128::from_ne_bytes(crate::random_id())
    ));
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&directory)
        .unwrap();
    let log = crate::open_private_append(&directory, "asura.log").unwrap();
    let mut original = log.try_clone().unwrap();
    original.write_all(b"before\n").unwrap();
    let (read, write) = pipe().unwrap();
    let notice = duplicate_for_spawn(write.as_raw_fd()).unwrap();
    let stderr = duplicate_for_spawn(log.as_raw_fd()).unwrap();
    assert!(notice.as_raw_fd() > 4 && stderr.as_raw_fd() > 4);
    assert_ne!(notice.as_raw_fd(), stderr.as_raw_fd());
    let inherited = duplicate_for_spawn(2).unwrap();
    assert!(inherited.as_raw_fd() > 4);
    // Deliberately inheritable unrelated descriptor: fixed close-on-exec-default must close it.
    // SAFETY: duplicates a valid log descriptor; ownership is taken immediately below.
    let leak = unsafe { libc::fcntl(log.as_raw_fd(), libc::F_DUPFD, 50) };
    assert!(leak >= 50);
    // SAFETY: successful fcntl duplication returns one owned descriptor.
    let leak = unsafe { OwnedFd::from_raw_fd(leak) };
    let (lease, lifetime) = crate::ServiceLifetime::new().unwrap();
    if owned {
        // This fixture deliberately uses a blocking reader so shell read cannot
        // confuse EAGAIN with EOF. The production inherited() owner restores
        // nonblocking before the service uses fd4.
        // SAFETY: mutate only this isolated test pipe's retained reader flags.
        let flags = unsafe { libc::fcntl(lifetime.as_raw_fd(), libc::F_GETFL) };
        assert!(flags >= 0);
        checked(unsafe {
            libc::fcntl(
                lifetime.as_raw_fd(),
                libc::F_SETFL,
                flags & !libc::O_NONBLOCK,
            )
        })
        .unwrap();
    }
    let lifetime_source = owned.then(|| duplicate_for_spawn(lifetime.as_raw_fd()).unwrap());
    if let Some(source) = &lifetime_source {
        assert!(source.as_raw_fd() > 4);
    }
    let actions = startup_actions(&notice, &stderr, lifetime_source.as_ref()).unwrap();
    // SAFETY: spawn attributes initialized before use/destruction.
    let mut attr = unsafe { std::mem::zeroed() };
    spawn_result(unsafe { libc::posix_spawnattr_init(&mut attr) }).unwrap();
    let mut attr = SpawnAttributes(attr);
    // SAFETY: fixed SDK flags; matches production attributes.
    spawn_result(unsafe {
        libc::posix_spawnattr_setflags(
            &mut attr.0,
            (0x0400 | libc::POSIX_SPAWN_CLOEXEC_DEFAULT) as i16,
        )
    })
    .unwrap();
    let lifetime_check = if owned {
        "if read -r life <&4; then exit 13; fi; "
    } else {
        "[ ! -e /dev/fd/4 ] || exit 14; "
    };
    let program = CString::new(format!(r#"{lifetime_check}[ "$PWD" = / ] || exit 10; if read -r line; then exit 11; fi; [ ! -e /dev/fd/{} ] || exit 12; printf child-log >&2; printf stdout-discarded; printf 'ASST\001\001\000\000' >&3"#, leak.as_raw_fd())).unwrap();
    let mut argv = [
        c"/bin/sh".as_ptr().cast_mut(),
        c"-c".as_ptr().cast_mut(),
        program.as_ptr().cast_mut(),
        std::ptr::null_mut(),
    ];
    let mut environment = [
        c"LANG=C".as_ptr().cast_mut(),
        c"LC_ALL=C".as_ptr().cast_mut(),
        std::ptr::null_mut(),
    ];
    let mut pid = 0;
    // SAFETY: fixed test executable/arguments, retained file actions/attributes and terminated arrays.
    spawn_result(unsafe {
        libc::posix_spawn(
            &mut pid,
            c"/bin/sh".as_ptr(),
            &actions.0,
            &attr.0,
            argv.as_mut_ptr(),
            environment.as_mut_ptr(),
        )
    })
    .unwrap();
    drop(lease);
    drop(actions);
    drop(notice);
    drop(write);
    drop(stderr);
    nonblocking(read.as_raw_fd()).unwrap();
    let mut child = StartupChild {
        pid,
        reader: File::from(read),
        bytes: Vec::new(),
        notice: None,
        reaped: None,
    };
    let end = Instant::now() + Duration::from_secs(5);
    let result = loop {
        match child.poll_notice(end) {
            Ok(None) => std::thread::sleep(Duration::from_millis(5)),
            outcome => break outcome,
        }
    };
    let status = loop {
        if let Some(status) = child.try_reap().unwrap() {
            break status;
        }
        if Instant::now() >= end {
            // SAFETY: this unreaped direct child is still retained by the test.
            checked(unsafe { libc::kill(child.pid, libc::SIGKILL) }).unwrap();
            let cleanup_end = Instant::now() + Duration::from_secs(2);
            while child.try_reap().unwrap().is_none() {
                assert!(
                    Instant::now() < cleanup_end,
                    "fixture cleanup unproved; retained evidence at {}",
                    directory.display()
                );
                std::thread::sleep(Duration::from_millis(5));
            }
            panic!(
                "fixed spawn fixture did not exit; child settled; evidence at {}",
                directory.display()
            );
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    assert!(
        libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0,
        "fixture status={status}"
    );
    assert_eq!(result.unwrap(), Some(StartupNotice::Bound));
    assert_eq!(
        std::fs::read(directory.join("asura.log")).unwrap(),
        b"before\nchild-log"
    );
    std::fs::remove_dir_all(directory).unwrap();
}
