//! Per-request composition of a turn's later requests (context-composer.md,
//! CC4 request composition, Q5). Pure: equal inputs give equal bytes and
//! manifest. Earlier entries stay byte-stable unless the budget requires a
//! reduction, so provider prefix caches survive.
use super::*;

/// The request manifest's format byte (kind 21 manifests use 1).
const REQUEST_FORMAT: u8 = 2;
/// Segment kinds of the continuation (composer revision 5).
const TOOL_CALL: u8 = 8;
const TOOL_RESULT: u8 = 9;

/// One settled call of the current turn, as later requests show it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Pair {
    /// The turn ordinal (1 to 201).
    pub ordinal: u32,
    /// The request that proposed the call.
    pub request: u32,
    /// The kind 14 name; never empty.
    pub name: String,
    /// The kind 14 stored argument text when it is a JSON object, else `{}`.
    pub arguments: String,
    /// The rendered result text (`tools::render_reply`).
    pub output: String,
    /// The job the result names, for the reference line.
    pub job: Option<Id>,
}

/// Request 1's composition and the sources that built it, kept for the turn.
pub(crate) struct TurnBase {
    pub instructions: &'static str,
    pub prompt: String,
    pub candidates: Vec<Candidate>,
    pub unlisted: u32,
    pub notice: Notice,
    pub views: Vec<Option<NoticeView>>,
    pub budget: Budget,
    /// Request 1's input as composed and accepted.
    pub input: pb::ModelInput,
    /// Kind 21's manifest digest.
    pub manifest_digest: Hash,
}

/// The previous request's measured input (`ContextMeasured`): its tokens and
/// its encoded size. Kind 33 keeps `tokens` for recomputation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Measured {
    pub tokens: u32,
    pub bytes: u64,
}
/// Tokens counted for each appended entry's template markers (CC4 step 2).
const ENTRY_TOKENS: u64 = 32;

#[derive(Debug)]
pub(crate) struct ComposedRequest {
    pub input: Vec<u8>,
    pub manifest_digest: Hash,
    /// Results shown as reference lines.
    pub reduced: u32,
    /// The base was recomposed under budget pressure (CC-F4).
    pub recomposed: bool,
}

/// The service's call ID of a turn ordinal.
pub(crate) fn call_id(ordinal: u32) -> String {
    format!("call_{ordinal}")
}
fn reference(pair: &Pair) -> String {
    let mut text = format!(
        "[Asura: tool result {} ({}) not shown for space; {} bytes]",
        pair.ordinal,
        pair.name,
        pair.output.len()
    );
    if let Some(job) = pair.job {
        text.push_str("\njob: ");
        for byte in job {
            text.push_str(&format!("{byte:02x}"));
        }
    }
    text
}
fn tool_entry(pair: &Pair, text: String) -> pb::HistoryTurn {
    pb::HistoryTurn {
        role: Some(3),
        text: Some(text),
        tool_calls: Vec::new(),
        call_id: Some(call_id(pair.ordinal)),
        tool_name: Some(pair.name.clone()),
    }
}
/// Groups of one assistant entry and its tool entries, in request order.
fn groups(pairs: &[Pair]) -> Vec<&[Pair]> {
    let mut out = Vec::new();
    let mut start = 0;
    for index in 1..=pairs.len() {
        if index == pairs.len() || pairs[index].request != pairs[start].request {
            out.push(&pairs[start..index]);
            start = index;
        }
    }
    out
}
fn call_entry(group: &[Pair]) -> pb::HistoryTurn {
    pb::HistoryTurn {
        role: Some(2),
        text: Some(String::new()),
        tool_calls: group
            .iter()
            .map(|pair| pb::HistoryToolCall {
                call_id: Some(call_id(pair.ordinal)),
                name: Some(pair.name.clone()),
                arguments: Some(pair.arguments.clone()),
            })
            .collect(),
        call_id: None,
        tool_name: None,
    }
}
/// Encoded size of one entry of the repeated `continuation` field: each
/// present field is a one-byte key and its value (the role a one-byte varint).
fn entry_size(entry: &pb::HistoryTurn) -> u64 {
    let text = |value: &Option<String>| value.as_ref().map_or(0, |t| field_len(t.len() as u64));
    let calls: u64 = entry
        .tool_calls
        .iter()
        .map(|call| field_len(text(&call.call_id) + text(&call.name) + text(&call.arguments)))
        .sum();
    field_len(2 + text(&entry.text) + calls + text(&entry.call_id) + text(&entry.tool_name))
}

