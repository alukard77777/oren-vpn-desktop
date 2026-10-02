use std::{
    future::Future,
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    pin::Pin,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use reqwest::header::{ACCEPT, HeaderMap, HeaderValue, USER_AGENT};
use serde_json::value::RawValue;
use thiserror::Error;

use crate::{
    DeviceIdentity, IDLE_XRAY_CONFIG, MAX_CONFIG_BYTES, MAX_SERVICE_LOGO_BYTES, Snapshot,
    SnapshotCommit, SnapshotSink, normalize_group_icon, normalize_service_logo, parse_mihomo,
};

pub const UA_NATIVE: &str = "multicore-json-massive";
pub const UA_MIHOMO: &str = "multicore-mihomo";
pub const UA_XRAY: &str = "multicore-xray";
pub const UA_SERVICE_LOGO: &str = "multicore-service-logo";
/// Mihomo `proxy-groups[].icon` images: fetched exactly like the service logo.
pub const UA_GROUP_ICON: &str = "multicore-group-icon";
/// All group icons of one refresh share this budget; a slow icon host never holds the
/// subscription refresh for long, and icons not fetched in time keep their previous copy.
const GROUP_ICONS_TOTAL_TIMEOUT: Duration = Duration::from_secs(20);
const GROUP_ICON_CONCURRENCY: usize = 4;
const LOGO_DNS_TIMEOUT: Duration = Duration::from_secs(5);
const LOGO_TOTAL_TIMEOUT: Duration = Duration::from_secs(12);
const SUBSCRIPTION_CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const SUBSCRIPTION_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_SUBSCRIPTION_REDIRECTS: usize = 5;
const RETRY_DELAY: Duration = Duration::from_millis(800);
const MAX_RETRY_AFTER: Duration = Duration::from_secs(3);
/// A 429/5xx is asked again only when it came back this fast. A slow one is the provider's
/// gateway giving up on its own backend (one panel answers every request it does not serve with
/// a 502 after ~15 s): asking again only doubled the wait.
const QUICK_STATUS_RETRY_WITHIN: Duration = Duration::from_secs(5);
/// One request through the running connection's local proxy; past it (or on any transport
/// failure) the request goes to the provider directly, once.
const TUNNEL_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(15);
/// The same for a logo or group icon, within its own `LOGO_TOTAL_TIMEOUT`.
const TUNNEL_IMAGE_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(6);
/// The native (combined) request answers first; past this head start the per-core requests
/// run alongside it, so a provider that is slow to refuse the combined request costs a few
/// seconds instead of its whole timeout.
const NATIVE_HEAD_START: Duration = Duration::from_secs(3);
/// How long a Mihomo configuration that needs no Xray still waits for the Xray answer.
const XRAY_GRACE: Duration = Duration::from_secs(2);

/// The local proxy of the running connection (Mihomo's loopback mixed port), published by the
/// daemon while MULTICORE is connected in system-proxy mode and cleared when it disconnects.
/// While it is set, subscription, logo and icon downloads go through the connection first (the
/// provider may be unreachable directly: DNS or DPI blocking), then directly once. In TUN mode
/// there is no such port: direct requests already follow the tunnel's routes.
#[derive(Clone, Default)]
pub struct TunnelProxy(Arc<std::sync::RwLock<Option<SocketAddr>>>);

impl TunnelProxy {
    /// Only a loopback address is ever used.
    pub fn set(&self, address: Option<SocketAddr>) {
        if let Ok(mut current) = self.0.write() {
            *current = address.filter(|address| address.ip().is_loopback());
        }
    }

    #[must_use]
    pub fn get(&self) -> Option<SocketAddr> {
        self.0.read().ok().and_then(|current| *current)
    }
}

impl std::fmt::Debug for TunnelProxy {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_tuple("TunnelProxy")
            .field(&self.get())
            .finish()
    }
}

#[derive(Clone, PartialEq, Eq)]
pub struct HttpResponse {
    pub status: u16,
    pub body: Vec<u8>,
    metadata: SubscriptionMetadataHeaders,
}

impl std::fmt::Debug for HttpResponse {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("HttpResponse")
            .field("status", &self.status)
            .field("body_len", &self.body.len())
            .field("metadata_count", &self.metadata.fields.len())
            .finish()
    }
}

const MAX_METADATA_HEADER_BYTES: usize = 16 * 1024;
const MAX_METADATA_FIELD_BYTES: usize = 4 * 1024;

#[derive(Clone, Default, PartialEq, Eq)]
pub(crate) struct SubscriptionMetadataHeaders {
    fields: Vec<(String, String)>,
}

impl SubscriptionMetadataHeaders {
    pub(crate) fn from_legacy_userinfo(value: Option<&str>) -> Self {
        Self::from_pairs(
            value
                .into_iter()
                .map(|value| ("subscription-userinfo", value)),
        )
    }

    fn from_pairs<N, V>(headers: impl IntoIterator<Item = (N, V)>) -> Self
    where
        N: AsRef<str>,
        V: AsRef<str>,
    {
        let mut fields = Vec::new();
        let mut aggregate = 0_usize;
        for (name, value) in headers {
            let name = name.as_ref().to_ascii_lowercase();
            let value = value.as_ref();
            if !is_metadata_header(&name)
                || name.len() > 128
                || value.len() > MAX_METADATA_FIELD_BYTES
                || value.contains(['\r', '\n'])
            {
                continue;
            }
            let size = name.len().saturating_add(value.len());
            if aggregate.saturating_add(size) > MAX_METADATA_HEADER_BYTES {
                break;
            }
            aggregate += size;
            fields.push((name, value.to_owned()));
        }
        Self { fields }
    }

    fn from_header_map(headers: &HeaderMap) -> Self {
        Self::from_pairs(headers.iter().filter_map(|(name, value)| {
            std::str::from_utf8(value.as_bytes())
                .ok()
                .map(|value| (name.as_str(), value))
        }))
    }

    pub(crate) fn values<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a str> + 'a {
        self.fields
            .iter()
            .filter(move |(candidate, _)| candidate == name)
            .map(|(_, value)| value.as_str())
    }

    pub(crate) fn suffix_values<'a>(
        &'a self,
        suffix: &'a str,
    ) -> impl Iterator<Item = &'a str> + 'a {
        self.fields
            .iter()
            .filter(move |(candidate, _)| {
                candidate == suffix
                    || candidate
                        .strip_suffix(suffix)
                        .is_some_and(|prefix| prefix.ends_with('-'))
            })
            .map(|(_, value)| value.as_str())
    }

    pub(crate) fn vendor_suffix_values<'a>(
        &'a self,
        suffix: &'a str,
    ) -> impl Iterator<Item = &'a str> + 'a {
        self.fields
            .iter()
            .filter(move |(candidate, _)| {
                candidate
                    .strip_suffix(suffix)
                    .is_some_and(|prefix| !prefix.is_empty() && prefix.ends_with('-'))
            })
            .map(|(_, value)| value.as_str())
    }
}

fn is_metadata_header(name: &str) -> bool {
    matches!(
        name,
        "profile-title"
            | "content-disposition"
            | "profile-update-interval"
            | "profile-web-page-url"
            | "support-url"
            | "announce"
            | "sub-info-text"
            | "banner-text"
            | "announce-url"
            | "sub-info-button-link"
            | "banner-button-url"
            | "sub-info-button-text"
            | "banner-button-text"
            | "sub-info-color"
            | HWID_LIMIT_HEADER
    ) || ["servicename", "servicelogo"].iter().any(|suffix| {
        name.strip_suffix(suffix)
            .is_some_and(|prefix| !prefix.is_empty() && prefix.ends_with('-'))
    }) || name == "subscription-userinfo"
        || name.ends_with("-subscription-userinfo")
        || name == "dumb-mode"
        || name.ends_with("-dumb-mode")
        || name == "app-language"
        || name.ends_with("-app-language")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum FetchError {
    #[error("subscription request failed")]
    Network,
    #[error("subscription request timed out")]
    Timeout,
    #[error("subscription server TLS certificate or handshake was rejected")]
    Tls,
    #[error("subscription server redirected too many times or to an insecure URL")]
    Redirect,
    #[error("subscription server returned HTTP status {0}")]
    HttpStatus(u16),
    #[error("subscription server answered with something that is not a MultiCore subscription")]
    NotSubscription,
    #[error("native subscription bundle is invalid")]
    InvalidBundle,
    #[error("subscription configuration is invalid")]
    InvalidConfig,
    #[error("subscription snapshot could not be persisted")]
    Persistence,
    #[error("subscription response exceeds its size limit")]
    TooLarge,
    #[error("subscription server refused this device: its device limit is reached")]
    DeviceLimit,
}

impl FetchError {
    /// A short, URL-free label for diagnostics: never includes the path,
    /// query, or fragment a subscription token could live in.
    fn diagnostic_reason(self) -> &'static str {
        match self {
            Self::Network => "connect_failed",
            Self::Timeout => "timed_out",
            Self::Tls => "tls_rejected",
            Self::Redirect => "redirect_rejected",
            Self::HttpStatus(_) => "http_status",
            Self::NotSubscription => "not_a_subscription",
            Self::InvalidBundle => "invalid_bundle",
            Self::InvalidConfig => "invalid_config",
            Self::Persistence => "could_not_persist",
            Self::TooLarge => "response_too_large",
            Self::DeviceLimit => "device_limit_reached",
        }
    }
}

