//! Streaming scanner (SD-S1), per-job numbering, literal matching and loss
//! markers. Raw bytes live only in the carry and are zeroized when released.
use super::rewrite::SecretVariable;
use super::rules::{Candidate, EndFinder, RuleSet, is_ascii_space, merge};
use super::{SecretKind, placeholder};
use aho_corasick::{AhoCorasick, MatchKind};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use zeroize::{Zeroize, Zeroizing};

/// SD5: an unfinished line or open private-key block holds at most this many bytes.
pub const CARRY_LIMIT: usize = 64 * 1024;
/// Numbered values per job; later values use `$ASURA_SECRET_0`.
pub const MAX_NUMBERS: u32 = 16_384;
pub const PROPAGATED_VALUES_LIMIT: usize = 256;
pub const PROPAGATED_BYTES_LIMIT: usize = 64 * 1024;
const PROPAGATED_MIN: usize = 8;
const PROPAGATED_MAX: usize = 4096;
const LITERAL_PIECE_MIN: usize = 3;
pub const MAX_LOSS_SPANS: usize = 64;
pub const MAX_SUMMARY_NUMBERS: usize = 256;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Stream {
    Stdout,
    Stderr,
}
impl Stream {
    fn index(self) -> usize {
        match self {
            Self::Stdout => 0,
            Self::Stderr => 1,
        }
    }
}

/// Bytes the scanner released: checked, with every detected value replaced.
/// Only this module constructs it, so a stage that accepts `Redacted` cannot
/// receive raw output.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct Redacted(Vec<u8>);
impl Redacted {
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
    pub fn into_bytes(self) -> Vec<u8> {
        self.0
    }
    pub fn len(&self) -> usize {
        self.0.len()
    }
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LossReason {
    LineOverLimit,
    KeyBlockOverLimit,
    DetectorError,
}
impl LossReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::LineOverLimit => "line_over_limit",
            Self::KeyBlockOverLimit => "key_block_over_limit",
            Self::DetectorError => "detector_error",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LossSpan {
    /// Offset of the marker in the stored stream.
    pub stored_offset: u64,
    pub withheld_bytes: u64,
    pub reason: LossReason,
}

/// Per-stream counts for the descriptor. Holds no value or value digest.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StreamStats {
    /// Raw bytes received.
    pub observed_bytes: u64,
    /// Bytes released, after replacement.
    pub stored_bytes: u64,
    pub replacements: u64,
    pub unnumbered_replacements: u64,
    /// Replacements that were an exact literal match (command or propagated).
    pub literal_replacements: u64,
    kind_counts: [u64; SecretKind::ALL.len()],
    /// The first `MAX_LOSS_SPANS` loss spans.
    pub loss_spans: Vec<LossSpan>,
    pub loss_span_count: u64,
    pub withheld_bytes: u64,
    pub degraded: bool,
}
impl Default for StreamStats {
    fn default() -> Self {
        Self {
            observed_bytes: 0,
            stored_bytes: 0,
            replacements: 0,
            unnumbered_replacements: 0,
            literal_replacements: 0,
            kind_counts: [0; SecretKind::ALL.len()],
            loss_spans: Vec::new(),
            loss_span_count: 0,
            withheld_bytes: 0,
            degraded: false,
        }
    }
}
impl StreamStats {
    pub fn kind_count(&self, kind: SecretKind) -> u64 {
        self.kind_counts[kind.index()]
    }
    /// Non-zero counts per kind, in kind order.
    pub fn kind_counts(&self) -> impl Iterator<Item = (SecretKind, u64)> + '_ {
        SecretKind::ALL
            .iter()
            .map(|k| (*k, self.kind_counts[k.index()]))
            .filter(|(_, n)| *n > 0)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NumberOrigin {
    Command,
    Output,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct NumberInfo {
    pub number: u32,
    pub kind: SecretKind,
    pub origin: NumberOrigin,
}
/// Settlement facts: numbers by kind and origin, with no value.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct JobSummary {
    /// The first `MAX_SUMMARY_NUMBERS` numbers.
    pub numbers: Vec<NumberInfo>,
    pub distinct_numbers: u32,
    pub stdout: StreamStats,
    pub stderr: StreamStats,
}

/// A replaced span in raw stream offsets, for whole-buffer mode.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RawSpan {
    pub start: u64,
    pub end: u64,
    pub kind: SecretKind,
    pub number: u32,
}

