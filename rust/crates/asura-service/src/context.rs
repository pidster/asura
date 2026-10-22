//! Context composer: per-request model input from the model's window (context-composer.md).
//! The core is pure: equal candidates and budget give equal bytes and manifest.
use asura_control::model::{self, pb};
use asura_storage::authority::conversation::{Composition, Hash, Id, MAX_COMPOSED_PRIOR};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, VecDeque};

mod job_notice;
#[allow(unused_imports)] // J5-E builds the notice inputs.
pub(crate) use job_notice::{
    MAX_NOTICE_ENTRIES, NOTICE_SETTLEMENT_READ, Notice, NoticeError, NoticeInputs, Settlement,
};

/// Versions the composition rules and the manifest encoding. Revision 2
/// adds the J5 job notice and its segments.
pub(crate) const COMPOSER_REVISION: u32 = 2;
/// Calibration before enough measurements: one token per byte (D2).
const FALLBACK_MILLI: u32 = 1000;
/// Upper bound on the learned bytes per token.
const MAX_MILLI: u32 = 8000;
const CALIBRATION_PAIRS: usize = 16;
const CALIBRATION_MINIMUM: usize = 3;
const CALIBRATION_MODELS: usize = 32;
/// Encode attempts before `context_limit` (CC-F1 step 7).
const ENCODE_ATTEMPTS: usize = 4;

/// One completed prior turn, as the journal recorded it.
pub(crate) struct Candidate {
    pub operation: Id,
    pub generation: u64,
    pub prompt: String,
    pub answer: String,
}