/// One request flavour's outcome, for `FetchDiagnostics`. Never carries the
/// response body or the request path/query - only what is needed to tell
/// apart "never reached the server" from "server answered but rejected the
/// request" from "server answered with something we can't use".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttemptOutcome {
    /// Never attempted (e.g. the native combined request already answered).
    Skipped,
    /// Started, then given up on: another request brought the configuration first.
    Abandoned,
    Ok {
        status: u16,
    },
    Failed {
        reason: &'static str,
        status: Option<u16>,
    },
}

impl AttemptOutcome {
    fn from_result(result: &Result<HttpResponse, FetchError>) -> Self {
        match result {
            Ok(response) if response.status_is_success() => Self::Ok {
                status: response.status,
            },
            Ok(response) => Self::Failed {
                reason: FetchError::HttpStatus(response.status).diagnostic_reason(),
                status: Some(response.status),
            },
            Err(error) => Self::Failed {
                reason: error.diagnostic_reason(),
                status: match error {
                    FetchError::HttpStatus(status) => Some(*status),
                    _ => None,
                },
            },
        }
    }
}

impl std::fmt::Display for AttemptOutcome {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Skipped => write!(formatter, "skipped"),
            Self::Abandoned => write!(formatter, "abandoned"),
            Self::Ok { status } => write!(formatter, "ok({status})"),
            Self::Failed {
                reason,
                status: Some(status),
            } => write!(formatter, "{reason}({status})"),
            Self::Failed {
                reason,
                status: None,
            } => write!(formatter, "{reason}"),
        }
    }
}

/// A redacted, shareable record of exactly what happened on one subscription
/// refresh attempt, safe to log or hand to a user for support: it never
/// contains the request path, query string, headers, or response body -
/// only the host reached and a short outcome label per request flavour.
#[derive(Debug, Clone)]
pub struct FetchDiagnostics {
    /// `host[:port]` (no scheme, no path/query - a subscription token would
    /// live in the path/query, and the event pipeline's own redaction wipes
    /// any `scheme://...`-shaped text as if it were a full URL).
    pub host: String,
    pub native: AttemptOutcome,
    pub mihomo: AttemptOutcome,
    pub xray: AttemptOutcome,
    pub elapsed_ms: u64,
}

impl std::fmt::Display for FetchDiagnostics {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "host={} native={} mihomo={} xray={} elapsed_ms={}",
            self.host, self.native, self.mihomo, self.xray, self.elapsed_ms
        )
    }
}

/// `host[:port]` only - deliberately never the scheme (this event pipeline's
/// own redaction treats any `scheme://...` text as a full URL and wipes it
/// entirely, host included) and never the path, query, or fragment, which is
/// where a subscription URL's access token lives.
fn safe_host(url: &str) -> String {
    reqwest::Url::parse(url)
        .ok()
        .and_then(|parsed| {
            let host = parsed.host_str()?.to_owned();
            Some(match parsed.port() {
                Some(port) => format!("{host}:{port}"),
                None => host,
            })
        })
        .unwrap_or_else(|| "unknown-host".to_owned())
}

pub trait HttpClient: Send + Sync {
    fn get<'a>(
        &'a self,
        url: &'a str,
        user_agent: &'static str,
    ) -> Pin<Box<dyn Future<Output = Result<HttpResponse, FetchError>> + Send + 'a>>;
}

#[derive(Clone)]
pub struct ReqwestHttpClient {
    subscription_client: reqwest::Client,
    /// Used only for one retry after the direct request could not connect.
    /// It honours the OS/environment proxy the way a browser does, which
    /// helps when the provider domain is unreachable directly (DNS or DPI
    /// blocking) but reachable through the user's configured proxy.
    system_proxy_client: reqwest::Client,
    headers: HeaderMap,
    tunnel: TunnelProxy,
    tunnel_attempt_timeout: Duration,
    /// Clients through the tunnel proxy, for the address they were built for.
    tunnel_clients: Arc<std::sync::Mutex<Option<TunnelClients>>>,
}

#[derive(Clone)]
struct TunnelClients {
    proxy: SocketAddr,
    subscription: reqwest::Client,
    image: reqwest::Client,
}

impl ReqwestHttpClient {
    pub fn new(identity: DeviceIdentity) -> Result<Self, FetchError> {
        crate::ensure_crypto_provider_installed();
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-hwid",
            HeaderValue::from_str(identity.hwid()).map_err(|_| FetchError::Network)?,
        );
        headers.insert(
            "x-device-os",
            HeaderValue::from_static(std::env::consts::OS),
        );
        headers.insert(
            "x-ver-os",
            HeaderValue::from_str(identity.os_version()).map_err(|_| FetchError::Network)?,
        );
        headers.insert(
            "x-device-model",
            HeaderValue::from_str(identity.device_model()).map_err(|_| FetchError::Network)?,
        );
        let subscription_client = subscription_client_builder(&headers)
            .no_proxy()
            .build()
            .map_err(|_| FetchError::Network)?;
        let system_proxy_client = subscription_client_builder(&headers)
            .build()
            .map_err(|_| FetchError::Network)?;
        Ok(Self {
            subscription_client,
            system_proxy_client,
            headers,
            tunnel: TunnelProxy::default(),
            tunnel_attempt_timeout: TUNNEL_ATTEMPT_TIMEOUT,
            tunnel_clients: Arc::default(),
        })
    }

    /// Downloads go through `tunnel` first whenever it names a proxy (see [`TunnelProxy`]).
    #[must_use]
    pub fn with_tunnel_proxy(mut self, tunnel: TunnelProxy) -> Self {
        self.tunnel = tunnel;
        self
    }

    #[cfg(test)]
    fn with_tunnel_attempt_timeout(mut self, timeout: Duration) -> Self {
        self.tunnel_attempt_timeout = timeout;
        self
    }

    /// The clients through the running connection's proxy, while there is one.
    fn tunnel_clients(&self) -> Option<TunnelClients> {
        let proxy = self.tunnel.get()?;
        let mut cached = self.tunnel_clients.lock().ok()?;
        if let Some(clients) = cached.as_ref().filter(|clients| clients.proxy == proxy) {
            return Some(clients.clone());
        }
        let through = || reqwest::Proxy::all(format!("http://{proxy}")).ok();
        let subscription = subscription_client_builder(&self.headers)
            .proxy(through()?)
            .build()
            .ok()?;
        let image = reqwest::Client::builder()
            .proxy(through()?)
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(30))
            .build()
            .ok()?;
        let clients = TunnelClients {
            proxy,
            subscription,
            image,
        };
        *cached = Some(clients.clone());
        Some(clients)
    }

    async fn get_subscription(
        &self,
        url: &str,
        user_agent: &'static str,
    ) -> Result<HttpResponse, FetchError> {
        if let Some(tunnel) = self.tunnel_clients() {
            let through = tokio::time::timeout(
                self.tunnel_attempt_timeout,
                get_with_quick_status_retry(&tunnel.subscription, url, user_agent),
            )
            .await;
            match through {
                Ok(Ok(response)) => return Ok(response),
                // Answers about the subscription itself are the same either way.
                Ok(Err(error @ (FetchError::Redirect | FetchError::TooLarge))) => {
                    return Err(error);
                }
                // The connection did not get through to the provider: straight to it, once.
                // The system proxy is this same connection now, so it is not tried again.
                Ok(Err(_)) | Err(_) => {}
            }
            return get_with_quick_status_retry(&self.subscription_client, url, user_agent).await;
        }
        let started = std::time::Instant::now();
        match perform_get_detailed(&self.subscription_client, url, user_agent).await {
            Ok(response) => {
                if let Some(delay) = retry_delay_for_status(&response, started.elapsed()) {
                    tokio::time::sleep(delay).await;
                    return perform_get(&self.subscription_client, url, user_agent).await;
                }
                Ok(response.response)
            }
            Err(TransportFailure::Connect) => {
                tokio::time::sleep(RETRY_DELAY).await;
                perform_get(&self.system_proxy_client, url, user_agent).await
            }
            Err(TransportFailure::Other(error)) => Err(error),
        }
    }
}

fn subscription_client_builder(headers: &HeaderMap) -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .default_headers(headers.clone())
        .redirect(subscription_redirect_policy())
        .connect_timeout(SUBSCRIPTION_CONNECT_TIMEOUT)
        .timeout(SUBSCRIPTION_REQUEST_TIMEOUT)
}

/// One request, asked once more after a quick 429/5xx (see [`QUICK_STATUS_RETRY_WITHIN`]).
async fn get_with_quick_status_retry(
    client: &reqwest::Client,
    url: &str,
    user_agent: &'static str,
) -> Result<HttpResponse, FetchError> {
    let started = std::time::Instant::now();
    match perform_get_detailed(client, url, user_agent).await {
        Ok(response) => {
            if let Some(delay) = retry_delay_for_status(&response, started.elapsed()) {
                tokio::time::sleep(delay).await;
                return perform_get(client, url, user_agent).await;
            }
            Ok(response.response)
        }
        Err(TransportFailure::Connect) => Err(FetchError::Network),
        Err(TransportFailure::Other(error)) => Err(error),
    }
}

