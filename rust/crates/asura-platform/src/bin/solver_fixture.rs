//! The stalled-solver fixture (security-policy.md, PL3 validation map,
//! SA-T1 to SA-T6). The platform copies it as `MacOS/cvc5` and starts it
//! under the solver profile; it reads its mode from
//! `<own directory>/../Frameworks/fixture.mode`. Test-support only; the
//! standard library only.
//!
//! The fixture defines the C `main` itself (`no_main`): the Rust runtime's
//! start-up installs a stack guard page with calls that the deny-default
//! solver profile refuses, and the fixture must run under the very profile
//! that the platform renders for cvc5.
#![no_main]
use std::io::{Read, Write};

const MIB: usize = 1024 * 1024;

fn mode() -> String {
    let exe = std::env::current_exe().unwrap_or_default();
    let path = exe
        .parent()
        .and_then(|d| d.parent())
        .map(|d| d.join("Frameworks/fixture.mode"))
        .unwrap_or_default();
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .trim()
        .to_owned()
}

/// Answers one line per `(check-sat)` and an empty model per `(get-model)`.
fn answer(sat: bool, huge_model: bool) {
    let mut stdin = std::io::stdin().lock();
    let mut stdout = std::io::stdout().lock();
    let mut pending: Vec<u8> = Vec::new();
    let mut chunk = vec![0u8; 64 * 1024];
    loop {
        let n = match stdin.read(&mut chunk) {
            Ok(0) | Err(_) => return,
            Ok(n) => n,
        };
        pending.extend_from_slice(&chunk[..n]);
        loop {
            let check = find(&pending, b"(check-sat)");
            let model = find(&pending, b"(get-model)");
            let next = match (check, model) {
                (Some(c), Some(m)) if m < c => Some((m, false)),
                (Some(c), _) => Some((c, true)),
                (None, Some(m)) => Some((m, false)),
                (None, None) => None,
            };
            let Some((at, is_check)) = next else {
                let keep = pending.len().saturating_sub(16);
                pending.drain(..keep);
                break;
            };
            pending.drain(..at + 11);
            let reply: Vec<u8> = if is_check {
                if sat {
                    b"sat\n".to_vec()
                } else {
                    b"unsat\n".to_vec()
                }
            } else if huge_model {
                let mut text = b"(\n".to_vec();
                text.extend(std::iter::repeat_n(b'x', MIB + 1024));
                text.extend_from_slice(b"\n)\n");
                text
            } else {
                b"(\n)\n".to_vec()
            };
            if stdout
                .write_all(&reply)
                .and_then(|()| stdout.flush())
                .is_err()
            {
                return;
            }
        }
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn read_forever() {
    let mut stdin = std::io::stdin().lock();
    let mut chunk = vec![0u8; 64 * 1024];
    while let Ok(n) = stdin.read(&mut chunk) {
        if n == 0 {
            // End of input: wait without reading, as a stalled solver does.
            loop {
                std::thread::park();
            }
        }
    }
}

fn flood(out: &mut dyn Write) {
    let line = [b'z'; 63];
    for _ in 0..(MIB / 64) {
        if out
            .write_all(&line)
            .and_then(|()| out.write_all(b"\n"))
            .is_err()
        {
            return;
        }
    }
    let _ = out.flush();
}

/// The C entry point; see the crate note on `no_main`.
#[unsafe(no_mangle)]
pub extern "C" fn main(_argc: i32, _argv: *const *const u8) -> i32 {
    run();
    0
}

fn run() {
    match mode().as_str() {
        "answer-unsat" => answer(false, false),
        "answer-sat" => answer(true, false),
        "huge-model" => answer(true, true),
        "silent" => read_forever(),
        "flood-stdout" => {
            flood(&mut std::io::stdout().lock());
            let mut sink = Vec::new();
            let _ = std::io::stdin().lock().read_to_end(&mut sink);
        }
        "flood-stderr" => {
            flood(&mut std::io::stderr().lock());
            answer(false, false);
        }
        "exit-half" => {
            let mut taken = 0usize;
            let mut chunk = vec![0u8; 64 * 1024];
            let mut stdin = std::io::stdin().lock();
            while taken < 2 * MIB {
                match stdin.read(&mut chunk[..(2 * MIB - taken).min(64 * 1024)]) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => taken += n,
                }
            }
            std::process::exit(3);
        }
        "memory" => {
            let mut blocks: Vec<Vec<u8>> = Vec::new();
            for _ in 0..32 {
                let mut block = vec![0u8; 64 * MIB];
                for i in (0..block.len()).step_by(4096) {
                    block[i] = 1;
                }
                blocks.push(block);
            }
            read_forever();
            drop(blocks);
        }
        _ => std::process::exit(64),
    }
}
