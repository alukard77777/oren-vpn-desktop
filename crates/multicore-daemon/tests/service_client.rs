use std::{
    io::{Read, Write},
    net::TcpListener,
    sync::{Arc, Mutex},
    thread,
};

use multicore_core::{
    Engine, ProcessController, Snapshot, StartFailureCause, Supervisor,
    elevation_protocol::{BrokerErrorCode, ElevatedRuntimeState},
    service_protocol::{ControllerEndpoint, ServiceCommand, ServiceResponse},
};
use multicore_daemon::MihomoSelector;
use multicore_daemon::service_client::{
    ControllerSlot, ServiceConnector, ServiceCoresController, ServiceError, ServiceSession,
    ServiceTransport, ServiceTunController, SlotSelector, warm_key,
};

#[derive(Default)]
struct FakeService {
    commands: Vec<ServiceCommand>,
    connects: usize,
    busy: bool,
    status_failure: Option<BrokerErrorCode>,
    break_next: bool,
    tun_form: bool,
}

#[derive(Clone, Default)]
struct FakeConnector(Arc<Mutex<FakeService>>);
struct FakeTransport(Arc<Mutex<FakeService>>);

impl ServiceConnector for FakeConnector {
    type Transport = FakeTransport;
    fn connect(&self) -> Result<FakeTransport, ServiceError> {
        self.0.lock().unwrap().connects += 1;
        Ok(FakeTransport(self.0.clone()))
    }
}

impl ServiceTransport for FakeTransport {
    fn request(&self, command: &ServiceCommand) -> Result<ServiceResponse, ServiceError> {
        let mut service = self.0.lock().unwrap();
        if std::mem::take(&mut service.break_next) {
            return Err(ServiceError::Disconnected);
        }
        service.commands.push(command.clone());
        if service.busy {
            return Ok(ServiceResponse::Busy);
        }
        Ok(match command {
            ServiceCommand::Prepare { generation_id } => ServiceResponse::Prepared {
                generation_id: *generation_id,
                controller: ControllerEndpoint {
                    port: 40123,
                    secret: "svc".into(),
                    mixed_port: if service.tun_form { 0 } else { 28765 },
                },
            },
            ServiceCommand::SetTun { on } => ServiceResponse::TunState { on: *on },
            ServiceCommand::Status => ServiceResponse::Status {
                generation_id: Some(3),
                xray: ElevatedRuntimeState::Ready,
                mihomo: ElevatedRuntimeState::Ready,
                tun_on: false,
                failure: service.status_failure,
            },
            ServiceCommand::Stop { engine } => ServiceResponse::Stopped { engine: *engine },
            ServiceCommand::Diagnostics => ServiceResponse::ProcessDiagnostics {
                xray: ElevatedRuntimeState::Ready,
                mihomo: ElevatedRuntimeState::Ready,
                tun: ElevatedRuntimeState::Stopped,
                logs: Vec::new(),
            },
            ServiceCommand::Shutdown => ServiceResponse::ShuttingDown,
            ServiceCommand::Update { .. } => ServiceResponse::UpdateStarted,
        })
    }
}

fn session() -> (
    Arc<ServiceSession<FakeConnector>>,
    FakeConnector,
    Arc<ControllerSlot>,
) {
    let connector = FakeConnector::default();
    let slot = Arc::new(ControllerSlot::default());
    (
        Arc::new(ServiceSession::new(connector.clone(), slot.clone())),
        connector,
        slot,
    )
}

#[test]
fn prepare_publishes_the_endpoint_and_is_idempotent() {
    let (session, connector, slot) = session();
    session.prepare(3).unwrap();
    session.prepare(3).unwrap();
    let target = slot.get().unwrap();
    assert_eq!(target.address, "127.0.0.1:40123".parse().unwrap());
    assert_eq!(target.secret(), "svc");
    assert_eq!(target.mixed_port, Some(28765));
    assert!(session.cores_ready());
    let service = connector.0.lock().unwrap();
    assert_eq!(
        service.commands,
        [ServiceCommand::Prepare { generation_id: 3 }]
    );
    assert_eq!(service.connects, 1);
    assert!(!format!("{target:?}").contains("svc"));
}

#[test]
fn a_tun_form_generation_publishes_no_mixed_port() {
    let (session, connector, slot) = session();
    connector.0.lock().unwrap().tun_form = true;
    session.prepare(3).unwrap();
    assert_eq!(slot.get().unwrap().mixed_port, None);
}