/// Browsers follow redirects, and panels commonly redirect http to https,
/// add a trailing slash or sit behind short links. Never downgrade to http.
fn subscription_redirect_policy() -> reqwest::redirect::Policy {
    reqwest::redirect::Policy::custom(|attempt| {
        if attempt.previous().len() > MAX_SUBSCRIPTION_REDIRECTS {
            attempt.error("too many subscription redirects")
        } else if redirect_allowed(attempt.previous(), attempt.url()) {
            attempt.follow()
        } else {
            attempt.error("insecure subscription redirect")
        }
    })
}

fn redirect_allowed(previous: &[reqwest::Url], next: &reqwest::Url) -> bool {
    if !matches!(next.scheme(), "http" | "https") {
        return false;
    }
    let downgrade = next.scheme() != "https" && previous.iter().any(|url| url.scheme() == "https");
    let origin_is_public = previous
        .first()
        .is_none_or(|origin| !host_is_non_global_ip(origin));
    !downgrade && !(origin_is_public && host_is_non_global_ip(next))
}

/// A public provider must not bounce the request (with identity headers) to
/// loopback or private addresses on the user's machine or network.
fn host_is_non_global_ip(url: &reqwest::Url) -> bool {
    let Some(host) = url.host_str() else {
        return true;
    };
    let literal = host.trim_start_matches('[').trim_end_matches(']');
    match literal.parse::<IpAddr>() {
        Ok(ip) => !is_global_ip(ip),
        Err(_) => {
            let host = host.trim_end_matches('.');
            host.eq_ignore_ascii_case("localhost")
                || host.to_ascii_lowercase().ends_with(".localhost")
        }
    }
}

impl HttpClient for ReqwestHttpClient {
    fn get<'a>(
        &'a self,
        url: &'a str,
        user_agent: &'static str,
    ) -> Pin<Box<dyn Future<Output = Result<HttpResponse, FetchError>> + Send + 'a>> {
        Box::pin(async move {
            if is_image_user_agent(user_agent) {
                tokio::time::timeout(LOGO_TOTAL_TIMEOUT, self.get_service_logo(url, user_agent))
                    .await
                    .map_err(|_| FetchError::Timeout)?
            } else {
                self.get_subscription(url, user_agent).await
            }
        })
    }
}

impl ReqwestHttpClient {
    async fn get_service_logo(
        &self,
        url: &str,
        user_agent: &'static str,
    ) -> Result<HttpResponse, FetchError> {
        let parsed = reqwest::Url::parse(url).map_err(|_| FetchError::Network)?;
        let host = parsed.host_str().ok_or(FetchError::Network)?;
        let port = parsed.port_or_known_default().ok_or(FetchError::Network)?;
        // Checked even when the request goes through the connection, which resolves the name
        // again itself: an image link never reaches this machine's own network.
        let addresses = resolve_global_logo_addresses(host, port).await?;
        if let Some(tunnel) = self.tunnel_clients()
            && let Ok(Ok(response)) = tokio::time::timeout(
                TUNNEL_IMAGE_ATTEMPT_TIMEOUT,
                perform_get(&tunnel.image, url, user_agent),
            )
            .await
        {
            return Ok(response);
        }
        let client = build_pinned_logo_client(host, &addresses)?;
        perform_get(&client, url, user_agent).await
    }
}

struct DetailedResponse {
    response: HttpResponse,
    retry_after: Option<Duration>,
}

enum TransportFailure {
    /// The TCP/TLS connection could not be established at all.
    Connect,
    Other(FetchError),
}

async fn perform_get(
    client: &reqwest::Client,
    url: &str,
    user_agent: &'static str,
) -> Result<HttpResponse, FetchError> {
    match perform_get_detailed(client, url, user_agent).await {
        Ok(detailed) => Ok(detailed.response),
        Err(TransportFailure::Connect) => Err(FetchError::Network),
        Err(TransportFailure::Other(error)) => Err(error),
    }
}

async fn perform_get_detailed(
    client: &reqwest::Client,
    url: &str,
    user_agent: &'static str,
) -> Result<DetailedResponse, TransportFailure> {
    let mut response = request_builder(client, url, user_agent)
        .send()
        .await
        .map_err(|error| classify_send_error(&error))?;
    let status = response.status().as_u16();
    let retry_after = response
        .headers()
        .get(reqwest::header::RETRY_AFTER)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse::<u64>().ok())
        .map(Duration::from_secs);
    let metadata = SubscriptionMetadataHeaders::from_header_map(response.headers());
    let limit = response_limit(user_agent);
    if response
        .content_length()
        .is_some_and(|size| size > limit as u64)
    {
        return Err(TransportFailure::Other(FetchError::TooLarge));
    }
    let mut body =
        Vec::with_capacity(response.content_length().unwrap_or(0).min(limit as u64) as usize);
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| TransportFailure::Other(classify_body_error(&error)))?
    {
        if body.len().saturating_add(chunk.len()) > limit {
            return Err(TransportFailure::Other(FetchError::TooLarge));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(DetailedResponse {
        response: HttpResponse {
            status,
            body,
            metadata,
        },
        retry_after,
    })
}

fn classify_send_error(error: &reqwest::Error) -> TransportFailure {
    if error.is_redirect() {
        return TransportFailure::Other(FetchError::Redirect);
    }
    if is_tls_error(error) {
        return TransportFailure::Other(FetchError::Tls);
    }
    if error.is_connect() {
        return TransportFailure::Connect;
    }
    if error.is_timeout() {
        return TransportFailure::Other(FetchError::Timeout);
    }
    TransportFailure::Other(FetchError::Network)
}

fn classify_body_error(error: &reqwest::Error) -> FetchError {
    if error.is_timeout() {
        FetchError::Timeout
    } else {
        FetchError::Network
    }
}

fn is_tls_error(error: &reqwest::Error) -> bool {
    let mut source: Option<&(dyn std::error::Error + 'static)> = Some(error);
    while let Some(current) = source {
        let text = current.to_string().to_ascii_lowercase();
        if text.contains("certificate")
            || text.contains("tls handshake")
            || text.contains("invalid peer")
            || text.contains("unknownissuer")
            || text.contains("peer is incompatible")
        {
            return true;
        }
        source = current.source();
    }
    false
}

/// One quick retry for rate limiting and transient upstream errors. Anti-bot
/// front ends commonly answer the first burst of requests with 429/503.
/// Only a quick answer is asked again (see [`QUICK_STATUS_RETRY_WITHIN`]).
fn retry_delay_for_status(response: &DetailedResponse, elapsed: Duration) -> Option<Duration> {
    let status = response.response.status;
    if elapsed >= QUICK_STATUS_RETRY_WITHIN {
        return None;
    }
    if status == 429 || matches!(status, 500 | 502 | 503 | 504) {
        Some(
            response
                .retry_after
                .map_or(RETRY_DELAY, |delay| delay.min(MAX_RETRY_AFTER)),
        )
    } else {
        None
    }
}

fn request_builder(
    client: &reqwest::Client,
    url: &str,
    user_agent: &'static str,
) -> reqwest::RequestBuilder {
    client
        .get(url)
        .header(USER_AGENT, user_agent)
        .header(ACCEPT, accept_for(user_agent))
}

async fn resolve_global_logo_addresses(
    host: &str,
    port: u16,
) -> Result<Vec<SocketAddr>, FetchError> {
    resolve_global_logo_addresses_with(host, port, LOGO_DNS_TIMEOUT, |host, port| async move {
        tokio::net::lookup_host((host.as_str(), port))
            .await
            .map(|addresses| addresses.collect::<Vec<_>>())
            .map_err(|_| FetchError::Network)
    })
    .await
}

async fn resolve_global_logo_addresses_with<F, Fut>(
    host: &str,
    port: u16,
    timeout: Duration,
    resolver: F,
) -> Result<Vec<SocketAddr>, FetchError>
where
    F: FnOnce(String, u16) -> Fut,
    Fut: Future<Output = Result<Vec<SocketAddr>, FetchError>>,
{
    let mut addresses = if let Ok(ip) = host.parse::<IpAddr>() {
        vec![SocketAddr::new(ip, port)]
    } else {
        tokio::time::timeout(timeout, resolver(host.to_owned(), port))
            .await
            .map_err(|_| FetchError::Network)??
    };
    addresses.sort_unstable();
    addresses.dedup();
    approve_logo_addresses(addresses)
}

fn approve_logo_addresses(addresses: Vec<SocketAddr>) -> Result<Vec<SocketAddr>, FetchError> {
    if addresses.is_empty()
        || addresses
            .iter()
            .any(|address| !is_global_ip(address.ip()) && !is_fake_ip(address.ip()))
    {
        return Err(FetchError::Network);
    }
    Ok(addresses)
}

/// 198.18.0.0/15 is reserved for benchmarking and holds no real hosts; proxy
/// clients (Mihomo/Clash "fake-ip", including MultiCore itself while
/// connected) answer DNS from it and route the connection through the
/// tunnel. Rejecting it meant no provider logo while any such VPN was on;
/// loopback, private and link-local answers stay rejected.
fn is_fake_ip(ip: IpAddr) -> bool {
    matches!(ip, IpAddr::V4(ip) if ip.octets()[0] == 198 && matches!(ip.octets()[1], 18 | 19))
}

fn build_pinned_logo_client(
    host: &str,
    addresses: &[SocketAddr],
) -> Result<reqwest::Client, FetchError> {
    crate::ensure_crypto_provider_installed();
    reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(30))
        .resolve_to_addrs(host, addresses)
        .build()
        .map_err(|_| FetchError::Network)
}

fn is_global_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => is_global_ipv4(ip),
        IpAddr::V6(ip) => is_global_ipv6(ip),
    }
}