/// Number map keyed by SHA-256 over a per-job random key and the value, so it
/// holds no value bytes. The keyed digest never leaves memory.
struct Numbers {
    key: Zeroizing<[u8; 32]>,
    map: HashMap<[u8; 32], u32>,
    next: u32,
    info: Vec<NumberInfo>,
}
impl Numbers {
    fn new() -> Self {
        let mut key = Zeroizing::new([0u8; 32]);
        crate::fill_random(&mut key[..]);
        Self {
            key,
            map: HashMap::new(),
            next: 1,
            info: Vec::new(),
        }
    }
    fn digest(&self, value: &[u8]) -> [u8; 32] {
        let mut hash = Sha256::new();
        hash.update(&self.key[..]);
        hash.update(value);
        hash.finalize().into()
    }
    fn record(&mut self, number: u32, kind: SecretKind, origin: NumberOrigin) {
        if self.info.len() < MAX_SUMMARY_NUMBERS {
            self.info.push(NumberInfo {
                number,
                kind,
                origin,
            });
        }
    }
    fn assign(&mut self, digest: [u8; 32], kind: SecretKind) -> u32 {
        if let Some(&n) = self.map.get(&digest) {
            return n;
        }
        if self.next > MAX_NUMBERS {
            return 0;
        }
        let n = self.next;
        self.next += 1;
        self.map.insert(digest, n);
        self.record(n, kind, NumberOrigin::Output);
        n
    }
}

struct Literal {
    value: Zeroizing<Vec<u8>>,
    number: u32,
    kind: SecretKind,
}
/// Known values matched exactly: command secrets and propagated output values.
struct Literals {
    entries: Vec<Literal>,
    known: HashSet<[u8; 32]>,
    propagated: usize,
    propagated_bytes: usize,
    automaton: Option<(AhoCorasick, Vec<(u32, SecretKind)>)>,
    dirty: bool,
}
impl Literals {
    fn add(&mut self, digest: [u8; 32], value: &[u8], number: u32, kind: SecretKind) {
        if self.known.insert(digest) {
            self.entries.push(Literal {
                value: Zeroizing::new(value.to_vec()),
                number,
                kind,
            });
            self.dirty = true;
        }
    }
    fn propagate(&mut self, digest: [u8; 32], value: &[u8], number: u32, kind: SecretKind) {
        if !(PROPAGATED_MIN..=PROPAGATED_MAX).contains(&value.len())
            || self.known.contains(&digest)
            || self.propagated >= PROPAGATED_VALUES_LIMIT
            || self.propagated_bytes + value.len() > PROPAGATED_BYTES_LIMIT
        {
            return;
        }
        self.propagated += 1;
        self.propagated_bytes += value.len();
        self.add(digest, value, number, kind);
    }
    /// Rebuilds the automaton before the next line when the set grew. A
    /// literal with a line terminator is matched by its pieces of 3+ bytes.
    fn prepare(&mut self) {
        if !self.dirty {
            return;
        }
        self.dirty = false;
        let mut pieces: Vec<&[u8]> = Vec::new();
        let mut ids = Vec::new();
        for literal in &self.entries {
            for piece in literal.value.split(|b| matches!(b, b'\n' | b'\r')) {
                if piece.len() >= LITERAL_PIECE_MIN {
                    pieces.push(piece);
                    ids.push((literal.number, literal.kind));
                }
            }
        }
        if pieces.is_empty() {
            self.automaton = None;
            return;
        }
        // A build failure keeps the previous automaton: values are still
        // replaced by rule, as when the propagation limit is reached.
        if let Ok(automaton) = AhoCorasick::builder()
            .match_kind(MatchKind::LeftmostLongest)
            .build(&pieces)
        {
            self.automaton = Some((automaton, ids));
        }
    }
    fn find(&self, line: &[u8], out: &mut Vec<Candidate>) {
        if let Some((automaton, ids)) = &self.automaton {
            for m in automaton.find_iter(line) {
                let (number, kind) = ids[m.pattern().as_usize()];
                out.push(Candidate::literal(m.start(), m.end(), kind, number));
            }
        }
    }
}

