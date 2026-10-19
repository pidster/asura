//! Index run files: a 64-byte header, fixed 4,096-byte checksummed pages of
//! at most 31 sorted 128-byte entries, and a 40-byte trailer. Runs are
//! immutable; a merge writes a new run.
use super::{ENTRY_BYTES, Key, Result, Value};
use crate::authority::ReplayError;
use crate::authority::conversation::{Hash, Id};
use sha2::{Digest, Sha256};

pub const HEADER_BYTES: usize = 64;
pub const PAGE_BYTES: usize = 4096;
pub const PAGE_ENTRIES: usize = 31;
pub const TRAILER_BYTES: usize = 40;
const _: () = assert!(PAGE_BYTES - PAGE_ENTRIES * ENTRY_BYTES == 128);
const MAGIC: &[u8; 8] = b"ASURAX01";
const COMMIT: &[u8; 8] = b"ASURAC01";
/// A level-0 run holds at most 131,072 entries (17 MiB).
pub const MAX_LEVEL0_ENTRIES: u64 = 131_072;
/// Output pages per merge step.
pub const MERGE_STEP_PAGES: u64 = 256;
/// Runs merged at once.
pub const FAN_IN: usize = 8;

/// Byte reads of one run file. The journal worker implements it over a held
/// descriptor; tests use memory.
pub trait RunSource {
    fn read_exact_at(&mut self, offset: u64, buffer: &mut [u8]) -> Result<()>;
}
impl RunSource for &[u8] {
    fn read_exact_at(&mut self, offset: u64, buffer: &mut [u8]) -> Result<()> {
        let start = usize::try_from(offset).map_err(|_| ReplayError::Corrupt)?;
        let bytes = self
            .get(start..start + buffer.len())
            .ok_or(ReplayError::Corrupt)?;
        buffer.copy_from_slice(bytes);
        Ok(())
    }
}
/// Byte writes of one new run file.
pub trait RunSink {
    fn write_all_at(&mut self, offset: u64, bytes: &[u8]) -> Result<()>;
}
impl RunSink for Vec<u8> {
    fn write_all_at(&mut self, offset: u64, bytes: &[u8]) -> Result<()> {
        let start = offset as usize;
        if self.len() < start + bytes.len() {
            self.resize(start + bytes.len(), 0);
        }
        self[start..start + bytes.len()].copy_from_slice(bytes);
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RunHeader {
    pub level: u8,
    pub installation_id: Id,
    pub first: u64,
    pub last: u64,
    pub entries: u64,
    pub pages: u64,
}
impl RunHeader {
    pub fn new(level: u8, installation_id: Id, first: u64, last: u64, entries: u64) -> Self {
        Self {
            level,
            installation_id,
            first,
            last,
            entries,
            pages: entries.div_ceil(PAGE_ENTRIES as u64),
        }
    }
    pub fn file_bytes(&self) -> u64 {
        HEADER_BYTES as u64 + self.pages * PAGE_BYTES as u64 + TRAILER_BYTES as u64
    }
    pub fn encode(&self) -> [u8; HEADER_BYTES] {
        let mut out = [0; HEADER_BYTES];
        out[..8].copy_from_slice(MAGIC);
        out[8..10].copy_from_slice(&1u16.to_be_bytes());
        out[10] = self.level;
        out[11..27].copy_from_slice(&self.installation_id);
        out[27..35].copy_from_slice(&self.first.to_be_bytes());
        out[35..43].copy_from_slice(&self.last.to_be_bytes());
        out[43..51].copy_from_slice(&self.entries.to_be_bytes());
        out[51..59].copy_from_slice(&self.pages.to_be_bytes());
        out
    }
    pub fn decode(bytes: &[u8]) -> Result<Self> {
        if bytes.len() != HEADER_BYTES || &bytes[..8] != MAGIC {
            return Err(ReplayError::Corrupt);
        }
        if bytes[8..10] != 1u16.to_be_bytes() {
            return Err(ReplayError::UnsupportedFormat);
        }
        let u64_at = |at: usize| u64::from_be_bytes(bytes[at..at + 8].try_into().expect("8"));
        let header = Self {
            level: bytes[10],
            installation_id: bytes[11..27].try_into().expect("16"),
            first: u64_at(27),
            last: u64_at(35),
            entries: u64_at(43),
            pages: u64_at(51),
        };
        if bytes[59..].iter().any(|b| *b != 0)
            || header.first == 0
            || header.last < header.first
            || header.entries == 0
            || header.pages != header.entries.div_ceil(PAGE_ENTRIES as u64)
        {
            return Err(ReplayError::Corrupt);
        }
        Ok(header)
    }
    pub fn read(source: &mut dyn RunSource) -> Result<Self> {
        let mut bytes = [0; HEADER_BYTES];
        source.read_exact_at(0, &mut bytes)?;
        Self::decode(&bytes)
    }
}

/// One verified page: its entries in ascending order.
pub type Page = Vec<(Key, Value)>;

fn page_digest(entries: &[u8], count: u16, page: u64, first: u64) -> Hash {
    let mut hash = Sha256::new();
    hash.update(entries);
    hash.update(count.to_be_bytes());
    hash.update(page.to_be_bytes());
    hash.update(first.to_be_bytes());
    hash.finalize().into()
}
fn encode_page(entries: &[(Key, Value)], page: u64, first: u64) -> [u8; PAGE_BYTES] {
    let mut out = [0; PAGE_BYTES];
    for (index, (key, value)) in entries.iter().enumerate() {
        let at = index * ENTRY_BYTES;
        out[at..at + 32].copy_from_slice(&key.encode());
        out[at + 32..at + ENTRY_BYTES].copy_from_slice(value);
    }
    let body = PAGE_ENTRIES * ENTRY_BYTES;
    let count = entries.len() as u16;
    let digest = page_digest(&out[..body], count, page, first);
    out[body..body + 2].copy_from_slice(&count.to_be_bytes());
    out[body + 2..body + 10].copy_from_slice(&page.to_be_bytes());
    out[body + 10..body + 18].copy_from_slice(&first.to_be_bytes());
    out[body + 18..body + 50].copy_from_slice(&digest);
    out
}
/// Verifies one page and decodes its entries. Any difference is `Corrupt`.
pub fn decode_page(bytes: &[u8], header: &RunHeader, page: u64) -> Result<Page> {
    if bytes.len() != PAGE_BYTES || page >= header.pages {
        return Err(ReplayError::Corrupt);
    }
    let body = PAGE_ENTRIES * ENTRY_BYTES;
    let count = u16::from_be_bytes(bytes[body..body + 2].try_into().expect("2"));
    let number = u64::from_be_bytes(bytes[body + 2..body + 10].try_into().expect("8"));
    let first = u64::from_be_bytes(bytes[body + 10..body + 18].try_into().expect("8"));
    let expected = if page + 1 == header.pages {
        header.entries - page * PAGE_ENTRIES as u64
    } else {
        PAGE_ENTRIES as u64
    };
    if u64::from(count) != expected
        || number != page
        || first != header.first
        || bytes[body + 18..body + 50] != page_digest(&bytes[..body], count, page, first)
        || bytes[body + 50..].iter().any(|b| *b != 0)
        || bytes[count as usize * ENTRY_BYTES..body]
            .iter()
            .any(|b| *b != 0)
    {
        return Err(ReplayError::Corrupt);
    }
    let mut entries = Vec::with_capacity(count as usize);
    for index in 0..count as usize {
        let at = index * ENTRY_BYTES;
        let key = Key::decode(&bytes[at..at + 32])?;
        if entries
            .last()
            .is_some_and(|(prior, _): &(Key, Value)| *prior >= key)
        {
            return Err(ReplayError::Corrupt);
        }
        entries.push((
            key,
            bytes[at + 32..at + ENTRY_BYTES].try_into().expect("96"),
        ));
    }
    Ok(entries)
}
pub fn read_page(source: &mut dyn RunSource, header: &RunHeader, page: u64) -> Result<Page> {
    let mut bytes = vec![0; PAGE_BYTES];
    source.read_exact_at(HEADER_BYTES as u64 + page * PAGE_BYTES as u64, &mut bytes)?;
    decode_page(&bytes, header, page)
}

/// Streams entries in strictly ascending key order into a new run file.
pub struct RunWriter {
    header: RunHeader,
    digest: Sha256,
    page: Vec<(Key, Value)>,
    written_pages: u64,
    written_entries: u64,
    last: Option<Key>,
}
impl RunWriter {
    pub fn new(header: RunHeader, sink: &mut dyn RunSink) -> Result<Self> {
        let bytes = header.encode();
        sink.write_all_at(0, &bytes)?;
        let mut digest = Sha256::new();
        digest.update(bytes);
        Ok(Self {
            header,
            digest,
            page: Vec::with_capacity(PAGE_ENTRIES),
            written_pages: 0,
            written_entries: 0,
            last: None,
        })
    }
    pub fn header(&self) -> &RunHeader {
        &self.header
    }
    pub fn pages_written(&self) -> u64 {
        self.written_pages
    }
    /// A key at or below the previous one is `Corrupt` (duplicate keys).
    pub fn push(&mut self, key: Key, value: Value, sink: &mut dyn RunSink) -> Result<()> {
        if self.last.is_some_and(|last| last >= key)
            || self.written_entries + self.page.len() as u64 >= self.header.entries
        {
            return Err(ReplayError::Corrupt);
        }
        self.last = Some(key);
        self.page.push((key, value));
        if self.page.len() == PAGE_ENTRIES {
            self.flush(sink)?;
        }
        Ok(())
    }
    fn flush(&mut self, sink: &mut dyn RunSink) -> Result<()> {
        let bytes = encode_page(&self.page, self.written_pages, self.header.first);
        sink.write_all_at(
            HEADER_BYTES as u64 + self.written_pages * PAGE_BYTES as u64,
            &bytes,
        )?;
        self.digest.update(bytes);
        self.written_entries += self.page.len() as u64;
        self.written_pages += 1;
        self.page.clear();
        Ok(())
    }
    /// Writes the last page and the trailer. Returns the file digest.
    pub fn finish(mut self, sink: &mut dyn RunSink) -> Result<Hash> {
        if !self.page.is_empty() {
            self.flush(sink)?;
        }
        if self.written_entries != self.header.entries || self.written_pages != self.header.pages {
            return Err(ReplayError::Corrupt);
        }
        let digest: Hash = self.digest.finalize().into();
        let mut trailer = [0; TRAILER_BYTES];
        trailer[..32].copy_from_slice(&digest);
        trailer[32..].copy_from_slice(COMMIT);
        sink.write_all_at(self.header.file_bytes() - TRAILER_BYTES as u64, &trailer)?;
        Ok(digest)
    }
}

/// Encodes a complete run from sorted entries (tests and small runs).
pub fn encode_run(
    level: u8,
    installation_id: Id,
    first: u64,
    last: u64,
    entries: &[(Key, Value)],
) -> Result<(Vec<u8>, Hash)> {
    let mut bytes = Vec::new();
    let header = RunHeader::new(level, installation_id, first, last, entries.len() as u64);
    let mut writer = RunWriter::new(header, &mut bytes)?;
    for (key, value) in entries {
        writer.push(*key, *value, &mut bytes)?;
    }
    let digest = writer.finish(&mut bytes)?;
    Ok((bytes, digest))
}

/// The first position whose key is at least `key`: `(page, index)`, or
/// `None` when every key is smaller. Reads at most `ceil(log2(pages)) + 1` pages.
fn lower_bound(
    source: &mut dyn RunSource,
    header: &RunHeader,
    key: &Key,
) -> Result<Option<(u64, Page, usize)>> {
    let (mut lo, mut hi) = (0, header.pages);
    let mut cached: Option<(u64, Page)> = None;
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        let page = read_page(source, header, mid)?;
        let last = page.last().map(|(k, _)| *k).ok_or(ReplayError::Corrupt)?;
        if last < *key {
            lo = mid + 1;
        } else {
            hi = mid;
            cached = Some((mid, page));
        }
    }
    if lo == header.pages {
        return Ok(None);
    }
    let page = match cached {
        Some((number, page)) if number == lo => page,
        _ => read_page(source, header, lo)?,
    };
    let index = page.partition_point(|(k, _)| k < key);
    Ok(Some((lo, page, index)))
}
/// One exact lookup.
pub fn get(source: &mut dyn RunSource, header: &RunHeader, key: &Key) -> Result<Option<Value>> {
    Ok(
        lower_bound(source, header, key)?.and_then(|(_, page, index)| {
            page.get(index)
                .filter(|(found, _)| found == key)
                .map(|(_, value)| *value)
        }),
    )
}
/// Entries with `lo <= key < hi`, ascending or descending, at most `limit`.
pub fn range(
    source: &mut dyn RunSource,
    header: &RunHeader,
    lo: &Key,
    hi: &Key,
    descending: bool,
    limit: usize,
) -> Result<Vec<(Key, Value)>> {
    let mut out = Vec::new();
    if limit == 0 || lo >= hi {
        return Ok(out);
    }
    if !descending {
        let Some((mut number, mut page, mut index)) = lower_bound(source, header, lo)? else {
            return Ok(out);
        };
        loop {
            while index < page.len() {
                let (key, value) = page[index];
                if key >= *hi {
                    return Ok(out);
                }
                if out
                    .last()
                    .is_some_and(|(prior, _): &(Key, Value)| *prior >= key)
                {
                    return Err(ReplayError::Corrupt);
                }
                out.push((key, value));
                if out.len() == limit {
                    return Ok(out);
                }
                index += 1;
            }
            number += 1;
            if number == header.pages {
                return Ok(out);
            }
            page = read_page(source, header, number)?;
            index = 0;
        }
    }
    // Descending: start just below `hi`.
    let (mut number, mut page, mut index) = match lower_bound(source, header, hi)? {
        Some(position) => position,
        None => {
            let last = header.pages - 1;
            let page = read_page(source, header, last)?;
            let length = page.len();
            (last, page, length)
        }
    };
    loop {
        while index > 0 {
            index -= 1;
            let (key, value) = page[index];
            if key < *lo {
                return Ok(out);
            }
            if out
                .last()
                .is_some_and(|(prior, _): &(Key, Value)| *prior <= key)
            {
                return Err(ReplayError::Corrupt);
            }
            out.push((key, value));
            if out.len() == limit {
                return Ok(out);
            }
        }
        if number == 0 {
            return Ok(out);
        }
        number -= 1;
        page = read_page(source, header, number)?;
        index = page.len();
    }
}

/// Reads the trailer and checks the commit marker; returns the recorded digest.
pub fn trailer(source: &mut dyn RunSource, header: &RunHeader) -> Result<Hash> {
    let mut bytes = [0; TRAILER_BYTES];
    source.read_exact_at(header.file_bytes() - TRAILER_BYTES as u64, &mut bytes)?;
    if &bytes[32..] != COMMIT {
        return Err(ReplayError::Corrupt);
    }
    Ok(bytes[..32].try_into().expect("32"))
}

/// One input of a merge: reads its pages in order and checks every page
/// digest and, at the end, the whole-file digest.
pub struct MergeInput {
    pub header: RunHeader,
    pub expected_digest: Hash,
    digest: Sha256,
    next_page: u64,
    page: Page,
    index: usize,
    done: bool,
}
impl MergeInput {
    pub fn new(header: RunHeader, expected_digest: Hash) -> Self {
        let mut digest = Sha256::new();
        digest.update(header.encode());
        Self {
            header,
            expected_digest,
            digest,
            next_page: 0,
            page: Vec::new(),
            index: 0,
            done: false,
        }
    }
    /// The current smallest entry, reading the next page when needed.
    fn peek(&mut self, source: &mut dyn RunSource) -> Result<Option<(Key, Value)>> {
        if self.done {
            return Ok(None);
        }
        if self.index == self.page.len() {
            if self.next_page == self.header.pages {
                let recorded = trailer(source, &self.header)?;
                let actual: Hash = std::mem::take(&mut self.digest).finalize().into();
                if recorded != actual || actual != self.expected_digest {
                    return Err(ReplayError::Corrupt);
                }
                self.done = true;
                return Ok(None);
            }
            let mut bytes = vec![0; PAGE_BYTES];
            source.read_exact_at(
                HEADER_BYTES as u64 + self.next_page * PAGE_BYTES as u64,
                &mut bytes,
            )?;
            let page = decode_page(&bytes, &self.header, self.next_page)?;
            if let (Some((prior, _)), Some((first, _))) = (self.page.last(), page.first())
                && prior >= first
            {
                return Err(ReplayError::Corrupt);
            }
            self.digest.update(&bytes);
            self.page = page;
            self.index = 0;
            self.next_page += 1;
        }
        Ok(Some(self.page[self.index]))
    }
}

/// Reads of every merge input and writes of the merge output.
pub trait MergeIo {
    fn read_input(&mut self, input: usize, offset: u64, buffer: &mut [u8]) -> Result<()>;
    fn write_output(&mut self, offset: u64, bytes: &[u8]) -> Result<()>;
}
struct InputSource<'a> {
    io: &'a mut dyn MergeIo,
    input: usize,
}
impl RunSource for InputSource<'_> {
    fn read_exact_at(&mut self, offset: u64, buffer: &mut [u8]) -> Result<()> {
        self.io.read_input(self.input, offset, buffer)
    }
}
struct OutputSink<'a> {
    io: &'a mut dyn MergeIo,
}
impl RunSink for OutputSink<'_> {
    fn write_all_at(&mut self, offset: u64, bytes: &[u8]) -> Result<()> {
        self.io.write_output(offset, bytes)
    }
}
/// In-memory merge I/O for tests.
pub struct MemoryMerge<'a> {
    pub inputs: Vec<&'a [u8]>,
    pub output: Vec<u8>,
}
impl MergeIo for MemoryMerge<'_> {
    fn read_input(&mut self, input: usize, offset: u64, buffer: &mut [u8]) -> Result<()> {
        let mut source = self.inputs[input];
        source.read_exact_at(offset, buffer)
    }
    fn write_output(&mut self, offset: u64, bytes: &[u8]) -> Result<()> {
        self.output.write_all_at(offset, bytes)
    }
}

