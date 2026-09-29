//! Fixed queries and strict record decoding. No query text crosses the public API.
use super::types::*;
use std::{
    cell::Cell,
    fs,
    os::unix::fs::{MetadataExt, PermissionsExt},
    path::{Component, Path, PathBuf},
    time::{Duration, Instant},
};
use surrealdb::{
    Surreal,
    engine::local::{Db, SurrealKv},
    opt::{Config, capabilities::Capabilities},
    types::{Object, RecordId, SurrealValue, Value},
};

const SCHEMA: &str = include_str!("schema.surql");
pub(crate) struct Database {
    pub(super) db: Surreal<Db>,
    pub(super) binding: Binding,
    deadline: Cell<Instant>,
}
fn record(table: &str, id: Id) -> RecordId {
    RecordId::new(table, id.hex())
}
pub(super) fn marker(binding: &Binding) -> Object {
    let mut value = Object::new();
    value.insert("id", RecordId::new("graph_marker", "installation"));
    value.insert("schema_version", 1i64);
    value.insert("installation_id", binding.installation_id.hex());
    value.insert("graph_id", binding.graph_id.hex());
    value.insert("init_operation_id", binding.init_operation_id.hex());
    value.insert("binding_generation", 1i64);
    value.insert("schema_digest", hex(&digest(SCHEMA.as_bytes())));
    value
}
/// HM0 permits only explicit private fixtures below /private/tmp, not user homes.
pub(crate) fn validate_root_spelling(root: &Path) -> Result<()> {
    if !root.is_absolute()
        || root.parent() != Some(Path::new("/private/tmp"))
        || !root
            .file_name()
            .and_then(|s| s.to_str())
            .is_some_and(|s| s.starts_with("asura-memory-"))
        || root
            .components()
            .any(|c| !matches!(c, Component::RootDir | Component::Normal(_)))
    {
        return Err(Error::UnsafePath);
    }
    Ok(())
}
fn path(root: &Path, initialize: bool) -> Result<PathBuf> {
    validate_root_spelling(root)?;
    let metadata = fs::symlink_metadata(root).map_err(|_| Error::UnsafePath)?;
    if !metadata.is_dir() || metadata.permissions().mode() & 0o777 != 0o700 {
        return Err(Error::UnsafePath);
    }
    let home = root.join(".asura");
    let db = home.join("db");
    for part in [&home, &db] {
        match fs::symlink_metadata(part) {
            Ok(meta)
                if meta.is_dir()
                    && meta.uid() == metadata.uid()
                    && meta.permissions().mode() & 0o777 == 0o700 => {}
            Ok(_) => return Err(Error::UnsafePath),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && initialize => {
                use std::os::unix::fs::DirBuilderExt;
                fs::DirBuilder::new()
                    .mode(0o700)
                    .create(part)
                    .map_err(|_| Error::Unavailable)?;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(Error::MissingDatabase);
            }
            Err(_) => return Err(Error::Unavailable),
        }
    }
    if !initialize
        && fs::read_dir(&db)
            .map_err(|_| Error::Unavailable)?
            .next()
            .is_none()
    {
        return Err(Error::MissingDatabase);
    }
    Ok(db)
}
impl Database {
    pub(crate) async fn connect(
        root: &Path,
        binding: Binding,
        initialize: bool,
        deadline: Instant,
    ) -> Result<Self> {
        let path = path(root, initialize)?;
        Self::connect_path(path, binding, initialize, deadline).await
    }
    pub(crate) async fn connect_path(
        path: PathBuf,
        binding: Binding,
        initialize: bool,
        deadline: Instant,
    ) -> Result<Self> {
        let empty = fs::read_dir(&path)
            .map_err(|_| Error::Unavailable)?
            .next()
            .is_none();
        if empty && !initialize {
            return Err(Error::MissingDatabase);
        }
        if !empty {
            existing_layout(&path)?;
        }
        let config = Config::new()
            .query_timeout(Duration::from_secs(2))
            .transaction_timeout(Duration::from_secs(2))
            .capabilities(Capabilities::none());
        let db = Surreal::new::<SurrealKv>((path, config))
            .sync("every")
            .with_capacity(1)
            .await
            .map_err(|_| Error::Unavailable)?;
        db.use_ns("asura")
            .use_db("memory")
            .await
            .map_err(|_| Error::Unavailable)?;
        let adapter = Self {
            db,
            binding,
            deadline: Cell::new(deadline),
        };
        adapter.active()?;
        if initialize && empty {
            let sql = format!(
                "BEGIN TRANSACTION;\n{SCHEMA}\nCREATE graph_marker:installation CONTENT $marker;\nCOMMIT TRANSACTION;"
            );
            adapter
                .db
                .query(sql)
                .bind(("marker", marker(&adapter.binding)))
                .await
                .map_err(|_| Error::OutcomeUnconfirmed)?
                .check()
                .map_err(|_| Error::OutcomeUnconfirmed)?;
        }
        initialization_verification(initialize && empty, adapter.verify().await)?;
        Ok(adapter)
    }
    pub(crate) fn set_deadline(&self, deadline: Instant) {
        self.deadline.set(deadline);
    }
    pub(super) fn active(&self) -> Result<()> {
        if Instant::now() >= self.deadline.get() {
            Err(Error::Timeout)
        } else {
            Ok(())
        }
    }
    pub(crate) async fn verify(&self) -> Result<()> {
        self.active()?;
        let mut result = self
            .db
            .query("SELECT * FROM graph_marker:installation;")
            .await
            .map_err(|_| Error::Unavailable)?
            .check()
            .map_err(|_| Error::BindingMismatch)?;
        let rows: Vec<Object> = result.take(0).map_err(|_| Error::InvalidRecord)?;
        if rows != vec![marker(&self.binding)] {
            return Err(Error::BindingMismatch);
        }
        Ok(())
    }
    fn check_binding(&self, binding: &Binding) -> Result<()> {
        if binding == &self.binding {
            Ok(())
        } else {
            Err(Error::BindingMismatch)
        }
    }
    pub(crate) async fn put(&self, command: PutNote) -> Result<Receipt> {
        self.put_cancellable(command, &std::sync::atomic::AtomicBool::new(false))
            .await
    }
    pub(crate) async fn put_cancellable(
        &self,
        command: PutNote,
        cancel: &std::sync::atomic::AtomicBool,
    ) -> Result<Receipt> {
        self.check_binding(&command.binding)?;
        let command_digest = command.command_digest()?;
        self.verify().await?;
        match self.resolve(command.operation_id, command_digest).await {
            Ok(receipt) => return Ok(receipt),
            Err(Error::NotFound) => (),
            Err(error) => return Err(error),
        }
        let mut note = Object::new();
        note.insert("schema_version", 1i64);
        note.insert("installation_id", self.binding.installation_id.hex());
        note.insert("graph_id", self.binding.graph_id.hex());
        note.insert("binding_generation", 1i64);
        note.insert("context_id", command.context_id.hex());
        note.insert("object_id", command.object_id.hex());
        note.insert("operation_id", command.operation_id.hex());
        note.insert("body", command.body.clone());
        note.insert("body_sha256", hex(&digest(command.body.as_bytes())));
        let mut receipt = Object::new();
        receipt.insert("schema_version", 1i64);
        receipt.insert("installation_id", self.binding.installation_id.hex());
        receipt.insert("command_sha256", hex(&command_digest));
        receipt.insert("version_id", command.version_id.hex());
        receipt.insert("edge_id", command.source.map(|(_, edge)| edge.hex()));
        let mut edge = Object::new();
        edge.insert("schema_version", 1i64);
        edge.insert("installation_id", self.binding.installation_id.hex());
        edge.insert("context_id", command.context_id.hex());
        edge.insert("operation_id", command.operation_id.hex());
        edge.insert("kind", "derived_from");
        self.active()?;
        let sql = if command.source.is_some() {
            PUT_LINK
        } else {
            PUT_NOTE
        };
        let mut query = self
            .db
            .query(sql)
            .bind(("expected_marker", marker(&self.binding)))
            .bind(("note_id", record("memory_note", command.version_id)))
            .bind(("note", note))
            .bind(("receipt_id", record("memory_receipt", command.operation_id)))
            .bind(("receipt", receipt));
        if let Some((source, id)) = command.source {
            query = query
                .bind(("source_id", record("memory_note", source)))
                .bind(("edge_id", record("memory_link", id)))
                .bind(("edge", edge));
        }
        if cancel.load(std::sync::atomic::Ordering::Acquire) {
            return Err(Error::Cancelled);
        }
        self.active()?;
        query
            .await
            .map_err(|_| Error::OutcomeUnconfirmed)?
            .check()
            .map_err(|_| Error::OutcomeUnconfirmed)?;
        confirmed_receipt(self.resolve(command.operation_id, command_digest).await)
    }
    pub(crate) async fn read(&self, context: Id, query: ReadNotes) -> Result<ReadResult> {
        query.validate()?;
        match query {
            ReadNotes::Get(version) => self
                .get(self.binding.clone(), context, version)
                .await
                .map(ReadResult::Note),
            ReadNotes::Sources(version) => self
                .sources(self.binding.clone(), context, version)
                .await
                .map(ReadResult::Sources),
            ReadNotes::List { after, limit } => self
                .list(ListNotes {
                    binding: self.binding.clone(),
                    context_id: context,
                    after,
                    limit,
                })
                .await
                .map(ReadResult::Page),
        }
    }
    pub(crate) async fn list(&self, command: ListNotes) -> Result<NotePage> {
        command.validate()?;
        self.check_binding(&command.binding)?;
        self.verify().await?;
        self.active()?;
        let after = command
            .after
            .map(|id| record("memory_note", id))
            .unwrap_or_else(|| RecordId::new("memory_note", ""));
        let mut response = self.db.query(
            "SELECT * FROM memory_note WHERE installation_id = $installation AND graph_id = $graph AND context_id = $context AND id > $after ORDER BY id LIMIT $limit;"
        )
            .bind(("installation", command.binding.installation_id.hex()))
            .bind(("graph", command.binding.graph_id.hex()))
            .bind(("context", command.context_id.hex()))
            .bind(("after", after))
            .bind(("limit", i64::from(command.limit) + 1))
            .await.map_err(|_| Error::Unavailable)?
            .check().map_err(|_| Error::Unavailable)?;
        let rows: Vec<Object> = response.take(0).map_err(|_| Error::InvalidRecord)?;
        if rows.len() > usize::from(command.limit) + 1 {
            return Err(Error::InvalidRecord);
        }
        let more = rows.len() > usize::from(command.limit);
        let mut notes = Vec::with_capacity(rows.len().min(usize::from(command.limit)));
        let mut previous = command.after.map(Id::bytes);
        for (index, row) in rows.into_iter().enumerate() {
            let version = record_id(
                row.get("id")
                    .ok_or(Error::InvalidRecord)?
                    .clone()
                    .into_t::<RecordId>()
                    .map_err(|_| Error::InvalidRecord)?,
                "memory_note",
            )?;
            if previous.is_some_and(|id| version.bytes() <= id) {
                return Err(Error::InvalidRecord);
            }
            previous = Some(version.bytes());
            let note = decode_note(row, &command.binding, command.context_id, version)?;
            if index < usize::from(command.limit) {
                notes.push(NoteSummary::from(note));
            }
        }
        let next = if more {
            notes.last().map(|note| note.version_id)
        } else {
            None
        };
        Ok(NotePage { notes, next })
    }
    pub(crate) async fn get(&self, binding: Binding, context: Id, version: Id) -> Result<Note> {
        self.check_binding(&binding)?;
        self.verify().await?;
        self.active()?;
        let mut rows = self
            .db
            .query("SELECT * FROM $note_id;")
            .bind(("note_id", record("memory_note", version)))
            .await
            .map_err(|_| Error::Unavailable)?
            .check()
            .map_err(|_| Error::Unavailable)?;
        let mut objects: Vec<Object> = rows.take(0).map_err(|_| Error::InvalidRecord)?;
        if objects.is_empty() {
            return Err(Error::NotFound);
        }
        if objects.len() != 1 {
            return Err(Error::InvalidRecord);
        }
        decode_note(objects.remove(0), &self.binding, context, version)
    }
    pub(crate) async fn sources(
        &self,
        binding: Binding,
        context: Id,
        version: Id,
    ) -> Result<Vec<Note>> {
        self.get(binding.clone(), context, version).await?;
        self.active()?;
        let mut rows = self
            .db
            .query("SELECT * FROM memory_link WHERE in = $note_id LIMIT 33;")
            .bind(("note_id", record("memory_note", version)))
            .await
            .map_err(|_| Error::Unavailable)?
            .check()
            .map_err(|_| Error::Unavailable)?;
        let edges: Vec<Object> = rows.take(0).map_err(|_| Error::InvalidRecord)?;
        if edges.len() > 32 {
            return Err(Error::LimitExceeded);
        }
        let mut result = Vec::new();
        let mut bytes = 0usize;
        for mut edge in edges {
            exact_one(&mut edge, "schema_version")?;
            equal_string(&mut edge, "installation_id", &binding.installation_id.hex())?;
            equal_string(&mut edge, "context_id", &context.hex())?;
            equal_string(&mut edge, "kind", "derived_from")?;
            let _: Id = Id::parse(&take::<String>(&mut edge, "operation_id")?)?;
            let edge_id: RecordId = take(&mut edge, "id")?;
            record_id(edge_id, "memory_link")?;
            let input: RecordId = take(&mut edge, "in")?;
            if input != record("memory_note", version) {
                return Err(Error::InvalidRecord);
            }
            let output: RecordId = take(&mut edge, "out")?;
            let source = record_id(output, "memory_note")?;
            if !edge.is_empty() {
                return Err(Error::InvalidRecord);
            }
            let note = self.get(binding.clone(), context, source).await?;
            bytes = bytes
                .checked_add(note.body.len() + 256)
                .ok_or(Error::LimitExceeded)?;
            if bytes > 64 * 1024 {
                return Err(Error::LimitExceeded);
            }
            result.push(note);
        }
        Ok(result)
    }
    /// Resolve an immutable create only after the serialized mutation has settled.
    /// A receipt is evidence only when its full note and edge agree with the intent.
    pub(crate) async fn resolve_create(
        &self,
        intent: &crate::authority::conversation::MemoryCreateIntent,
    ) -> Result<super::MemoryCreateResult> {
        use super::MemoryCreateResult;
        let operation = Id::new(intent.memory_operation)?;
        let receipt = match self.resolve(operation, intent.command_sha256).await {
            Ok(value) => value,
            Err(Error::NotFound) => {
                // Missing receipt is absence only if no partial effect exists.
                // These identities are immutable and unique to this intent.
                self.active()?;
                let version = Id::new(intent.version)?;
                let mut rows = self.db.query("SELECT id FROM $note_id; SELECT id FROM memory_link WHERE operation_id = $operation OR in = $note_id LIMIT 1;")
                    .bind(("note_id", record("memory_note", version)))
                    .bind(("operation", operation.hex()))
                    .await.map_err(|_| Error::Unavailable)?.check().map_err(|_| Error::Unavailable)?;
                let notes: Vec<Object> = rows.take(0).map_err(|_| Error::InvalidRecord)?;
                let edges: Vec<Object> = rows.take(1).map_err(|_| Error::InvalidRecord)?;
                if !notes.is_empty() || !edges.is_empty() {
                    return Err(Error::InvalidRecord);
                }
                return Ok(MemoryCreateResult::NotCommitted);
            }
            Err(error) => return Err(error),
        };
        let version = Id::new(intent.version)?;
        let context = Id::new(intent.project)?;
        if receipt.version_id != version
            || receipt.edge_id.map(Id::bytes) != intent.source.map(|(_, edge)| edge)
        {
            return Err(Error::InvalidRecord);
        }
        let note = self
            .get(self.binding.clone(), context, version)
            .await
            .map_err(|e| {
                if e == Error::NotFound {
                    Error::InvalidRecord
                } else {
                    e
                }
            })?;
        if note.object_id.bytes() != intent.object
            || note.operation_id != operation
            || note.body.len() != intent.body_bytes as usize
            || note.body_sha256 != intent.body_sha256
        {
            return Err(Error::InvalidRecord);
        }
        super::create::command_for_intent(self.binding.clone(), intent, note.body)?;
        self.active()?;
        let mut rows = self
            .db
            .query("SELECT * FROM memory_link WHERE in = $note_id LIMIT 2;")
            .bind(("note_id", record("memory_note", version)))
            .await
            .map_err(|_| Error::Unavailable)?
            .check()
            .map_err(|_| Error::Unavailable)?;
        let mut edges: Vec<Object> = rows.take(0).map_err(|_| Error::InvalidRecord)?;
        if edges.len() != usize::from(intent.source.is_some()) {
            return Err(Error::InvalidRecord);
        }
        if let Some((source, expected_edge)) = intent.source {
            let source = Id::new(source)?;
            let mut edge = edges.remove(0);
            exact_one(&mut edge, "schema_version")?;
            equal_string(
                &mut edge,
                "installation_id",
                &self.binding.installation_id.hex(),
            )?;
            equal_string(&mut edge, "context_id", &context.hex())?;
            equal_string(&mut edge, "operation_id", &operation.hex())?;
            equal_string(&mut edge, "kind", "derived_from")?;
            if take::<RecordId>(&mut edge, "id")? != record("memory_link", Id::new(expected_edge)?)
                || take::<RecordId>(&mut edge, "in")? != record("memory_note", version)
                || take::<RecordId>(&mut edge, "out")? != record("memory_note", source)
                || !edge.is_empty()
            {
                return Err(Error::InvalidRecord);
            }
            self.get(self.binding.clone(), context, source)
                .await
                .map_err(|e| {
                    if e == Error::NotFound {
                        Error::InvalidRecord
                    } else {
                        e
                    }
                })?;
        }
        Ok(MemoryCreateResult::Committed(receipt))
    }
    pub(crate) async fn resolve(&self, operation: Id, hash: [u8; 32]) -> Result<Receipt> {
        self.verify().await?;
        self.active()?;
        let mut rows = self
            .db
            .query("SELECT * FROM $receipt_id;")
            .bind(("receipt_id", record("memory_receipt", operation)))
            .await
            .map_err(|_| Error::Unavailable)?
            .check()
            .map_err(|_| Error::Unavailable)?;
        let mut objects: Vec<Object> = rows.take(0).map_err(|_| Error::InvalidRecord)?;
        if objects.is_empty() {
            return Err(Error::NotFound);
        }
        if objects.len() != 1 {
            return Err(Error::InvalidRecord);
        }
        let mut value = objects.remove(0);
        let id: RecordId = take(&mut value, "id")?;
        if id != record("memory_receipt", operation) {
            return Err(Error::InvalidRecord);
        }
        exact_one(&mut value, "schema_version")?;
        equal_string(
            &mut value,
            "installation_id",
            &self.binding.installation_id.hex(),
        )?;
        let command_sha256 = parse_hex::<32>(&take::<String>(&mut value, "command_sha256")?)?;
        if command_sha256 != hash {
            return Err(Error::IdempotencyConflict);
        }
        let version_id = Id::parse(&take::<String>(&mut value, "version_id")?)?;
        let edge_id = match value.remove("edge_id") {
            None | Some(Value::None) => None,
            Some(v) => Some(Id::parse(
                &v.into_t::<String>().map_err(|_| Error::InvalidRecord)?,
            )?),
        };
        if !value.is_empty() {
            return Err(Error::InvalidRecord);
        }
        Ok(Receipt {
            operation_id: operation,
            command_sha256,
            version_id,
            edge_id,
        })
    }
}
const PUT_NOTE: &str = r#"
BEGIN TRANSACTION;
LET $marker = SELECT * FROM ONLY graph_marker:installation;
IF $marker != $expected_marker { THROW 'binding_mismatch'; };
CREATE $note_id CONTENT $note;
CREATE $receipt_id CONTENT $receipt;
COMMIT TRANSACTION;
"#;
const PUT_LINK: &str = r#"
BEGIN TRANSACTION;
LET $marker = SELECT * FROM ONLY graph_marker:installation;
IF $marker != $expected_marker { THROW 'binding_mismatch'; };
LET $source = SELECT * FROM ONLY $source_id;
IF $source = NONE OR $source.context_id != $note.context_id OR $source.installation_id != $note.installation_id OR $source.graph_id != $note.graph_id { THROW 'source_mismatch'; };
CREATE $note_id CONTENT $note;
RELATE $note_id->$edge_id->$source_id CONTENT $edge;
CREATE $receipt_id CONTENT $receipt;
COMMIT TRANSACTION;
"#;
fn take<T: SurrealValue>(value: &mut Object, key: &str) -> Result<T> {
    value
        .remove(key)
        .ok_or(Error::InvalidRecord)?
        .into_t()
        .map_err(|_| Error::InvalidRecord)
}
fn equal_string(value: &mut Object, key: &str, expected: &str) -> Result<()> {
    if take::<String>(value, key)? == expected {
        Ok(())
    } else {
        Err(Error::InvalidRecord)
    }
}
fn exact_one(value: &mut Object, key: &str) -> Result<()> {
    if take::<i64>(value, key)? == 1 {
        Ok(())
    } else {
        Err(Error::InvalidRecord)
    }
}
fn record_id(value: RecordId, table: &str) -> Result<Id> {
    if value.table.as_str() != table {
        return Err(Error::InvalidRecord);
    }
    // Record keys in this schema are always plain strings, never generated IDs or arrays.
    let surrealdb::types::RecordIdKey::String(text) = value.key else {
        return Err(Error::InvalidRecord);
    };
    Id::parse(&text)
}
fn decode_note(mut value: Object, binding: &Binding, context: Id, version: Id) -> Result<Note> {
    if take::<RecordId>(&mut value, "id")? != record("memory_note", version) {
        return Err(Error::InvalidRecord);
    }
    exact_one(&mut value, "schema_version")?;
    exact_one(&mut value, "binding_generation")?;
    equal_string(
        &mut value,
        "installation_id",
        &binding.installation_id.hex(),
    )?;
    equal_string(&mut value, "graph_id", &binding.graph_id.hex())?;
    let context_id = Id::parse(&take::<String>(&mut value, "context_id")?)?;
    if context_id != context {
        return Err(Error::NotFound);
    }
    let object_id = Id::parse(&take::<String>(&mut value, "object_id")?)?;
    let operation_id = Id::parse(&take::<String>(&mut value, "operation_id")?)?;
    let body: String = take(&mut value, "body")?;
    let body_sha256 = parse_hex::<32>(&take::<String>(&mut value, "body_sha256")?)?;
    if body.len() > MAX_BODY || digest(body.as_bytes()) != body_sha256 || !value.is_empty() {
        return Err(Error::InvalidRecord);
    }
    Ok(Note {
        binding: binding.clone(),
        context_id,
        object_id,
        version_id: version,
        operation_id,
        body,
        body_sha256,
    })
}

