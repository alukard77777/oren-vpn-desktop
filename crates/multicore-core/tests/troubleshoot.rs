use multicore_core::elevation_protocol::{BrokerErrorCode, ProtocolError};
use multicore_core::service_protocol::{
    ServiceCommand, ServiceResponse, decode_service_command, decode_service_frame,
    encode_service_frame, service_response_matches,
};
use multicore_core::troubleshoot::{
    DpiService, FixBatchError, FixOutcome, HELPER_FLAG, MAX_PRIVILEGED_FIXES, PrivilegedFix,
    decode_helper_exit, encode_helper_exit, execution_order, helper_arguments,
    parse_helper_arguments, validate_fix_batch,
};

fn raw(payload: &[u8]) -> Vec<u8> {
    let mut frame = (payload.len() as u32).to_le_bytes().to_vec();
    frame.extend_from_slice(payload);
    frame
}

fn every_fix() -> Vec<PrivilegedFix> {
    let mut fixes = vec![
        PrivilegedFix::WinsockReset,
        PrivilegedFix::WinhttpResetProxy,
        PrivilegedFix::ResyncTime,
        PrivilegedFix::ReinstallWintun,
        PrivilegedFix::ReinstallService,
    ];
    fixes.extend(
        DpiService::ALL
            .into_iter()
            .map(|service| PrivilegedFix::RemoveDpiService { service }),
    );
    fixes
}

#[test]
fn the_dpi_allowlist_is_exactly_the_four_known_services() {
    let names = DpiService::ALL.map(DpiService::service_name);
    assert_eq!(names, ["WinDivert", "WinDivert14", "zapret", "GoodbyeDPI"]);
}

#[test]
fn helper_tokens_round_trip_and_nothing_else_parses() {
    for fix in every_fix() {
        let token = fix.token();
        assert!(!token.contains([' ', '"', '\\', '/']), "{token}");
        assert_eq!(PrivilegedFix::parse_token(&token), Some(fix), "{token}");
    }
    for bad in [
        "",
        "winsock",
        "Winsock-Reset",
        "remove-dpi-service:",
        "remove-dpi-service:Spooler",
        "remove-dpi-service:zapret ",
        "remove-dpi-service:..\\zapret",
        "reinstall-service --service",
        "C:\\Windows\\System32\\cmd.exe",
    ] {
        assert_eq!(PrivilegedFix::parse_token(bad), None, "{bad:?}");
    }
}

#[test]
fn batches_are_bounded_unique_and_keep_the_service_reinstall_alone() {
    assert_eq!(validate_fix_batch(&[], false), Err(FixBatchError::Empty));
    assert_eq!(
        validate_fix_batch(
            &[PrivilegedFix::WinsockReset, PrivilegedFix::WinsockReset],
            false
        ),
        Err(FixBatchError::Duplicate)
    );
    let service_side = every_fix()
        .into_iter()
        .filter(|fix| fix.service_may_run())
        .collect::<Vec<_>>();
    assert_eq!(service_side.len(), MAX_PRIVILEGED_FIXES);
    assert_eq!(validate_fix_batch(&service_side, true), Ok(()));
    assert_eq!(
        validate_fix_batch(&every_fix(), false),
        Err(FixBatchError::TooMany)
    );
    assert_eq!(
        validate_fix_batch(&[PrivilegedFix::ReinstallService], true),
        Err(FixBatchError::NotForService)
    );
    assert_eq!(
        validate_fix_batch(&[PrivilegedFix::ReinstallService], false),
        Ok(())
    );
    assert_eq!(
        validate_fix_batch(
            &[
                PrivilegedFix::ReinstallService,
                PrivilegedFix::ReinstallWintun
            ],
            false
        ),
        Err(FixBatchError::ServiceReinstallNotAlone)
    );
}

#[test]
fn helper_arguments_round_trip_and_reject_anything_unexpected() {
    let fixes = vec![
        PrivilegedFix::RemoveDpiService {
            service: DpiService::Zapret,
        },
        PrivilegedFix::WinsockReset,
    ];
    let arguments = helper_arguments(&fixes);
    assert_eq!(
        arguments,
        [HELPER_FLAG, "remove-dpi-service:zapret", "winsock-reset"]
    );
    assert_eq!(parse_helper_arguments(&arguments), Some(fixes));
    for bad in [
        vec![],
        vec![HELPER_FLAG.to_owned()],
        vec!["--service".to_owned()],
        vec!["winsock-reset".to_owned()],
        vec![HELPER_FLAG.to_owned(), "nope".to_owned()],
        vec![
            HELPER_FLAG.to_owned(),
            "winsock-reset".to_owned(),
            "winsock-reset".to_owned(),
        ],
        vec![
            HELPER_FLAG.to_owned(),
            "reinstall-service".to_owned(),
            "reinstall-wintun".to_owned(),
        ],
    ] {
        assert_eq!(parse_helper_arguments(&bad), None, "{bad:?}");
    }
}

