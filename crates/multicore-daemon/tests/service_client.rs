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
use multicore_daemon::connection_mode::{ProxySwitch, SystemProxyController};
use multicore_daemon::service_client::{
    ControllerSlot, ServiceConnector, ServiceCoresController, ServiceError, ServiceMode,
    ServiceSession, ServiceTransport, ServiceTunController, SlotSelector, WarmStage, WarmStages,
    service_response_timeout, warm_key,
};

#[derive(Default)]
struct FakeService {
    commands: Vec<ServiceCommand>,
    connects: usize,
    busy: bool,
    status_failure: Option<BrokerErrorCode>,
    break_next: bool,
    tun_form: bool,
    /// Errors the next connects fail with, in order.
    connect_errors: Vec<ServiceError>,
    reject_prepare: Option<BrokerErrorCode>,
    /// `SetTun{on: true}` is rolled back: the adapter never came up.
    tun_rollback: bool,
    tun: bool,
    /// `Update` waits for this before answering: a download that takes a while.
    update_gate: Option<std::sync::mpsc::Receiver<()>>,
    /// The next `Prepare` waits here (outside the service lock) until released.
    gate: Option<std::sync::mpsc::Receiver<()>>,
    gate_entered: bool,
    /// The generation the fake runs (the last one prepared).
    generation: Option<u64>,
}

#[derive(Clone, Default)]
struct FakeConnector(Arc<Mutex<FakeService>>);
struct FakeTransport(Arc<Mutex<FakeService>>);

impl ServiceConnector for FakeConnector {
    type Transport = FakeTransport;
    fn connect(&self) -> Result<FakeTransport, ServiceError> {
        let mut service = self.0.lock().unwrap();
        service.connects += 1;
        if !service.connect_errors.is_empty() {
            return Err(service.connect_errors.remove(0));
        }
        Ok(FakeTransport(self.0.clone()))
    }
}