fn is_global_ipv4(ip: Ipv4Addr) -> bool {
    let [a, b, c, _] = ip.octets();
    !(a == 0
        || a == 10
        || a == 127
        || (a == 100 && (64..=127).contains(&b))
        || (a == 64 && b == 0 && c == 0)
        || (a == 169 && b == 254)
        || (a == 172 && (16..=31).contains(&b))
        || (a == 192 && b == 0 && c == 0)
        || (a == 192 && b == 0 && c == 2)
        || (a == 192 && b == 168)
        || (a == 192 && b == 88 && c == 99)
        || (a == 198 && (b == 18 || b == 19))
        || (a == 198 && b == 51 && c == 100)
        || (a == 203 && b == 0 && c == 113)
        || a >= 224)
}

fn is_global_ipv6(ip: Ipv6Addr) -> bool {
    if let Some(ipv4) = ip.to_ipv4_mapped() {
        return is_global_ipv4(ipv4);
    }
    let segments = ip.segments();
    (segments[0] & 0xe000) == 0x2000
        && !(segments[0] == 0x2001 && (segments[1] <= 0x01ff || segments[1] == 0x0db8))
        && segments[0] != 0x2002
        && !(segments[0] == 0x3fff && segments[1] < 0x1000)
}

/// What a provider-logo download gave.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServiceLogoDownload {
    /// The canonical PNG.
    Logo(Vec<u8>),
    /// The image host answers with its "image unavailable" placeholder: the logo is gone.
    Gone,
    /// Not reachable or not an image this time: keep what is shown.
    Failed,
}

pub struct SubscriptionFetcher<C> {
    client: C,
}

impl<C: HttpClient> SubscriptionFetcher<C> {
    pub fn new(client: C) -> Self {
        Self { client }
    }

    pub async fn refresh<S: SnapshotSink>(
        &self,
        url: &str,
        store: &S,
    ) -> Result<SnapshotCommit<Arc<Snapshot>>, FetchError> {
        self.refresh_with_diagnostics(url, store).await.0
    }

    /// Same as `refresh`, but also returns a redacted per-attempt summary
    /// (which of the native/Mihomo/Xray requests reached the server, and
    /// with what outcome) safe to show a user or attach to a bug report: it
    /// never contains the request path/query (where a subscription token
    /// lives), headers, or response bodies - only the host reached and a
    /// short outcome label per request flavour.
    pub async fn refresh_with_diagnostics<S: SnapshotSink>(
        &self,
        url: &str,
        store: &S,
    ) -> (
        Result<SnapshotCommit<Arc<Snapshot>>, FetchError>,
        FetchDiagnostics,
    ) {
        self.refresh_impl(url, store, false).await
    }

    /// `refresh_with_diagnostics` without the provider-logo download: a named logo is only
    /// marked as not fetched (so `Snapshot::with_service_logo_carried_from` keeps the previous
    /// one) and its URL stays readable in memory (`Snapshot::subscription_logo_url`) for a
    /// background `download_service_logo`. A slow logo host never holds a refresh up.
    pub async fn refresh_deferring_logo<S: SnapshotSink>(
        &self,
        url: &str,
        store: &S,
    ) -> (
        Result<SnapshotCommit<Arc<Snapshot>>, FetchError>,
        FetchDiagnostics,
    ) {
        self.refresh_impl(url, store, true).await
    }

    async fn refresh_impl<S: SnapshotSink>(
        &self,
        url: &str,
        store: &S,
        defer_logo: bool,
    ) -> (
        Result<SnapshotCommit<Arc<Snapshot>>, FetchError>,
        FetchDiagnostics,
    ) {
        let started = std::time::Instant::now();
        let host = safe_host(url);
        let (native_response, per_core) = self.negotiate(url).await;
        let per_core_skipped = if matches!(per_core, PerCoreState::Abandoned) {
            AttemptOutcome::Abandoned
        } else {
            AttemptOutcome::Skipped
        };
        let native_outcome = native_response
            .as_ref()
            .map_or(AttemptOutcome::Abandoned, AttemptOutcome::from_result);

        let native_error = match native_response {
            // Abandoned: the per-core requests brought a configuration first.
            None => FetchError::Timeout,
            Some(Ok(response)) if response.status_is_success() => {
                let body = strip_utf8_bom(&response.body);
                if looks_like_native_bundle(body) {
                    // A JSON array means the backend recognised the combined
                    // MultiCore request, so its answer is final.
                    let outcome = parse_native(body).and_then(|snapshot| {
                        snapshot
                            .with_subscription_metadata(
                                url,
                                &response.metadata,
                                current_unix_time(),
                            )
                            .map_err(|_| FetchError::InvalidBundle)
                    });
                    let outcome = match outcome {
                        Ok(mut snapshot) => {
                            self.logo_for(&mut snapshot, defer_logo).await;
                            store.save(snapshot).map_err(|_| FetchError::Persistence)
                        }
                        Err(error) => Err(error),
                    };
                    let diagnostics = FetchDiagnostics {
                        host,
                        native: native_outcome,
                        mihomo: per_core_skipped,
                        xray: per_core_skipped,
                        elapsed_ms: u64::try_from(started.elapsed().as_millis())
                            .unwrap_or(u64::MAX),
                    };
                    return (outcome, diagnostics);
                }
                // HTML, share links, Base64 or plain YAML: the panel did not
                // recognise the combined request; try the per-core requests.
                FetchError::NotSubscription
            }
            Some(Ok(response)) => FetchError::HttpStatus(response.status),
            Some(Err(error)) => error,
        };

        let PerCore {
            mihomo,
            xray,
            xray_skipped,
        } = match per_core {
            PerCoreState::Done(per_core) => per_core,
            PerCoreState::NotStarted | PerCoreState::Abandoned => self.per_core(url).await,
        };
        let mihomo_outcome = AttemptOutcome::from_result(&mihomo);
        let xray_outcome = if xray_skipped {
            AttemptOutcome::Skipped
        } else {
            AttemptOutcome::from_result(&xray)
        };
        let outcome = match parse_fallback(url, mihomo, xray)
            .map_err(|error| prefer_informative(native_error, error))
        {
            Ok(mut snapshot) => {
                self.logo_for(&mut snapshot, defer_logo).await;
                store.save(snapshot).map_err(|_| FetchError::Persistence)
            }
            Err(error) => Err(error),
        };
        let diagnostics = FetchDiagnostics {
            host,
            native: native_outcome,
            mihomo: mihomo_outcome,
            xray: xray_outcome,
            elapsed_ms: u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
        };
        (outcome, diagnostics)
    }

    /// The native (combined) request first. When it has not answered within
    /// [`NATIVE_HEAD_START`], the per-core requests run alongside it: a native bundle that
    /// arrives first still wins; a Mihomo configuration that arrives first wins and the native
    /// request is abandoned (`None`).
    async fn negotiate(
        &self,
        url: &str,
    ) -> (Option<Result<HttpResponse, FetchError>>, PerCoreState) {
        let native = self.client.get(url, UA_NATIVE);
        tokio::pin!(native);
        if let Ok(response) = tokio::time::timeout(NATIVE_HEAD_START, &mut native).await {
            return (Some(response), PerCoreState::NotStarted);
        }
        let per_core = self.per_core(url);
        tokio::pin!(per_core);
        // Polled in order (as `join!` did): the native request, then Mihomo, then Xray.
        tokio::select! {
            biased;
            response = &mut native => {
                if is_native_bundle(&response) {
                    (Some(response), PerCoreState::Abandoned)
                } else {
                    (Some(response), PerCoreState::Done(per_core.await))
                }
            }
            answers = &mut per_core => {
                if answers.mihomo.as_ref().is_ok_and(HttpResponse::status_is_success) {
                    (None, PerCoreState::Done(answers))
                } else {
                    (Some(native.await), PerCoreState::Done(answers))
                }
            }
        }
    }

    /// The Mihomo and Xray requests together. A Mihomo configuration that routes nothing
    /// into Xray (see `standalone_mihomo`) waits only [`XRAY_GRACE`] for the Xray answer,
    /// which it does not need: a panel that serves only Mihomo may take its whole timeout to
    /// refuse the Xray request.
    async fn per_core(&self, url: &str) -> PerCore {
        let mihomo = self.client.get(url, UA_MIHOMO);
        let xray = self.client.get(url, UA_XRAY);
        tokio::pin!(mihomo, xray);
        tokio::select! {
            biased;
            mihomo = &mut mihomo => {
                if !mihomo_stands_alone(&mihomo) {
                    let xray = xray.await;
                    return PerCore { mihomo, xray, xray_skipped: false };
                }
                match tokio::time::timeout(XRAY_GRACE, &mut xray).await {
                    Ok(xray) => PerCore { mihomo, xray, xray_skipped: false },
                    Err(_) => PerCore {
                        mihomo,
                        xray: Err(FetchError::Timeout),
                        xray_skipped: true,
                    },
                }
            }
            xray = &mut xray => {
                let mihomo = mihomo.await;
                PerCore { mihomo, xray, xray_skipped: false }
            }
        }
    }

