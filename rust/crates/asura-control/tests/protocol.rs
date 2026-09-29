use asura_control::{
    pb::{self, envelope::Body},
    *,
};
fn hello() -> pb::Envelope {
    pb::Envelope {
        service_epoch: None,
        attachment_id: None,
        request_counter: None,
        body: Some(Body::Hello(pb::Hello {
            client_build: Some("asura/0.1.0".into()),
        })),
    }
}
fn inspect() -> pb::Envelope {
    pb::Envelope {
        service_epoch: Some(vec![1; 16]),
        attachment_id: Some(vec![2; 16]),
        request_counter: Some(1),
        body: Some(Body::Inspect(pb::Inspect {})),
    }
}
#[test]
fn every_split_and_coalesced_frames_decode_once() {
    let original = hello();
    let encoded = encode_frame(&original).unwrap();
    for split in 0..=encoded.len() {
        let mut codec = ControlCodec::new();
        assert_eq!(codec.push(&encoded[..split]).unwrap(), split);
        if split < encoded.len() {
            assert!(codec.next_frame().unwrap().is_none());
        }
        assert_eq!(
            codec.push(&encoded[split..]).unwrap(),
            encoded.len() - split
        );
        assert_eq!(
            codec.next_frame().unwrap(),
            Some(Frame::Message(Box::new(original.clone())))
        );
        assert_eq!(codec.next_frame().unwrap(), None);
    }
    let doubled = [encoded.as_slice(), encoded.as_slice()].concat();
    let mut codec = ControlCodec::new();
    assert_eq!(codec.push(&doubled).unwrap(), encoded.len());
    assert_eq!(
        codec.next_frame().unwrap(),
        Some(Frame::Message(Box::new(original)))
    );
    assert_eq!(codec.buffered_len(), 0);
}
#[test]
fn header_rejections_happen_before_body_allocation() {
    let mut bad = version_rejection();
    bad[0] = 0;
    assert_eq!(ControlCodec::new().push(&bad), Err(ProtocolError::BadMagic));
    bad = version_rejection();
    bad[5] = 2;
    bad[11] = 1;
    assert_eq!(
        ControlCodec::new().push(&bad),
        Err(ProtocolError::UnsupportedVersion { major: 2, minor: 1 })
    );
    bad = version_rejection();
    bad[8..].copy_from_slice(&65_537u32.to_be_bytes());
    let mut codec = ControlCodec::new();
    assert_eq!(codec.push(&bad), Err(ProtocolError::InvalidLength));
    assert_eq!(codec.buffered_len(), HEADER_BYTES);
    let mut codec = ControlCodec::new();
    codec.push(&version_rejection()).unwrap();
    assert_eq!(codec.next_frame().unwrap(), Some(Frame::VersionRejected));
    let mut rejection = version_rejection();
    rejection[5] = 2;
    let mut codec = ControlCodec::new();
    codec.push(&rejection).unwrap();
    assert_eq!(codec.next_frame().unwrap(), Some(Frame::VersionRejected));
}
#[test]
fn strict_wire_rejects_ambiguous_and_invalid_encodings() {
    let invalid: &[&[u8]] = &[
        &[0],                         // field zero
        &[0x98, 1, 0],                // unknown field
        &[0x50, 0],                   // body with wrong wire type
        &[0x52, 0, 0x52, 0],          // duplicate body
        &[0x52, 0, 0x62, 0],          // multiple body variants
        &[0x18, 1, 0x18, 1, 0x62, 0], // duplicate scalar
        &[0x52, 3, 0x0a, 2, b'x'],    // truncated nested string
        &[0x52, 3, 0x0a, 1, 0xff],    // invalid UTF-8
        &[0x18, 0x80],                // truncated varint
        &[
            0x18, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 2,
        ], // overflow
        &[
            0x52, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 1,
        ], // overflow length
        &[0x5a, 2, 0x10, 7],          // unknown capability (1 through 6 are defined)
        &[0x5a, 4, 0x12, 0, 0x12, 0], // repeated packed segment
        &[0x5a, 4, 0x12, 0, 0x10, 1], // packed/unpacked mix
        &[0x5a, 6, 0x18, 0x80, 0x80, 0x80, 0x80, 0x10], // uint32 overflow
    ];
    for bytes in invalid {
        assert!(decode_body(bytes).is_err(), "accepted {bytes:?}");
    }
    assert!(decode_body(&[]).is_err());
    assert!(decode_body(&vec![0; MAX_FRAME_BYTES + 1]).is_err());
    assert!(decode_body(&[0x18, 1]).is_err()); // no body
}
#[test]
fn packed_and_unpacked_capability_count_is_bounded() {
    for packed in [false, true] {
        for count in [16, 17] {
            let payload = if packed {
                let mut v = vec![0x12, count];
                v.extend(vec![1; count as usize]);
                v
            } else {
                vec![vec![0x10, 1]; count as usize].concat()
            };
            let mut bytes = vec![0x5a, payload.len() as u8];
            bytes.extend(payload);
            assert_eq!(decode_body(&bytes).is_ok(), count == 16);
        }
    }
}
#[test]
fn semantic_presence_direction_text_and_identity_are_required() {
    let mut value = hello();
    assert!(validate_semantics(&value, Direction::ClientToServer).is_ok());
    assert_eq!(
        validate_semantics(&value, Direction::ServerToClient),
        Err(ProtocolError::WrongDirection)
    );
    value.request_counter = Some(0);
    assert!(validate_semantics(&value, Direction::ClientToServer).is_err());
    for text in ["".to_owned(), "x".repeat(129), "x\ny".into(), "é".into()] {
        let mut value = hello();
        value.body = Some(Body::Hello(pb::Hello {
            client_build: Some(text),
        }));
        assert!(validate_semantics(&value, Direction::ClientToServer).is_err());
    }
    let value = inspect();
    assert!(validate_semantics(&value, Direction::ClientToServer).is_ok());
    assert!(validate_identity(&value, &[1; 16], &[2; 16], 1).is_ok());
    for (epoch, attachment, counter) in [
        ([3; 16], [2; 16], 1),
        ([1; 16], [3; 16], 1),
        ([1; 16], [2; 16], 2),
    ] {
        assert_eq!(
            validate_identity(&value, &epoch, &attachment, counter),
            Err(ProtocolError::IdentityMismatch)
        );
    }
    for id in [vec![0; 16], vec![1; 15], vec![1; 17]] {
        let mut value = inspect();
        value.service_epoch = Some(id);
        assert!(validate_semantics(&value, Direction::ClientToServer).is_err());
    }
}
#[test]
fn service_projection_capabilities_and_stop_epoch_are_exact() {
    let mut value = inspect();
    value.request_counter = None;
    value.body = Some(Body::HelloReply(pb::HelloReply {
        service_build: Some("asura/0.1.0".into()),
        capabilities: vec![1, 2, 3, 4],
        max_frame_bytes: Some(MAX_FRAME_BYTES as u32),
    }));
    assert!(validate_semantics(&value, Direction::ServerToClient).is_ok());
    if let Some(Body::HelloReply(v)) = &mut value.body {
        v.capabilities = vec![1, 1];
    }
    assert!(validate_semantics(&value, Direction::ServerToClient).is_err());
    value = inspect();
    value.body = Some(Body::InspectReply(pb::InspectReply {
        lifecycle: Some(2),
        installation: Some(1),
        unavailable_reason: None,
        reason: Some(12),
    }));
    assert!(validate_semantics(&value, Direction::ServerToClient).is_ok());
    if let Some(Body::InspectReply(v)) = &mut value.body {
        v.installation = Some(2);
    }
    assert!(validate_semantics(&value, Direction::ServerToClient).is_err());
    value = inspect();
    value.body = Some(Body::Stop(pb::Stop {
        expected_epoch: Some(vec![1; 16]),
    }));
    assert!(validate_semantics(&value, Direction::ClientToServer).is_ok());
    if let Some(Body::Stop(v)) = &mut value.body {
        v.expected_epoch = Some(vec![3; 16]);
    }
    assert!(validate_semantics(&value, Direction::ClientToServer).is_err());
}

