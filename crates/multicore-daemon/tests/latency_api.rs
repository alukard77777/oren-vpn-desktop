use std::{
    future::Future,
    io::{Read, Write},
    net::TcpListener,
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
    time::Duration,
};

use async_trait::async_trait;
use axum::{
    body::Body,
    http::{Request, StatusCode, header},
};
use http_body_util::BodyExt;
use multicore_core::{
    Engine, FetchError, HttpClient, HttpResponse, ProcessController, ProfileStore, Snapshot,
    SubscriptionFetcher,
};
use multicore_daemon::{
    Backend, BackendError, CoreBackend, LatencyProbeError, LatencyRequest, LatencyStatus,
    MihomoHttpSelector, MihomoSelector, SelectionRequest, SelectorError, router,
};
use serde_json::{Value, json};
use tower::ServiceExt;

const TOKEN: &str = "latency-token";
const MIHOMO: &str = r#"
proxies:
  - {name: "private / fast", type: socks5}
  - {name: "private / timeout", type: socks5}
proxy-groups:
  - {name: "private group / ?", type: select, proxies: ["private / fast", "private / timeout"]}
"#;

struct NeverHttp;

impl HttpClient for NeverHttp {
    fn get<'a>(
        &'a self,
        _: &'a str,
        _: &'static str,
    ) -> Pin<Box<dyn Future<Output = Result<HttpResponse, FetchError>> + Send + 'a>> {
        Box::pin(async { Err(FetchError::Network) })
    }
}

struct NoopController;

impl ProcessController for NoopController {
    type Error = ();

    fn start<'a>(
        &'a self,
        _: Engine,
    ) -> Pin<Box<dyn Future<Output = Result<(), Self::Error>> + Send + 'a>> {
        Box::pin(async { Ok(()) })
    }

    fn stop<'a>(
        &'a self,
        _: Engine,
    ) -> Pin<Box<dyn Future<Output = Result<(), Self::Error>> + Send + 'a>> {
        Box::pin(async { Ok(()) })
    }
}

#[derive(Default)]
struct RecordingController {
    probes: Mutex<Vec<String>>,
}

#[async_trait]
impl MihomoSelector for RecordingController {
    async fn select(&self, _: &str, _: &str) -> Result<(), SelectorError> {
        Ok(())
    }

    async fn probe_latency(&self, raw_name: &str) -> Result<u64, LatencyProbeError> {
        self.probes.lock().unwrap().push(raw_name.to_owned());
        if raw_name.ends_with("timeout") {
            Err(LatencyProbeError::Timeout)
        } else {
            Ok(42)
        }
    }
}

fn backend(
    controller: Arc<dyn MihomoSelector>,
) -> CoreBackend<
    NeverHttp,
    NoopController,
    impl Fn(&Snapshot) -> Result<Arc<NoopController>, BackendError> + use<>,
> {
    backend_with_mihomo(controller, MIHOMO)
}

fn backend_with_mihomo(
    controller: Arc<dyn MihomoSelector>,
    mihomo: &str,
) -> CoreBackend<
    NeverHttp,
    NoopController,
    impl Fn(&Snapshot) -> Result<Arc<NoopController>, BackendError> + use<>,
