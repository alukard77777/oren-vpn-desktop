use multicore_core::elevation_protocol::{
    BrokerErrorCode, ElevatedEngine, ElevatedRuntimeState, ElevationCommand, ProtocolError,
    decode_command, encode_frame,
};
use multicore_core::service_protocol::{
    ControllerEndpoint, CoreJournalCursor, SERVICE_PROTOCOL_VERSION, ServiceCommand,
    ServiceLogFile, ServiceResponse, ServiceUpdateStage, decode_service_command,
    decode_service_frame, encode_service_frame, redact_service_log, service_response_matches,
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
        ServiceCommand::StageUpdate {
            version: "1.1.0".into(),
        },
        ServiceCommand::ServiceLog {
            file: ServiceLogFile::Current,
            before: None,
        },
        ServiceCommand::ServiceLog {
            file: ServiceLogFile::Previous,
            before: Some(4096),
        },
        ServiceCommand::CoreJournal {
            since_ms: 0,
            cursor: None,
        },
        ServiceCommand::UpdateProgress,
        ServiceCommand::CoreJournal {
            since_ms: 1_791_064_846_000,
            cursor: Some(CoreJournalCursor {
                segment: 1_791_064_000_000,
                offset: 4096,
            }),
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
        for command in [
            ServiceCommand::Update {
                version: version.to_owned(),
            },
            ServiceCommand::StageUpdate {
                version: version.to_owned(),
            },
        ] {
            let frame = encode_service_frame(&command).unwrap();
            assert_eq!(
                decode_service_command(&frame),
                Err(ProtocolError::InvalidSchema),
                "{command:?}"
            );
        }
    }
}

/// The two-phase update is additive: `Update` keeps its v2 wire form, so a client that only
/// knows `Update` still drives a newer service, and `stage_update` is a new variant a v2
/// service either understands or rejects as an invalid request (never misreads).
#[test]
fn staging_is_an_additive_v2_command() {
    assert_eq!(
        decode_service_command(&raw(
            br#"{"version":2,"body":{"update":{"version":"1.2.0"}}}"#
        )),
        Ok(ServiceCommand::Update {
            version: "1.2.0".into()
        })
    );
    assert_eq!(
        decode_service_command(&raw(
            br#"{"version":2,"body":{"stage_update":{"version":"1.2.0"}}}"#
        )),
        Ok(ServiceCommand::StageUpdate {
            version: "1.2.0".into()
        })
    );
    let staged = ServiceResponse::UpdateStaged {
        version: "1.2.0".into(),
    };
    let frame = encode_service_frame(&staged).unwrap();
    assert_eq!(
        decode_service_frame::<ServiceResponse>(&frame).unwrap(),
        staged
    );
    let stage = ServiceCommand::StageUpdate {
        version: "1.2.0".into(),
    };
    assert!(service_response_matches(&stage, &staged));
    assert!(!service_response_matches(
        &stage,
        &ServiceResponse::UpdateStaged {
            version: "1.2.1".into()
        }
    ));
    assert!(!service_response_matches(
        &stage,
        &ServiceResponse::UpdateStarted
    ));
    assert!(!service_response_matches(
        &ServiceCommand::Update {
            version: "1.2.0".into()
        },
        &staged
    ));
    assert!(service_response_matches(&stage, &ServiceResponse::Busy));
}

