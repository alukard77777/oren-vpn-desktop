//! Starting Xray proves its endpoint DNS + dial path through a real SOCKS CONNECT, but that
//! CONNECT necessarily goes through a remote proxy node. A user on Windows 10 could never
//! connect: the probe always picked the subscription's first node, that one node was
//! unreachable from their network, so every Connect failed with «Xray не успел запуститься
//! вовремя» after the full readiness timeout although Xray and every other node were fine.
//!
//! Here a fake Xray stands in for the real one: one loopback SOCKS listener per inbound,
//! answering a CONNECT the way Xray does (the success reply at once) and then either keeping
//! the tunnel (a live node: Xray resolved and dialled it) or closing it shortly after (a dead
//! node, or an endpoint DNS lookup that has not settled yet).
use std::{
    ffi::OsString,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::PathBuf,
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

use multicore_core::{
    CommandSpec, Engine, ManagedChild, ProcessLauncher, RuntimePaths, TokioProcessLauncher,
    XRAY_STARTUP_SETTLE_BUDGET,
};

/// What the fake Xray does with one CONNECT, decided per connection.
type Behaviour = Arc<dyn Fn() -> bool + Send + Sync>;

fn alive() -> Behaviour {
    Arc::new(|| true)
}

fn dead() -> Behaviour {
    Arc::new(|| false)
}

/// Fails every CONNECT until `after` has passed, then keeps them: endpoint DNS that settles late.
fn alive_after(after: Duration) -> Behaviour {
    let from = Instant::now() + after;
    Arc::new(move || Instant::now() >= from)
}

/// One fake Xray SOCKS inbound; returns its port.
fn fake_inbound(behaviour: Behaviour) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let behaviour = behaviour.clone();
            thread::spawn(move || serve(stream, &*behaviour));
        }
    });
    port
}

fn serve(mut stream: TcpStream, keep: &(dyn Fn() -> bool + Send + Sync)) {
    let mut greeting = [0_u8; 3];
    if stream.read_exact(&mut greeting).is_err() {
        return;
    }
    let _ = stream.write_all(&[5, 0]);
    let mut buffer = [0_u8; 512];
    if stream.read(&mut buffer).unwrap_or(0) == 0 {
        return;
    }
    // Xray acknowledges a CONNECT before it has resolved or dialled anything.
    let _ = stream.write_all(&[5, 0, 0, 1, 0, 0, 0, 0, 0, 0]);
    if keep() {
        while stream.read(&mut buffer).unwrap_or(0) > 0 {}
    } else {
        // The node refused (or the lookup failed): Xray closes the client side shortly after.
        thread::sleep(Duration::from_millis(50));
    }
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

fn node(inbound: u16, domain: &str) -> Node {
    Node {
        tag: format!("node-{domain}"),
        domain: domain.to_owned(),
        inbound: Some(inbound),
        via: None,
    }
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
            OsString::from("settle_child_helper"),
            OsString::from("--nocapture"),
        ]
        .into(),
        env: Vec::new(),
    }
}

#[test]
#[ignore = "isolated helper process for the Xray startup settle tests"]
fn settle_child_helper() {
    thread::sleep(Duration::from_secs(30));
}

async fn start(
    launcher: &TokioProcessLauncher,
) -> (Result<(), multicore_core::ProcessError>, Duration) {
    let command = helper_command();
    let mut child = launcher.launch(command.clone()).await.unwrap();
    let started = Instant::now();
    let result = launcher.wait_until_ready(&command, &mut child).await;
    let elapsed = started.elapsed();
    child.stop().await.unwrap();
    (result, elapsed)
}

/// Ready with the endpoint path proven: in time, and with nothing to note.
fn assert_proven(launcher: &TokioProcessLauncher, elapsed: Duration) {
    assert!(
        elapsed < XRAY_STARTUP_SETTLE_BUDGET,
        "Xray's startup took {elapsed:?}"
    );
    assert_eq!(launcher.logs().take_notes(), Vec::<String>::new());
}

/// The reported failure: the first node is dead, the second works. Xray must be ready as soon
/// as the live node proves the DNS + dial path, not time out on the dead one.
#[tokio::test]
async fn a_dead_first_node_does_not_hold_xray_startup() {
    let root = tempfile::tempdir().unwrap();
    let nodes = [
        node(fake_inbound(dead()), "srv33.dead.test"),
        node(fake_inbound(alive()), "edge.alive.test"),
    ];
    let launcher = launcher_for(root.path(), &nodes);
    let (result, elapsed) = start(&launcher).await;
    assert_eq!(result, Ok(()));
    assert_proven(&launcher, elapsed);
}