#[test]
fn protocol_zero_one_is_exact_and_development_versions_are_rejected() {
    let frame = encode_frame(&hello()).unwrap();
    assert_eq!(&frame[4..8], &[0, 0, 0, 1]);
    assert_eq!(&version_rejection()[4..8], &[0, 0, 0, 1]);
    for (major, minor) in [(0u16, 0u16), (0, 2), (1, 0), (1, 1), (1, 2)] {
        let mut other = frame.clone();
        other[4..6].copy_from_slice(&major.to_be_bytes());
        other[6..8].copy_from_slice(&minor.to_be_bytes());
        assert_eq!(
            ControlCodec::new().push(&other),
            Err(ProtocolError::UnsupportedVersion { major, minor })
        );
    }
}

fn installation_reply(state: i32, reason: i32) -> pb::InspectInstallationReply {
    pb::InspectInstallationReply {
        installation: Some(state),
        reason: Some(reason),
        installation_id: None,
        authority_revision: None,
        recorded_owner_generation: None,
        binding_generation: None,
        authority_format: None,
    }
}
fn valid_reply(reply: pb::InspectInstallationReply) -> bool {
    let mut envelope = inspect();
    envelope.body = Some(Body::InspectInstallationReply(reply));
    validate_semantics(&envelope, Direction::ServerToClient).is_ok()
}
#[test]
fn inspection_state_reason_pairs_and_journal_presence_are_exact() {
    let pairs = [
        (3, 1),
        (2, 2),
        (3, 3),
        (4, 4),
        (5, 5),
        (5, 6),
        (5, 7),
        (5, 8),
        (5, 9),
        (5, 10),
        (1, 11),
        (1, 12),
        (1, 13),
        (1, 14),
        (1, 15),
    ];
    for (state, reason) in pairs {
        let mut reply = installation_reply(state, reason);
        if reason == 3 || reason == 4 {
            reply.installation_id = Some(vec![1; 16]);
            reply.authority_revision = Some(1);
            reply.recorded_owner_generation = Some(1);
            reply.authority_format = Some(1);
            if reason == 4 {
                reply.binding_generation = Some(1);
            }
        }
        assert!(valid_reply(reply.clone()), "{state}/{reason}");
        for other in 0..=6 {
            let mut wrong = reply.clone();
            wrong.installation = Some(other);
            assert_eq!(valid_reply(wrong), other == state);
        }
        for field in 0..5 {
            let mut wrong = reply.clone();
            match field {
                0 => {
                    wrong.installation_id = if reason == 3 || reason == 4 {
                        None
                    } else {
                        Some(vec![1; 16])
                    }
                }
                1 => wrong.authority_revision = Some(0),
                2 => wrong.recorded_owner_generation = Some(0),
                3 => wrong.authority_format = Some(2),
                _ => wrong.binding_generation = Some(0),
            }
            assert!(
                !valid_reply(wrong),
                "accepted bad field {field} for reason {reason}"
            );
        }
    }
    let mut missing = installation_reply(2, 2);
    missing.reason = None;
    assert!(!valid_reply(missing));
    assert!(!valid_reply(installation_reply(0, 0)));
    assert!(!valid_reply(installation_reply(1, 16)));
}
#[test]
fn installation_request_uses_identity_and_rejects_legacy_reason() {
    let mut envelope = inspect();
    envelope.body = Some(Body::InspectInstallation(pb::InspectInstallation {}));
    assert!(validate_semantics(&envelope, Direction::ClientToServer).is_ok());
    assert!(validate_semantics(&envelope, Direction::ServerToClient).is_err());
    let bytes = encode_frame(&envelope).unwrap();
    for split in 0..=bytes.len() {
        let mut codec = ControlCodec::new();
        codec.push(&bytes[..split]).unwrap();
        codec.push(&bytes[split..]).unwrap();
        assert_eq!(
            codec.next_frame().unwrap(),
            Some(Frame::Message(Box::new(envelope.clone())))
        );
    }
    envelope.request_counter = Some(0);
    assert!(validate_semantics(&envelope, Direction::ClientToServer).is_err());
    envelope.request_counter = Some(1);
    for reason in [None, Some(2)] {
        envelope.body = Some(Body::InspectReply(pb::InspectReply {
            lifecycle: Some(2),
            installation: Some(2),
            unavailable_reason: Some(1),
            reason,
        }));
        assert!(validate_semantics(&envelope, Direction::ServerToClient).is_err());
    }
}

