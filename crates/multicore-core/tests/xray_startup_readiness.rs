//! Xray is ready as soon as its loopback listeners accept connections: starting it never dials
//! a node. A start that waited on a node let one unreachable node fail every Connect with
//! «Xray не успел запуститься вовремя», and on a Russian mobile "whitelist" network every
//! direct node is unreachable by design. A dead node shows as its own failed ping instead.
//!
//! Here a fake Xray stands in for the real one: one loopback SOCKS listener per inbound that
//! counts every SOCKS request it receives (a node probe), and a long-running helper process as
//! the child.
use std::{
    ffi::OsString,
    io::Read,
    net::TcpListener,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use multicore_core::{
    CommandSpec, Engine, ManagedChild, ProcessLauncher, RuntimePaths, TokioProcessLauncher,
};

/// One fake Xray SOCKS inbound; returns its port. A connection that sends anything (a SOCKS
/// greeting) is counted in `probes`; a readiness check only connects and closes.
fn fake_inbound(probes: Arc<AtomicUsize>) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    thread::spawn(move || {
        for mut stream in listener.incoming().flatten() {
            let probes = probes.clone();
            thread::spawn(move || {
                let mut byte = [0_u8; 1];
                if stream.read(&mut byte).unwrap_or(0) > 0 {
                    probes.fetch_add(1, Ordering::SeqCst);
                }
            });
        }
    });
    port
}

/// One node of a fake subscription: an outbound to `domain`, with a 1:1-tagged loopback SOCKS
/// inbound when `inbound` is set (a relay used only to reach other nodes has none).
struct Node {
    tag: String,
    domain: String,
    inbound: Option<u16>,
    via: Option<Chain>,
}

/// How a chained node reaches its server: Xray's two ways of dialling through another outbound.
enum Chain {
    DialerProxy(&'static str),
    ProxySettings(&'static str),
}

fn xray_config(root: &std::path::Path, nodes: &[Node]) -> PathBuf {
    let inbounds = nodes
        .iter()
        .filter_map(|node| {
            node.inbound.map(|port| {
                serde_json::json!({
                    "tag": node.tag, "protocol": "socks", "listen": "127.0.0.1", "port": port
                })
            })
        })
        .collect::<Vec<_>>();
    let outbounds = nodes
        .iter()
        .map(|node| {
            let mut outbound = serde_json::json!({
                "tag": node.tag, "protocol": "shadowsocks",
                "settings": {"address": node.domain, "port": 3478}
            });
            match node.via {
                Some(Chain::DialerProxy(via)) => {
                    outbound["streamSettings"] =
                        serde_json::json!({"sockopt": {"dialerProxy": via}});
                }
                Some(Chain::ProxySettings(via)) => {
                    outbound["proxySettings"] = serde_json::json!({"tag": via});
                }
                None => {}
            }
            outbound
        })
        .collect::<Vec<_>>();
    let path = root.join("xray.json");
    std::fs::write(
        &path,
        serde_json::json!({"inbounds": inbounds, "outbounds": outbounds}).to_string(),
    )
    .unwrap();
    path
}

fn launcher_for(root: &std::path::Path, nodes: &[Node]) -> TokioProcessLauncher {
    let paths = RuntimePaths {
        xray_binary: PathBuf::new(),
        mihomo_binary: PathBuf::new(),
        xray_config: xray_config(root, nodes),
        mihomo_config: root.join("mihomo.yaml"),
        dashboard_directory: None,
        mihomo_home_directory: None,
    };
    TokioProcessLauncher::for_runtime(paths, multicore_core::CORE_READINESS_TIMEOUT)
}

/// A stand-in Xray process: readiness only needs a live child, the fake listeners do the rest.
fn helper_command() -> CommandSpec {
    CommandSpec {
        engine: Engine::Xray,
        program: std::env::current_exe().unwrap(),
        args: [
            OsString::from("--ignored"),
            OsString::from("--exact"),
            OsString::from("readiness_child_helper"),
            OsString::from("--nocapture"),
        ]
        .into(),
        env: Vec::new(),
    }
}

#[test]
#[ignore = "isolated helper process for the Xray startup readiness tests"]
fn readiness_child_helper() {
    thread::sleep(Duration::from_secs(30));
}

/// A Russian mobile "whitelist" network: every direct node is unreachable by design and only
/// chains through a whitelisted relay work («Sweden via RU»); the relay has no inbound of its
/// own and the chain comes last. Xray must be ready as soon as its listeners are up, without
/// dialling any node.
async fn whitelist_network_is_ready_without_probing_a_node(chain: Chain) {
    let root = tempfile::tempdir().unwrap();
    let probes = Arc::new(AtomicUsize::new(0));
    let mut nodes = (0..12)
        .map(|index| {
            let domain = format!("direct{index}.blocked.test");
            Node {
                tag: format!("node-{domain}"),
                domain,
                inbound: Some(fake_inbound(probes.clone())),
                via: None,
            }
        })
        .collect::<Vec<_>>();
    nodes.push(Node {
        tag: "RU relay".to_owned(),
        domain: "relay.ru.test".to_owned(),
        inbound: None,
        via: None,
    });
    nodes.push(Node {
        tag: "Sweden via RU".to_owned(),
        domain: "se.chain.test".to_owned(),
        inbound: Some(fake_inbound(probes.clone())),
        via: Some(chain),
    });
    let launcher = launcher_for(root.path(), &nodes);

    let command = helper_command();
    let mut child = launcher.launch(command.clone()).await.unwrap();
    let started = Instant::now();
    let result = launcher.wait_until_ready(&command, &mut child).await;
    let elapsed = started.elapsed();
    child.stop().await.unwrap();
    eprintln!("Xray readiness with the whitelist fixture: {elapsed:?}");

    assert_eq!(result, Ok(()));
    assert!(
        elapsed < Duration::from_millis(1_500),
        "Xray's startup took {elapsed:?}: it waited on something beyond its listeners"
    );
    // Give a stray probe connection time to deliver its greeting before counting.
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        probes.load(Ordering::SeqCst),
        0,
        "starting Xray sent a SOCKS request to a node"
    );
}

#[tokio::test]
async fn a_whitelist_network_with_a_dialer_proxy_chain_is_ready_at_once() {
    whitelist_network_is_ready_without_probing_a_node(Chain::DialerProxy("RU relay")).await;
}

#[tokio::test]
async fn a_whitelist_network_with_a_proxy_settings_chain_is_ready_at_once() {
    whitelist_network_is_ready_without_probing_a_node(Chain::ProxySettings("RU relay")).await;
}
