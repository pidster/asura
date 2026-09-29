//! Fixed sensor projection queries on the existing embedded database owner.
use super::{
    database::{Database, marker},
    types::{hex, parse_hex},
};
use crate::sensors::{Error, Id, ProjectState};
use surrealdb::types::{Object, RecordId};
fn record(project: Id) -> RecordId {
    RecordId::new("sensor_state", hex(&project))
}
fn map(error: super::Error) -> Error {
    match error {
        super::Error::OutcomeUnconfirmed => Error::Unconfirmed,
        _ => Error::Unavailable,
    }
}
const SCHEMA: &str = r#"
DEFINE TABLE IF NOT EXISTS sensor_state SCHEMAFULL;
DEFINE FIELD IF NOT EXISTS installation_id ON sensor_state TYPE string;
DEFINE FIELD IF NOT EXISTS graph_id ON sensor_state TYPE string;
DEFINE FIELD IF NOT EXISTS project ON sensor_state TYPE string;
DEFINE FIELD IF NOT EXISTS revision ON sensor_state TYPE int;
DEFINE FIELD IF NOT EXISTS write_id ON sensor_state TYPE string;
DEFINE FIELD IF NOT EXISTS digest ON sensor_state TYPE string;
DEFINE FIELD IF NOT EXISTS body ON sensor_state TYPE string;
"#;
impl Database {
    pub(crate) async fn sensor_load(&self, project: Id) -> Result<ProjectState, Error> {
        if project == [0; 16] {
            return Err(Error::Invalid);
        }
        self.verify().await.map_err(map)?;
        self.active().map_err(map)?;
        // A newly bound graph has no sensor projection table yet. Establish
        // only this additive schema after verifying the installation binding.
        self.db
            .query(SCHEMA)
            .await
            .map_err(|_| Error::Unavailable)?
            .check()
            .map_err(|_| Error::Unavailable)?;
        self.active().map_err(map)?;
        let mut response = self
            .db
            .query("SELECT * FROM $record;")
            .bind(("record", record(project)))
            .await
            .map_err(|_| Error::Unavailable)?
            .check()
            .map_err(|_| Error::Unavailable)?;
        let mut rows: Vec<Object> = response.take(0).map_err(|_| Error::Invalid)?;
        if rows.is_empty() {
            return Ok(ProjectState::empty(project));
        }
        if rows.len() != 1 {
            return Err(Error::Invalid);
        }
        let mut row = rows.remove(0);
        let id = row
            .remove("id")
            .ok_or(Error::Invalid)?
            .into_t::<RecordId>()
            .map_err(|_| Error::Invalid)?;
        if id != record(project) {
            return Err(Error::Invalid);
        }
        let text = |row: &mut Object, key: &str| -> Result<String, Error> {
            row.remove(key)
                .ok_or(Error::Invalid)?
                .into_t::<String>()
                .map_err(|_| Error::Invalid)
        };
        if text(&mut row, "installation_id")? != self.binding.installation_id.hex()
            || text(&mut row, "graph_id")? != self.binding.graph_id.hex()
            || text(&mut row, "project")? != hex(&project)
        {
            return Err(Error::Invalid);
        }
        let revision = row
            .remove("revision")
            .ok_or(Error::Invalid)?
            .into_t::<i64>()
            .map_err(|_| Error::Invalid)?;
        let write_id = parse_hex::<16>(&text(&mut row, "write_id")?).map_err(|_| Error::Invalid)?;
        let digest = parse_hex::<32>(&text(&mut row, "digest")?).map_err(|_| Error::Invalid)?;
        let value = ProjectState::decode(&text(&mut row, "body")?)?;
        if !row.is_empty()
            || revision < 1
            || value.revision != revision as u64
            || value.project != project
            || value.write_id != write_id
            || value.digest()? != digest
        {
            return Err(Error::Invalid);
        }
        Ok(value)
    }
    pub(crate) async fn sensor_store(
        &self,
        expected_revision: u64,
        value: ProjectState,
    ) -> Result<ProjectState, Error> {
        let body = value.encode()?;
        let digest = hex(&value.digest()?);
        if expected_revision.checked_add(1) != Some(value.revision)
            || value.revision > i64::MAX as u64
        {
            return Err(Error::Invalid);
        }
        let current = self.sensor_load(value.project).await?;
        if current.write_id == value.write_id {
            return if current == value {
                Ok(current)
            } else {
                Err(Error::Conflict)
            };
        }
        if current.revision != expected_revision {
            return Err(Error::Stale);
        }
        self.active().map_err(map)?;
        self.db
            .query(SCHEMA)
            .await
            .map_err(|_| Error::Unconfirmed)?
            .check()
            .map_err(|_| Error::Unconfirmed)?;
        let mut row = Object::new();
        row.insert("installation_id", self.binding.installation_id.hex());
        row.insert("graph_id", self.binding.graph_id.hex());
        row.insert("project", hex(&value.project));
        row.insert("revision", value.revision as i64);
        row.insert("write_id", hex(&value.write_id));
        row.insert("digest", digest);
        row.insert("body", body);
        self.active().map_err(map)?;
        self.db
            .query(STORE)
            .bind(("record", record(value.project)))
            .bind(("value", row))
            .bind(("expected", expected_revision as i64))
            .bind(("marker", marker(&self.binding)))
            .await
            .map_err(|_| Error::Unconfirmed)?
            .check()
            .map_err(|_| Error::Unconfirmed)?;
        let confirmed = self
            .sensor_load(value.project)
            .await
            .map_err(|_| Error::Unconfirmed)?;
        if confirmed != value {
            return Err(Error::Unconfirmed);
        }
        Ok(confirmed)
    }
}
const STORE: &str = r#"
BEGIN TRANSACTION;
LET $binding = SELECT * FROM ONLY graph_marker:installation;
IF $binding != $marker { THROW 'sensor_binding_mismatch'; };
LET $old = SELECT * FROM ONLY $record;
IF $old = NONE {
    IF $expected != 0 { THROW 'sensor_revision_mismatch'; };
    CREATE $record CONTENT $value;
} ELSE {
    IF $old.installation_id != $value.installation_id OR $old.graph_id != $value.graph_id { THROW 'sensor_binding_mismatch'; };
    IF $old.write_id = $value.write_id {
        IF $old.digest != $value.digest { THROW 'sensor_identity_conflict'; };
    } ELSE {
        IF $old.revision != $expected { THROW 'sensor_revision_mismatch'; };
        UPDATE $record CONTENT $value;
    };
};
COMMIT TRANSACTION;
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::{Binding, Id as MemoryId};
    use std::{
        os::unix::fs::DirBuilderExt,
        time::{Duration, Instant},
    };
    struct Scratch(std::path::PathBuf);
    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    fn close(database: Database, runtime: &tokio::runtime::Runtime) {
        drop(database);
        let deadline = Instant::now() + Duration::from_secs(10);
        while runtime.metrics().num_alive_tasks() != 0 {
            assert!(Instant::now() < deadline, "sensor DB close did not settle");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    #[test]
    fn real_database_sensor_cas_retry_and_reopen() {
        let _fixture = super::super::owner::TEST_ENGINE.lock().unwrap();
        let root = Scratch(std::path::PathBuf::from(format!(
            "/private/tmp/asura-memory-sensors-{}-{}",
            std::process::id(),
            u128::from_be_bytes(asura_platform::random_id())
        )));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&root.0)
            .unwrap();
        let _claim = super::super::owner::Claim::acquire_validated(&root.0).unwrap();
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .max_blocking_threads(2)
            .thread_stack_size(10 * 1024 * 1024)
            .enable_time()
            .build()
            .unwrap();
        let binding = Binding {
            installation_id: MemoryId::new([1; 16]).unwrap(),
            graph_id: MemoryId::new([2; 16]).unwrap(),
            init_operation_id: MemoryId::new([3; 16]).unwrap(),
        };
        let database = runtime
            .block_on(Database::connect(
                &root.0,
                binding.clone(),
                true,
                Instant::now() + Duration::from_secs(10),
            ))
            .unwrap();
        assert_eq!(
            runtime.block_on(database.sensor_load([4; 16])).unwrap(),
            ProjectState::empty([4; 16])
        );
        let mut value = ProjectState::empty([4; 16]);
        value.revision = 1;
        value.write_id = [5; 16];
        assert_eq!(
            runtime.block_on(database.sensor_store(0, value.clone())),
            Ok(value.clone())
        );
        assert_eq!(
            runtime.block_on(database.sensor_store(0, value.clone())),
            Ok(value.clone())
        );
        let mut conflict = value.clone();
        conflict.last_received_ms = 9;
        assert_eq!(
            runtime.block_on(database.sensor_store(0, conflict)),
            Err(Error::Conflict)
        );
        let mut stale = value.clone();
        stale.write_id = [6; 16];
        assert_eq!(
            runtime.block_on(database.sensor_store(0, stale)),
            Err(Error::Stale)
        );
        close(database, &runtime);
        let database = runtime
            .block_on(Database::connect(
                &root.0,
                binding,
                false,
                Instant::now() + Duration::from_secs(10),
            ))
            .unwrap();
        assert_eq!(runtime.block_on(database.sensor_load([4; 16])), Ok(value));
        close(database, &runtime);
    }
}
