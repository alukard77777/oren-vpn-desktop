use std::{
    future::Future,
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use multicore_core::{
    Engine, FetchError, HttpClient, HttpResponse, ProcessController, ProfileId, ProfileStore,
    Snapshot, SubscriptionFetcher, UA_MIHOMO, UA_NATIVE, UA_XRAY,
};
use multicore_daemon::{
    Backend, BackendError, ConnectOptions, ConnectionState, CoreBackend, CoresReadiness,
    DaemonText, ImportSubscriptionRequest, MihomoSelector, OptionsControllerFactory,
    PreparedController, ProfileMutationError, SelectorError, options_controller_factory,
};

pub const PROFILE_A: &str = "proxies:\n  - {name: node-a, type: socks5}\nproxy-groups:\n  - {name: g, type: select, proxies: [node-a]}\n";
pub const PROFILE_B: &str = "proxies:\n  - {name: node-b, type: socks5}\nproxy-groups:\n  - {name: g, type: select, proxies: [node-b]}\n";
pub const PROFILE_C: &str = "proxies:\n  - {name: node-c, type: socks5}\nproxy-groups:\n  - {name: g, type: select, proxies: [node-c]}\n";
/// Served by [`SubscriptionHttp`].
pub const NEW_SUBSCRIPTION: &str = "https://c.invalid/sub";
/// Unreachable through [`SubscriptionHttp`].
pub const FAILING_SUBSCRIPTION: &str = "https://fail.invalid/sub";
const WARM_UP_FAILED: &str = "Ядра не удалось запустить заранее. Они запустятся при подключении.";
const CORES_LOST: &str =
    "Служба MULTICORE потеряла ядра — подключение разорвано. Подключитесь снова.";
const CORES_LOST_RECONNECTING: &str = "Служба MULTICORE потеряла ядра — переподключаемся…";
const CORES_RECONNECTED: &str = "Подключение восстановлено автоматически.";
const CORES_RECONNECT_FAILED: &str =
    "Не удалось переподключиться автоматически — подключитесь снова.";
const PROMPT: Duration = Duration::from_secs(2);

/// A subscription server: every URL serves `PROFILE_C`, except one containing `fail`,
/// which is unreachable.
pub struct SubscriptionHttp;

impl HttpClient for SubscriptionHttp {
    fn get<'a>(
        &'a self,
        url: &'a str,
        user_agent: &'static str,
    ) -> Pin<Box<dyn Future<Output = Result<HttpResponse, FetchError>> + Send + 'a>> {
        Box::pin(async move {
            if url.contains("fail") {
                return Err(FetchError::Network);
            }
            let body = match user_agent {
                UA_NATIVE => format!("[{},{{}}]", serde_json::to_string(PROFILE_C).unwrap()),
                UA_MIHOMO => PROFILE_C.to_owned(),
                UA_XRAY => "{}".to_owned(),
                _ => return Err(FetchError::Network),
            };
            Ok(HttpResponse::new(
                200,
                body.into_bytes(),
                std::iter::empty::<(&str, &str)>(),
            ))
        })
    }
}

#[derive(Default)]
pub struct Recording {
    pub calls: Mutex<Vec<(bool, Engine)>>,
    pub fail_xray: AtomicBool,
    /// `start(Xray)` never finishes: a `Prepare` stuck on a slow service.
    pub hang_xray: AtomicBool,
    /// The next factory call blocks here (a service probe stuck on a starting service).
    pub factory_gate: Mutex<Option<std::sync::mpsc::Receiver<()>>>,
    pub factory_entered: AtomicBool,
    /// When set, the factory reports that it staged anew instead of reusing the warm one.
    pub restage_reason: Mutex<Option<String>>,
    /// The next `stop` waits here (a disconnect still tearing the tunnel down).
    pub stop_gate: Mutex<Option<Arc<tokio::sync::Notify>>>,
    pub stop_entered: AtomicBool,
    /// The options of every factory call, in order.
    pub options: Mutex<Vec<ConnectOptions>>,
}

pub struct RecordingController(pub Arc<Recording>);

impl ProcessController for RecordingController {
    type Error = ();

    fn start<'a>(
        &'a self,
        engine: Engine,
    ) -> Pin<Box<dyn Future<Output = Result<(), ()>> + Send + 'a>> {
        Box::pin(async move {
            self.0.calls.lock().unwrap().push((true, engine));
            if engine == Engine::Xray && self.0.hang_xray.load(Ordering::SeqCst) {
                std::future::pending::<()>().await;
            }
            if engine == Engine::Xray && self.0.fail_xray.load(Ordering::SeqCst) {
                Err(())
            } else {
                Ok(())
            }
        })
    }

    fn stop<'a>(
        &'a self,
        engine: Engine,
    ) -> Pin<Box<dyn Future<Output = Result<(), ()>> + Send + 'a>> {
        Box::pin(async move {
            let gate = self.0.stop_gate.lock().unwrap().take();
            if let Some(gate) = gate {
                self.0.stop_entered.store(true, Ordering::SeqCst);
                gate.notified().await;
            }
            self.0.calls.lock().unwrap().push((false, engine));
            Ok(())
        })
    }
}

pub struct Readiness {
    pub ready: AtomicBool,
    pub available: AtomicBool,
    pub lost: AtomicBool,
    pub released: AtomicUsize,
}

impl Readiness {
    pub fn new(ready: bool) -> Self {
        Self {
            ready: AtomicBool::new(ready),
            available: AtomicBool::new(true),
            lost: AtomicBool::new(false),
            released: AtomicUsize::new(0),
        }
    }
}

impl CoresReadiness for Readiness {
    fn cores_ready(&self) -> bool {
        self.ready.load(Ordering::SeqCst)
    }

    fn service_available(&self) -> bool {
        self.available.load(Ordering::SeqCst)
    }

    fn connection_lost(&self) -> bool {
        self.lost.load(Ordering::SeqCst)
    }

    fn release(&self) {
        self.released.fetch_add(1, Ordering::SeqCst);
        self.ready.store(false, Ordering::SeqCst);
    }

    fn loss_details(&self) -> serde_json::Value {
        serde_json::json!({ "cause": "channel", "error": "TimedOut", "status_ms": 15_002 })
    }
}

#[derive(Default)]
pub struct QuietSelector {
    pub selections: Mutex<Vec<(String, String)>>,
}

#[async_trait]
impl MihomoSelector for QuietSelector {
    async fn select(&self, group: &str, node: &str) -> Result<(), SelectorError> {
        self.selections
            .lock()
            .unwrap()
            .push((group.to_owned(), node.to_owned()));
        Ok(())
    }

    async fn probe_latency(&self, _: &str) -> Result<u64, multicore_daemon::LatencyProbeError> {
        Ok(42)
    }

    async fn proxy_now(&self, raw_name: &str) -> Result<Option<String>, SelectorError> {
        Ok((raw_name == "g").then(|| "node-a".to_owned()))
    }
}

pub type FactoryFn = Box<
    dyn Fn(
            &Snapshot,
            ConnectOptions,
        ) -> Result<PreparedController<RecordingController>, BackendError>
        + Send
        + Sync,