#[test]
fn helper_exit_codes_carry_one_outcome_per_fix() {
    let outcomes = [
        FixOutcome::Done,
        FixOutcome::NothingToDo,
        FixOutcome::Failed,
        FixOutcome::Done,
    ];
    let code = encode_helper_exit(&outcomes);
    assert_eq!(decode_helper_exit(code, 4), Some(outcomes.to_vec()));
    // A different count than was sent is not trusted.
    assert_eq!(decode_helper_exit(code, 3), None);
    assert_eq!(decode_helper_exit(code, 5), None);
    let full = [FixOutcome::Failed; MAX_PRIVILEGED_FIXES];
    assert_eq!(
        decode_helper_exit(encode_helper_exit(&full), MAX_PRIVILEGED_FIXES),
        Some(full.to_vec())
    );
    // Ordinary exit codes (refused arguments, a crash, UAC cancelled) are never outcomes.
    for code in [0, 1, 2, 0xC000_0005, 1223] {
        assert_eq!(decode_helper_exit(code, 1), None, "{code:#x}");
    }
}

#[test]
fn troubleshoot_is_an_additive_v2_command_with_a_validated_batch() {
    let command = ServiceCommand::Troubleshoot {
        fixes: vec![
            PrivilegedFix::RemoveDpiService {
                service: DpiService::WinDivert,
            },
            PrivilegedFix::ReinstallWintun,
        ],
    };
    let frame = encode_service_frame(&command).unwrap();
    assert_eq!(decode_service_command(&frame), Ok(command.clone()));
    assert_eq!(
        decode_service_command(&raw(
            br#"{"version":2,"body":{"troubleshoot":{"fixes":["winsock_reset",{"remove_dpi_service":{"service":"zapret"}}]}}}"#
        )),
        Ok(ServiceCommand::Troubleshoot {
            fixes: vec![
                PrivilegedFix::WinsockReset,
                PrivilegedFix::RemoveDpiService {
                    service: DpiService::Zapret
                }
            ]
        })
    );
    // A client cannot name an arbitrary service, add fields, or ask for the service reinstall.
    for payload in [
        br#"{"version":2,"body":{"troubleshoot":{"fixes":[{"remove_dpi_service":{"service":"Spooler"}}]}}}"#.as_slice(),
        br#"{"version":2,"body":{"troubleshoot":{"fixes":[{"remove_dpi_service":{"service":"zapret","name":"Spooler"}}]}}}"#,
        br#"{"version":2,"body":{"troubleshoot":{"fixes":["winsock_reset"],"path":"C:\\x"}}}"#,
        br#"{"version":2,"body":{"troubleshoot":{"fixes":["run_command"]}}}"#,
        br#"{"version":2,"body":{"troubleshoot":{"fixes":[]}}}"#,
        br#"{"version":2,"body":{"troubleshoot":{"fixes":["reinstall_service"]}}}"#,
        br#"{"version":2,"body":{"troubleshoot":{"fixes":["winsock_reset","winsock_reset"]}}}"#,
    ] {
        assert_eq!(
            decode_service_command(&raw(payload)),
            Err(ProtocolError::InvalidSchema),
            "{}",
            String::from_utf8_lossy(payload)
        );
    }
    let response = ServiceResponse::Troubleshot {
        outcomes: vec![FixOutcome::Done, FixOutcome::NothingToDo],
    };
    let frame = encode_service_frame(&response).unwrap();
    assert_eq!(
        decode_service_frame::<ServiceResponse>(&frame).unwrap(),
        response
    );
    assert!(service_response_matches(&command, &response));
    assert!(!service_response_matches(
        &command,
        &ServiceResponse::Troubleshot {
            outcomes: vec![FixOutcome::Done]
        }
    ));
    assert!(!service_response_matches(
        &ServiceCommand::Status,
        &response
    ));
    // An older service rejects the unknown variant instead of misreading it.
    assert!(service_response_matches(
        &command,
        &ServiceResponse::Error {
            code: BrokerErrorCode::InvalidRequest
        }
    ));
}

#[test]
fn dpi_services_stop_before_their_driver_and_winsock_resets_last() {
    let fixes = [
        PrivilegedFix::WinsockReset,
        PrivilegedFix::RemoveDpiService {
            service: DpiService::WinDivert,
        },
        PrivilegedFix::ReinstallWintun,
        PrivilegedFix::RemoveDpiService {
            service: DpiService::Zapret,
        },
        PrivilegedFix::ResyncTime,
    ];
    let order = execution_order(&fixes);
    assert_eq!(order, [3, 1, 4, 2, 0]);
}