/// `R = ⌊E ÷ 4⌋`: room the composer leaves for the answer (D3 refinement,
/// owner decision 2026-10-19, context-composer.md "Output reservation R").
/// A composition target, not an output cap: the helper never receives it and
/// the model writes until it stops or the window is full.
pub(crate) fn output_reservation(window_tokens: u32) -> u32 {
    window_tokens / 4
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Budget {
    pub window_tokens: u32,
    pub output_reservation: u32,
    pub overhead_tokens: u32,
    pub bytes_per_token_milli: u32,
}
impl Budget {
    /// `W = (E − R − S) × k`, in bytes; `None` when `B ≤ 0`.
    fn bytes(&self) -> Option<u64> {
        let tokens = u64::from(self.window_tokens)
            .checked_sub(u64::from(self.output_reservation))?
            .checked_sub(u64::from(self.overhead_tokens))?;
        (tokens > 0).then(|| tokens * u64::from(self.bytes_per_token_milli) / 1000)
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Error {
    /// Mandatory segments do not fit, or no input budget remains.
    ContextLimit {
        mandatory_bytes: u64,
        budget_bytes: u64,
    },
    /// The composed input failed wire validation.
    Invalid,
}

#[derive(Debug)]
#[cfg_attr(not(test), allow(dead_code))] // J5-E records the notice fields.
pub(crate) struct Composed {
    pub input: Vec<u8>,
    /// Shown prior turns (full or reference), in generation order.
    pub shown: Vec<Id>,
    pub omitted: u32,
    pub manifest_digest: Hash,
    /// `notice_through` for kind 21: the notice cursor `S` (J5-E2).
    pub notice_through: u64,
    /// Notice entries at reference fidelity, oldest first (J5T-05).
    pub notice_reduced: u32,
}
impl Composed {
    pub(crate) fn composition(&self, budget: Budget) -> Composition {
        Composition {
            omitted_turns: self.omitted,
            manifest_digest: self.manifest_digest,
            composer_revision: COMPOSER_REVISION,
            window_tokens: budget.window_tokens,
            output_reservation: budget.output_reservation,
            overhead_tokens: budget.overhead_tokens,
            bytes_per_token_milli: budget.bytes_per_token_milli,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Fidelity {
    Reference = 2,
    Full = 3,
}

fn varint_len(value: u64) -> u64 {
    (64 - (value | 1).leading_zeros() as u64).div_ceil(7)
}
/// Encoded size of a length-delimited field with a one-byte key.
fn field_len(payload: u64) -> u64 {
    1 + varint_len(payload) + payload
}
/// Encoded size of one `HistoryTurn` entry inside `ModelInput.history`.
fn entry_len(text: &str) -> u64 {
    // role: key and one varint byte; text: length-delimited.
    field_len(2 + field_len(text.len() as u64))
}
fn reference_texts(turn: &Candidate) -> (String, String) {
    (
        format!(
            "[Asura: earlier request {} not shown for space; {} bytes]",
            turn.generation,
            turn.prompt.len()
        ),
        format!(
            "[Asura: earlier answer {} not shown for space; {} bytes]",
            turn.generation,
            turn.answer.len()
        ),
    )
}
fn turn_len(turn: &Candidate, fidelity: Fidelity) -> u64 {
    match fidelity {
        Fidelity::Full => entry_len(&turn.prompt) + entry_len(&turn.answer),
        Fidelity::Reference => {
            let (user, model) = reference_texts(turn);
            entry_len(&user) + entry_len(&model)
        }
    }
}
/// The labelled, service-written context block placed before the person's text.
/// `unlisted` counts older completed turns that `Prepare` did not return
/// (past its 8 MiB candidate bound); their size is not known here.
/// It holds, in order, the omitted-range line and the job notice (D9).
fn context_block(omitted: &[&Candidate], unlisted: u32, notice: &str) -> String {
    if omitted.is_empty() && unlisted == 0 && notice.is_empty() {
        return String::new();
    }
    let bytes: usize = omitted
        .iter()
        .map(|t| t.prompt.len() + t.answer.len())
        .sum();
    let range = if unlisted > 0 {
        format!(
            "Earlier turns omitted for space: {} turns, more than {} bytes.\n",
            omitted.len() as u64 + u64::from(unlisted),
            bytes
        )
    } else if !omitted.is_empty() {
        format!(
            "Earlier turns omitted for space: {} turns, {} bytes.\n",
            omitted.len(),
            bytes
        )
    } else {
        String::new()
    };
    format!("<asura-context>\n{range}{notice}</asura-context>\n\n")
}

/// Compose one turn-start request. `history` may arrive in any order.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn compose(
    instructions: &str,
    prompt: &str,
    history: &[Candidate],
    budget: Budget,
) -> Result<Composed, Error> {
    compose_with(instructions, prompt, history, 0, budget)
}
/// [`compose`] when `unlisted` older completed turns were not returned as
/// candidates; they join the omitted range (segmented journal, SJ-D10).
pub(crate) fn compose_with(
    instructions: &str,
    prompt: &str,
    history: &[Candidate],
    unlisted: u32,
    budget: Budget,
) -> Result<Composed, Error> {
    compose_turn(
        instructions,
        prompt,
        history,
        unlisted,
        &Notice::none([0; 16], 0),
        budget,
    )
}
/// [`compose_with`] with the turn's job notice (J5). Notice entries are
/// mandatory: when the budget is short they are reduced to reference
/// fidelity, oldest first; if even the reduced notice does not fit, the
/// turn fails with `context_limit`.
#[cfg_attr(not(test), allow(dead_code))] // J5-E calls it at turn start.
pub(crate) fn compose_turn(
    instructions: &str,
    prompt: &str,
    history: &[Candidate],
    unlisted: u32,
    notice: &Notice,
    budget: Budget,
) -> Result<Composed, Error> {
    let limit = budget.bytes().ok_or(Error::ContextLimit {
        mandatory_bytes: 0,
        budget_bytes: 0,
    })?;
    let mut ordered: Vec<&Candidate> = history.iter().collect();
    ordered.sort_by_key(|t| (std::cmp::Reverse(t.generation), t.operation));
    let turns = &ordered[..ordered.len().min(MAX_COMPOSED_PRIOR)];
    let mandatory = field_len(instructions.len() as u64) + field_len(prompt.len() as u64);
    // Reserve room for the largest possible context block, so the omitted line never
    // displaces a decision already made. The notice keeps as many entries at
    // full fidelity as fit; older entries are reduced first.
    let mut reduced = 0;
    let (notice_text, block_room) = loop {
        let text = notice.text(reduced);
        let room = context_block_room(history, unlisted, &text);
        if mandatory + room <= limit {
            break (text, room);
        }
        if reduced == notice.entries.len() {
            return Err(Error::ContextLimit {
                mandatory_bytes: mandatory + room,
                budget_bytes: limit,
            });
        }
        reduced += 1;
    };
    let mut used = mandatory + block_room;
    // Pass 1: references, newest first, until one does not fit.
    // A turn shorter than its reference line is shown in full from the start.
    let cheapest = |turn: &Candidate| {
        if turn_len(turn, Fidelity::Full) <= turn_len(turn, Fidelity::Reference) {
            Fidelity::Full
        } else {
            Fidelity::Reference
        }
    };
    let mut fidelity = Vec::new();
    for turn in turns {
        let level = cheapest(turn);
        let size = turn_len(turn, level);
        if used + size > limit {
            break;
        }
        used += size;
        fidelity.push(level);
    }
    // Pass 2: upgrade to full, newest first. A newer turn's full text outranks
    // older reference lines, so the oldest shown turns join the omitted range when
    // that makes room; nothing newer than the turn being upgraded is displaced.
    let mut upgrades = Vec::new();
    let mut index = 0;
    while index < fidelity.len() {
        if fidelity[index] == Fidelity::Reference {
            let turn = turns[index];
            let delta = turn_len(turn, Fidelity::Full) - turn_len(turn, Fidelity::Reference);
            while used + delta > limit && fidelity.len() > index + 1 {
                let last = fidelity.len() - 1;
                used -= turn_len(turns[last], fidelity[last]);
                fidelity.pop();
                upgrades.retain(|&upgraded| upgraded != last);
            }
            if used + delta <= limit {
                used += delta;
                fidelity[index] = Fidelity::Full;
                upgrades.push(index);
            }
        }
        index += 1;
    }
    for _ in 0..ENCODE_ATTEMPTS {
        let shown = &turns[..fidelity.len()];
        // Everything older than the shown turns, including any beyond the
        // shown-turn bound, joins one omitted range.
        let omitted = &ordered[fidelity.len()..];
        let block = context_block(omitted, unlisted, &notice_text);
        let input = build(instructions, &block, prompt, shown, &fidelity);
        let bytes = model::encode_input(&input).map_err(|_| Error::Invalid)?;
        if bytes.len() as u64 <= limit {
            let manifest_digest = manifest(
                &budget,
                instructions,
                &block,
                prompt,
                shown,
                &fidelity,
                omitted,
                notice,
                reduced,
            );
            let mut shown_ids: Vec<(u64, Id)> =
                shown.iter().map(|t| (t.generation, t.operation)).collect();
            shown_ids.sort();
            return Ok(Composed {
                input: bytes,
                shown: shown_ids.into_iter().map(|(_, id)| id).collect(),
                omitted: omitted.len() as u32 + unlisted,
                manifest_digest,
                notice_through: notice.through,
                notice_reduced: reduced as u32,
            });
        }
        match upgrades.pop() {
            Some(index) => fidelity[index] = Fidelity::Reference,
            None => {
                fidelity.pop();
            }
        }
    }
    Err(Error::ContextLimit {
        mandatory_bytes: mandatory,
        budget_bytes: limit,
    })
}
/// Room for the context block when every candidate is omitted.
fn context_block_room(history: &[Candidate], unlisted: u32, notice: &str) -> u64 {
    if history.is_empty() && unlisted == 0 && notice.is_empty() {
        return 0;
    }
    let all: Vec<&Candidate> = history.iter().collect();
    // The block joins the prompt field; its length can add varint bytes there.
    context_block(&all, unlisted, notice).len() as u64 + 2
}
fn build(
    instructions: &str,
    block: &str,
    prompt: &str,
    shown: &[&Candidate],
    fidelity: &[Fidelity],
) -> pb::ModelInput {
    let mut history = Vec::with_capacity(shown.len() * 2);
    for (turn, fidelity) in shown.iter().zip(fidelity).rev() {
        let (user, model) = match fidelity {
            Fidelity::Full => (turn.prompt.clone(), turn.answer.clone()),
            Fidelity::Reference => reference_texts(turn),
        };
        history.push(pb::HistoryTurn {
            role: Some(1),
            text: Some(user),
        });
        history.push(pb::HistoryTurn {
            role: Some(2),
            text: Some(model),
        });
    }
    pb::ModelInput {
        instructions: Some(instructions.into()),
        history,
        prompt: Some(format!("{block}{prompt}")),
    }
}
/// SHA-256 over the deterministic manifest encoding (context-composer.md, item 7).
#[allow(clippy::too_many_arguments)] // One input per manifest part, in encoding order.
fn manifest(
    budget: &Budget,
    instructions: &str,
    block: &str,
    prompt: &str,
    shown: &[&Candidate],
    fidelity: &[Fidelity],
    omitted: &[&Candidate],
    notice: &Notice,
    reduced: usize,
) -> Hash {
    let mut h = Sha256::new();
    h.update([1u8]);
    h.update(COMPOSER_REVISION.to_be_bytes());
    for value in [
        budget.window_tokens,
        budget.output_reservation,
        budget.overhead_tokens,
        budget.bytes_per_token_milli,
    ] {
        h.update(value.to_be_bytes());
    }
    let segment = |h: &mut Sha256,
                   kind: u8,
                   role: u8,
                   fidelity: u8,
                   source: Id,
                   source_text: &str,
                   shown: &str| {
        let digest: Hash = Sha256::digest(source_text.as_bytes()).into();
        h.update(segment_bytes(kind, role, fidelity, source, digest, shown));
    };
    segment(
        &mut h,
        INSTRUCTIONS,
        0,
        Fidelity::Full as u8,
        [0; 16],
        instructions,
        instructions,
    );
    for (turn, fidelity) in shown.iter().zip(fidelity).rev() {
        let (user, model) = match fidelity {
            Fidelity::Full => (turn.prompt.clone(), turn.answer.clone()),
            Fidelity::Reference => reference_texts(turn),
        };
        segment(
            &mut h,
            HISTORY,
            1,
            *fidelity as u8,
            turn.operation,
            &turn.prompt,
            &user,
        );
        segment(
            &mut h,
            HISTORY,
            2,
            *fidelity as u8,
            turn.operation,
            &turn.answer,
            &model,
        );
    }
    segment(
        &mut h,
        BLOCK,
        1,
        Fidelity::Full as u8,
        [0; 16],
        block,
        block,
    );
    h.update(notice_segments(notice, reduced));
    segment(
        &mut h,
        PROMPT,
        1,
        Fidelity::Full as u8,
        [0; 16],
        prompt,
        prompt,
    );
    let mut range: Vec<(u64, Id)> = omitted
        .iter()
        .map(|t| (t.generation, t.operation))
        .collect();
    range.sort();
    h.update((range.len() as u32).to_be_bytes());
    if let (Some(first), Some(last)) = (range.first(), range.last()) {
        h.update(first.1);
        h.update(last.1);
    }
    h.finalize().into()
}

const INSTRUCTIONS: u8 = 1;
const HISTORY: u8 = 2;
const BLOCK: u8 = 3;
const PROMPT: u8 = 4;
/// One notice entry; source identity the job ID, source digest the kind 28
/// frame header's command digest.
const JOB_NOTICE: u8 = 5;
/// The header, overflow and live lines; source identity the conversation
/// ID, source digest [`Notice::summary_digest`].
const JOB_NOTICE_SUMMARY: u8 = 6;

/// One manifest segment: kind, role, fidelity, source identity, source
/// digest, shown byte length (u64) and shown content digest.
fn segment_bytes(
    kind: u8,
    role: u8,
    fidelity: u8,
    source: Id,
    source_digest: Hash,
    shown: &str,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(3 + 16 + 32 + 8 + 32);
    out.extend_from_slice(&[kind, role, fidelity]);
    out.extend_from_slice(&source);
    out.extend_from_slice(&source_digest);
    out.extend_from_slice(&(shown.len() as u64).to_be_bytes());
    out.extend_from_slice(&Sha256::digest(shown.as_bytes()));
    out
}
/// The notice segments, after the context block segment: each entry in
/// settlement order at its fidelity, then the summary segment. None when
/// the turn has no notice block.
fn notice_segments(notice: &Notice, reduced: usize) -> Vec<u8> {
    let mut out = Vec::new();
    if notice.is_empty() {
        return out;
    }
    for (index, entry) in notice.entries.iter().enumerate() {
        let (fidelity, shown) = if index < reduced {
            (Fidelity::Reference, &entry.reference)
        } else {
            (Fidelity::Full, &entry.full)
        };
        out.extend(segment_bytes(
            JOB_NOTICE,
            1,
            fidelity as u8,
            entry.job,
            entry.digest,
            shown,
        ));
    }
    out.extend(segment_bytes(
        JOB_NOTICE_SUMMARY,
        1,
        Fidelity::Full as u8,
        notice.conversation,
        notice.summary_digest(),
        &notice.summary_text(),
    ));
    out
}

/// Per-model bytes-per-token lower bound, learned from measured input tokens (D2).
/// Memory only: a restart returns every model to the fallback.
#[derive(Default)]
pub(crate) struct Calibration {
    models: BTreeMap<String, VecDeque<u32>>,
    order: VecDeque<String>,
}
impl Calibration {
    /// `k` in milli bytes per token, and whether it was learned.
    pub(crate) fn milli(&self, model: &str) -> (u32, bool) {
        match self.models.get(model) {
            Some(ratios) if ratios.len() >= CALIBRATION_MINIMUM => {
                let smallest = *ratios.iter().min().expect("non-empty");
                (
                    (smallest as u64 * 4 / 5).clamp(1, MAX_MILLI as u64) as u32,
                    true,
                )
            }
            _ => (FALLBACK_MILLI, false),
        }
    }
    pub(crate) fn record(&mut self, model: &str, input_bytes: usize, input_tokens: u32) {
        if input_tokens == 0 || input_bytes == 0 {
            return;
        }
        let ratio =
            (input_bytes as u64 * 1000 / u64::from(input_tokens)).min(u32::MAX as u64) as u32;
        if !self.models.contains_key(model) {
            if self.order.len() == CALIBRATION_MODELS
                && let Some(oldest) = self.order.pop_front()
            {
                self.models.remove(&oldest);
            }
            self.order.push_back(model.into());
        }
        let ratios = self.models.entry(model.into()).or_default();
        if ratios.len() == CALIBRATION_PAIRS {
            ratios.pop_front();
        }
        ratios.push_back(ratio);
    }
    /// A helper `contextLimit` means the bound was wrong: return to the fallback.
    pub(crate) fn reset(&mut self, model: &str) {
        self.models.remove(model);
        self.order.retain(|m| m != model);
    }
}

#[cfg(test)]
mod journal_tests;
#[cfg(test)]
mod tests;
