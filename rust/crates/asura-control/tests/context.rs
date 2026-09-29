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
#[test]
fn context_scope_cursor_and_direction_are_strict() {
    let request = pb::ObserveContext {
        project_id: Some(vec![3; 16]),
        working_directory: Some("/project/src".into()),
        ..Default::default()
    };
    let value = envelope(Body::ObserveContext(request.clone()));
    assert!(validate_semantics(&value, Direction::ClientToServer).is_ok());
    assert!(validate_semantics(&value, Direction::ServerToClient).is_err());
    let frame = encode_frame(&value).unwrap();
    assert_eq!(decode_body(&frame[HEADER_BYTES..]).unwrap(), value);
    for path in [
        "relative",
        "/project/../outside",
        "/project/./src",
        "/bad\0path",
    ] {
        let mut invalid = request.clone();
        invalid.working_directory = Some(path.into());
        assert!(
            validate_semantics(
                &envelope(Body::ObserveContext(invalid)),
                Direction::ClientToServer
            )
            .is_err()
        );
    }
    let mut invalid = request;
    invalid.after_revision = Some(1);
    assert!(
        validate_semantics(
            &envelope(Body::ObserveContext(invalid)),
            Direction::ClientToServer
        )
        .is_err()
    );
}
#[test]
fn pending_observations_cannot_claim_git_results() {
    let mut reply = pb::ContextObservation {
        project_id: Some(vec![3; 16]),
        working_directory: Some("/project".into()),
        subscription_id: Some(vec![4; 16]),
        revision: Some(0),
        pending: Some(true),
        ..Default::default()
    };
    let valid = |reply| {
        validate_semantics(
            &envelope(Body::ContextObservation(reply)),
            Direction::ServerToClient,
        )
        .is_ok()
    };
    assert!(valid(reply.clone()));
    reply.git_state = Some(2);
    assert!(!valid(reply.clone()));
    reply.pending = Some(false);
    reply.detached = Some(false);
    reply.unborn = Some(false);
    reply.conflicts = Some(false);
    assert!(!valid(reply.clone())); // no observed revision
    reply.revision = Some(1);
    reply.files_changed = Some(0);
    assert!(valid(reply.clone()));
    reply.branch = Some("feature/日本語".into());
    assert!(valid(reply.clone()));
    reply.branch = Some("bad\x1b[2J".into());
    assert!(!valid(reply.clone()));
    reply.branch = None;
    reply.added = Some(12);
    assert!(!valid(reply.clone()));
    reply.deleted = Some(3);
    assert!(valid(reply.clone()));
    reply.files_changed = None;
    reply.added = None;
    reply.deleted = None;
    reply.git_state = Some(0);
    assert!(!valid(reply.clone()));
    reply.reason = Some("watch_unavailable".into());
    assert!(valid(reply));
}
