use std::{
    io::{Read, Write},
    net::TcpListener,
    sync::{Arc, Mutex},
    thread,
};

use multicore_core::{
    Engine, ProcessController, Snapshot, StartFailureCause, Supervisor,
    elevation_protocol::{BrokerErrorCode, ElevatedRuntimeState},
    service_protocol::{
        ControllerEndpoint, MAX_SERVICE_LOG_TAIL_BYTES, ServiceCommand, ServiceLogFile,
        ServiceResponse,
    },
};
use multicore_daemon::MihomoSelector;
use multicore_daemon::connection_mode::{ProxySwitch, SystemProxyController};
use multicore_daemon::service_client::{
    ControllerSlot, ServiceConnector, ServiceCoresController, ServiceError, ServiceMode,
    ServiceSession, ServiceTransport, ServiceTunController, SlotSelector, WarmInputs, WarmMiss,
    WarmStage, WarmStages, service_response_timeout, warm_key,
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
    /// `StageUpdate` answers with another version than it was asked for.
    stage_wrong_version: bool,
    /// The next `Prepare` waits here (outside the service lock) until released.
    gate: Option<std::sync::mpsc::Receiver<()>>,
    gate_entered: bool,
    /// The generation the fake runs (the last one prepared).
    generation: Option<u64>,
    /// `Troubleshoot` is answered like a service older than troubleshooting.
    troubleshoot_unknown: bool,
    /// The service's own log: `service.log`, `service.log.1`.
    logs: [String; 2],
    /// Bytes per `ServiceLog` chunk (64 when unset).
    log_chunk: usize,
    /// `ServiceLog` is answered like a service older than it.
    service_log_unknown: bool,
    /// `service.log` is rotated (replaced by a short file) once this many chunks were served.
    rotate_after: Option<usize>,
    log_requests: usize,
    /// The core journal's pages, served in order.
    journal: Vec<String>,
    /// `CoreJournal` is answered like a service older than it.
    journal_unknown: bool,
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
            ServiceCommand::Troubleshoot { .. } if service.troubleshoot_unknown => {
                ServiceResponse::Error {
                    code: BrokerErrorCode::InvalidRequest,
                }
            }
            ServiceCommand::CoreJournal { .. } if service.journal_unknown => {
                ServiceResponse::Error {
                    code: BrokerErrorCode::InvalidRequest,
                }
            }
            ServiceCommand::CoreJournal { cursor, .. } => {
                let page = cursor.map_or(0, |cursor| cursor.offset as usize);
                ServiceResponse::CoreJournal {
                    text: service.journal.get(page).cloned().unwrap_or_default(),
                    next: (page + 1 < service.journal.len()).then_some(
                        multicore_core::service_protocol::CoreJournalCursor {
                            segment: 1,
                            offset: page as u64 + 1,
                        },
                    ),
                }
            }
            ServiceCommand::ServiceLog { .. } if service.service_log_unknown => {
                ServiceResponse::Error {
                    code: BrokerErrorCode::InvalidRequest,
                }
            }
            ServiceCommand::ServiceLog { file, before } => {
                service.log_requests += 1;
                if service.rotate_after == Some(service.log_requests) {
                    service.logs[0] = "1 rotated\n".into();
                }
                let text = &service.logs[usize::from(*file == ServiceLogFile::Previous)];
                let chunk = if service.log_chunk == 0 {
                    64
                } else {
                    service.log_chunk
                };
                let end = before.map_or(text.len(), |before| (before as usize).min(text.len()));
                let mut start = end.saturating_sub(chunk);
                if start > 0
                    && let Some(newline) = text[start..end].find('\n')
                    && start + newline + 1 < end
                {
                    start += newline + 1;
                }
                ServiceResponse::ServiceLog {
                    file: *file,
                    start: start as u64,
                    end: end as u64,
                    text: text[start..end].to_owned(),
                }
            }
            ServiceCommand::Troubleshoot { fixes } => ServiceResponse::Troubleshot {
                outcomes: vec![multicore_core::troubleshoot::FixOutcome::Done; fixes.len()],
            },
            ServiceCommand::StageUpdate { version } => ServiceResponse::UpdateStaged {
                version: if service.stage_wrong_version {
                    "9.9.9".into()
                } else {
                    version.clone()
                },
            },
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
async fn connect_and_disconnect_after_a_warm_up_only_switch_the_tunnel() {
    let (session, connector, _) = session();
    // The warm-up stages the generation once.
    let warm = ServiceTunController::for_warm_up(session.clone(), 3);
    warm.start(Engine::Xray).await.unwrap();
    let controller = Arc::new(ServiceTunController::new(session.clone(), 3));
    let supervisor = Supervisor::new(controller);
    for _ in 0..2 {
        supervisor.connect().await.unwrap();
        supervisor.disconnect().await.unwrap();
        // The cores stay warm after the disconnect, so latency checks keep working.
        session.poll();
        assert!(session.cores_ready());
    }
    assert_eq!(
        connector.0.lock().unwrap().commands,
        [
            ServiceCommand::Prepare { generation_id: 3 },
            ServiceCommand::SetTun { on: true },
            ServiceCommand::SetTun { on: false },
            ServiceCommand::Status,
            ServiceCommand::SetTun { on: true },
            ServiceCommand::SetTun { on: false },
            ServiceCommand::Status,
        ],
        "a connect never prepares again and a disconnect never stops the cores"
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
async fn a_tun_switch_the_service_refused_is_a_tun_failure_not_a_missing_core() {
    // A live report: the service could not switch TUN on and the user was told to "check the
    // executables" of Mihomo, which were fine.
    let (session, connector, _) = session();
    connector.0.lock().unwrap().tun_rollback = true;
    let supervisor = Supervisor::new(Arc::new(ServiceTunController::new(session, 3)));
    let error = supervisor.connect().await.unwrap_err();
    assert!(
        matches!(
            error,
            multicore_core::SupervisorError::StartFailed {
                engine: Engine::Mihomo,
                cause: StartFailureCause::TunTimedOut,
                ..
            }
        ),
        "{error:?}"
    );
    for (code, cause) in [
        (BrokerErrorCode::NotReady, StartFailureCause::TunFailed),
        (BrokerErrorCode::TimedOut, StartFailureCause::TunTimedOut),
    ] {
        assert_eq!(ServiceError::TunRejected(code).start_failure_cause(), cause);
    }
    // A refusal outside the switch (Prepare) is still a core start failure.
    assert_eq!(
        ServiceError::Rejected(BrokerErrorCode::NotReady).start_failure_cause(),
        StartFailureCause::CoreFailed
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

/// Mimics the service's filtered controller proxy: one request per connection, `Connection:
/// close`, the real path forwarded, and a 403 for any name other than the known group.
fn fake_filtered_controller(group_path: &'static str, now: &'static str) -> std::net::SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    thread::spawn(move || {
        for stream in listener.incoming() {
            let mut stream = stream.unwrap();
            let mut buffer = [0_u8; 4096];
            let read = stream.read(&mut buffer).unwrap_or(0);
            let head = String::from_utf8_lossy(&buffer[..read]).into_owned();
            let target = head.split(' ').nth(1).unwrap_or_default().to_owned();
            let authorised = head.contains("Bearer proxy-token");
            let (status, body) = if authorised && target == group_path {
                ("200 OK", format!("{{\"now\":\"{now}\"}}"))
            } else {
                ("403 Forbidden", "Forbidden".to_owned())
            };
            let _ = write!(
                stream,
                "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
        }
    });
    address
}

#[tokio::test]
async fn exit_now_resolves_through_a_filtered_proxy_with_an_emoji_group() {
    let address = fake_filtered_controller(
        "/proxies/%E2%9A%A1%20%D0%90%D0%B2%D1%82%D0%BE%D0%B2%D1%8B%D0%B1%D0%BE%D1%80",
        "\u{1f1f8}\u{1f1ea} Sweden [se]",
    );
    let slot = Arc::new(ControllerSlot::default());
    slot.set(address, "proxy-token", None);
    let selector = SlotSelector::new(slot);
    // Several walks in a row: the proxy closes every connection, the client must not reuse one.
    for _ in 0..3 {
        assert_eq!(
            multicore_daemon::resolve_exit_now(&selector, "\u{26a1} Автовыбор")
                .await
                .as_deref(),
            Some("\u{1f1f8}\u{1f1ea} Sweden [se]"),
            "the leaf lookup is refused (403) yet the node is still the exit"
        );
    }
}

fn parsed(yaml: &str) -> Snapshot {
    Snapshot::parse(yaml.as_bytes(), multicore_core::IDLE_XRAY_CONFIG).unwrap()
}

const SNAPSHOT_A: &str = "proxies:
  - {name: a, type: socks5}
proxy-groups:
  - {name: g, type: select, proxies: [a]}
";
const SNAPSHOT_B: &str = "proxies:
  - {name: b, type: socks5}
proxy-groups:
  - {name: g, type: select, proxies: [b]}
";

#[test]
fn a_rejected_generation_is_not_reused_and_a_good_one_is() {
    let (session, connector, _) = session();
    let stages = WarmStages::default();
    let snapshot = parsed(SNAPSHOT_A);
    let inputs = WarmInputs::new(&snapshot, "Ethernet");
    assert_eq!(
        stages.reusable(&inputs, &session),
        Err(WarmMiss::NothingStaged)
    );
    stages.remember(
        WarmStage {
            key: inputs.key(),
            generation: 3,
            mixed_port: 28765,
        },
        inputs.clone(),
    );
    assert_eq!(
        stages
            .reusable(&inputs, &session)
            .map(|stage| stage.generation),
        Ok(3)
    );
    connector.0.lock().unwrap().reject_prepare = Some(BrokerErrorCode::NotReady);
    assert_eq!(
        session.prepare(3),
        Err(ServiceError::Rejected(BrokerErrorCode::NotReady))
    );
    assert!(session.prepare_failed(3));
    assert_eq!(
        stages.reusable(&inputs, &session),
        Err(WarmMiss::PrepareRejected),
        "re-stage with a fresh port"
    );
    assert_eq!(
        stages.reusable(&inputs, &session),
        Err(WarmMiss::NothingStaged),
        "the stale stage is gone"
    );
    connector.0.lock().unwrap().reject_prepare = None;
    session.prepare(4).unwrap();
    stages.remember(
        WarmStage {
            key: inputs.key(),
            generation: 4,
            mixed_port: 30001,
        },
        inputs.clone(),
    );
    assert!(!session.prepare_failed(3));
    assert_eq!(
        stages
            .reusable(&inputs, &session)
            .map(|stage| stage.mixed_port),
        Ok(30001)
    );
}

/// A miss names the staged input that differed, without any subscription content.
#[test]
fn a_warm_miss_names_which_input_differed() {
    let (session, _, _) = session();
    let stages = WarmStages::default();
    let staged = WarmInputs::new(&parsed(SNAPSHOT_A), "Ethernet");
    let remember = |generation| {
        stages.remember(
            WarmStage {
                key: staged.key(),
                generation,
                mixed_port: 1,
            },
            staged.clone(),
        );
    };
    remember(1);
    assert_eq!(
        stages.reusable(&WarmInputs::new(&parsed(SNAPSHOT_B), "Ethernet"), &session),
        Err(WarmMiss::SnapshotChanged)
    );
    remember(2);
    let miss = stages
        .reusable(&WarmInputs::new(&parsed(SNAPSHOT_A), "Wi-Fi"), &session)
        .unwrap_err();
    assert_eq!(
        miss,
        WarmMiss::InterfaceChanged {
            staged: "Ethernet".into(),
            now: "Wi-Fi".into()
        }
    );
    let text = miss.to_string();
    assert!(
        text.contains("Ethernet") && text.contains("Wi-Fi"),
        "{text}"
    );
    assert!(!text.contains("socks5"), "{text}");
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
fn a_prepare_that_switches_a_live_tunnel_back_on_has_room_for_it() {
    // The service re-prepares a generation whose tunnel was on by starting both cores and
    // switching TUN on again (with its rollback), all inside this one exchange.
    assert!(
        service_response_timeout(&ServiceCommand::Prepare { generation_id: 1 })
            >= multicore_core::CORE_READINESS_TIMEOUT * 2
                + multicore_core::service_protocol::SET_TUN_RESPONSE_TIMEOUT
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
fn staging_runs_on_its_own_channel_and_leaves_the_connection_alone() {
    let (session, connector, _) = session();
    session.prepare(3).unwrap();
    session.set_tun(true).unwrap();
    assert_eq!(session.stage_update("1.2.0"), Ok(()));
    let service = connector.0.lock().unwrap();
    assert_eq!(service.connects, 2, "staging opened its own channel");
    assert!(service.commands.contains(&ServiceCommand::StageUpdate {
        version: "1.2.0".into()
    }));
    assert!(
        !service
            .commands
            .iter()
            .any(|command| matches!(command, ServiceCommand::Update { .. })),
        "staging never asks the service to install"
    );
    assert!(
        service.tun,
        "the tunnel stays up while the release downloads"
    );
}

#[test]
fn a_refused_or_mismatched_stage_is_reported() {
    let (session, connector, _) = session();
    connector.0.lock().unwrap().busy = true;
    assert_eq!(session.stage_update("1.2.0"), Err(ServiceError::Busy));
    connector.0.lock().unwrap().busy = false;
    connector.0.lock().unwrap().stage_wrong_version = true;
    assert_eq!(session.stage_update("1.2.0"), Err(ServiceError::Protocol));
    assert!(
        service_response_timeout(&ServiceCommand::StageUpdate {
            version: "1.2.0".into()
        }) >= std::time::Duration::from_secs(10 * 60),
        "a download gets the update budget"
    );
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

#[test]
fn troubleshooting_runs_on_its_own_channel_and_an_old_service_is_told_apart() {
    use multicore_core::troubleshoot::{FixOutcome, PrivilegedFix};
    let (session, connector, _slot) = session();
    session.prepare(3).unwrap();
    let fixes = [PrivilegedFix::ResyncTime, PrivilegedFix::ReinstallWintun];
    assert_eq!(
        session.troubleshoot(&fixes),
        Ok(vec![FixOutcome::Done, FixOutcome::Done])
    );
    {
        let service = connector.0.lock().unwrap();
        assert_eq!(
            service.connects, 2,
            "troubleshooting opened its own channel"
        );
        assert!(service.commands.contains(&ServiceCommand::Troubleshoot {
            fixes: fixes.to_vec()
        }));
    }
    assert!(
        session.cores_ready(),
        "the connection channel was left alone"
    );
    connector.0.lock().unwrap().troubleshoot_unknown = true;
    assert_eq!(
        session.troubleshoot(&fixes),
        Err(ServiceError::Rejected(BrokerErrorCode::InvalidRequest))
    );
    assert!(
        service_response_timeout(&ServiceCommand::Troubleshoot {
            fixes: fixes.to_vec()
        }) >= std::time::Duration::from_secs(5 * 60)
    );
}

fn numbered_lines(prefix: &str, count: usize, width: usize) -> String {
    (0..count)
        .map(|index| format!("{prefix} {index:06} {}\n", "x".repeat(width)))
        .collect()
}

#[test]
fn the_service_log_is_paged_on_its_own_channel_and_claims_nothing() {
    use multicore_daemon::{ServiceLogSource, ServiceLogStatus};
    let (session, connector, _slot) = session();
    session.prepare(3).unwrap();
    let current = numbered_lines("current", 40, 10);
    let previous = numbered_lines("previous", 25, 10);
    connector.0.lock().unwrap().logs = [current.clone(), previous.clone()];
    let files = session.service_log().unwrap();
    assert_eq!(files.len(), 2);
    assert_eq!(files[0].file, ServiceLogFile::Current);
    assert_eq!(files[0].text, current);
    assert!(!files[0].truncated);
    assert_eq!(files[1].file, ServiceLogFile::Previous);
    assert_eq!(files[1].text, previous);
    assert!(!files[1].truncated);
    {
        let service = connector.0.lock().unwrap();
        assert_eq!(
            service.connects, 2,
            "the log was read on a channel of its own"
        );
        assert!(service.log_requests > 10, "paged in chunks");
    }
    assert!(
        session.cores_ready(),
        "the connection channel was left alone"
    );
    let dto = ServiceLogSource::service_log(session.as_ref());
    assert_eq!(dto.status, ServiceLogStatus::Included);
    let names = dto
        .files
        .iter()
        .map(|file| file.name.as_str())
        .collect::<Vec<_>>();
    assert_eq!(names, ["service.log", "service.log.1"]);

    // Only `service.log` exists: the missing predecessor is left out.
    connector.0.lock().unwrap().logs[1].clear();
    let dto = ServiceLogSource::service_log(session.as_ref());
    assert_eq!(dto.files.len(), 1);
    assert_eq!(dto.files[0].name, "service.log");
}

#[test]
fn the_service_log_keeps_its_newest_lines_within_the_size_cap() {
    let (session, connector, _slot) = session();
    let current = numbered_lines("current", 6000, 100);
    let previous = numbered_lines("previous", 1000, 100);
    assert!(current.len() as u64 > MAX_SERVICE_LOG_TAIL_BYTES);
    {
        let mut service = connector.0.lock().unwrap();
        service.logs = [current.clone(), previous];
        service.log_chunk = 16 * 1024;
    }
    let files = session.service_log().unwrap();
    let total = files.iter().map(|file| file.text.len() as u64).sum::<u64>();
    assert!(total <= MAX_SERVICE_LOG_TAIL_BYTES, "{total}");
    assert!(total > MAX_SERVICE_LOG_TAIL_BYTES - 1024, "{total}");
    assert!(files[0].truncated);
    assert!(
        files[0]
            .text
            .ends_with(&format!("current {:06} {}\n", 5999, "x".repeat(100)))
    );
    assert!(
        files[0].text.starts_with("current "),
        "starts at a whole line"
    );
    assert!(
        !files[0].text.contains("current 000000 "),
        "the oldest lines are left out"
    );
    assert!(files[1].text.is_empty() && files[1].truncated);
}

#[test]
fn a_log_rotated_while_it_is_paged_stops_at_the_rotation() {
    let (session, connector, _slot) = session();
    {
        let mut service = connector.0.lock().unwrap();
        service.logs = [numbered_lines("current", 40, 10), String::new()];
        service.rotate_after = Some(2);
    }
    let files = session.service_log().unwrap();
    assert!(files[0].truncated);
    assert!(
        files[0]
            .text
            .ends_with(&format!("current {:06} {}\n", 39, "x".repeat(10)))
    );
    assert!(!files[0].text.contains("rotated"));
}

#[test]
fn the_core_journal_is_paged_on_its_own_channel_and_claims_nothing() {
    use multicore_daemon::{ServiceLogSource, ServiceLogStatus};
    let (session, connector, _slot) = session();
    session.prepare(3).unwrap();
    let pages = (0..5)
        .map(|page| format!("{page}000\tmihomo\tstdout\tpage {page}\n"))
        .collect::<Vec<_>>();
    connector.0.lock().unwrap().journal = pages.clone();
    let (text, truncated) = session.core_journal(1_791_000_000_000).unwrap();
    assert_eq!(text, pages.concat());
    assert!(!truncated);
    {
        let service = connector.0.lock().unwrap();
        assert_eq!(service.connects, 2, "read on a channel of its own");
        let asked = service
            .commands
            .iter()
            .filter_map(|command| match command {
                ServiceCommand::CoreJournal { since_ms, .. } => Some(*since_ms),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(asked, [1_791_000_000_000; 5]);
    }
    assert!(
        session.cores_ready(),
        "the connection channel was left alone"
    );
    let dto = ServiceLogSource::core_journal(session.as_ref(), 0);
    assert_eq!(dto.status, ServiceLogStatus::Included);
    assert_eq!(dto.text, pages.concat());

    connector.0.lock().unwrap().journal_unknown = true;
    assert_eq!(
        ServiceLogSource::core_journal(session.as_ref(), 0).status,
        ServiceLogStatus::TooOld
    );
    {
        let mut service = connector.0.lock().unwrap();
        service.journal_unknown = false;
        service.busy = true;
    }
    assert_eq!(
        ServiceLogSource::core_journal(session.as_ref(), 0).status,
        ServiceLogStatus::Busy
    );
    {
        let mut service = connector.0.lock().unwrap();
        service.busy = false;
        service.connect_errors = vec![ServiceError::NotInstalled];
    }
    assert_eq!(
        ServiceLogSource::core_journal(session.as_ref(), 0).status,
        ServiceLogStatus::NoService
    );
}

#[test]
fn an_old_busy_or_missing_service_is_told_apart_for_the_service_log() {
    use multicore_daemon::{ServiceLogSource, ServiceLogStatus};
    let (session, connector, _slot) = session();
    connector.0.lock().unwrap().service_log_unknown = true;
    assert_eq!(
        session.service_log(),
        Err(ServiceError::Rejected(BrokerErrorCode::InvalidRequest))
    );
    let dto = ServiceLogSource::service_log(session.as_ref());
    assert_eq!(dto.status, ServiceLogStatus::TooOld);
    assert!(dto.files.is_empty());
    {
        let mut service = connector.0.lock().unwrap();
        service.service_log_unknown = false;
        service.busy = true;
    }
    assert_eq!(
        ServiceLogSource::service_log(session.as_ref()).status,
        ServiceLogStatus::Busy
    );
    {
        let mut service = connector.0.lock().unwrap();
        service.busy = false;
        service.connect_errors = vec![ServiceError::NotInstalled, ServiceError::Unavailable];
    }
    assert_eq!(
        ServiceLogSource::service_log(session.as_ref()).status,
        ServiceLogStatus::NoService
    );
    assert_eq!(
        ServiceLogSource::service_log(session.as_ref()).status,
        ServiceLogStatus::Unavailable
    );
    assert!(
        service_response_timeout(&ServiceCommand::ServiceLog {
            file: ServiceLogFile::Current,
            before: None,
        }) >= std::time::Duration::from_secs(15)
    );
}
