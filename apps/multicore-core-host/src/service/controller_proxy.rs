//! Filtered loopback proxy in front of the service's own Mihomo controller (plan amendment A1).
//!
//! Mihomo runs as SYSTEM/root, and anyone holding its controller secret could replace its
//! binary (`POST /upgrade`), load an arbitrary config (`PUT /configs` with a payload) or switch
//! TUN behind the service's back. The secret therefore never leaves the service: the daemon gets
//! this proxy's loopback port and a per-session token (`ServiceResponse::Prepared.controller`),
//! and the proxy forwards only the reads, selector switches and latency tests the app needs.
//!
//! Each accepted connection carries exactly one request: it is authenticated (`Authorization:
//! Bearer <token>`, or `?token=` as browsers use for WebSockets) and matched against an exact
//! method/route allow-list *before* anything reaches Mihomo; the forwarded request carries the
//! real secret and `Connection: close`, and bytes the client sends after it are never forwarded.
//! WebSocket upgrades are only allowed on the streaming endpoints and are spliced both ways only
//! after Mihomo answered `101`.

use std::{io, net::Ipv4Addr, sync::Arc, time::Duration};

use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    sync::{Semaphore, watch},
    task::JoinHandle,
    time::timeout,
};
use zeroize::Zeroizing;

const MAX_HEAD_BYTES: usize = 16 * 1024;
const MAX_BODY_BYTES: usize = 64 * 1024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const UPSTREAM_CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
const DRAIN_TIMEOUT: Duration = Duration::from_secs(1);
const MAX_CONNECTIONS: usize = 128;

/// Request headers passed on to Mihomo; everything else (credentials, hop-by-hop, framing) is
/// dropped and rebuilt by the proxy.
const FORWARDED_HEADERS: &[&str] = &[
    "accept",
    "accept-encoding",
    "accept-language",
    "cache-control",
    "content-type",
    "origin",
    "pragma",
    "user-agent",
    "sec-websocket-extensions",
    "sec-websocket-key",
    "sec-websocket-protocol",
    "sec-websocket-version",
];

/// A running proxy. Dropping it closes the listener and every relayed connection.
pub(crate) struct ControllerProxy {
    port: u16,
    shutdown: watch::Sender<bool>,
    accept: JoinHandle<()>,
}

struct Settings {
    upstream_port: u16,
    upstream_secret: Zeroizing<String>,
    token: Zeroizing<String>,
}

impl ControllerProxy {
    /// Binds `127.0.0.1:0` on the current tokio runtime and forwards to Mihomo's controller on
    /// `127.0.0.1:<upstream_port>` with `upstream_secret`; clients must present `token`.
    pub(crate) async fn start(
        upstream_port: u16,
        upstream_secret: String,
        token: String,
    ) -> io::Result<Self> {
        if token.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "controller proxy token must not be empty",
            ));
        }
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?;
        let port = listener.local_addr()?.port();
        let settings = Arc::new(Settings {
            upstream_port,
            upstream_secret: Zeroizing::new(upstream_secret),
            token: Zeroizing::new(token),
        });
        let (shutdown, receiver) = watch::channel(false);
        let accept = tokio::spawn(accept_loop(listener, settings, receiver));
        Ok(Self {
            port,
            shutdown,
            accept,
        })
    }

    pub(crate) fn port(&self) -> u16 {
        self.port
    }
}

impl Drop for ControllerProxy {
    fn drop(&mut self) {
        let _ = self.shutdown.send(true);
        self.accept.abort();
    }
}

async fn accept_loop(
    listener: TcpListener,
    settings: Arc<Settings>,
    mut shutdown: watch::Receiver<bool>,
) {
    let permits = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    loop {
        let accepted = tokio::select! {
            accepted = listener.accept() => accepted,
            _ = shutdown.changed() => return,
        };
        let stream = match accepted {
            Ok((stream, _)) => stream,
            Err(_) => {
                tokio::time::sleep(Duration::from_millis(50)).await;
                continue;
            }
        };
        let Ok(permit) = permits.clone().try_acquire_owned() else {
            continue;
        };
        let settings = settings.clone();
        let mut shutdown = shutdown.clone();
        tokio::spawn(async move {
            let _permit = permit;
            tokio::select! {
                () = serve(stream, &settings) => {}
                _ = shutdown.changed() => {}
            }
        });
    }
}