#[test]
fn installation_replay_fields_reject_invalid_identity_and_roundtrip() {
    let valid = pb::InspectInstallationReply {
        installation: Some(4),
        reason: Some(4),
        installation_id: Some(vec![7; 16]),
        authority_revision: Some(3),
        recorded_owner_generation: Some(2),
        binding_generation: Some(1),
        authority_format: Some(1),
    };
    let mut envelope = inspect();
    envelope.body = Some(Body::InspectInstallationReply(valid.clone()));
    let encoded = encode_frame(&envelope).unwrap();
    let mut codec = ControlCodec::new();
    for byte in encoded {
        codec.push(&[byte]).unwrap();
    }
    assert_eq!(
        codec.next_frame().unwrap(),
        Some(Frame::Message(Box::new(envelope)))
    );
    for id in [vec![], vec![0; 16], vec![7; 15], vec![7; 17]] {
        let mut reply = valid.clone();
        reply.installation_id = Some(id);
        assert!(!valid_reply(reply));
    }
    let mut pending = valid.clone();
    pending.installation = Some(3);
    pending.reason = Some(3);
    assert!(!valid_reply(pending.clone()));
    pending.binding_generation = None;
    assert!(valid_reply(pending));
    for field in 0..5 {
        let mut reply = valid.clone();
        match field {
            0 => reply.authority_revision = None,
            1 => reply.recorded_owner_generation = None,
            2 => reply.binding_generation = None,
            3 => reply.authority_format = None,
            _ => reply.installation = None,
        }
        assert!(!valid_reply(reply));
    }
}

