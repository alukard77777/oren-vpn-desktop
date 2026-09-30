use std::{
    collections::HashMap,
    future::Future,
    io::{Read, Write},
    net::TcpListener,
    pin::Pin,
    sync::{Arc, Mutex},
    thread,
    time::{SystemTime, UNIX_EPOCH},
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
    Backend, BackendError, CoreBackend, MihomoHttpSelector, MihomoSelector, SelectorError,
    SessionError, TrafficTotals, resolve_exit_now, router,
};
use serde_json::Value;
use tower::ServiceExt;

const TOKEN: &str = "session-token";
const MIHOMO: &str = r#"
proxies:
  - {name: "private / fast", type: socks5}
proxy-groups:
  - {name: "private group", type: select, proxies: ["private / fast"]}
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

/// Reports fixed counters, or none when `totals` is `None` (controller unreadable).
struct TrafficController {
    totals: Mutex<Option<TrafficTotals>>,
}

#[async_trait]
impl MihomoSelector for TrafficController {
    async fn select(&self, _: &str, _: &str) -> Result<(), SelectorError> {
        Ok(())
    }

    async fn traffic_totals(&self) -> Result<TrafficTotals, SelectorError> {
        self.totals.lock().unwrap().ok_or_else(SelectorError::new)
    }
}

fn backend(
    controller: Arc<dyn MihomoSelector>,
) -> CoreBackend<
    NeverHttp,
    NoopController,
    impl Fn(&Snapshot) -> Result<Arc<NoopController>, BackendError> + use<>,
> {
    let root = tempfile::tempdir().unwrap();
    let store = ProfileStore::open(root.path().join("profiles"), None).unwrap();
    store
        .create_profile(Arc::new(
            Snapshot::parse(MIHOMO.as_bytes(), br#"{}"#).unwrap(),
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

fn request(authenticated: bool) -> Request<Body> {
    let mut builder = Request::builder().method("GET").uri("/v1/session");
    if authenticated {
        builder = builder.header(header::AUTHORIZATION, format!("Bearer {TOKEN}"));
    }
    builder.body(Body::empty()).unwrap()
}

async fn json_body(response: axum::response::Response) -> Value {
    serde_json::from_slice(&response.into_body().collect().await.unwrap().to_bytes()).unwrap()
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis()
        .try_into()
        .unwrap()
}

#[tokio::test]
async fn session_requires_authentication_and_a_live_connection() {
    let controller = Arc::new(TrafficController {
        totals: Mutex::new(Some(TrafficTotals {
            download: 1,
            upload: 1,
        })),
    });
    let app = router(backend(controller), TOKEN);

    let unauthorized = app.clone().oneshot(request(false)).await.unwrap();
    assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);

    let disconnected = app.oneshot(request(true)).await.unwrap();
    assert_eq!(disconnected.status(), StatusCode::CONFLICT);
    assert_eq!(json_body(disconnected).await["code"], "not_connected");
}

#[tokio::test]
async fn connected_session_reports_start_time_and_mihomo_totals() {
    let controller = Arc::new(TrafficController {
        totals: Mutex::new(Some(TrafficTotals {
            download: 5_242_880,
            upload: 327_680,
        })),
    });
    let backend = backend(controller);
    let before = now_ms();
    backend.connect().await.unwrap();
    let after = now_ms();

    let response = router(backend, TOKEN).oneshot(request(true)).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = json_body(response).await;
    let since = body["connected_since_unix_ms"].as_u64().unwrap();
    assert!(
        (before..=after).contains(&since),
        "{since} not in {before}..={after}"
    );
    assert_eq!(body["download_total"], 5_242_880);
    assert_eq!(body["upload_total"], 327_680);
}

#[tokio::test]
async fn unreadable_counters_keep_the_session_but_omit_totals() {
    let controller = Arc::new(TrafficController {
        totals: Mutex::new(None),
    });
    let backend = backend(controller);
    backend.connect().await.unwrap();

    let session = backend.session().await.unwrap();
    assert!(session.connected_since_unix_ms > 0);
    assert_eq!(session.download_total, None);
    assert_eq!(session.upload_total, None);
}

#[tokio::test]
async fn disconnect_ends_the_session() {
    let controller = Arc::new(TrafficController {
        totals: Mutex::new(Some(TrafficTotals {
            download: 0,
            upload: 0,
        })),
    });
    let backend = backend(controller);
    backend.connect().await.unwrap();
    assert!(backend.session().await.is_ok());
    backend.disconnect().await.unwrap();
    assert_eq!(
        backend.session().await.unwrap_err(),
        SessionError::NotConnected
    );
}

#[tokio::test]
async fn http_traffic_totals_read_connections_with_an_authenticated_request() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let captured = Arc::new(Mutex::new(String::new()));
    let server_capture = captured.clone();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut request = [0_u8; 4096];
        let count = stream.read(&mut request).unwrap();
        *server_capture.lock().unwrap() = String::from_utf8_lossy(&request[..count]).into_owned();
        let body =
            r#"{"downloadTotal":1234,"uploadTotal":56,"connections":[{"id":"a"}],"memory":1}"#;
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .unwrap();
    });

    let controller = MihomoHttpSelector::new(address, "private-secret").unwrap();
    assert_eq!(
        controller.traffic_totals().await.unwrap(),
        TrafficTotals {
            download: 1234,
            upload: 56,
        }
    );
    server.join().unwrap();

    let request = captured.lock().unwrap().clone();
    assert!(
        request.starts_with("GET /connections HTTP/1.1\r\n"),
        "{request}"
    );
    assert!(
        request
            .to_ascii_lowercase()
            .contains("authorization: bearer private-secret\r\n"),
        "{request}"
    );
}