struct Shared {
    rules: &'static RuleSet,
    numbers: Numbers,
    literals: Literals,
}

enum Mode {
    Scanning,
    /// Long line (SD5): bytes counted, not stored, up to the terminator.
    Withholding {
        count: u64,
        reason: LossReason,
    },
    PemOpen {
        block: Zeroizing<Vec<u8>>,
        start: u64,
        finder: EndFinder,
    },
    PemOverflow {
        count: u64,
        finder: EndFinder,
        last_terminator: Option<u8>,
        /// Withholding an over-long line inside the block, to its terminator.
        skipping: bool,
    },
    Flushed,
}

struct StreamState {
    mode: Mode,
    pending: Zeroizing<Vec<u8>>,
    /// Raw bytes consumed, including `pending`.
    raw: u64,
    stats: StreamStats,
    record: Option<Vec<RawSpan>>,
}

fn terminator(bytes: &[u8]) -> Option<usize> {
    bytes.iter().position(|&b| b == b'\n' || b == b'\r')
}
fn is_delimiter(b: u8) -> bool {
    is_ascii_space(b) || b"\"',;&|<>(){}[]".contains(&b)
}
fn last_terminator(line: &[u8]) -> Option<u8> {
    line.last().copied().filter(|b| matches!(b, b'\n' | b'\r'))
}

/// A body line of an open private-key block (PEM block handling).
pub(crate) fn is_body_line(line: &[u8]) -> bool {
    let mut s = line.strip_suffix(b"\n").unwrap_or(line);
    let mut n = 0;
    while n < 16 && s.first().is_some_and(|b| b" \t+->#\"'".contains(b)) {
        s = &s[1..];
        n += 1;
    }
    n = 0;
    while n < 16 && s.last().is_some_and(|b| b" \t\r\"',\\".contains(b)) {
        s = &s[..s.len() - 1];
        n += 1;
    }
    if s.is_empty() {
        return true;
    }
    if s.len() <= 128
        && s.iter()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'/' | b'='))
    {
        return true;
    }
    if s.len() > 256 {
        return false;
    }
    let Some(colon) = s.iter().position(|&b| b == b':') else {
        return false;
    };
    let name = &s[..colon];
    (1..=32).contains(&name.len())
        && name[0].is_ascii_alphabetic()
        && name.iter().all(|b| b.is_ascii_alphanumeric() || *b == b'-')
        && matches!(s.get(colon + 1), None | Some(b' '))
}

impl StreamState {
    fn new() -> Self {
        Self {
            mode: Mode::Scanning,
            pending: Zeroizing::new(Vec::new()),
            raw: 0,
            stats: StreamStats::default(),
            record: None,
        }
    }
    fn store(&mut self, out: &mut Vec<u8>, bytes: &[u8]) {
        out.extend_from_slice(bytes);
        self.stats.stored_bytes += bytes.len() as u64;
    }
    fn loss(&mut self, out: &mut Vec<u8>, count: u64, reason: LossReason) {
        if count == 0 {
            return;
        }
        let stats = &mut self.stats;
        if stats.loss_spans.len() < MAX_LOSS_SPANS {
            stats.loss_spans.push(LossSpan {
                stored_offset: stats.stored_bytes,
                withheld_bytes: count,
                reason,
            });
        }
        stats.loss_span_count += 1;
        stats.withheld_bytes += count;
        let marker = format!("[ASURA_WITHHELD {count} bytes: {}]", reason.as_str());
        self.store(out, marker.as_bytes());
    }
    fn replace(
        &mut self,
        out: &mut Vec<u8>,
        number: u32,
        kind: SecretKind,
        literal: bool,
        start: u64,
        end: u64,
    ) {
        self.store(out, placeholder(number).as_bytes());
        let stats = &mut self.stats;
        stats.replacements += 1;
        stats.unnumbered_replacements += u64::from(number == 0);
        stats.literal_replacements += u64::from(literal);
        stats.kind_counts[kind.index()] += 1;
        if let Some(record) = &mut self.record {
            record.push(RawSpan {
                start,
                end,
                kind,
                number,
            });
        }
    }
    fn held(&self) -> usize {
        self.pending.len()
            + match &self.mode {
                Mode::PemOpen { block, .. } => block.len(),
                _ => 0,
            }
    }

