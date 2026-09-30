//! The read-only dashboard: static Zashboard files and a filtering reverse proxy to the current
//! Mihomo controller, on a loopback port the daemon owns. The browser only ever sees this
//! module's per-session token; the Mihomo secret is added here. Only `GET` on
//! [`ALLOWED_API_PATHS`] (and the WebSocket upgrade of the [`STREAM_PATHS`]) passes; everything
//! else is 403, including `DELETE /connections` and `DELETE /connections/{id}`.
//!
//! With the always-ready service the upstream is the service's filtered controller proxy (its
//! address and token come from `Prepared`); this allow-list narrows it further. Without the
//! service the upstream is the daemon's own on-demand Mihomo controller.

use std::{
    path::{Component, Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use axum::{
    Router,
    body::Body,
    extract::{Request, State},
    http::{HeaderMap, HeaderValue, Method, StatusCode, Uri, header},
    response::{IntoResponse, Redirect, Response},
    routing::get,
};
use subtle::ConstantTimeEq;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};

use crate::service_client::{ControllerSlot, ControllerTarget};

pub const ALLOWED_API_PATHS: [&str; 8] = [
    "/version",
    "/logs",
    "/rules",
    "/providers/rules",
    "/configs",
    "/connections",
    "/traffic",
    "/memory",
];
/// Routes Zashboard reads over a WebSocket: the Logs and Connections pages and the status bar
/// (traffic, memory, connection count) shown on every page.
pub const STREAM_PATHS: [&str; 4] = ["/logs", "/connections", "/traffic", "/memory"];
/// Streams Mihomo also serves as never-ending plain HTTP; Zashboard always uses the WebSocket.
/// (A plain `GET /connections` is a finite snapshot and is forwarded.)
const WEBSOCKET_ONLY_PATHS: [&str; 3] = ["/logs", "/traffic", "/memory"];
const MAX_UPSTREAM_BODY_BYTES: usize = 8 * 1024 * 1024;
const MAX_UPGRADE_HEAD_BYTES: usize = 8 * 1024;
const MAX_STATIC_PATH_CHARS: usize = 256;
/// Upper bound for reaching the upstream and reading its `101` head.
const UPGRADE_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone)]
struct ProxyState {
    static_root: Arc<PathBuf>,
    token: Arc<str>,
    upstream: Arc<ControllerSlot>,
    client: reqwest::Client,
}

#[must_use]
pub fn is_allowed(method: &Method, path: &str) -> bool {
    method == Method::GET && ALLOWED_API_PATHS.contains(&path)
}

#[must_use]
pub fn dashboard_url(port: u16, token: &str) -> String {
    format!("http://127.0.0.1:{port}/ui/#/setup?hostname=127.0.0.1&port={port}&secret={token}")
}