#[tokio::test]
async fn the_tun_controller_maps_connect_and_disconnect_to_the_service() {
    let (session, connector, _) = session();
    let supervisor = Supervisor::new(Arc::new(ServiceTunController::new(session.clone(), 3)));
    supervisor.connect().await.unwrap();
    supervisor.disconnect().await.unwrap();
    assert_eq!(
        connector.0.lock().unwrap().commands,
        [
            ServiceCommand::Prepare { generation_id: 3 },
            ServiceCommand::SetTun { on: true },
            ServiceCommand::SetTun { on: false },
        ],
        "disconnect switches the tunnel off and leaves the cores running"
    );
}

#[tokio::test]
async fn the_proxy_mode_controller_keeps_tun_off_and_never_stops_cores() {
    let (session, connector, _) = session();
    let controller = ServiceCoresController::new(session.clone(), 3);
    controller.start(Engine::Xray).await.unwrap();
    controller.start(Engine::Mihomo).await.unwrap();
    controller.stop(Engine::Mihomo).await.unwrap();
    controller.stop(Engine::Xray).await.unwrap();
    assert_eq!(
        connector.0.lock().unwrap().commands,
        [
            ServiceCommand::Prepare { generation_id: 3 },
            ServiceCommand::SetTun { on: false },
        ]
    );
}

#[tokio::test]
async fn busy_is_reported_as_another_user() {
    let (session, connector, _) = session();
    connector.0.lock().unwrap().busy = true;
    let supervisor = Supervisor::new(Arc::new(ServiceTunController::new(session, 3)));
    let error = supervisor.connect().await.unwrap_err();
    assert!(matches!(
        error,
        multicore_core::SupervisorError::StartFailed {
            engine: Engine::Xray,
            cause: StartFailureCause::ServiceBusy,
            ..
        }
    ));
}

#[test]
fn a_lost_channel_forgets_the_generation_and_prepares_again() {
    let (session, connector, _) = session();
    session.prepare(3).unwrap();
    connector.0.lock().unwrap().break_next = true;
    assert_eq!(session.set_tun(true), Err(ServiceError::Disconnected));
    assert!(!session.cores_ready());
    session.prepare(3).unwrap();
    let service = connector.0.lock().unwrap();
    assert_eq!(service.connects, 2);
    assert_eq!(
        service
            .commands
            .iter()
            .filter(|command| matches!(command, ServiceCommand::Prepare { .. }))
            .count(),
        2
    );
}

#[test]
fn a_reported_core_failure_clears_readiness() {
    let (session, connector, _) = session();
    session.prepare(3).unwrap();
    session.poll();
    assert!(session.cores_ready());
    connector.0.lock().unwrap().status_failure = Some(BrokerErrorCode::NotReady);
    session.poll();
    assert!(!session.cores_ready());
}

#[test]
fn warm_key_changes_when_the_outbound_interface_changes() {
    let snapshot = Snapshot::parse(
        b"proxies:\n  - {name: a, type: socks5}\nproxy-groups:\n  - {name: g, type: select, proxies: [a]}\n",
        multicore_core::IDLE_XRAY_CONFIG,
    )
    .unwrap();
    assert_eq!(
        warm_key(&snapshot, "Ethernet"),
        warm_key(&snapshot, "Ethernet")
    );
    assert_ne!(
        warm_key(&snapshot, "Ethernet"),
        warm_key(&snapshot, "Wi-Fi")
    );
}

fn fake_proxy_now(now: &'static str) -> std::net::SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    thread::spawn(move || {
        for stream in listener.incoming() {
            let mut stream = stream.unwrap();
            let mut buffer = [0_u8; 4096];
            let _ = stream.read(&mut buffer);
            let body = format!("{{\"now\":\"{now}\"}}");
            let _ = write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                body.len()
            );
        }
    });
    address
}

#[tokio::test]
async fn the_slot_selector_follows_the_current_endpoint() {
    let slot = Arc::new(ControllerSlot::default());
    let selector = SlotSelector::new(slot.clone());
    assert!(selector.proxy_now("Auto").await.is_err(), "no endpoint yet");
    slot.set(fake_proxy_now("node-a"), "secret-a", None);
    assert_eq!(
        selector.proxy_now("Auto").await.unwrap().as_deref(),
        Some("node-a")
    );
    slot.set(fake_proxy_now("node-b"), "secret-b", Some(1));
    assert_eq!(
        selector.proxy_now("Auto").await.unwrap().as_deref(),
        Some("node-b")
    );
}