> {
    // Kept for the whole test: a dropped TempDir deletes the directory, which Windows refuses
    // while the store holds it open but Linux allows, failing later writes (StorageFailed).
    let root: &'static tempfile::TempDir = Box::leak(Box::new(tempfile::tempdir().unwrap()));
    let store = ProfileStore::open(root.path().join("profiles"), None).unwrap();
    store
        .create_profile(Arc::new(
            Snapshot::parse(mihomo.as_bytes(), br#"{}"#).unwrap(),
        ))
        .unwrap();
    CoreBackend::new_with_selector(
        store,
        SubscriptionFetcher::new(NeverHttp),
        |_| Ok(Arc::new(NoopController)),
        controller,
    )
    .unwrap()
}

struct ConcurrencyController {
    active: AtomicUsize,
    maximum: AtomicUsize,
}

struct SlowController;

#[async_trait]
impl MihomoSelector for SlowController {
    async fn select(&self, _: &str, _: &str) -> Result<(), SelectorError> {
        Ok(())
    }

    async fn probe_latency(&self, _: &str) -> Result<u64, LatencyProbeError> {
        tokio::time::sleep(Duration::from_secs(60)).await;
        Ok(1)
    }
}

#[async_trait]
impl MihomoSelector for ConcurrencyController {
    async fn select(&self, _: &str, _: &str) -> Result<(), SelectorError> {
        Ok(())
    }

    async fn probe_latency(&self, _: &str) -> Result<u64, LatencyProbeError> {
        let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.maximum.fetch_max(active, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(20)).await;
        self.active.fetch_sub(1, Ordering::SeqCst);
        Ok(1)
    }
}

fn request(body: Value, authenticated: bool) -> Request<Body> {
    let mut builder = Request::builder()
        .method("POST")
        .uri("/v1/latencies")
        .header(header::CONTENT_TYPE, "application/json");
    if authenticated {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {TOKEN}"));
    }
    builder.body(Body::from(body.to_string())).unwrap()
}

async fn json_body(response: axum::response::Response) -> Value {
    serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap()
}

#[tokio::test]
async fn latency_probe_requires_authentication_and_a_live_connection() {
    let controller = Arc::new(RecordingController::default());
    let backend = backend(controller.clone());
    let revision = backend.catalog().await.unwrap().revision;
    let app = router(backend, TOKEN);

    let unauthorized = app
        .clone()
        .oneshot(request(json!({"revision": revision}), false))
        .await
        .unwrap();
    assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);

    let disconnected = app
        .oneshot(request(json!({"revision": revision}), true))
        .await
        .unwrap();
    assert_eq!(disconnected.status(), StatusCode::CONFLICT);
    assert_eq!(json_body(disconnected).await["code"], "not_connected");
    assert!(controller.probes.lock().unwrap().is_empty());
}

#[tokio::test]
async fn empty_post_body_probes_the_current_catalog_contract() {
    let controller = Arc::new(RecordingController::default());
    let backend = backend(controller);
    let response = router(backend, TOKEN)
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/latencies")
                .header(header::AUTHORIZATION, format!("Bearer {TOKEN}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();

    assert_eq!(response.status(), StatusCode::CONFLICT);
    assert_eq!(json_body(response).await["code"], "not_connected");
}

#[tokio::test]
async fn probe_all_uses_raw_names_but_returns_only_opaque_ids_and_statuses() {
    let controller = Arc::new(RecordingController::default());
    let backend = backend(controller.clone());
    backend.connect().await.unwrap();
    let catalog = backend.catalog().await.unwrap();
    let app = router(backend, TOKEN);

    let response = app
        .oneshot(request(json!({"revision": catalog.revision}), true))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = json_body(response).await;
    assert_eq!(body["entries"].as_array().unwrap().len(), 2);
    assert_eq!(body["entries"][0]["group_id"], catalog.groups[0].id);
    assert_eq!(body["entries"][0]["node_id"], catalog.groups[0].nodes[0].id);
    assert_eq!(body["entries"][0]["latency_ms"], 42);
    assert_eq!(body["entries"][0]["status"], "ok");
    assert!(body["entries"][1]["latency_ms"].is_null());
    assert_eq!(body["entries"][1]["status"], "timeout");

    let serialized = body.to_string();
    for forbidden in [
        "private / fast",
        "private / timeout",
        "private group / ?",
        TOKEN,
    ] {
        assert!(
            !serialized.contains(forbidden),
            "response leaked {forbidden}"
        );
    }
    let mut probes = controller.probes.lock().unwrap().clone();
    probes.sort();
    assert_eq!(probes, ["private / fast", "private / timeout"]);
}

#[tokio::test]
async fn explicit_groups_and_nodes_are_deduplicated_and_validated_against_revision() {
    let controller = Arc::new(RecordingController::default());
    let backend = backend(controller.clone());
    backend.connect().await.unwrap();
    let catalog = backend.catalog().await.unwrap();

    let response = backend
        .latencies(LatencyRequest {
            revision: Some(catalog.revision),
            group_ids: vec![catalog.groups[0].id.clone()],
            node_ids: vec![catalog.groups[0].nodes[0].id.clone()],
        })
        .await
        .unwrap();
    assert_eq!(response.entries.len(), 2);
    assert_eq!(response.entries[0].status, LatencyStatus::Ok);
    assert_eq!(controller.probes.lock().unwrap().len(), 2);

    let error = backend
        .latencies(LatencyRequest {
            revision: Some(catalog.revision + 1),
            group_ids: Vec::new(),
            node_ids: Vec::new(),
        })
        .await
        .unwrap_err();
    assert_eq!(error, multicore_daemon::LatencyError::StaleRevision);
}

#[tokio::test]
async fn http_latency_probe_percent_encodes_raw_name_and_uses_bounded_authenticated_request() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let captured = Arc::new(Mutex::new(String::new()));
    let server_capture = captured.clone();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut request = [0_u8; 4096];
        let count = stream.read(&mut request).unwrap();
        *server_capture.lock().unwrap() = String::from_utf8_lossy(&request[..count]).into_owned();
        let body = r#"{"delay":73}"#;
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .unwrap();
    });

    let controller = MihomoHttpSelector::new(address, "private-secret").unwrap();
    assert_eq!(controller.probe_latency("node / ? # %").await, Ok(73));
    server.join().unwrap();

    let request = captured.lock().unwrap().clone();
    assert!(
        request.starts_with("GET /proxies/node%20%2F%20%3F%20%23%20%25/delay?"),
        "{request}"
    );
    assert!(request.contains("timeout=3000"), "{request}");
    assert!(
        request.contains("url=https%3A%2F%2Fwww.gstatic.com%2Fgenerate_204"),
        "{request}"
    );
    assert!(
        request
            .to_ascii_lowercase()
            .contains("authorization: bearer private-secret\r\n"),
        "{request}"
    );
}

