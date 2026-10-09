//! Real pinned Xray; all DNS, SOCKS and destination traffic stays on loopback.
use std::{
    fs,
    io::{Read, Write},
    net::{SocketAddr, TcpListener, TcpStream, UdpSocket},
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use multicore_core::{MihomoRuntimeControl, Snapshot, publish_runtime_with_mihomo_control};
use serde_json::json;
use sha2::{Digest, Sha256};

const TTL: u32 = 3;
/// A syntactically valid (correct length, base64) but meaningless
/// 2022-blake3-aes-128-gcm key: only the TCP-level dial-after-resolve is
/// exercised, never a real shadowsocks handshake.
const WARMUP_PLACEHOLDER_KEY: &str = "MDEyMzQ1Njc4OWFiY2RlZg==";

struct Fixture {
    stop: Arc<AtomicBool>,
    threads: Vec<thread::JoinHandle<()>>,
}

impl Fixture {
    fn new() -> Self {
        Self {
            stop: Arc::new(AtomicBool::new(false)),
            threads: Vec::new(),
        }
    }

    fn dns(&mut self, phase: Arc<AtomicU8>, queries: Arc<AtomicUsize>) -> SocketAddr {
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        socket
            .set_read_timeout(Some(Duration::from_millis(100)))
            .unwrap();
        let address = socket.local_addr().unwrap();
        let stop = self.stop.clone();
        self.threads.push(thread::spawn(move || {
            let mut packet = [0_u8; 1500];
            while !stop.load(Ordering::SeqCst) {
                let Ok((len, peer)) = socket.recv_from(&mut packet) else {
                    continue;
                };
                assert!(peer.ip().is_loopback());
                assert!(len >= 17);
                let mut end = 12;
                while packet[end] != 0 {
                    end += usize::from(packet[end]) + 1;
                }
                assert!(
                    &packet[12..=end] == b"\x04edge\x07example\0"
                        || &packet[12..=end] == b"\x08ordinary\x07example\x03com\0"
                );
                let query_type = u16::from_be_bytes([packet[end + 1], packet[end + 2]]);
                queries.fetch_add(1, Ordering::SeqCst);
                let phase = phase.load(Ordering::SeqCst);
                // A blocked resolver drops packets. SERVFAIL instead exercises
                // Xray's separate negative-cache TTL, not transport recovery.
                if phase == 2 {
                    continue;
                }
                let mut response = packet[..end + 5].to_vec();
                response[2] = 0x81;
                response[3] = if phase == 2 { 0x82 } else { 0x80 };
                response[6..12].fill(0);
                if query_type == 1 && phase != 2 {
                    response[7] = 1;
                    response.extend_from_slice(&[0xc0, 0x0c, 0, 1, 0, 1]);
                    response.extend_from_slice(&TTL.to_be_bytes());
                    response.extend_from_slice(&[0, 4, 127, 0, 0, if phase == 0 { 2 } else { 3 }]);
                }
                socket.send_to(&response, peer).unwrap();
            }
        }));
        address
    }

    fn socks(&mut self, address: SocketAddr, marker: u8) -> SocketAddr {
        let listener = TcpListener::bind(address).unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let stop = self.stop.clone();
        self.threads.push(thread::spawn(move || {
            while !stop.load(Ordering::SeqCst) {
                let Ok((mut stream, peer)) = listener.accept() else {
                    thread::sleep(Duration::from_millis(5));
                    continue;
                };
                assert!(peer.ip().is_loopback());
                stream.set_nonblocking(false).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                stream
                    .set_write_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                let mut greeting = [0; 2];
                stream.read_exact(&mut greeting).unwrap();
                assert_eq!(greeting[0], 5);
                let mut methods = vec![0; greeting[1] as usize];
                stream.read_exact(&mut methods).unwrap();
                stream.write_all(&[5, 0]).unwrap();
                let mut header = [0; 4];
                stream.read_exact(&mut header).unwrap();
                assert_eq!(&header[..3], &[5, 1, 0]);
                let address_len = match header[3] {
                    1 => 4,
                    4 => 16,
                    3 => {
                        let mut len = [0];
                        stream.read_exact(&mut len).unwrap();
                        len[0] as usize
                    }
                    other => panic!("unexpected SOCKS address type {other}"),
                };
                stream.read_exact(&mut vec![0; address_len + 2]).unwrap();
                stream
                    .write_all(&[5, 0, 0, 1, 127, 0, 0, 1, 0, 0, marker])
                    .unwrap();
            }
        }));
        address
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        for worker in self.threads.drain(..) {
            let result = worker.join();
            if !thread::panicking() {
                result.unwrap();
            }
        }
    }
}

struct Core(Child);
impl Drop for Core {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn dial(address: SocketAddr) -> std::io::Result<u8> {
    let mut stream = TcpStream::connect_timeout(&address, Duration::from_secs(2))?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    stream.write_all(&[5, 1, 0])?;
    let mut greeting = [0; 2];
    stream.read_exact(&mut greeting)?;
    assert_eq!(greeting, [5, 0]);
    // Synthetic target is consumed by our fake SOCKS server, never dialled.
    stream.write_all(&[5, 1, 0, 1, 192, 0, 2, 1, 1, 187])?;
    let mut reply = [0; 10];
    stream.read_exact(&mut reply)?;
    if reply[1] != 0 {
        return Err(std::io::Error::other("SOCKS dial rejected"));
    }
    let mut marker = [0];
    stream.read_exact(&mut marker)?;
    Ok(marker[0])
}

#[test]
#[ignore = "requires explicit MULTICORE_TEST_XRAY_BIN; loopback-only native core smoke"]
fn live_dns_refreshes_after_ttl_without_core_restart_or_blocked_fallback() {
    let binary =
        PathBuf::from(std::env::var_os("MULTICORE_TEST_XRAY_BIN").expect("Xray binary required"));
    #[cfg(windows)]
    assert_eq!(
        format!("{:x}", Sha256::digest(fs::read(&binary).unwrap())),
        "43fa465275a8a64ddce4a27c3317ae3e99f0c264fec1962e04c3fadda83adc79"
    );
    #[cfg(not(windows))]
    assert_eq!(
        format!("{:x}", Sha256::digest(fs::read(&binary).unwrap())),
        "96efb28b8b2086275598dc46e73b5dc340da948b4e1ccc4f1719f83f67b793a7"
    );
    let mut fixture = Fixture::new();
    let phase = Arc::new(AtomicU8::new(0));
    let queries = Arc::new(AtomicUsize::new(0));
    let fallback_queries = Arc::new(AtomicUsize::new(0));
    let dns = fixture.dns(phase.clone(), queries.clone());
    let fallback_phase = Arc::new(AtomicU8::new(2));
    let fallback = fixture.dns(fallback_phase.clone(), fallback_queries.clone());
    let first = fixture.socks("127.0.0.2:0".parse().unwrap(), b'A');
    fixture.socks(
        SocketAddr::new("127.0.0.3".parse().unwrap(), first.port()),
        b'B',
    );
    let lease = TcpListener::bind("127.0.0.1:0").unwrap();
    let inbound = lease.local_addr().unwrap();
    let dns_lease = UdpSocket::bind("127.0.0.1:0").unwrap();
    let dns_inbound = dns_lease.local_addr().unwrap();
    let original = json!({
        "log":{"loglevel":"debug"},
        "dns":{"queryStrategy":"UseIPv4", "enableParallelQuery":true,
          "disableFallback":true,"tag":"provider-dns",
          "servers":[{"address":"127.0.0.1","port":fallback.port()}]},
        "inbounds":[{"tag":"client","listen":"127.0.0.1","port":inbound.port(),
          "protocol":"socks","settings":{"auth":"noauth","udp":false}},
          {"tag":"test-dns","listen":"127.0.0.1","port":dns_inbound.port(),
           "protocol":"dokodemo-door","settings":{"address":"127.0.0.1","port":53,"network":"udp"}}],
        "outbounds":[{"tag":"proxy","protocol":"socks",
          "settings":{"servers":[{"address":"edge.example","port":first.port()}]}},
          {"tag":"provider-direct","protocol":"freedom"},
          {"tag":"dns-query","protocol":"dns"}],
        "routing":{"rules":[
          {"type":"field","inboundTag":["provider-dns"],"outboundTag":"provider-direct"},
          {"type":"field","inboundTag":["test-dns"],"outboundTag":"dns-query"},
          {"type":"field","network":"tcp,udp","outboundTag":"proxy"}]}
    });
    let snapshot =
        Snapshot::parse(b"proxies: []\n", &serde_json::to_vec(&original).unwrap()).unwrap();
    let directory = tempfile::tempdir().unwrap();
    let control = MihomoRuntimeControl::new("127.0.0.1:19090".parse().unwrap(), "fixture")
        .unwrap()
        .with_xray_outbound_interface("fixture-interface")
        .unwrap();
    let paths = publish_runtime_with_mihomo_control(&snapshot, directory.path(), &control).unwrap();
    let mut runtime: serde_json::Value =
        serde_json::from_slice(&fs::read(&paths.xray_config).unwrap()).unwrap();
    assert_eq!(runtime["dns"]["servers"][1]["address"], "77.88.8.8");
    // Only transport addresses are adapted for isolation. Unit tests assert
    // physical-interface binding; this test does NOT claim live TUN coverage.
    runtime["dns"]["servers"][1]["address"] = json!("127.0.0.1");
    runtime["dns"]["servers"][1]["port"] = json!(dns.port());
    for outbound in runtime["outbounds"].as_array_mut().unwrap() {
        if outbound["tag"] == "multicore-endpoint-dns-direct" {
            // Deliberately unbound: Mihomo's DNS-leak protection blackholes
            // UDP:53 explicitly bound to the physical interface (proven on
            // real hardware), so this one outbound must never carry it.
            assert_eq!(
                outbound["streamSettings"]["sockopt"]["interface"],
                json!(null)
            );
            continue;
        }
        assert_eq!(
            outbound["streamSettings"]["sockopt"]["interface"],
            "fixture-interface"
        );
        outbound["streamSettings"]["sockopt"]
            .as_object_mut()
            .unwrap()
            .remove("interface");
    }
    // Negative control: reproduce the removed session-long host pinning.
    if std::env::var_os("MULTICORE_TEST_STATIC_DNS_NEGATIVE_CONTROL").is_some() {
        runtime["dns"]["hosts"] = json!({"edge.example":"127.0.0.2"});
    }
    if std::env::var_os("MULTICORE_TEST_NO_DNS_GUARD_NEGATIVE_CONTROL").is_some() {
        runtime["dns"]["servers"].as_array_mut().unwrap().remove(0);
    }
    fs::write(&paths.xray_config, serde_json::to_vec(&runtime).unwrap()).unwrap();
    let log_path = directory.path().join("xray.log");
    let log = fs::File::create(&log_path).unwrap();
    let mut command = Command::new(binary);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000);
    }
    command
        .args(["run", "-c"])
        .arg(&paths.xray_config)
        .stdin(Stdio::null())
        .stdout(log.try_clone().unwrap())
        .stderr(log);
    drop(lease);
    drop(dns_lease);
    let mut core = Core(command.spawn().unwrap());
    let pid = core.0.id();
    let ready_deadline = Instant::now() + Duration::from_secs(10);
    loop {
        assert!(
            core.0.try_wait().unwrap().is_none(),
            "{}",
            fs::read_to_string(&log_path).unwrap()
        );
        if TcpStream::connect_timeout(&inbound, Duration::from_millis(100)).is_ok() {
            break;
        }
        assert!(
            Instant::now() < ready_deadline,
            "Xray did not bind fixture inbound"
        );
        thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(
        dial(inbound).unwrap(),
        b'A',
        "{}",
        fs::read_to_string(&log_path).unwrap()
    );
    let initial_queries = queries.load(Ordering::SeqCst);
    phase.store(1, Ordering::SeqCst);
    assert_eq!(
        dial(inbound).unwrap(),
        b'A',
        "cached answer must be valid before TTL"
    );
    assert_eq!(queries.load(Ordering::SeqCst), initial_queries);
    // A short authoritative TTL tests expiry while the same core stays alive.
    thread::sleep(Duration::from_secs(u64::from(TTL) + 1));
    assert_eq!(
        dial(inbound).unwrap(),
        b'B',
        "expired DNS must reach the replacement endpoint"
    );
    assert!(queries.load(Ordering::SeqCst) > initial_queries);
    assert_eq!(fallback_queries.load(Ordering::SeqCst), 0);
    phase.store(2, Ordering::SeqCst);
    thread::sleep(Duration::from_secs(u64::from(TTL) + 1));
    assert!(
        dial(inbound).is_err(),
        "expired address must not survive resolver failure"
    );
    assert_eq!(
        fallback_queries.load(Ordering::SeqCst),
        0,
        "must not query blocked alternatives"
    );
    phase.store(0, Ordering::SeqCst);
    assert_eq!(
        dial(inbound).unwrap(),
        b'A',
        "DNS recovery must not need a restart"
    );
    assert_eq!(core.0.id(), pid);
    assert!(core.0.try_wait().unwrap().is_none());
    // Even with disableFallback, a non-endpoint query must retain the
    // provider's emergency resolver rather than use our scoped Yandex entry.
    fallback_phase.store(1, Ordering::SeqCst);
    let endpoint_queries = queries.load(Ordering::SeqCst);
    let client = UdpSocket::bind("127.0.0.1:0").unwrap();
    client
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut request = vec![0x12, 0x34, 1, 0, 0, 1, 0, 0, 0, 0, 0, 0];
    request.extend_from_slice(b"\x08ordinary\x07example\x03com\0\x00\x01\x00\x01");
    client.send_to(&request, dns_inbound).unwrap();
    let mut response = [0; 1500];
    let (size, _) = client.recv_from(&mut response).unwrap();
    assert_eq!(&response[..2], &[0x12, 0x34]);
    assert_eq!(
        &response[size - 4..size],
        &[127, 0, 0, 3],
        "provider resolver must answer unrelated DNS"
    );
    assert_eq!(queries.load(Ordering::SeqCst), endpoint_queries);
    assert_eq!(fallback_queries.load(Ordering::SeqCst), 1);
}

/// Regression coverage for a real failure found on real hardware: right after Xray starts
/// (and, in production, right as MultiCore's own brand-new TUN adapter attaches), a
/// domain-scoped endpoint DNS lookup can fail ("record not found") for a moment even though
/// the resolver itself is healthy. The SOCKS listener accepting a TCP connection proves
/// nothing about whether Xray's DNS client and interface-bound outbound sockets have settled,
/// so starting Xray also checks a real SOCKS5 CONNECT through its own loopback inbounds -
/// which forces a real domain resolve-then-dial.
///
/// That check goes through a remote node, and a node can be unreachable from the user's
/// network (a dead server; a "whitelist" mobile network that blocks every direct node). So it
/// is best effort since 1.1.12: the first node that answers proves the path, and when none
/// answers within `XRAY_STARTUP_SETTLE_BUDGET` Xray is ready anyway with a note in its log.
/// In 1.1.11 the one node it picked (always the first) being dead failed every Connect with
/// «Xray не успел запуститься вовремя».
///
/// Uses "ordinary.example.com" (already recognized by `Fixture::dns`'s wire-format stub
/// above), never "edge.example": Xray silently treats a bare `.example` suffix as a local-only
/// name and resolves it via its `localhost` client regardless of any scoped-resolver config,
/// entirely bypassing the resolver under test - the exact landmine `local_dns_domain` in
/// crates/multicore-core/src/snapshot.rs exists to keep provider endpoint domains away from.
struct LiveXray {
    _directory: tempfile::TempDir,
    paths: multicore_core::RuntimePaths,
    command: multicore_core::CommandSpec,
}

/// A real Xray whose inbounds each lead (1:1 tags) to a shadowsocks node at
/// `ordinary.example.com:<port>`, resolved by the fixture resolver at `dns` to 127.0.0.2.
fn live_xray(dns: SocketAddr, node_ports: &[u16]) -> LiveXray {
    use multicore_core::{CommandSpec, Engine, RuntimePaths};

    let binary =
        PathBuf::from(std::env::var_os("MULTICORE_TEST_XRAY_BIN").expect("Xray binary required"));
    #[cfg(windows)]
    assert_eq!(
        format!("{:x}", Sha256::digest(fs::read(&binary).unwrap())),
        "43fa465275a8a64ddce4a27c3317ae3e99f0c264fec1962e04c3fadda83adc79"
    );
    #[cfg(not(windows))]
    assert_eq!(
        format!("{:x}", Sha256::digest(fs::read(&binary).unwrap())),
        "96efb28b8b2086275598dc46e73b5dc340da948b4e1ccc4f1719f83f67b793a7"
    );

    let domain = "ordinary.example.com";
    // The DNS client's own outgoing query carries the DNS server's own `tag` as its virtual
    // inbound. Without an explicit routing rule for it, Xray's default route sends it out the
    // *same* outbound being resolved for - a circular dependency (the query needs the
    // outbound, the outbound needs the query) that production
    // (crates/multicore-core/src/snapshot.rs) avoids with a dedicated freedom outbound.
    // Mirrored here for the same reason.
    const DNS_OUTBOUND_TAG: &str = "warm-dns-direct";
    let mut inbounds = Vec::new();
    let mut outbounds = Vec::new();
    let mut rules = vec![json!({"type": "field", "inboundTag": ["warm-dns"],
        "outboundTag": DNS_OUTBOUND_TAG})];
    for (index, port) in node_ports.iter().enumerate() {
        let tag = format!("node-{index}");
        let reserved = TcpListener::bind("127.0.0.1:0").unwrap();
        let inbound_port = reserved.local_addr().unwrap().port();
        drop(reserved);
        inbounds.push(
            json!({"tag": tag, "listen": "127.0.0.1", "port": inbound_port,
            "protocol": "socks", "settings": {"auth": "noauth", "udp": false}}),
        );
        // shadowsocks, like every real production outbound this app generates, is a raw relay
        // protocol: Xray must resolve `settings.address` itself before dialing. A bogus key
        // still completes the TCP-level dial and the client-facing SOCKS5 CONNECT reply.
        outbounds.push(json!({"tag": tag, "protocol": "shadowsocks",
            "settings": {"address": domain, "port": port,
                "method": "2022-blake3-aes-128-gcm", "password": WARMUP_PLACEHOLDER_KEY},
            "streamSettings": {"sockopt": {"domainStrategy": "ForceIP"}}}));
        rules.push(json!({"type": "field", "inboundTag": [tag], "outboundTag": tag}));
    }
    outbounds.push(json!({"tag": DNS_OUTBOUND_TAG, "protocol": "freedom",
        "settings": {"domainStrategy": "AsIs"}}));
    let config = json!({
        "log": {"loglevel": "debug"},
        "dns": {"servers": [
            {"address": "127.0.0.1", "port": dns.port(), "domains": [format!("full:{domain}")],
             "skipFallback": true, "finalQuery": true, "tag": "warm-dns", "timeoutMs": 1200}
        ]},
        "inbounds": inbounds,
        "outbounds": outbounds,
        "routing": {"rules": rules}
    });
    let directory = tempfile::tempdir().unwrap();
    let xray_config = directory.path().join("xray.json");
    fs::write(&xray_config, serde_json::to_vec(&config).unwrap()).unwrap();
    let paths = RuntimePaths {
        xray_binary: binary.clone(),
        // Never launched or read for an Engine::Xray readiness wait; must simply exist.
        mihomo_binary: binary.clone(),
        xray_config: xray_config.clone(),
        mihomo_config: xray_config.clone(),
        dashboard_directory: None,
        mihomo_home_directory: None,
    };
    let command = CommandSpec {
        engine: Engine::Xray,
        program: paths.xray_binary.clone(),
        args: vec!["run".into(), "-c".into(), xray_config.into_os_string()],
        env: Vec::new(),
    };
    LiveXray {
        _directory: directory,
        paths,
        command,
    }
}

/// Starts `xray` and waits for readiness: the result, how long it took, and the notes logged.
async fn start_live(
    xray: &LiveXray,
) -> (
    Result<(), multicore_core::ProcessError>,
    Duration,
    Vec<String>,
) {
    use multicore_core::{
        CORE_READINESS_TIMEOUT, ManagedChild, ProcessLauncher, TokioProcessLauncher,
    };

    let launcher = TokioProcessLauncher::for_runtime(xray.paths.clone(), CORE_READINESS_TIMEOUT);
    let mut child = launcher.launch(xray.command.clone()).await.unwrap();
    let started = Instant::now();
    let result = launcher.wait_until_ready(&xray.command, &mut child).await;
    let elapsed = started.elapsed();
    child.stop().await.unwrap();
    if std::env::var_os("MULTICORE_TEST_XRAY_LOG").is_some() {
        for record in launcher.logs().snapshot() {
            eprintln!("{} {}", record.timestamp_ms, record.message);
        }
    }
    (result, elapsed, launcher.logs().take_notes())
}

/// A node that accepts the dial: the real Xray keeps the proxied tunnel open.
fn live_node(threads: &mut Vec<thread::JoinHandle<()>>) -> u16 {
    let target = TcpListener::bind("127.0.0.2:0").unwrap();
    let port = target.local_addr().unwrap().port();
    threads.push(thread::spawn(move || {
        let _held = target.accept();
        thread::sleep(Duration::from_secs(5));
    }));
    port
}

/// A node nothing listens on: the real Xray's dial is refused and it closes the tunnel.
fn dead_node() -> u16 {
    let reserved = TcpListener::bind("127.0.0.2:0").unwrap();
    let port = reserved.local_addr().unwrap().port();
    drop(reserved);
    port
}

/// Healthy endpoint DNS and a live node: readiness still waits out Xray's own DNS timeout
/// window (it does not trust the optimistic first SOCKS5 reply), and notes nothing.
#[tokio::test]
#[ignore = "requires explicit MULTICORE_TEST_XRAY_BIN; loopback-only native core smoke"]
async fn readiness_gate_waits_for_a_real_dns_resolve_then_dial_before_reporting_ready() {
    let mut fixture = Fixture::new();
    let queries = Arc::new(AtomicUsize::new(0));
    let dns = fixture.dns(Arc::new(AtomicU8::new(0)), queries.clone());
    let mut threads = Vec::new();
    let xray = live_xray(dns, &[live_node(&mut threads)]);
    let (result, elapsed, notes) = start_live(&xray).await;
    eprintln!("healthy DNS, live node: ready after {elapsed:?}");
    assert_eq!(result, Ok(()));
    assert!(
        elapsed >= Duration::from_millis(1_200),
        "ready after only {elapsed:?}: the gate trusted the first SOCKS5 reply"
    );
    // Proven, not given up on: nothing to note.
    assert_eq!(notes, Vec::<String>::new());
    assert!(
        queries.load(Ordering::SeqCst) > 0,
        "no endpoint DNS query was ever observed"
    );
}

/// The 1.1.11 failure with the real Xray: the first node is dead, the second is alive.
#[tokio::test]
#[ignore = "requires explicit MULTICORE_TEST_XRAY_BIN; loopback-only native core smoke"]
async fn real_xray_with_a_dead_first_node_is_ready_through_the_live_one() {
    let mut fixture = Fixture::new();
    let dns = fixture.dns(Arc::new(AtomicU8::new(0)), Arc::new(AtomicUsize::new(0)));
    let mut threads = Vec::new();
    let xray = live_xray(dns, &[dead_node(), live_node(&mut threads)]);
    let (result, elapsed, notes) = start_live(&xray).await;
    eprintln!("dead first node: ready after {elapsed:?}");
    assert_eq!(result, Ok(()));
    // Proven through the live node (Xray's own start takes ~0.5 s before the probe begins).
    assert_eq!(notes, Vec::<String>::new());
    assert!(elapsed < Duration::from_secs(3), "took {elapsed:?}");
}

/// Every node dead: the real Xray is ready within the budget anyway, with a note saying why
/// nothing could be proven.
#[tokio::test]
#[ignore = "requires explicit MULTICORE_TEST_XRAY_BIN; loopback-only native core smoke"]
async fn real_xray_with_every_node_dead_is_ready_within_the_budget() {
    let mut fixture = Fixture::new();
    let dns = fixture.dns(Arc::new(AtomicU8::new(0)), Arc::new(AtomicUsize::new(0)));
    let xray = live_xray(dns, &[dead_node(), dead_node()]);
    let (result, elapsed, notes) = start_live(&xray).await;
    eprintln!("every node dead: ready after {elapsed:?}, notes {notes:?}");
    assert_eq!(result, Ok(()));
    // Xray's own start (~0.5 s before its listeners answer) plus the budget.
    assert!(
        elapsed < multicore_core::XRAY_STARTUP_SETTLE_BUDGET + Duration::from_secs(1),
        "took {elapsed:?}"
    );
    assert_eq!(notes.len(), 1, "{notes:?}");
}

/// Endpoint DNS that never answers. Recorded on the pinned Xray 26.9.30: after a failed
/// lookup ("failed to resolve ip > record not found" after `timeoutMs`) Xray retries the
/// dial, lookup included, instead of closing the client's tunnel - so for several seconds
/// the probe cannot tell a blocked resolver from a node that answered. Whatever the probe
/// concludes, starting Xray never waits past the budget.
#[tokio::test]
#[ignore = "requires explicit MULTICORE_TEST_XRAY_BIN; loopback-only native core smoke"]
async fn real_xray_with_blocked_endpoint_dns_is_ready_within_the_budget() {
    let mut fixture = Fixture::new();
    // Phase 2 drops every query: a resolver that never answers.
    let blocked_dns = fixture.dns(Arc::new(AtomicU8::new(2)), Arc::new(AtomicUsize::new(0)));
    let mut threads = Vec::new();
    let xray = live_xray(blocked_dns, &[live_node(&mut threads)]);
    let (result, elapsed, notes) = start_live(&xray).await;
    eprintln!("endpoint DNS blocked: ready after {elapsed:?}, notes {notes:?}");
    assert_eq!(result, Ok(()));
    assert!(
        elapsed < multicore_core::XRAY_STARTUP_SETTLE_BUDGET + Duration::from_secs(1),
        "took {elapsed:?}"
    );
}