pub fn dashboard_router(
    static_root: PathBuf,
    token: Arc<str>,
    upstream: Arc<ControllerSlot>,
) -> Router {
    multicore_core::ensure_crypto_provider_installed();
    let client = reqwest::Client::builder()
        .no_proxy()
        .connect_timeout(Duration::from_secs(2))
        .timeout(Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap_or_default();
    let state = ProxyState {
        static_root: Arc::new(static_root),
        token,
        upstream,
        client,
    };
    Router::new()
        .route("/ui", get(|| async { Redirect::permanent("/ui/") }))
        .route("/ui/", get(static_index))
        .route("/ui/{*path}", get(static_file))
        .fallback(api)
        .with_state(state)
}

async fn static_index(State(state): State<ProxyState>) -> Response {
    serve_static(&state.static_root, "index.html").await
}

async fn static_file(
    State(state): State<ProxyState>,
    axum::extract::Path(path): axum::extract::Path<String>,
) -> Response {
    serve_static(&state.static_root, &path).await
}

fn safe_relative(path: &str) -> Option<PathBuf> {
    if path.is_empty() || path.len() > MAX_STATIC_PATH_CHARS {
        return None;
    }
    let relative = Path::new(path);
    let mut clean = PathBuf::new();
    for component in relative.components() {
        let Component::Normal(part) = component else {
            return None;
        };
        let part = part.to_str()?;
        if !part
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
            || part.starts_with('.')
        {
            return None;
        }
        clean.push(part);
    }
    Some(clean)
}

fn content_type(path: &Path) -> &'static str {
    match path.extension().and_then(|extension| extension.to_str()) {
        Some("html") => "text/html; charset=utf-8",
        Some("js") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("ico") => "image/x-icon",
        Some("json" | "webmanifest") => "application/manifest+json",
        Some("woff2") => "font/woff2",
        Some("txt") => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

async fn serve_static(root: &Path, path: &str) -> Response {
    let Some(relative) = safe_relative(path) else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let full = root.join(&relative);
    match tokio::fs::read(&full).await {
        Ok(bytes) => (
            [
                (header::CONTENT_TYPE, content_type(&full)),
                (header::CACHE_CONTROL, "no-cache"),
                (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            ],
            bytes,
        )
            .into_response(),
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

fn authorized(headers: &HeaderMap, uri: &Uri, token: &str) -> bool {
    let bearer = headers
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "));
    let query = uri.query().and_then(|query| {
        query
            .split('&')
            .find_map(|pair| pair.strip_prefix("token="))
    });
    [bearer, query]
        .into_iter()
        .flatten()
        .any(|candidate| bool::from(candidate.as_bytes().ct_eq(token.as_bytes())))
}

/// The client's query without its dashboard `token`, ready to append (`""` or `"?a=b"`).
fn forwarded_query(uri: &Uri) -> String {
    let kept = uri
        .query()
        .map(|query| {
            query
                .split('&')
                .filter(|pair| !pair.is_empty() && !pair.starts_with("token="))
                .collect::<Vec<_>>()
                .join("&")
        })
        .unwrap_or_default();
    if kept.is_empty() {
        String::new()
    } else {
        format!("?{kept}")
    }
}

fn is_websocket_upgrade(headers: &HeaderMap) -> bool {
    headers
        .get(header::UPGRADE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.eq_ignore_ascii_case("websocket"))
}

async fn api(State(state): State<ProxyState>, request: Request) -> Response {
    let path = request.uri().path().to_owned();
    if !is_allowed(request.method(), &path) {
        return StatusCode::FORBIDDEN.into_response();
    }
    if !authorized(request.headers(), request.uri(), &state.token) {
        return StatusCode::UNAUTHORIZED.into_response();
    }
    let Some(target) = state.upstream.get() else {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    };
    let query = forwarded_query(request.uri());
    if is_websocket_upgrade(request.headers()) {
        if !STREAM_PATHS.contains(&path.as_str()) {
            return StatusCode::FORBIDDEN.into_response();
        }
        return websocket(request, target, &path, &query).await;
    }
    if WEBSOCKET_ONLY_PATHS.contains(&path.as_str()) {
        // Mihomo streams these forever over plain HTTP; Zashboard always uses the WebSocket.
        return StatusCode::BAD_REQUEST.into_response();
    }
    let url = format!("http://{}{path}{query}", target.address);
    let Ok(response) = state
        .client
        .get(url)
        .bearer_auth(target.secret())
        .send()
        .await
    else {
        return StatusCode::BAD_GATEWAY.into_response();
    };
    let status = response.status();
    let content = response.headers().get(header::CONTENT_TYPE).cloned();
    if response
        .content_length()
        .is_some_and(|length| length > MAX_UPSTREAM_BODY_BYTES as u64)
    {
        return StatusCode::BAD_GATEWAY.into_response();
    }
    // Read chunk by chunk: a chunked body has no length to check up front.
    let mut response = response;
    let mut body = Vec::new();
    loop {
        match response.chunk().await {
            Ok(Some(chunk)) => {
                if body.len() + chunk.len() > MAX_UPSTREAM_BODY_BYTES {
                    return StatusCode::BAD_GATEWAY.into_response();
                }
                body.extend_from_slice(&chunk);
            }
            Ok(None) => break,
            Err(_) => return StatusCode::BAD_GATEWAY.into_response(),
        }
    }
    let mut out = Response::new(Body::from(body));
    *out.status_mut() = status;
    if let Some(content) = content {
        out.headers_mut().insert(header::CONTENT_TYPE, content);
    }
    out
}

async fn websocket(
    mut request: Request,
    target: ControllerTarget,
    path: &str,
    query: &str,
) -> Response {
    let Some(key) = request
        .headers()
        .get(header::SEC_WEBSOCKET_KEY)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
    else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    // A stalled upstream must not pin the handler.
    let Ok(Some((mut upstream, buffer, header_end))) =
        tokio::time::timeout(UPGRADE_TIMEOUT, open_upstream(&target, path, query, &key)).await
    else {
        return StatusCode::BAD_GATEWAY.into_response();
    };
    let head = String::from_utf8_lossy(&buffer[..header_end]).into_owned();
    if !head.starts_with("HTTP/1.1 101") {
        return StatusCode::BAD_GATEWAY.into_response();
    }
    let Some(accept) = head
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.trim()
                .eq_ignore_ascii_case("sec-websocket-accept")
                .then(|| value.trim().to_owned())
        })
        .and_then(|value| HeaderValue::from_str(&value).ok())
    else {
        return StatusCode::BAD_GATEWAY.into_response();
    };
    let leftover = buffer[header_end..].to_vec();
    let on_upgrade = hyper::upgrade::on(&mut request);
    tokio::spawn(async move {
        let Ok(upgraded) = on_upgrade.await else {
            return;
        };
        let mut client = hyper_util::rt::TokioIo::new(upgraded);
        if !leftover.is_empty() && client.write_all(&leftover).await.is_err() {
            return;
        }
        let _ = tokio::io::copy_bidirectional(&mut client, &mut upstream).await;
    });
    Response::builder()
        .status(StatusCode::SWITCHING_PROTOCOLS)
        .header(header::UPGRADE, "websocket")
        .header(header::CONNECTION, "Upgrade")
        .header(header::SEC_WEBSOCKET_ACCEPT, accept)
        .body(Body::empty())
        .unwrap_or_else(|_| StatusCode::BAD_GATEWAY.into_response())
}

/// Connects to the upstream, sends the upgrade with the Mihomo secret and reads its answer
/// head. Returns the stream, what was read and where the head ends.
async fn open_upstream(
    target: &ControllerTarget,
    path: &str,
    query: &str,
    key: &str,
) -> Option<(TcpStream, Vec<u8>, usize)> {
    let mut upstream = TcpStream::connect(target.address).await.ok()?;
    let head = format!(
        "GET {path}{query} HTTP/1.1\r\nHost: {}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\nAuthorization: Bearer {}\r\n\r\n",
        target.address,
        target.secret()
    );
    upstream.write_all(head.as_bytes()).await.ok()?;
    let mut buffer = Vec::new();
    let mut chunk = [0_u8; 1024];
    loop {
        let read = upstream.read(&mut chunk).await.ok()?;
        if read == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(position) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            return Some((upstream, buffer, position + 4));
        }
        if buffer.len() > MAX_UPGRADE_HEAD_BYTES {
            return None;
        }
    }
}
