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
        selected_model: None,
        asset_root: None,
        endpoint: None,
        model_name: None,
        build_id: Some(vec![1; 32]),
        schema_digest: Some(vec![2; 32]),
        max_frame_bytes: Some(65536),
        availability: Some(if service { 3 } else { 1 }),
        capabilities: Some(if service { 0 } else { 1 }),
        context_tokens: (!service).then_some(4096),
        reported_context_tokens: (!service).then_some(4096),
        context_source: (!service).then_some(1),
        reason: Some(0),
    }))
}
fn fixtures() -> Vec<(pb::Envelope, Direction)> {
    use Direction::*;
    vec![
        (hello(true), ServiceToHelper),
        (hello(false), HelperToService),
        (
            envelope(Body::Begin(pb::Begin {
                model: Some("system".into()),
                input_bytes: Some(10),
                deadline_remaining_ms: Some(60000),
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
            envelope(Body::ToolCall(pb::ToolCall {
                ordinal: Some(1),
                arguments: Some(pb::tool_call::Arguments::ReadFile(pb::ProjectReadFile {
                    path: Some("README.md".into()),
                    offset: Some(0),
                    limit: Some(1024),
                })),
            })),
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
            32
        ],
        prompt: Some("question".into()),
    };
    let bytes = encode_input(&input).unwrap();
    assert_eq!(decode_input(&bytes).unwrap(), input);
    input.history.push(input.history[0].clone());
    assert!(encode_input(&input).is_err());
    assert!(decode_input(&input.encode_to_vec()).is_err());
    input.history.clear();
    input.prompt = Some("x".repeat(32769));
    assert!(encode_input(&input).is_err());
    input.prompt = Some("x".repeat(32768));
    input.instructions = Some("x".repeat(32768));
    assert!(encode_input(&input).is_err()); // encoded total, even when decoded text fits
    input.instructions = None;
    assert!(encode_input(&input).is_err());
    input.instructions = Some(String::new());
    input.prompt = Some("x".into());
    let mut bytes = input.encode_to_vec();
    bytes.extend_from_slice(&[0x0a, 0]);
    assert!(decode_input(&bytes).is_err());
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
        value.capabilities = Some(3);
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
    write(
        "unknown_support_with_tools.pb",
        "helper",
        false,
        local.encode_to_vec(),
    );
    for field in 13..=16 {
        write(
            &format!("memory_unknown_nested_{field}.pb"),
            "helper",
            false,
            memory_unknown_nested(field),
        );
    }
    let inventory = memory_call(pb::tool_call::Arguments::ListTools(pb::ListTools {}));
    write("list_tools.pb", "helper", true, inventory.encode_to_vec());
    write(
        "list_tools_wrong_direction.pb",
        "service",
        false,
        inventory.encode_to_vec(),
    );
    write(
        "list_tools_nonempty.pb",
        "helper",
        false,
        inventory_unknown_nested(),
    );
    for (name, args, accepted) in [
        (
            "shell_default.pb",
            pb::Shell {
                command: Some("printf fixture".into()),
                cwd: None,
                timeout_seconds: None,
            },
            true,
        ),
        (
            "shell_max.pb",
            pb::Shell {
                command: Some("é".repeat(4096)),
                cwd: Some(".".into()),
                timeout_seconds: Some(60),
            },
            true,
        ),
        (
            "shell_missing_command.pb",
            pb::Shell {
                command: None,
                cwd: None,
                timeout_seconds: None,
            },
            false,
        ),
        (
            "shell_parent.pb",
            pb::Shell {
                command: Some("pwd".into()),
                cwd: Some("a/../b".into()),
                timeout_seconds: Some(30),
            },
            false,
        ),
        (
            "shell_timeout.pb",
            pb::Shell {
                command: Some("pwd".into()),
                cwd: None,
                timeout_seconds: Some(61),
            },
            false,
        ),
    ] {
        write(
            name,
            "helper",
            accepted,
            memory_call(pb::tool_call::Arguments::Shell(args)).encode_to_vec(),
        );
    }
    write(
        "shell_failed_output.pb",
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
    for (name, message, accepted) in memory_fixtures() {
        write(
            &format!("memory_{name}.pb"),
            "helper",
            accepted,
            message.encode_to_vec(),
        );
        if accepted {
            write(
                &format!("memory_{name}_wrong_direction.pb"),
                "service",
                false,
                message.encode_to_vec(),
            );
        }
    }
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
        value.capabilities = Some(0);
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
    let mut oversized = input.clone();
    oversized.prompt.as_mut().unwrap().push('x');
    write(
        "invalid_input_prompt_overflow.pb",
        "input",
        false,
        oversized.encode_to_vec(),
    );
    let mut oversized = input.clone();
    oversized.instructions = Some("x".repeat(32768));
    write(
        "invalid_input_encoded_overflow.pb",
        "input",
        false,
        oversized.encode_to_vec(),
    );
    let mut excessive = input;
    excessive.prompt = Some("x".into());
    excessive.history = vec![
        pb::HistoryTurn {
            role: Some(1),
            text: Some(String::new())
        };
        33
    ];
    write(
        "invalid_input_history_33.pb",
        "input",
        false,
        excessive.encode_to_vec(),
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

#[test]
fn typed_tool_calls_and_results_enforce_direction_and_bounds() {
    let call = envelope(Body::ToolCall(pb::ToolCall {
        ordinal: Some(1),
        arguments: Some(pb::tool_call::Arguments::ReadFile(pb::ProjectReadFile {
            path: Some("src/main.rs".into()),
            offset: Some(0),
            limit: Some(1024),
        })),
    }));
    assert!(encode_frame(&call, Direction::HelperToService).is_ok());
    assert_eq!(
        encode_frame(&call, Direction::ServiceToHelper),
        Err(ProtocolError::WrongDirection)
    );
    let mut bad = call.clone();
    if let Some(Body::ToolCall(v)) = &mut bad.body {
        v.ordinal = Some(9);
    }
    assert_eq!(
        encode_frame(&bad, Direction::HelperToService),
        Err(ProtocolError::InvalidSemantics)
    );
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
        v.capabilities = Some(3);
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
    reply.capabilities = Some(0);
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

fn memory_call(arguments: pb::tool_call::Arguments) -> pb::Envelope {
    envelope(Body::ToolCall(pb::ToolCall {
        ordinal: Some(1),
        arguments: Some(arguments),
    }))
}
fn memory_fixtures() -> Vec<(&'static str, pb::Envelope, bool)> {
    use pb::tool_call::Arguments::*;
    let id = "0123456789abcdef0123456789abcdef";
    vec![
        (
            "create_plain",
            memory_call(MemoryCreateNote(pb::MemoryCreateNote {
                body: Some(" exact\nbody ".into()),
                source_version: None,
            })),
            true,
        ),
        (
            "create_source",
            memory_call(MemoryCreateNote(pb::MemoryCreateNote {
                body: Some("body".into()),
                source_version: Some(id.into()),
            })),
            true,
        ),
        (
            "create_bounded_invalid",
            memory_call(MemoryCreateNote(pb::MemoryCreateNote {
                body: Some(String::new()),
                source_version: Some(String::new()),
            })),
            true,
        ),
        (
            "create_max",
            memory_call(MemoryCreateNote(pb::MemoryCreateNote {
                body: Some("é".repeat(8192)),
                source_version: Some("é".repeat(512)),
            })),
            true,
        ),
        (
            "create_body_oversize",
            memory_call(MemoryCreateNote(pb::MemoryCreateNote {
                body: Some("é".repeat(8192) + "x"),
                source_version: None,
            })),
            false,
        ),
        (
            "create_source_oversize",
            memory_call(MemoryCreateNote(pb::MemoryCreateNote {
                body: Some("x".into()),
                source_version: Some("x".repeat(1025)),
            })),
            false,
        ),
        (
            "create_missing_body",
            memory_call(MemoryCreateNote(pb::MemoryCreateNote {
                body: None,
                source_version: None,
            })),
            false,
        ),
        (
            "list_first",
            memory_call(MemoryListNotes(pb::MemoryListNotes {
                after: None,
                limit: Some(8),
            })),
            true,
        ),
        (
            "list_empty_present",
            memory_call(MemoryListNotes(pb::MemoryListNotes {
                after: Some(String::new()),
                limit: Some(0),
            })),
            true,
        ),
        (
            "list_bounded_invalid",
            memory_call(MemoryListNotes(pb::MemoryListNotes {
                after: Some("é".repeat(512)),
                limit: Some(u32::MAX),
            })),
            true,
        ),
        (
            "list_oversize",
            memory_call(MemoryListNotes(pb::MemoryListNotes {
                after: Some("x".repeat(1025)),
                limit: Some(1),
            })),
            false,
        ),
        (
            "list_missing_limit",
            memory_call(MemoryListNotes(pb::MemoryListNotes {
                after: None,
                limit: None,
            })),
            false,
        ),
        (
            "get_valid",
            memory_call(MemoryGetNote(pb::MemoryGetNote {
                version: Some(id.into()),
                offset: Some(0),
                limit: Some(16384),
            })),
            true,
        ),
        (
            "get_bounded_invalid",
            memory_call(MemoryGetNote(pb::MemoryGetNote {
                version: Some(String::new()),
                offset: Some(u64::MAX),
                limit: Some(u32::MAX),
            })),
            true,
        ),
        (
            "get_missing_version",
            memory_call(MemoryGetNote(pb::MemoryGetNote {
                version: None,
                offset: Some(0),
                limit: Some(1),
            })),
            false,
        ),
        (
            "get_missing_offset",
            memory_call(MemoryGetNote(pb::MemoryGetNote {
                version: Some(id.into()),
                offset: None,
                limit: Some(1),
            })),
            false,
        ),
        (
            "get_missing_limit",
            memory_call(MemoryGetNote(pb::MemoryGetNote {
                version: Some(id.into()),
                offset: Some(0),
                limit: None,
            })),
            false,
        ),
        (
            "get_oversize",
            memory_call(MemoryGetNote(pb::MemoryGetNote {
                version: Some("x".repeat(1025)),
                offset: Some(0),
                limit: Some(1),
            })),
            false,
        ),
        (
            "sources_valid",
            memory_call(MemoryNoteSources(pb::MemoryNoteSources {
                version: Some(id.into()),
            })),
            true,
        ),
        (
            "sources_bounded_invalid",
            memory_call(MemoryNoteSources(pb::MemoryNoteSources {
                version: Some(String::new()),
            })),
            true,
        ),
        (
            "sources_missing_version",
            memory_call(MemoryNoteSources(pb::MemoryNoteSources { version: None })),
            false,
        ),
        (
            "sources_oversize",
            memory_call(MemoryNoteSources(pb::MemoryNoteSources {
                version: Some("x".repeat(1025)),
            })),
            false,
        ),
    ]
}
#[test]
fn memory_calls_preserve_semantic_rejections_and_reject_protocol_faults() {
    for (name, message, accepted) in memory_fixtures() {
        assert_eq!(
            encode_frame(&message, Direction::HelperToService).is_ok(),
            accepted,
            "{name}"
        );
        assert_eq!(
            decode_body(&message.encode_to_vec(), Direction::HelperToService).is_ok(),
            accepted,
            "{name}"
        );
        assert!(
            decode_body(&message.encode_to_vec(), Direction::ServiceToHelper).is_err(),
            "{name}"
        );
    }
}

fn memory_unknown_nested(field: u8) -> Vec<u8> {
    let mut arguments = match field {
        13 => vec![0x10, 8],
        14 => vec![0x0a, 0, 0x10, 0, 0x18, 0],
        15 | 16 => vec![0x0a, 0],
        _ => unreachable!(),
    };
    arguments.extend_from_slice(&[0xa0, 0x06, 1]); // Unknown nested field 100.
    let mut call = vec![8, 1];
    let tag = u16::from(field) << 3 | 2;
    if tag >= 128 {
        call.extend_from_slice(&[(tag as u8 & 127) | 128, (tag >> 7) as u8]);
    } else {
        call.push(tag as u8);
    }
    call.push(u8::try_from(arguments.len()).unwrap());
    call.extend(arguments);
    let mut bytes = pb::Envelope {
        operation_id: Some(vec![7; 16]),
        generation: Some(1),
        body: None,
    }
    .encode_to_vec();
    bytes.extend_from_slice(&[0xa2, 1, u8::try_from(call.len()).unwrap()]);
    bytes.extend(call);
    bytes
}
#[test]
fn memory_calls_reject_unknown_nested_fields_before_prost_discards_them() {
    for field in 13..=16 {
        assert!(decode_body(&memory_unknown_nested(field), Direction::HelperToService).is_err());
    }
}

fn inventory_unknown_nested() -> Vec<u8> {
    let mut bytes = pb::Envelope {
        operation_id: Some(vec![7; 16]),
        generation: Some(1),
        body: None,
    }
    .encode_to_vec();
    // ToolCall field 17 contains unknown field 1 in the empty ListTools message.
    bytes.extend_from_slice(&[0xa2, 1, 7, 8, 1, 0x8a, 1, 2, 8, 1]);
    bytes
}

#[test]
fn inventory_tool_requires_empty_arguments_and_helper_direction() {
    let mut message = memory_call(pb::tool_call::Arguments::ListTools(pb::ListTools {}));
    assert_eq!(
        decode_body(&message.encode_to_vec(), Direction::HelperToService).unwrap(),
        message
    );
    assert!(decode_body(&message.encode_to_vec(), Direction::ServiceToHelper).is_err());
    assert!(decode_body(&inventory_unknown_nested(), Direction::HelperToService).is_err());
    if let Some(Body::ToolCall(call)) = &mut message.body {
        call.ordinal = Some(9);
    }
    assert!(decode_body(&message.encode_to_vec(), Direction::HelperToService).is_err());
}

#[test]
fn audit_tool_requires_bounded_present_limit_and_helper_direction() {
    for limit in [Some(1), Some(16)] {
        let message = memory_call(pb::tool_call::Arguments::ReadAudit(pb::AuditRead { limit }));
        assert_eq!(
            decode_body(&message.encode_to_vec(), Direction::HelperToService).unwrap(),
            message
        );
        assert!(decode_body(&message.encode_to_vec(), Direction::ServiceToHelper).is_err());
    }
    for limit in [None, Some(0), Some(17)] {
        let message = memory_call(pb::tool_call::Arguments::ReadAudit(pb::AuditRead { limit }));
        assert!(decode_body(&message.encode_to_vec(), Direction::HelperToService).is_err());
    }
}

#[test]
fn shell_wire_requires_bounded_command_relative_cwd_and_deadline() {
    let valid = pb::Shell {
        command: Some("printf fixture".into()),
        cwd: None,
        timeout_seconds: None,
    };
    for args in [
        valid.clone(),
        pb::Shell {
            command: Some("é".repeat(4096)),
            cwd: Some(".".into()),
            timeout_seconds: Some(60),
        },
        pb::Shell {
            cwd: Some("src/subdir".into()),
            timeout_seconds: Some(1),
            ..valid.clone()
        },
    ] {
        let message = memory_call(pb::tool_call::Arguments::Shell(args));
        assert_eq!(
            decode_body(&message.encode_to_vec(), Direction::HelperToService).unwrap(),
            message
        );
        assert!(decode_body(&message.encode_to_vec(), Direction::ServiceToHelper).is_err());
    }
    let mut invalid = Vec::new();
    for command in [
        None,
        Some(String::new()),
        Some("a\0b".into()),
        Some("é".repeat(4097)),
    ] {
        invalid.push(pb::Shell {
            command,
            ..valid.clone()
        });
    }
    for cwd in ["", "/tmp", "..", "src/../other", "a\0b", &"x".repeat(1025)] {
        invalid.push(pb::Shell {
            cwd: Some(cwd.into()),
            ..valid.clone()
        });
    }
    for timeout_seconds in [Some(0), Some(61)] {
        invalid.push(pb::Shell {
            timeout_seconds,
            ..valid.clone()
        });
    }
    for args in invalid {
        let message = memory_call(pb::tool_call::Arguments::Shell(args));
        assert!(decode_body(&message.encode_to_vec(), Direction::HelperToService).is_err());
    }
    let mut bytes = pb::Envelope {
        operation_id: Some(vec![7; 16]),
        generation: Some(1),
        body: None,
    }
    .encode_to_vec();
    // ToolCall field 19, valid command x, then unknown nested field 100.
    bytes.extend_from_slice(&[0xa2, 1, 11, 8, 1, 0x9a, 1, 6, 10, 1, b'x', 0xa0, 6, 1]);
    assert!(decode_body(&bytes, Direction::HelperToService).is_err());
}

#[test]
fn failed_shell_wire_result_preserves_bounded_output_but_never_cursor() {
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

#[test]
fn begin_deadline_is_optional_but_explicit_values_remain_bounded() {
    let mut message = envelope(Body::Begin(pb::Begin {
        model: Some("system".into()),
        input_bytes: Some(1),
        deadline_remaining_ms: None,
        max_response_tokens: Some(512),
        enable_project_tools: None,
    }));
    assert!(encode_frame(&message, Direction::ServiceToHelper).is_ok());
    for deadline in [0, 60001] {
        if let Some(Body::Begin(value)) = &mut message.body {
            value.deadline_remaining_ms = Some(deadline);
        }
        assert!(encode_frame(&message, Direction::ServiceToHelper).is_err());
    }
}