fn initialization_verification(submitted: bool, result: Result<()>) -> Result<()> {
    if submitted {
        result.map_err(|_| Error::OutcomeUnconfirmed)
    } else {
        result
    }
}
fn confirmed_receipt(result: Result<Receipt>) -> Result<Receipt> {
    result.map_err(|_| Error::OutcomeUnconfirmed)
}
fn existing_layout(path: &Path) -> Result<()> {
    use std::io::Read;
    let owner = fs::symlink_metadata(path)
        .map_err(|_| Error::UnsafePath)?
        .uid();
    for name in ["manifest", "wal", "sstables", "vlog"] {
        let meta = fs::symlink_metadata(path.join(name)).map_err(|_| Error::MissingDatabase)?;
        if !meta.is_dir() || meta.uid() != owner {
            return Err(Error::UnsafePath);
        }
    }
    for name in ["LOCK", "manifest/00000000000000000000.manifest"] {
        let meta = fs::symlink_metadata(path.join(name)).map_err(|_| Error::MissingDatabase)?;
        if !meta.is_file() || meta.uid() != owner || meta.nlink() != 1 {
            return Err(Error::UnsafePath);
        }
    }
    let mut manifest = fs::File::open(path.join("manifest/00000000000000000000.manifest"))
        .map_err(|_| Error::Unavailable)?;
    let mut header = [0u8; 26];
    manifest
        .read_exact(&mut header)
        .map_err(|_| Error::InvalidRecord)?;
    if header[..2] != [0, 1] {
        return Err(Error::InvalidRecord);
    }
    Ok(())
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn initialization_verification_preserves_uncertainty_only_after_submission() {
        for error in [Error::Timeout, Error::Unavailable, Error::BindingMismatch] {
            assert_eq!(
                initialization_verification(true, Err(error)),
                Err(Error::OutcomeUnconfirmed)
            );
            assert_eq!(initialization_verification(false, Err(error)), Err(error));
        }
        assert_eq!(initialization_verification(true, Ok(())), Ok(()));
    }
    #[test]
    fn every_failure_after_commit_is_an_uncertain_outcome() {
        for error in [
            Error::Timeout,
            Error::Unavailable,
            Error::BindingMismatch,
            Error::InvalidRecord,
            Error::NotFound,
        ] {
            assert_eq!(
                confirmed_receipt(Err(error)),
                Err(Error::OutcomeUnconfirmed)
            );
        }
    }
}
