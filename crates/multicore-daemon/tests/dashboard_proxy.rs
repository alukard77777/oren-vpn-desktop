use std::{
    net::SocketAddr,
    sync::{Arc, Mutex},
};

use axum::{
    Json, Router,
    body::{Body, to_bytes},
    extract::Request,
    http::{Method, StatusCode},
};
use multicore_daemon::dashboard_proxy::{dashboard_router, dashboard_url, is_allowed};
use multicore_daemon::service_client::ControllerSlot;
use serde_json::json;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};
use tower::ServiceExt;

const TOKEN: &str = "dash-token";

async fn fake_mihomo() -> (SocketAddr, Arc<Mutex<Vec<String>>>) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let record = seen.clone();
    let app = Router::new().fallback(move |request: Request| {
        let record = record.clone();
        async move {
            let authorization = request
                .headers()
                .get("authorization")
                .and_then(|value| value.to_str().ok())
                .unwrap_or("")
                .to_owned();
            record.lock().unwrap().push(format!(
                "{} {} {authorization}",
                request.method(),
                request.uri()
            ));
            Json(json!({ "version": "v1.19.30" }))
        }
    });
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (address, seen)
}

fn router(upstream: Option<SocketAddr>, root: &std::path::Path) -> Router {
    let slot = Arc::new(ControllerSlot::default());
    if let Some(address) = upstream {
        slot.set(address, "mihomo-secret", None);
    }
    dashboard_router(root.to_path_buf(), Arc::from(TOKEN), slot)
}

async fn call(
    app: Router,
    method: Method,
    uri: &str,
    bearer: Option<&str>,
) -> (StatusCode, String) {
    let mut request = axum::http::Request::builder().method(method).uri(uri);
    if let Some(token) = bearer {
        request = request.header("authorization", format!("Bearer {token}"));
    }
    let response = app
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let body = to_bytes(response.into_body(), 1 << 20).await.unwrap();
    (status, String::from_utf8_lossy(&body).into_owned())
}

#[test]
fn only_reads_of_the_documented_routes_are_allowed() {
    for path in [
        "/version",
        "/logs",
        "/rules",
        "/providers/rules",
        "/configs",
        "/connections",
        "/traffic",
        "/memory",
    ] {
        assert!(is_allowed(&Method::GET, path), "{path}");
    }
    for (method, path) in [
        (Method::PUT, "/configs"),
        (Method::PATCH, "/configs"),
        (Method::POST, "/configs/geo"),
        (Method::GET, "/proxies"),
        (Method::PUT, "/proxies/GLOBAL"),
        (Method::DELETE, "/connections"),
        (Method::DELETE, "/connections/abc"),
        (Method::GET, "/connections/abc"),
        (Method::DELETE, "/connections/smart/abc"),
        (Method::PUT, "/connections"),
        (Method::POST, "/connections"),
        (Method::PATCH, "/connections"),
        (Method::DELETE, "/traffic"),
        (Method::POST, "/memory"),
        (Method::POST, "/upgrade"),
        (Method::POST, "/upgrade/ui"),
        (Method::PATCH, "/rules/disable"),
        (Method::PUT, "/providers/rules/ads"),
        (Method::GET, "/providers/rules/ads"),
        (Method::GET, "/providers/proxies"),
        (Method::POST, "/restart"),
        (Method::POST, "/cache/fakeip/flush"),
        (Method::GET, "/debug/pprof"),
    ] {
        assert!(!is_allowed(&method, path), "{method} {path}");
    }
}

#[test]
fn the_dashboard_url_points_setup_at_the_proxy_itself() {
    assert_eq!(
        dashboard_url(41000, TOKEN),
        "http://127.0.0.1:41000/ui/#/setup?hostname=127.0.0.1&port=41000&secret=dash-token"
    );
}