    async fn logo_for(&self, snapshot: &mut Snapshot, defer: bool) {
        if defer {
            if snapshot.subscription_logo_url().is_some() {
                snapshot.mark_service_logo_unfetched();
            }
            return;
        }
        let Some(url) = snapshot.take_subscription_logo_url() else {
            return;
        };
        match self.download_service_logo(&url).await {
            ServiceLogoDownload::Logo(png) => snapshot.set_service_logo_png(png),
            // Show no logo rather than the placeholder or a stale copy of it.
            ServiceLogoDownload::Gone => {}
            ServiceLogoDownload::Failed => snapshot.mark_service_logo_unfetched(),
        }
    }

    /// Downloads and normalizes a provider logo (`Snapshot::subscription_logo_url`) through
    /// the pinned, global-address-only, size- and time-bounded image path.
    pub async fn download_service_logo(&self, url: &str) -> ServiceLogoDownload {
        let Ok(response) = self.client.get(url, UA_SERVICE_LOGO).await else {
            return ServiceLogoDownload::Failed;
        };
        // A host placeholder ("image unavailable") means the provider's logo is gone.
        if response.status_is_success()
            && crate::service_logo::is_image_host_placeholder(&response.body)
        {
            return ServiceLogoDownload::Gone;
        }
        if response.status_is_success()
            && let Some(png) = normalize_service_logo(&response.body)
        {
            ServiceLogoDownload::Logo(png)
        } else {
            ServiceLogoDownload::Failed
        }
    }

    /// Downloads Mihomo `proxy-groups[].icon` images (`(group_icon_key, url)` pairs from
    /// `Snapshot::group_icon_urls`) through the same pinned, global-address-only, size- and
    /// time-bounded path as the service logo, a few at a time and within one overall budget.
    /// Returns the canonical PNG of each icon that arrived, by key; failures are left out.
    ///
    /// Never part of a subscription refresh: the daemon runs it in the background after the
    /// subscription is saved, so a slow or dead icon host cannot hold an import up.
    pub async fn download_group_icons(
        &self,
        pending: &[(String, String)],
    ) -> std::collections::BTreeMap<String, Vec<u8>> {
        let mut downloaded = std::collections::BTreeMap::new();
        if pending.is_empty() {
            return downloaded;
        }
        let mut outcomes: Vec<(String, Option<Vec<u8>>)> = Vec::with_capacity(pending.len());
        let fetch_one = |url: Option<String>| async move {
            let url = url?;
            let response = self.client.get(&url, UA_GROUP_ICON).await.ok()?;
            Some(
                (response.status_is_success()
                    && !crate::service_logo::is_image_host_placeholder(&response.body))
                .then(|| normalize_group_icon(&response.body))
                .flatten(),
            )
        };
        let fetch_all = async {
            for batch in pending.chunks(GROUP_ICON_CONCURRENCY) {
                let url = |index: usize| batch.get(index).map(|(_, url)| url.clone());
                let (first, second, third, fourth) = tokio::join!(
                    fetch_one(url(0)),
                    fetch_one(url(1)),
                    fetch_one(url(2)),
                    fetch_one(url(3))
                );
                for ((key, _), result) in batch.iter().zip([first, second, third, fourth]) {
                    outcomes.push((key.clone(), result.flatten()));
                }
            }
        };
        let _ = tokio::time::timeout(GROUP_ICONS_TOTAL_TIMEOUT, fetch_all).await;
        for (key, png) in outcomes {
            if let Some(png) = png {
                downloaded.insert(key, png);
            }
        }
        downloaded
    }
}

impl HttpResponse {
    pub fn new<N, V>(status: u16, body: Vec<u8>, headers: impl IntoIterator<Item = (N, V)>) -> Self
    where
        N: AsRef<str>,
        V: AsRef<str>,
    {
        Self {
            status,
            body,
            metadata: SubscriptionMetadataHeaders::from_pairs(headers),
        }
    }

    fn status_is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }
}

#[cfg(test)]
mod subscription_transport_tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn subscription_request_uses_direct_tun_route_even_with_proxy_environment() {
        let direct = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let direct_url = format!(
            "http://127.0.0.1:{}/subscription",
            direct.local_addr().unwrap().port()
        );
        let proxy_url = format!("http://127.0.0.1:{}", proxy.local_addr().unwrap().port());
        let direct_server = tokio::spawn(async move {
            let (mut stream, _) = tokio::time::timeout(Duration::from_secs(5), direct.accept())
                .await
                .expect("direct subscription request")
                .unwrap();
            let mut request = [0_u8; 2048];
            let length = stream.read(&mut request).await.unwrap();
            assert!(String::from_utf8_lossy(&request[..length]).contains("GET /subscription"));
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nOK")
                .await
                .unwrap();
        });
        let proxy_server = tokio::spawn(async move {
            tokio::time::timeout(Duration::from_secs(3), proxy.accept())
                .await
                .is_ok()
        });
        let output = tokio::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--ignored",
                "--exact",
                "fetch::subscription_transport_tests::subscription_proxy_environment_child",
            ])
            .env("MULTICORE_SUBSCRIPTION_PROXY_TEST_URL", direct_url)
            .env("HTTP_PROXY", &proxy_url)
            .env("http_proxy", &proxy_url)
            .env("ALL_PROXY", &proxy_url)
            .env("all_proxy", &proxy_url)
            .env("NO_PROXY", "")
            .env("no_proxy", "")
            .output()
            .await
            .unwrap();
        assert!(
            output.status.success(),
            "subscription proxy child failed: stdout={} stderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        direct_server.await.unwrap();
        assert!(
            !proxy_server.await.unwrap(),
            "subscription request used the environment proxy"
        );
    }

    #[test]
    #[ignore = "isolated helper invoked by subscription_request_uses_direct_tun_route_even_with_proxy_environment"]
    fn subscription_proxy_environment_child() {
        let root = tempfile::tempdir().unwrap();
        let identity = DeviceIdentity::load_or_create(root.path()).unwrap();
        let client = ReqwestHttpClient::new(identity).unwrap();
        let url = std::env::var("MULTICORE_SUBSCRIPTION_PROXY_TEST_URL").unwrap();
        let runtime = tokio::runtime::Runtime::new().unwrap();
        let response = runtime.block_on(client.get(&url, UA_NATIVE)).unwrap();
        assert_eq!(response.status, 200);
        assert_eq!(response.body, b"OK");
    }
}

fn success_response(response: HttpResponse) -> Result<HttpResponse, FetchError> {
    if response.status_is_success() {
        Ok(response)
    } else {
        Err(FetchError::HttpStatus(response.status))
    }
}

/// Whether the per-core requests ran during the native one.
enum PerCoreState {
    NotStarted,
    /// Started, then dropped for a native bundle that arrived first.
    Abandoned,
    Done(PerCore),
}

/// The per-core answers; `xray_skipped` when a standalone Mihomo configuration did not wait
/// for the Xray one.
struct PerCore {
    mihomo: Result<HttpResponse, FetchError>,
    xray: Result<HttpResponse, FetchError>,
    xray_skipped: bool,
}

fn is_native_bundle(response: &Result<HttpResponse, FetchError>) -> bool {
    response.as_ref().is_ok_and(|response| {
        response.status_is_success() && looks_like_native_bundle(strip_utf8_bom(&response.body))
    })
}

/// A Mihomo answer that is a configuration routing nothing into Xray.
fn mihomo_stands_alone(response: &Result<HttpResponse, FetchError>) -> bool {
    response.as_ref().is_ok_and(|response| {
        response.status_is_success()
            && parse_mihomo(strip_utf8_bom(&response.body))
                .is_ok_and(|config| config.loopback_socks_mappings().is_empty())
    })
}

fn parse_fallback(
    url: &str,
    mihomo: Result<HttpResponse, FetchError>,
    xray: Result<HttpResponse, FetchError>,
) -> Result<Snapshot, FetchError> {
    let mihomo = success_response(mihomo?)?;
    if device_limit_reached(&mihomo) {
        return Err(FetchError::DeviceLimit);
    }
    let xray = match xray.and_then(success_response) {
        Ok(xray) => xray,
        Err(error) => return standalone_mihomo(url, &mihomo).unwrap_or(Err(error)),
    };
    let snapshot = match Snapshot::parse(strip_utf8_bom(&mihomo.body), strip_utf8_bom(&xray.body)) {
        Ok(snapshot) => snapshot,
        Err(_) => {
            if let Some(standalone) = standalone_mihomo(url, &mihomo) {
                return standalone;
            }
            return Err(
                if looks_like_non_config(&mihomo.body) || looks_like_non_config(&xray.body) {
                    FetchError::NotSubscription
                } else {
                    FetchError::InvalidConfig
                },
            );
        }
    };
    snapshot
        .with_merged_subscription_metadata(
            url,
            &mihomo.metadata,
            &xray.metadata,
            current_unix_time(),
        )
        .map_err(|_| FetchError::InvalidConfig)
}

