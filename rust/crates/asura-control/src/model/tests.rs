use super::*;
use pb::envelope::Body;
fn envelope(body: Body) -> pb::Envelope {
    let hello = matches!(body, Body::Hello(_));
    pb::Envelope {
        operation_id: (!hello).then(|| vec![7; 16]),
        generation: (!hello).then_some(1),
        body: Some(body),
    }
}
fn hello(service: bool) -> pb::Envelope {
    envelope(Body::Hello(pb::Hello {
        local_tool_destination: None,
        reasoning_disabled: None,
        supported_capabilities: None,
        capability_source: None,
        model_capabilities: None,
        inventory_only: None,
        models: Vec::new(),
        issues: Vec::new(),
        selected_model: Some("system".into()),
        asset_root: None,
        endpoint: None,
        model_name: None,
        build_id: Some(vec![1; 32]),
        schema_digest: Some(vec![2; 32]),
        max_frame_bytes: Some(65536),
        availability: Some(if service { 3 } else { 1 }),
        context_tokens: (!service).then_some(4096),
        reported_context_tokens: (!service).then_some(4096),
        context_source: (!service).then_some(1),
        tool_schema_bytes: None,
        reason: Some(0),
    }))
}
fn fixtures() -> Vec<(pb::Envelope, Direction)> {
    use Direction::*;
    vec![
        (hello(true), ServiceToHelper),
        (hello(false), HelperToService),
        (
            envelope(Body::InferenceUsage(pb::InferenceUsage {
                request_ordinal: Some(1),
                input_tokens: Some(0),
                output_tokens: Some(2),
                complete: Some(true),
                ..Default::default()
            })),
            HelperToService,
        ),
        (
            envelope(Body::Begin(pb::Begin {
                model: Some("system".into()),
                input_bytes: Some(10),
                max_response_tokens: Some(512),
                enable_project_tools: None,
            })),
            ServiceToHelper,
        ),
        (
            envelope(Body::Chunk(pb::Chunk {
                transfer_id: Some(1),
                direction: Some(1),
                ordinal: Some(0),
                data: Some(vec![1]),
                revision: Some(0),
            })),
            ServiceToHelper,
        ),
        (
            envelope(Body::Chunk(pb::Chunk {
                transfer_id: Some(2),
                direction: Some(2),
                ordinal: Some(0),
                data: Some("世界".as_bytes().into()),
                revision: Some(1),
            })),
            HelperToService,
        ),
        (
            envelope(Body::Credit(pb::Credit {
                transfer_id: Some(1),
                direction: Some(1),
                accepted_bytes: Some(0),
                granted_bytes: Some(65536),
            })),
            HelperToService,
        ),
        (
            envelope(Body::Credit(pb::Credit {
                transfer_id: Some(2),
                direction: Some(2),
                accepted_bytes: Some(0),
                granted_bytes: Some(65536),
            })),
            ServiceToHelper,
        ),
        (
            envelope(Body::InputEnd(pb::InputEnd {
                count: Some(1),
                total_bytes: Some(10),
            })),
            ServiceToHelper,
        ),
        (envelope(Body::Ready(pb::Ready {})), HelperToService),
        (envelope(Body::Start(pb::Start {})), ServiceToHelper),
        (
            envelope(Body::Cancel(pb::Cancel { reason: Some(1) })),
            ServiceToHelper,
        ),
        (
            envelope(Body::SnapshotEnd(pb::SnapshotEnd {
                revision: Some(1),
                count: Some(1),
                total_bytes: Some(10),
            })),
            HelperToService,
        ),
        (
            envelope(Body::ToolCall(raw_call(
                "project",
                r#"{"command":"read_file","path":"README.md"}"#,
            ))),
            HelperToService,
        ),
        (
            envelope(Body::ToolResult(pb::ToolResult {
                ordinal: Some(1),
                status: Some(1),
                text: Some("content".into()),
                next_offset: Some(7),
                truncated: Some(false),
            })),
            ServiceToHelper,
        ),
        (
            envelope(Body::Terminal(pb::Terminal {
                outcome: Some(1),
                last_revision: Some(1),
                count: Some(1),
                total_bytes: Some(10),
                usage_tokens: None,
                usage_known: Some(false),
                reason: Some(0),
            })),
            HelperToService,
        ),
    ]
}
#[test]
fn all_variants_round_trip_fragmented_and_coalesced() {
    for (message, direction) in fixtures() {
        let encoded = encode_frame(&message, direction).unwrap();
        assert_eq!(
            u32::from_be_bytes(encoded[..4].try_into().unwrap()) as usize,
            encoded.len() - 4
        );
        let mut codec = ModelCodec::new();
        for byte in &encoded[..encoded.len() - 1] {
            assert_eq!(codec.push(&[*byte]).unwrap(), 1);
            assert_eq!(codec.next_frame(direction).unwrap(), None);
        }
        codec.push(&encoded[encoded.len() - 1..]).unwrap();
        assert_eq!(codec.next_frame(direction).unwrap(), Some(message.clone()));
        assert_eq!(codec.buffered_len(), 0);
        let joined = [encoded.as_slice(), encoded.as_slice()].concat();
        let used = codec.push(&joined).unwrap();
        assert_eq!(used, encoded.len());
        assert_eq!(codec.next_frame(direction).unwrap(), Some(message.clone()));
        assert_eq!(codec.push(&joined[used..]).unwrap(), encoded.len());
        assert_eq!(codec.next_frame(direction).unwrap(), Some(message));
    }
}
#[test]
fn canonical_wire_rejects_duplicate_unknown_bool_and_overlong_varints() {
    let (message, direction) = fixtures().pop().unwrap();
    let mut duplicate = message.encode_to_vec();
    duplicate.extend_from_slice(&[0x10, 1]);
    assert!(decode_body(&duplicate, direction).is_err());
    let mut unknown = message.encode_to_vec();
    unknown.extend_from_slice(&[0xa0, 1, 0]);
    assert!(decode_body(&unknown, direction).is_err());
    let mut invalid_bool = message.encode_to_vec();
    let at = invalid_bool
        .windows(2)
        .position(|w| w == [0x30, 0])
        .unwrap();
    invalid_bool[at + 1] = 2;
    assert!(decode_body(&invalid_bool, direction).is_err());
    let mut noncanonical = message.encode_to_vec();
    let at = noncanonical
        .windows(2)
        .position(|w| w == [0x10, 1])
        .unwrap();
    noncanonical.splice(at + 1..at + 2, [0x81, 0]);
    assert!(decode_body(&noncanonical, direction).is_err());
    let mut reordered = message.encode_to_vec();
    let generation = reordered.drain(18..20).collect::<Vec<_>>();
    reordered.extend(generation);
    assert!(decode_body(&reordered, direction).is_err());
    for length in [0u32, 65537, u32::MAX] {
        assert!(ModelCodec::new().push(&length.to_be_bytes()).is_err());
    }
}
#[test]
fn semantic_presence_direction_identity_and_usage() {
    for (mut message, direction) in fixtures() {
        if matches!(message.body, Some(Body::Hello(_))) {
            message.operation_id = Some(vec![1; 16]);
        } else {
            message.generation = Some(0);
        }
        assert!(validate_semantics(&message, direction).is_err());
    }
    let message = envelope(Body::Start(pb::Start {}));
    assert_eq!(
        validate_semantics(&message, Direction::HelperToService),
        Err(ProtocolError::WrongDirection)
    );
    assert!(validate_identity(&message, &[7; 16], 1).is_ok());
    assert!(validate_identity(&message, &[7; 16], 2).is_err());
    let (mut message, direction) = fixtures().pop().unwrap();
    let Some(Body::Terminal(terminal)) = message.body.as_mut() else {
        panic!()
    };
    terminal.usage_tokens = Some(0);
    assert!(validate_semantics(&message, direction).is_err());
    let Some(Body::Terminal(terminal)) = message.body.as_mut() else {
        panic!()
    };
    terminal.usage_known = Some(true);
    assert!(validate_semantics(&message, direction).is_ok());
    let Some(Body::Terminal(terminal)) = message.body.as_mut() else {
        panic!()
    };
    terminal.usage_tokens = Some(2049);
    assert!(validate_semantics(&message, direction).is_err());
}
#[test]
fn input_unicode_repeated_history_presence_and_byte_bounds() {
    let mut input = pb::ModelInput {
        instructions: Some(String::new()),
        history: vec![
            pb::HistoryTurn {
                role: Some(1),
                text: Some("世界".into())
            };
            MAX_HISTORY_ENTRIES
        ],
        prompt: Some("question".into()),
    };
    let bytes = encode_input(&input).unwrap();
    assert_eq!(decode_input(&bytes).unwrap(), input);
    input.history.push(input.history[0].clone());
    assert!(encode_input(&input).is_err());
    assert!(decode_input(&input.encode_to_vec()).is_err());
    // The former 32 KiB prompt and 64 KiB input bounds no longer apply.
    input.history.clear();
    input.prompt = Some("x".repeat(32769));
    input.instructions = Some("x".repeat(65536));
    let bytes = encode_input(&input).unwrap();
    assert!(bytes.len() > 65536);
    assert_eq!(decode_input(&bytes).unwrap(), input);
    input.prompt = Some(String::new());
    assert!(encode_input(&input).is_err());
    input.prompt = Some("x".into());
    input.instructions = None;
    assert!(encode_input(&input).is_err());
    input.instructions = Some(String::new());
    let mut bytes = input.encode_to_vec();
    bytes.extend_from_slice(&[0x0a, 0]);
    assert!(decode_input(&bytes).is_err());
}
/// CC-U18 at the stateless layer: `Begin` may announce up to 1 GiB; credit stays a
/// sliding 64 KiB window over input up to that size.
#[test]
fn input_transfer_bounds_follow_chunk_count_not_a_content_limit() {
    let begin = |bytes: u64| {
        envelope(Body::Begin(pb::Begin {
            model: Some("system".into()),
            input_bytes: Some(bytes),
            max_response_tokens: Some(2048),
            ..Default::default()
        }))
    };
    for bytes in [1, 65_537, MAX_INPUT_BYTES] {
        assert!(
            encode_frame(&begin(bytes), Direction::ServiceToHelper).is_ok(),
            "{bytes}"
        );
    }
    for bytes in [0, MAX_INPUT_BYTES + 1] {
        assert!(
            encode_frame(&begin(bytes), Direction::ServiceToHelper).is_err(),
            "{bytes}"
        );
    }
    let credit = |accepted: u64, granted: u64| {
        envelope(Body::Credit(pb::Credit {
            transfer_id: Some(1),
            direction: Some(1),
            accepted_bytes: Some(accepted),
            granted_bytes: Some(granted),
        }))
    };
    assert!(
        encode_frame(
            &credit(1 << 20, (1 << 20) + 65_536),
            Direction::HelperToService
        )
        .is_ok()
    );
    assert!(
        encode_frame(
            &credit(1 << 20, (1 << 20) + 65_537),
            Direction::HelperToService
        )
        .is_err()
    );
    let end = |count: u64, total: u64| {
        envelope(Body::InputEnd(pb::InputEnd {
            count: Some(count),
            total_bytes: Some(total),
        }))
    };
    assert!(encode_frame(&end(65, 1 << 20), Direction::ServiceToHelper).is_ok());
    assert!(
        encode_frame(
            &end(MAX_INPUT_CHUNKS + 1, MAX_INPUT_BYTES),
            Direction::ServiceToHelper
        )
        .is_err()
    );
}
/// The helper's tool schema size is a helper-only, available-model field.
#[test]
fn tool_schema_bytes_only_from_an_available_helper() {
    let with = |service: bool, bytes: u32| {
        let mut message = hello(service);
        let Some(Body::Hello(value)) = message.body.as_mut() else {
            panic!()
        };
        value.tool_schema_bytes = Some(bytes);
        message
    };
    assert!(encode_frame(&with(false, 4096), Direction::HelperToService).is_ok());
    assert!(
        encode_frame(
            &with(false, MAX_TOOL_SCHEMA_BYTES + 1),
            Direction::HelperToService
        )
        .is_err()
    );
    assert!(encode_frame(&with(true, 4096), Direction::ServiceToHelper).is_err());
}
#[test]
fn nested_unknown_fields_and_limits_reject() {
    let message = envelope(Body::Ready(pb::Ready {}));
    let mut bytes = message.encode_to_vec();
    assert_eq!(&bytes[bytes.len() - 2..], &[0x7a, 0]);
    bytes.pop();
    bytes.extend_from_slice(&[2, 8, 0]);
    assert!(decode_body(&bytes, Direction::HelperToService).is_err());
    let message = envelope(Body::Chunk(pb::Chunk {
        transfer_id: Some(1),
        direction: Some(1),
        ordinal: Some(0),
        data: Some(vec![0; 16385]),
        revision: Some(0),
    }));
    assert!(encode_frame(&message, Direction::ServiceToHelper).is_err());
    let message = envelope(Body::Credit(pb::Credit {
        transfer_id: Some(1),
        direction: Some(1),
        accepted_bytes: Some(1),
        granted_bytes: Some(65538),
    }));
    assert!(encode_frame(&message, Direction::HelperToService).is_err());
}