#[tokio::test]
async fn allowed_reads_are_forwarded_with_the_mihomo_secret() {
    let (address, seen) = fake_mihomo().await;
    let root = tempfile::tempdir().unwrap();
    let (status, body) = call(
        router(Some(address), root.path()),
        Method::GET,
        "/version",
        Some(TOKEN),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("v1.19.30"));
    let (status, _) = call(
        router(Some(address), root.path()),
        Method::GET,
        "/rules?token=dash-token&x=1",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = call(
        router(Some(address), root.path()),
        Method::GET,
        "/connections",
        Some(TOKEN),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        *seen.lock().unwrap(),
        [
            "GET /version Bearer mihomo-secret".to_owned(),
            "GET /rules?x=1 Bearer mihomo-secret".to_owned(),
            "GET /connections Bearer mihomo-secret".to_owned(),
        ]
    );
}

#[tokio::test]
async fn writes_and_other_routes_are_forbidden_without_reaching_mihomo() {
    let (address, seen) = fake_mihomo().await;
    let root = tempfile::tempdir().unwrap();
    for (method, path) in [
        (Method::PUT, "/configs"),
        (Method::PATCH, "/configs"),
        (Method::GET, "/proxies"),
        (Method::DELETE, "/connections"),
        (Method::DELETE, "/connections/abc"),
        (Method::DELETE, "/connections?token=dash-token"),
        (Method::POST, "/upgrade"),
        (Method::PATCH, "/rules/disable"),
    ] {
        let (status, _) = call(
            router(Some(address), root.path()),
            method.clone(),
            path,
            Some(TOKEN),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{method} {path}");
    }
    assert!(seen.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_missing_or_wrong_token_is_unauthorized() {
    let (address, seen) = fake_mihomo().await;
    let root = tempfile::tempdir().unwrap();
    for bearer in [None, Some("wrong")] {
        let (status, _) = call(
            router(Some(address), root.path()),
            Method::GET,
            "/version",
            bearer,
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }
    assert!(seen.lock().unwrap().is_empty());
}

#[tokio::test]
async fn without_a_controller_the_api_is_unavailable() {
    let root = tempfile::tempdir().unwrap();
    let (status, _) = call(
        router(None, root.path()),
        Method::GET,
        "/version",
        Some(TOKEN),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn static_files_are_served_without_escaping_the_directory() {
    let root = tempfile::tempdir().unwrap();
    let ui = root.path().join("ui");
    std::fs::create_dir_all(ui.join("assets")).unwrap();
    std::fs::write(ui.join("index.html"), "<html>zashboard</html>").unwrap();
    std::fs::write(ui.join("assets/app-1.js"), "console.log(1)").unwrap();
    std::fs::write(root.path().join("secret.txt"), "outside").unwrap();
    let app = router(None, &ui);
    let (status, body) = call(app.clone(), Method::GET, "/ui/", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.contains("zashboard"));
    let (status, body) = call(app.clone(), Method::GET, "/ui/assets/app-1.js", None).await;
    assert_eq!((status, body.as_str()), (StatusCode::OK, "console.log(1)"));
    for escape in [
        "/ui/../secret.txt",
        "/ui/..%2Fsecret.txt",
        "/ui/assets/%2e%2e/%2e%2e/secret.txt",
    ] {
        let (status, _) = call(app.clone(), Method::GET, escape, None).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{escape}");
    }
    let (status, _) = call(app, Method::GET, "/ui", None).await;
    assert_eq!(status, StatusCode::PERMANENT_REDIRECT);
}

#[tokio::test]
async fn plain_stream_requests_without_an_upgrade_are_rejected() {
    let (address, seen) = fake_mihomo().await;
    let root = tempfile::tempdir().unwrap();
    for path in ["/logs", "/traffic", "/memory"] {
        let (status, _) = call(
            router(Some(address), root.path()),
            Method::GET,
            path,
            Some(TOKEN),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{path}");
    }
    assert!(seen.lock().unwrap().is_empty());
}

#[tokio::test]
async fn the_log_websocket_is_passed_through_with_the_mihomo_secret() {
    assert_websocket_passes_through("/logs?level=info", "/logs?level=info").await;
}

#[tokio::test]
async fn the_connections_websocket_is_passed_through_with_the_mihomo_secret() {
    assert_websocket_passes_through("/connections?interval=1000", "/connections?interval=1000")
        .await;
}

#[tokio::test]
async fn the_status_bar_websockets_are_passed_through_with_the_mihomo_secret() {
    assert_websocket_passes_through("/traffic", "/traffic").await;
    assert_websocket_passes_through("/memory", "/memory").await;
}

/// Opens `client_path` (plus the dashboard token) as a WebSocket through the proxy and checks
/// that the upstream saw `upstream_path` with the Mihomo secret and without the token.
async fn assert_websocket_passes_through(client_path: &str, upstream_path: &str) {
    // Upstream: accepts one upgrade, reports its request head, says hello and echoes.
    let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_address = upstream.local_addr().unwrap();
    let (head_sender, head_receiver) = tokio::sync::oneshot::channel::<String>();
    tokio::spawn(async move {
        let (mut stream, _) = upstream.accept().await.unwrap();
        let mut head = Vec::new();
        let mut byte = [0_u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            stream.read_exact(&mut byte).await.unwrap();
            head.push(byte[0]);
        }
        head_sender.send(String::from_utf8(head).unwrap()).unwrap();
        stream
            .write_all(b"HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: s3pPLMBiTxaQ9kYGzzhZRbK+xOo=\r\n\r\nhello")
            .await
            .unwrap();
        let mut echo = [0_u8; 4];
        stream.read_exact(&mut echo).await.unwrap();
        stream.write_all(&echo).await.unwrap();
    });
    let root = tempfile::tempdir().unwrap();
    let proxy = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy_address = proxy.local_addr().unwrap();
    let app = router(Some(upstream_address), root.path());
    tokio::spawn(async move { axum::serve(proxy, app).await.unwrap() });

    let mut client = TcpStream::connect(proxy_address).await.unwrap();
    client
        .write_all(
            format!(
                "GET {client_path}{}token=dash-token HTTP/1.1\r\nHost: 127.0.0.1\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n",
                if client_path.contains('?') { '&' } else { '?' }
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    let mut received = Vec::new();
    let mut chunk = [0_u8; 512];
    while !String::from_utf8_lossy(&received).contains("hello") {
        let read = client.read(&mut chunk).await.unwrap();
        assert_ne!(read, 0, "proxy closed the upgraded connection");
        received.extend_from_slice(&chunk[..read]);
    }
    let text = String::from_utf8_lossy(&received);
    assert!(text.starts_with("HTTP/1.1 101"), "{text}");
    assert!(
        text.to_ascii_lowercase()
            .contains("sec-websocket-accept: s3pplmbitxaq9kygzzhzrbk+xoo=")
    );
    client.write_all(b"ping").await.unwrap();
    let mut echo = [0_u8; 4];
    client.read_exact(&mut echo).await.unwrap();
    assert_eq!(&echo, b"ping");
    let head = head_receiver.await.unwrap();
    assert!(
        head.starts_with(&format!("GET {upstream_path} HTTP/1.1\r\n")),
        "{head}"
    );
    assert!(head.contains("Authorization: Bearer mihomo-secret\r\n"));
    assert!(!head.contains("dash-token"));
}

#[tokio::test]
async fn only_the_streams_may_upgrade_to_a_websocket() {
    let (address, seen) = fake_mihomo().await;
    let root = tempfile::tempdir().unwrap();
    let request = axum::http::Request::builder()
        .method(Method::GET)
        .uri("/version")
        .header("authorization", format!("Bearer {TOKEN}"))
        .header("upgrade", "websocket")
        .header("connection", "Upgrade")
        .header("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ==")
        .body(Body::empty())
        .unwrap();
    let response = router(Some(address), root.path())
        .oneshot(request)
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::FORBIDDEN);
    assert!(seen.lock().unwrap().is_empty());
}

#[tokio::test]
async fn an_oversized_chunked_answer_is_not_buffered() {
    // 9 MiB in 1 MiB chunks, without a Content-Length.
    let upstream = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = upstream.local_addr().unwrap();
    tokio::spawn(async move {
        let (mut stream, _) = upstream.accept().await.unwrap();
        let mut head = Vec::new();
        let mut byte = [0_u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            stream.read_exact(&mut byte).await.unwrap();
            head.push(byte[0]);
        }
        let _ = stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\n\r\n")
            .await;
        let chunk = vec![b'x'; 1024 * 1024];
        for _ in 0..9 {
            if stream.write_all(b"100000\r\n").await.is_err()
                || stream.write_all(&chunk).await.is_err()
                || stream.write_all(b"\r\n").await.is_err()
            {
                return;
            }
        }
        let _ = stream.write_all(b"0\r\n\r\n").await;
    });
    let root = tempfile::tempdir().unwrap();
    let (status, _) = call(
        router(Some(address), root.path()),
        Method::GET,
        "/rules",
        Some(TOKEN),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_GATEWAY);
}
