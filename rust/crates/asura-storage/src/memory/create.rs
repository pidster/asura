//! HM3 identity preparation. Authority comes from the committed typed intent.
use super::types::*;
use crate::authority::conversation::{CreateNoteIdentity, MemoryCreateIntent};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MemoryCreateResult {
    Committed(Receipt),
    NotCommitted,
}

pub fn prepare_create(
    binding: Binding,
    project: Id,
    turn: [u8; 16],
    generation: u64,
    ordinal: u32,
    body: String,
    source_version: Option<Id>,
) -> Result<(MemoryCreateIntent, PutNote)> {
    if body.is_empty() || body.len() > MAX_BODY {
        return Err(Error::InvalidInput);
    }
    let ids = CreateNoteIdentity::derive(
        [
            binding.installation_id.bytes(),
            binding.graph_id.bytes(),
            binding.init_operation_id.bytes(),
        ],
        turn,
        generation,
        ordinal,
    )
    .map_err(|_| Error::InvalidInput)?;
    let command = PutNote {
        binding,
        context_id: project,
        object_id: Id::new(ids.object)?,
        version_id: Id::new(ids.version)?,
        operation_id: Id::new(ids.operation)?,
        body,
        source: source_version.map(|source| (source, Id::new(ids.edge).expect("derived nonzero"))),
    };
    let intent = MemoryCreateIntent {
        operation: turn,
        generation,
        ordinal,
        project: project.bytes(),
        memory_operation: ids.operation,
        object: ids.object,
        version: ids.version,
        body_bytes: command.body.len() as u32,
        body_sha256: digest(command.body.as_bytes()),
        source: command
            .source
            .map(|(source, edge)| (source.bytes(), edge.bytes())),
        command_sha256: command.command_digest()?,
    };
    Ok((intent, command))
}

pub(crate) fn command_for_intent(
    binding: Binding,
    intent: &MemoryCreateIntent,
    body: String,
) -> Result<PutNote> {
    let (actual, command) = prepare_create(
        binding,
        Id::new(intent.project)?,
        intent.operation,
        intent.generation,
        intent.ordinal,
        body,
        intent
            .source
            .map(|(source, _)| Id::new(source))
            .transpose()?,
    )?;
    if actual != *intent {
        return Err(Error::IdempotencyConflict);
    }
    Ok(command)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn binding() -> Binding {
        Binding {
            installation_id: Id::new([1; 16]).unwrap(),
            graph_id: Id::new([2; 16]).unwrap(),
            init_operation_id: Id::new([3; 16]).unwrap(),
        }
    }
    #[test]
    fn identities_are_stable_but_changed_body_cannot_reuse_intent() {
        let (a, command) = prepare_create(
            binding(),
            Id::new([4; 16]).unwrap(),
            [5; 16],
            1,
            1,
            "first".into(),
            None,
        )
        .unwrap();
        let (b, _) = prepare_create(
            binding(),
            Id::new([4; 16]).unwrap(),
            [5; 16],
            1,
            1,
            "second".into(),
            None,
        )
        .unwrap();
        assert_eq!(a.memory_operation, b.memory_operation);
        assert_eq!(a.version, b.version);
        assert_ne!(a.command_sha256, b.command_sha256);
        assert_eq!(
            command_for_intent(binding(), &a, "first".into()).unwrap(),
            command
        );
        assert_eq!(
            command_for_intent(binding(), &a, "second".into()),
            Err(Error::IdempotencyConflict)
        );
        assert!(
            prepare_create(
                binding(),
                Id::new([4; 16]).unwrap(),
                [5; 16],
                1,
                1,
                String::new(),
                None
            )
            .is_err()
        );
    }
}

#[cfg(test)]
mod database_tests {
    use super::super::database::Database;
    use super::*;
    use std::{
        fs,
        os::unix::fs::DirBuilderExt,
        time::{Duration, Instant},
    };
    struct Scratch(std::path::PathBuf);
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    #[test]
    fn immutable_create_receipt_checks_link_and_detects_partial_evidence() {
        let scratch = Scratch(
            format!(
                "/private/tmp/asura-memory-create-{:x}",
                u128::from_ne_bytes(asura_platform::random_id())
            )
            .into(),
        );
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&scratch.0)
            .unwrap();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .max_blocking_threads(2)
            .thread_stack_size(10 * 1024 * 1024)
            .enable_time()
            .build()
            .unwrap();
        runtime.block_on(async {
            let binding = Binding {
                installation_id: Id::new([1; 16]).unwrap(),
                graph_id: Id::new([2; 16]).unwrap(),
                init_operation_id: Id::new([3; 16]).unwrap(),
            };
            let project = Id::new([4; 16]).unwrap();
            let database = Database::connect(
                &scratch.0,
                binding.clone(),
                true,
                Instant::now() + Duration::from_secs(10),
            )
            .await
            .unwrap();
            let (source_intent, source) = prepare_create(
                binding.clone(),
                project,
                [5; 16],
                1,
                1,
                "source".into(),
                None,
            )
            .unwrap();
            assert_eq!(
                database.resolve_create(&source_intent).await.unwrap(),
                MemoryCreateResult::NotCommitted
            );
            database.put(source.clone()).await.unwrap();
            let (intent, note) = prepare_create(
                binding.clone(),
                project,
                [5; 16],
                1,
                2,
                "derived".into(),
                Some(source.version_id),
            )
            .unwrap();
            let receipt = database.put(note.clone()).await.unwrap();
            assert_eq!(database.put(note.clone()).await.unwrap(), receipt);
            assert_eq!(
                database.resolve_create(&intent).await.unwrap(),
                MemoryCreateResult::Committed(receipt.clone())
            );
            let mut altered = note.clone();
            altered.body = "changed".into();
            assert_eq!(database.put(altered).await, Err(Error::IdempotencyConflict));
            assert_eq!(
                database
                    .list(ListNotes {
                        binding: binding.clone(),
                        context_id: project,
                        after: None,
                        limit: 16
                    })
                    .await
                    .unwrap()
                    .notes
                    .len(),
                2
            );
            // Missing receipt plus an orphan note cannot establish absence.
            database
                .db
                .query("DELETE $receipt;")
                .bind((
                    "receipt",
                    surrealdb::types::RecordId::new("memory_receipt", source.operation_id.hex()),
                ))
                .await
                .unwrap()
                .check()
                .unwrap();
            assert_eq!(
                database.resolve_create(&source_intent).await,
                Err(Error::InvalidRecord)
            );
            // A receipt without the exact note is never a committed result.
            database
                .db
                .query("DELETE $note;")
                .bind((
                    "note",
                    surrealdb::types::RecordId::new("memory_note", note.version_id.hex()),
                ))
                .await
                .unwrap()
                .check()
                .unwrap();
            assert_eq!(
                database.resolve_create(&intent).await,
                Err(Error::InvalidRecord)
            );
            drop(database);
        });
        let end = Instant::now() + Duration::from_secs(10);
        while runtime.metrics().num_alive_tasks() != 0 {
            assert!(Instant::now() < end, "database shutdown did not settle");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}
