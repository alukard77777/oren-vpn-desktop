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
    Snapshot, SubscriptionFetcher,
};
use multicore_daemon::{
    Backend, BackendError, ConnectOptions, ConnectionState, CoreBackend, CoresReadiness,
    MihomoSelector, OptionsControllerFactory, PreparedController, SelectorError,
    options_controller_factory,
};

pub const PROFILE_A: &str = "proxies:\n  - {name: node-a, type: socks5}\nproxy-groups:\n  - {name: g, type: select, proxies: [node-a]}\n";
pub const PROFILE_B: &str = "proxies:\n  - {name: node-b, type: socks5}\nproxy-groups:\n  - {name: g, type: select, proxies: [node-b]}\n";
const WARM_UP_FAILED: &str = "Ядра не удалось запустить заранее. Они запустятся при подключении.";
const CORES_LOST: &str =
    "Служба MultiCore потеряла ядра — подключение разорвано. Подключитесь снова.";
const PROMPT: Duration = Duration::from_secs(2);

pub struct NeverHttp;

impl HttpClient for NeverHttp {
    fn get<'a>(
        &'a self,
        _: &'a str,
        _: &'static str,
    ) -> Pin<Box<dyn Future<Output = Result<HttpResponse, FetchError>> + Send + 'a>> {
        Box::pin(async { Err(FetchError::Network) })
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
    pub backend: Arc<CoreBackend<NeverHttp, RecordingController, Factory>>,
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
    let factory: FactoryFn = Box::new(move |snapshot: &Snapshot, _options: ConnectOptions| {
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
            } else {
                "a"
            });
        Ok(PreparedController::reuse(controller.clone()))
    });
    let selector = Arc::new(QuietSelector::default());
    let backend = CoreBackend::new_with_connect_options(
        store,
        SubscriptionFetcher::new(NeverHttp),
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
        .record_notice("service_unavailable", "Служба MultiCore недоступна.")
        .await;
    assert_eq!(
        h.backend.status().await.unwrap().message.as_deref(),
        Some("Служба MultiCore недоступна.")
    );
    assert!(
        h.backend
            .events_after(0)
            .await
            .unwrap()
            .iter()
            .any(|event| event.message == "Служба MultiCore недоступна.")
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
    assert_eq!(status.message.as_deref(), Some(CORES_LOST));
    assert!(
        h.backend
            .events_after(0)
            .await
            .unwrap()
            .iter()
            .any(|event| event.message == CORES_LOST)
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
}

async fn post_update(
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
        Request::post("/v1/service/update")
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
        .record_notice("service_unavailable", "Служба MultiCore недоступна.")
        .await;
    h.backend.clear_notice("другое сообщение").await;
    assert_eq!(
        h.backend.status().await.unwrap().message.as_deref(),
        Some("Служба MultiCore недоступна.")
    );
    h.backend.clear_notice("Служба MultiCore недоступна.").await;
    assert_eq!(h.backend.status().await.unwrap().message, None);
}
