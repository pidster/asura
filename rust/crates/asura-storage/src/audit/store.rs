//! Synchronous bounded adapter. Only the service audit worker may call these methods.
use super::*;
use asura_platform::{AuditArchive, AuditDirectory, AuditFile, RuntimeDirectory};
use std::{
    collections::VecDeque,
    sync::atomic::{AtomicBool, Ordering},
    time::Instant,
};
const HYDRATE_BYTES: usize = 256 * 1024;
pub struct Opened {
    pub store: AuditStore,
    pub recent: Vec<Record>,
    pub older_omitted: bool,
    pub lost_partial: bool,
}
pub struct AuditStore {
    directory: Option<AuditDirectory>,
    active: Option<AuditFile>,
    settings: Settings,
    epoch: Id,
    last_sequence: u64,
    archives: VecDeque<AuditArchive>,
    failed: bool,
}
fn map(error: asura_platform::Error) -> Error {
    match error {
        asura_platform::Error::UnsafeRuntime | asura_platform::Error::OwnerBusy => {
            Error::UnsafeStorage
        }
        asura_platform::Error::Deadline => Error::Deadline,
        _ => Error::Io,
    }
}
fn check(deadline: Instant, cancel: &AtomicBool) -> Result<(), Error> {
    if cancel.load(Ordering::Acquire) {
        Err(Error::Cancelled)
    } else if Instant::now() >= deadline {
        Err(Error::Deadline)
    } else {
        Ok(())
    }
}
fn archive_name(epoch: Id, sequence: u64) -> String {
    let epoch: String = epoch.iter().map(|b| format!("{b:02x}")).collect();
    format!("audit.{epoch}.{sequence:020}.jsonl")
}
impl AuditStore {
    pub fn open(
        runtime: RuntimeDirectory,
        settings: Settings,
        epoch: Id,
        deadline: Instant,
        cancel: &AtomicBool,
    ) -> Result<Opened, Error> {
        settings.validate()?;
        if epoch == [0; 16] {
            return Err(Error::Invalid);
        }
        check(deadline, cancel)?;
        let directory =
            AuditDirectory::open(runtime, settings.enabled, deadline, cancel).map_err(map)?;
        let Some(directory) = directory else {
            return Ok(Opened {
                store: Self {
                    directory: None,
                    active: None,
                    settings,
                    epoch,
                    last_sequence: 0,
                    archives: VecDeque::new(),
                    failed: false,
                },
                recent: vec![],
                older_omitted: false,
                lost_partial: false,
            });
        };
        let mut archives = if settings.enabled {
            directory.archives(deadline, cancel).map_err(map)?.into()
        } else {
            VecDeque::new()
        };
        let mut active = directory
            .open_active(settings.enabled, false, deadline, cancel)
            .map_err(map)?;
        let mut recent = Vec::new();
        let mut older_omitted = false;
        let mut lost_partial = false;
        if let Some(file) = &mut active {
            let (complete, lost) = tail_boundary(&directory, file, deadline, cancel)?;
            lost_partial = lost;
            let hydrated = hydrate(&directory, file, complete, deadline, cancel)?;
            recent = hydrated.0;
            older_omitted = hydrated.1;
            if settings.enabled {
                if archives.len() >= 10_001 {
                    return Err(Error::Limit);
                }
                if lost {
                    file.truncate(&directory, complete, deadline, cancel)
                        .map_err(map)?;
                }
                let file = active.take().expect("validated active");
                archives.push_back(
                    directory
                        .seal(file, &archive_name(epoch, 0), deadline, cancel)
                        .map_err(map)?,
                );
            }
        }
        if settings.enabled {
            active = directory
                .open_active(true, true, deadline, cancel)
                .map_err(map)?;
        }
        let mut store = Self {
            directory: Some(directory),
            active,
            settings,
            epoch,
            last_sequence: 0,
            archives,
            failed: false,
        };
        if settings.enabled {
            store.maintain(deadline, cancel)?;
        }
        check(deadline, cancel)?;
        Ok(Opened {
            store,
            recent,
            older_omitted,
            lost_partial,
        })
    }
    /// Success means every record in the supplied batch has been synchronized.
    /// A failure after any append is uncertain; never automatically replay that batch.
    pub fn append_batch(
        &mut self,
        records: &[Record],
        deadline: Instant,
        cancel: &AtomicBool,
    ) -> Result<(), Error> {
        if !self.settings.enabled {
            return Err(Error::Disabled);
        }
        if self.failed {
            return Err(Error::Io);
        }
        check(deadline, cancel)?;
        if records.is_empty() || records.len() > 16 {
            return Err(Error::Limit);
        }
        let mut encoded = Vec::with_capacity(records.len());
        let mut total = 0;
        let mut sequence = self.last_sequence;
        for record in records {
            let bytes = record.encode()?;
            if record.service_epoch != self.epoch || record.sequence <= sequence {
                return Err(Error::Invalid);
            }
            sequence = record.sequence;
            if bytes.len() as u64 > self.settings.max_file_bytes {
                return Err(Error::Limit);
            }
            total += bytes.len();
            if total > 65536 {
                return Err(Error::Limit);
            }
            encoded.push(bytes);
        }
        let result = (|| {
            for (record, bytes) in records.iter().zip(encoded) {
                check(deadline, cancel)?;
                let directory = self.directory.as_ref().ok_or(Error::Io)?;
                let active = self.active.as_ref().ok_or(Error::Io)?;
                active.validate(directory).map_err(map)?;
                if active
                    .len()
                    .checked_add(bytes.len() as u64)
                    .is_none_or(|size| size > self.settings.max_file_bytes)
                {
                    if self.archives.len() >= 10_001 {
                        return Err(Error::Limit);
                    }
                    let active = self.active.take().expect("validated active");
                    let archive = directory
                        .seal(
                            active,
                            &archive_name(self.epoch, self.last_sequence),
                            deadline,
                            cancel,
                        )
                        .map_err(map)?;
                    self.archives.push_back(archive);
                    self.active = directory
                        .open_active(true, true, deadline, cancel)
                        .map_err(map)?;
                }
                self.active
                    .as_mut()
                    .ok_or(Error::Io)?
                    .append(directory, &bytes, deadline, cancel)
                    .map_err(map)?;
                self.last_sequence = record.sequence;
            }
            self.sync(deadline, cancel)
        })();
        if let Err(error) = result {
            self.failed = true;
            self.active = None;
            return Err(match error {
                Error::UnsafeStorage => error,
                _ => Error::Unconfirmed,
            });
        }
        Ok(())
    }
    pub fn sync(&mut self, deadline: Instant, cancel: &AtomicBool) -> Result<(), Error> {
        if !self.settings.enabled {
            return Ok(());
        }
        if self.failed {
            return Err(Error::Io);
        }
        check(deadline, cancel)?;
        let directory = self.directory.as_ref().ok_or(Error::Io)?;
        let result = self
            .active
            .as_mut()
            .ok_or(Error::Io)?
            .sync(directory, deadline, cancel)
            .map_err(map);
        if result.is_err() {
            self.failed = true;
            self.active = None;
        }
        result
    }
    /// At most 32 removals per worker cycle. Temporary retention excess is explicit.
    pub fn maintain(&mut self, deadline: Instant, cancel: &AtomicBool) -> Result<(), Error> {
        if !self.settings.enabled {
            return Ok(());
        }
        if self.failed {
            return Err(Error::Io);
        }
        let directory = self.directory.as_ref().ok_or(Error::Io)?;
        let active = self.active.as_ref().ok_or(Error::Io)?;
        active.validate(directory).map_err(map)?;
        // Newly sealed files are sorted by birth time, not record chronology.
        self.archives
            .make_contiguous()
            .sort_by(|a, b| a.created.cmp(&b.created).then_with(|| a.name.cmp(&b.name)));
        for _ in 0..32 {
            if self.archives.len() <= self.settings.keep_files as usize {
                break;
            }
            check(deadline, cancel)?;
            directory
                .remove(
                    self.archives.front().expect("excess archive"),
                    deadline,
                    cancel,
                )
                .map_err(map)?;
            self.archives.pop_front();
        }
        Ok(())
    }
}
fn tail_boundary(
    directory: &AuditDirectory,
    file: &AuditFile,
    deadline: Instant,
    cancel: &AtomicBool,
) -> Result<(u64, bool), Error> {
    let size = file.len();
    if size == 0 {
        return Ok((0, false));
    }
    let start = size.saturating_sub(4097);
    let tail = file
        .read_range(directory, start, (size - start) as usize, deadline, cancel)
        .map_err(map)?;
    let complete = match tail.iter().rposition(|b| *b == b'\n') {
        Some(at) => start + at as u64 + 1,
        None if start == 0 => 0,
        None => return Err(Error::MalformedRecord),
    };
    if complete > 0 {
        let before = complete.saturating_sub(4097);
        let line = file
            .read_range(
                directory,
                before,
                (complete - before) as usize,
                deadline,
                cancel,
            )
            .map_err(map)?;
        let start = match line[..line.len() - 1].iter().rposition(|b| *b == b'\n') {
            Some(at) => at + 1,
            None if before == 0 => 0,
            None => return Err(Error::MalformedRecord),
        };
        Record::decode(&line[start..])?;
    }
    Ok((complete, complete < size))
}
fn hydrate(
    directory: &AuditDirectory,
    file: &AuditFile,
    complete: u64,
    deadline: Instant,
    cancel: &AtomicBool,
) -> Result<(Vec<Record>, bool), Error> {
    let start = complete.saturating_sub(HYDRATE_BYTES as u64);
    let bytes = file
        .read_range(
            directory,
            start,
            (complete - start) as usize,
            deadline,
            cancel,
        )
        .map_err(map)?;
    let mut offset = if start > 0 {
        bytes
            .iter()
            .position(|b| *b == b'\n')
            .map_or(bytes.len(), |at| at + 1)
    } else {
        0
    };
    let mut recent = VecDeque::new();
    let mut omitted = start > 0;
    let mut prior: Option<(Id, u64)> = None;
    while offset < bytes.len() {
        check(deadline, cancel)?;
        let end = bytes[offset..]
            .iter()
            .position(|b| *b == b'\n')
            .ok_or(Error::MalformedRecord)?
            + offset
            + 1;
        let record = Record::decode(&bytes[offset..end])?;
        if prior.is_some_and(|(epoch, sequence)| {
            epoch != record.service_epoch || sequence >= record.sequence
        }) {
            return Err(Error::MalformedRecord);
        }
        prior = Some((record.service_epoch, record.sequence));
        offset = end;
        if recent.len() == WINDOW_CAPACITY {
            recent.pop_back();
            omitted = true;
        }
        recent.push_front(record);
    }
    Ok((recent.into(), omitted))
}
