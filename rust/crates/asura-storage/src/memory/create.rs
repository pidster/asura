//! HM3 identity preparation. Authority comes from the committed kind 14
//! `run` record; every identity is derived, never read from the journal (HM3,
//! TCR-D2, TCR-D23).
use super::types::*;
use crate::authority::conversation::CreateNoteCall;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MemoryCreateResult {
    Committed(Receipt),
    NotCommitted,
}

/// Derives the call identity and the put command for one create_note call.
pub fn prepare_create(
    binding: Binding,
    project: Id,
    turn: [u8; 16],
    generation: u64,
    ordinal: u32,
    body: String,
    source_version: Option<Id>,
) -> Result<(CreateNoteCall, PutNote)> {
    if body.is_empty() || body.len() > MAX_BODY {
        return Err(Error::InvalidInput);
    }
    let call = CreateNoteCall::derive(
        [
            binding.installation_id.bytes(),
            binding.graph_id.bytes(),
            binding.init_operation_id.bytes(),
        ],
        project.bytes(),
        turn,
        generation,
        ordinal,
    )
    .map_err(|_| Error::InvalidInput)?;
    let command = command_for_call(binding, &call, body, source_version)?;
    Ok((call, command))
}

/// The put command for a derived call: the derived identities plus the typed
/// body and source of the service's parse. A call derived under another
/// binding is an idempotency conflict.
pub(crate) fn command_for_call(
    binding: Binding,
    call: &CreateNoteCall,
    body: String,
    source_version: Option<Id>,
) -> Result<PutNote> {
    if body.is_empty() || body.len() > MAX_BODY {
        return Err(Error::InvalidInput);
    }
    let expected = CreateNoteCall::derive(
        [
            binding.installation_id.bytes(),
            binding.graph_id.bytes(),
            binding.init_operation_id.bytes(),
        ],
        call.project,
        call.operation,
        call.generation,
        call.ordinal,
    )
    .map_err(|_| Error::InvalidInput)?;
    if expected != *call {
        return Err(Error::IdempotencyConflict);
    }
    let ids = call.identity;
    Ok(PutNote {
        binding,
        context_id: Id::new(call.project)?,
        object_id: Id::new(ids.object)?,
        version_id: Id::new(ids.version)?,
        operation_id: Id::new(ids.operation)?,
        body,
        source: source_version
            .map(|source| Ok::<_, Error>((source, Id::new(ids.edge)?)))
            .transpose()?,
    })
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
        // Identities are derived from the call alone; the body is not part of
        // them, so the journal needs no body digest (TCR-D2).
        assert_eq!(a, b);
        assert_eq!(
            command_for_call(binding(), &a, "first".into(), None).unwrap(),
            command
        );
        let other = Binding {
            graph_id: Id::new([9; 16]).unwrap(),
            ..binding()
        };
        assert_eq!(
            command_for_call(other, &a, "first".into(), None),
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
            let database = Database::connect(&scratch.0, binding.clone(), true)
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
