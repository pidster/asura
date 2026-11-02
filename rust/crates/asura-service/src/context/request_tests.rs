//! CC4 request composition cases CC-U29 and CC-U30 (context-composer.md).
use super::*;

fn turn(generation: u64, size: usize) -> Candidate {
    Candidate {
        operation: [generation as u8; 16],
        generation,
        prompt: format!("question {generation} {}", "q".repeat(size)),
        answer: format!("answer {generation} {}", "a".repeat(size)),
        notice: Notice::none([0; 16], 0),
    }
}
/// Request 1 composed from `history` under a window of `window` tokens at
/// one byte per token, with no output reservation.
fn base(window: u32, history: Vec<Candidate>) -> TurnBase {
    let budget = Budget {
        window_tokens: window,
        output_reservation: 0,
        overhead_tokens: 0,
        bytes_per_token_milli: 1000,
    };
    let composed = compose_views(
        "rules",
        "do it",
        &history,
        0,
        &Notice::none([0; 16], 0),
        budget,
        &[],
    )
    .unwrap();
    TurnBase {
        instructions: "rules",
        prompt: "do it".into(),
        candidates: history,
        unlisted: 0,
        notice: Notice::none([0; 16], 0),
        views: Vec::new(),
        budget,
        input: model::decode_input(&composed.input).unwrap(),
        manifest_digest: composed.manifest_digest,
    }
}
fn pair(ordinal: u32, request: u32, output: usize) -> Pair {
    Pair {
        ordinal,
        request,
        name: "exec_cmd".into(),
        arguments: format!(r#"{{"command":"step {ordinal}"}}"#),
        output: format!("job: {ordinal}\n{}", "o".repeat(output)),
        job: Some([ordinal as u8; 16]),
    }
}

/// CC-U29: with room, the base is unchanged and each later request extends
/// the earlier one byte for byte: the earlier input is a prefix.
#[test]
fn cc_u29_requests_extend_the_base_byte_for_byte() {
    let base = base(100_000, (1..=3).map(|g| turn(g, 100)).collect());
    let request2 = [pair(1, 1, 200), pair(2, 1, 200)];
    let first = compose_request(&base, &request2, 2, None).unwrap();
    let again = compose_request(&base, &request2, 2, None).unwrap();
    assert_eq!(first.input, again.input);
    assert_eq!(first.manifest_digest, again.manifest_digest);
    assert_eq!((first.reduced, first.recomposed), (0, false));
    let input = model::decode_input(&first.input).unwrap();
    assert_eq!(input.history, base.input.history);
    assert_eq!(input.prompt, base.input.prompt);
    assert_eq!(input.instructions, base.input.instructions);
    // One assistant entry with both calls, then their outputs in order.
    assert_eq!(input.continuation.len(), 3);
    let calls = &input.continuation[0].tool_calls;
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0].call_id.as_deref(), Some("call_1"));
    assert_eq!(calls[1].call_id.as_deref(), Some("call_2"));
    assert_eq!(input.continuation[1].call_id.as_deref(), Some("call_1"));
    assert_eq!(
        input.continuation[2].text.as_deref(),
        Some(request2[1].output.as_str())
    );
    // Request 3 adds a third call: request 2's bytes are its prefix.
    let request3 = [pair(1, 1, 200), pair(2, 1, 200), pair(3, 2, 200)];
    let second = compose_request(&base, &request3, 3, None).unwrap();
    assert!(second.input.starts_with(&first.input));
    assert_ne!(second.manifest_digest, first.manifest_digest);
    // The request ordinal is bound by the manifest.
    let renumbered = compose_request(&base, &request2, 4, None).unwrap();
    assert_eq!(renumbered.input, first.input);
    assert_ne!(renumbered.manifest_digest, first.manifest_digest);
}