#[tokio::test]
async fn probe_all_caps_controller_concurrency() {
    let nodes = (0..20)
        .map(|index| format!("node-{index}"))
        .collect::<Vec<_>>();
    let proxies = nodes
        .iter()
        .map(|node| format!("  - {{name: {node}, type: socks5}}"))
        .collect::<Vec<_>>()
        .join("\n");
    let mihomo = format!(
        "proxies:\n{proxies}\nproxy-groups:\n  - name: Main\n    type: select\n    proxies: [{}]\n",
        nodes.join(", ")
    );
    let controller = Arc::new(ConcurrencyController {
        active: AtomicUsize::new(0),
        maximum: AtomicUsize::new(0),
    });
    let backend = backend_with_mihomo(controller.clone(), &mihomo);
    backend.connect().await.unwrap();

    let response = backend.latencies(LatencyRequest::default()).await.unwrap();

    assert_eq!(response.entries.len(), 20);
    // The sweep's budget, plus the group's selected node on the priority lane.
    assert!(
        controller.maximum.load(Ordering::SeqCst)
            <= multicore_daemon::MAX_CONCURRENT_LATENCY_PROBES + 1
    );
}

#[tokio::test(start_paused = true)]
async fn slow_multiwave_probe_finishes_within_the_whole_batch_deadline() {
    let nodes = (0..17)
        .map(|index| format!("slow-{index}"))
        .collect::<Vec<_>>();
    let proxies = nodes
        .iter()
        .map(|node| format!("  - {{name: {node}, type: socks5}}"))
        .collect::<Vec<_>>()
        .join("\n");
    let mihomo = format!(
        "proxies:\n{proxies}\nproxy-groups:\n  - name: Main\n    type: select\n    proxies: [{}]\n",
        nodes.join(", ")
    );
    let backend = backend_with_mihomo(Arc::new(SlowController), &mihomo);
    backend.connect().await.unwrap();
    let started = tokio::time::Instant::now();

    let response = backend.latencies(LatencyRequest::default()).await.unwrap();

    assert!(started.elapsed() <= Duration::from_secs(8));
    assert_eq!(response.entries.len(), 17);
    assert!(
        response
            .entries
            .iter()
            .all(|entry| { entry.status == LatencyStatus::Timeout && entry.latency_ms.is_none() })
    );
}

#[tokio::test]
async fn probe_all_deterministically_caps_large_catalog_instead_of_rejecting_it() {
    let first_group = (0..256)
        .map(|index| format!("first-{index:03}"))
        .collect::<Vec<_>>();
    let second_group = (0..4)
        .map(|index| format!("second-{index:03}"))
        .collect::<Vec<_>>();
    let proxies = first_group
        .iter()
        .chain(&second_group)
        .map(|node| format!("  - {{name: {node}, type: socks5}}"))
        .collect::<Vec<_>>()
        .join("\n");
    let mihomo = format!(
        "proxies:\n{proxies}\nproxy-groups:\n  - name: First\n    type: select\n    proxies: [{}]\n  - name: Second\n    type: select\n    proxies: [{}]\n",
        first_group.join(", "),
        second_group.join(", ")
    );
    let controller = Arc::new(RecordingController::default());
    let backend = backend_with_mihomo(controller.clone(), &mihomo);
    backend.connect().await.unwrap();
    let catalog = backend.catalog().await.unwrap();

    let response = backend.latencies(LatencyRequest::default()).await.unwrap();

    assert_eq!(response.entries.len(), 256);
    assert!(
        response
            .entries
            .iter()
            .all(|entry| entry.group_id == catalog.groups[0].id)
    );
    let probes = controller.probes.lock().unwrap();
    assert_eq!(probes.len(), 256);
    assert!(probes.iter().all(|raw_name| raw_name.starts_with("first-")));
}

