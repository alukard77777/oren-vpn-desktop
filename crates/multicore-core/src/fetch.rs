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
    DeviceIdentity, MAX_CONFIG_BYTES, MAX_SERVICE_LOGO_BYTES, Snapshot, SnapshotCommit,
    SnapshotSink, normalize_service_logo,
};

pub const UA_NATIVE: &str = "multicore-json-massive";
pub const UA_MIHOMO: &str = "multicore-mihomo";
pub const UA_XRAY: &str = "multicore-xray";
pub const UA_SERVICE_LOGO: &str = "multicore-service-logo";
const LOGO_DNS_TIMEOUT: Duration = Duration::from_secs(5);
const LOGO_TOTAL_TIMEOUT: Duration = Duration::from_secs(12);
const SUBSCRIPTION_CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const SUBSCRIPTION_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_SUBSCRIPTION_REDIRECTS: usize = 5;
const RETRY_DELAY: Duration = Duration::from_millis(800);
const MAX_RETRY_AFTER: Duration = Duration::from_secs(3);

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
    ) || ["servicename", "servicelogo"].iter().any(|suffix| {
        name.strip_suffix(suffix)
            .is_some_and(|prefix| !prefix.is_empty() && prefix.ends_with('-'))
    }) || name == "subscription-userinfo"
        || name.ends_with("-subscription-userinfo")
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
        let builder = || {
            reqwest::Client::builder()
                .default_headers(headers.clone())
                .redirect(subscription_redirect_policy())
                .connect_timeout(SUBSCRIPTION_CONNECT_TIMEOUT)
                .timeout(SUBSCRIPTION_REQUEST_TIMEOUT)
        };
        let subscription_client = builder()
            .no_proxy()
            .build()
            .map_err(|_| FetchError::Network)?;
        let system_proxy_client = builder().build().map_err(|_| FetchError::Network)?;
        Ok(Self {
            subscription_client,
            system_proxy_client,
        })
    }

    async fn get_subscription(
        &self,
        url: &str,
        user_agent: &'static str,
    ) -> Result<HttpResponse, FetchError> {
        match perform_get_detailed(&self.subscription_client, url, user_agent).await {
            Ok(response) => {
                if let Some(delay) = retry_delay_for_status(&response) {
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
            if user_agent == UA_SERVICE_LOGO {
                tokio::time::timeout(LOGO_TOTAL_TIMEOUT, self.get_service_logo(url))
                    .await
                    .map_err(|_| FetchError::Timeout)?
            } else {
                self.get_subscription(url, user_agent).await
            }
        })
    }
}

impl ReqwestHttpClient {
    async fn get_service_logo(&self, url: &str) -> Result<HttpResponse, FetchError> {
        let parsed = reqwest::Url::parse(url).map_err(|_| FetchError::Network)?;
        let host = parsed.host_str().ok_or(FetchError::Network)?;
        let port = parsed.port_or_known_default().ok_or(FetchError::Network)?;
        let addresses = resolve_global_logo_addresses(host, port).await?;
        let client = build_pinned_logo_client(host, &addresses)?;
        perform_get(&client, url, UA_SERVICE_LOGO).await
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
fn retry_delay_for_status(response: &DetailedResponse) -> Option<Duration> {
    let status = response.response.status;
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
    if addresses.is_empty() || addresses.iter().any(|address| !is_global_ip(address.ip())) {
        return Err(FetchError::Network);
    }
    Ok(addresses)
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
        let started = std::time::Instant::now();
        let host = safe_host(url);
        let native_response = self.client.get(url, UA_NATIVE).await;
        let native_outcome = AttemptOutcome::from_result(&native_response);

        let native_error = match native_response {
            Ok(response) if response.status_is_success() => {
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
                            self.fetch_optional_logo(&mut snapshot).await;
                            store.save(snapshot).map_err(|_| FetchError::Persistence)
                        }
                        Err(error) => Err(error),
                    };
                    let diagnostics = FetchDiagnostics {
                        host,
                        native: native_outcome,
                        mihomo: AttemptOutcome::Skipped,
                        xray: AttemptOutcome::Skipped,
                        elapsed_ms: u64::try_from(started.elapsed().as_millis())
                            .unwrap_or(u64::MAX),
                    };
                    return (outcome, diagnostics);
                }
                // HTML, share links, Base64 or plain YAML: the panel did not
                // recognise the combined request; try the per-core requests.
                FetchError::NotSubscription
            }
            Ok(response) => FetchError::HttpStatus(response.status),
            Err(error) => error,
        };

        let (mihomo, xray) = tokio::join!(
            self.client.get(url, UA_MIHOMO),
            self.client.get(url, UA_XRAY)
        );
        let mihomo_outcome = AttemptOutcome::from_result(&mihomo);
        let xray_outcome = AttemptOutcome::from_result(&xray);
        let outcome = match parse_fallback(url, mihomo, xray)
            .map_err(|error| prefer_informative(native_error, error))
        {
            Ok(mut snapshot) => {
                self.fetch_optional_logo(&mut snapshot).await;
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

    async fn fetch_optional_logo(&self, snapshot: &mut Snapshot) {
        let Some(url) = snapshot.take_subscription_logo_url() else {
            return;
        };
        let Ok(response) = self.client.get(&url, UA_SERVICE_LOGO).await else {
            return;
        };
        if response.status_is_success()
            && let Some(png) = normalize_service_logo(&response.body)
        {
            snapshot.set_service_logo_png(png);
        }
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

fn parse_fallback(
    url: &str,
    mihomo: Result<HttpResponse, FetchError>,
    xray: Result<HttpResponse, FetchError>,
) -> Result<Snapshot, FetchError> {
    let mihomo = success_response(mihomo?)?;
    let xray = success_response(xray?)?;
    Snapshot::parse(strip_utf8_bom(&mihomo.body), strip_utf8_bom(&xray.body))
        .map_err(|_| {
            if looks_like_non_config(&mihomo.body) || looks_like_non_config(&xray.body) {
                FetchError::NotSubscription
            } else {
                FetchError::InvalidConfig
            }
        })?
        .with_merged_subscription_metadata(
            url,
            &mihomo.metadata,
            &xray.metadata,
            current_unix_time(),
        )
        .map_err(|_| FetchError::InvalidConfig)
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

fn accept_for(user_agent: &str) -> &'static str {
    if user_agent == UA_SERVICE_LOGO {
        "image/png, image/jpeg, image/webp, image/svg+xml"
    } else if user_agent == UA_NATIVE {
        "application/vnd.multicore.bundle+json, application/json"
    } else {
        "application/json, application/yaml, text/yaml, text/plain"
    }
}

fn response_limit(user_agent: &str) -> usize {
    if user_agent == UA_SERVICE_LOGO {
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
