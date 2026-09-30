use std::{
    future::Future,
    pin::Pin,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
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

pub struct Readiness(pub AtomicBool);

impl CoresReadiness for Readiness {
    fn cores_ready(&self) -> bool {
        self.0.load(Ordering::SeqCst)
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
    let readiness = Arc::new(Readiness(AtomicBool::new(ready)));
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
    readiness.0.store(false, Ordering::SeqCst);
    assert!(!h.backend.status().await.unwrap().cores_ready);
}

#[tokio::test]
async fn disconnect_keeps_the_cores_warm() {
    let h = harness(&[PROFILE_A]);
    enable(&h, true).await;
    h.backend.warm_up().await.unwrap();
    h.backend.connect().await.unwrap();
    h.backend.disconnect().await.unwrap();
    assert_eq!(
        *h.recording.calls.lock().unwrap(),
        [
            (true, Engine::Xray),
            (true, Engine::Xray),
            (true, Engine::Mihomo),
            (false, Engine::Mihomo),
            (false, Engine::Xray),
            (true, Engine::Xray),
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
    Backend::activate_subscription(&*h.backend, h.profiles[1].clone())
        .await
        .unwrap();
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
    let app = multicore_daemon::try_router_shared(backend, "shared-token", None).unwrap();
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