/// Every node unreachable from this network: Xray itself is up (its listeners answer), so it
/// is ready within the settle budget and the logs say why nothing could be proven. Each dead
/// node then fails its own ping instead of the whole connection.
#[tokio::test]
async fn xray_is_ready_within_the_settle_budget_when_every_node_is_dead() {
    let root = tempfile::tempdir().unwrap();
    let nodes = [
        node(fake_inbound(dead()), "a.dead.test"),
        node(fake_inbound(dead()), "b.dead.test"),
        node(fake_inbound(dead()), "c.dead.test"),
    ];
    let launcher = launcher_for(root.path(), &nodes);
    let (result, elapsed) = start(&launcher).await;
    assert_eq!(result, Ok(()));
    assert!(
        elapsed >= XRAY_STARTUP_SETTLE_BUDGET.saturating_sub(Duration::from_millis(200)),
        "gave up on the nodes after only {elapsed:?}"
    );
    assert!(
        elapsed < XRAY_STARTUP_SETTLE_BUDGET + Duration::from_millis(1_000),
        "every node dead held Xray's startup for {elapsed:?}"
    );
    let notes = launcher.logs().take_notes();
    assert_eq!(notes.len(), 1, "{notes:?}");
    assert!(notes[0].contains("3 "), "{}", notes[0]);
    let logged = format!("{:?}", launcher.logs().snapshot());
    assert!(
        logged.contains(&notes[0]),
        "the note is in the core log too"
    );
    assert!(
        launcher.logs().take_notes().is_empty(),
        "taking the notes empties them"
    );
}

/// What the probe exists for: right after a fresh start Xray's endpoint DNS can fail for a
/// moment. When it settles within the budget, startup still waits for it.
#[tokio::test]
async fn xray_startup_still_waits_for_endpoint_dns_that_settles_late() {
    let root = tempfile::tempdir().unwrap();
    let settles_after = Duration::from_millis(200);
    let nodes = [
        node(fake_inbound(alive_after(settles_after)), "a.late.test"),
        node(fake_inbound(alive_after(settles_after)), "b.late.test"),
    ];
    let launcher = launcher_for(root.path(), &nodes);
    let (result, elapsed) = start(&launcher).await;
    assert_eq!(result, Ok(()));
    // Settles at 0.2 s, then the tunnel must survive Xray's own DNS timeout window (1.2 s).
    assert!(
        elapsed >= settles_after + Duration::from_millis(1_200),
        "ready after {elapsed:?}, before endpoint DNS had settled"
    );
    assert_proven(&launcher, elapsed);
}

/// A large subscription must not open a connection to every node at each start.
#[tokio::test]
async fn xray_startup_probes_a_bounded_number_of_nodes() {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    let root = tempfile::tempdir().unwrap();
    let probed = Arc::new(AtomicUsize::new(0));
    let nodes = (0..40)
        .map(|index| {
            let probed = probed.clone();
            let counted = Arc::new(AtomicBool::new(false));
            let port = fake_inbound(Arc::new(move || {
                if !counted.swap(true, Ordering::SeqCst) {
                    probed.fetch_add(1, Ordering::SeqCst);
                }
                false
            }));
            node(port, &format!("n{index}.dead.test"))
        })
        .collect::<Vec<_>>();
    let launcher = launcher_for(root.path(), &nodes);
    let (result, _) = start(&launcher).await;
    assert_eq!(result, Ok(()));
    let probed = probed.load(Ordering::SeqCst);
    assert!(
        (2..=8).contains(&probed),
        "{probed} of 40 nodes were probed"
    );
}

/// A Russian mobile "whitelist" network: only whitelisted addresses are reachable, so every
/// direct node is dead by design and only chains through a whitelisted relay work («Sweden via
/// RU»). The relay itself has no inbound of its own, and the chain comes last. Readiness must
/// be quick, and the chain must stay usable afterwards.
async fn whitelist_network_with_a_working_chain(chain: Chain) {
    let root = tempfile::tempdir().unwrap();
    let mut nodes = (0..12)
        .map(|index| node(fake_inbound(dead()), &format!("direct{index}.blocked.test")))
        .collect::<Vec<_>>();
    nodes.push(Node {
        tag: "RU relay".to_owned(),
        domain: "relay.ru.test".to_owned(),
        inbound: None,
        via: None,
    });
    let chain_port = fake_inbound(alive());
    nodes.push(Node {
        tag: "Sweden via RU".to_owned(),
        domain: "se.chain.test".to_owned(),
        inbound: Some(chain_port),
        via: Some(chain),
    });
    let launcher = launcher_for(root.path(), &nodes);
    let (result, elapsed) = start(&launcher).await;
    assert_eq!(result, Ok(()));
    assert_proven(&launcher, elapsed);

    // The chained route still carries traffic after readiness.
    let mut stream = TcpStream::connect(("127.0.0.1", chain_port)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    stream.write_all(&[5, 1, 0]).unwrap();
    let mut greeting = [0_u8; 2];
    stream.read_exact(&mut greeting).unwrap();
    stream
        .write_all(&[5, 1, 0, 1, 192, 0, 2, 1, 1, 187])
        .unwrap();
    let mut reply = [0_u8; 10];
    stream.read_exact(&mut reply).unwrap();
    assert_eq!(reply[1], 0, "the chain refused traffic after readiness");
}

#[tokio::test]
async fn a_whitelist_network_with_a_dialer_proxy_chain_is_ready_quickly() {
    whitelist_network_with_a_working_chain(Chain::DialerProxy("RU relay")).await;
}

#[tokio::test]
async fn a_whitelist_network_with_a_proxy_settings_chain_is_ready_quickly() {
    whitelist_network_with_a_working_chain(Chain::ProxySettings("RU relay")).await;
}