    fn scan(&mut self, sh: &mut Shared, chunk: &[u8], out: &mut Vec<u8>) {
        self.stats.observed_bytes += chunk.len() as u64;
        if matches!(self.mode, Mode::Flushed) {
            // Bytes after stream end were never checked by a line rule.
            self.raw += chunk.len() as u64;
            self.stats.degraded = true;
            self.loss(out, chunk.len() as u64, LossReason::DetectorError);
            return;
        }
        let mut rest = chunk;
        while !rest.is_empty() {
            match &mut self.mode {
                Mode::Withholding { count, reason } => {
                    if let Some(i) = terminator(rest) {
                        let (count, reason) = (*count + i as u64, *reason);
                        self.mode = Mode::Scanning;
                        self.raw += i as u64 + 1;
                        self.loss(out, count, reason);
                        self.store(out, &rest[i..=i]);
                        rest = &rest[i + 1..];
                    } else {
                        *count += rest.len() as u64;
                        self.raw += rest.len() as u64;
                        rest = &[];
                    }
                    continue;
                }
                Mode::PemOverflow {
                    count,
                    last_terminator,
                    skipping: skipping @ true,
                    ..
                } => {
                    if let Some(i) = terminator(rest) {
                        *count += i as u64 + 1;
                        *last_terminator = Some(rest[i]);
                        *skipping = false;
                        self.raw += i as u64 + 1;
                        rest = &rest[i + 1..];
                    } else {
                        *count += rest.len() as u64;
                        self.raw += rest.len() as u64;
                        rest = &[];
                    }
                    continue;
                }
                _ => {}
            }
            let room = CARRY_LIMIT - self.held();
            match terminator(&rest[..rest.len().min(room)]) {
                Some(i) => {
                    self.raw += i as u64 + 1;
                    if self.pending.is_empty() {
                        self.line(sh, &rest[..=i], self.raw - i as u64 - 1, out);
                    } else {
                        self.pending.extend_from_slice(&rest[..=i]);
                        let mut held = std::mem::take(&mut self.pending);
                        self.line(sh, &held, self.raw - held.len() as u64, out);
                        held.zeroize();
                        self.pending = held;
                    }
                    rest = &rest[i + 1..];
                }
                None if rest.len() < room => {
                    self.pending.extend_from_slice(rest);
                    self.raw += rest.len() as u64;
                    rest = &[];
                }
                None => {
                    self.pending.extend_from_slice(&rest[..room]);
                    self.raw += room as u64;
                    rest = &rest[room..];
                    self.overflow(sh, out);
                }
            }
        }
    }

    /// The carry reached `CARRY_LIMIT` without a line terminator (SD5).
    fn overflow(&mut self, sh: &mut Shared, out: &mut Vec<u8>) {
        let mut held = std::mem::take(&mut self.pending);
        match std::mem::replace(&mut self.mode, Mode::Scanning) {
            Mode::PemOpen { block, finder, .. } => {
                self.mode = Mode::PemOverflow {
                    count: (block.len() + held.len()) as u64,
                    finder,
                    last_terminator: None,
                    skipping: true,
                };
            }
            Mode::PemOverflow { count, finder, .. } => {
                self.mode = Mode::PemOverflow {
                    count: count + held.len() as u64,
                    finder,
                    last_terminator: None,
                    skipping: true,
                };
            }
            _ => self.long_line(sh, &held, self.raw - held.len() as u64, out),
        }
        held.zeroize();
        self.pending = held;
    }