struct Request<'a> {
    method: &'a str,
    path: &'a str,
    query: Option<&'a str>,
    headers: Vec<(&'a str, &'a str)>,
}

impl Request<'_> {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| *value)
    }

    fn header_count(&self, name: &str) -> usize {
        self.headers
            .iter()
            .filter(|(key, _)| key.eq_ignore_ascii_case(name))
            .count()
    }
}

enum Rejection {
    BadRequest,
    Unauthorized,
    Forbidden,
    TooLarge,
    BadGateway,
}

impl Rejection {
    fn response(&self) -> String {
        let (status, reason) = match self {
            Self::BadRequest => (400, "Bad Request"),
            Self::Unauthorized => (401, "Unauthorized"),
            Self::Forbidden => (403, "Forbidden"),
            Self::TooLarge => (413, "Payload Too Large"),
            Self::BadGateway => (502, "Bad Gateway"),
        };
        format!(
            "HTTP/1.1 {status} {reason}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reason}",
            reason.len()
        )
    }
}

async fn serve(mut client: TcpStream, settings: &Settings) {
    let _ = client.set_nodelay(true);
    if let Err(Some(rejection)) = relay(&mut client, settings).await {
        let _ = client.write_all(rejection.response().as_bytes()).await;
    }
    finish(&mut client).await;
}

/// Closes our side, then drains what the client still sends so the close is a FIN, not a RST
/// that could discard the response before the client read it.
async fn finish(client: &mut TcpStream) {
    let _ = client.shutdown().await;
    // One overall deadline: a client trickling bytes cannot hold a connection slot for long.
    let _ = timeout(DRAIN_TIMEOUT, async {
        let mut sink = [0_u8; 4096];
        let mut drained = 0_usize;
        while drained < MAX_BODY_BYTES {
            match client.read(&mut sink).await {
                Ok(read) if read > 0 => drained += read,
                _ => break,
            }
        }
    })
    .await;
}

/// `Err(None)`: the connection is unusable, close silently. `Err(Some(_))`: answer and close.
async fn relay(client: &mut TcpStream, settings: &Settings) -> Result<(), Option<Rejection>> {
    let (head, mut rest) = match timeout(REQUEST_TIMEOUT, read_head(client)).await {
        Ok(Ok(Some(read))) => read,
        Ok(Ok(None)) => return Err(Some(Rejection::BadRequest)),
        _ => return Err(None),
    };
    let head = std::str::from_utf8(&head).map_err(|_| Some(Rejection::BadRequest))?;
    let request = parse_head(head).ok_or(Some(Rejection::BadRequest))?;
    if !authorised(&request, &settings.token) {
        return Err(Some(Rejection::Unauthorized));
    }
    let websocket = request
        .header("upgrade")
        .is_some_and(|value| value.eq_ignore_ascii_case("websocket"));
    let segments = path_segments(request.path).ok_or(Some(Rejection::Forbidden))?;
    if !is_allowed(request.method, &segments, websocket) {
        return Err(Some(Rejection::Forbidden));
    }
    let query = forwarded_query(request.query).ok_or(Some(Rejection::Forbidden))?;
    if request.header("transfer-encoding").is_some() || request.header_count("content-length") > 1 {
        return Err(Some(Rejection::BadRequest));
    }
    let length = match request.header("content-length") {
        None => 0,
        Some(value) => value
            .parse::<usize>()
            .map_err(|_| Some(Rejection::BadRequest))?,
    };
    if length > MAX_BODY_BYTES {
        return Err(Some(Rejection::TooLarge));
    }
    let body = match timeout(REQUEST_TIMEOUT, read_body(client, &mut rest, length)).await {
        Ok(Ok(body)) => body,
        _ => return Err(None),
    };
    let upstream_head = upstream_head(&request, &query, settings, websocket, length);
    let mut upstream = match timeout(
        UPSTREAM_CONNECT_TIMEOUT,
        TcpStream::connect((Ipv4Addr::LOCALHOST, settings.upstream_port)),
    )
    .await
    {
        Ok(Ok(stream)) => stream,
        _ => return Err(Some(Rejection::BadGateway)),
    };
    let _ = upstream.set_nodelay(true);
    if upstream.write_all(upstream_head.as_bytes()).await.is_err()
        || upstream.write_all(&body).await.is_err()
    {
        return Err(Some(Rejection::BadGateway));
    }
    if !websocket {
        let _ = tokio::io::copy(&mut upstream, client).await;
        return Ok(());
    }
    let (response_head, response_rest) =
        match timeout(REQUEST_TIMEOUT, read_head(&mut upstream)).await {
            Ok(Ok(Some(read))) => read,
            _ => return Err(Some(Rejection::BadGateway)),
        };
    let switched = response_head.starts_with(b"HTTP/1.1 101 ");
    if client.write_all(&response_head).await.is_err()
        || client.write_all(b"\r\n\r\n").await.is_err()
        || client.write_all(&response_rest).await.is_err()
    {
        return Err(None);
    }
    if !switched {
        let _ = tokio::io::copy(&mut upstream, client).await;
        return Ok(());
    }
    // Bytes the client sent after its upgrade request are WebSocket frames for Mihomo.
    if upstream.write_all(&rest).await.is_err() {
        return Err(None);
    }
    let _ = tokio::io::copy_bidirectional(client, &mut upstream).await;
    Ok(())
}