/// Composes request `request` (2 or more) from the base and the settled
/// pairs, in turn-ordinal order (CC-F4). `measured` is the previous request's
/// measured input, when the provider reported it. `W` is an estimate: a
/// request still above it after the reductions is sent, and the helper's
/// exact count or the provider's window decides (step 5).
pub(crate) fn compose_request(
    base: &TurnBase,
    pairs: &[Pair],
    request: u32,
    measured: Option<Measured>,
) -> Result<ComposedRequest, Error> {
    let limit = base.budget.bytes().unwrap_or(0);
    if pairs.is_empty() || request < 2 {
        return Err(Error::Invalid);
    }
    let latest = pairs.iter().map(|pair| pair.request).max().unwrap_or(0);
    let groups = groups(pairs);
    // Fixed parts: the assistant entries; each result full or a reference.
    let calls: u64 = groups.iter().map(|g| entry_size(&call_entry(g))).sum();
    let full: Vec<u64> = pairs
        .iter()
        .map(|p| entry_size(&tool_entry(p, p.output.clone())))
        .collect();
    let short: Vec<u64> = pairs
        .iter()
        .map(|p| entry_size(&tool_entry(p, reference(p))))
        .collect();
    let mut shown = vec![true; pairs.len()];
    let base_size = model::encode_input(&base.input)
        .map_err(|_| Error::Invalid)?
        .len() as u64;
    let mut continuation = calls + full.iter().sum::<u64>();
    // Step 2: the exact-count bound from the previous request's measured
    // tokens: one token covers at least one byte; each appended entry adds
    // its template markers.
    let latest_group = groups.last().map_or(0, |g| g.len() as u64 + 1);
    let room = u64::from(base.budget.window_tokens)
        .saturating_sub(u64::from(base.budget.output_reservation));
    let measured_fits = measured.is_some_and(|m| {
        let appended = (base_size + continuation).saturating_sub(m.bytes);
        u64::from(m.tokens) + appended + ENTRY_TOKENS * latest_group <= room
    });
    // Step 3: older results become references, oldest first.
    for (index, pair) in pairs.iter().enumerate() {
        if measured_fits || base_size + continuation <= limit {
            break;
        }
        if pair.request == latest || short[index] >= full[index] {
            continue;
        }
        shown[index] = false;
        continuation = continuation - full[index] + short[index];
    }
    // Step 4: recompose the base within what the continuation leaves.
    let mut base_input = base.input.clone();
    let mut base_digest = base.manifest_digest;
    let mut recomposed = false;
    if !measured_fits && base_size + continuation > limit {
        // A base that cannot fit the room stays as composed (step 4).
        if let Ok(composed) = compose_views_reserving(
            base.instructions,
            &base.prompt,
            &base.candidates,
            base.unlisted,
            &base.notice,
            base.budget,
            &base.views,
            continuation,
        ) {
            base_input = model::decode_input(&composed.input).map_err(|_| Error::Invalid)?;
            base_digest = composed.manifest_digest;
            recomposed = true;
        }
    }
    let mut entries = Vec::with_capacity(pairs.len() + groups.len());
    let mut offset = 0;
    for group in &groups {
        entries.push(call_entry(group));
        for pair in group.iter() {
            let text = if shown[offset] {
                pair.output.clone()
            } else {
                reference(pair)
            };
            entries.push(tool_entry(pair, text));
            offset += 1;
        }
    }
    base_input.continuation = entries;
    // Step 5: the latest pairs are mandatory and the request is sent; the
    // `Begin` sanity bound (capacity × 8 bytes) is the model owner's check.
    let bytes = model::encode_input(&base_input).map_err(|_| Error::Invalid)?;
    let manifest_digest = request_manifest(
        request,
        measured.map_or(0, |m| m.tokens),
        base_digest,
        pairs,
        &shown,
    );
    Ok(ComposedRequest {
        input: bytes,
        manifest_digest,
        reduced: shown.iter().filter(|s| !**s).count() as u32,
        recomposed,
    })
}

/// The request manifest digest (context-composer.md, request manifest and
/// kind 33): format, revision, request, base digest, then each continuation
/// segment in input order.
fn request_manifest(request: u32, tokens: u32, base: Hash, pairs: &[Pair], shown: &[bool]) -> Hash {
    let mut h = Sha256::new();
    h.update([REQUEST_FORMAT]);
    h.update(COMPOSER_REVISION.to_be_bytes());
    h.update(request.to_be_bytes());
    h.update(tokens.to_be_bytes());
    h.update(base);
    let segment =
        |h: &mut Sha256, kind: u8, role: u8, full: bool, ordinal: u32, source: &str, text: &str| {
            h.update([kind, role, if full { 3 } else { 2 }]);
            h.update(ordinal.to_be_bytes());
            h.update(Sha256::digest(source.as_bytes()));
            h.update((text.len() as u32).to_be_bytes());
            h.update(Sha256::digest(text.as_bytes()));
        };
    let mut offset = 0;
    for group in groups(pairs) {
        for pair in group {
            segment(
                &mut h,
                TOOL_CALL,
                2,
                true,
                pair.ordinal,
                &pair.arguments,
                &pair.arguments,
            );
        }
        for pair in group {
            let text = if shown[offset] {
                pair.output.clone()
            } else {
                reference(pair)
            };
            segment(
                &mut h,
                TOOL_RESULT,
                3,
                shown[offset],
                pair.ordinal,
                &pair.output,
                &text,
            );
            offset += 1;
        }
    }
    h.finalize().into()
}

#[cfg(test)]
#[path = "request_tests.rs"]
mod tests;