    /// Scan the held bytes as a line; release up to the last delimiter, never
    /// part of a value; withhold the rest of the line.
    fn long_line(&mut self, sh: &mut Shared, bytes: &[u8], start: u64, out: &mut Vec<u8>) {
        sh.literals.prepare();
        let mut candidates = Vec::new();
        sh.literals.find(bytes, &mut candidates);
        if let Some(begin) = sh.rules.candidates(bytes, &mut candidates) {
            candidates.push(Candidate::block(begin.start, bytes.len()));
        }
        let merged = merge(candidates);
        let mut cut = bytes
            .iter()
            .rposition(|&b| is_delimiter(b))
            .map_or(0, |i| i + 1);
        if let Some(span) = merged.iter().find(|m| m.start < cut && m.end > cut) {
            cut = span.start;
        }
        let released: Vec<_> = merged.into_iter().filter(|m| m.end <= cut).collect();
        self.release(sh, &bytes[..cut], start, &released, out);
        self.mode = Mode::Withholding {
            count: (bytes.len() - cut) as u64,
            reason: LossReason::LineOverLimit,
        };
    }

    /// One complete line, terminator included, or the final unterminated line.
    fn line(&mut self, sh: &mut Shared, line: &[u8], start: u64, out: &mut Vec<u8>) {
        match std::mem::replace(&mut self.mode, Mode::Scanning) {
            Mode::PemOpen {
                mut block,
                start: block_start,
                finder,
            } => {
                if let Some(end) = finder.find(line) {
                    let total = block.len() + end;
                    if total >= CARRY_LIMIT {
                        self.loss(out, total as u64, LossReason::KeyBlockOverLimit);
                    } else {
                        block.extend_from_slice(&line[..end]);
                        self.replace_block(sh, &block, block_start, out);
                    }
                    self.scan_line(sh, &line[end..], start + end as u64, out);
                } else if is_body_line(line) {
                    if block.len() + line.len() >= CARRY_LIMIT {
                        self.mode = Mode::PemOverflow {
                            count: (block.len() + line.len()) as u64,
                            finder,
                            last_terminator: last_terminator(line),
                            skipping: false,
                        };
                    } else {
                        block.extend_from_slice(line);
                        self.mode = Mode::PemOpen {
                            block,
                            start: block_start,
                            finder,
                        };
                    }
                } else {
                    self.close_block(sh, &block, block_start, out);
                    self.scan_line(sh, line, start, out);
                }
            }
            Mode::PemOverflow {
                count,
                finder,
                last_terminator: last,
                skipping,
            } => {
                if let Some(end) = finder.find(line) {
                    self.loss(out, count + end as u64, LossReason::KeyBlockOverLimit);
                    self.scan_line(sh, &line[end..], start + end as u64, out);
                } else if is_body_line(line) {
                    self.mode = Mode::PemOverflow {
                        count: count + line.len() as u64,
                        finder,
                        last_terminator: last_terminator(line),
                        skipping,
                    };
                } else {
                    self.close_overflow(out, count, last);
                    self.scan_line(sh, line, start, out);
                }
            }
            _ => self.scan_line(sh, line, start, out),
        }
    }

    fn scan_line(&mut self, sh: &mut Shared, line: &[u8], start: u64, out: &mut Vec<u8>) {
        if line.is_empty() {
            return;
        }
        sh.literals.prepare();
        let mut candidates = Vec::new();
        sh.literals.find(line, &mut candidates);
        match sh.rules.candidates(line, &mut candidates) {
            None => {
                let merged = merge(candidates);
                self.release(sh, line, start, &merged, out);
            }
            Some(begin) => {
                // Release the text before BEGIN; hold the block (B1). A
                // candidate that reaches into the block is cut at BEGIN.
                let b = begin.start;
                for c in &mut candidates {
                    c.end = c.end.min(b);
                }
                candidates.retain(|c| c.start < c.end);
                let merged = merge(candidates);
                self.release(sh, &line[..b], start, &merged, out);
                self.mode = Mode::PemOpen {
                    block: Zeroizing::new(line[b..].to_vec()),
                    start: start + b as u64,
                    finder: begin.finder,
                };
            }
        }
    }