/// A merge of several runs into one run of the next level, run in steps of
/// at most [`MERGE_STEP_PAGES`] output pages.
pub struct Merge {
    pub inputs: Vec<MergeInput>,
    writer: Option<RunWriter>,
}
impl Merge {
    pub fn new(
        inputs: Vec<MergeInput>,
        level: u8,
        installation_id: Id,
        io: &mut dyn MergeIo,
    ) -> Result<Self> {
        let sink = &mut OutputSink { io };
        let first = inputs
            .iter()
            .map(|i| i.header.first)
            .min()
            .ok_or(ReplayError::Corrupt)?;
        let last = inputs
            .iter()
            .map(|i| i.header.last)
            .max()
            .ok_or(ReplayError::Corrupt)?;
        let entries = inputs.iter().map(|i| i.header.entries).sum();
        let header = RunHeader::new(level, installation_id, first, last, entries);
        Ok(Self {
            inputs,
            writer: Some(RunWriter::new(header, sink)?),
        })
    }
    pub fn header(&self) -> Option<&RunHeader> {
        self.writer.as_ref().map(|w| w.header())
    }
    /// Writes at most `pages` output pages. Returns the output digest when
    /// the merge is complete. A duplicate key is `Corrupt`.
    pub fn step(&mut self, io: &mut dyn MergeIo, pages: u64) -> Result<Option<Hash>> {
        let writer = self.writer.as_mut().ok_or(ReplayError::Corrupt)?;
        let target = writer.pages_written() + pages;
        while writer.pages_written() < target {
            let mut best: Option<(usize, Key, Value)> = None;
            for (index, input) in self.inputs.iter_mut().enumerate() {
                let mut source = InputSource {
                    io: &mut *io,
                    input: index,
                };
                if let Some((key, value)) = input.peek(&mut source)? {
                    match best {
                        Some((_, current, _)) if current == key => {
                            return Err(ReplayError::Corrupt);
                        }
                        Some((_, current, _)) if current < key => {}
                        _ => best = Some((index, key, value)),
                    }
                }
            }
            let Some((index, key, value)) = best else {
                let writer = self.writer.take().expect("writer");
                return writer.finish(&mut OutputSink { io }).map(Some);
            };
            self.inputs[index].index += 1;
            writer.push(key, value, &mut OutputSink { io: &mut *io })?;
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::super::Tag;
    use super::*;
    fn entries(n: u64, step: u64, offset: u64) -> Vec<(Key, Value)> {
        (0..n)
            .map(|i| {
                let mut value = [0; super::super::VALUE_BYTES];
                value[..8].copy_from_slice(&(i * step + offset).to_be_bytes());
                (Key::new(Tag::Operation, [1; 16], i * step + offset), value)
            })
            .collect()
    }
    #[test]
    fn pages_hold_31_entries_and_lookups_read_few_pages() {
        let all = entries(1000, 2, 0);
        let (bytes, digest) = encode_run(0, [9; 16], 1, 50, &all).unwrap();
        let mut source: &[u8] = &bytes;
        let header = RunHeader::read(&mut source).unwrap();
        assert_eq!(header.pages, 33);
        assert_eq!(bytes.len() as u64, header.file_bytes());
        assert_eq!(trailer(&mut source, &header).unwrap(), digest);
        for (key, value) in &all {
            assert_eq!(get(&mut source, &header, key).unwrap(), Some(*value));
        }
        let missing = Key::new(Tag::Operation, [1; 16], 7);
        assert_eq!(get(&mut source, &header, &missing).unwrap(), None);
        let up = range(
            &mut source,
            &header,
            &Key::new(Tag::Operation, [1; 16], 61),
            &Key::new(Tag::Operation, [1; 16], 100),
            false,
            5,
        )
        .unwrap();
        assert_eq!(
            up.iter().map(|(k, _)| k.disc).collect::<Vec<_>>(),
            [62, 64, 66, 68, 70]
        );
        let down = range(
            &mut source,
            &header,
            &Key::first(Tag::Operation),
            &Key::new(Tag::Operation, [1; 16], 62),
            true,
            3,
        )
        .unwrap();
        assert_eq!(
            down.iter().map(|(k, _)| k.disc).collect::<Vec<_>>(),
            [60, 58, 56]
        );
        let tail = range(
            &mut source,
            &header,
            &Key::first(Tag::Operation),
            &Key::last(Tag::Operation),
            true,
            2,
        )
        .unwrap();
        assert_eq!(
            tail.iter().map(|(k, _)| k.disc).collect::<Vec<_>>(),
            [1998, 1996]
        );
    }
    #[test]
    fn sj21_page_damage_and_unsorted_or_duplicate_keys_are_corrupt() {
        let all = entries(100, 1, 0);
        let (mut bytes, _) = encode_run(0, [9; 16], 1, 5, &all).unwrap();
        bytes[HEADER_BYTES + PAGE_BYTES + 40] ^= 1;
        let mut source: &[u8] = &bytes;
        let header = RunHeader::read(&mut source).unwrap();
        assert_eq!(
            get(&mut source, &header, &all[40].0),
            Err(ReplayError::Corrupt)
        );
        let mut duplicate = entries(3, 1, 0);
        duplicate[2] = duplicate[1];
        assert_eq!(
            encode_run(0, [9; 16], 1, 5, &duplicate).unwrap_err(),
            ReplayError::Corrupt
        );
    }
    #[test]
    fn sj11_sj12_merge_equals_union_and_rejects_duplicates() {
        let parts: Vec<_> = (0..8u64).map(|i| entries(70, 8, i)).collect();
        let runs: Vec<_> = parts
            .iter()
            .enumerate()
            .map(|(i, part)| {
                encode_run(0, [9; 16], i as u64 * 10 + 1, i as u64 * 10 + 10, part).unwrap()
            })
            .collect();
        let inputs = runs
            .iter()
            .map(|(bytes, digest)| {
                let mut source: &[u8] = bytes;
                MergeInput::new(RunHeader::read(&mut source).unwrap(), *digest)
            })
            .collect();
        let mut io = MemoryMerge {
            inputs: runs.iter().map(|(b, _)| b.as_slice()).collect(),
            output: Vec::new(),
        };
        let mut merge = Merge::new(inputs, 1, [9; 16], &mut io).unwrap();
        let mut steps = 0;
        let digest = loop {
            steps += 1;
            if let Some(digest) = merge.step(&mut io, 3).unwrap() {
                break digest;
            }
        };
        let output = io.output;
        assert!(steps > 5, "a merge runs in bounded steps");
        let mut union: Vec<_> = parts.concat();
        union.sort();
        let (expected, expected_digest) = encode_run(1, [9; 16], 1, 80, &union).unwrap();
        assert_eq!(output, expected);
        assert_eq!(digest, expected_digest);
        // A duplicate key across inputs is Corrupt.
        let a = encode_run(0, [9; 16], 1, 2, &entries(3, 1, 0)).unwrap();
        let b = encode_run(0, [9; 16], 3, 4, &entries(3, 1, 2)).unwrap();
        let inputs = [&a, &b]
            .iter()
            .map(|(bytes, digest)| {
                let mut source: &[u8] = bytes;
                MergeInput::new(RunHeader::read(&mut source).unwrap(), *digest)
            })
            .collect();
        let mut io = MemoryMerge {
            inputs: vec![a.0.as_slice(), b.0.as_slice()],
            output: Vec::new(),
        };
        let mut merge = Merge::new(inputs, 1, [9; 16], &mut io).unwrap();
        assert_eq!(merge.step(&mut io, 100), Err(ReplayError::Corrupt));
        // A wrong recorded input digest is Corrupt.
        let inputs = vec![{
            let mut source: &[u8] = &a.0;
            MergeInput::new(RunHeader::read(&mut source).unwrap(), [0; 32])
        }];
        let mut io = MemoryMerge {
            inputs: vec![a.0.as_slice()],
            output: Vec::new(),
        };
        let mut merge = Merge::new(inputs, 1, [9; 16], &mut io).unwrap();
        assert_eq!(merge.step(&mut io, 100), Err(ReplayError::Corrupt));
    }
}