#[test]
fn config_messages_are_bounded_directional_and_correlated() {
    for body in [
        Body::ConfigGet(pb::ConfigGet {
            key: Some("model".into()),
        }),
        Body::ConfigSet(pb::ConfigSet {
            key: Some("audit.enabled".into()),
            value_yaml: Some("true".into()),
        }),
    ] {
        let mut message = inspect();
        message.body = Some(body);
        assert!(validate_semantics(&message, Direction::ClientToServer).is_ok());
        assert_eq!(
            validate_semantics(&message, Direction::ServerToClient),
            Err(ProtocolError::WrongDirection)
        );
        assert_eq!(
            decode_body(&encode_frame(&message).unwrap()[HEADER_BYTES..]).unwrap(),
            message
        );
    }
    let mut message = inspect();
    message.body = Some(Body::ConfigSet(pb::ConfigSet {
        key: Some("model".into()),
        value_yaml: Some("x".repeat(4097)),
    }));
    assert!(validate_semantics(&message, Direction::ClientToServer).is_err());
    for reply in [
        pb::ConfigReply {
            value_yaml: Some("true\n".into()),
            error: None,
        },
        pb::ConfigReply {
            value_yaml: None,
            error: Some("config_busy".into()),
        },
    ] {
        message.body = Some(Body::ConfigReply(reply));
        assert!(validate_semantics(&message, Direction::ServerToClient).is_ok());
        assert!(validate_identity(&message, &[1; 16], &[2; 16], 1).is_ok());
        assert!(validate_identity(&message, &[1; 16], &[2; 16], 2).is_err());
    }
    for reply in [
        pb::ConfigReply {
            value_yaml: None,
            error: None,
        },
        pb::ConfigReply {
            value_yaml: Some("true".into()),
            error: Some("config_busy".into()),
        },
    ] {
        message.body = Some(Body::ConfigReply(reply));
        assert!(validate_semantics(&message, Direction::ServerToClient).is_err());
    }
    let mut old = encode_frame(&hello()).unwrap();
    old[5] = 1;
    old[7] = 1;
    assert_eq!(
        ControlCodec::new().push(&old),
        Err(ProtocolError::UnsupportedVersion { major: 1, minor: 1 })
    );
}