    fn release(
        &mut self,
        sh: &mut Shared,
        bytes: &[u8],
        start: u64,
        merged: &[super::rules::Merged],
        out: &mut Vec<u8>,
    ) {
        let mut cursor = 0;
        for m in merged {
            self.store(out, &bytes[cursor..m.start]);
            let value = &bytes[m.start..m.end];
            let literal = m.literal_number();
            let number = match literal {
                Some(n) => n,
                None => {
                    let digest = sh.numbers.digest(value);
                    let n = sh.numbers.assign(digest, m.kind);
                    if m.from_rule {
                        sh.literals.propagate(digest, value, n, m.kind);
                    }
                    n
                }
            };
            self.replace(
                out,
                number,
                m.kind,
                literal.is_some(),
                start + m.start as u64,
                start + m.end as u64,
            );
            cursor = m.end;
        }
        self.store(out, &bytes[cursor..]);
    }

    /// The whole block, BEGIN to END, becomes one `private-key` placeholder.
    fn replace_block(&mut self, sh: &mut Shared, block: &[u8], start: u64, out: &mut Vec<u8>) {
        let digest = sh.numbers.digest(block);
        let number = sh.numbers.assign(digest, SecretKind::PrivateKey);
        sh.literals
            .propagate(digest, block, number, SecretKind::PrivateKey);
        self.replace(
            out,
            number,
            SecretKind::PrivateKey,
            false,
            start,
            start + block.len() as u64,
        );
    }

    /// An unterminated block, BEGIN to the last body line, is replaced whole;
    /// the last line's terminator is stored after the placeholder.
    fn close_block(&mut self, sh: &mut Shared, block: &[u8], start: u64, out: &mut Vec<u8>) {
        let t = usize::from(last_terminator(block).is_some());
        let (value, tail) = block.split_at(block.len() - t);
        self.replace_block(sh, value, start, out);
        self.store(out, tail);
    }

    fn close_overflow(&mut self, out: &mut Vec<u8>, count: u64, last: Option<u8>) {
        let t = u64::from(last.is_some());
        self.loss(out, count - t, LossReason::KeyBlockOverLimit);
        if let Some(b) = last {
            self.store(out, &[b]);
        }
    }

    fn finish(&mut self, sh: &mut Shared, out: &mut Vec<u8>) {
        match std::mem::replace(&mut self.mode, Mode::Scanning) {
            Mode::Flushed => {}
            Mode::Withholding { count, reason } => self.loss(out, count, reason),
            Mode::PemOverflow {
                count,
                skipping: true,
                ..
            } => self.loss(out, count, LossReason::KeyBlockOverLimit),
            mode => {
                self.mode = mode;
                if !self.pending.is_empty() {
                    let mut held = std::mem::take(&mut self.pending);
                    self.line(sh, &held, self.raw - held.len() as u64, out);
                    held.zeroize();
                }
                match std::mem::replace(&mut self.mode, Mode::Scanning) {
                    Mode::PemOpen { block, start, .. } => self.close_block(sh, &block, start, out),
                    Mode::PemOverflow {
                        count,
                        last_terminator,
                        ..
                    } => self.close_overflow(out, count, last_terminator),
                    _ => {}
                }
            }
        }
        self.mode = Mode::Flushed;
    }

    fn detector_error(&mut self, undecided: u64, out: &mut Vec<u8>) {
        self.stats.observed_bytes += undecided;
        self.raw += undecided;
        let mut count = undecided + self.pending.len() as u64;
        self.pending.zeroize();
        count += match std::mem::replace(&mut self.mode, Mode::Scanning) {
            Mode::Withholding { count, .. } | Mode::PemOverflow { count, .. } => count,
            Mode::PemOpen { block, .. } => block.len() as u64,
            Mode::Flushed => {
                self.mode = Mode::Flushed;
                0
            }
            Mode::Scanning => 0,
        };
        self.stats.degraded = true;
        self.loss(out, count, LossReason::DetectorError);
    }
}

