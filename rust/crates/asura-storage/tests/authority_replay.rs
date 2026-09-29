use asura_storage::authority::{MAX_FRAME, MAX_JOURNAL, ReplayError, ReplayState, replay};
use sha2::{Digest, Sha256};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const PENDING: &[u8] = include_bytes!("fixtures/pending.bin");
const ACTIVE: &[u8] = include_bytes!("fixtures/active.bin");
fn inspect(bytes: &[u8]) -> Result<asura_storage::authority::Replay, ReplayError> {
    replay(
        bytes,
        Instant::now() + Duration::from_secs(2),
        &AtomicBool::new(false),
    )
}
fn seal(frame: &mut [u8]) {
    let end = frame.len() - 40;
    let mut command = Sha256::new();
    command.update(&frame[10..12]);
    command.update(&frame[144..end]);
    frame[88..120].copy_from_slice(&command.finalize());
    let digest = Sha256::digest(&frame[..end]);
    frame[end..end + 32].copy_from_slice(&digest);
}
fn owner(previous: &[u8], sequence: u64, generation: u64) -> Vec<u8> {
    let mut frame = PENDING[..144].to_vec();
    frame.extend_from_slice(&[0; 32]);
    frame.extend_from_slice(b"ASURAC01");
    frame[10..12].copy_from_slice(&3u16.to_be_bytes());
    frame[12..16].copy_from_slice(&184u32.to_be_bytes());
    frame[16..24].copy_from_slice(&sequence.to_be_bytes());
    frame[24..56].copy_from_slice(&previous[previous.len() - 40..previous.len() - 8]);
    frame[72..88].fill(sequence as u8);
    frame[120..128].copy_from_slice(&(sequence - 1).to_be_bytes());
    frame[128..136].copy_from_slice(&sequence.to_be_bytes());
    frame[136..144].copy_from_slice(&generation.to_be_bytes());
    seal(&mut frame);
    frame
}
#[test]
fn complete_pending_binding_and_historical_owner_replay() {
    let pending = inspect(PENDING).unwrap();
    assert_eq!(pending.state, ReplayState::PendingInit);
    assert_eq!(pending.installation_id, [0x11; 16]);
    assert_eq!(
        (
            pending.revision,
            pending.owner_generation,
            pending.binding_generation
        ),
        (1, 1, None)
    );
    let active = inspect(ACTIVE).unwrap();
    assert_eq!(
        (active.state, active.revision, active.binding_generation),
        (ReplayState::ActiveBinding, 2, Some(1))
    );
    for prefix in [PENDING, ACTIVE] {
        let sequence = if prefix == PENDING { 2 } else { 3 };
        let mut journal = prefix.to_vec();
        journal.extend(owner(prefix, sequence, 2));
        let replay = inspect(&journal).unwrap();
        assert_eq!(replay.owner_generation, 2);
        assert_eq!(replay.revision, sequence);
    }
}
#[test]
fn every_byte_is_integrity_checked_and_each_tail_boundary_classified() {
    for offset in 0..ACTIVE.len() {
        let mut damaged = ACTIVE.to_vec();
        damaged[offset] ^= 0x40;
        assert!(inspect(&damaged).is_err(), "accepted damage at {offset}");
    }
    for end in 0..ACTIVE.len() {
        let result = inspect(&ACTIVE[..end]);
        if end == 0 {
            assert_eq!(result, Err(ReplayError::Empty));
        } else if end == PENDING.len() {
            assert_eq!(result.unwrap().state, ReplayState::PendingInit);
        } else {
            assert_eq!(result, Err(ReplayError::IncompleteTail), "boundary {end}");
        }
    }
}
#[test]
fn valid_hashes_do_not_bypass_field_and_transition_rules() {
    for (offset, value) in [
        (16, 0u64),
        (16, 2),
        (120, 1),
        (128, 0),
        (128, u64::MAX),
        (136, 0),
        (136, 2),
        (145, 0),
    ] {
        let mut bytes = PENDING.to_vec();
        bytes[offset..offset + 8].copy_from_slice(&value.to_be_bytes());
        seal(&mut bytes);
        assert_eq!(
            inspect(&bytes),
            Err(ReplayError::Corrupt),
            "field {offset}={value}"
        );
    }
    for range in [24..56, 56..72, 72..88, 185..201] {
        let mut bytes = PENDING.to_vec();
        if range.start == 24 {
            bytes[range].fill(1);
        } else {
            bytes[range].fill(0);
        }
        seal(&mut bytes);
        assert_eq!(inspect(&bytes), Err(ReplayError::Corrupt));
    }
    for mode in [0, 3, 255] {
        let mut bytes = PENDING.to_vec();
        bytes[144] = mode;
        seal(&mut bytes);
        assert_eq!(inspect(&bytes), Err(ReplayError::Corrupt));
    }
    let mut external = PENDING.to_vec();
    external[144] = 2;
    seal(&mut external);
    assert!(inspect(&external).is_ok());
    for offset in [56, 72, 136, 144, 152, 168] {
        let mut second = ACTIVE[PENDING.len()..].to_vec();
        if offset == 72 {
            second[72..88].fill(1);
        } else {
            second[offset] ^= 1;
        }
        seal(&mut second);
        let mut bytes = PENDING.to_vec();
        bytes.extend(second);
        assert_eq!(
            inspect(&bytes),
            Err(ReplayError::Corrupt),
            "binding offset {offset}"
        );
    }
    let mut wrong_owner = PENDING.to_vec();
    wrong_owner.extend(owner(PENDING, 2, 3));
    assert_eq!(inspect(&wrong_owner), Err(ReplayError::Corrupt));
}
#[test]
fn unsupported_format_and_lengths_limits_and_cancellation() {
    let mut bytes = PENDING.to_vec();
    bytes[8..10].copy_from_slice(&2u16.to_be_bytes());
    assert_eq!(inspect(&bytes), Err(ReplayError::UnsupportedFormat));
    bytes = PENDING.to_vec();
    bytes[10..12].copy_from_slice(&4u16.to_be_bytes());
    seal(&mut bytes);
    assert_eq!(inspect(&bytes), Err(ReplayError::UnsupportedFormat));
    for length in [0, 143, 183] {
        let mut bytes = PENDING.to_vec();
        bytes[12..16].copy_from_slice(&(length as u32).to_be_bytes());
        assert_eq!(inspect(&bytes), Err(ReplayError::Corrupt));
    }
    bytes = PENDING.to_vec();
    bytes[12..16].copy_from_slice(&((MAX_FRAME + 1) as u32).to_be_bytes());
    assert_eq!(inspect(&bytes), Err(ReplayError::Limit));
    assert_eq!(inspect(&vec![0; MAX_JOURNAL + 1]), Err(ReplayError::Limit));
    assert_eq!(
        replay(PENDING, Instant::now(), &AtomicBool::new(false)),
        Err(ReplayError::Timeout)
    );
    assert_eq!(
        replay(
            PENDING,
            Instant::now() + Duration::from_secs(1),
            &AtomicBool::new(true)
        ),
        Err(ReplayError::Cancelled)
    );
}
#[test]
fn crash_writer_fixture() {
    let Some(root) = std::env::var_os("ASURA_REPLAY_CRASH_ROOT") else {
        assert_eq!(inspect(ACTIVE).unwrap().state, ReplayState::ActiveBinding);
        return;
    };
    let root = PathBuf::from(root);
    let cut: usize = std::env::var("ASURA_REPLAY_CRASH_CUT")
        .unwrap()
        .parse()
        .unwrap();
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(root.join("slot.log"))
        .unwrap();
    file.write_all(&ACTIVE[..cut]).unwrap();
    fs::write(root.join("ready"), b"ready").unwrap();
    loop {
        std::thread::park();
    }
}
#[test]
fn real_writer_death_at_every_byte_preserves_residue() {
    let id = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = PathBuf::from(format!(
        "/private/tmp/asura-replay-{}-{id}",
        std::process::id()
    ));
    fs::DirBuilder::new().mode(0o700).create(&root).unwrap();
    let suite_deadline = Instant::now() + Duration::from_secs(55);
    for cut in 0..=ACTIVE.len() {
        assert!(
            Instant::now() < suite_deadline,
            "crash suite deadline; retained {}",
            root.display()
        );
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "crash_writer_fixture", "--nocapture"])
            .env("ASURA_REPLAY_CRASH_ROOT", &root)
            .env("ASURA_REPLAY_CRASH_CUT", cut.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let end = Instant::now() + Duration::from_secs(3);
        let mut ready = false;
        while Instant::now() < end {
            if root.join("ready").exists() {
                ready = true;
                break;
            }
            if child.try_wait().unwrap().is_some() {
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        // The retained direct child is the only signal target. Never signal a saved PID.
        if child.try_wait().unwrap().is_none() {
            child.kill().unwrap();
        }
        let end = Instant::now() + Duration::from_secs(2);
        loop {
            if child.try_wait().unwrap().is_some() {
                break;
            }
            assert!(
                Instant::now() < end,
                "writer settlement unknown; retained {}",
                root.display()
            );
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(
            ready,
            "writer readiness failed at {cut}; retained {}",
            root.display()
        );
        let path = root.join("slot.log");
        let before = fs::read(&path).unwrap();
        assert_eq!(before, ACTIVE[..cut]);
        let before_hash = Sha256::digest(&before);
        let result = inspect(&before);
        match cut {
            0 => assert_eq!(result, Err(ReplayError::Empty)),
            n if n == PENDING.len() || n == ACTIVE.len() => assert!(result.is_ok()),
            _ => assert_eq!(result, Err(ReplayError::IncompleteTail)),
        }
        assert_eq!(before_hash, Sha256::digest(fs::read(&path).unwrap()));
        fs::remove_file(path).unwrap();
        fs::remove_file(root.join("ready")).unwrap();
    }
    fs::remove_dir(root).unwrap();
}