#[test]
fn model_inventory_contract_bounds_partial_results_and_direction() {
    let mut envelope = inspect();
    envelope.body = Some(Body::ModelsList(pb::ModelsList {}));
    assert!(validate_semantics(&envelope, Direction::ClientToServer).is_ok());
    assert!(validate_semantics(&envelope, Direction::ServerToClient).is_err());
    let rows: Vec<_> = (0..65)
        .map(|i| pb::ModelInventoryEntry {
            selector: Some(format!("mlx:model{i}")),
            provider: Some("mlx".into()),
            status: Some(2),
            detail: None,
        })
        .collect();
    let reply = pb::ModelsReply {
        configured_model: Some("mlx:model0".into()),
        models: rows,
        issues: vec![pb::ModelInventoryIssue {
            provider: Some("ollama".into()),
            reason: Some("provider_unavailable".into()),
        }],
        error: None,
    };
    envelope.body = Some(Body::ModelsReply(reply.clone()));
    assert!(validate_semantics(&envelope, Direction::ServerToClient).is_ok());
    let encoded = encode_frame(&envelope).unwrap();
    let mut codec = ControlCodec::new();
    codec.push(&encoded).unwrap();
    assert_eq!(
        codec.next_frame().unwrap(),
        Some(Frame::Message(Box::new(envelope.clone())))
    );
    for mutation in 0..6 {
        let mut invalid = reply.clone();
        match mutation {
            0 => invalid.models.push(invalid.models[0].clone()),
            1 => invalid.models[1].selector = invalid.models[0].selector.clone(),
            2 => invalid.models[0].status = Some(0),
            3 => invalid.models[0].detail = Some("bad\ncontrol".into()),
            4 => invalid.configured_model = Some("mlx:absent".into()),
            _ => invalid.error = Some("models_busy".into()),
        }
        envelope.body = Some(Body::ModelsReply(invalid));
        assert!(validate_semantics(&envelope, Direction::ServerToClient).is_err());
    }
    envelope.body = Some(Body::ModelsReply(pb::ModelsReply {
        configured_model: Some("bad selection".into()),
        models: vec![pb::ModelInventoryEntry {
            selector: Some("bad selection".into()),
            provider: Some("unknown".into()),
            status: Some(4),
            detail: Some("unsupported_selection".into()),
        }],
        ..Default::default()
    }));
    assert!(validate_semantics(&envelope, Direction::ServerToClient).is_ok());
}

#[test]
fn stored_memory_status_is_bounded_truthful_and_paired_with_service_uptime() {
    let observation = pb::ServiceObservation {
        revision: Some(1),
        pending: Some(false),
        status: Some(pb::InspectReply {
            lifecycle: Some(2),
            installation: Some(6),
            reason: Some(16),
            unavailable_reason: None,
        }),
        configured_model: None,
        uptime_ms: Some(100),
        stored_memory: Some(pb::StoredMemoryStatus {
            available: Some(true),
            size_bytes: Some(1024),
            sampled_uptime_ms: Some(90),
            stale: Some(false),
            size_reason: Some(1),
        }),
    };
    let envelope = |value| pb::Envelope {
        service_epoch: Some(vec![1; 16]),
        attachment_id: Some(vec![2; 16]),
        request_counter: Some(1),
        body: Some(Body::ServiceObservation(value)),
    };
    let valid = envelope(observation.clone());
    assert!(validate_semantics(&valid, Direction::ServerToClient).is_ok());
    let frame = encode_frame(&valid).unwrap();
    assert_eq!(decode_body(&frame[HEADER_BYTES..]).unwrap(), valid);
    for change in 0..9 {
        let mut bad = observation.clone();
        let memory = bad.stored_memory.as_mut().unwrap();
        match change {
            0 => bad.uptime_ms = None,
            1 => memory.available = Some(false),
            2 => memory.size_bytes = None,
            3 => memory.sampled_uptime_ms = Some(101),
            4 => memory.stale = Some(true),
            5 => memory.size_reason = Some(0),
            6 => memory.size_reason = Some(8),
            7 => memory.stale = None,
            _ => bad.stored_memory = None,
        }
        assert!(validate_semantics(&envelope(bad), Direction::ServerToClient).is_err());
    }
    for reason in 2..=7 {
        let mut stale = observation.clone();
        let memory = stale.stored_memory.as_mut().unwrap();
        memory.stale = Some(true);
        memory.size_reason = Some(reason);
        assert!(validate_semantics(&envelope(stale.clone()), Direction::ServerToClient).is_ok());
        let memory = stale.stored_memory.as_mut().unwrap();
        memory.size_bytes = None;
        memory.sampled_uptime_ms = None;
        assert!(validate_semantics(&envelope(stale), Direction::ServerToClient).is_ok());
    }
}
