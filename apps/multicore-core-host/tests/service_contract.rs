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
    fail_tun_off: bool,
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
        if !on && self.fail_tun_off {
            return Err(BrokerErrorCode::NotReady);
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

/// Review M5: a failed switch-off is not recorded as off, so the next request retries it.
#[test]
fn a_failed_tun_switch_off_is_retried() {
    let mut service = ServiceLoop::new(FakeCores::default());
    let alice = user("alice");
    service.execute(A, &alice, ServiceCommand::Prepare { generation_id: 1 });
    service.execute(A, &alice, ServiceCommand::SetTun { on: true });
    service.cores_mut().fail_tun_off = true;
    assert_eq!(
        service.execute(A, &alice, ServiceCommand::SetTun { on: false }),
        ServiceResponse::Error {
            code: BrokerErrorCode::NotReady
        }
    );
    assert!(matches!(
        service.execute(A, &alice, ServiceCommand::Status),
        ServiceResponse::Status { tun_on: true, .. }
    ));
    service.cores_mut().fail_tun_off = false;
    assert_eq!(
        service.execute(A, &alice, ServiceCommand::SetTun { on: false }),
        ServiceResponse::TunState { on: false }
    );
    assert_eq!(
        service.cores_mut().calls,
        ["prepare:1", "tun:true", "tun:false", "tun:false"]
    );
}

/// Review I5: provider caches, cache.db and geodata live in a per-account Mihomo home.
#[test]
fn each_account_gets_its_own_mihomo_home() {
    use service::cores::client_home;
    let root = std::path::Path::new("/var/lib/multicore");
    let (alice, bob) = (
        ClientIdentity {
            user_key: "1:S-1-5-21-1-2-3-1001".into(),
            data_root: PathBuf::from("/a"),
            uid: 1001,
        },
        ClientIdentity {
            user_key: "2:S-1-5-21-1-2-3-1002".into(),
            data_root: PathBuf::from("/b"),
            uid: 1002,
        },
    );
    let alice_home = client_home(root, &alice);
    assert!(alice_home.starts_with(root.join("users")));
    assert_ne!(alice_home, client_home(root, &bob));
    // The same account in a later logon session keeps its caches.
    let alice_again = ClientIdentity {
        user_key: "7:S-1-5-21-1-2-3-1001".into(),
        ..alice.clone()
    };
    assert_eq!(alice_home, client_home(root, &alice_again));
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

mod rest {
    use super::service::mihomo_rest;
    use std::{
        io::{Read, Write},
        net::TcpListener,
        sync::mpsc,
        thread,
    };

    /// Serves one canned response per connection and reports each raw request.
    fn fake_mihomo(responses: Vec<&'static str>) -> (u16, mpsc::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let (sender, receiver) = mpsc::channel();
        thread::spawn(move || {
            for response in responses {
                let (mut stream, _) = listener.accept().unwrap();
                let mut request = Vec::new();
                let mut buffer = [0_u8; 4096];
                // Read the head, then exactly Content-Length body bytes.
                loop {
                    let read = stream.read(&mut buffer).unwrap();
                    request.extend_from_slice(&buffer[..read]);
                    let text = String::from_utf8_lossy(&request).into_owned();
                    if let Some(end) = text.find("\r\n\r\n") {
                        let length = text
                            .lines()
                            .find_map(|line| line.strip_prefix("Content-Length: "))
                            .and_then(|value| value.trim().parse::<usize>().ok())
                            .unwrap_or(0);
                        if request.len() >= end + 4 + length {
                            break;
                        }
                    }
                    if read == 0 {
                        break;
                    }
                }
                sender
                    .send(String::from_utf8_lossy(&request).into_owned())
                    .unwrap();
                stream.write_all(response.as_bytes()).unwrap();
            }
        });
        (port, receiver)
    }

    #[test]
    fn patch_sends_the_exact_tun_body_with_the_secret() {
        let (port, requests) = fake_mihomo(vec!["HTTP/1.1 204 No Content\r\n\r\n"]);
        mihomo_rest::set_tun_enabled(port, "s3cret", true).unwrap();
        let request = requests.recv().unwrap();
        assert!(request.starts_with("PATCH /configs HTTP/1.1\r\n"));
        assert!(request.contains("Authorization: Bearer s3cret\r\n"));
        assert!(request.ends_with(r#"{"tun":{"enable":true}}"#));
    }

    #[test]
    fn a_non_204_patch_is_an_error() {
        let (port, _requests) = fake_mihomo(vec![
            "HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\n\r\n",
        ]);
        assert!(mihomo_rest::set_tun_enabled(port, "wrong", true).is_err());
    }

    #[test]
    fn tun_enable_rolls_back_when_the_adapter_never_appears() {
        // Mihomo answers 204 but GET /configs keeps reporting TUN off (verified v1.19.30
        // behaviour when sing-tun fails): the REST layer must say "not enabled".
        let (port, _requests) = fake_mihomo(vec![
            "HTTP/1.1 204 No Content\r\n\r\n",
            "HTTP/1.1 200 OK\r\nContent-Length: 24\r\n\r\n{\"tun\":{\"enable\":false}}",
        ]);
        mihomo_rest::set_tun_enabled(port, "s", true).unwrap();
        assert!(!mihomo_rest::tun_enabled(port, "s").unwrap());
    }

    #[test]
    fn tun_state_is_read_from_a_chunked_configs_response() {
        let (port, _requests) = fake_mihomo(vec![
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nContent-Type: application/json\r\n\r\nb\r\n{\"tun\":{\"en\r\nc\r\nable\":true}}\r\n0\r\n\r\n",
        ]);
        assert!(mihomo_rest::tun_enabled(port, "s").unwrap());
    }
}

mod state {
    use super::service::state_dir;

    #[test]
    fn fresh_generation_directory_replaces_older_generations() {
        let root = tempfile::tempdir().unwrap();
        let old = root.path().join("runtime-00000000000000000001");
        std::fs::create_dir(&old).unwrap();
        std::fs::write(old.join("mihomo.yaml"), b"old").unwrap();
        std::fs::create_dir(root.path().join("providers")).unwrap();
        let fresh = state_dir::fresh_generation_directory(root.path(), 2).unwrap();
        assert_eq!(fresh, root.path().join("runtime-00000000000000000002"));
        assert!(fresh.is_dir());
        assert!(!old.exists());
        assert!(
            root.path().join("providers").is_dir(),
            "provider caches survive"
        );
        std::fs::write(fresh.join("stale"), b"x").unwrap();
        let again = state_dir::fresh_generation_directory(root.path(), 2).unwrap();
        assert!(
            !again.join("stale").exists(),
            "a generation is always recreated empty"
        );
    }

    #[cfg(unix)]
    #[test]
    fn private_directory_is_mode_0700_and_symlinks_are_rejected() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("service");
        std::fs::create_dir(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        state_dir::ensure_private_directory(&dir).unwrap();
        let mode = std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
        let link = root.path().join("link");
        symlink(&dir, &link).unwrap();
        assert!(state_dir::ensure_private_directory(&link).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn private_sddl_grants_only_system_and_administrators() {
        assert!(state_dir::PRIVATE_SDDL.starts_with("D:P"));
        assert!(state_dir::PRIVATE_SDDL.contains(";;;SY)"));
        assert!(state_dir::PRIVATE_SDDL.contains(";;;BA)"));
        for forbidden in [";;;IU)", ";;;BU)", ";;;AU)", ";;;WD)", ";;;CO)"] {
            assert!(!state_dir::PRIVATE_SDDL.contains(forbidden), "{forbidden}");
        }
    }

    #[cfg(windows)]
    #[test]
    fn untrusted_owner_is_detected() {
        assert!(state_dir::owner_is_trusted("S-1-5-18"));
        assert!(state_dir::owner_is_trusted("S-1-5-32-544"));
        assert!(!state_dir::owner_is_trusted("S-1-5-21-1-2-3-1001"));
        assert!(!state_dir::owner_is_trusted("S-1-1-0"));
    }

    #[cfg(windows)]
    #[test]
    fn reparse_point_service_directory_is_rejected() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("target");
        std::fs::create_dir(&target).unwrap();
        let parent = root.path().join("MultiCore");
        std::fs::create_dir(&parent).unwrap();
        let link = parent.join("service");
        // A junction needs no privilege, so a standard user can plant one; fall back to it
        // when symbolic links are not permitted.
        if std::os::windows::fs::symlink_dir(&target, &link).is_err() {
            let status = std::process::Command::new("cmd")
                .arg("/C")
                .arg("mklink")
                .arg("/J")
                .arg(&link)
                .arg(&target)
                .stdout(std::process::Stdio::null())
                .status()
                .unwrap();
            assert!(status.success(), "junction creation failed");
        }
        assert!(state_dir::ensure_private_directory(&link).is_err());
        assert!(
            parent.is_dir(),
            "nothing is moved before every check passed"
        );
        assert!(link.exists());
    }
}

mod hashes {
    use super::service::core_hashes::{CoreHashError, verify_core_hashes};
    use sha2::{Digest, Sha256};

    fn package(core_bytes: &[u8]) -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("cores")).unwrap();
        std::fs::write(root.path().join("cores/mihomo.exe"), core_bytes).unwrap();
        let digest = format!("{:x}", Sha256::digest(core_bytes));
        std::fs::write(
            root.path().join("SHA256SUMS.txt"),
            format!("{digest} *cores/mihomo.exe\n"),
        )
        .unwrap();
        root
    }

    #[test]
    fn matching_core_is_accepted() {
        let root = package(b"pinned mihomo");
        verify_core_hashes(root.path(), &["cores/mihomo.exe"]).unwrap();
    }

    #[test]
    fn tampered_core_is_refused() {
        let root = package(b"pinned mihomo");
        std::fs::write(
            root.path().join("cores/mihomo.exe"),
            b"self-upgraded mihomo",
        )
        .unwrap();
        assert_eq!(
            verify_core_hashes(root.path(), &["cores/mihomo.exe"]),
            Err(CoreHashError::Mismatch("cores/mihomo.exe".into()))
        );
    }

    #[test]
    fn a_core_missing_from_the_manifest_is_refused() {
        let root = package(b"pinned mihomo");
        assert_eq!(
            verify_core_hashes(root.path(), &["cores/xray.exe"]),
            Err(CoreHashError::Unlisted("cores/xray.exe".into()))
        );
    }

    #[test]
    fn a_missing_manifest_is_refused() {
        let root = package(b"pinned mihomo");
        std::fs::remove_file(root.path().join("SHA256SUMS.txt")).unwrap();
        assert_eq!(
            verify_core_hashes(root.path(), &["cores/mihomo.exe"]),
            Err(CoreHashError::Manifest)
        );
    }
}

/// Plan amendment A1: the daemon only ever reaches Mihomo through the service's filtered proxy.
mod controller_proxy {
    use super::service::controller_proxy::ControllerProxy;
    use std::{
        sync::{Arc, Mutex},
        time::Duration,
    };
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::{TcpListener, TcpStream},
        time::timeout,
    };

    const TOKEN: &str = "proxy-session-token";
    const SECRET: &str = "raw-mihomo-secret";
    const WAIT: Duration = Duration::from_secs(5);

    #[derive(Clone, Copy)]
    enum Mode {
        Reply(&'static str),
        WebSocket,
    }

    struct FakeMihomo {
        port: u16,
        seen: Arc<Mutex<Vec<String>>>,
    }

    impl FakeMihomo {
        fn seen(&self) -> Vec<String> {
            self.seen.lock().unwrap().clone()
        }
    }

    async fn read_head(stream: &mut TcpStream) -> Vec<u8> {
        let mut data = Vec::new();
        let mut buffer = [0_u8; 4096];
        while !data.windows(4).any(|window| window == b"\r\n\r\n") {
            let read = stream.read(&mut buffer).await.unwrap();
            if read == 0 {
                break;
            }
            data.extend_from_slice(&buffer[..read]);
        }
        data
    }

    /// Records every byte each connection sends (head, body, and anything that follows within
    /// a short grace period), then answers per `mode`.
    async fn fake_mihomo(mode: Mode) -> FakeMihomo {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let record = seen.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let record = record.clone();
                tokio::spawn(async move {
                    let mut data = read_head(&mut stream).await;
                    match mode {
                        Mode::Reply(response) => {
                            let mut buffer = [0_u8; 4096];
                            while let Ok(Ok(read)) =
                                timeout(Duration::from_millis(300), stream.read(&mut buffer)).await
                            {
                                if read == 0 {
                                    break;
                                }
                                data.extend_from_slice(&buffer[..read]);
                            }
                            record
                                .lock()
                                .unwrap()
                                .push(String::from_utf8_lossy(&data).into_owned());
                            let _ = stream.write_all(response.as_bytes()).await;
                            let _ = stream.shutdown().await;
                        }
                        Mode::WebSocket => {
                            record
                                .lock()
                                .unwrap()
                                .push(String::from_utf8_lossy(&data).into_owned());
                            stream
                                .write_all(
                                    b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\r\nhello",
                                )
                                .await
                                .unwrap();
                            let mut frame = [0_u8; 4];
                            if timeout(WAIT, stream.read_exact(&mut frame)).await.is_ok() {
                                record
                                    .lock()
                                    .unwrap()
                                    .push(format!("ws:{}", String::from_utf8_lossy(&frame)));
                            }
                        }
                    }
                });
            }
        });
        FakeMihomo { port, seen }
    }

    async fn proxy_for(mihomo: &FakeMihomo) -> ControllerProxy {
        ControllerProxy::start(mihomo.port, SECRET.to_owned(), TOKEN.to_owned())
            .await
            .unwrap()
    }

    async fn exchange(proxy: &ControllerProxy, request: &str) -> String {
        let mut stream = TcpStream::connect(("127.0.0.1", proxy.port()))
            .await
            .unwrap();
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut response = Vec::new();
        timeout(WAIT, stream.read_to_end(&mut response))
            .await
            .expect("the proxy answers and closes")
            .unwrap();
        String::from_utf8_lossy(&response).into_owned()
    }

    fn authorised(method_and_target: &str, body: &str) -> String {
        format!(
            "{method_and_target} HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {TOKEN}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        )
    }

    const OK: Mode = Mode::Reply("HTTP/1.1 200 OK\r\nContent-Length: 11\r\n\r\n{\"ok\":true}");

    #[tokio::test(flavor = "multi_thread")]
    async fn an_allowed_get_is_forwarded_with_the_real_secret() {
        let mihomo = fake_mihomo(OK).await;
        let proxy = proxy_for(&mihomo).await;
        let response = exchange(&proxy, &authorised("GET /proxies", "")).await;
        assert!(response.starts_with("HTTP/1.1 200 OK"), "{response}");
        assert!(response.ends_with("{\"ok\":true}"));
        let seen = mihomo.seen();
        assert_eq!(seen.len(), 1);
        assert!(
            seen[0].starts_with("GET /proxies HTTP/1.1\r\n"),
            "{}",
            seen[0]
        );
        assert!(seen[0].contains(&format!("Authorization: Bearer {SECRET}\r\n")));
        assert!(seen[0].contains("Connection: close\r\n"));
        assert!(
            !seen[0].contains(TOKEN),
            "the session token never reaches Mihomo"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_wrong_or_missing_token_is_401_and_never_reaches_mihomo() {
        let mihomo = fake_mihomo(OK).await;
        let proxy = proxy_for(&mihomo).await;
        for request in [
            "GET /proxies HTTP/1.1\r\nHost: x\r\n\r\n".to_owned(),
            "GET /proxies HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer wrong\r\n\r\n".to_owned(),
            format!("GET /proxies HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer {SECRET}\r\n\r\n"),
            "GET /proxies?token=wrong HTTP/1.1\r\nHost: x\r\n\r\n".to_owned(),
            format!("GET /proxies HTTP/1.1\r\nHost: x\r\nAuthorization: Basic {TOKEN}\r\n\r\n"),
            // Even a denied route answers 401 first, so probing reveals nothing.
            "POST /upgrade HTTP/1.1\r\nHost: x\r\n\r\n".to_owned(),
        ] {
            let response = exchange(&proxy, &request).await;
            assert!(
                response.starts_with("HTTP/1.1 401 "),
                "{request}: {response}"
            );
        }
        assert!(mihomo.seen().is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_deny_list_is_403_and_never_reaches_mihomo() {
        let mihomo = fake_mihomo(OK).await;
        let proxy = proxy_for(&mihomo).await;
        let payload = r#"{"payload":"mixed-port: 7890\nexternal-controller: 0.0.0.0:9090"}"#;
        for (target, body) in [
            ("POST /upgrade", ""),
            ("POST /upgrade/ui", ""),
            ("POST /upgrade/geo", ""),
            ("PUT /configs", payload),
            ("PUT /configs?force=true", r#"{"path":"C:\\x.yaml"}"#),
            ("PATCH /configs", r#"{"tun":{"enable":true}}"#),
            ("POST /configs/geo", ""),
            ("POST /restart", ""),
            ("POST /cache/fakeip/flush", ""),
            ("POST /cache/dns/flush", ""),
            ("PUT /providers/proxies/sub", ""),
            ("PUT /providers/rules/ads", ""),
            ("PATCH /rules/disable", r#"{"0":true}"#),
            ("GET /debug/pprof", ""),
            ("GET /dns/query?name=example.com", ""),
            ("GET /ui", ""),
            ("GET /", ""),
            ("DELETE /connections/abc", ""),
            ("DELETE /proxies/x", ""),
            ("POST /proxies/x", ""),
            ("PUT /proxies/a/b", ""),
            ("PUT /proxies", ""),
            ("HEAD /version", ""),
            ("OPTIONS /proxies", ""),
            ("GET /proxies/..%2Fconfigs", ""),
            ("PUT /proxies/%2e%2e", ""),
            ("GET /proxies/a%2Fb/delay", ""),
            ("GET //configs", ""),
            ("GET /configs/", ""),
            ("GET /configs/geo", ""),
            ("GET /group/g/healthcheck", ""),
            ("GET /version/../configs", ""),
            ("GET http://127.0.0.1/version", ""),
        ] {
            let response = exchange(&proxy, &authorised(target, body)).await;
            assert!(
                response.starts_with("HTTP/1.1 403 "),
                "{target}: {response}"
            );
        }
        assert!(mihomo.seen().is_empty(), "{:?}", mihomo.seen());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_selector_switch_is_forwarded_with_its_body() {
        let mihomo = fake_mihomo(Mode::Reply("HTTP/1.1 204 No Content\r\n\r\n")).await;
        let proxy = proxy_for(&mihomo).await;
        let body = r#"{"name":"Узел 1"}"#;
        let target = "PUT /proxies/%D0%90%D0%B2%D1%82%D0%BE";
        let response = exchange(&proxy, &authorised(target, body)).await;
        assert!(response.starts_with("HTTP/1.1 204 "), "{response}");
        let seen = mihomo.seen();
        assert!(
            seen[0].starts_with("PUT /proxies/%D0%90%D0%B2%D1%82%D0%BE HTTP/1.1\r\n"),
            "{}",
            seen[0]
        );
        assert!(seen[0].contains(&format!("Content-Length: {}\r\n", body.len())));
        assert!(seen[0].ends_with(body));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn latency_reads_and_connection_close_are_forwarded() {
        let mihomo = fake_mihomo(OK).await;
        let proxy = proxy_for(&mihomo).await;
        for target in [
            "GET /version",
            "GET /proxies/node/delay?url=http%3A%2F%2Fcp.cloudflare.com&timeout=5000",
            "GET /group/Auto/delay?url=http%3A%2F%2Fcp.cloudflare.com&timeout=5000",
            "GET /group",
            "GET /providers/proxies",
            "GET /providers/rules",
            "GET /rules",
            "GET /configs",
            "GET /connections",
            "DELETE /connections",
        ] {
            let response = exchange(&proxy, &authorised(target, "")).await;
            assert!(
                response.starts_with("HTTP/1.1 200 "),
                "{target}: {response}"
            );
        }
        assert_eq!(mihomo.seen().len(), 10);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_query_token_authenticates_and_is_stripped_before_mihomo() {
        let mihomo = fake_mihomo(OK).await;
        let proxy = proxy_for(&mihomo).await;
        let request = format!(
            "GET /group/g/delay?url=http%3A%2F%2Fx&token={TOKEN}&timeout=100 HTTP/1.1\r\nHost: x\r\n\r\n"
        );
        let response = exchange(&proxy, &request).await;
        assert!(response.starts_with("HTTP/1.1 200 "), "{response}");
        let seen = mihomo.seen();
        assert!(
            seen[0].starts_with("GET /group/g/delay?url=http%3A%2F%2Fx&timeout=100 HTTP/1.1\r\n"),
            "{}",
            seen[0]
        );
        assert!(!seen[0].contains(TOKEN));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_pipelined_second_request_is_never_forwarded() {
        let mihomo = fake_mihomo(OK).await;
        let proxy = proxy_for(&mihomo).await;
        let request = format!(
            "{}{}",
            authorised("GET /version", ""),
            authorised("POST /upgrade", "")
        );
        let response = exchange(&proxy, &request).await;
        assert!(response.starts_with("HTTP/1.1 200 "), "{response}");
        let seen = mihomo.seen();
        assert_eq!(seen.len(), 1);
        assert!(!seen[0].contains("upgrade"), "{}", seen[0]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn websocket_logs_are_passed_through_both_ways() {
        let mihomo = fake_mihomo(Mode::WebSocket).await;
        let proxy = proxy_for(&mihomo).await;
        let mut stream = TcpStream::connect(("127.0.0.1", proxy.port()))
            .await
            .unwrap();
        let request = format!(
            "GET /logs?level=info&token={TOKEN} HTTP/1.1\r\nHost: x\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n"
        );
        stream.write_all(request.as_bytes()).await.unwrap();
        let mut received = Vec::new();
        let mut buffer = [0_u8; 1024];
        while !received.ends_with(b"hello") {
            let read = timeout(WAIT, stream.read(&mut buffer))
                .await
                .unwrap()
                .unwrap();
            assert_ne!(read, 0, "{}", String::from_utf8_lossy(&received));
            received.extend_from_slice(&buffer[..read]);
        }
        assert!(received.starts_with(b"HTTP/1.1 101 "));
        stream.write_all(b"ping").await.unwrap();
        let deadline = tokio::time::Instant::now() + WAIT;
        while !mihomo.seen().iter().any(|line| line == "ws:ping") {
            assert!(
                tokio::time::Instant::now() < deadline,
                "{:?}",
                mihomo.seen()
            );
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let head = &mihomo.seen()[0];
        assert!(
            head.starts_with("GET /logs?level=info HTTP/1.1\r\n"),
            "{head}"
        );
        assert!(head.contains("Upgrade: websocket\r\n"));
        assert!(head.contains("Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n"));
        assert!(head.contains(&format!("Authorization: Bearer {SECRET}\r\n")));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn websocket_upgrade_is_only_for_the_streaming_endpoints() {
        let mihomo = fake_mihomo(Mode::WebSocket).await;
        let proxy = proxy_for(&mihomo).await;
        let request = format!(
            "GET /configs HTTP/1.1\r\nHost: x\r\nAuthorization: Bearer {TOKEN}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n"
        );
        let response = exchange(&proxy, &request).await;
        assert!(response.starts_with("HTTP/1.1 403 "), "{response}");
        assert!(mihomo.seen().is_empty());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_stopped_proxy_refuses_new_connections() {
        let mihomo = fake_mihomo(OK).await;
        let proxy = proxy_for(&mihomo).await;
        let port = proxy.port();
        drop(proxy);
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(TcpStream::connect(("127.0.0.1", port)).await.is_err());
    }
}

/// Review M3: an unauthenticated client trickling bytes is cut off after one overall deadline.
#[tokio::test(flavor = "multi_thread")]
async fn a_trickling_client_is_released_within_the_drain_deadline() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let proxy = service::controller_proxy::ControllerProxy::start(1, "s".into(), "t".into())
        .await
        .unwrap();
    let mut stream = tokio::net::TcpStream::connect(("127.0.0.1", proxy.port()))
        .await
        .unwrap();
    stream
        .write_all(b"GET /proxies HTTP/1.1\r\nHost: x\r\n\r\n")
        .await
        .unwrap();
    let started = std::time::Instant::now();
    let mut response = vec![0_u8; 64];
    let read = stream.read(&mut response).await.unwrap();
    assert!(String::from_utf8_lossy(&response[..read]).starts_with("HTTP/1.1 401 "));
    // Our read side already saw the proxy's FIN; keep trickling a byte every 200 ms. Once the
    // proxy drops the socket, a write fails (RST). It must do so well before 3 s.
    let closed = loop {
        if stream.write_all(b"x").await.is_err() {
            break true;
        }
        if started.elapsed() > std::time::Duration::from_secs(3) {
            break false;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    };
    assert!(closed, "the drain deadline must end the connection");
}