/// Panels such as Remnawave serve a Mihomo YAML with native proxies but have
/// no single-object Xray representation (their Xray JSON is an array, their
/// default answer share links). When that YAML routes nothing into Xray - no
/// loopback SOCKS bridge - the Xray half carries no traffic, so it is optional
/// and an idle config runs in its place. `None` when Mihomo is invalid or does
/// bridge into Xray: then the provider's Xray response is required as before.
fn standalone_mihomo(url: &str, mihomo: &HttpResponse) -> Option<Result<Snapshot, FetchError>> {
    let body = strip_utf8_bom(&mihomo.body);
    if !parse_mihomo(body)
        .ok()?
        .loopback_socks_mappings()
        .is_empty()
    {
        return None;
    }
    Some(
        Snapshot::parse(body, IDLE_XRAY_CONFIG)
            .and_then(|snapshot| {
                snapshot.with_subscription_metadata(url, &mihomo.metadata, current_unix_time())
            })
            .map_err(|_| FetchError::InvalidConfig),
    )
}

/// Remnawave answers a device over its HWID limit with an empty `200` and
/// this header instead of an error status.
const HWID_LIMIT_HEADER: &str = "x-hwid-max-devices-reached";

fn device_limit_reached(response: &HttpResponse) -> bool {
    response
        .metadata
        .values(HWID_LIMIT_HEADER)
        .any(|value| value.trim().eq_ignore_ascii_case("true"))
}

/// When the per-core fallback also fails, report whichever failure tells the
/// user more. A panel that served HTML to the combined request and then 404 to
/// the per-core requests simply does not support MultiCore.
fn prefer_informative(native: FetchError, fallback: FetchError) -> FetchError {
    match (native, fallback) {
        (
            FetchError::NotSubscription,
            FetchError::HttpStatus(_) | FetchError::InvalidConfig | FetchError::NotSubscription,
        ) => FetchError::NotSubscription,
        (FetchError::HttpStatus(status), FetchError::HttpStatus(_)) => {
            FetchError::HttpStatus(status)
        }
        (_, fallback) => fallback,
    }
}

fn strip_utf8_bom(body: &[u8]) -> &[u8] {
    body.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(body)
}

fn first_significant_byte(body: &[u8]) -> Option<u8> {
    strip_utf8_bom(body)
        .iter()
        .copied()
        .find(|byte| !byte.is_ascii_whitespace())
}

fn looks_like_native_bundle(body: &[u8]) -> bool {
    first_significant_byte(body) == Some(b'[')
}

/// Panels answer unrecognised clients with an HTML page, a list of share
/// links (`vless://...`) or one Base64 blob. None of these is a config.
fn looks_like_non_config(body: &[u8]) -> bool {
    let body = strip_utf8_bom(body);
    match first_significant_byte(body) {
        None | Some(b'<') => return true,
        Some(b'{') => return false,
        _ => {}
    }
    let sample = String::from_utf8_lossy(&body[..body.len().min(4096)]);
    let first_line = sample.trim().lines().next().unwrap_or_default().trim();
    let share_link = first_line.split_once("://").is_some_and(|(scheme, _)| {
        !scheme.is_empty() && scheme.chars().all(|c| c.is_ascii_alphanumeric())
    });
    let base64_blob = first_line.len() >= 32
        && first_line.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'/' | b'=' | b'-' | b'_')
        });
    share_link || base64_blob
}

fn current_unix_time() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn parse_native(body: &[u8]) -> Result<Snapshot, FetchError> {
    let (mihomo, xray): (String, Box<RawValue>) =
        serde_json::from_slice(body).map_err(|_| FetchError::InvalidBundle)?;
    Snapshot::parse(mihomo.as_bytes(), xray.get().as_bytes()).map_err(|_| FetchError::InvalidBundle)
}

fn is_image_user_agent(user_agent: &str) -> bool {
    user_agent == UA_SERVICE_LOGO || user_agent == UA_GROUP_ICON
}

fn accept_for(user_agent: &str) -> &'static str {
    if is_image_user_agent(user_agent) {
        "image/png, image/jpeg, image/webp, image/svg+xml"
    } else if user_agent == UA_NATIVE {
        "application/vnd.multicore.bundle+json, application/json"
    } else {
        "application/json, application/yaml, text/yaml, text/plain"
    }
}

fn response_limit(user_agent: &str) -> usize {
    if is_image_user_agent(user_agent) {
        MAX_SERVICE_LOGO_BYTES
    } else {
        MAX_CONFIG_BYTES
    }
}

#[cfg(test)]
mod redirect_policy_tests {
    use super::*;

    fn url(value: &str) -> reqwest::Url {
        reqwest::Url::parse(value).unwrap()
    }

    #[test]
    fn follows_upgrades_and_same_scheme_moves() {
        assert!(redirect_allowed(
            &[url("http://panel.example/sub")],
            &url("https://panel.example/sub/")
        ));
        assert!(redirect_allowed(
            &[url("https://short.example/x")],
            &url("https://panel.example/api/sub/abc")
        ));
        assert!(redirect_allowed(
            &[url("http://127.0.0.1:8080/a")],
            &url("http://127.0.0.1:8080/b")
        ));
    }

    #[test]
    fn refuses_downgrades_private_targets_and_foreign_schemes() {
        assert!(!redirect_allowed(
            &[url("https://panel.example/sub")],
            &url("http://panel.example/sub")
        ));
        for target in [
            "https://127.0.0.1/x",
            "https://[::1]/x",
            "https://192.168.1.1/x",
            "https://localhost/x",
            "https://api.localhost./x",
        ] {
            assert!(
                !redirect_allowed(&[url("https://panel.example/sub")], &url(target)),
                "{target}"
            );
        }
        assert!(!redirect_allowed(
            &[url("https://panel.example/sub")],
            &url("file:///etc/passwd")
        ));
    }
}

#[cfg(test)]
mod logo_network_tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn logo_address_policy_rejects_non_global_and_mixed_answers() {
        for address in [
            "0.0.0.0",
            "10.0.0.1",
            "100.64.0.1",
            "64.0.0.1",
            "127.0.0.1",
            "169.254.1.1",
            "172.16.0.1",
            "192.168.0.1",
            "192.0.2.1",
            "192.88.99.1",
            "198.18.0.1",
            "224.0.0.1",
            "::",
            "::1",
            "fc00::1",
            "fe80::1",
            "ff02::1",
            "2001:db8::1",
            "2001:2::1",
            "2002::1",
            "3fff::1",
        ] {
            let ip = address.parse().unwrap();
            assert!(!is_global_ip(ip), "accepted {address}");
        }
        for address in ["8.8.8.8", "1.1.1.1", "2606:4700:4700::1111"] {
            assert!(is_global_ip(address.parse().unwrap()), "rejected {address}");
        }
        assert_eq!(
            approve_logo_addresses(vec![
                "1.1.1.1:443".parse().unwrap(),
                "127.0.0.1:443".parse().unwrap(),
            ]),
            Err(FetchError::Network)
        );
    }

    #[test]
    fn logo_addresses_from_a_fake_ip_vpn_are_accepted_but_not_private_ones() {
        for address in ["198.18.0.24:443", "198.19.255.1:443"] {
            let answer = vec![address.parse().unwrap()];
            assert_eq!(approve_logo_addresses(answer.clone()), Ok(answer));
        }
        for address in ["192.168.0.1:443", "127.0.0.1:443", "10.0.0.1:443"] {
            assert_eq!(
                approve_logo_addresses(vec![address.parse().unwrap()]),
                Err(FetchError::Network),
                "{address}"
            );
        }
    }

    #[tokio::test]
    async fn logo_dns_resolution_has_an_independent_bounded_timeout() {
        let started = std::time::Instant::now();
        let result = resolve_global_logo_addresses_with(
            "pending.example",
            443,
            Duration::from_millis(1),
            |_, _| std::future::pending(),
        )
        .await;
        assert_eq!(result, Err(FetchError::Network));
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[tokio::test]
    async fn pinned_logo_client_connects_only_to_supplied_address_without_identity_headers() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = vec![0_u8; 4096];
            let length = stream.read(&mut request).await.unwrap();
            let request = String::from_utf8_lossy(&request[..length]).to_ascii_lowercase();
            assert!(request.contains("host: public-logo.example"));
            assert!(request.contains("user-agent: multicore-service-logo"));
            for header in ["x-hwid:", "x-device-os:", "x-ver-os:", "x-device-model:"] {
                assert!(!request.contains(header));
            }
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                .await
                .unwrap();
        });
        let client = build_pinned_logo_client("public-logo.example", &[address]).unwrap();
        let response = perform_get(
            &client,
            &format!("http://public-logo.example:{}/logo", address.port()),
            UA_SERVICE_LOGO,
        )
        .await
        .unwrap();
        assert_eq!(response.status, 200);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn pinned_logo_transport_caps_declared_and_streamed_responses() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 2048];
            let _ = stream.read(&mut request).await.unwrap();
            stream
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        MAX_SERVICE_LOGO_BYTES + 1
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
        });
        let client = build_pinned_logo_client("public-logo.example", &[address]).unwrap();
        assert_eq!(
            perform_get(
                &client,
                &format!("http://public-logo.example:{}/logo", address.port()),
                UA_SERVICE_LOGO,
            )
            .await,
            Err(FetchError::TooLarge)
        );
        server.await.unwrap();

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 2048];
            let _ = stream.read(&mut request).await.unwrap();
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n")
                .await
                .unwrap();
            let chunk = [b'x'; 64 * 1024];
            for _ in 0..=(MAX_SERVICE_LOGO_BYTES / chunk.len()) {
                if stream.write_all(&chunk).await.is_err() {
                    break;
                }
            }
        });
        let client = build_pinned_logo_client("public-logo.example", &[address]).unwrap();
        assert_eq!(
            perform_get(
                &client,
                &format!("http://public-logo.example:{}/logo", address.port()),
                UA_SERVICE_LOGO,
            )
            .await,
            Err(FetchError::TooLarge)
        );
        server.await.unwrap();
    }
}