/// Every probe takes `delay`; records which node was probed, in order.
struct SleepyController {
    delay: Duration,
    probes: Mutex<Vec<String>>,
    active: AtomicUsize,
    maximum: AtomicUsize,
}

impl SleepyController {
    fn new(delay: Duration) -> Arc<Self> {
        Arc::new(Self {
            delay,
            probes: Mutex::new(Vec::new()),
            active: AtomicUsize::new(0),
            maximum: AtomicUsize::new(0),
        })
    }

    fn probes(&self) -> Vec<String> {
        self.probes.lock().unwrap().clone()
    }
}

#[async_trait]
impl MihomoSelector for SleepyController {
    async fn select(&self, _: &str, _: &str) -> Result<(), SelectorError> {
        Ok(())
    }

    async fn probe_latency(&self, raw_name: &str) -> Result<u64, LatencyProbeError> {
        self.probes.lock().unwrap().push(raw_name.to_owned());
        let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
        self.maximum.fetch_max(active, Ordering::SeqCst);
        tokio::time::sleep(self.delay).await;
        self.active.fetch_sub(1, Ordering::SeqCst);
        Ok(50)
    }
}

fn slow_group(count: usize) -> String {
    let nodes = (0..count)
        .map(|index| format!("slow-{index:02}"))
        .collect::<Vec<_>>();
    let proxies = nodes
        .iter()
        .map(|node| format!("  - {{name: {node}, type: socks5}}"))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "proxies:\n{proxies}\nproxy-groups:\n  - name: Main\n    type: select\n    proxies: [{}]\n",
        nodes.join(", ")
    )
}

fn assert_each_probed_once(probes: &[String]) {
    let mut unique = probes.to_vec();
    unique.sort();
    unique.dedup();
    assert_eq!(
        unique.len(),
        probes.len(),
        "a node was measured twice: {probes:?}"
    );
}

#[tokio::test(start_paused = true)]
async fn a_priority_node_is_answered_while_a_sweep_of_slow_nodes_still_runs() {
    let controller = SleepyController::new(Duration::from_secs(1));
    let backend = Arc::new(backend_with_mihomo(controller.clone(), &slow_group(40)));
    backend.connect().await.unwrap();
    let catalog = backend.catalog().await.unwrap();
    let group = &catalog.groups[0];
    let last = group.nodes.last().unwrap().id.clone();
    let started = tokio::time::Instant::now();

    let sweep = tokio::spawn({
        let backend = backend.clone();
        let request = LatencyRequest {
            revision: Some(catalog.revision),
            group_ids: vec![group.id.clone()],
            node_ids: Vec::new(),
        };
        async move { backend.latencies(request).await.unwrap() }
    });
    tokio::time::sleep(Duration::from_millis(100)).await;

    // The last node of the sweep queue: measured at once, outside the sweep's budget.
    let priority = backend
        .latencies(LatencyRequest {
            revision: Some(catalog.revision),
            group_ids: Vec::new(),
            node_ids: vec![last.clone()],
        })
        .await
        .unwrap();
    assert!(
        started.elapsed() < Duration::from_millis(1_500),
        "answered after {:?}",
        started.elapsed()
    );
    assert!(!sweep.is_finished(), "the sweep is still running");
    assert_eq!(priority.entries.len(), 1);
    assert_eq!(priority.entries[0].node_id, last);
    assert_eq!(priority.entries[0].status, LatencyStatus::Ok);

    let sweep = sweep.await.unwrap();
    assert!(started.elapsed() >= Duration::from_secs(4));
    assert_eq!(sweep.entries.len(), 40);
    assert!(
        sweep
            .entries
            .iter()
            .all(|entry| entry.status == LatencyStatus::Ok)
    );
    // The sweep reused the priority result instead of measuring that node again.
    let probes = controller.probes();
    assert_eq!(probes.len(), 40);
    assert_each_probed_once(&probes);
    // Sweep budget plus the priority lane, never more.
    assert!(
        controller.maximum.load(Ordering::SeqCst)
            <= multicore_daemon::MAX_CONCURRENT_LATENCY_PROBES
                + multicore_daemon::MAX_PRIORITY_LATENCY_PROBES
    );
}

