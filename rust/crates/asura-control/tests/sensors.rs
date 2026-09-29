use asura_control::{Direction, HEADER_BYTES, decode_body, encode_frame, pb, validate_semantics};
use pb::envelope::Body;
fn envelope(body: Body) -> pb::Envelope {
    pb::Envelope {
        service_epoch: Some(vec![1; 16]),
        attachment_id: Some(vec![2; 16]),
        request_counter: Some(1),
        body: Some(body),
    }
}
fn query() -> pb::SensorsInspect {
    pb::SensorsInspect {
        project_id: Some(vec![3; 16]),
        revision: None,
        offset: Some(0),
        limit: Some(16),
    }
}
fn page() -> pb::SensorsReply {
    pb::SensorsReply {
        project_id: Some(vec![3; 16]),
        revision: Some(0),
        offset: Some(0),
        total_observations: Some(0),
        pending_persistence: Some(false),
        intake_unavailable: Some(false),
        clock_uncertain: Some(false),
        ..Default::default()
    }
}
#[test]
fn sensor_query_direction_cursor_and_frame_contract() {
    let original = envelope(Body::SensorsInspect(query()));
    assert!(validate_semantics(&original, Direction::ClientToServer).is_ok());
    assert!(validate_semantics(&original, Direction::ServerToClient).is_err());
    let encoded = encode_frame(&original).unwrap();
    assert_eq!(decode_body(&encoded[HEADER_BYTES..]).unwrap(), original);
    for invalid in [
        pb::SensorsInspect {
            project_id: Some(vec![0; 16]),
            ..query()
        },
        pb::SensorsInspect {
            limit: Some(17),
            ..query()
        },
        pb::SensorsInspect {
            limit: Some(0),
            ..query()
        },
        pb::SensorsInspect {
            offset: Some(65),
            ..query()
        },
        pb::SensorsInspect {
            offset: Some(1),
            ..query()
        },
    ] {
        assert!(
            validate_semantics(
                &envelope(Body::SensorsInspect(invalid)),
                Direction::ClientToServer
            )
            .is_err()
        );
    }
}
#[test]
fn sensor_reply_health_errors_and_page_consistency() {
    let original = envelope(Body::SensorsReply(page()));
    assert!(validate_semantics(&original, Direction::ServerToClient).is_ok());
    let encoded = encode_frame(&original).unwrap();
    assert_eq!(decode_body(&encoded[HEADER_BYTES..]).unwrap(), original);
    for invalid in [
        pb::SensorsReply {
            next_offset: Some(0),
            ..page()
        },
        pb::SensorsReply {
            total_observations: Some(65),
            ..page()
        },
        pb::SensorsReply {
            clock_uncertain: None,
            ..page()
        },
        pb::SensorsReply {
            error: Some("sensor_loading".into()),
            ..page()
        },
        pb::SensorsReply {
            observations: vec![pb::SensorObservation::default(); 17],
            ..page()
        },
        pb::SensorsReply {
            proposals: vec![pb::SensorProposal::default(); 9],
            ..page()
        },
    ] {
        assert!(
            validate_semantics(
                &envelope(Body::SensorsReply(invalid)),
                Direction::ServerToClient
            )
            .is_err()
        );
    }
    let error = pb::SensorsReply {
        error: Some("revision_conflict".into()),
        ..Default::default()
    };
    assert!(
        validate_semantics(
            &envelope(Body::SensorsReply(error)),
            Direction::ServerToClient
        )
        .is_ok()
    );
}