#[cfg(test)]
mod persistence_outcome_tests {
    use super::*;
    use crate::PersistentSnapshotStore;

    #[derive(Clone)]
    struct NativeBundleClient;

    impl HttpClient for NativeBundleClient {
        fn get<'a>(
            &'a self,
            _url: &'a str,
            _user_agent: &'static str,
        ) -> Pin<Box<dyn Future<Output = Result<HttpResponse, FetchError>> + Send + 'a>> {
            Box::pin(async move {
                let mihomo = "proxies: []\nproxy-groups: []\nrules: []\n";
                let bundle = format!(
                    "[{},{}]",
                    serde_json::to_string(mihomo).unwrap(),
                    r#"{"outbounds":[]}"#
                );
                Ok(HttpResponse::new(
                    200,
                    bundle.into_bytes(),
                    std::iter::empty::<(&str, &str)>(),
                ))
            })
        }
    }

    #[tokio::test]
    async fn refresh_returns_committed_snapshot_with_directory_sync_debt() {
        let root = tempfile::tempdir().unwrap();
        let store = PersistentSnapshotStore::open(root.path()).unwrap();
        store.inject_directory_sync_failure();

        let outcome = SubscriptionFetcher::new(NativeBundleClient)
            .refresh("https://subscription.invalid/private", &store)
            .await
            .expect("post-publication debt is a committed fetch outcome");
        assert!(outcome.maintenance_debt().is_some());
        let (committed, debt) = outcome.into_parts();
        assert!(debt.is_some());
        assert!(Arc::ptr_eq(&committed, &store.current().unwrap()));
        assert_eq!(
            committed.subscription_source_url(),
            Some("https://subscription.invalid/private")
        );
    }
}

#[cfg(test)]
mod diagnostics_tests {
    use super::*;
    use crate::PersistentSnapshotStore;

    #[test]
    fn safe_host_never_carries_a_scheme_path_query_or_token() {
        assert_eq!(
            safe_host("https://user:pw@provider.example:8443/sub?token=super-secret#frag"),
            "provider.example:8443"
        );
        assert_eq!(safe_host("http://plain.example/sub"), "plain.example");
        assert_eq!(safe_host("not a url at all"), "unknown-host");
        // The event pipeline's own redaction wipes any `scheme://...`-shaped
        // text wholesale, so the host must never come back with one.
        assert!(!safe_host("https://provider.example/sub?token=x").contains("://"));
    }

    #[derive(Clone)]
    struct PerFlavourClient;

    impl HttpClient for PerFlavourClient {
        fn get<'a>(
            &'a self,
            url: &'a str,
            user_agent: &'static str,
        ) -> Pin<Box<dyn Future<Output = Result<HttpResponse, FetchError>> + Send + 'a>> {
            let url = url.to_owned();
            Box::pin(async move {
                match user_agent {
                    UA_NATIVE => Ok(HttpResponse::new(
                        404,
                        b"not found".to_vec(),
                        std::iter::empty::<(&str, &str)>(),
                    )),
                    UA_MIHOMO => Err(FetchError::Tls),
                    UA_XRAY if url.contains("provider.example") => Err(FetchError::Timeout),
                    _ => Err(FetchError::Network),
                }
            })
        }
    }

    #[tokio::test]
    async fn refresh_with_diagnostics_reports_a_redacted_per_attempt_breakdown_on_failure() {
        let root = tempfile::tempdir().unwrap();
        let store = PersistentSnapshotStore::open(root.path()).unwrap();
        let (result, diagnostics) = SubscriptionFetcher::new(PerFlavourClient)
            .refresh_with_diagnostics("https://provider.example/sub?token=super-secret", &store)
            .await;
        assert!(result.is_err());
        assert_eq!(diagnostics.host, "provider.example");
        assert!(!diagnostics.host.contains("token"));
        assert_eq!(
            diagnostics.native,
            AttemptOutcome::Failed {
                reason: "http_status",
                status: Some(404),
            }
        );
        assert_eq!(
            diagnostics.mihomo,
            AttemptOutcome::Failed {
                reason: "tls_rejected",
                status: None,
            }
        );
        assert_eq!(
            diagnostics.xray,
            AttemptOutcome::Failed {
                reason: "timed_out",
                status: None,
            }
        );
        let rendered = diagnostics.to_string();
        assert!(!rendered.contains("token"), "{rendered}");
        assert!(!rendered.contains("://"), "{rendered}");
    }

    #[derive(Clone)]
    struct NativeSuccessClient;

    impl HttpClient for NativeSuccessClient {
        fn get<'a>(
            &'a self,
            _url: &'a str,
            _user_agent: &'static str,
        ) -> Pin<Box<dyn Future<Output = Result<HttpResponse, FetchError>> + Send + 'a>> {
            Box::pin(async move {
                let mihomo = "proxies: []\nproxy-groups: []\nrules: []\n";
                let bundle = format!(
                    "[{},{}]",
                    serde_json::to_string(mihomo).unwrap(),
                    r#"{"outbounds":[]}"#
                );
                Ok(HttpResponse::new(
                    200,
                    bundle.into_bytes(),
                    std::iter::empty::<(&str, &str)>(),
                ))
            })
        }
    }

    #[tokio::test]
    async fn refresh_with_diagnostics_skips_fallback_attempts_once_the_native_bundle_succeeds() {
        let root = tempfile::tempdir().unwrap();
        let store = PersistentSnapshotStore::open(root.path()).unwrap();
        let (result, diagnostics) = SubscriptionFetcher::new(NativeSuccessClient)
            .refresh_with_diagnostics("https://provider.example/sub", &store)
            .await;
        assert!(result.is_ok());
        assert_eq!(diagnostics.native, AttemptOutcome::Ok { status: 200 });
        assert_eq!(diagnostics.mihomo, AttemptOutcome::Skipped);
        assert_eq!(diagnostics.xray, AttemptOutcome::Skipped);
    }
}

#[cfg(test)]
mod negotiation_tests {
    use super::*;
    use crate::PersistentSnapshotStore;

    /// The shape of a Mihomo-only subscription (redacted): VLESS and Hysteria2 servers, an
    /// url-test group with an exclude filter, select groups holding groups, an include-all
    /// group with a filter, group icons.
    const MIHOMO_ONLY: &str = r#"mixed-port: 7890
mode: rule
proxies:
  - {name: "🇩🇪 Germany", type: vless, server: de.example.invalid, port: 443, uuid: 00000000-0000-4000-8000-000000000001, network: tcp, tls: true, flow: xtls-rprx-vision, servername: de.example.invalid, client-fingerprint: chrome}
  - {name: "🇳🇱 Netherlands", type: vless, server: nl.example.invalid, port: 443, uuid: 00000000-0000-4000-8000-000000000002, network: tcp, tls: true}
  - {name: "🇫🇮 Finland torrent", type: hysteria2, server: fi.example.invalid, port: 443, password: redacted}
proxy-groups:
  - {name: "⚡ Fastest", icon: "https://icons.example.invalid/fast.png", type: url-test, exclude-filter: "(?i)(backup|torrent)", lazy: true, tolerance: 500, url: "https://captive.apple.com/generate_204", interval: 300, proxies: ["🇩🇪 Germany", "🇳🇱 Netherlands", "🇫🇮 Finland torrent"]}
  - {name: "🌍 Abroad", icon: "https://icons.example.invalid/globe.png", type: select, proxies: ["⚡ Fastest", DIRECT, "🇩🇪 Germany", "🇳🇱 Netherlands", "🇫🇮 Finland torrent"]}
  - {name: "💬 Discord", type: select, proxies: ["🌍 Abroad", "⚡ Fastest", DIRECT, "🇩🇪 Germany"]}
  - {name: "⬇️ Torrent", type: select, include-all-proxies: true, filter: "(?i)(torrent)", proxies: [DIRECT]}
rules:
  - MATCH,🌍 Abroad
"#;