/// Explicit export for the real Rust/Swift fixture exchange. Normal tests perform no I/O.
#[test]
#[ignore = "requires ASURA_MODEL_FIXTURE_DIR and the separate Swift fixture consumer"]
fn export_cross_language_fixtures() {
    let directory = std::path::PathBuf::from(
        std::env::var_os("ASURA_MODEL_FIXTURE_DIR").expect("set ASURA_MODEL_FIXTURE_DIR"),
    );
    std::fs::create_dir_all(&directory).unwrap();
    let mut manifest = String::new();
    let mut write = |name: &str, kind: &str, accepted: bool, bytes: Vec<u8>| {
        let result = match kind {
            "service" => decode_body(&bytes, Direction::ServiceToHelper).map(|_| ()),
            "helper" => decode_body(&bytes, Direction::HelperToService).map(|_| ()),
            "input" => decode_input(&bytes).map(|_| ()),
            _ => panic!("unknown fixture kind"),
        };
        assert_eq!(result.is_ok(), accepted, "Rust fixture {name}: {result:?}");
        std::fs::write(directory.join(name), bytes).unwrap();
        manifest.push_str(&format!(
            "{name}\t{kind}\t{}\n",
            if accepted { "accept" } else { "reject" }
        ));
    };
    for (name, value, accepted) in [
        (
            "usage_unknown",
            pb::InferenceUsage {
                request_ordinal: Some(1),
                complete: Some(false),
                ..Default::default()
            },
            true,
        ),
        (
            "usage_zero",
            pb::InferenceUsage {
                request_ordinal: Some(1),
                input_tokens: Some(0),
                output_tokens: Some(0),
                complete: Some(true),
                ..Default::default()
            },
            true,
        ),
        (
            "usage_bad_ordinal",
            pb::InferenceUsage {
                request_ordinal: Some(65),
                complete: Some(false),
                ..Default::default()
            },
            false,
        ),
        (
            "usage_bad_count",
            pb::InferenceUsage {
                request_ordinal: Some(1),
                input_tokens: Some(1_000_000_001),
                complete: Some(false),
                ..Default::default()
            },
            false,
        ),
        (
            "usage_bad_cached",
            pb::InferenceUsage {
                request_ordinal: Some(1),
                input_tokens: Some(0),
                cached_input_tokens: Some(1),
                complete: Some(false),
                ..Default::default()
            },
            false,
        ),
        (
            "usage_missing_complete",
            pb::InferenceUsage {
                request_ordinal: Some(1),
                ..Default::default()
            },
            false,
        ),
    ] {
        let message = envelope(Body::InferenceUsage(value));
        write(
            &format!("{name}.pb"),
            "helper",
            accepted,
            message.encode_to_vec(),
        );
        write(
            &format!("{name}_wrong_direction.pb"),
            "service",
            false,
            message.encode_to_vec(),
        );
    }
    let mut declared = hello(true);
    if let Some(Body::Hello(value)) = &mut declared.body {
        value.selected_model = Some("mlx:fixture".into());
        value.model_capabilities = Some(5);
    }
    write("mlx_declared.pb", "service", true, declared.encode_to_vec());
    write(
        "mlx_declared_wrong_direction.pb",
        "helper",
        false,
        declared.encode_to_vec(),
    );
    let mut local = hello(false);
    if let Some(Body::Hello(value)) = &mut local.body {
        value.local_tool_destination = Some(true);
        value.supported_capabilities = Some(5);
        value.capability_source = Some(2);
    }
    write(
        "local_tools_profile.pb",
        "helper",
        true,
        local.encode_to_vec(),
    );
    let mut reasoning = local.clone();
    if let Some(Body::Hello(value)) = &mut reasoning.body {
        value.selected_model = Some("mlx:fixture".into());
        value.reasoning_disabled = Some(true);
        value.context_source = Some(3);
    }
    write(
        "mlx_reasoning_disabled.pb",
        "helper",
        true,
        reasoning.encode_to_vec(),
    );
    write(
        "mlx_reasoning_wrong_direction.pb",
        "service",
        false,
        reasoning.encode_to_vec(),
    );
    if let Some(Body::Hello(value)) = &mut reasoning.body {
        value.supported_capabilities = Some(1);
    }
    write(
        "mlx_reasoning_unsupported.pb",
        "helper",
        false,
        reasoning.encode_to_vec(),
    );
    write(
        "local_tools_wrong_direction.pb",
        "service",
        false,
        local.encode_to_vec(),
    );
    if let Some(Body::Hello(value)) = &mut local.body {
        value.supported_capabilities = Some(16);
    }
    write(
        "invalid_capability_mask.pb",
        "helper",
        false,
        local.encode_to_vec(),
    );
    if let Some(Body::Hello(value)) = &mut local.body {
        value.supported_capabilities = None;
        value.capability_source = Some(4);
    }
    // Source 4 (support unknown) carries no mask and so grants no tools.
    write(
        "unknown_support_without_mask.pb",
        "helper",
        true,
        local.encode_to_vec(),
    );
    // Unparsed tool calls (tool-call-records.md): any argument text within
    // the bound is accepted; sizes replace text only above the bound.
    for (name, call, accepted) in [
        (
            "tool_call_exec_cmd.pb",
            raw_call("exec_cmd", r#"{"command":"printf fixture"}"#),
            true,
        ),
        ("tool_call_invalid_json.pb", raw_call("project", "{"), true),
        (
            "tool_call_unregistered.pb",
            pb::ToolCall {
                source: Some(2),
                ..raw_call("delete_everything", "{}")
            },
            true,
        ),
        (
            "tool_call_sizes_only.pb",
            pb::ToolCall {
                name: None,
                name_bytes: Some(300),
                arguments: None,
                arguments_bytes: Some(70_000),
                ..raw_call("x", "")
            },
            true,
        ),
        (
            "tool_call_ordinal_nine.pb",
            pb::ToolCall {
                ordinal: Some(9),
                ..raw_call("service", r#"{"command":"status"}"#)
            },
            true,
        ),
        (
            "tool_call_ordinal_ten.pb",
            pb::ToolCall {
                ordinal: Some(10),
                ..raw_call("service", "{}")
            },
            false,
        ),
        (
            "tool_call_both_text_and_size.pb",
            pb::ToolCall {
                arguments_bytes: Some(70_000),
                ..raw_call("service", "{}")
            },
            false,
        ),
    ] {
        let message = envelope(Body::ToolCall(call));
        write(name, "helper", accepted, message.encode_to_vec());
        if accepted {
            write(
                &name.replace(".pb", "_wrong_direction.pb"),
                "service",
                false,
                message.encode_to_vec(),
            );
        }
    }
    write(
        "exec_cmd_failed_output.pb",
        "service",
        true,
        envelope(Body::ToolResult(pb::ToolResult {
            ordinal: Some(1),
            status: Some(5),
            text: Some("stdout: partial\nstderr: deadline".into()),
            next_offset: None,
            truncated: Some(true),
        }))
        .encode_to_vec(),
    );
    for (message, direction) in fixtures() {
        let name = match message.body.as_ref().unwrap() {
            Body::Hello(_) => "hello",
            Body::Begin(_) => "begin",
            Body::Chunk(_) => "chunk",
            Body::Credit(_) => "credit",
            Body::InputEnd(_) => "input_end",
            Body::Ready(_) => "ready",
            Body::Start(_) => "start",
            Body::Cancel(_) => "cancel",
            Body::SnapshotEnd(_) => "snapshot_end",
            Body::Terminal(_) => "terminal",
            Body::ToolCall(_) => "tool_call",
            Body::ContextMeasured(_) => "context_measured",
            Body::ToolResult(_) => "tool_result",
            Body::InferenceUsage(_) => "inference_usage",
        };
        let kind = if direction == Direction::ServiceToHelper {
            "service"
        } else {
            "helper"
        };
        write(
            &format!("{kind}_{name}.pb"),
            kind,
            true,
            message.encode_to_vec(),
        );
    }
    for usage in [0, 512, 2048] {
        let message = envelope(Body::Terminal(pb::Terminal {
            outcome: Some(1),
            last_revision: Some(1024),
            count: Some(256),
            total_bytes: Some(MAX_OUTPUT_BYTES),
            usage_tokens: Some(usage),
            usage_known: Some(true),
            reason: Some(0),
        }));
        write(
            &format!("helper_terminal_usage_{usage}.pb"),
            "helper",
            true,
            message.encode_to_vec(),
        );
    }
    let mut message = envelope(Body::Start(pb::Start {}));
    message.generation = Some(u64::MAX);
    write(
        "service_start_generation_max.pb",
        "service",
        true,
        message.encode_to_vec(),
    );
    let message = envelope(Body::Chunk(pb::Chunk {
        transfer_id: Some(1),
        direction: Some(1),
        ordinal: Some(0),
        data: Some(vec![0x80; MAX_CHUNK_BYTES]),
        revision: Some(0),
    }));
    write(
        "service_chunk_max.pb",
        "service",
        true,
        message.encode_to_vec(),
    );
    let message = envelope(Body::Terminal(pb::Terminal {
        outcome: Some(2),
        last_revision: Some(0),
        count: Some(0),
        total_bytes: Some(0),
        usage_tokens: None,
        usage_known: Some(false),
        reason: Some(12),
    }));
    write(
        "helper_terminal_failed_empty.pb",
        "helper",
        true,
        message.encode_to_vec(),
    );
    for availability in [2, 3] {
        let mut message = hello(false);
        let Some(Body::Hello(value)) = message.body.as_mut() else {
            panic!()
        };
        value.availability = Some(availability);
        value.context_tokens = None;
        value.reported_context_tokens = None;
        value.context_source = None;
        value.reason = Some(1);
        write(
            &format!("helper_hello_availability_{availability}.pb"),
            "helper",
            true,
            message.encode_to_vec(),
        );
    }
    let input = pb::ModelInput {
        instructions: Some(String::new()),
        history: (0..32)
            .map(|n| pb::HistoryTurn {
                role: Some(if n % 2 == 0 { 1 } else { 2 }),
                text: Some(format!("turn {n}: 世界 e\u{301} 🙂\n")),
            })
            .collect(),
        prompt: Some("Hello 世界".into()),
    };
    write(
        "input_unicode_history_32.pb",
        "input",
        true,
        input.encode_to_vec(),
    );
    let input = pb::ModelInput {
        instructions: Some(String::new()),
        history: vec![pb::HistoryTurn {
            role: Some(2),
            text: Some(String::new()),
        }],
        prompt: Some("x".repeat(32768)),
    };
    write(
        "input_prompt_max_empty_optional.pb",
        "input",
        true,
        input.encode_to_vec(),
    );
    // The context block shares the prompt field, so only the total input bounds it.
    let mut larger = input.clone();
    larger.prompt.as_mut().unwrap().push('x');
    write(
        "input_prompt_above_person_limit.pb",
        "input",
        true,
        larger.encode_to_vec(),
    );
    let mut larger = input.clone();
    larger.instructions = Some("x".repeat(32768));
    write(
        "input_above_former_64k.pb",
        "input",
        true,
        larger.encode_to_vec(),
    );
    let mut history = input;
    history.prompt = Some("x".into());
    history.history = vec![
        pb::HistoryTurn {
            role: Some(1),
            text: Some(String::new())
        };
        MAX_HISTORY_ENTRIES
    ];
    write(
        "input_history_2048.pb",
        "input",
        true,
        history.encode_to_vec(),
    );
    history.history.push(history.history[0].clone());
    write(
        "invalid_input_history_2049.pb",
        "input",
        false,
        history.encode_to_vec(),
    );
    let mut nested = pb::ModelInput {
        instructions: Some(String::new()),
        history: vec![pb::HistoryTurn {
            role: Some(99),
            text: Some("text".into()),
        }],
        prompt: Some("x".into()),
    };
    write(
        "invalid_input_role.pb",
        "input",
        false,
        nested.encode_to_vec(),
    );
    nested.history[0].role = Some(1);
    nested.instructions = None;
    write(
        "invalid_input_presence.pb",
        "input",
        false,
        nested.encode_to_vec(),
    );
    let (terminal, _) = fixtures().pop().unwrap();
    let mut bytes = terminal.encode_to_vec();
    let at = bytes.windows(2).position(|pair| pair == [0x30, 0]).unwrap();
    bytes[at + 1] = 2;
    write("invalid_terminal_bool.pb", "helper", false, bytes);
    let mut bytes = terminal.encode_to_vec();
    bytes.extend_from_slice(&[0x10, 1]);
    write("invalid_duplicate_generation.pb", "helper", false, bytes);
    let mut bytes = terminal.encode_to_vec();
    bytes.extend_from_slice(&[0xa0, 1, 0]);
    write("invalid_unknown_field.pb", "helper", false, bytes);
    let mut bytes = terminal.encode_to_vec();
    let at = bytes.windows(2).position(|pair| pair == [0x10, 1]).unwrap();
    bytes.splice(at + 1..at + 2, [0x81, 0]);
    write("invalid_overlong_varint.pb", "helper", false, bytes);
    let mut bytes = terminal.encode_to_vec();
    let generation = bytes.drain(18..20).collect::<Vec<_>>();
    bytes.extend(generation);
    write("invalid_field_order.pb", "helper", false, bytes);
    let mut bytes = envelope(Body::Ready(pb::Ready {})).encode_to_vec();
    bytes.pop();
    bytes.extend_from_slice(&[2, 8, 0]);
    write("invalid_nested_unknown.pb", "helper", false, bytes);
    let mut terminal_with_usage = terminal;
    let Some(Body::Terminal(value)) = terminal_with_usage.body.as_mut() else {
        panic!()
    };
    value.usage_tokens = Some(0);
    write(
        "invalid_unknown_usage_with_tokens.pb",
        "helper",
        false,
        terminal_with_usage.encode_to_vec(),
    );
    write(
        "invalid_start_direction.pb",
        "helper",
        false,
        envelope(Body::Start(pb::Start {})).encode_to_vec(),
    );
    let overflow = envelope(Body::Terminal(pb::Terminal {
        outcome: Some(1),
        last_revision: Some(1),
        count: Some(1),
        total_bytes: Some(1),
        usage_tokens: Some(2049),
        usage_known: Some(true),
        reason: Some(0),
    }));
    write(
        "invalid_usage_output_overflow.pb",
        "helper",
        false,
        overflow.encode_to_vec(),
    );
    std::fs::write(directory.join("manifest.tsv"), manifest).unwrap();
}

fn raw_call(name: &str, arguments: &str) -> pb::ToolCall {
    pb::ToolCall {
        ordinal: Some(1),
        name: Some(name.into()),
        arguments: Some(arguments.into()),
        arguments_bytes: None,
        name_bytes: None,
        source: Some(1),
    }
}
fn helper_accepts(call: pb::ToolCall) -> bool {
    let message = envelope(Body::ToolCall(call));
    let bytes = message.encode_to_vec();
    let accepted = decode_body(&bytes, Direction::HelperToService).is_ok();
    assert_eq!(
        encode_frame(&message, Direction::HelperToService).is_ok(),
        accepted
    );
    // Never service to helper.
    assert!(decode_body(&bytes, Direction::ServiceToHelper).is_err());
    accepted
}
/// TCR-U14: the unparsed ToolCall is bounded: name 1-256 bytes or a size over
/// 256, arguments 0-64,000 bytes or a size over 64,000, exactly one of each,
/// ordinal 1-9 and a known source. Malformed argument text is not a codec
/// fault: the service records it.
#[test]
fn tcr_u14_tool_call_bounds_and_sizes() {
    let base = raw_call("project", "{");
    for call in [
        base.clone(),
        raw_call("exec_cmd", ""),
        raw_call("not_registered", "[1,2"),
        raw_call(
            &"n".repeat(MAX_TOOL_NAME_BYTES),
            &"a".repeat(MAX_TOOL_ARGUMENT_BYTES),
        ),
        pb::ToolCall {
            ordinal: Some(9),
            source: Some(2),
            ..base.clone()
        },
        pb::ToolCall {
            name: None,
            name_bytes: Some(257),
            ..base.clone()
        },
        pb::ToolCall {
            arguments: None,
            arguments_bytes: Some(64_001),
            ..base.clone()
        },
        pb::ToolCall {
            arguments: None,
            arguments_bytes: Some(u32::MAX),
            name: None,
            name_bytes: Some(u32::MAX),
            ..base.clone()
        },
    ] {
        assert!(helper_accepts(call.clone()), "{call:?}");
    }
    for call in [
        pb::ToolCall {
            ordinal: None,
            ..base.clone()
        },
        pb::ToolCall {
            ordinal: Some(0),
            ..base.clone()
        },
        pb::ToolCall {
            ordinal: Some(10),
            ..base.clone()
        },
        raw_call("", "{}"),
        raw_call(&"n".repeat(MAX_TOOL_NAME_BYTES + 1), "{}"),
        raw_call("project", &"a".repeat(MAX_TOOL_ARGUMENT_BYTES + 1)),
        // Field 3 and field 4 both absent, or both present.
        pb::ToolCall {
            arguments: None,
            ..base.clone()
        },
        pb::ToolCall {
            arguments_bytes: Some(64_001),
            ..base.clone()
        },
        // A size is sent only above the limit.
        pb::ToolCall {
            arguments: None,
            arguments_bytes: Some(64_000),
            ..base.clone()
        },
        pb::ToolCall {
            name: None,
            ..base.clone()
        },
        pb::ToolCall {
            name_bytes: Some(300),
            ..base.clone()
        },
        pb::ToolCall {
            name: None,
            name_bytes: Some(256),
            ..base.clone()
        },
        pb::ToolCall {
            source: None,
            ..base.clone()
        },
        pb::ToolCall {
            source: Some(3),
            ..base.clone()
        },
    ] {
        assert!(!helper_accepts(call.clone()), "{call:?}");
    }
    // A removed typed argument field (10-19) is an unknown, reserved field.
    let mut bytes = pb::Envelope {
        operation_id: Some(vec![7; 16]),
        generation: Some(1),
        body: None,
    }
    .encode_to_vec();
    // ToolCall: ordinal 1, name "x", arguments "", source 1, then field 10 (len 0).
    bytes.extend_from_slice(&[0xa2, 1, 11, 8, 1, 0x12, 1, b'x', 0x1a, 0, 0x30, 1, 0x52, 0]);
    assert!(decode_body(&bytes, Direction::HelperToService).is_err());
    bytes.truncate(bytes.len() - 2);
    let len = bytes.len();
    bytes[len - 10] = 9;
    assert!(decode_body(&bytes, Direction::HelperToService).is_ok());
}

#[test]
fn tool_results_enforce_direction_bounds_and_ordinal_nine_limit() {
    let result = envelope(Body::ToolResult(pb::ToolResult {
        ordinal: Some(1),
        status: Some(1),
        text: Some("source".into()),
        next_offset: Some(6),
        truncated: Some(false),
    }));
    assert!(encode_frame(&result, Direction::ServiceToHelper).is_ok());
    assert_eq!(
        encode_frame(&result, Direction::HelperToService),
        Err(ProtocolError::WrongDirection)
    );
    let mut bad = result.clone();
    if let Some(Body::ToolResult(v)) = &mut bad.body {
        v.text = Some("x".repeat(16_385));
    }
    assert_eq!(
        encode_frame(&bad, Direction::ServiceToHelper),
        Err(ProtocolError::InvalidSemantics)
    );
    if let Some(Body::ToolResult(v)) = &mut bad.body {
        v.text = Some(String::new());
        v.status = Some(2);
    }
    assert_eq!(
        encode_frame(&bad, Direction::ServiceToHelper),
        Err(ProtocolError::InvalidSemantics)
    );
    // Ordinal 9 only with status 7 (TCR-D6); ordinal 10 never.
    let nine = |status| {
        envelope(Body::ToolResult(pb::ToolResult {
            ordinal: Some(9),
            status: Some(status),
            text: Some("limit".into()),
            next_offset: None,
            truncated: Some(false),
        }))
    };
    assert!(encode_frame(&nine(7), Direction::ServiceToHelper).is_ok());
    for status in [1, 3, 6] {
        assert_eq!(
            encode_frame(&nine(status), Direction::ServiceToHelper),
            Err(ProtocolError::InvalidSemantics)
        );
    }
    if let Some(Body::ToolResult(v)) = &mut bad.body {
        v.ordinal = Some(10);
        v.status = Some(7);
    }
    assert!(encode_frame(&bad, Direction::ServiceToHelper).is_err());
}

#[test]
fn context_measurement_is_bounded_and_helper_only() {
    let message = envelope(Body::ContextMeasured(pb::ContextMeasured {
        input_tokens: Some(0),
        capacity_tokens: Some(4096),
    }));
    assert!(encode_frame(&message, Direction::HelperToService).is_ok());
    assert!(encode_frame(&message, Direction::ServiceToHelper).is_err());
    for (n, c) in [
        (None, Some(4096)),
        (Some(1), Some(0)),
        (Some(4097), Some(4096)),
    ] {
        let bad = envelope(Body::ContextMeasured(pb::ContextMeasured {
            input_tokens: n,
            capacity_tokens: c,
        }));
        assert!(encode_frame(&bad, Direction::HelperToService).is_err());
    }
    let mut named = hello(false);
    if let Some(Body::Hello(v)) = &mut named.body {
        v.model_name = Some("Native 模型".into());
    }
    assert!(encode_frame(&named, Direction::HelperToService).is_ok());
    if let Some(Body::Hello(v)) = &mut named.body {
        v.model_name = Some("bad\nname".into());
    }
    assert!(encode_frame(&named, Direction::HelperToService).is_err());
}

#[test]
fn discovered_context_requires_source_and_consistent_window() {
    let mut message = hello(false);
    assert!(encode_frame(&message, Direction::HelperToService).is_ok());
    let Some(Body::Hello(value)) = message.body.as_mut() else {
        panic!()
    };
    value.reported_context_tokens = None;
    assert!(encode_frame(&message, Direction::HelperToService).is_err());
    let Some(Body::Hello(value)) = message.body.as_mut() else {
        panic!()
    };
    value.reported_context_tokens = Some(2048);
    assert!(encode_frame(&message, Direction::HelperToService).is_err());
    let Some(Body::Hello(value)) = message.body.as_mut() else {
        panic!()
    };
    value.reported_context_tokens = Some(4096);
    value.context_source = Some(9);
    assert!(encode_frame(&message, Direction::HelperToService).is_err());
}

#[test]
fn provider_selection_is_bounded_and_asset_paths_never_come_from_helper() {
    let mut request = hello(true);
    if let Some(Body::Hello(v)) = &mut request.body {
        v.selected_model = Some("coreai:fixture".into());
        v.asset_root = Some("/private/tmp/models".into());
    }
    assert!(encode_frame(&request, Direction::ServiceToHelper).is_ok());
    if let Some(Body::Hello(v)) = &mut request.body {
        v.selected_model = Some("bad model".into());
    }
    assert!(encode_frame(&request, Direction::ServiceToHelper).is_err());
    let mut response = hello(false);
    if let Some(Body::Hello(v)) = &mut response.body {
        v.selected_model = Some("coreai:fixture".into());
        v.context_source = Some(2);
    }
    assert!(encode_frame(&response, Direction::HelperToService).is_ok());
    if let Some(Body::Hello(v)) = &mut response.body {
        v.selected_model = Some("COREAI:fixture".into());
    }
    assert!(encode_frame(&response, Direction::HelperToService).is_ok());
    if let Some(Body::Hello(v)) = &mut response.body {
        v.endpoint = Some("http://untrusted".into());
    }
    assert!(encode_frame(&response, Direction::HelperToService).is_err());
}

#[test]
fn inventory_hello_is_metadata_only_and_has_its_own_row_bounds() {
    let mut value = hello(true);
    let Some(Body::Hello(request)) = value.body.as_mut() else {
        panic!()
    };
    request.selected_model = Some("system".into());
    request.inventory_only = Some(true);
    assert!(validate_semantics(&value, Direction::ServiceToHelper).is_ok());
    let mut response = hello(false);
    let Some(Body::Hello(reply)) = response.body.as_mut() else {
        panic!()
    };
    reply.selected_model = Some("system".into());
    reply.inventory_only = Some(true);
    reply.availability = Some(3);
    reply.context_tokens = None;
    reply.reported_context_tokens = None;
    reply.context_source = None;
    reply.models.push(pb::ModelInventoryEntry {
        selector: Some("system".into()),
        provider: Some("system".into()),
        status: Some(1),
        detail: None,
    });
    for (provider, status) in [("mlx", 2), ("coreai", 2), ("ollama", 3)] {
        for i in 0..21 {
            reply.models.push(pb::ModelInventoryEntry {
                selector: Some(format!("{provider}:m{i}")),
                provider: Some(provider.into()),
                status: Some(status),
                detail: None,
            });
        }
    }
    assert!(encode_frame(&response, Direction::HelperToService).is_ok());
    let Some(Body::Hello(reply)) = response.body.as_mut() else {
        panic!()
    };
    reply.models.push(pb::ModelInventoryEntry {
        selector: Some("mlx:overflow".into()),
        provider: Some("mlx".into()),
        status: Some(2),
        detail: None,
    });
    assert!(encode_frame(&response, Direction::HelperToService).is_err());
    let Some(Body::Hello(reply)) = response.body.as_mut() else {
        panic!()
    };
    reply.models.truncate(1);
    reply.inventory_only = None;
    assert!(validate_semantics(&response, Direction::HelperToService).is_err());
}

#[test]
fn locality_and_declared_capabilities_have_strict_direction_and_scope() {
    let mut service = hello(true);
    if let Some(Body::Hello(value)) = &mut service.body {
        value.selected_model = Some("mlx:fixture".into());
        value.model_capabilities = Some(15);
    }
    assert!(encode_frame(&service, Direction::ServiceToHelper).is_ok());
    if let Some(Body::Hello(value)) = &mut service.body {
        value.model_capabilities = Some(16);
    }
    assert!(encode_frame(&service, Direction::ServiceToHelper).is_err());
    if let Some(Body::Hello(value)) = &mut service.body {
        value.model_capabilities = None;
        value.local_tool_destination = Some(true);
    }
    assert!(encode_frame(&service, Direction::ServiceToHelper).is_err());
    let mut helper = hello(false);
    if let Some(Body::Hello(value)) = &mut helper.body {
        value.local_tool_destination = Some(true);
    }
    assert!(encode_frame(&helper, Direction::HelperToService).is_ok());
    if let Some(Body::Hello(value)) = &mut helper.body {
        value.model_capabilities = Some(0);
    }
    assert!(encode_frame(&helper, Direction::HelperToService).is_err());
}

#[test]
fn failed_exec_cmd_wire_result_preserves_bounded_output_but_never_cursor() {
    let result = pb::ToolResult {
        ordinal: Some(1),
        status: Some(5),
        text: Some("stdout: partial\nstderr: deadline".into()),
        next_offset: None,
        truncated: Some(true),
    };
    assert!(
        encode_frame(
            &envelope(Body::ToolResult(result.clone())),
            Direction::ServiceToHelper
        )
        .is_ok()
    );
    for bad in [
        pb::ToolResult {
            next_offset: Some(1),
            ..result.clone()
        },
        pb::ToolResult {
            text: Some("x".repeat(16385)),
            ..result
        },
    ] {
        assert!(
            encode_frame(&envelope(Body::ToolResult(bad)), Direction::ServiceToHelper).is_err()
        );
    }
}

/// A5: Begin has no time field; removed `deadline_remaining_ms` (field 3)
/// is an unknown field and the Begin is rejected.
#[test]
fn begin_carries_no_deadline_and_removed_field_is_rejected() {
    let begin = pb::Begin {
        model: Some("system".into()),
        input_bytes: Some(1),
        max_response_tokens: Some(512),
        enable_project_tools: None,
    };
    let message = envelope(Body::Begin(begin.clone()));
    assert!(encode_frame(&message, Direction::ServiceToHelper).is_ok());
    let mut body = begin.encode_to_vec();
    body.extend_from_slice(&[0x18, 100]);
    let mut bytes = pb::Envelope {
        operation_id: Some(vec![7; 16]),
        generation: Some(1),
        body: None,
    }
    .encode_to_vec();
    bytes.push(0x5a);
    bytes.push(body.len() as u8);
    bytes.extend(body);
    assert!(decode_body(&bytes, Direction::ServiceToHelper).is_err());
}

#[test]
fn inference_usage_preserves_zero_unknown_and_rejects_invalid_counts() {
    let value = pb::InferenceUsage {
        request_ordinal: Some(1),
        input_tokens: Some(0),
        output_tokens: Some(0),
        cached_input_tokens: None,
        complete: Some(true),
    };
    let message = envelope(Body::InferenceUsage(value));
    let frame = encode_frame(&message, Direction::HelperToService).unwrap();
    assert_eq!(
        decode_body(&frame[4..], Direction::HelperToService).unwrap(),
        message
    );
    assert!(encode_frame(&message, Direction::ServiceToHelper).is_err());
    for invalid in [
        pb::InferenceUsage {
            request_ordinal: Some(65),
            ..value
        },
        pb::InferenceUsage {
            input_tokens: None,
            ..value
        },
        pb::InferenceUsage {
            output_tokens: Some(1_000_000_001),
            ..value
        },
        pb::InferenceUsage {
            cached_input_tokens: Some(1),
            ..value
        },
    ] {
        assert!(
            encode_frame(
                &envelope(Body::InferenceUsage(invalid)),
                Direction::HelperToService
            )
            .is_err()
        );
    }
    assert!(
        encode_frame(
            &envelope(Body::InferenceUsage(pb::InferenceUsage {
                request_ordinal: Some(1),
                complete: Some(false),
                ..Default::default()
            })),
            Direction::HelperToService
        )
        .is_ok()
    );
}

/// B4: every Hello names its selector; a missing one never defaults to "system".
#[test]
fn hello_without_selected_model_is_rejected_in_both_directions() {
    for (service, direction) in [
        (true, Direction::ServiceToHelper),
        (false, Direction::HelperToService),
    ] {
        let mut message = hello(service);
        assert!(encode_frame(&message, direction).is_ok());
        if let Some(Body::Hello(value)) = &mut message.body {
            value.selected_model = None;
        }
        assert!(encode_frame(&message, direction).is_err());
    }
}

/// B3: tool support comes from `supported_capabilities` bit 0 alone; a Hello
/// with any mask shape is valid without a separate tool bit.
#[test]
fn tool_support_is_stated_only_by_the_capability_mask() {
    for mask in [0, 1, 4, 5, 15] {
        let mut message = hello(false);
        if let Some(Body::Hello(value)) = &mut message.body {
            value.supported_capabilities = Some(mask);
            value.capability_source = Some(2);
            value.local_tool_destination = Some(true);
        }
        assert!(
            encode_frame(&message, Direction::HelperToService).is_ok(),
            "{mask}"
        );
    }
}