>;

pub type Factory = OptionsControllerFactory<FactoryFn>;

pub struct Harness {
    pub backend: Arc<CoreBackend<SubscriptionHttp, RecordingController, Factory>>,
    pub recording: Arc<Recording>,
    pub seen: Arc<Mutex<Vec<&'static str>>>,
    pub profiles: Vec<ProfileId>,
    pub selector: Arc<QuietSelector>,
    _root: tempfile::TempDir,
}

pub fn harness(profiles: &[&str]) -> Harness {
    let root = tempfile::tempdir().unwrap();
    let store = ProfileStore::open(root.path().join("profiles"), None).unwrap();
    let ids = profiles
        .iter()
        .map(|yaml| {
            store
                .create_profile(Arc::new(
                    Snapshot::parse(yaml.as_bytes(), br#"{}"#).unwrap(),
                ))
                .unwrap()
        })
        .collect::<Vec<_>>();
    let recording = Arc::new(Recording::default());
    let seen = Arc::new(Mutex::new(Vec::new()));
    let controller = Arc::new(RecordingController(recording.clone()));
    let factory_seen = seen.clone();
    let factory: FactoryFn = Box::new(move |snapshot: &Snapshot, options: ConnectOptions| {
        controller.0.options.lock().unwrap().push(options);
        let gate = controller.0.factory_gate.lock().unwrap().take();
        if let Some(gate) = gate {
            controller.0.factory_entered.store(true, Ordering::SeqCst);
            let _ = gate.recv();
        }
        factory_seen
            .lock()
            .unwrap()
            .push(if snapshot.mihomo.raw_yaml().contains("node-b") {
                "b"
            } else if snapshot.mihomo.raw_yaml().contains("node-c") {
                "c"
            } else {
                "a"
            });
        let prepared = PreparedController::reuse(controller.clone());
        Ok(match controller.0.restage_reason.lock().unwrap().clone() {
            Some(reason) => prepared.restaged_because(reason),
            None => prepared,
        })
    });
    let selector = Arc::new(QuietSelector::default());
    let backend = CoreBackend::new_with_connect_options(
        store,
        SubscriptionFetcher::new(SubscriptionHttp),
        options_controller_factory(factory),
        selector.clone(),
        None,
    )
    .unwrap();
    Harness {
        backend: Arc::new(backend),
        recording,
        seen,
        profiles: ids,
        selector,
        _root: root,
    }
}

pub async fn enable(harness: &Harness, ready: bool) -> Arc<Readiness> {
    let readiness = Arc::new(Readiness::new(ready));
    harness.backend.enable_always_ready(readiness.clone()).await;
    readiness
}

#[tokio::test]
async fn warm_up_does_nothing_until_always_ready_is_enabled() {
    let h = harness(&[PROFILE_A]);
    h.backend.warm_up().await.unwrap();
    assert!(h.recording.calls.lock().unwrap().is_empty());
    let status = h.backend.status().await.unwrap();
    assert!(!status.cores_ready);
    assert!(!status.service_mode);
}

#[tokio::test]
async fn warm_up_starts_the_idle_cores_and_reports_them_ready() {
    let h = harness(&[PROFILE_A]);
    enable(&h, true).await;
    h.backend.warm_up().await.unwrap();
    assert_eq!(*h.recording.calls.lock().unwrap(), [(true, Engine::Xray)]);
    let status = h.backend.status().await.unwrap();
    assert_eq!(status.state, ConnectionState::Ready);
    assert!(status.cores_ready);
    assert!(status.service_mode);
}

#[tokio::test]
async fn lost_readiness_is_reflected_in_status() {
    let h = harness(&[PROFILE_A]);
    let readiness = enable(&h, true).await;
    h.backend.warm_up().await.unwrap();
    readiness.ready.store(false, Ordering::SeqCst);
    assert!(!h.backend.status().await.unwrap().cores_ready);
}

#[tokio::test]
async fn disconnect_keeps_the_cores_warm() {
    let h = harness(&[PROFILE_A]);
    enable(&h, true).await;
    h.backend.warm_up().await.unwrap();
    h.backend.connect().await.unwrap();
    h.backend.disconnect().await.unwrap();
    // The watcher's warm-up finds the cores of this catalog still running: nothing to start.
    h.backend.warm_up().await.unwrap();
    assert_eq!(
        *h.recording.calls.lock().unwrap(),
        [
            (true, Engine::Xray),
            (true, Engine::Xray),
            (true, Engine::Mihomo),
            (false, Engine::Mihomo),
            (false, Engine::Xray),
        ]
    );
    let status = h.backend.status().await.unwrap();
    assert_eq!(status.state, ConnectionState::Ready);
    assert!(status.cores_ready);
}

#[tokio::test]
async fn a_connect_that_could_not_reuse_the_warm_generation_says_why() {
    let h = harness(&[PROFILE_A]);
    enable(&h, true).await;
    h.backend.warm_up().await.unwrap();
    *h.recording.restage_reason.lock().unwrap() =
        Some("interface_changed staged=\"Ethernet\" now=\"Wi-Fi\"".to_owned());
    h.backend.connect().await.unwrap();
    let events = h.backend.events_after(0).await.unwrap();
    let miss = events
        .iter()
        .find(|event| event.level == "debug" && event.component == "core")
        .expect("a diagnostic event names the miss");
    let details = miss.details.as_ref().unwrap();
    assert_eq!(
        details["reason"],
        "interface_changed staged=\"Ethernet\" now=\"Wi-Fi\""
    );
    let connected = events
        .iter()
        .find(|event| {
            event
                .details
                .as_ref()
                .is_some_and(|d| d.get("total_ms").is_some())
        })
        .unwrap();
    assert_eq!(connected.details.as_ref().unwrap()["restaged"], true);
}

#[tokio::test]
async fn a_reused_warm_generation_leaves_no_miss_event() {
    let h = harness(&[PROFILE_A]);
    enable(&h, true).await;
    h.backend.warm_up().await.unwrap();
    h.backend.connect().await.unwrap();
    let events = h.backend.events_after(0).await.unwrap();
    assert!(events.iter().all(|event| event.level != "debug"));
}

#[tokio::test]
async fn activating_another_profile_rewarms_with_its_snapshot() {
    let h = harness(&[PROFILE_A, PROFILE_B]);
    enable(&h, true).await;
    h.backend.warm_up().await.unwrap();
    // The trait method (not the inherent `activate_subscription(&ProfileId)`) re-warms.
    let warm = h.backend.warm_requests();
    let _ = tokio::time::timeout(Duration::from_millis(50), warm.notified()).await;
    Backend::activate_subscription(&*h.backend, h.profiles[1].clone())
        .await
        .unwrap();
    tokio::time::timeout(PROMPT, warm.notified())
        .await
        .expect("the change asks the watcher to re-warm");
    h.backend.warm_up().await.unwrap();
    assert_eq!(h.seen.lock().unwrap().last(), Some(&"b"));
    let xray_starts = h
        .recording
        .calls
        .lock()
        .unwrap()
        .iter()
        .filter(|call| **call == (true, Engine::Xray))
        .count();
    assert_eq!(xray_starts, 2);
    assert!(h.backend.status().await.unwrap().cores_ready);
}

#[tokio::test]
async fn a_failing_warm_up_is_reported_once() {
    let h = harness(&[PROFILE_A]);
    h.recording.fail_xray.store(true, Ordering::SeqCst);
    enable(&h, true).await;
    assert!(h.backend.warm_up().await.is_err());
    assert!(h.backend.warm_up().await.is_err());
    let events = h.backend.events_after(0).await.unwrap();
    assert_eq!(
        events
            .iter()
            .filter(|event| event.message == WARM_UP_FAILED)
            .count(),
        1
    );
    assert!(!h.backend.status().await.unwrap().cores_ready);
}

#[tokio::test]
async fn a_notice_reaches_status_and_events() {
    let h = harness(&[PROFILE_A]);
    h.backend
        .record_notice(
            "service_unavailable",
            DaemonText::code("service.unavailable_notice"),
        )
        .await;
    assert_eq!(
        h.backend.status().await.unwrap().message.as_deref(),
        Some("Служба MULTICORE недоступна — права администратора будут запрошены при подключении.")
    );
    assert!(
        h.backend
            .events_after(0)
            .await
            .unwrap()
            .iter()
            .any(|event| event.message == "Служба MULTICORE недоступна — права администратора будут запрошены при подключении."
                && event.message_key.as_ref().is_some_and(|key| key.code == "service.unavailable_notice"))
    );
}

#[tokio::test]
async fn a_shared_backend_serves_the_router() {
    use axum::{body::Body, http::Request};
    use tower::ServiceExt;
    let h = harness(&[PROFILE_A]);
    let backend: Arc<dyn Backend> = h.backend.clone();
    let app = multicore_daemon::try_router_shared(backend, "shared-token", None, None).unwrap();
    let response = app
        .oneshot(
            Request::get("/v1/status")
                .header("authorization", "Bearer shared-token")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
}

#[tokio::test]
async fn latency_session_and_selection_work_before_connecting_with_warm_cores() {
    use multicore_daemon::LatencyRequest;
    let h = harness(&[PROFILE_A]);
    enable(&h, true).await;
    h.backend.warm_up().await.unwrap();
    let latencies = h
        .backend
        .latencies(LatencyRequest {
            revision: None,
            group_ids: Vec::new(),
            node_ids: Vec::new(),
        })
        .await
        .unwrap();
    assert!(
        latencies
            .entries
            .iter()
            .all(|entry| entry.latency_ms == Some(42))
    );
    let session = h.backend.session().await.unwrap();
    assert_eq!(
        session.connected_since_unix_ms, 0,
        "no uptime before connecting"
    );
    assert_eq!(
        session.connected_for_ms, None,
        "no uptime before connecting"
    );
    assert_eq!(
        session.download_total, None,
        "no counters before connecting"
    );
    let catalog = h.backend.catalog().await.unwrap();
    let group = &catalog.groups[0];
    h.backend
        .select(multicore_daemon::SelectionRequest {
            revision: catalog.revision,
            group_id: group.id.clone(),
            node_id: group.nodes[0].id.clone(),
        })
        .await
        .unwrap();
    assert_eq!(
        *h.selector.selections.lock().unwrap(),
        [("g".to_owned(), "node-a".to_owned())]
    );
}

#[tokio::test]
async fn latency_selection_and_catalog_keep_working_after_a_disconnect() {
    use multicore_daemon::LatencyRequest;
    let h = harness(&[PROFILE_A]);
    enable(&h, true).await;
    h.backend.warm_up().await.unwrap();
    h.backend.connect().await.unwrap();
    let catalog = h.backend.catalog().await.unwrap();
    let status = h.backend.disconnect().await.unwrap();
    assert_eq!(status.state, ConnectionState::Ready);
    assert!(
        status.cores_ready,
        "the disconnect answer says the cores stay warm"
    );
    assert_eq!(
        h.backend.catalog().await.unwrap().revision,
        catalog.revision,
        "the catalog revision survives the disconnect"
    );
    let latencies = h
        .backend
        .latencies(LatencyRequest {
            revision: Some(catalog.revision),
            group_ids: Vec::new(),
            node_ids: Vec::new(),
        })
        .await
        .expect("latency after a disconnect with warm cores");
    assert!(
        latencies
            .entries
            .iter()
            .all(|entry| entry.latency_ms == Some(42))
    );
}

#[tokio::test]
async fn without_warm_cores_latency_and_session_still_require_a_connection() {
    let h = harness(&[PROFILE_A]);
    assert!(matches!(
        h.backend
            .latencies(multicore_daemon::LatencyRequest {
                revision: None,
                group_ids: Vec::new(),
                node_ids: Vec::new(),
            })
            .await,
        Err(multicore_daemon::LatencyError::NotConnected)
    ));
    assert!(matches!(
        h.backend.session().await,
        Err(multicore_daemon::SessionError::NotConnected)
    ));
}

#[tokio::test]
async fn disconnect_returns_without_waiting_for_the_warm_up() {
    let h = harness(&[PROFILE_A]);
    let readiness = enable(&h, true).await;
    h.backend.warm_up().await.unwrap();
    h.backend.connect().await.unwrap();
    let warm = h.backend.warm_requests();
    let _ = tokio::time::timeout(Duration::from_millis(50), warm.notified()).await;
    // From here a warm-up would hang (a service that stopped answering Prepare).
    h.recording.hang_xray.store(true, Ordering::SeqCst);
    readiness.ready.store(false, Ordering::SeqCst);
    let status = tokio::time::timeout(PROMPT, h.backend.disconnect())
        .await
        .expect("disconnect does not wait for the warm-up")
        .unwrap();
    assert_eq!(status.state, ConnectionState::Ready);
    tokio::time::timeout(PROMPT, warm.notified())
        .await
        .expect("the re-warm is requested in the background");
}

#[tokio::test]
async fn a_warm_up_in_flight_blocks_neither_selection_nor_disconnect() {
    let h = harness(&[PROFILE_A]);
    enable(&h, false).await;
    h.recording.hang_xray.store(true, Ordering::SeqCst);
    let warming = {
        let backend = h.backend.clone();
        tokio::spawn(async move { backend.warm_up().await })
    };
    // Let the warm-up reach its (never finishing) Prepare.
    for _ in 0..50 {
        if !h.recording.calls.lock().unwrap().is_empty() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(*h.recording.calls.lock().unwrap(), [(true, Engine::Xray)]);
    let catalog = h.backend.catalog().await.unwrap();
    let group = &catalog.groups[0];
    tokio::time::timeout(
        PROMPT,
        h.backend.select(multicore_daemon::SelectionRequest {
            revision: catalog.revision,
            group_id: group.id.clone(),
            node_id: group.nodes[0].id.clone(),
        }),
    )
    .await
    .expect("a selection does not wait for the warm-up")
    .unwrap();
    tokio::time::timeout(PROMPT, h.backend.disconnect())
        .await
        .expect("a disconnect does not wait for the warm-up")
        .unwrap();
    warming.abort();
}

#[tokio::test]
async fn a_service_that_becomes_available_later_is_used_without_a_restart() {
    let h = harness(&[PROFILE_A]);
    let readiness = enable(&h, true).await;
    readiness.available.store(false, Ordering::SeqCst);
    assert!(!h.backend.watch_service().await);
    assert!(h.recording.calls.lock().unwrap().is_empty(), "no warm-up");
    assert!(!h.backend.status().await.unwrap().service_mode);
    readiness.available.store(true, Ordering::SeqCst);
    assert!(h.backend.watch_service().await);
    assert_eq!(*h.recording.calls.lock().unwrap(), [(true, Engine::Xray)]);
    let status = h.backend.status().await.unwrap();
    assert!(status.service_mode);
    assert!(status.cores_ready);
}

#[tokio::test]
async fn a_connection_the_service_lost_is_torn_down_and_reported() {
    let h = harness(&[PROFILE_A]);
    let readiness = enable(&h, true).await;
    h.backend.warm_up().await.unwrap();
    h.backend.connect().await.unwrap();
    assert!(h.backend.watch_service().await, "healthy: nothing to do");
    assert_eq!(
        h.backend.status().await.unwrap().state,
        ConnectionState::Connected
    );
    readiness.lost.store(true, Ordering::SeqCst);
    readiness.ready.store(false, Ordering::SeqCst);
    assert!(!h.backend.watch_service().await);
    // `stop(Mihomo)` is where proxy mode restores the system proxy.
    assert_eq!(
        h.recording.calls.lock().unwrap()[3..],
        [(false, Engine::Mihomo), (false, Engine::Xray)]
    );
    let status = h.backend.status().await.unwrap();
    assert_eq!(status.state, ConnectionState::Ready);
    assert_eq!(status.message.as_deref(), Some(CORES_LOST_RECONNECTING));
    let events = h.backend.events_after(0).await.unwrap();
    let lost = events
        .iter()
        .find(|event| event.message == CORES_LOST_RECONNECTING)
        .expect("the loss is reported");
    assert_eq!(lost.level, "error");
    // Why: what the service channel saw, how long the connection had been up, and that one
    // automatic reconnect follows.
    let details = lost.details.as_ref().expect("the loss says why");
    assert_eq!(
        details["service"],
        serde_json::json!({ "cause": "channel", "error": "TimedOut", "status_ms": 15_002 })
    );
    assert!(details["connected_ms"].is_u64(), "{details}");
    assert_eq!(details["auto_reconnect"], true);
}

/// The owner's 1.1.9 report: the service lost the cores, the daemon re-warmed them 4 s later,
/// and the user was left disconnected with «Подключитесь снова». A loss the user did not ask
/// for is followed by one automatic reconnect through the service.
#[tokio::test]
async fn a_lost_connection_is_reconnected_once_automatically() {
    use multicore_core::core_sessions::read_sessions;
    let h = harness(&[PROFILE_A]);
    let directory = tempfile::tempdir().unwrap();
    h.backend
        .enable_session_log(directory.path().to_path_buf())
        .await;
    let readiness = enable(&h, true).await;
    h.backend.warm_up().await.unwrap();
    h.backend.connect().await.unwrap();
    readiness.lost.store(true, Ordering::SeqCst);
    readiness.ready.store(false, Ordering::SeqCst);
    assert!(!h.backend.watch_service().await);
    assert!(h.backend.session().await.is_err(), "no uptime while lost");
    // The service is usable again (it answered the next probe).
    readiness.lost.store(false, Ordering::SeqCst);
    readiness.ready.store(true, Ordering::SeqCst);
    assert!(h.backend.watch_service().await);
    let status = h.backend.status().await.unwrap();
    assert_eq!(status.state, ConnectionState::Connected);
    assert_eq!(status.message.as_deref(), Some(CORES_RECONNECTED));
    let session = h.backend.session().await.unwrap();
    assert!(
        session.connected_for_ms.is_some_and(|ms| ms < 60_000),
        "the uptime starts over: {session:?}"
    );
    // Through the service only: an automatic reconnect never asks for administrator rights.
    let options = *h.recording.options.lock().unwrap().last().unwrap();
    assert!(
        options.service_only && options.tun && !options.warm_up,
        "{options:?}"
    );
    assert!(
        h.backend
            .events_after(0)
            .await
            .unwrap()
            .iter()
            .any(|event| event.message == CORES_RECONNECTED && event.level == "info")
    );
    let reasons = read_sessions(directory.path())
        .iter()
        .map(|session| session.reason.clone().unwrap_or_else(|| "open".into()))
        .collect::<Vec<_>>();
    assert_eq!(reasons, ["cores_lost", "open"]);

    // Lost again soon after: a crash loop is left to the user.
    readiness.lost.store(true, Ordering::SeqCst);
    readiness.ready.store(false, Ordering::SeqCst);
    assert!(!h.backend.watch_service().await);
    let status = h.backend.status().await.unwrap();
    assert_eq!(status.state, ConnectionState::Ready);
    assert_eq!(status.message.as_deref(), Some(CORES_LOST));
    readiness.lost.store(false, Ordering::SeqCst);
    readiness.ready.store(true, Ordering::SeqCst);
    h.backend.watch_service().await;
    assert_eq!(
        h.backend.status().await.unwrap().state,
        ConnectionState::Ready,
        "no second automatic reconnect"
    );
}

#[tokio::test]
async fn a_disconnect_while_the_reconnect_is_pending_cancels_it() {
    let h = harness(&[PROFILE_A]);
    let readiness = enable(&h, true).await;
    h.backend.warm_up().await.unwrap();
    h.backend.connect().await.unwrap();
    readiness.lost.store(true, Ordering::SeqCst);
    readiness.ready.store(false, Ordering::SeqCst);
    assert!(!h.backend.watch_service().await);
    readiness.lost.store(false, Ordering::SeqCst);
    readiness.ready.store(true, Ordering::SeqCst);
    h.backend.disconnect().await.unwrap();
    h.backend.watch_service().await;
    let status = h.backend.status().await.unwrap();
    assert_eq!(status.state, ConnectionState::Ready);
    assert_eq!(
        status.message, None,
        "the user's disconnect clears the notice"
    );
}

#[tokio::test]
async fn a_failed_automatic_reconnect_says_to_connect_again() {
    let h = harness(&[PROFILE_A]);
    let readiness = enable(&h, true).await;
    h.backend.warm_up().await.unwrap();
    h.backend.connect().await.unwrap();
    readiness.lost.store(true, Ordering::SeqCst);
    readiness.ready.store(false, Ordering::SeqCst);
    assert!(!h.backend.watch_service().await);
    h.recording.fail_xray.store(true, Ordering::SeqCst);
    readiness.lost.store(false, Ordering::SeqCst);
    h.backend.watch_service().await;
    let status = h.backend.status().await.unwrap();
    assert_eq!(status.state, ConnectionState::Ready);
    assert_eq!(status.message.as_deref(), Some(CORES_RECONNECT_FAILED));
    let attempts = h.recording.options.lock().unwrap().len();
    h.recording.fail_xray.store(false, Ordering::SeqCst);
    h.backend.watch_service().await;
    assert_eq!(
        h.backend.status().await.unwrap().state,
        ConnectionState::Ready,
        "one attempt only"
    );
    assert!(
        h.recording
            .options
            .lock()
            .unwrap()
            .iter()
            .skip(attempts)
            .all(|options| !options.service_only || options.warm_up),
        "no further automatic connect"
    );
}

#[tokio::test]
async fn without_the_service_the_automatic_reconnect_gives_up_at_once() {
    let h = harness(&[PROFILE_A]);
    let readiness = enable(&h, true).await;
    h.backend.warm_up().await.unwrap();
    h.backend.connect().await.unwrap();
    readiness.lost.store(true, Ordering::SeqCst);
    readiness.ready.store(false, Ordering::SeqCst);
    assert!(!h.backend.watch_service().await);
    readiness.lost.store(false, Ordering::SeqCst);
    readiness.available.store(false, Ordering::SeqCst);
    let factory_calls = h.recording.options.lock().unwrap().len();
    h.backend.watch_service().await;
    let status = h.backend.status().await.unwrap();
    assert_eq!(status.state, ConnectionState::Ready);
    assert_eq!(status.message.as_deref(), Some(CORES_RECONNECT_FAILED));
    assert_eq!(
        h.recording.options.lock().unwrap().len(),
        factory_calls,
        "never an on-demand (UAC) connect"
    );
}

/// Every connection attempt becomes a session of the per-connection log index: when it began
/// and ended, why, the mode and a hash of the profile, never the profile id itself.
#[tokio::test]
async fn every_connection_is_recorded_in_the_session_index_with_why_it_ended() {
    use multicore_core::core_sessions::{profile_tag, read_sessions};
    let h = harness(&[PROFILE_A]);
    let directory = tempfile::tempdir().unwrap();
    let sessions_directory = directory.path().join("core-sessions");
    h.backend
        .enable_session_log(sessions_directory.clone())
        .await;
    let readiness = enable(&h, true).await;
    h.backend.warm_up().await.unwrap();
    assert!(
        read_sessions(&sessions_directory).is_empty(),
        "warming is no session"
    );

    h.backend.connect().await.unwrap();
    let open = read_sessions(&sessions_directory);
    assert_eq!(open.len(), 1);
    assert!(open[0].is_open());
    h.backend.disconnect().await.unwrap();

    h.recording.fail_xray.store(true, Ordering::SeqCst);
    assert!(h.backend.connect().await.is_err());
    h.recording.fail_xray.store(false, Ordering::SeqCst);

    h.backend.connect().await.unwrap();
    readiness.lost.store(true, Ordering::SeqCst);
    readiness.ready.store(false, Ordering::SeqCst);
    assert!(!h.backend.watch_service().await);
    readiness.lost.store(false, Ordering::SeqCst);
    readiness.ready.store(true, Ordering::SeqCst);

    h.backend.connect().await.unwrap();
    h.backend.shutdown_cores().await;

    let sessions = read_sessions(&sessions_directory);
    let reasons = sessions
        .iter()
        .map(|session| session.reason.as_deref().unwrap_or("open"))
        .collect::<Vec<_>>();
    assert_eq!(reasons, ["user", "failed", "cores_lost", "quit"]);
    let tag = profile_tag(h.profiles[0].as_str());
    for session in &sessions {
        assert!(
            session
                .disconnect_ms
                .is_some_and(|end| end >= session.connect_ms)
        );
        assert_eq!(session.start.mode, "tun");
        assert!(session.start.service);
        assert_eq!(session.start.engines, ["xray", "mihomo"]);
        assert_eq!(session.start.profile.as_deref(), Some(tag.as_str()));
    }
    assert!(
        sessions
            .windows(2)
            .all(|pair| pair[0].connect_ms <= pair[1].connect_ms)
    );
    let index = std::fs::read_to_string(sessions_directory.join("sessions.jsonl")).unwrap();
    assert!(!index.contains(h.profiles[0].as_str()), "{index}");
}

#[tokio::test]
async fn a_subscription_switch_ends_the_session_and_the_reconnect_starts_a_new_one() {
    use multicore_core::core_sessions::read_sessions;
    let h = harness(&[PROFILE_A, PROFILE_B]);
    let directory = tempfile::tempdir().unwrap();
    h.backend
        .enable_session_log(directory.path().to_path_buf())
        .await;
    enable(&h, true).await;
    h.backend.warm_up().await.unwrap();
    h.backend.connect().await.unwrap();
    let active = h.backend.status().await.unwrap().active_profile_id;
    let other = if active.as_deref() == Some(h.profiles[0].as_str()) {
        h.profiles[1].clone()
    } else {
        h.profiles[0].clone()
    };
    Backend::activate_subscription(&*h.backend, other.clone())
        .await
        .unwrap();
    let sessions = read_sessions(directory.path());
    assert_eq!(sessions.len(), 2, "{sessions:?}");
    assert_eq!(sessions[0].reason.as_deref(), Some("switch"));
    assert!(sessions[1].is_open());
    assert_eq!(
        sessions[1].start.profile,
        Some(multicore_core::core_sessions::profile_tag(other.as_str()))
    );
}

#[tokio::test]
async fn switching_while_connected_moves_the_connection_to_the_new_generation() {
    let h = harness(&[PROFILE_A, PROFILE_B]);
    enable(&h, true).await;
    h.backend.warm_up().await.unwrap();
    h.backend.connect().await.unwrap();
    let active = h.backend.status().await.unwrap().active_profile_id;
    let (other, letter) = if active.as_deref() == Some(h.profiles[0].as_str()) {
        (h.profiles[1].clone(), "b")
    } else {
        (h.profiles[0].clone(), "a")
    };
    let status = Backend::activate_subscription(&*h.backend, other.clone())
        .await
        .unwrap();
    assert_eq!(status.state, ConnectionState::Connected);
    assert_eq!(status.active_profile_id.as_deref(), Some(other.as_str()));
    assert_eq!(h.seen.lock().unwrap().last(), Some(&letter));
    assert_eq!(
        h.recording.calls.lock().unwrap()[3..],
        [
            (false, Engine::Mihomo),
            (false, Engine::Xray),
            (true, Engine::Xray),
            (true, Engine::Mihomo),
        ]
    );
}

#[tokio::test]
async fn without_the_service_switching_while_connected_is_still_refused() {
    let h = harness(&[PROFILE_A, PROFILE_B]);
    h.backend.connect().await.unwrap();
    let active = h.backend.status().await.unwrap().active_profile_id;
    let other = h
        .profiles
        .iter()
        .find(|id| active.as_deref() != Some(id.as_str()))
        .unwrap()
        .clone();
    assert!(
        Backend::activate_subscription(&*h.backend, other)
            .await
            .is_err()
    );
    assert_eq!(
        h.backend.status().await.unwrap().state,
        ConnectionState::Connected
    );
}

#[tokio::test]
async fn deleting_the_last_subscription_releases_the_service_cores() {
    let h = harness(&[PROFILE_A]);
    let readiness = enable(&h, true).await;
    h.backend.warm_up().await.unwrap();
    Backend::delete_subscription(&*h.backend, h.profiles[0].clone())
        .await
        .unwrap();
    assert_eq!(
        h.backend.status().await.unwrap().state,
        ConnectionState::Empty
    );
    assert!(h.backend.watch_service().await);
    assert_eq!(readiness.released.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn quit_disconnects_releases_the_cores_and_stops_warming() {
    let h = harness(&[PROFILE_A]);
    let readiness = enable(&h, true).await;
    h.backend.warm_up().await.unwrap();
    h.backend.connect().await.unwrap();
    Backend::shutdown(&*h.backend).await.unwrap();
    assert_eq!(readiness.released.load(Ordering::SeqCst), 1);
    assert_eq!(
        h.backend.status().await.unwrap().state,
        ConnectionState::Ready
    );
    let calls = h.recording.calls.lock().unwrap().len();
    h.backend.warm_up().await.unwrap();
    assert!(h.backend.watch_service().await);
    assert_eq!(
        h.recording.calls.lock().unwrap().len(),
        calls,
        "never warmed again"
    );
}

struct RecordingUpdates(
    Mutex<Vec<String>>,
    Option<multicore_daemon::service_client::ServiceError>,
);

impl multicore_daemon::ServiceUpdates for RecordingUpdates {
    fn update(&self, version: &str) -> Result<(), multicore_daemon::service_client::ServiceError> {
        self.0.lock().unwrap().push(version.to_owned());
        self.1.map_or(Ok(()), Err)
    }

    fn stage(&self, version: &str) -> Result<(), multicore_daemon::service_client::ServiceError> {
        self.0.lock().unwrap().push(format!("stage {version}"));
        self.1.map_or(Ok(()), Err)
    }
}

async fn post_update(
    updates: Option<Arc<RecordingUpdates>>,
    body: &'static str,
) -> axum::http::StatusCode {
    post_service_update("/v1/service/update", updates, body).await
}

async fn post_service_update(
    path: &'static str,
    updates: Option<Arc<RecordingUpdates>>,
    body: &'static str,
) -> axum::http::StatusCode {
    use axum::{body::Body, http::Request};
    use tower::ServiceExt;
    let h = harness(&[PROFILE_A]);
    let backend: Arc<dyn Backend> = h.backend.clone();
    let updates = updates.map(|updates| updates as Arc<dyn multicore_daemon::ServiceUpdates>);
    let app = multicore_daemon::try_router_shared(backend, "t", None, updates).unwrap();
    app.oneshot(
        Request::post(path)
            .header("authorization", "Bearer t")
            .header("content-type", "application/json")
            .body(Body::from(body))
            .unwrap(),
    )
    .await
    .unwrap()
    .status()
}

#[tokio::test]
async fn service_updates_are_routed_to_the_service() {
    let updates = Arc::new(RecordingUpdates(Mutex::default(), None));
    assert_eq!(
        post_update(Some(updates.clone()), r#"{"version":"1.2.0"}"#).await,
        202
    );
    assert_eq!(*updates.0.lock().unwrap(), ["1.2.0"]);
    assert_eq!(
        post_update(Some(updates.clone()), r#"{"version":"1.2.0","x":1}"#).await,
        400
    );
    assert_eq!(post_update(None, r#"{"version":"1.2.0"}"#).await, 404);
    let busy = Arc::new(RecordingUpdates(
        Mutex::default(),
        Some(multicore_daemon::service_client::ServiceError::Busy),
    ));
    assert_eq!(post_update(Some(busy), r#"{"version":"1.2.0"}"#).await, 409);
}

#[tokio::test]
async fn staging_an_update_only_downloads_through_the_service() {
    use multicore_daemon::service_client::ServiceError;
    const STAGE: &str = "/v1/service/update/stage";
    let updates = Arc::new(RecordingUpdates(Mutex::default(), None));
    assert_eq!(
        post_service_update(STAGE, Some(updates.clone()), r#"{"version":"1.2.0"}"#).await,
        200
    );
    assert_eq!(*updates.0.lock().unwrap(), ["stage 1.2.0"]);
    assert_eq!(
        post_service_update(STAGE, Some(updates.clone()), r#"{"version":"1.2.0","x":1}"#).await,
        400
    );
    assert_eq!(
        post_service_update(STAGE, None, r#"{"version":"1.2.0"}"#).await,
        404
    );
    for (error, status) in [
        (ServiceError::Busy, 409),
        (ServiceError::Unavailable, 404),
        (ServiceError::TimedOut, 502),
    ] {
        let updates = Arc::new(RecordingUpdates(Mutex::default(), Some(error)));
        assert_eq!(
            post_service_update(STAGE, Some(updates), r#"{"version":"1.2.0"}"#).await,
            status,
            "{error:?}"
        );
    }
}

#[tokio::test]
async fn a_missing_service_and_a_failed_download_are_told_apart() {
    use multicore_core::elevation_protocol::BrokerErrorCode;
    use multicore_daemon::service_client::ServiceError;
    for (error, status) in [
        (ServiceError::NotInstalled, 404),
        (ServiceError::Unavailable, 404),
        (ServiceError::Rejected(BrokerErrorCode::NotReady), 502),
        (ServiceError::TimedOut, 502),
    ] {
        let updates = Arc::new(RecordingUpdates(Mutex::default(), Some(error)));
        assert_eq!(
            post_update(Some(updates), r#"{"version":"1.2.0"}"#).await,
            status,
            "{error:?}"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 1)]
async fn status_answers_while_the_controller_factory_blocks() {
    let h = harness(&[PROFILE_A]);
    enable(&h, true).await;
    let (release, gate) = std::sync::mpsc::channel();
    *h.recording.factory_gate.lock().unwrap() = Some(gate);
    let connecting = {
        let backend = h.backend.clone();
        tokio::spawn(async move { backend.connect().await })
    };
    while !h.recording.factory_entered.load(Ordering::SeqCst) {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    // The only worker is inside the blocked factory; a status request must still run.
    let status = {
        let backend = h.backend.clone();
        tokio::spawn(async move { backend.status().await })
    };
    tokio::time::timeout(PROMPT, status)
        .await
        .expect("status does not wait for the blocked factory")
        .unwrap()
        .unwrap();
    release.send(()).unwrap();
    connecting.await.unwrap().unwrap();
}

#[tokio::test]
async fn a_new_connection_is_checked_by_the_watcher_at_once() {
    let h = harness(&[PROFILE_A]);
    enable(&h, true).await;
    h.backend.warm_up().await.unwrap();
    let warm = h.backend.warm_requests();
    let _ = tokio::time::timeout(Duration::from_millis(50), warm.notified()).await;
    h.backend.connect().await.unwrap();
    tokio::time::timeout(PROMPT, warm.notified())
        .await
        .expect("a connect wakes the watcher");
    assert!(h.backend.is_connected().await);
}

#[tokio::test]
async fn the_unavailable_notice_is_cleared_once_the_service_is_back() {
    let h = harness(&[PROFILE_A]);
    h.backend
        .record_notice(
            "service_unavailable",
            DaemonText::code("service.unavailable_notice"),
        )
        .await;
    h.backend
        .clear_notice(&DaemonText::code("cores.lost"))
        .await;
    assert_eq!(
        h.backend.status().await.unwrap().message.as_deref(),
        Some("Служба MULTICORE недоступна — права администратора будут запрошены при подключении.")
    );
    h.backend
        .clear_notice(&DaemonText::code("service.unavailable_notice"))
        .await;
    assert_eq!(h.backend.status().await.unwrap().message, None);
}

#[tokio::test]
async fn a_connect_during_a_switch_waits_and_ends_on_the_new_profile() {
    let h = harness(&[PROFILE_A, PROFILE_B]);
    enable(&h, true).await;
    h.backend.warm_up().await.unwrap();
    h.backend.connect().await.unwrap();
    let active = h.backend.status().await.unwrap().active_profile_id;
    let (other, letter) = if active.as_deref() == Some(h.profiles[0].as_str()) {
        (h.profiles[1].clone(), "b")
    } else {
        (h.profiles[0].clone(), "a")
    };
    // The switch's disconnect is held while the tunnel goes down.
    let gate = Arc::new(tokio::sync::Notify::new());
    *h.recording.stop_gate.lock().unwrap() = Some(gate.clone());
    let switching = {
        let backend = h.backend.clone();
        let other = other.clone();
        tokio::spawn(async move { Backend::activate_subscription(&*backend, other).await })
    };
    while !h.recording.stop_entered.load(Ordering::SeqCst) {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    // The user presses Connect in the middle of the switch.
    let connecting = {
        let backend = h.backend.clone();
        tokio::spawn(async move { backend.connect().await })
    };
    tokio::time::sleep(Duration::from_millis(50)).await;
    gate.notify_one();
    let switched = tokio::time::timeout(PROMPT, switching)
        .await
        .expect("the switch finishes")
        .unwrap();
    let connected = tokio::time::timeout(PROMPT, connecting)
        .await
        .expect("the Connect finishes")
        .unwrap();
    let switched = switched.expect("the switch is not broken by the Connect");
    let connected = connected.expect("the Connect is answered with the switched connection");
    assert_eq!(switched.active_profile_id.as_deref(), Some(other.as_str()));
    assert_eq!(connected.state, ConnectionState::Connected);
    assert_eq!(connected.active_profile_id.as_deref(), Some(other.as_str()));
    let status = h.backend.status().await.unwrap();
    assert_eq!(status.state, ConnectionState::Connected);
    assert_eq!(status.active_profile_id.as_deref(), Some(other.as_str()));
    assert_eq!(h.seen.lock().unwrap().last(), Some(&letter));
}

#[tokio::test]
async fn a_disconnect_during_a_switch_is_applied_after_it() {
    let h = harness(&[PROFILE_A, PROFILE_B]);
    enable(&h, true).await;
    h.backend.warm_up().await.unwrap();
    h.backend.connect().await.unwrap();
    let active = h.backend.status().await.unwrap().active_profile_id;
    let other = h
        .profiles
        .iter()
        .find(|id| active.as_deref() != Some(id.as_str()))
        .unwrap()
        .clone();
    let gate = Arc::new(tokio::sync::Notify::new());
    *h.recording.stop_gate.lock().unwrap() = Some(gate.clone());
    let switching = {
        let backend = h.backend.clone();
        let other = other.clone();
        tokio::spawn(async move { Backend::activate_subscription(&*backend, other).await })
    };
    while !h.recording.stop_entered.load(Ordering::SeqCst) {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let disconnecting = {
        let backend = h.backend.clone();
        tokio::spawn(async move { backend.disconnect().await })
    };
    tokio::time::sleep(Duration::from_millis(50)).await;
    gate.notify_one();
    switching.await.unwrap().unwrap();
    let status = disconnecting.await.unwrap().unwrap();
    // The user's last word wins: no connection is resumed behind the Disconnect.
    assert_eq!(status.state, ConnectionState::Ready);
    let status = h.backend.status().await.unwrap();
    assert_eq!(status.state, ConnectionState::Ready);
    assert_eq!(status.active_profile_id.as_deref(), Some(other.as_str()));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_warm_up_superseded_by_a_failed_connect_is_not_reported_as_failed() {
    let h = harness(&[PROFILE_A]);
    enable(&h, false).await;
    let (release, gate) = std::sync::mpsc::channel();
    *h.recording.factory_gate.lock().unwrap() = Some(gate);
    let warming = {
        let backend = h.backend.clone();
        tokio::spawn(async move { backend.warm_up().await })
    };
    while !h.recording.factory_entered.load(Ordering::SeqCst) {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    // A Connect takes the session and fails; the warm-up's Prepare is then refused.
    h.recording.fail_xray.store(true, Ordering::SeqCst);
    assert!(h.backend.connect().await.is_err());
    assert_eq!(
        h.backend.status().await.unwrap().state,
        ConnectionState::Ready
    );
    release.send(()).unwrap();
    assert!(
        warming.await.unwrap().is_ok(),
        "a superseded warm-up does not make the watcher back off"
    );
    let events = h.backend.events_after(0).await.unwrap();
    assert!(
        events.iter().all(|event| event.message != WARM_UP_FAILED),
        "a refused warm-up is not a warm-up failure"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_late_warm_up_after_a_connection_cycle_prepares_nothing_and_asks_again() {
    let h = harness(&[PROFILE_A]);
    enable(&h, false).await;
    let warm = h.backend.warm_requests();
    // A warm-up stuck in its service probe...
    let (release, gate) = std::sync::mpsc::channel();
    *h.recording.factory_gate.lock().unwrap() = Some(gate);
    let warming = {
        let backend = h.backend.clone();
        tokio::spawn(async move { backend.warm_up().await })
    };
    while !h.recording.factory_entered.load(Ordering::SeqCst) {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    // ...outlives a whole Connect and Disconnect.
    h.backend.connect().await.unwrap();
    h.backend.disconnect().await.unwrap();
    let _ = tokio::time::timeout(Duration::from_millis(50), warm.notified()).await;
    release.send(()).unwrap();
    warming.await.unwrap().unwrap();
    assert_eq!(
        *h.recording.calls.lock().unwrap(),
        [
            (true, Engine::Xray),
            (true, Engine::Mihomo),
            (false, Engine::Mihomo),
            (false, Engine::Xray),
        ],
        "the stale warm-up does not Prepare its old generation"
    );
    tokio::time::timeout(PROMPT, warm.notified())
        .await
        .expect("a fresh warm-up is requested at once");
}

async fn import(h: &Harness, url: &str) -> Result<multicore_daemon::StatusDto, BackendError> {
    Backend::import_subscription(&*h.backend, ImportSubscriptionRequest { url: url.into() }).await
}

async fn profile_count(h: &Harness) -> usize {
    Backend::list_subscriptions(&*h.backend)
        .await
        .unwrap()
        .profiles
        .len()
}

#[tokio::test]
async fn adding_a_subscription_while_connected_moves_the_connection_to_it() {
    let h = harness(&[PROFILE_A]);
    enable(&h, true).await;
    h.backend.warm_up().await.unwrap();
    h.backend.connect().await.unwrap();
    let before = h.recording.calls.lock().unwrap().len();

    let status = import(&h, NEW_SUBSCRIPTION).await.unwrap();

    assert_eq!(status.state, ConnectionState::Connected);
    let added = status
        .imported_profile_id
        .clone()
        .expect("the added profile");
    assert_ne!(added, h.profiles[0].as_str());
    assert_eq!(status.active_profile_id.as_deref(), Some(added.as_str()));
    assert_eq!(h.seen.lock().unwrap().last(), Some(&"c"));
    assert_eq!(
        h.recording.calls.lock().unwrap()[before..],
        [
            (false, Engine::Mihomo),
            (false, Engine::Xray),
            (true, Engine::Xray),
            (true, Engine::Mihomo),
        ]
    );
    assert_eq!(profile_count(&h).await, 2);
    let status = h.backend.status().await.unwrap();
    assert_eq!(status.state, ConnectionState::Connected);
    assert_eq!(status.active_profile_id.as_deref(), Some(added.as_str()));
}

#[tokio::test]
async fn without_the_service_a_subscription_added_while_connected_waits_in_the_list() {
    let h = harness(&[PROFILE_A]);
    h.backend.connect().await.unwrap();
    let before = h.recording.calls.lock().unwrap().clone();

    let status = import(&h, NEW_SUBSCRIPTION).await.unwrap();

    assert_eq!(status.state, ConnectionState::Connected);
    assert_eq!(
        status.active_profile_id.as_deref(),
        Some(h.profiles[0].as_str())
    );
    let added = status
        .imported_profile_id
        .clone()
        .expect("the added profile");
    assert_ne!(added, h.profiles[0].as_str());
    assert_eq!(
        *h.recording.calls.lock().unwrap(),
        before,
        "the connection is left alone"
    );
    let list = Backend::list_subscriptions(&*h.backend).await.unwrap();
    assert_eq!(list.profiles.len(), 2);
    assert_eq!(
        list.active_profile_id.as_deref(),
        Some(h.profiles[0].as_str())
    );

    // After a disconnect the added subscription is one click away.
    h.backend.disconnect().await.unwrap();
    let switched = Backend::activate_subscription(&*h.backend, ProfileId::parse(&added).unwrap())
        .await
        .unwrap();
    assert_eq!(switched.active_profile_id.as_deref(), Some(added.as_str()));
}

#[tokio::test]
async fn a_failed_import_while_connected_keeps_the_connection() {
    for service in [true, false] {
        let h = harness(&[PROFILE_A]);
        if service {
            enable(&h, true).await;
            h.backend.warm_up().await.unwrap();
        }
        h.backend.connect().await.unwrap();
        let before = h.recording.calls.lock().unwrap().clone();

        let error = import(&h, FAILING_SUBSCRIPTION).await.unwrap_err();

        assert!(
            matches!(
                error.profile_mutation(),
                Some(ProfileMutationError::Fetch(FetchError::Network))
            ),
            "service={service}: {error:?}"
        );
        assert_eq!(
            *h.recording.calls.lock().unwrap(),
            before,
            "service={service}"
        );
        let status = h.backend.status().await.unwrap();
        assert_eq!(
            status.state,
            ConnectionState::Connected,
            "service={service}"
        );
        assert_eq!(
            status.active_profile_id.as_deref(),
            Some(h.profiles[0].as_str())
        );
        assert_eq!(profile_count(&h).await, 1, "service={service}");
    }
}

#[tokio::test]
async fn re_adding_the_connected_subscription_refreshes_it_in_place() {
    let h = harness(&[]);
    enable(&h, true).await;
    let first = import(&h, NEW_SUBSCRIPTION).await.unwrap();
    let id = first
        .active_profile_id
        .clone()
        .expect("first import activates");
    h.backend.warm_up().await.unwrap();
    h.backend.connect().await.unwrap();

    let status = import(&h, NEW_SUBSCRIPTION).await.unwrap();

    assert_eq!(status.state, ConnectionState::Connected);
    assert_eq!(status.active_profile_id.as_deref(), Some(id.as_str()));
    assert_eq!(status.imported_profile_id.as_deref(), Some(id.as_str()));
    assert_eq!(profile_count(&h).await, 1);
}

#[tokio::test]
async fn the_first_subscription_is_activated_at_once() {
    let h = harness(&[]);
    let status = import(&h, NEW_SUBSCRIPTION).await.unwrap();
    let added = status
        .imported_profile_id
        .clone()
        .expect("the added profile");
    assert_eq!(status.active_profile_id.as_deref(), Some(added.as_str()));
    assert_eq!(status.state, ConnectionState::Ready);
}

#[tokio::test]
async fn a_connect_during_an_import_switch_waits_and_ends_on_the_new_subscription() {
    let h = harness(&[PROFILE_A]);
    enable(&h, true).await;
    h.backend.warm_up().await.unwrap();
    h.backend.connect().await.unwrap();
    let gate = Arc::new(tokio::sync::Notify::new());
    *h.recording.stop_gate.lock().unwrap() = Some(gate.clone());
    let importing = {
        let backend = h.backend.clone();
        tokio::spawn(async move {
            Backend::import_subscription(
                &*backend,
                ImportSubscriptionRequest {
                    url: NEW_SUBSCRIPTION.into(),
                },
            )
            .await
        })
    };
    while !h.recording.stop_entered.load(Ordering::SeqCst) {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let connecting = {
        let backend = h.backend.clone();
        tokio::spawn(async move { backend.connect().await })
    };
    tokio::time::sleep(Duration::from_millis(50)).await;
    gate.notify_one();
    let imported = tokio::time::timeout(PROMPT, importing)
        .await
        .expect("the import finishes")
        .unwrap()
        .expect("the import is not broken by the Connect");
    let connected = tokio::time::timeout(PROMPT, connecting)
        .await
        .expect("the Connect finishes")
        .unwrap()
        .expect("the Connect is answered with the switched connection");
    let added = imported.imported_profile_id.clone().unwrap();
    assert_eq!(imported.active_profile_id.as_deref(), Some(added.as_str()));
    assert_eq!(connected.state, ConnectionState::Connected);
    assert_eq!(connected.active_profile_id.as_deref(), Some(added.as_str()));
    assert_eq!(h.seen.lock().unwrap().last(), Some(&"c"));
}

#[tokio::test]
async fn a_refresh_that_changes_only_metadata_keeps_the_connection() {
    let h = harness(&[]);
    enable(&h, true).await;
    import(&h, NEW_SUBSCRIPTION).await.unwrap();
    h.backend.warm_up().await.unwrap();
    h.backend.connect().await.unwrap();
    let before = h.recording.calls.lock().unwrap().clone();

    // The provider serves the same configuration again: nothing to move the connection to.
    let status = Backend::refresh_subscription(&*h.backend).await.unwrap();

    assert_eq!(status.state, ConnectionState::Connected);
    assert_eq!(*h.recording.calls.lock().unwrap(), before);
    assert_eq!(
        h.backend.status().await.unwrap().state,
        ConnectionState::Connected
    );
}