    /// Each flavour answers after its own delay (virtual time).
    #[derive(Clone)]
    struct TimedClient {
        native: (u64, u16, &'static str),
        mihomo: (u64, u16, &'static str),
        xray: (u64, u16, &'static str),
    }

    impl HttpClient for TimedClient {
        fn get<'a>(
            &'a self,
            _url: &'a str,
            user_agent: &'static str,
        ) -> Pin<Box<dyn Future<Output = Result<HttpResponse, FetchError>> + Send + 'a>> {
            let (delay, status, body) = match user_agent {
                UA_NATIVE => self.native,
                UA_MIHOMO => self.mihomo,
                _ => self.xray,
            };
            Box::pin(async move {
                tokio::time::sleep(Duration::from_millis(delay)).await;
                Ok(HttpResponse::new(
                    status,
                    body.as_bytes().to_vec(),
                    std::iter::empty::<(&str, &str)>(),
                ))
            })
        }
    }

    /// A panel that serves only Mihomo and answers every other request with a 502 after
    /// ~15 s took over a minute to refresh (and the refresh indicator spun all along).
    #[tokio::test(start_paused = true)]
    async fn a_mihomo_only_panel_slow_to_refuse_other_requests_refreshes_in_seconds() {
        let root = tempfile::tempdir().unwrap();
        let store = PersistentSnapshotStore::open(root.path()).unwrap();
        let client = TimedClient {
            native: (15_400, 502, ""),
            mihomo: (400, 200, MIHOMO_ONLY),
            xray: (15_400, 502, ""),
        };
        let started = tokio::time::Instant::now();
        let (result, diagnostics) = SubscriptionFetcher::new(client)
            .refresh_with_diagnostics("https://provider.example/sub", &store)
            .await;
        let elapsed = started.elapsed();
        assert!(result.is_ok(), "{diagnostics}");
        assert!(
            elapsed <= NATIVE_HEAD_START + XRAY_GRACE + Duration::from_millis(500),
            "{elapsed:?}"
        );
        assert_eq!(diagnostics.native, AttemptOutcome::Abandoned);
        assert_eq!(diagnostics.mihomo, AttemptOutcome::Ok { status: 200 });
        assert_eq!(diagnostics.xray, AttemptOutcome::Skipped);
    }

    #[tokio::test(start_paused = true)]
    async fn a_native_bundle_that_answers_late_still_wins() {
        let root = tempfile::tempdir().unwrap();
        let store = PersistentSnapshotStore::open(root.path()).unwrap();
        let client = TimedClient {
            native: (
                5_000,
                200,
                r#"["proxies: []\nproxy-groups: []\nrules: []\n",{"outbounds":[]}]"#,
            ),
            mihomo: (10_000, 200, MIHOMO_ONLY),
            xray: (10_000, 502, ""),
        };
        let started = tokio::time::Instant::now();
        let (result, diagnostics) = SubscriptionFetcher::new(client)
            .refresh_with_diagnostics("https://provider.example/sub", &store)
            .await;
        assert!(result.is_ok(), "{diagnostics}");
        assert!(started.elapsed() < Duration::from_secs(6));
        assert_eq!(diagnostics.native, AttemptOutcome::Ok { status: 200 });
        assert_eq!(diagnostics.mihomo, AttemptOutcome::Abandoned);
        assert_eq!(diagnostics.xray, AttemptOutcome::Abandoned);
    }

    #[tokio::test(start_paused = true)]
    async fn a_quick_native_refusal_still_goes_straight_to_the_per_core_requests() {
        let root = tempfile::tempdir().unwrap();
        let store = PersistentSnapshotStore::open(root.path()).unwrap();
        let client = TimedClient {
            native: (100, 404, "not found"),
            mihomo: (300, 200, MIHOMO_ONLY),
            xray: (300, 404, "not found"),
        };
        let (result, diagnostics) = SubscriptionFetcher::new(client)
            .refresh_with_diagnostics("https://provider.example/sub", &store)
            .await;
        assert!(result.is_ok(), "{diagnostics}");
        assert!(matches!(diagnostics.native, AttemptOutcome::Failed { .. }));
        assert_eq!(
            diagnostics.xray,
            AttemptOutcome::Failed {
                reason: "http_status",
                status: Some(404),
            }
        );
    }

    #[test]
    fn only_a_quick_429_or_5xx_is_asked_again() {
        let answer = |status| DetailedResponse {
            response: HttpResponse::new(status, Vec::new(), std::iter::empty::<(&str, &str)>()),
            retry_after: None,
        };
        assert_eq!(
            retry_delay_for_status(&answer(502), Duration::from_millis(300)),
            Some(RETRY_DELAY)
        );
        assert_eq!(
            retry_delay_for_status(&answer(429), Duration::from_secs(1)),
            Some(RETRY_DELAY)
        );
        // A gateway that gave up on its backend after 15 s.
        assert_eq!(
            retry_delay_for_status(&answer(502), Duration::from_millis(15_400)),
            None
        );
        assert_eq!(
            retry_delay_for_status(&answer(404), Duration::from_millis(10)),
            None
        );
    }
}

#[cfg(test)]
mod tunnel_proxy_tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    type Seen = Arc<std::sync::Mutex<Vec<String>>>;

    /// Answers every connection with `body`; returns its address and the request lines seen.
    async fn server(body: &'static str) -> (SocketAddr, Seen) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let seen: Seen = Arc::default();
        let log = Arc::clone(&seen);
        tokio::spawn(async move {
            while let Ok((mut stream, _)) = listener.accept().await {
                let log = Arc::clone(&log);
                tokio::spawn(async move {
                    let mut request = vec![0_u8; 4096];
                    let length = stream.read(&mut request).await.unwrap_or(0);
                    let line = String::from_utf8_lossy(&request[..length])
                        .lines()
                        .next()
                        .unwrap_or_default()
                        .to_owned();
                    log.lock().unwrap().push(line);
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = stream.write_all(response.as_bytes()).await;
                });
            }
        });
        (address, seen)
    }

    /// Accepts connections and never answers.
    async fn stalled_server() -> SocketAddr {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((stream, _)) = listener.accept().await {
                held.push(stream);
            }
        });
        address
    }

    fn client(tunnel: &TunnelProxy) -> ReqwestHttpClient {
        let root = tempfile::tempdir().unwrap();
        let identity = DeviceIdentity::load_or_create(root.path()).unwrap();
        ReqwestHttpClient::new(identity)
            .unwrap()
            .with_tunnel_proxy(tunnel.clone())
            .with_tunnel_attempt_timeout(Duration::from_secs(2))
    }

    #[tokio::test]
    async fn connected_the_subscription_is_downloaded_through_the_connection() {
        let (provider, provider_seen) = server("DIRECT").await;
        let (proxy, proxy_seen) = server("THROUGH-THE-CONNECTION").await;
        let tunnel = TunnelProxy::default();
        tunnel.set(Some(proxy));
        let url = format!("http://{provider}/sub");
        let response = client(&tunnel).get(&url, UA_MIHOMO).await.unwrap();
        assert_eq!(response.body, b"THROUGH-THE-CONNECTION");
        let proxy_seen = proxy_seen.lock().unwrap().clone();
        assert_eq!(proxy_seen.len(), 1, "{proxy_seen:?}");
        // An HTTP proxy is asked for the absolute URL.
        assert!(proxy_seen[0].contains(&url), "{proxy_seen:?}");
        assert!(provider_seen.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn disconnected_the_subscription_is_downloaded_directly() {
        let (provider, provider_seen) = server("DIRECT").await;
        let (proxy, proxy_seen) = server("THROUGH-THE-CONNECTION").await;
        let tunnel = TunnelProxy::default();
        tunnel.set(Some(proxy));
        tunnel.set(None);
        let url = format!("http://{provider}/sub");
        let response = client(&tunnel).get(&url, UA_MIHOMO).await.unwrap();
        assert_eq!(response.body, b"DIRECT");
        assert_eq!(provider_seen.lock().unwrap().len(), 1);
        assert!(proxy_seen.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_connection_that_refuses_falls_back_to_the_provider_directly() {
        let (provider, provider_seen) = server("DIRECT").await;
        // Nothing listens there any more.
        let closed = TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap()
            .local_addr()
            .unwrap();
        let tunnel = TunnelProxy::default();
        tunnel.set(Some(closed));
        let url = format!("http://{provider}/sub");
        let response = client(&tunnel).get(&url, UA_MIHOMO).await.unwrap();
        assert_eq!(response.body, b"DIRECT");
        assert_eq!(provider_seen.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_connection_that_stalls_falls_back_to_the_provider_directly_in_time() {
        let (provider, provider_seen) = server("DIRECT").await;
        let tunnel = TunnelProxy::default();
        tunnel.set(Some(stalled_server().await));
        let url = format!("http://{provider}/sub");
        let started = std::time::Instant::now();
        let response = client(&tunnel).get(&url, UA_MIHOMO).await.unwrap();
        assert_eq!(response.body, b"DIRECT");
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "{:?}",
            started.elapsed()
        );
        assert_eq!(provider_seen.lock().unwrap().len(), 1);
    }

    #[test]
    fn only_a_loopback_proxy_is_ever_used() {
        let tunnel = TunnelProxy::default();
        tunnel.set(Some("192.168.1.10:7890".parse().unwrap()));
        assert_eq!(tunnel.get(), None);
        tunnel.set(Some("127.0.0.1:7890".parse().unwrap()));
        assert_eq!(tunnel.get(), Some("127.0.0.1:7890".parse().unwrap()));
    }
}