/// CC-U30: under pressure older results become references, oldest first;
/// the latest request's results stay full; the base is recomposed only when
/// references are not enough; a latest request that cannot fit fails.
#[test]
fn cc_u30_pressure_reduces_older_results_then_the_base() {
    let history = || (1..=6).map(|g| turn(g, 400)).collect::<Vec<_>>();
    let pairs = [pair(1, 1, 3_000), pair(2, 2, 3_000), pair(3, 3, 3_000)];
    // Room for the base, one full result and two references.
    let roomy = base(100_000, history());
    let full = compose_request(&roomy, &pairs, 4, None).unwrap();
    let base_bytes = model::encode_input(&roomy.input).unwrap().len() as u32;
    let tight = base(base_bytes + 3_000 + 1_500, history());
    assert_eq!(tight.input, roomy.input, "the base itself still fits");
    let reduced = compose_request(&tight, &pairs, 4, None).unwrap();
    assert_eq!((reduced.reduced, reduced.recomposed), (2, false));
    assert!(reduced.input.len() < full.input.len());
    let input = model::decode_input(&reduced.input).unwrap();
    let texts: Vec<&str> = input
        .continuation
        .iter()
        .filter(|e| e.role == Some(3))
        .map(|e| e.text.as_deref().unwrap())
        .collect();
    assert!(texts[0].starts_with("[Asura: tool result 1 (exec_cmd) not shown for space;"));
    assert!(texts[0].ends_with(&format!("\njob: {}", "01".repeat(16))));
    assert!(texts[1].starts_with("[Asura: tool result 2"));
    assert_eq!(texts[2], pairs[2].output, "the latest result stays full");
    // Every call entry stays: call and result remain paired.
    assert_eq!(
        input
            .continuation
            .iter()
            .filter(|e| e.role == Some(2))
            .count(),
        3
    );
    assert_eq!(input.history, roomy.input.history);
    // Less room: references are not enough; history turns are reduced.
    let squeezed = base(base_bytes + 3_000 + 1_000 - 2_000, history());
    let recomposed = compose_request(&squeezed, &pairs, 4, None).unwrap();
    assert!(recomposed.recomposed);
    assert!(recomposed.input.len() as u32 <= squeezed.budget.byte_budget() as u32);
    let input = model::decode_input(&recomposed.input).unwrap();
    assert_ne!(input.history, squeezed.input.history);
    assert_eq!(
        input.continuation.last().unwrap().text.as_deref(),
        Some(pairs[2].output.as_str())
    );
    // The latest result alone exceeds the estimate: the request is still
    // sent with the base kept; the helper's exact count decides (step 5).
    let small = base(2_500, Vec::new());
    let over = compose_request(&small, &pairs, 4, None).unwrap();
    assert!(!over.recomposed);
    assert!(over.input.len() as u64 > small.budget.byte_budget());
    let input = model::decode_input(&over.input).unwrap();
    assert_eq!(
        input.continuation.last().unwrap().text.as_deref(),
        Some(pairs[2].output.as_str())
    );
    // A measured previous request with room keeps every result full and the
    // input byte-stable, although the estimate is exceeded.
    let previous = compose_request(&tight, &pairs[..2], 3, None).unwrap();
    let measured = Measured {
        tokens: 500,
        bytes: previous.input.len() as u64,
    };
    let exact = compose_request(&tight, &pairs, 4, Some(measured)).unwrap();
    assert_eq!((exact.reduced, exact.recomposed), (0, false));
    assert_eq!(exact.input, full.input);
    assert_ne!(
        exact.manifest_digest, full.manifest_digest,
        "T is bound by the manifest"
    );
    // Without room in the window the reductions apply as before.
    let crowded = Measured {
        tokens: base_bytes + 3_000,
        bytes: previous.input.len() as u64,
    };
    assert!(
        compose_request(&tight, &pairs, 4, Some(crowded))
            .unwrap()
            .reduced
            > 0
    );
}

/// The call ID is the service's, from the turn ordinal.
#[test]
fn call_ids_are_service_assigned() {
    assert_eq!(call_id(1), "call_1");
    assert_eq!(call_id(201), "call_201");
}