/// The download progress of a running `StageUpdate`/`Update` is additive and read-only: no
/// arguments on the wire, and a v2 service that does not know it rejects it (never misreads).
#[test]
fn update_progress_is_an_additive_read_only_v2_command() {
    assert_eq!(
        decode_service_command(&raw(br#"{"version":2,"body":"update_progress"}"#)),
        Ok(ServiceCommand::UpdateProgress)
    );
    assert_eq!(
        decode_service_command(&raw(
            br#"{"version":2,"body":{"update_progress":{"path":"x"}}}"#
        )),
        Err(ProtocolError::InvalidSchema)
    );
    let downloading = ServiceResponse::UpdateProgress {
        stage: ServiceUpdateStage::Downloading,
        received: 18,
        total: 52,
    };
    let frame = encode_service_frame(&downloading).unwrap();
    assert_eq!(
        decode_service_frame::<ServiceResponse>(&frame).unwrap(),
        downloading
    );
    let ask = ServiceCommand::UpdateProgress;
    assert!(service_response_matches(&ask, &downloading));
    for stage in [
        ServiceUpdateStage::Idle,
        ServiceUpdateStage::Downloading,
        ServiceUpdateStage::Verifying,
    ] {
        // An unknown size (`total` 0) is allowed; more received than the size is not.
        assert!(service_response_matches(
            &ask,
            &ServiceResponse::UpdateProgress {
                stage,
                received: 0,
                total: 0,
            }
        ));
        assert!(!service_response_matches(
            &ask,
            &ServiceResponse::UpdateProgress {
                stage,
                received: 53,
                total: 52,
            }
        ));
    }
    assert!(!service_response_matches(
        &ask,
        &ServiceResponse::UpdateStarted
    ));
    assert!(!service_response_matches(
        &ServiceCommand::Status,
        &downloading
    ));
    assert!(service_response_matches(
        &ask,
        &ServiceResponse::Error {
            code: BrokerErrorCode::InvalidRequest
        }
    ));
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

/// The service log is additive like `StageUpdate`: a fixed wire form a newer service decodes,
/// a file chosen from an enum (never a path), and a response that pairs only with its file and
/// never claims bytes past the requested end.
#[test]
fn the_service_log_is_an_additive_read_only_v2_command() {
    assert_eq!(
        decode_service_command(&raw(
            br#"{"version":2,"body":{"service_log":{"file":"previous","before":10}}}"#
        )),
        Ok(ServiceCommand::ServiceLog {
            file: ServiceLogFile::Previous,
            before: Some(10)
        })
    );
    for body in [
        br#"{"version":2,"body":{"service_log":{"file":"C:\\Windows\\win.ini","before":null}}}"#
            .as_slice(),
        br#"{"version":2,"body":{"service_log":{"file":"current","before":null,"path":"x"}}}"#,
        br#"{"version":2,"body":{"service_log":{"file":"current","before":-1}}}"#,
    ] {
        assert_eq!(
            decode_service_command(&raw(body)),
            Err(ProtocolError::InvalidSchema)
        );
    }
    let chunk = ServiceResponse::ServiceLog {
        file: ServiceLogFile::Current,
        start: 100,
        end: 200,
        text: "1 prepare ok\n".into(),
    };
    let frame = encode_service_frame(&chunk).unwrap();
    assert_eq!(
        decode_service_frame::<ServiceResponse>(&frame).unwrap(),
        chunk
    );
    let ask = |file, before| ServiceCommand::ServiceLog { file, before };
    assert!(service_response_matches(
        &ask(ServiceLogFile::Current, None),
        &chunk
    ));
    assert!(service_response_matches(
        &ask(ServiceLogFile::Current, Some(200)),
        &chunk
    ));
    assert!(!service_response_matches(
        &ask(ServiceLogFile::Current, Some(150)),
        &chunk
    ));
    assert!(!service_response_matches(
        &ask(ServiceLogFile::Previous, None),
        &chunk
    ));
    assert!(!service_response_matches(
        &ask(ServiceLogFile::Current, None),
        &ServiceResponse::ServiceLog {
            file: ServiceLogFile::Current,
            start: 9,
            end: 8,
            text: String::new(),
        }
    ));
    assert!(!service_response_matches(
        &ServiceCommand::Diagnostics,
        &chunk
    ));
    assert!(service_response_matches(
        &ask(ServiceLogFile::Current, None),
        &ServiceResponse::Error {
            code: BrokerErrorCode::InvalidRequest
        }
    ));
    assert_eq!(ServiceLogFile::Current.file_name(), "service.log");
    assert_eq!(ServiceLogFile::Previous.file_name(), "service.log.1");
}

/// The core journal is additive too: no path on the wire (the service picks the caller's own
/// directory), and a page must move forward or end, so a client's paging always terminates.
#[test]
fn the_core_journal_is_an_additive_read_only_v2_command() {
    assert_eq!(
        decode_service_command(&raw(
            br#"{"version":2,"body":{"core_journal":{"since_ms":5,"cursor":{"segment":7,"offset":9}}}}"#
        )),
        Ok(ServiceCommand::CoreJournal {
            since_ms: 5,
            cursor: Some(CoreJournalCursor {
                segment: 7,
                offset: 9
            })
        })
    );
    for body in [
        br#"{"version":2,"body":{"core_journal":{"since_ms":5,"cursor":null,"path":"C:\\x"}}}"#
            .as_slice(),
        br#"{"version":2,"body":{"core_journal":{"since_ms":5,"cursor":{"segment":1,"offset":2,"user":"x"}}}}"#,
        br#"{"version":2,"body":{"core_journal":{"since_ms":-5,"cursor":null}}}"#,
    ] {
        assert_eq!(
            decode_service_command(&raw(body)),
            Err(ProtocolError::InvalidSchema)
        );
    }
    let at = CoreJournalCursor {
        segment: 7,
        offset: 9,
    };
    let ask = |cursor| ServiceCommand::CoreJournal {
        since_ms: 0,
        cursor,
    };
    let page = |next| ServiceResponse::CoreJournal {
        text: "1\tmihomo\tstdout\tok\n".into(),
        next,
    };
    let frame = encode_service_frame(&page(Some(at))).unwrap();
    assert_eq!(
        decode_service_frame::<ServiceResponse>(&frame).unwrap(),
        page(Some(at))
    );
    assert!(service_response_matches(&ask(None), &page(Some(at))));
    assert!(service_response_matches(&ask(Some(at)), &page(None)));
    assert!(!service_response_matches(&ask(Some(at)), &page(Some(at))));
    assert!(!service_response_matches(
        &ServiceCommand::Status,
        &page(None)
    ));
}

#[test]
fn the_service_log_never_shows_links_secrets_ids_or_user_names() {
    let planted = [
        // `{error:?}` of a path doubles its backslashes.
        "1700000000 prepare generation 3 refused (NotReady) at resolve: Owner(\"C:\\\\Users\\\\alice\\\\AppData\\\\Local\\\\MultiCore\") C:\\Users\\dave\\x",
        "1700000001 fetch https://panel.example.net/api/sub/Zx9QpLm2Kd7Vb4Ns failed",
        "1700000002 share vless://0f8e6a1c-1111-2222-3333-444455556666@node.example.org:443?sni=x#Name",
        "1700000003 token=Zx9QpLm2Kd7Vb4Ns secret: \"3f9a1c7e5b2d8f60\" subscription_url=abc",
        "1700000004 Authorization: Bearer 3f9a1c7e5b2d8f604a1e9c3b7d5f2a86",
        "1700000005 client 1:S-1-5-21-1111111111-2222222222-3333333333-1001 busy",
        "1700000006 path /home/bob/.local/share/multicore and C:/Users/carol/x",
        "1700000007 config proxies: [{name: a, server: hidden.example.com}]",
        "1700000008 bell\u{7}\r ok",
    ]
    .join("\n");
    let redacted = redact_service_log(&planted);
    for secret in [
        "panel.example.net",
        "Zx9QpLm2Kd7Vb4Ns",
        "vless://",
        "node.example.org",
        "0f8e6a1c-1111-2222-3333-444455556666",
        "3f9a1c7e5b2d8f60",
        "alice",
        "dave",
        "bob",
        "carol",
        "1111111111",
        "hidden.example.com",
        "abc",
        "\u{7}",
        "\r",
    ] {
        assert!(!redacted.contains(secret), "leaks {secret}: {redacted}");
    }
    assert_eq!(redacted.lines().count(), 9, "one line per line: {redacted}");
    assert!(redacted.contains("1700000000 prepare generation 3 refused (NotReady) at resolve"));
    assert!(redacted.contains("C:\\\\Users\\\\[user]\\\\AppData"));
    assert!(redacted.contains("C:\\Users\\[user]\\x"));
    assert!(redacted.contains("/home/[user]/.local"));
    assert!(redacted.contains("1700000007 [redacted privileged diagnostic]"));
    assert!(redacted.contains("1700000008 bell"));
}