/// Answers `now` from a fixed table; names missing from it are unreadable proxies.
struct NowController {
    now: HashMap<&'static str, Option<&'static str>>,
}

#[async_trait]
impl MihomoSelector for NowController {
    async fn select(&self, _: &str, _: &str) -> Result<(), SelectorError> {
        Ok(())
    }

    async fn proxy_now(&self, raw_name: &str) -> Result<Option<String>, SelectorError> {
        self.now
            .get(raw_name)
            .map(|now| now.map(str::to_owned))
            .ok_or_else(SelectorError::new)
    }
}

fn now_controller(entries: &[(&'static str, Option<&'static str>)]) -> NowController {
    NowController {
        now: entries.iter().copied().collect(),
    }
}

#[tokio::test]
async fn exit_now_follows_group_chains_to_the_node() {
    let controller = now_controller(&[
        ("Автовыбор", Some("Europe")),
        ("Europe", Some("🇸🇪 Sweden")),
        ("🇸🇪 Sweden", None),
    ]);
    assert_eq!(
        resolve_exit_now(&controller, "Автовыбор").await.as_deref(),
        Some("🇸🇪 Sweden")
    );
    // A plain node is its own exit: nothing extra to show.
    assert_eq!(resolve_exit_now(&controller, "🇸🇪 Sweden").await, None);
}

#[tokio::test]
async fn exit_now_gives_up_on_cycles_and_unreadable_proxies() {
    let cycle = now_controller(&[("A", Some("B")), ("B", Some("A"))]);
    assert_eq!(resolve_exit_now(&cycle, "A").await, None);

    // A node the controller will not describe (a leaf whose name the filtered proxy refuses)
    // is still the exit; only an unreadable starting point gives up.
    let leaf_unreadable = now_controller(&[("Автовыбор", Some("gone"))]);
    assert_eq!(
        resolve_exit_now(&leaf_unreadable, "Автовыбор")
            .await
            .as_deref(),
        Some("gone")
    );
    assert_eq!(resolve_exit_now(&leaf_unreadable, "unknown").await, None);

    let chain: Vec<(&'static str, Option<&'static str>)> = vec![
        ("g0", Some("g1")),
        ("g1", Some("g2")),
        ("g2", Some("g3")),
        ("g3", Some("g4")),
        ("g4", Some("g5")),
        ("g5", Some("g6")),
        ("g6", Some("g7")),
        ("g7", Some("g8")),
        ("g8", Some("g9")),
        ("g9", None),
    ];
    assert_eq!(resolve_exit_now(&now_controller(&chain), "g0").await, None);
}

#[tokio::test]
async fn session_reports_the_node_behind_an_automatic_exit() {
    let controller = Arc::new(now_controller(&[
        ("private / fast", Some("🇸🇪 Sweden")),
        ("🇸🇪 Sweden", None),
    ]));
    let backend = backend(controller);
    backend.connect().await.unwrap();

    let response = router(backend, TOKEN).oneshot(request(true)).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(json_body(response).await["exit_now"], "🇸🇪 Sweden");
}

#[tokio::test]
async fn session_omits_exit_now_for_a_plain_node_exit() {
    let controller = Arc::new(now_controller(&[("private / fast", None)]));
    let backend = backend(controller);
    backend.connect().await.unwrap();

    let response = router(backend, TOKEN).oneshot(request(true)).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert!(json_body(response).await.get("exit_now").is_none());
}

#[tokio::test]
async fn http_proxy_now_reads_the_group_with_an_authenticated_request() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let captured = Arc::new(Mutex::new(String::new()));
    let server_capture = captured.clone();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut request = [0_u8; 4096];
        let count = stream.read(&mut request).unwrap();
        *server_capture.lock().unwrap() = String::from_utf8_lossy(&request[..count]).into_owned();
        let body = r#"{"name":"auto","type":"URLTest","now":"sweden","all":["sweden"]}"#;
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .unwrap();
    });

    let controller = MihomoHttpSelector::new(address, "private-secret").unwrap();
    assert_eq!(
        controller.proxy_now("auto").await.unwrap().as_deref(),
        Some("sweden")
    );
    server.join().unwrap();

    let request = captured.lock().unwrap().clone();
    assert!(
        request.starts_with("GET /proxies/auto HTTP/1.1\r\n"),
        "{request}"
    );
    assert!(
        request
            .to_ascii_lowercase()
            .contains("authorization: bearer private-secret\r\n"),
        "{request}"
    );
}
