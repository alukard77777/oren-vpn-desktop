//! Service-mode unit tests. The host binary sets `test = false`, so its modules are compiled
//! into this test crate with `#[path]`, like `broker_contract.rs` does for the on-demand host.

#[path = "../src/runtime_host.rs"]
#[allow(dead_code)]
mod runtime_host;
#[cfg(windows)]
#[path = "../src/runtime_paths.rs"]
#[allow(dead_code)]
mod runtime_paths;
#[path = "../src/service/mod.rs"]
#[allow(dead_code)]
mod service;
#[cfg(unix)]
#[path = "../src/unix_runtime_paths.rs"]
#[allow(dead_code)]
mod unix_runtime_paths;
#[cfg(windows)]
#[path = "../src/windows_pipe.rs"]
#[allow(dead_code)]
mod windows_pipe;
#[cfg(windows)]
#[path = "../src/wintun_cleanup.rs"]
#[allow(dead_code)]
mod wintun_cleanup;

use std::path::PathBuf;

use multicore_core::elevation_protocol::{BrokerErrorCode, ElevatedRuntimeState};
use multicore_core::service_protocol::{ControllerEndpoint, ServiceCommand, ServiceResponse};
use service::core_loop::{ClientIdentity, ConnectionId, CoreHealth, Cores, ServiceLoop};

#[derive(Default)]
struct FakeCores {
    calls: Vec<String>,
    running: bool,
    tun: bool,
    fail_tun: Option<BrokerErrorCode>,
    fail_restart: bool,
}

impl Cores for FakeCores {
    fn prepare(
        &mut self,
        _client: &ClientIdentity,
        generation: u64,
    ) -> Result<ControllerEndpoint, BrokerErrorCode> {
        self.calls.push(format!("prepare:{generation}"));
        self.running = true;
        self.tun = false;
        Ok(ControllerEndpoint {
            port: 40_000 + generation as u16,
            secret: format!("secret-{generation}"),
            mixed_port: 28_765,
        })
    }
    fn restart(&mut self) -> Result<(), BrokerErrorCode> {
        self.calls.push("restart".into());
        if self.fail_restart {
            return Err(BrokerErrorCode::NotReady);
        }
        self.running = true;
        self.tun = false;
        Ok(())
    }
    fn set_tun(&mut self, on: bool) -> Result<(), BrokerErrorCode> {
        self.calls.push(format!("tun:{on}"));
        if on && let Some(code) = self.fail_tun {
            return Err(code);
        }
        self.tun = on;
        Ok(())
    }
    fn health(&mut self) -> CoreHealth {
        CoreHealth {
            xray: self.running,
            mihomo: self.running,
        }
    }
    fn stop_all(&mut self) {
        self.calls.push("stop_all".into());
        self.running = false;
        self.tun = false;
    }
    fn diagnostics(&mut self) -> ServiceResponse {
        ServiceResponse::ProcessDiagnostics {
            xray: ElevatedRuntimeState::Ready,
            mihomo: ElevatedRuntimeState::Ready,
            tun: ElevatedRuntimeState::Stopped,
            logs: Vec::new(),
        }
    }
}

fn user(key: &str) -> ClientIdentity {
    ClientIdentity {
        user_key: key.to_owned(),
        data_root: PathBuf::from("/home/user/.local/share/multicore"),
        uid: 1000,
    }
}

const A: ConnectionId = ConnectionId(1);
const A2: ConnectionId = ConnectionId(2);
const B: ConnectionId = ConnectionId(3);

fn is_prepared(response: &ServiceResponse, generation: u64) -> bool {
    matches!(response, ServiceResponse::Prepared { generation_id, .. } if *generation_id == generation)
}

#[test]
fn prepare_then_tun_on_off_keeps_cores_running() {
    let mut service = ServiceLoop::new(FakeCores::default());
    let alice = user("1:S-1-5-21-alice");
    let response = service.execute(A, &alice, ServiceCommand::Prepare { generation_id: 7 });
    assert!(is_prepared(&response, 7));
    assert_eq!(
        service.execute(A, &alice, ServiceCommand::SetTun { on: true }),
        ServiceResponse::TunState { on: true }
    );
    assert_eq!(
        service.execute(A, &alice, ServiceCommand::SetTun { on: false }),
        ServiceResponse::TunState { on: false }
    );
    assert!(
        service.cores_mut().running,
        "disconnect leaves the cores running"
    );
    assert_eq!(
        service.cores_mut().calls,
        ["prepare:7", "tun:true", "tun:false"]
    );
}

#[test]
fn repeated_prepare_of_a_healthy_generation_is_idempotent() {
    let mut service = ServiceLoop::new(FakeCores::default());
    let alice = user("alice");
    service.execute(A, &alice, ServiceCommand::Prepare { generation_id: 3 });
    let again = service.execute(A, &alice, ServiceCommand::Prepare { generation_id: 3 });
    assert!(is_prepared(&again, 3));
    assert_eq!(service.cores_mut().calls, ["prepare:3"]);
}

#[test]
fn reload_while_the_tunnel_is_on_switches_it_on_again() {
    let mut service = ServiceLoop::new(FakeCores::default());
    let alice = user("alice");
    service.execute(A, &alice, ServiceCommand::Prepare { generation_id: 1 });
    service.execute(A, &alice, ServiceCommand::SetTun { on: true });
    let reload = service.execute(A, &alice, ServiceCommand::Prepare { generation_id: 2 });
    assert!(is_prepared(&reload, 2));
    assert_eq!(
        service.cores_mut().calls,
        ["prepare:1", "tun:true", "prepare:2", "tun:true"]
    );
    assert!(service.cores_mut().tun);
}

