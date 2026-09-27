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
        "0d0fc0ea2b05641acb78c01fc36ad694e7b029861b2d5eb93da0e3e9fda9a98f"
    );
    #[cfg(not(windows))]
    assert_eq!(
        format!("{:x}", Sha256::digest(fs::read(&binary).unwrap())),
        "c4ae6798c38e0e5343b192406746333cd0ba7ff3eb984f8c4b9939dcb68c3f8a"
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