impl ServiceTransport for FakeTransport {
    fn request(&self, command: &ServiceCommand) -> Result<ServiceResponse, ServiceError> {
        let gate = if matches!(command, ServiceCommand::Prepare { .. }) {
            self.0.lock().unwrap().gate.take()
        } else {
            None
        };
        if let Some(gate) = gate {
            self.0.lock().unwrap().gate_entered = true;
            let _ = gate.recv();
        }
        let mut service = self.0.lock().unwrap();
        if std::mem::take(&mut service.break_next) {
            return Err(ServiceError::Disconnected);
        }
        service.commands.push(command.clone());
        if service.busy {
            return Ok(ServiceResponse::Busy);
        }
        if let ServiceCommand::Update { .. } = command
            && let Some(gate) = service.update_gate.take()
        {
            drop(service);
            let _ = gate.recv();
            return Ok(ServiceResponse::UpdateStarted);
        }
        if let (ServiceCommand::Prepare { .. }, Some(code)) = (command, service.reject_prepare) {
            return Ok(ServiceResponse::Error { code });
        }
        Ok(match command {
            ServiceCommand::Prepare { generation_id } => {
                service.generation = Some(*generation_id);
                ServiceResponse::Prepared {
                    generation_id: *generation_id,
                    controller: ControllerEndpoint {
                        port: 40123,
                        secret: "svc".into(),
                        mixed_port: if service.tun_form { 0 } else { 28765 },
                    },
                }
            }
            ServiceCommand::SetTun { on } => {
                service.tun = *on && !service.tun_rollback;
                ServiceResponse::TunState { on: service.tun }
            }
            ServiceCommand::Status => ServiceResponse::Status {
                generation_id: service.generation,
                xray: ElevatedRuntimeState::Ready,
                mihomo: ElevatedRuntimeState::Ready,
                tun_on: service.tun,
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

#[test]
fn a_rejected_generation_is_not_reused_and_a_good_one_is() {
    let (session, connector, _) = session();
    let stages = WarmStages::default();
    let key = [7_u8; 32];
    stages.remember(WarmStage {
        key,
        generation: 3,
        mixed_port: 28765,
    });
    assert_eq!(
        stages
            .reusable(&key, &session)
            .map(|stage| stage.generation),
        Some(3)
    );
    connector.0.lock().unwrap().reject_prepare = Some(BrokerErrorCode::NotReady);
    assert_eq!(
        session.prepare(3),
        Err(ServiceError::Rejected(BrokerErrorCode::NotReady))
    );
    assert!(session.prepare_failed(3));
    assert_eq!(
        stages.reusable(&key, &session),
        None,
        "re-stage with a fresh port"
    );
    assert_eq!(
        stages.reusable(&key, &session),
        None,
        "the stale stage is gone"
    );
    connector.0.lock().unwrap().reject_prepare = None;
    session.prepare(4).unwrap();
    stages.remember(WarmStage {
        key,
        generation: 4,
        mixed_port: 30001,
    });
    assert!(!session.prepare_failed(3));
    assert_eq!(
        stages
            .reusable(&key, &session)
            .map(|stage| stage.mixed_port),
        Some(30001)
    );
    assert_eq!(
        stages.reusable(&[8_u8; 32], &session),
        None,
        "another snapshot"
    );
}

#[test]
fn availability_is_rechecked_on_every_probe() {
    let (session, connector, _) = session();
    connector.0.lock().unwrap().connect_errors =
        vec![ServiceError::Unavailable, ServiceError::NotInstalled];
    assert_eq!(session.probe_mode(), ServiceMode::Unavailable);
    assert!(!session.service_available());
    assert_eq!(session.probe_mode(), ServiceMode::Absent);
    assert!(!session.service_available());
    // The service started after the app: the next probe picks it up.
    assert_eq!(session.probe_mode(), ServiceMode::Service);
    assert!(session.service_available());
    // A channel that died meanwhile is not mistaken for a missing service.
    connector.0.lock().unwrap().break_next = true;
    assert_eq!(session.probe_mode(), ServiceMode::Service);
    connector.0.lock().unwrap().busy = true;
    assert_eq!(
        session.probe_mode(),
        ServiceMode::Service,
        "Busy is reported by Prepare"
    );
}

#[test]
fn a_tunnel_the_service_rolled_back_is_an_adapter_failure() {
    let (session, connector, _) = session();
    session.prepare(3).unwrap();
    connector.0.lock().unwrap().tun_rollback = true;
    let error = session.set_tun(true).unwrap_err();
    assert_eq!(error, ServiceError::Rejected(BrokerErrorCode::TimedOut));
    assert_eq!(
        error.start_failure_cause(),
        StartFailureCause::ReadinessTimedOut
    );
}

#[test]
fn read_only_commands_get_a_generous_timeout() {
    assert!(
        service_response_timeout(&ServiceCommand::Status) >= std::time::Duration::from_secs(15)
    );
    assert!(
        service_response_timeout(&ServiceCommand::Diagnostics)
            >= std::time::Duration::from_secs(15)
    );
}

#[test]
fn an_update_runs_on_its_own_channel_and_keeps_the_cores() {
    let (session, connector, _) = session();
    session.prepare(3).unwrap();
    let (release, gate) = std::sync::mpsc::channel();
    connector.0.lock().unwrap().update_gate = Some(gate);
    let updating = {
        let session = session.clone();
        thread::spawn(move || session.update("1.2.0"))
    };
    while !connector
        .0
        .lock()
        .unwrap()
        .commands
        .contains(&ServiceCommand::Update {
            version: "1.2.0".into(),
        })
    {
        thread::yield_now();
    }
    // While the service downloads, the connection's own channel stays usable: the watcher
    // keeps polling (the service answers outside its update) and a disconnect goes through.
    session.poll();
    assert!(session.cores_ready());
    session.set_tun(false).unwrap();
    release.send(()).unwrap();
    updating.join().unwrap().unwrap();
    let service = connector.0.lock().unwrap();
    assert_eq!(service.connects, 2, "the update opened its own channel");
    assert!(service.commands.contains(&ServiceCommand::Status));
}

#[test]
fn a_refused_update_is_reported() {
    let (session, connector, _) = session();
    connector.0.lock().unwrap().busy = true;
    assert_eq!(session.update("1.2.0"), Err(ServiceError::Busy));
    connector.0.lock().unwrap().busy = false;
    connector.0.lock().unwrap().connect_errors = vec![ServiceError::NotInstalled];
    assert_eq!(session.update("1.2.0"), Err(ServiceError::NotInstalled));
}

#[tokio::test]
async fn a_lost_tunnel_is_detected_while_connected() {
    let (session, connector, _) = session();
    let supervisor = Supervisor::new(Arc::new(ServiceTunController::new(session.clone(), 3)));
    supervisor.connect().await.unwrap();
    session.poll();
    assert!(!session.connection_lost());
    // The service switched the tunnel off (for example after a core crash).
    connector.0.lock().unwrap().tun = false;
    session.poll();
    assert!(session.connection_lost());
    // Tearing the connection down still succeeds and clears the flag.
    connector.0.lock().unwrap().status_failure = Some(BrokerErrorCode::NotReady);
    session.poll();
    supervisor.disconnect().await.unwrap();
    assert!(!session.connection_lost());
}

#[derive(Default)]
struct FakeProxy {
    applied: std::sync::atomic::AtomicUsize,
    restored: std::sync::atomic::AtomicUsize,
}

impl ProxySwitch for FakeProxy {
    fn apply(&self) -> std::io::Result<()> {
        self.applied
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }

    fn restore(&self) -> std::io::Result<()> {
        self.restored
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }
}

#[tokio::test]
async fn proxy_mode_restores_the_system_proxy_when_the_service_channel_drops() {
    let (session, connector, _) = session();
    let proxy = Arc::new(FakeProxy::default());
    let supervisor = Supervisor::new(Arc::new(SystemProxyController::new(
        ServiceCoresController::new(session.clone(), 3),
        proxy.clone(),
    )));
    supervisor.connect().await.unwrap();
    assert_eq!(proxy.applied.load(std::sync::atomic::Ordering::SeqCst), 1);
    session.poll();
    assert!(!session.connection_lost());
    connector.0.lock().unwrap().break_next = true;
    session.poll();
    assert!(
        session.connection_lost(),
        "the mixed port is gone with the cores"
    );
    supervisor.disconnect().await.unwrap();
    assert_eq!(proxy.restored.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert!(!session.connection_lost());
}

fn proxy_connection(
    session: &Arc<ServiceSession<FakeConnector>>,
    generation: u64,
    proxy: Arc<FakeProxy>,
) -> Supervisor<SystemProxyController<ServiceCoresController<FakeConnector>>> {
    Supervisor::new(Arc::new(SystemProxyController::new(
        ServiceCoresController::new(session.clone(), generation),
        proxy,
    )))
}

#[tokio::test]
async fn a_warm_up_never_prepares_over_a_connection() {
    let (session, connector, _) = session();
    let proxy = Arc::new(FakeProxy::default());
    let supervisor = proxy_connection(&session, 2, proxy.clone());
    supervisor.connect().await.unwrap();
    // A warm-up that lost the race for a superseded generation arrives now.
    assert_eq!(session.prepare_idle(1), Err(ServiceError::InUse));
    assert!(
        !connector
            .0
            .lock()
            .unwrap()
            .commands
            .contains(&ServiceCommand::Prepare { generation_id: 1 }),
        "the service never sees the stale Prepare"
    );
    session.poll();
    assert!(!session.connection_lost());
    // The connection's own generation stays fine to warm.
    assert_eq!(session.prepare_idle(2), Ok(()));
    supervisor.disconnect().await.unwrap();
    assert_eq!(
        session.prepare_idle(1),
        Ok(()),
        "free again after the disconnect"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_connect_waits_for_a_warm_up_prepare_in_flight_and_wins() {
    let (session, connector, _) = session();
    let (release, gate) = std::sync::mpsc::channel();
    connector.0.lock().unwrap().gate = Some(gate);
    let warming = {
        let session = session.clone();
        std::thread::spawn(move || session.prepare_idle(1))
    };
    while !connector.0.lock().unwrap().gate_entered {
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
    }
    let proxy = Arc::new(FakeProxy::default());
    let connecting = {
        let supervisor = proxy_connection(&session, 2, proxy.clone());
        tokio::spawn(async move {
            let result = supervisor.connect().await;
            (supervisor, result)
        })
    };
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert!(
        !connecting.is_finished(),
        "the Connect waits for the channel"
    );
    release.send(()).unwrap();
    assert_eq!(warming.join().unwrap(), Ok(()));
    let (supervisor, result) = connecting.await.unwrap();
    result.unwrap();
    assert_eq!(
        connector.0.lock().unwrap().commands,
        [
            ServiceCommand::Prepare { generation_id: 1 },
            ServiceCommand::Prepare { generation_id: 2 },
            ServiceCommand::SetTun { on: false },
        ]
    );
    session.poll();
    assert!(
        !session.connection_lost(),
        "the connection's generation runs"
    );
    supervisor.disconnect().await.unwrap();
}

#[tokio::test]
async fn a_generation_replaced_under_a_proxy_connection_is_a_lost_connection() {
    let (session, _, _) = session();
    let proxy = Arc::new(FakeProxy::default());
    let supervisor = proxy_connection(&session, 2, proxy.clone());
    supervisor.connect().await.unwrap();
    // Something else prepared another generation: the mixed port the system proxy points at
    // is gone.
    session.prepare(1).unwrap();
    session.poll();
    assert!(session.connection_lost());
    supervisor.disconnect().await.unwrap();
    assert_eq!(proxy.restored.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert!(!session.connection_lost());
}