/// Reads up to the end of an HTTP head. Returns the head (without the blank line) and the bytes
/// read past it; `None` if the head is too large or the peer closed first.
async fn read_head(stream: &mut TcpStream) -> io::Result<Option<(Vec<u8>, Vec<u8>)>> {
    let mut data = Vec::new();
    let mut buffer = [0_u8; 4096];
    loop {
        if let Some(end) = data.windows(4).position(|window| window == b"\r\n\r\n") {
            let rest = data.split_off(end + 4);
            data.truncate(end);
            return Ok(Some((data, rest)));
        }
        if data.len() > MAX_HEAD_BYTES {
            return Ok(None);
        }
        let read = stream.read(&mut buffer).await?;
        if read == 0 {
            return Ok(None);
        }
        data.extend_from_slice(&buffer[..read]);
    }
}

/// Takes exactly `length` body bytes, first from `rest` (what followed the head), then from the
/// stream. Whatever `rest` still holds afterwards is left there.
async fn read_body(
    client: &mut TcpStream,
    rest: &mut Vec<u8>,
    length: usize,
) -> io::Result<Vec<u8>> {
    let mut body = if rest.len() >= length {
        let tail = rest.split_off(length);
        std::mem::replace(rest, tail)
    } else {
        std::mem::take(rest)
    };
    let mut buffer = [0_u8; 4096];
    while body.len() < length {
        let wanted = (length - body.len()).min(buffer.len());
        let read = client.read(&mut buffer[..wanted]).await?;
        if read == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        body.extend_from_slice(&buffer[..read]);
    }
    Ok(body)
}

fn parse_head(head: &str) -> Option<Request<'_>> {
    let mut lines = head.split("\r\n");
    let mut parts = lines.next()?.split(' ');
    let (method, target, version) = (parts.next()?, parts.next()?, parts.next()?);
    if parts.next().is_some()
        || method.is_empty()
        || !method.bytes().all(|byte| byte.is_ascii_uppercase())
        || !matches!(version, "HTTP/1.1" | "HTTP/1.0")
    {
        return None;
    }
    let (path, query) = match target.split_once('?') {
        Some((path, query)) => (path, Some(query)),
        None => (target, None),
    };
    let mut headers = Vec::new();
    for line in lines {
        let (name, value) = line.split_once(':')?;
        if name.is_empty()
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte))
        {
            return None;
        }
        let value = value.trim_matches([' ', '\t']);
        if value
            .bytes()
            .any(|byte| byte.is_ascii_control() && byte != b'\t')
        {
            return None;
        }
        headers.push((name, value));
    }
    Some(Request {
        method,
        path,
        query,
        headers,
    })
}

fn same_secret(presented: &str, expected: &str) -> bool {
    let (presented, expected) = (presented.as_bytes(), expected.as_bytes());
    presented.len() == expected.len()
        && presented
            .iter()
            .zip(expected)
            .fold(0_u8, |acc, (left, right)| acc | (left ^ right))
            == 0
}

fn authorised(request: &Request<'_>, token: &str) -> bool {
    let bearer = request
        .header("authorization")
        .and_then(|value| value.split_once(' '))
        .filter(|(scheme, _)| scheme.eq_ignore_ascii_case("bearer"))
        .is_some_and(|(_, presented)| same_secret(presented.trim(), token));
    let query = request.query.is_some_and(|query| {
        query
            .split('&')
            .filter_map(|pair| pair.strip_prefix("token="))
            .any(|presented| same_secret(presented, token))
    });
    bearer || query
}