#[tokio::test(start_paused = true)]
async fn a_node_already_in_flight_is_joined_not_measured_twice() {
    let controller = SleepyController::new(Duration::from_secs(1));
    let backend = Arc::new(backend_with_mihomo(controller.clone(), &slow_group(12)));
    backend.connect().await.unwrap();
    let catalog = backend.catalog().await.unwrap();
    let group = &catalog.groups[0];
    // Started by the sweep in its first wave.
    let early = group.nodes[2].id.clone();

    let sweep = tokio::spawn({
        let backend = backend.clone();
        let request = LatencyRequest {
            revision: Some(catalog.revision),
            group_ids: vec![group.id.clone()],
            node_ids: Vec::new(),
        };
        async move { backend.latencies(request).await.unwrap() }
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    let started = tokio::time::Instant::now();
    let joined = backend
        .latencies(LatencyRequest {
            revision: Some(catalog.revision),
            group_ids: Vec::new(),
            node_ids: vec![early],
        })
        .await
        .unwrap();
    // It got the sweep's probe, which had 0.7 s left.
    assert!(started.elapsed() < Duration::from_millis(900));
    assert_eq!(joined.entries[0].status, LatencyStatus::Ok);
    sweep.await.unwrap();
    let probes = controller.probes();
    assert_eq!(probes.len(), 12);
    assert_each_probed_once(&probes);
}

#[tokio::test(start_paused = true)]
async fn a_sweep_measures_each_groups_selected_node_first() {
    let controller = SleepyController::new(Duration::from_millis(200));
    let backend = backend_with_mihomo(controller.clone(), &slow_group(20));
    backend.connect().await.unwrap();
    let catalog = backend.catalog().await.unwrap();
    let group = &catalog.groups[0];
    let last = group.nodes.last().unwrap();
    backend
        .select(SelectionRequest {
            revision: catalog.revision,
            node_id: last.id.clone(),
            group_id: group.id.clone(),
        })
        .await
        .unwrap();
    let catalog = backend.catalog().await.unwrap();

    let response = backend
        .latencies(LatencyRequest {
            revision: Some(catalog.revision),
            group_ids: vec![catalog.groups[0].id.clone()],
            node_ids: Vec::new(),
        })
        .await
        .unwrap();
    assert_eq!(response.entries.len(), 20);
    // Entries keep the catalog order; the probes started with the selected node.
    assert_eq!(response.entries[19].node_id, catalog.groups[0].nodes[19].id);
    assert_eq!(controller.probes()[0], "slow-19");
}

#[tokio::test(start_paused = true)]
async fn an_abandoned_request_leaves_its_probe_to_whoever_waits_and_nothing_stale() {
    let controller = SleepyController::new(Duration::from_secs(1));
    let backend = backend_with_mihomo(controller.clone(), &slow_group(3));
    backend.connect().await.unwrap();
    let catalog = backend.catalog().await.unwrap();
    let node = catalog.groups[0].nodes[1].id.clone();
    let request = || LatencyRequest {
        revision: Some(catalog.revision),
        group_ids: Vec::new(),
        node_ids: vec![node.clone()],
    };

    // The caller gives up after 100 ms (a closed window, a timed-out client).
    assert!(
        tokio::time::timeout(Duration::from_millis(100), backend.latencies(request()))
            .await
            .is_err()
    );
    // The probe keeps running; a new request joins it instead of starting another.
    let started = tokio::time::Instant::now();
    let joined = backend.latencies(request()).await.unwrap();
    assert!(started.elapsed() < Duration::from_millis(950));
    assert_eq!(joined.entries[0].status, LatencyStatus::Ok);
    assert_eq!(controller.probes().len(), 1);

    // A later request measures afresh: a finished result from before it was asked is stale.
    tokio::time::sleep(Duration::from_secs(5)).await;
    let fresh = backend.latencies(request()).await.unwrap();
    assert_eq!(fresh.entries[0].status, LatencyStatus::Ok);
    assert_eq!(controller.probes().len(), 2);
}
