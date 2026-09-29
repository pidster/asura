#![cfg(feature = "embedded-memory")]
//! HM1 exercises real embedded storage and the bound authority owner, in scratch roots.
use asura_storage::memory::{
    Binding, Error, Id, ListNotes, Memory, Pending, PutNote, ReadNotes, ReadResult,
};
use std::{
    fs,
    os::unix::fs::DirBuilderExt,
    path::PathBuf,
    time::{Duration, Instant},
};
static OWNER: std::sync::Mutex<()> = std::sync::Mutex::new(());
fn id(n: u8) -> Id {
    Id::new([n; 16]).unwrap()
}
fn binding() -> Binding {
    Binding {
        installation_id: id(1),
        graph_id: id(2),
        init_operation_id: id(3),
    }
}
fn root(prefix: &str) -> PathBuf {
    let root = PathBuf::from(format!(
        "/private/tmp/{prefix}-{:x}",
        u128::from_ne_bytes(asura_platform::random_id())
    ));
    fs::DirBuilder::new().mode(0o700).create(&root).unwrap();
    root
}
fn wait<T>(mut pending: Pending<T>) -> asura_storage::memory::Result<T> {
    let deadline = Instant::now() + Duration::from_secs(12);
    loop {
        if let Some(result) = pending.poll() {
            return result;
        }
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(2));
    }
}
fn close(memory: &mut Memory) {
    let deadline = Instant::now() + Duration::from_secs(12);
    loop {
        match memory.close() {
            Ok(true) => break,
            Ok(false) | Err(Error::Timeout) => (),
            Err(error) => panic!("close failed: {error}"),
        }
        assert!(Instant::now() < deadline, "unsettled fixture retained");
        std::thread::sleep(Duration::from_millis(2));
    }
}
#[test]
fn pages_preserve_scope_literal_previews_sources_and_replay_after_restart() {
    let _owner = OWNER.lock().unwrap();
    let root = root("asura-memory-notes");
    let (mut memory, ready) = Memory::initialize(root.clone(), binding()).unwrap();
    wait(ready).unwrap();
    let mut original = None;
    for n in [13, 10, 12, 11] {
        let command = PutNote {
            binding: binding(),
            context_id: id(4),
            object_id: id(n + 20),
            version_id: id(n),
            operation_id: id(n + 40),
            body: "€".repeat(100),
            source: (n == 11).then_some((id(10), id(90))),
        };
        let receipt = wait(memory.put_note(command.clone()).unwrap()).unwrap();
        if n == 10 {
            original = Some((command, receipt));
        }
    }
    let list = |after| ListNotes {
        binding: binding(),
        context_id: id(4),
        after,
        limit: 2,
    };
    let first = wait(memory.list_notes(list(None)).unwrap()).unwrap();
    assert_eq!(
        first.notes.iter().map(|n| n.version_id).collect::<Vec<_>>(),
        vec![id(10), id(11)]
    );
    assert_eq!(first.next, Some(id(11)));
    assert_eq!(first.notes[0].preview, "€".repeat(85));
    let second = wait(memory.list_notes(list(first.next)).unwrap()).unwrap();
    assert_eq!(
        second
            .notes
            .iter()
            .map(|n| n.version_id)
            .collect::<Vec<_>>(),
        vec![id(12), id(13)]
    );
    assert_eq!(second.next, None);
    assert!(
        wait(memory.list_notes(list(Some(id(99)))).unwrap())
            .unwrap()
            .notes
            .is_empty()
    );
    let mut other = list(None);
    other.context_id = id(5);
    assert!(
        wait(memory.list_notes(other).unwrap())
            .unwrap()
            .notes
            .is_empty()
    );
    let mut wrong = list(None);
    wrong.binding.graph_id = id(6);
    assert_eq!(
        wait(memory.list_notes(wrong).unwrap()),
        Err(Error::BindingMismatch)
    );
    for limit in [0, 33, 255] {
        let mut invalid = list(None);
        invalid.limit = limit;
        assert!(matches!(
            memory.list_notes(invalid),
            Err(Error::InvalidInput)
        ));
    }
    let cancelled = memory.list_notes(list(None)).unwrap();
    cancelled.cancel();
    assert_eq!(wait(cancelled), Err(Error::Cancelled));
    assert_eq!(
        wait(memory.get_sources(binding(), id(4), id(11)).unwrap()).unwrap()[0].version_id,
        id(10)
    );
    assert_eq!(
        wait(memory.get_sources(binding(), id(5), id(11)).unwrap()),
        Err(Error::NotFound)
    );
    close(&mut memory);
    drop(memory);
    let (mut memory, ready) = Memory::open(root.clone(), binding()).unwrap();
    wait(ready).unwrap();
    assert_eq!(wait(memory.list_notes(list(None)).unwrap()).unwrap(), first);
    let (command, receipt) = original.unwrap();
    assert_eq!(
        wait(memory.put_note(command.clone()).unwrap()).unwrap(),
        receipt
    );
    assert_eq!(
        wait(memory.get_note(binding(), id(4), id(10)).unwrap())
            .unwrap()
            .body,
        command.body
    );
    close(&mut memory);
    drop(memory);
    fs::remove_dir_all(root).unwrap();
}
#[test]
fn bound_authority_reads_only_registered_project_without_journal_mutation() {
    use asura_storage::authority::writer::{
        Command, Error as WriterError, Reply, Ticket, WriterHandle,
    };
    fn receive(mut ticket: Ticket) -> Result<Reply, WriterError> {
        let deadline = Instant::now() + Duration::from_secs(12);
        loop {
            if let Some(result) = ticket.poll() {
                return result;
            }
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(2));
        }
    }
    let _owner = OWNER.lock().unwrap();
    let root = root("asura-writer-memory");
    let runtime = asura_platform::RuntimeDirectory::scratch(&root, true).unwrap();
    let mut writer = WriterHandle::start(runtime).unwrap();
    receive(writer.try_submit(Command::Open).unwrap()).unwrap();
    receive(
        writer
            .try_submit(Command::Initialize { request: [1; 16] })
            .unwrap(),
    )
    .unwrap();
    let registered = receive(
        writer
            .try_submit(Command::Register {
                request: [2; 16],
                location: root.to_str().unwrap().into(),
            })
            .unwrap(),
    )
    .unwrap();
    let project = *registered
        .replay
        .as_ref()
        .unwrap()
        .projects
        .keys()
        .next()
        .unwrap();
    drop(registered);
    let journal = root.join(".asura/state/control/slot-0.log");
    let before = fs::read(&journal).unwrap();
    let reply = receive(
        writer
            .try_submit(Command::MemoryRead {
                project,
                query: ReadNotes::List {
                    after: None,
                    limit: 32,
                },
            })
            .unwrap(),
    )
    .unwrap();
    assert!(
        matches!(reply.memory_result, Some(Ok(ReadResult::Page(page))) if page.notes.is_empty() && page.next.is_none())
    );
    let reply = receive(
        writer
            .try_submit(Command::MemoryRead {
                project,
                query: ReadNotes::Get(id(50)),
            })
            .unwrap(),
    )
    .unwrap();
    assert_eq!(reply.memory_result, Some(Err(Error::NotFound)));
    assert!(matches!(
        receive(
            writer
                .try_submit(Command::MemoryRead {
                    project: [99; 16],
                    query: ReadNotes::Get(id(50))
                })
                .unwrap()
        ),
        Err(WriterError::Invalid)
    ));
    assert!(matches!(
        writer.try_submit(Command::MemoryRead {
            project,
            query: ReadNotes::List {
                after: None,
                limit: 0
            }
        }),
        Err(WriterError::Invalid)
    ));
    assert_eq!(before, fs::read(&journal).unwrap());
    writer.close();
    let deadline = Instant::now() + Duration::from_secs(12);
    while !writer.settled() {
        assert!(Instant::now() < deadline, "unsettled fixture retained");
        std::thread::sleep(Duration::from_millis(2));
    }
    drop(writer);
    fs::remove_dir_all(root).unwrap();
}