/// The query Mihomo receives: validated, and without any `token` parameter.
fn forwarded_query(query: Option<&str>) -> Option<String> {
    let Some(query) = query else {
        return Some(String::new());
    };
    if !query
        .bytes()
        .all(|byte| (0x21..=0x7e).contains(&byte) && byte != b'#')
    {
        return None;
    }
    let kept = query
        .split('&')
        .filter(|pair| !pair.is_empty() && *pair != "token" && !pair.starts_with("token="))
        .collect::<Vec<_>>();
    Some(if kept.is_empty() {
        String::new()
    } else {
        format!("?{}", kept.join("&"))
    })
}

/// Decoded path segments of an origin-form target, or `None` for anything that is not a plain
/// sequence of non-empty segments (absolute-form, `//`, trailing `/`, dot segments, encoded
/// separators, raw non-ASCII, control characters).
fn path_segments(path: &str) -> Option<Vec<String>> {
    let rest = path.strip_prefix('/')?;
    if rest.is_empty() {
        return Some(Vec::new());
    }
    rest.split('/').map(decode_segment).collect()
}

fn decode_segment(raw: &str) -> Option<String> {
    if raw.is_empty() {
        return None;
    }
    let bytes = raw.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        if byte == b'%' {
            let hex = std::str::from_utf8(bytes.get(index + 1..index + 3)?).ok()?;
            decoded.push(u8::from_str_radix(hex, 16).ok()?);
            index += 3;
            continue;
        }
        if !(byte.is_ascii_alphanumeric() || b"-._~!$&'()*+,;=:@".contains(&byte)) {
            return None;
        }
        decoded.push(byte);
        index += 1;
    }
    let text = String::from_utf8(decoded).ok()?;
    if text == "."
        || text == ".."
        || text.contains(['/', '\\'])
        || text.chars().any(char::is_control)
    {
        return None;
    }
    Some(text)
}

/// Plan amendment A1 allow-list. Everything else is 403, including `PUT|PATCH /configs`,
/// `POST /configs/geo`, `/upgrade*`, `/restart`, `/cache/*`, `PUT /providers/*`, `/debug*`, rule
/// toggles and `/dns/query`.
fn is_allowed(method: &str, segments: &[String], websocket: bool) -> bool {
    let segments = segments.iter().map(String::as_str).collect::<Vec<_>>();
    let streaming = matches!(
        segments.as_slice(),
        ["connections"] | ["traffic"] | ["memory"] | ["logs"]
    );
    if websocket {
        return method == "GET" && streaming;
    }
    match (method, segments.as_slice()) {
        ("GET", ["version" | "proxies" | "group" | "rules" | "configs"])
        | ("GET", ["providers", "proxies" | "rules"])
        | ("GET", ["proxies" | "group", _])
        | ("GET", ["proxies" | "group", _, "delay"])
        | ("GET", ["providers", "proxies", _])
        // A provider, one of its proxies, or its healthcheck.
        | ("GET", ["providers", "proxies", _, _])
        | ("GET", ["providers", "proxies", _, _, "healthcheck"])
        | ("PUT", ["proxies", _])
        | ("DELETE", ["connections"]) => true,
        ("GET", _) => streaming,
        _ => false,
    }
}

fn upstream_head(
    request: &Request<'_>,
    query: &str,
    settings: &Settings,
    websocket: bool,
    length: usize,
) -> Zeroizing<String> {
    let mut head = Zeroizing::new(format!(
        "{} {}{} HTTP/1.1\r\nHost: 127.0.0.1:{}\r\nAuthorization: Bearer {}\r\n",
        request.method,
        request.path,
        query,
        settings.upstream_port,
        settings.upstream_secret.as_str()
    ));
    for (name, value) in &request.headers {
        if FORWARDED_HEADERS
            .iter()
            .any(|allowed| name.eq_ignore_ascii_case(allowed))
        {
            head.push_str(&format!("{name}: {value}\r\n"));
        }
    }
    if websocket {
        head.push_str("Connection: Upgrade\r\nUpgrade: websocket\r\n");
    } else {
        head.push_str("Connection: close\r\n");
    }
    if length > 0 || matches!(request.method, "PUT" | "POST" | "PATCH") {
        head.push_str(&format!("Content-Length: {length}\r\n"));
    }
    head.push_str("\r\n");
    head
}