#[test]
fn tun_enable_failure_rolls_back_to_off() {
    let mut service = ServiceLoop::new(FakeCores {
        fail_tun: Some(BrokerErrorCode::TimedOut),
        ..FakeCores::default()
    });
    let alice = user("alice");
    service.execute(A, &alice, ServiceCommand::Prepare { generation_id: 1 });
    assert_eq!(
        service.execute(A, &alice, ServiceCommand::SetTun { on: true }),
        ServiceResponse::Error {
            code: BrokerErrorCode::TimedOut
        }
    );
    assert_eq!(
        service.cores_mut().calls,
        ["prepare:1", "tun:true", "tun:false"]
    );
    assert!(matches!(
        service.execute(A, &alice, ServiceCommand::Status),
        ServiceResponse::Status {
            tun_on: false,
            failure: None,
            ..
        }
    ));
}

#[test]
fn set_tun_before_prepare_is_not_ready() {
    let mut service = ServiceLoop::new(FakeCores::default());
    assert_eq!(
        service.execute(A, &user("alice"), ServiceCommand::SetTun { on: true }),
        ServiceResponse::Error {
            code: BrokerErrorCode::NotReady
        }
    );
}

#[test]
fn a_second_user_is_busy_until_the_owner_leaves() {
    let mut service = ServiceLoop::new(FakeCores::default());
    let (alice, bob) = (user("1:alice"), user("2:bob"));
    service.execute(A, &alice, ServiceCommand::Prepare { generation_id: 1 });
    for command in [
        ServiceCommand::Prepare { generation_id: 9 },
        ServiceCommand::SetTun { on: true },
        ServiceCommand::Status,
        ServiceCommand::Shutdown,
    ] {
        assert_eq!(service.execute(B, &bob, command), ServiceResponse::Busy);
    }
    service.client_gone(A);
    let response = service.execute(B, &bob, ServiceCommand::Prepare { generation_id: 9 });
    assert!(is_prepared(&response, 9));
}

#[test]
fn owner_channel_drop_stops_the_cores() {
    let mut service = ServiceLoop::new(FakeCores::default());
    let alice = user("alice");
    service.execute(A, &alice, ServiceCommand::Prepare { generation_id: 1 });
    service.execute(A, &alice, ServiceCommand::SetTun { on: true });
    service.client_gone(A);
    assert!(!service.cores_mut().running);
    assert_eq!(
        service.cores_mut().calls,
        ["prepare:1", "tun:true", "tun:false", "stop_all"]
    );
    assert_eq!(service.owner_key(), None);
}

#[test]
fn stale_connection_drop_does_not_stop_a_takeover() {
    let mut service = ServiceLoop::new(FakeCores::default());
    let alice = user("alice");
    service.execute(A, &alice, ServiceCommand::Prepare { generation_id: 1 });
    // The restarted daemon of the same user reconnects before the old pipe is torn down.
    let takeover = service.execute(A2, &alice, ServiceCommand::Prepare { generation_id: 2 });
    assert!(is_prepared(&takeover, 2));
    service.client_gone(A);
    assert!(service.cores_mut().running);
    service.client_gone(A2);
    assert!(!service.cores_mut().running);
}

#[test]
fn a_crash_is_restarted_once_then_reported_with_the_tunnel_off() {
    let mut service = ServiceLoop::new(FakeCores::default());
    let alice = user("alice");
    service.execute(A, &alice, ServiceCommand::Prepare { generation_id: 1 });
    service.execute(A, &alice, ServiceCommand::SetTun { on: true });
    service.cores_mut().running = false;
    service.tick();
    assert!(service.cores_mut().running);
    assert!(
        service.cores_mut().tun,
        "the tunnel comes back after the first restart"
    );
    service.cores_mut().running = false;
    service.tick();
    assert!(!service.cores_mut().tun);
    assert!(matches!(
        service.execute(A, &alice, ServiceCommand::Status),
        ServiceResponse::Status {
            generation_id: Some(1),
            tun_on: false,
            failure: Some(BrokerErrorCode::NotReady),
            ..
        }
    ));
    // A later Prepare of the same generation starts over instead of trusting the failure.
    let retry = service.execute(A, &alice, ServiceCommand::Prepare { generation_id: 1 });
    assert!(is_prepared(&retry, 1));
    assert_eq!(
        service
            .cores_mut()
            .calls
            .iter()
            .filter(|c| *c == "prepare:1")
            .count(),
        2
    );
}

#[test]
fn a_failed_crash_restart_is_reported_and_stops_the_cores() {
    let mut service = ServiceLoop::new(FakeCores {
        fail_restart: true,
        ..FakeCores::default()
    });
    let alice = user("alice");
    service.execute(A, &alice, ServiceCommand::Prepare { generation_id: 1 });
    service.cores_mut().running = false;
    service.tick();
    assert_eq!(
        service.cores_mut().calls,
        ["prepare:1", "restart", "stop_all"]
    );
    assert!(matches!(
        service.execute(A, &alice, ServiceCommand::SetTun { on: true }),
        ServiceResponse::Error {
            code: BrokerErrorCode::NotReady
        }
    ));
}

#[test]
fn shutdown_stops_and_releases() {
    let mut service = ServiceLoop::new(FakeCores::default());
    let (alice, bob) = (user("alice"), user("bob"));
    service.execute(A, &alice, ServiceCommand::Prepare { generation_id: 1 });
    assert_eq!(
        service.execute(A, &alice, ServiceCommand::Shutdown),
        ServiceResponse::ShuttingDown
    );
    assert!(!service.cores_mut().running);
    let response = service.execute(B, &bob, ServiceCommand::Prepare { generation_id: 4 });
    assert!(is_prepared(&response, 4));
}
