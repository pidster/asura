//! In-memory journal files for pure tests of the journal driver: the same
//! roll, checkpoint, run and replay code as on disk, without I/O.
use super::JournalFiles;
use asura_platform::{JournalEntry, JournalError, JournalName, JournalPoint};
use std::collections::BTreeMap;

/// Files held in memory. `crash_at` makes the named point fail as if the
/// process died there; `free` is the free-space value the ledger reads.
#[derive(Clone, Debug, Default)]
pub struct MemoryFiles {
    pub files: BTreeMap<JournalName, Vec<u8>>,
    pub free: u64,
    pub crash_at: Option<JournalPoint>,
    /// Points reached, in order.
    pub reached: Vec<JournalPoint>,
    /// The next append fails with `ENOSPC` and no byte written.
    pub no_space: bool,
}
impl MemoryFiles {
    pub fn new() -> Self {
        Self {
            files: BTreeMap::from([(JournalName::Lock, Vec::new())]),
            free: 1 << 40,
            ..Self::default()
        }
    }
    pub fn names(&self) -> Vec<JournalName> {
        self.files.keys().copied().collect()
    }
}
impl JournalFiles for MemoryFiles {
    fn census(&mut self) -> Result<Vec<JournalEntry>, JournalError> {
        Ok(self
            .files
            .iter()
            .map(|(name, bytes)| JournalEntry {
                name: *name,
                bytes: bytes.len() as u64,
            })
            .collect())
    }
    fn read_at(
        &mut self,
        name: JournalName,
        offset: u64,
        buffer: &mut [u8],
    ) -> Result<(), JournalError> {
        let bytes = self.files.get(&name).ok_or(JournalError::Absent)?;
        let start = offset as usize;
        let found = bytes
            .get(start..start + buffer.len())
            .ok_or(JournalError::Absent)?;
        buffer.copy_from_slice(found);
        Ok(())
    }
    fn create(&mut self, name: JournalName) -> Result<(), JournalError> {
        if self.files.contains_key(&name) {
            return Err(JournalError::Io);
        }
        self.files.insert(name, Vec::new());
        Ok(())
    }
    fn write_at(
        &mut self,
        name: JournalName,
        offset: u64,
        bytes: &[u8],
    ) -> Result<(), JournalError> {
        if !name.is_tmp() {
            return Err(JournalError::Unsafe);
        }
        let file = self.files.get_mut(&name).ok_or(JournalError::Absent)?;
        let end = offset as usize + bytes.len();
        if file.len() < end {
            file.resize(end, 0);
        }
        file[offset as usize..end].copy_from_slice(bytes);
        Ok(())
    }
    fn sync(&mut self, name: JournalName) -> Result<(), JournalError> {
        self.files
            .contains_key(&name)
            .then_some(())
            .ok_or(JournalError::Absent)
    }
    fn append(&mut self, segment: u64, expected: u64, bytes: &[u8]) -> Result<(), JournalError> {
        if std::mem::take(&mut self.no_space) {
            return Err(JournalError::NoSpace { unchanged: true });
        }
        let file = self
            .files
            .get_mut(&JournalName::Segment(segment))
            .ok_or(JournalError::Absent)?;
        if file.len() as u64 != expected {
            return Err(JournalError::Unsafe);
        }
        file.extend_from_slice(bytes);
        Ok(())
    }
    fn publish(&mut self, from: JournalName, to: JournalName) -> Result<(), JournalError> {
        let bytes = self.files.remove(&from).ok_or(JournalError::Absent)?;
        self.files.insert(to, bytes);
        Ok(())
    }
    fn remove(&mut self, name: JournalName) -> Result<(), JournalError> {
        if !name.is_derived() {
            return Err(JournalError::Unsafe);
        }
        self.files.remove(&name);
        Ok(())
    }
    fn free_bytes(&mut self) -> Result<u64, JournalError> {
        Ok(self.free)
    }
    fn point(&mut self, point: JournalPoint) -> Result<(), JournalError> {
        self.reached.push(point);
        if self.crash_at == Some(point) {
            return Err(JournalError::Cancelled);
        }
        Ok(())
    }
}
