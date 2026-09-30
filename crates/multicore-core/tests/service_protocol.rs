use multicore_core::elevation_protocol::{
    BrokerErrorCode, ElevatedEngine, ElevatedRuntimeState, ElevationCommand, ProtocolError,
    decode_command, encode_frame,
};
use multicore_core::service_protocol::{
    ControllerEndpoint, SERVICE_PROTOCOL_VERSION, ServiceCommand, ServiceResponse,
    decode_service_command, decode_service_frame, encode_service_frame, service_response_matches,
};

fn raw(payload: &[u8]) -> Vec<u8> {
    let mut frame = (payload.len() as u32).to_le_bytes().to_vec();
    frame.extend_from_slice(payload);
    frame
}

#[test]
fn every_v2_command_round_trips() {
    assert_eq!(SERVICE_PROTOCOL_VERSION, 2);
    for command in [
        ServiceCommand::Prepare { generation_id: 7 },
        ServiceCommand::SetTun { on: true },
        ServiceCommand::SetTun { on: false },
        ServiceCommand::Status,
        ServiceCommand::Stop {
            engine: ElevatedEngine::Mihomo,
        },
        ServiceCommand::Diagnostics,
        ServiceCommand::Shutdown,
        ServiceCommand::Update {
            version: "1.1.0".into(),
        },
    ] {
        let frame = encode_service_frame(&command).unwrap();
        assert_eq!(decode_service_command(&frame), Ok(command));
    }
}

#[test]
fn unknown_fields_zero_generation_and_bad_versions_are_rejected() {
    assert_eq!(
        decode_service_command(&raw(
            br#"{"version":2,"body":{"set_tun":{"on":true,"path":"C:\\x"}}}"#
        )),
        Err(ProtocolError::InvalidSchema)
    );
    assert_eq!(
        decode_service_command(&raw(
            br#"{"version":2,"body":{"prepare":{"generation_id":1,"config":"x"}}}"#
        )),
        Err(ProtocolError::InvalidSchema)
    );
    assert_eq!(
        decode_service_command(&raw(
            br#"{"version":2,"body":{"prepare":{"generation_id":0}}}"#
        )),
        Err(ProtocolError::InvalidGeneration)
    );
    for version in [
        "",
        "1.1",
        "1.1.0-beta",
        "../1.0.0",
        "01.2.3x",
        &"9".repeat(40),
    ] {
        let frame = encode_service_frame(&ServiceCommand::Update {
            version: version.to_owned(),
        })
        .unwrap();
        assert_eq!(
            decode_service_command(&frame),
            Err(ProtocolError::InvalidSchema),
            "{version}"
        );
    }
}

#[test]
fn v1_and_v2_frames_never_cross() {
    let v1 = encode_frame(&ElevationCommand::Shutdown).unwrap();
    assert_eq!(
        decode_service_command(&v1),
        Err(ProtocolError::UnknownVersion)
    );
    let v2 = encode_service_frame(&ServiceCommand::Shutdown).unwrap();
    assert_eq!(decode_command(&v2), Err(ProtocolError::UnknownVersion));
}

#[test]
fn prepared_round_trips_and_debug_redacts_the_secret() {
    let response = ServiceResponse::Prepared {
        generation_id: 3,
        controller: ControllerEndpoint {
            port: 41234,
            secret: "controller-secret".into(),
            mixed_port: 28765,
        },
    };
    let frame = encode_service_frame(&response).unwrap();
    assert_eq!(
        decode_service_frame::<ServiceResponse>(&frame).unwrap(),
        response
    );
    assert!(!format!("{response:?}").contains("controller-secret"));
    let extra = raw(br#"{"version":2,"body":{"tun_state":{"on":true,"adapter":"x"}}}"#);
    assert_eq!(
        decode_service_frame::<ServiceResponse>(&extra),
        Err(ProtocolError::InvalidSchema)
    );
}

#[test]
fn responses_pair_with_their_commands_and_busy_or_error_pair_with_all() {
    let status = ServiceResponse::Status {
        generation_id: None,
        xray: ElevatedRuntimeState::Stopped,
        mihomo: ElevatedRuntimeState::Stopped,
        tun_on: false,
        failure: None,
    };
    assert!(service_response_matches(&ServiceCommand::Status, &status));
    assert!(service_response_matches(
        &ServiceCommand::SetTun { on: true },
        &ServiceResponse::TunState { on: true }
    ));
    assert!(!service_response_matches(
        &ServiceCommand::SetTun { on: true },
        &ServiceResponse::TunState { on: false }
    ));
    assert!(!service_response_matches(
        &ServiceCommand::Prepare { generation_id: 4 },
        &ServiceResponse::Prepared {
            generation_id: 5,
            controller: ControllerEndpoint {
                port: 1,
                secret: "s".into(),
                mixed_port: 2
            }
        }
    ));
    for command in [
        ServiceCommand::Prepare { generation_id: 1 },
        ServiceCommand::Shutdown,
        ServiceCommand::Update {
            version: "1.2.3".into(),
        },
    ] {
        assert!(service_response_matches(&command, &ServiceResponse::Busy));
        assert!(service_response_matches(
            &command,
            &ServiceResponse::Error {
                code: BrokerErrorCode::NotReady
            }
        ));
    }
    assert!(!service_response_matches(
        &ServiceCommand::Shutdown,
        &ServiceResponse::UpdateStarted
    ));
}