/// Per-job redaction state: one scanner per stream, shared numbering and
/// literals. Created at admission, moved to the capture worker, dropped at
/// settlement; dropping zeroizes the key, the values and the carry.
pub struct JobRedaction {
    shared: Shared,
    streams: [StreamState; 2],
}
impl std::fmt::Debug for JobRedaction {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JobRedaction")
            .field("distinct_numbers", &self.shared.numbers.map.len())
            .finish_non_exhaustive()
    }
}
impl JobRedaction {
    /// `command_secrets` are the rewrite's variables: their values become
    /// literals with their numbers, so an echo maps to the variable (SD3).
    pub fn new(rules: &'static RuleSet, command_secrets: &[SecretVariable]) -> Self {
        let mut shared = Shared {
            rules,
            numbers: Numbers::new(),
            literals: Literals {
                entries: Vec::new(),
                known: HashSet::new(),
                propagated: 0,
                propagated_bytes: 0,
                automaton: None,
                dirty: false,
            },
        };
        for secret in command_secrets {
            let number = u32::from(secret.number());
            let digest = shared.numbers.digest(secret.value());
            if shared.numbers.map.insert(digest, number).is_none() {
                shared
                    .numbers
                    .record(number, secret.kind(), NumberOrigin::Command);
            }
            shared.numbers.next = shared.numbers.next.max(number + 1);
            shared
                .literals
                .add(digest, secret.value(), number, secret.kind());
        }
        Self {
            shared,
            streams: [StreamState::new(), StreamState::new()],
        }
    }
    /// Scans one chunk; returns the bytes it releases.
    pub fn scan(&mut self, stream: Stream, chunk: &[u8]) -> Redacted {
        let mut out = Vec::with_capacity(chunk.len());
        self.streams[stream.index()].scan(&mut self.shared, chunk, &mut out);
        Redacted(out)
    }
    /// Pipe EOF: scans the tail as a final line and closes any open state.
    pub fn finish(&mut self, stream: Stream) -> Redacted {
        let mut out = Vec::new();
        self.streams[stream.index()].finish(&mut self.shared, &mut out);
        Redacted(out)
    }
    /// The scanner failed on a chunk: its `undecided_bytes` and the stream's
    /// carry are withheld with `detector_error`; the job continues (SD2).
    pub fn detector_error(&mut self, stream: Stream, undecided_bytes: u64) -> Redacted {
        let mut out = Vec::new();
        self.streams[stream.index()].detector_error(undecided_bytes, &mut out);
        Redacted(out)
    }
    pub fn stats(&self, stream: Stream) -> &StreamStats {
        &self.streams[stream.index()].stats
    }
    pub fn distinct_numbers(&self) -> u32 {
        self.shared.numbers.map.len() as u32
    }
    /// Flushes both streams and drops values, numbers, literals and the key.
    /// Returns the facts and each stream's final released bytes.
    pub fn settle(mut self) -> (JobSummary, [Redacted; 2]) {
        let tails = [self.finish(Stream::Stdout), self.finish(Stream::Stderr)];
        let [stdout, stderr] = &self.streams;
        let summary = JobSummary {
            numbers: self.shared.numbers.info.clone(),
            distinct_numbers: self.shared.numbers.map.len() as u32,
            stdout: stdout.stats.clone(),
            stderr: stderr.stats.clone(),
        };
        (summary, tails)
    }
}

/// Whole-buffer mode: the same scanner fed one chunk and flushed. Returns the
/// replaced spans in input offsets and whether any byte was withheld.
pub(crate) fn scan_spans(rules: &'static RuleSet, bytes: &[u8]) -> (Vec<RawSpan>, bool) {
    let mut job = JobRedaction::new(rules, &[]);
    job.streams[0].record = Some(Vec::new());
    let mut out = job.scan(Stream::Stdout, bytes).into_bytes();
    out.extend(job.finish(Stream::Stdout).into_bytes());
    let state = &mut job.streams[0];
    (
        state.record.take().unwrap_or_default(),
        state.stats.withheld_bytes > 0 || state.stats.degraded,
    )
}

/// Whole-buffer mode returning the redacted text, for display records.
pub(crate) fn redact_whole(rules: &'static RuleSet, bytes: &[u8]) -> Redacted {
    let mut job = JobRedaction::new(rules, &[]);
    let mut out = job.scan(Stream::Stdout, bytes).into_bytes();
    out.extend(job.finish(Stream::Stdout).into_bytes());
    Redacted(out)
}
