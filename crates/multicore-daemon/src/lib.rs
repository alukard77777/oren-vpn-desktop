pub mod elevated_controller;

use std::{
    collections::{HashMap, HashSet, VecDeque},
    ffi::OsStr,
    fmt, io,
    marker::PhantomData,
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use axum::{
    Json, Router,
    body::{Body, Bytes},
    extract::{Path, Query, State, rejection::JsonRejection},
    http::{Request, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{delete, get, post, put},
};
use base64::{Engine as _, engine::general_purpose};
use multicore_core::{
    AtomicSnapshot, AttemptOutcome, Component, ConnectionState as CoreConnectionState,
    DiagnosticStream, Event, FetchDiagnostics, FetchError, HttpClient, MAX_PROFILES,
    ProcessController, ProcessDiagnostics, ProfileError, ProfileId, ProfileStore, ProfileSummary,
    RuntimeCheckState, ServiceLogoLease, Severity, Snapshot, SnapshotCommit, StagedRuntime,
    StateMachine, SubscriptionFetcher, Supervisor,
};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use tokio::{net::TcpListener, sync::RwLock};
use tower_http::limit::RequestBodyLimitLayer;

const MAX_REQUEST_BODY_BYTES: usize = 1024 * 1024;
/// Maximum number of untrusted Mihomo groups exposed by the local UI API.
pub const MAX_CATALOG_GROUPS: usize = 64;
/// Maximum number of untrusted node names exposed for a single Mihomo group.
pub const MAX_CATALOG_NODES_PER_GROUP: usize = 256;
/// Maximum Unicode scalar count retained for an untrusted group or node name.
pub const MAX_CATALOG_NAME_CHARS: usize = 256;
/// Maximum number of latency results accepted and returned by one request.
pub const MAX_LATENCY_RESULTS: usize = 256;
/// Maximum number of in-flight requests to the Mihomo controller per latency operation.
pub const MAX_CONCURRENT_LATENCY_PROBES: usize = 8;
const LATENCY_PROBE_TIMEOUT: Duration = Duration::from_secs(3);
const LATENCY_BATCH_TIMEOUT: Duration = Duration::from_secs(8);
/// Maximum number of Mihomo-to-Xray loopback bridge mappings exposed in diagnostics.
pub const MAX_DIAGNOSTIC_MAPPINGS: usize = 256;
static NEXT_CORRELATION_ID: AtomicU64 = AtomicU64::new(1);
/// Maximum number of daemon events retained in memory. Older entries are evicted first.
pub const MAX_RETAINED_EVENTS: usize = 512;
/// Maximum number of events returned by one cursor request.
///
/// Pages contain the oldest retained events whose IDs are greater than `after`, so a client can
/// advance its cursor to the last returned ID without skipping retained entries.
pub const MAX_EVENT_PAGE_SIZE: usize = 256;
/// Maximum byte length of the single readiness line written for an owning launcher.
pub const MAX_READINESS_LINE_BYTES: usize = 80;

#[cfg(windows)]
const CONNECTING_MESSAGE: &str = "Запускается подключение.";
#[cfg(not(windows))]
const CONNECTING_MESSAGE: &str =
    "Запускается подключение. Подтвердите права администратора в системном окне.";

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectionState {
    #[default]
    Empty,
    Ready,
    Connected,
    Connecting,
    Error,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct StatusDto {
    pub state: ConnectionState,
    #[serde(default)]
    pub active_profile_id: Option<String>,
    pub profile: Option<String>,
    pub current_node: Option<String>,
    pub message: Option<String>,
    pub degraded: bool,
    pub subscription: Option<SubscriptionInfoDto>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SubscriptionInfoDto {
    pub source_name: String,
    #[serde(default)]
    pub display_name: String,
    #[serde(default)]
    pub uploaded_bytes: Option<u64>,
    pub downloaded_bytes: Option<u64>,
    #[serde(default)]
    pub used_bytes: Option<u64>,
    pub total_bytes: Option<u64>,
    pub expires_at_unix: Option<u64>,
    pub updated_at_unix: u64,
    #[serde(default)]
    pub refresh_interval_secs: Option<u64>,
    #[serde(default)]
    pub announcement_text: Option<String>,
    #[serde(default)]
    pub announcement_action_label: Option<String>,
    #[serde(default)]
    pub announcement_tone: Option<String>,
    #[serde(default)]
    pub home_available: bool,
    #[serde(default)]
    pub support_available: bool,
    #[serde(default)]
    pub announcement_action_available: bool,
    #[serde(default)]
    pub service_logo_path: Option<String>,
    pub refresh_available: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SubscriptionListDto {
    pub active_profile_id: Option<String>,
    pub profiles: Vec<SubscriptionProfileDto>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SubscriptionProfileDto {
    pub id: String,
    pub active: bool,
    pub available: bool,
    pub display_name: String,
    pub used_bytes: Option<u64>,
    pub total_bytes: Option<u64>,
    pub expires_at_unix: Option<u64>,
    pub service_logo_path: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ImportSubscriptionRequest {
    pub url: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct EventDto {
    pub id: u64,
    pub timestamp: String,
    pub level: String,
    pub component: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Value>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct EventPageDto {
    pub epoch: String,
    pub events: Vec<EventDto>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeCheckDto {
    #[default]
    Stopped,
    Starting,
    Ready,
    Failed,
    Unsupported,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DiagnosticLogDto {
    pub id: u64,
    pub timestamp: String,
    pub component: String,
    pub level: String,
    pub message: String,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProxyMappingDto {
    pub proxy_name: String,
    pub address: String,
    pub xray_label: String,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct DiagnosticsDto {
    pub xray: RuntimeCheckDto,
    pub mihomo: RuntimeCheckDto,
    pub tun: RuntimeCheckDto,
    pub logs: Vec<DiagnosticLogDto>,
    #[serde(default)]
    pub mappings: Vec<ProxyMappingDto>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CatalogDto {
    pub revision: u64,
    pub groups: Vec<GroupDto>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct GroupDto {
    pub id: String,
    pub label: String,
    pub selected: bool,
    pub nodes: Vec<NodeDto>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct NodeDto {
    pub id: String,
    pub label: String,
    pub selected: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delay_ms: Option<u64>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SelectionRequest {
    pub revision: u64,
    pub node_id: String,
    #[serde(skip)]
    pub group_id: String,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LatencyRequest {
    #[serde(default)]
    pub revision: Option<u64>,
    #[serde(default)]
    pub group_ids: Vec<String>,
    #[serde(default)]
    pub node_ids: Vec<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LatencyStatus {
    Ok,
    Timeout,
    Error,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct LatencyResultDto {
    pub group_id: String,
    pub node_id: String,
    pub latency_ms: Option<u64>,
    pub status: LatencyStatus,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct LatencyResponseDto {
    pub entries: Vec<LatencyResultDto>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ApiErrorBody {
    pub code: String,
    pub message: String,
    pub correlation_id: String,
}

#[derive(Clone, Debug)]
pub struct BackendError {
    profile_mutation: Option<ProfileMutationError>,
}

impl BackendError {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            profile_mutation: None,
        }
    }

    #[must_use]
    pub const fn from_profile_mutation(error: ProfileMutationError) -> Self {
        Self {
            profile_mutation: Some(error),
        }
    }

    #[must_use]
    pub const fn profile_mutation(&self) -> Option<ProfileMutationError> {
        self.profile_mutation
    }
}

impl Default for BackendError {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for BackendError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("операция backend не выполнена")
    }
}

impl std::error::Error for BackendError {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SelectionError {
    StaleRevision,
    NotConnected,
    UnknownGroup,
    UnknownNode,
    SelectorFailed,
    StorageFailed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProfileMutationError {
    NotFound,
    Connected,
    LimitReached,
    Storage,
    Committed,
    /// The provider could not be reached or answered with something unusable.
    Fetch(FetchError),
    /// The subscription was downloaded but its runtime could not be prepared.
    Prepare,
}

impl fmt::Display for ProfileMutationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("profile mutation was not completed")
    }
}

impl std::error::Error for ProfileMutationError {}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LatencyError {
    StaleRevision,
    NotConnected,
    UnknownGroup,
    UnknownNode,
    TooManyTargets,
}

impl fmt::Display for LatencyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Mihomo latency probe was not completed")
    }
}

impl std::error::Error for LatencyError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LatencyProbeError {
    Timeout,
    Unavailable,
}

impl fmt::Display for SelectionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Mihomo selection was not applied")
    }
}

impl std::error::Error for SelectionError {}

#[derive(Clone, Debug)]
pub struct SelectorError {
    _private: (),
}

impl SelectorError {
    #[must_use]
    pub const fn new() -> Self {
        Self { _private: () }
    }
}

impl Default for SelectorError {
    fn default() -> Self {
        Self::new()
    }
}

impl fmt::Display for SelectorError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Mihomo controller request failed")
    }
}

impl std::error::Error for SelectorError {}

#[async_trait]
pub trait MihomoSelector: Send + Sync + 'static {
    async fn select(&self, raw_group: &str, raw_node: &str) -> Result<(), SelectorError>;

    async fn probe_latency(&self, _raw_name: &str) -> Result<u64, LatencyProbeError> {
        Err(LatencyProbeError::Unavailable)
    }
}

struct UnavailableSelector;

#[async_trait]
impl MihomoSelector for UnavailableSelector {
    async fn select(&self, _raw_group: &str, _raw_node: &str) -> Result<(), SelectorError> {
        Err(SelectorError::new())
    }
}

pub struct MihomoHttpSelector {
    client: reqwest::Client,
    base_url: reqwest::Url,
    authorization: header::HeaderValue,
}

impl MihomoHttpSelector {
    pub fn new(address: SocketAddr, secret: &str) -> io::Result<Self> {
        if !address.ip().is_loopback() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Mihomo controller address must be loopback",
            ));
        }
        if secret.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Mihomo controller secret must not be empty",
            ));
        }
        let base_url = reqwest::Url::parse(&format!("http://{address}/"))
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
        let authorization = header::HeaderValue::from_str(&format!("Bearer {secret}"))
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
        let client = reqwest::Client::builder()
            .no_proxy()
            .connect_timeout(Duration::from_secs(1))
            .timeout(Duration::from_secs(3))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(io::Error::other)?;
        Ok(Self {
            client,
            base_url,
            authorization,
        })
    }
}

#[async_trait]
impl MihomoSelector for MihomoHttpSelector {
    async fn select(&self, raw_group: &str, raw_node: &str) -> Result<(), SelectorError> {
        let mut url = self.base_url.clone();
        url.path_segments_mut()
            .map_err(|_| SelectorError::new())?
            .extend(["proxies", raw_group]);
        let response = self
            .client
            .put(url.clone())
            .header(header::AUTHORIZATION, self.authorization.clone())
            .header(header::CONTENT_TYPE, "application/json")
            .body(
                serde_json::to_vec(&json!({ "name": raw_node }))
                    .map_err(|_| SelectorError::new())?,
            )
            .send()
            .await
            .map_err(|_| SelectorError::new())?;
        if response.status() != StatusCode::NO_CONTENT {
            return Err(SelectorError::new());
        }

        let mut response = self
            .client
            .get(url)
            .header(header::AUTHORIZATION, self.authorization.clone())
            .send()
            .await
            .map_err(|_| SelectorError::new())?;
        if response.status() != StatusCode::OK {
            return Err(SelectorError::new());
        }
        const MAX_PROXY_STATUS_BYTES: usize = 1024 * 1024;
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| SelectorError::new())? {
            if body.len().saturating_add(chunk.len()) > MAX_PROXY_STATUS_BYTES {
                return Err(SelectorError::new());
            }
            body.extend_from_slice(&chunk);
        }
        let live: serde_json::Value =
            serde_json::from_slice(&body).map_err(|_| SelectorError::new())?;
        if live.get("now").and_then(serde_json::Value::as_str) != Some(raw_node) {
            return Err(SelectorError::new());
        }

        let mut connections_url = self.base_url.clone();
        connections_url
            .path_segments_mut()
            .map_err(|_| SelectorError::new())?
            .push("connections");
        let _ = self
            .client
            .delete(connections_url)
            .header(header::AUTHORIZATION, self.authorization.clone())
            .send()
            .await;
        Ok(())
    }

    async fn probe_latency(&self, raw_name: &str) -> Result<u64, LatencyProbeError> {
        let mut url = self.base_url.clone();
        url.path_segments_mut()
            .map_err(|_| LatencyProbeError::Unavailable)?
            .extend(["proxies", raw_name, "delay"]);
        url.query_pairs_mut()
            .append_pair("timeout", &LATENCY_PROBE_TIMEOUT.as_millis().to_string())
            .append_pair("url", "https://www.gstatic.com/generate_204");
        let mut response = self
            .client
            .get(url)
            .header(header::AUTHORIZATION, self.authorization.clone())
            .send()
            .await
            .map_err(|error| {
                if error.is_timeout() {
                    LatencyProbeError::Timeout
                } else {
                    LatencyProbeError::Unavailable
                }
            })?;
        if response.status() == StatusCode::REQUEST_TIMEOUT
            || response.status() == StatusCode::GATEWAY_TIMEOUT
        {
            return Err(LatencyProbeError::Timeout);
        }
        if response.status() != StatusCode::OK {
            return Err(LatencyProbeError::Unavailable);
        }

        const MAX_DELAY_RESPONSE_BYTES: usize = 1024;
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| LatencyProbeError::Unavailable)?
        {
            if body.len().saturating_add(chunk.len()) > MAX_DELAY_RESPONSE_BYTES {
                return Err(LatencyProbeError::Unavailable);
            }
            body.extend_from_slice(&chunk);
        }
        serde_json::from_slice::<serde_json::Value>(&body)
            .ok()
            .and_then(|value| value.get("delay").and_then(serde_json::Value::as_u64))
            .ok_or(LatencyProbeError::Unavailable)
    }
}

async fn probe_latency_target(
    selector: Arc<dyn MihomoSelector>,
    permits: Arc<tokio::sync::Semaphore>,
    index: usize,
    group_id: String,
    node_id: String,
    raw_node: String,
) -> (usize, LatencyResultDto) {
    let outcome = tokio::time::timeout(LATENCY_PROBE_TIMEOUT, async {
        let _permit = permits
            .acquire_owned()
            .await
            .map_err(|_| LatencyProbeError::Unavailable)?;
        selector.probe_latency(&raw_node).await
    })
    .await;
    let (latency_ms, status) = match outcome {
        Ok(Ok(latency_ms)) => (Some(latency_ms), LatencyStatus::Ok),
        Ok(Err(LatencyProbeError::Timeout)) | Err(_) => (None, LatencyStatus::Timeout),
        Ok(Err(LatencyProbeError::Unavailable)) => (None, LatencyStatus::Error),
    };
    (
        index,
        LatencyResultDto {
            group_id,
            node_id,
            latency_ms,
            status,
        },
    )
}

#[async_trait]
pub trait Backend: Send + Sync + 'static {
    async fn status(&self) -> Result<StatusDto, BackendError>;
    async fn catalog(&self) -> Result<CatalogDto, BackendError>;
    async fn import_subscription(
        &self,
        request: ImportSubscriptionRequest,
    ) -> Result<StatusDto, BackendError>;
    async fn refresh_subscription(&self) -> Result<StatusDto, BackendError> {
        Err(BackendError::new())
    }
    async fn list_subscriptions(&self) -> Result<SubscriptionListDto, BackendError> {
        Err(BackendError::new())
    }
    async fn activate_subscription(&self, _id: ProfileId) -> Result<StatusDto, BackendError> {
        Err(BackendError::new())
    }
    async fn refresh_subscription_profile(
        &self,
        _id: ProfileId,
    ) -> Result<SubscriptionListDto, BackendError> {
        Err(BackendError::new())
    }
    async fn replace_subscription_source(
        &self,
        _id: ProfileId,
        _request: ImportSubscriptionRequest,
    ) -> Result<SubscriptionListDto, BackendError> {
        Err(BackendError::new())
    }
    async fn delete_subscription(&self, _id: ProfileId) -> Result<StatusDto, BackendError> {
        Err(BackendError::new())
    }
    async fn connect(&self) -> Result<StatusDto, BackendError>;
    async fn disconnect(&self) -> Result<StatusDto, BackendError>;
    async fn select(&self, _request: SelectionRequest) -> Result<CatalogDto, SelectionError> {
        Err(SelectionError::NotConnected)
    }
    async fn latencies(
        &self,
        _request: LatencyRequest,
    ) -> Result<LatencyResponseDto, LatencyError> {
        Err(LatencyError::NotConnected)
    }
    /// Returns at most [`MAX_EVENT_PAGE_SIZE`] oldest retained events with IDs greater than
    /// `after`. A stale cursor resumes at the oldest event still present in the bounded buffer.
    async fn events_after(&self, after: u64) -> Result<Vec<EventDto>, BackendError>;
    async fn diagnostics(&self) -> Result<DiagnosticsDto, BackendError> {
        Ok(DiagnosticsDto::default())
    }
}

#[derive(Clone)]
struct AppState {
    backend: Arc<dyn Backend>,
    event_epoch: Arc<str>,
    /// The fully-formed local dashboard URL, secret included. Never logged or included in any
    /// other response; only ever handed back to an already-authenticated caller, and only while a
    /// live controller session exists. `AppState` deliberately has no `Debug` impl so this can
    /// never be leaked through an accidental `{:?}` format.
    dashboard_url: Option<Arc<str>>,
}

#[derive(Clone)]
struct AuthState {
    token_hash: [u8; 32],
}

/// Builds the local daemon HTTP API. No CORS layer is installed intentionally.
pub fn router<B>(backend: B, bearer_token: impl AsRef<str>) -> Router
where
    B: Backend,
{
    try_router(backend, bearer_token)
        .expect("operating-system randomness unavailable for daemon event epoch")
}

/// Fallible production constructor. Event epochs are generated exclusively from OS randomness.
pub fn try_router<B>(backend: B, bearer_token: impl AsRef<str>) -> Result<Router, getrandom::Error>
where
    B: Backend,
{
    Ok(router_with_event_epoch(
        backend,
        bearer_token,
        generate_event_epoch()?,
    ))
}

/// Fallible production constructor that also exposes the authenticated `/v1/dashboard-url`
/// endpoint, returning `dashboard_url` to the caller only while a live controller session exists
/// (see [`ConnectionState::Connected`]). `dashboard_url` should be the fully-formed local Mihomo
/// dashboard URL, including its per-session secret; it is never logged and never returned from any
/// other endpoint.
pub fn try_router_with_dashboard_url<B>(
    backend: B,
    bearer_token: impl AsRef<str>,
    dashboard_url: impl Into<Arc<str>>,
) -> Result<Router, getrandom::Error>
where
    B: Backend,
{
    Ok(build_router(
        backend,
        bearer_token,
        generate_event_epoch()?,
        Some(dashboard_url.into()),
    ))
}

/// Builds the local daemon HTTP API with an explicit event epoch.
///
/// Production callers should use [`try_router`]. This constructor exists so transport tests can
/// use a deterministic restart identity.
pub fn router_with_event_epoch<B>(
    backend: B,
    bearer_token: impl AsRef<str>,
    event_epoch: impl Into<String>,
) -> Router
where
    B: Backend,
{
    build_router(backend, bearer_token, event_epoch, None)
}

fn build_router<B>(
    backend: B,
    bearer_token: impl AsRef<str>,
    event_epoch: impl Into<String>,
    dashboard_url: Option<Arc<str>>,
) -> Router
where
    B: Backend,
{
    let app_state = AppState {
        backend: Arc::new(backend),
        event_epoch: Arc::from(event_epoch.into()),
        dashboard_url,
    };
    let auth_state = AuthState {
        token_hash: Sha256::digest(bearer_token.as_ref().as_bytes()).into(),
    };

    let v1 = Router::new()
        .route("/status", get(status))
        .route("/catalog", get(catalog))
        .route("/latencies", post(latencies))
        .route("/selections/{group_id}", put(select_node))
        .route("/subscriptions", get(list_subscriptions))
        .route("/subscriptions/import", post(import_subscription))
        .route("/subscriptions/refresh", post(refresh_subscription))
        .route(
            "/subscriptions/{profile_id}/activate",
            put(activate_subscription),
        )
        .route(
            "/subscriptions/{profile_id}/refresh",
            post(refresh_subscription_profile),
        )
        .route(
            "/subscriptions/{profile_id}/source",
            put(replace_subscription_source),
        )
        .route("/subscriptions/{profile_id}", delete(delete_subscription))
        .route("/connect", post(connect))
        .route("/disconnect", post(disconnect))
        .route("/events", get(events))
        .route("/diagnostics", get(diagnostics))
        .route("/dashboard-url", get(dashboard_url_handler))
        .fallback(not_found)
        .layer(RequestBodyLimitLayer::new(MAX_REQUEST_BODY_BYTES))
        .layer(middleware::from_fn(normalize_errors))
        .layer(middleware::from_fn_with_state(auth_state, authorize))
        .with_state(app_state);

    Router::new().nest("/v1", v1)
}

fn generate_event_epoch() -> Result<String, getrandom::Error> {
    let mut random = [0_u8; 16];
    getrandom::fill(&mut random)?;
    Ok(random.iter().map(|byte| format!("{byte:02x}")).collect())
}

async fn normalize_errors(request: Request<Body>, next: Next) -> Response {
    let response = next.run(request).await;
    if !response.status().is_client_error() && !response.status().is_server_error() {
        return response;
    }
    if response
        .headers()
        .get(header::CONTENT_TYPE)
        .is_some_and(|value| value.as_bytes().starts_with(b"application/json"))
    {
        return response;
    }

    match response.status() {
        StatusCode::METHOD_NOT_ALLOWED => ApiError::new(
            StatusCode::METHOD_NOT_ALLOWED,
            "method_not_allowed",
            "Метод не поддерживается. Проверьте HTTP-метод запроса.",
        )
        .into_response(),
        StatusCode::PAYLOAD_TOO_LARGE => ApiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "payload_too_large",
            "Тело запроса превышает 1 МиБ. Уменьшите запрос и повторите попытку.",
        )
        .into_response(),
        status => ApiError::new(
            status,
            "request_failed",
            "Запрос не выполнен. Проверьте его параметры и повторите попытку.",
        )
        .into_response(),
    }
}

/// Binds only to an IP loopback interface.
pub async fn bind_loopback(address: SocketAddr) -> io::Result<TcpListener> {
    if !address.ip().is_loopback() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "адрес daemon должен быть loopback; используйте 127.0.0.1 или ::1",
        ));
    }
    TcpListener::bind(address).await
}

/// Publishes the resolved daemon address to an owning launcher when explicitly enabled.
///
/// The bearer token is deliberately not an input to this function, so the bounded stdout
/// protocol can contain only the literal socket address. `Ok(false)` means publication was not
/// enabled; the writer is left untouched in that case.
pub fn publish_readiness_if_enabled<W>(
    writer: &mut W,
    address: SocketAddr,
    ready_stdout: Option<&OsStr>,
) -> io::Result<bool>
where
    W: io::Write,
{
    if ready_stdout != Some(OsStr::new("1")) {
        return Ok(false);
    }
    if !address.ip().is_loopback() || address.port() == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "readiness address must be a loopback SocketAddr with a nonzero port",
        ));
    }

    let line = format!("MULTICORE_READY {address}\n");
    if !line.is_ascii() || line.len() > MAX_READINESS_LINE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "readiness line exceeds the bounded ASCII protocol",
        ));
    }
    writer.write_all(line.as_bytes())?;
    writer.flush()?;
    Ok(true)
}

async fn authorize(State(auth): State<AuthState>, request: Request<Body>, next: Next) -> Response {
    let supplied = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .filter(|token| !token.is_empty());

    let authenticated = supplied.is_some_and(|token| {
        let supplied_hash: [u8; 32] = Sha256::digest(token.as_bytes()).into();
        bool::from(supplied_hash.ct_eq(&auth.token_hash))
    });

    if authenticated {
        next.run(request).await
    } else {
        ApiError::unauthorized().into_response()
    }
}

async fn status(State(state): State<AppState>, body: Bytes) -> Result<Json<StatusDto>, ApiError> {
    reject_nonempty_body(&body)?;
    state
        .backend
        .status()
        .await
        .map(Json)
        .map_err(|_| ApiError::backend("Не удалось получить состояние. Повторите попытку."))
}

async fn catalog(State(state): State<AppState>, body: Bytes) -> Result<Json<CatalogDto>, ApiError> {
    reject_nonempty_body(&body)?;
    state
        .backend
        .catalog()
        .await
        .map(Json)
        .map_err(|_| ApiError::backend("Не удалось получить каталог. Повторите попытку."))
}

async fn latencies(
    State(state): State<AppState>,
    body: Bytes,
) -> Result<Json<LatencyResponseDto>, ApiError> {
    let request = if body.is_empty() {
        LatencyRequest::default()
    } else {
        serde_json::from_slice(&body).map_err(|_| {
            ApiError::bad_request("Некорректное тело запроса. Проверьте JSON и поля целей.")
        })?
    };
    state
        .backend
        .latencies(request)
        .await
        .map(Json)
        .map_err(|error| match error {
            LatencyError::StaleRevision => ApiError::new(
                StatusCode::CONFLICT,
                "stale_revision",
                "Каталог изменился. Обновите список маршрутов и повторите проверку.",
            ),
            LatencyError::NotConnected => ApiError::new(
                StatusCode::CONFLICT,
                "not_connected",
                "Сначала установите подключение.",
            ),
            LatencyError::UnknownGroup => ApiError::new(
                StatusCode::NOT_FOUND,
                "unknown_group",
                "Группа маршрутов не найдена. Обновите каталог.",
            ),
            LatencyError::UnknownNode => ApiError::new(
                StatusCode::NOT_FOUND,
                "unknown_node",
                "Узел не найден. Обновите каталог.",
            ),
            LatencyError::TooManyTargets => ApiError::new(
                StatusCode::UNPROCESSABLE_ENTITY,
                "too_many_targets",
                "Запрос содержит слишком много целей проверки.",
            ),
        })
}

async fn select_node(
    State(state): State<AppState>,
    Path(group_id): Path<String>,
    payload: Result<Json<SelectionRequest>, JsonRejection>,
) -> Result<Json<CatalogDto>, ApiError> {
    let Json(mut request) = payload.map_err(ApiError::from_json_rejection)?;
    request.group_id = group_id;
    state
        .backend
        .select(request)
        .await
        .map(Json)
        .map_err(|error| match error {
            SelectionError::StaleRevision => ApiError::new(
                StatusCode::CONFLICT,
                "stale_revision",
                "Каталог изменился. Обновите список маршрутов и повторите выбор.",
            ),
            SelectionError::NotConnected => ApiError::new(
                StatusCode::CONFLICT,
                "not_connected",
                "Сначала установите подключение.",
            ),
            SelectionError::UnknownGroup => ApiError::new(
                StatusCode::NOT_FOUND,
                "unknown_group",
                "Группа маршрутов не найдена. Обновите каталог.",
            ),
            SelectionError::UnknownNode => ApiError::new(
                StatusCode::NOT_FOUND,
                "unknown_node",
                "Узел не найден. Обновите каталог.",
            ),
            SelectionError::SelectorFailed => ApiError::new(
                StatusCode::BAD_GATEWAY,
                "selector_failed",
                "Mihomo не применил выбор маршрута. Повторите попытку.",
            ),
            SelectionError::StorageFailed => ApiError::new(
                StatusCode::INTERNAL_SERVER_ERROR,
                "selection_storage_failed",
                "Не удалось сохранить выбор маршрута. Повторите попытку.",
            ),
        })
}

async fn list_subscriptions(
    State(state): State<AppState>,
    body: Bytes,
) -> Result<Json<SubscriptionListDto>, ApiError> {
    reject_nonempty_body(&body)?;
    state
        .backend
        .list_subscriptions()
        .await
        .map(Json)
        .map_err(|error| profile_api_error(error, "Не удалось получить список подписок."))
}

async fn activate_subscription(
    State(state): State<AppState>,
    Path(profile_id): Path<String>,
    body: Bytes,
) -> Result<Json<StatusDto>, ApiError> {
    reject_nonempty_body(&body)?;
    let profile_id = parse_profile_id(&profile_id)?;
    state
        .backend
        .activate_subscription(profile_id)
        .await
        .map(Json)
        .map_err(|error| profile_api_error(error, "Не удалось активировать профиль."))
}

async fn refresh_subscription_profile(
    State(state): State<AppState>,
    Path(profile_id): Path<String>,
    body: Bytes,
) -> Result<Json<SubscriptionListDto>, ApiError> {
    reject_nonempty_body(&body)?;
    let profile_id = parse_profile_id(&profile_id)?;
    state
        .backend
        .refresh_subscription_profile(profile_id)
        .await
        .map(Json)
        .map_err(|error| profile_api_error(error, "Не удалось обновить профиль."))
}

async fn replace_subscription_source(
    State(state): State<AppState>,
    Path(profile_id): Path<String>,
    payload: Result<Json<ImportSubscriptionRequest>, JsonRejection>,
) -> Result<Json<SubscriptionListDto>, ApiError> {
    let profile_id = parse_profile_id(&profile_id)?;
    let Json(request) = payload.map_err(ApiError::from_json_rejection)?;
    let Some(url) = valid_subscription_source_url(&request.url) else {
        return Err(ApiError::bad_request(
            "Укажите корректный HTTP(S) URL подписки.",
        ));
    };
    state
        .backend
        .replace_subscription_source(profile_id, ImportSubscriptionRequest { url })
        .await
        .map(Json)
        .map_err(|error| profile_api_error(error, "Не удалось изменить ссылку подписки."))
}

async fn delete_subscription(
    State(state): State<AppState>,
    Path(profile_id): Path<String>,
    body: Bytes,
) -> Result<Json<StatusDto>, ApiError> {
    reject_nonempty_body(&body)?;
    let profile_id = parse_profile_id(&profile_id)?;
    state
        .backend
        .delete_subscription(profile_id)
        .await
        .map(Json)
        .map_err(|error| profile_api_error(error, "Не удалось удалить профиль."))
}

fn parse_profile_id(value: &str) -> Result<ProfileId, ApiError> {
    ProfileId::parse(value).map_err(|_| profile_not_found())
}

fn valid_subscription_source_url(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() || value.len() > 8 * 1024 || value.chars().any(char::is_control) {
        return None;
    }
    let parsed = reqwest::Url::parse(value).ok()?;
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
        return None;
    }
    Some(value.to_owned())
}

/// Stays below the desktop's 150 s subscription request timeout.
const SUBSCRIPTION_FETCH_DEADLINE: Duration = Duration::from_secs(120);

fn subscription_fetch_reason(error: FetchError) -> String {
    match error {
        FetchError::Network => "network".to_owned(),
        FetchError::Timeout => "timeout".to_owned(),
        FetchError::Tls => "tls".to_owned(),
        FetchError::Redirect => "redirect".to_owned(),
        FetchError::HttpStatus(status) => format!("http {status}"),
        FetchError::NotSubscription => "not a subscription".to_owned(),
        FetchError::InvalidBundle | FetchError::InvalidConfig => "invalid config".to_owned(),
        FetchError::Persistence => "persistence".to_owned(),
        FetchError::TooLarge => "too large".to_owned(),
    }
}

fn subscription_fetch_api_error(error: FetchError) -> ApiError {
    let (status, code, message): (StatusCode, &str, String) = match error {
        FetchError::Network => (
            StatusCode::BAD_GATEWAY,
            "subscription_unreachable",
            "Сервер подписки недоступен. Проверьте подключение к интернету и повторите попытку."
                .to_owned(),
        ),
        FetchError::Timeout => (
            StatusCode::GATEWAY_TIMEOUT,
            "subscription_timeout",
            "Сервер подписки не ответил вовремя. Повторите попытку чуть позже.".to_owned(),
        ),
        FetchError::Tls => (
            StatusCode::BAD_GATEWAY,
            "subscription_tls",
            "Не удалось установить защищённое соединение с сервером подписки: сертификат не принят. Проверьте ссылку или обратитесь к провайдеру."
                .to_owned(),
        ),
        FetchError::Redirect => (
            StatusCode::BAD_GATEWAY,
            "subscription_redirect",
            "Сервер подписки перенаправил запрос небезопасно или слишком много раз.".to_owned(),
        ),
        FetchError::HttpStatus(status) => (
            StatusCode::BAD_GATEWAY,
            match status {
                401 | 403 => "subscription_forbidden",
                404 | 410 => "subscription_not_found",
                429 => "subscription_rate_limited",
                500..=599 => "subscription_server_error",
                _ => "subscription_http_status",
            },
            match status {
                401 | 403 => format!(
                    "Сервер подписки отказал в доступе (HTTP {status}). Проверьте ссылку и срок действия подписки."
                ),
                404 | 410 => format!(
                    "Подписка не найдена (HTTP {status}). Проверьте, что ссылка скопирована полностью."
                ),
                429 => "Сервер подписки временно ограничил запросы (HTTP 429). Повторите попытку через минуту.".to_owned(),
                500..=599 => format!(
                    "Сервер подписки временно не работает (HTTP {status}). Повторите попытку позже."
                ),
                _ => format!("Сервер подписки вернул ошибку HTTP {status}."),
            },
        ),
        FetchError::NotSubscription => (
            StatusCode::UNPROCESSABLE_ENTITY,
            "subscription_not_multicore",
            "По ссылке пришла веб-страница или список ссылок, а не подписка MultiCore. Скопируйте ссылку подписки для приложения у провайдера."
                .to_owned(),
        ),
        FetchError::InvalidBundle | FetchError::InvalidConfig => (
            StatusCode::UNPROCESSABLE_ENTITY,
            "subscription_invalid",
            "Провайдер прислал повреждённую или неподдерживаемую конфигурацию.".to_owned(),
        ),
        FetchError::TooLarge => (
            StatusCode::UNPROCESSABLE_ENTITY,
            "subscription_too_large",
            "Ответ сервера подписки слишком большой.".to_owned(),
        ),
        FetchError::Persistence => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "subscription_storage",
            "Не удалось сохранить подписку на диск. Проверьте свободное место.".to_owned(),
        ),
    };
    ApiError::new(status, code, &message)
}

fn profile_api_error(error: BackendError, internal_message: &str) -> ApiError {
    match error.profile_mutation() {
        Some(ProfileMutationError::Fetch(error)) => subscription_fetch_api_error(error),
        Some(ProfileMutationError::Prepare) => ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "subscription_prepare_failed",
            "Подписка загружена, но подготовить её к запуску не удалось. Проверьте сетевое подключение и повторите попытку.",
        ),
        Some(ProfileMutationError::NotFound) => profile_not_found(),
        Some(ProfileMutationError::Connected) => ApiError::new(
            StatusCode::CONFLICT,
            "profile_busy",
            "Отключитесь перед изменением профиля.",
        ),
        Some(ProfileMutationError::LimitReached) => ApiError::new(
            StatusCode::CONFLICT,
            "profile_limit",
            "Достигнут предел количества профилей.",
        ),
        Some(ProfileMutationError::Committed) => ApiError::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "profile_mutation_ambiguous",
            "Изменение могло сохраниться. Обновите состояние подписок.",
        ),
        Some(ProfileMutationError::Storage) | None => ApiError::backend(internal_message),
    }
}

fn profile_not_found() -> ApiError {
    ApiError::new(
        StatusCode::NOT_FOUND,
        "profile_not_found",
        "Профиль не найден. Обновите список подписок.",
    )
}

async fn import_subscription(
    State(state): State<AppState>,
    payload: Result<Json<ImportSubscriptionRequest>, JsonRejection>,
) -> Result<Json<StatusDto>, ApiError> {
    let Json(request) = payload.map_err(ApiError::from_json_rejection)?;
    if request.url.trim().is_empty() {
        return Err(ApiError::bad_request(
            "Ссылка подписки пуста. Укажите URL и повторите попытку.",
        ));
    }
    state
        .backend
        .import_subscription(request)
        .await
        .map(Json)
        .map_err(|error| {
            if error.profile_mutation() == Some(ProfileMutationError::Connected) {
                return ApiError::new(
                    StatusCode::CONFLICT,
                    "import_requires_disconnect",
                    "Отключите VPN, чтобы добавить подписку: новая подписка сразу становится активной.",
                );
            }
            profile_api_error(
                error,
                "Не удалось импортировать подписку. Проверьте ссылку и повторите попытку.",
            )
        })
}

async fn refresh_subscription(
    State(state): State<AppState>,
    body: Bytes,
) -> Result<Json<StatusDto>, ApiError> {
    reject_nonempty_body(&body)?;
    state
        .backend
        .refresh_subscription()
        .await
        .map(Json)
        .map_err(|error| {
            profile_api_error(
                error,
                "Не удалось обновить подписку. Если профиль старый, укажите ссылку ещё раз.",
            )
        })
}

async fn connect(State(state): State<AppState>, body: Bytes) -> Result<Json<StatusDto>, ApiError> {
    reject_nonempty_body(&body)?;
    state
        .backend
        .connect()
        .await
        .map(Json)
        .map_err(|_| ApiError::backend("Не удалось подключиться. Повторите попытку."))
}

async fn disconnect(
    State(state): State<AppState>,
    body: Bytes,
) -> Result<Json<StatusDto>, ApiError> {
    reject_nonempty_body(&body)?;
    state
        .backend
        .disconnect()
        .await
        .map(Json)
        .map_err(|_| ApiError::backend("Не удалось отключиться. Повторите попытку."))
}

#[derive(Deserialize)]
struct EventsQuery {
    after: u64,
    epoch: Option<String>,
}

async fn events(
    State(state): State<AppState>,
    query: Result<Query<EventsQuery>, axum::extract::rejection::QueryRejection>,
    body: Bytes,
) -> Result<Json<EventPageDto>, ApiError> {
    reject_nonempty_body(&body)?;
    let Query(query) = query.map_err(|_| {
        ApiError::bad_request("Некорректный параметр after. Укажите целое неотрицательное число.")
    })?;
    let after = match query.epoch.as_deref() {
        Some(epoch) if epoch != state.event_epoch.as_ref() => 0,
        Some(_) | None => query.after,
    };
    let mut events = state
        .backend
        .events_after(after)
        .await
        .map_err(|_| ApiError::backend("Не удалось получить события. Повторите попытку."))?;
    events.truncate(MAX_EVENT_PAGE_SIZE);
    Ok(Json(EventPageDto {
        epoch: state.event_epoch.to_string(),
        events,
    }))
}

async fn diagnostics(
    State(state): State<AppState>,
    body: Bytes,
) -> Result<Json<DiagnosticsDto>, ApiError> {
    reject_nonempty_body(&body)?;
    state
        .backend
        .diagnostics()
        .await
        .map(Json)
        .map_err(|_| ApiError::backend("Не удалось получить диагностику. Повторите попытку."))
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DashboardUrlDto {
    pub url: String,
}

/// Returns the fully-formed, secret-bearing local Mihomo dashboard URL, but only while a live
/// controller session exists (`ConnectionState::Connected`). The URL is never included in
/// `/status`, `/events`, or any diagnostic output, and this handler never logs it: an error path
/// here must not accidentally embed the URL in any tracing/log output added later.
async fn dashboard_url_handler(
    State(state): State<AppState>,
    body: Bytes,
) -> Result<Json<DashboardUrlDto>, ApiError> {
    reject_nonempty_body(&body)?;
    let Some(dashboard_url) = state.dashboard_url.clone() else {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "dashboard_unavailable",
            "Панель Mihomo недоступна в этой сборке.",
        ));
    };
    let status = state
        .backend
        .status()
        .await
        .map_err(|_| ApiError::backend("Не удалось получить состояние. Повторите попытку."))?;
    if status.state != ConnectionState::Connected {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "not_connected",
            "Подключитесь, чтобы открыть панель Mihomo.",
        ));
    }
    Ok(Json(DashboardUrlDto {
        url: dashboard_url.to_string(),
    }))
}

fn reject_nonempty_body(body: &Bytes) -> Result<(), ApiError> {
    if body.is_empty() {
        Ok(())
    } else {
        Err(ApiError::bad_request(
            "Тело для этого запроса не допускается. Удалите тело и повторите попытку.",
        ))
    }
}

async fn not_found() -> ApiError {
    ApiError::new(
        StatusCode::NOT_FOUND,
        "route_not_found",
        "Маршрут не найден. Проверьте путь запроса.",
    )
}

struct ApiError {
    status: StatusCode,
    body: ApiErrorBody,
}

impl ApiError {
    fn new(status: StatusCode, code: &str, message: &str) -> Self {
        let id = NEXT_CORRELATION_ID.fetch_add(1, Ordering::Relaxed);
        Self {
            status,
            body: ApiErrorBody {
                code: code.to_owned(),
                message: message.to_owned(),
                correlation_id: format!("daemon-{id:016x}"),
            },
        }
    }

    fn unauthorized() -> Self {
        Self::new(
            StatusCode::UNAUTHORIZED,
            "unauthorized",
            "Требуется авторизация. Передайте действующий Bearer-токен.",
        )
    }

    fn bad_request(message: &str) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "invalid_request", message)
    }

    fn backend(message: &str) -> Self {
        Self::new(StatusCode::INTERNAL_SERVER_ERROR, "backend_error", message)
    }

    fn from_json_rejection(rejection: JsonRejection) -> Self {
        let status = rejection.status();
        if status == StatusCode::PAYLOAD_TOO_LARGE {
            Self::new(
                status,
                "payload_too_large",
                "Тело запроса превышает 1 МиБ. Уменьшите запрос и повторите попытку.",
            )
        } else {
            Self::bad_request("Некорректное тело запроса. Проверьте JSON и обязательные поля.")
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(self.body)).into_response()
    }
}

struct EventBuffer<T> {
    entries: VecDeque<(u64, T)>,
    next_event_id: u64,
}

impl<T> Default for EventBuffer<T> {
    fn default() -> Self {
        Self {
            entries: VecDeque::with_capacity(MAX_RETAINED_EVENTS),
            next_event_id: 1,
        }
    }
}

impl<T> EventBuffer<T> {
    fn push_with(&mut self, build: impl FnOnce(u64) -> T) -> u64 {
        let id = self.next_event_id;
        self.next_event_id = self
            .next_event_id
            .checked_add(1)
            .expect("daemon event ID space exhausted");
        if self.entries.len() == MAX_RETAINED_EVENTS {
            self.entries.pop_front();
        }
        self.entries.push_back((id, build(id)));
        id
    }

    fn after(&self, cursor: u64) -> impl Iterator<Item = (u64, &T)> {
        self.entries
            .iter()
            .filter(move |(id, _)| *id > cursor)
            .take(MAX_EVENT_PAGE_SIZE)
            .map(|(id, event)| (*id, event))
    }
}

pub struct PreparedController<P: ProcessController> {
    controller: Arc<P>,
    runtime: Option<StagedRuntime>,
}

impl<P: ProcessController> PreparedController<P> {
    pub fn with_runtime(controller: Arc<P>, runtime: StagedRuntime) -> Self {
        Self {
            controller,
            runtime: Some(runtime),
        }
    }

    fn plain(controller: Arc<P>) -> Self {
        Self {
            controller,
            runtime: None,
        }
    }

    fn controller(&self) -> Arc<P> {
        self.controller.clone()
    }

    fn activate(mut self) -> Result<(), BackendError> {
        if let Some(runtime) = self.runtime.take() {
            runtime.commit().map_err(|_| BackendError::new())?;
        }
        Ok(())
    }
}

pub struct TransactionalControllerFactory<F>(F);

pub fn transactional_controller_factory<F>(factory: F) -> TransactionalControllerFactory<F> {
    TransactionalControllerFactory(factory)
}

type PreparedFactory<P> =
    dyn Fn(&Snapshot) -> Result<PreparedController<P>, BackendError> + Send + Sync;

struct ProfileLogoLease {
    physical_generation: u64,
    lease: Option<ServiceLogoLease>,
}

struct ListLogoPublishHook {
    reached: std::sync::mpsc::SyncSender<()>,
    resume: std::sync::mpsc::Receiver<()>,
}

pub struct CoreBackend<C, P, F>
where
    C: HttpClient,
    P: ProcessController,
{
    store: ProfileStore,
    fetcher: SubscriptionFetcher<C>,
    controller_factory: Arc<PreparedFactory<P>>,
    factory_type: PhantomData<fn() -> F>,
    selector: Arc<dyn MihomoSelector>,
    latency_permits: Arc<tokio::sync::Semaphore>,
    operation: tokio::sync::Mutex<()>,
    post_commit_load_fault: AtomicBool,
    list_logo_publish_hook: std::sync::Mutex<Option<ListLogoPublishHook>>,
    profile_logo_leases: std::sync::Mutex<HashMap<ProfileId, ProfileLogoLease>>,
    inner: RwLock<CoreRuntime<P>>,
}

struct CoreRuntime<P: ProcessController> {
    active_profile_id: Option<ProfileId>,
    state: StateMachine,
    controller: Option<Arc<P>>,
    snapshot: Option<Arc<Snapshot>>,
    pending_snapshot: Option<(u64, Arc<Snapshot>)>,
    catalog: CatalogDto,
    profile: Option<String>,
    current_node: Option<String>,
    selected_nodes: Vec<Option<String>>,
    explicit_selected_nodes: Vec<Option<String>>,
    message: Option<String>,
    degraded: bool,
    events: EventBuffer<Event>,
}

impl<P: ProcessController> CoreRuntime<P> {
    fn empty() -> Self {
        Self {
            active_profile_id: None,
            state: StateMachine::default(),
            controller: None,
            snapshot: None,
            pending_snapshot: None,
            catalog: CatalogDto {
                revision: 0,
                groups: Vec::new(),
            },
            profile: None,
            current_node: None,
            selected_nodes: Vec::new(),
            explicit_selected_nodes: Vec::new(),
            message: None,
            degraded: false,
            events: EventBuffer::default(),
        }
    }

    fn status(&self) -> StatusDto {
        let profile = self
            .catalog
            .groups
            .iter()
            .find(|group| group.selected)
            .map(|group| group.label.clone())
            .or_else(|| {
                self.profile
                    .as_deref()
                    .map(|profile| safe_catalog_label(profile, "Профиль", 0))
            });
        let current_node = self
            .catalog
            .groups
            .iter()
            .find(|group| group.selected)
            .and_then(|group| group.nodes.iter().find(|node| node.selected))
            .map(|node| node.label.clone())
            .or_else(|| {
                self.current_node
                    .as_deref()
                    .map(|node| safe_catalog_label(node, "Узел", 0))
            });
        StatusDto {
            state: map_core_state(self.state.current()),
            active_profile_id: self
                .active_profile_id
                .as_ref()
                .map(|id| id.as_str().to_owned()),
            profile,
            current_node,
            message: self.message.clone(),
            degraded: self.degraded,
            subscription: self
                .pending_snapshot
                .as_ref()
                .map(|(_, snapshot)| snapshot)
                .or(self.snapshot.as_ref())
                .and_then(|snapshot| {
                    snapshot
                        .subscription_info()
                        .map(|info| SubscriptionInfoDto {
                            source_name: info.source_host.clone(),
                            display_name: info.display_name.clone(),
                            uploaded_bytes: info.uploaded_bytes,
                            downloaded_bytes: info.downloaded_bytes,
                            used_bytes: info.used_bytes(),
                            total_bytes: info.total_bytes,
                            expires_at_unix: info.expires_at_unix,
                            updated_at_unix: info.updated_at_unix,
                            refresh_interval_secs: info.refresh_interval_secs,
                            announcement_text: info
                                .announcement_text
                                .as_deref()
                                .map(|text| text.chars().take(512).collect()),
                            announcement_action_label: info.announcement_action_label.clone(),
                            announcement_tone: info
                                .announcement_tone
                                .map(|tone| tone.as_str().to_owned()),
                            home_available: snapshot.subscription_home_url().is_some(),
                            support_available: snapshot.subscription_support_url().is_some(),
                            announcement_action_available: snapshot
                                .subscription_announcement_url()
                                .is_some(),
                            service_logo_path: snapshot
                                .service_logo_path()
                                .and_then(std::path::Path::to_str)
                                .map(ToOwned::to_owned),
                            refresh_available: snapshot.subscription_source_url().is_some(),
                        })
                }),
        }
    }

    fn apply_snapshot(
        &mut self,
        id: ProfileId,
        snapshot: Arc<Snapshot>,
        controller: Arc<P>,
        revision: u64,
        selected_nodes: Vec<Option<String>>,
        explicit_selected_nodes: Vec<Option<String>>,
    ) {
        self.active_profile_id = Some(id);
        self.controller = Some(controller);
        self.apply_snapshot_metadata(snapshot, revision, selected_nodes, explicit_selected_nodes);
    }

    fn apply_snapshot_without_controller(
        &mut self,
        id: ProfileId,
        snapshot: Arc<Snapshot>,
        revision: u64,
        selected_nodes: Vec<Option<String>>,
        explicit_selected_nodes: Vec<Option<String>>,
    ) {
        self.active_profile_id = Some(id);
        self.controller = None;
        self.apply_snapshot_metadata(snapshot, revision, selected_nodes, explicit_selected_nodes);
    }

    fn apply_snapshot_metadata(
        &mut self,
        snapshot: Arc<Snapshot>,
        revision: u64,
        selected_nodes: Vec<Option<String>>,
        explicit_selected_nodes: Vec<Option<String>>,
    ) {
        if matches!(
            self.state.current(),
            CoreConnectionState::Empty | CoreConnectionState::Error
        ) {
            let _ = self.state.transition(CoreConnectionState::Ready);
        }
        let selectable = selectable_groups(&snapshot);
        self.profile = selectable.first().map(|group| group.name.to_owned());
        self.selected_nodes = selected_nodes;
        self.explicit_selected_nodes = explicit_selected_nodes;
        self.current_node = self.selected_nodes.first().cloned().flatten();
        self.catalog = build_catalog(
            &snapshot,
            revision,
            self.profile.as_deref(),
            &self.selected_nodes,
        );
        self.snapshot = Some(snapshot);
        self.pending_snapshot = None;
        self.message = None;
        self.degraded = false;
    }

    fn clear_for_active(&mut self, active_profile_id: Option<ProfileId>) {
        self.active_profile_id = active_profile_id;
        self.state = StateMachine::default();
        self.controller = None;
        self.snapshot = None;
        self.pending_snapshot = None;
        self.catalog = CatalogDto {
            revision: 0,
            groups: Vec::new(),
        };
        self.profile = None;
        self.current_node = None;
        self.selected_nodes.clear();
        self.explicit_selected_nodes.clear();
        self.message = None;
        self.degraded = false;
    }

    fn mark_error(&mut self, message: &str, degraded: bool) {
        match self.state.current() {
            CoreConnectionState::Ready => {
                let _ = self.state.transition(CoreConnectionState::Empty);
                let _ = self.state.transition(CoreConnectionState::Error);
            }
            CoreConnectionState::Empty
            | CoreConnectionState::Connecting
            | CoreConnectionState::Connected => {
                let _ = self.state.transition(CoreConnectionState::Error);
            }
            CoreConnectionState::Error => {}
        }
        self.message = Some(message.to_owned());
        self.degraded = degraded;
    }

    fn push_event(&mut self, severity: Severity, component: Component, phase: &str, message: &str) {
        self.push_event_with_details(severity, component, phase, message, json!({}));
    }

    /// Same as `push_event`, but attaches a structured payload for cases
    /// where the plain message alone isn't enough to diagnose a failure.
    /// `Event::new` redacts it the same way it redacts the message, so this
    /// is exactly as safe to log or hand to a user as `push_event` already
    /// is - it just carries more of what actually happened.
    fn push_event_with_details(
        &mut self,
        severity: Severity,
        component: Component,
        phase: &str,
        message: &str,
        details: serde_json::Value,
    ) {
        self.events.push_with(|id| {
            Event::new(
                unix_timestamp_ms(),
                severity,
                component,
                phase,
                format!("daemon-{id:016x}"),
                message,
                details,
            )
        });
    }
}

impl<C, P, F> CoreBackend<C, P, F>
where
    C: HttpClient,
    P: ProcessController,
    F: Fn(&Snapshot) -> Result<Arc<P>, BackendError> + Send + Sync + 'static,
{
    pub fn new(
        store: ProfileStore,
        fetcher: SubscriptionFetcher<C>,
        controller_factory: F,
    ) -> Result<Self, BackendError> {
        Self::new_with_selector(
            store,
            fetcher,
            controller_factory,
            Arc::new(UnavailableSelector),
        )
    }

    pub fn new_with_selector(
        store: ProfileStore,
        fetcher: SubscriptionFetcher<C>,
        controller_factory: F,
        selector: Arc<dyn MihomoSelector>,
    ) -> Result<Self, BackendError> {
        let controller_factory: Arc<PreparedFactory<P>> = Arc::new(move |snapshot: &Snapshot| {
            controller_factory(snapshot).map(PreparedController::plain)
        });
        Self::from_prepared_factory(store, fetcher, controller_factory, selector)
    }
}

impl<C, P, F> CoreBackend<C, P, TransactionalControllerFactory<F>>
where
    C: HttpClient,
    P: ProcessController,
    F: Fn(&Snapshot) -> Result<PreparedController<P>, BackendError> + Send + Sync + 'static,
{
    pub fn new_transactional(
        store: ProfileStore,
        fetcher: SubscriptionFetcher<C>,
        controller_factory: TransactionalControllerFactory<F>,
    ) -> Result<Self, BackendError> {
        Self::new_transactional_with_selector(
            store,
            fetcher,
            controller_factory,
            Arc::new(UnavailableSelector),
        )
    }

    pub fn new_transactional_with_selector(
        store: ProfileStore,
        fetcher: SubscriptionFetcher<C>,
        controller_factory: TransactionalControllerFactory<F>,
        selector: Arc<dyn MihomoSelector>,
    ) -> Result<Self, BackendError> {
        let factory = controller_factory.0;
        let controller_factory: Arc<PreparedFactory<P>> =
            Arc::new(move |snapshot: &Snapshot| factory(snapshot));
        Self::from_prepared_factory(store, fetcher, controller_factory, selector)
    }
}

impl<C, P, F> CoreBackend<C, P, F>
where
    C: HttpClient,
    P: ProcessController,
{
    fn from_prepared_factory(
        store: ProfileStore,
        fetcher: SubscriptionFetcher<C>,
        controller_factory: Arc<PreparedFactory<P>>,
        selector: Arc<dyn MihomoSelector>,
    ) -> Result<Self, BackendError> {
        let mut inner = CoreRuntime::empty();
        let mut profile_logo_leases = HashMap::new();
        inner.active_profile_id = store.active_id();
        if let Ok(Some((id, revision, physical_generation, snapshot))) =
            store.active_snapshot_generation()
        {
            profile_logo_leases.insert(
                id.clone(),
                ProfileLogoLease {
                    physical_generation,
                    lease: snapshot.service_logo_lease(),
                },
            );
            let (selections, explicit_selections) =
                initial_and_explicit_selections(&store, &id, revision, &snapshot);
            inner.apply_snapshot_without_controller(
                id,
                snapshot,
                revision,
                selections,
                explicit_selections,
            );
        }
        Ok(Self {
            store,
            fetcher,
            controller_factory,
            factory_type: PhantomData,
            selector,
            latency_permits: Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_LATENCY_PROBES)),
            operation: tokio::sync::Mutex::new(()),
            post_commit_load_fault: AtomicBool::new(false),
            list_logo_publish_hook: std::sync::Mutex::new(None),
            profile_logo_leases: std::sync::Mutex::new(profile_logo_leases),
            inner: RwLock::new(inner),
        })
    }

    pub fn list_subscriptions(&self) -> Result<Vec<ProfileSummary>, ProfileMutationError> {
        self.store.list().map_err(map_profile_error)
    }

    #[doc(hidden)]
    pub fn inject_post_commit_load_fault_for_testing(&self) {
        self.post_commit_load_fault.store(true, Ordering::SeqCst);
    }

    #[doc(hidden)]
    pub fn inject_list_logo_publish_hook_for_testing(
        &self,
    ) -> (
        std::sync::mpsc::Receiver<()>,
        std::sync::mpsc::SyncSender<()>,
    ) {
        let (reached, reached_rx) = std::sync::mpsc::sync_channel(0);
        let (resume, resume_rx) = std::sync::mpsc::sync_channel(0);
        *self
            .list_logo_publish_hook
            .lock()
            .expect("list logo publish hook lock poisoned") = Some(ListLogoPublishHook {
            reached,
            resume: resume_rx,
        });
        (reached_rx, resume)
    }

    #[doc(hidden)]
    pub fn retained_profile_logo_path_for_testing(
        &self,
        id: &ProfileId,
    ) -> Option<std::path::PathBuf> {
        self.profile_logo_leases
            .lock()
            .expect("profile logo lease lock poisoned")
            .get(id)
            .and_then(|entry| entry.lease.as_ref())
            .and_then(ServiceLogoLease::path)
            .map(std::path::Path::to_owned)
    }

    fn subscription_list_dto(&self) -> Result<SubscriptionListDto, ProfileMutationError> {
        let mut active_profile_id = None;
        let mut index = 0;
        let profiles = self
            .store
            .map_loaded(|loaded| {
                let profile_index = index;
                index += 1;
                if loaded.summary.active {
                    active_profile_id = Some(loaded.summary.id.as_str().to_owned());
                }
                let info = loaded
                    .snapshot
                    .as_ref()
                    .and_then(|snapshot| snapshot.subscription_info());
                let display_name = info.map_or_else(
                    || format!("Профиль {}", profile_index + 1),
                    |info| safe_catalog_label(&info.display_name, "Профиль", profile_index),
                );
                let service_logo_path = loaded.snapshot.as_ref().and_then(|snapshot| {
                    loaded.physical_generation.and_then(|physical_generation| {
                        self.wait_for_list_logo_publish_hook();
                        self.retain_profile_logo(&loaded.summary.id, physical_generation, snapshot)
                    })
                });
                SubscriptionProfileDto {
                    id: loaded.summary.id.as_str().to_owned(),
                    active: loaded.summary.active,
                    available: loaded.summary.available,
                    display_name,
                    used_bytes: info.and_then(|info| info.used_bytes()),
                    total_bytes: info.and_then(|info| info.total_bytes),
                    expires_at_unix: info.and_then(|info| info.expires_at_unix),
                    service_logo_path,
                }
            })
            .map_err(map_profile_error)?;
        Ok(SubscriptionListDto {
            active_profile_id,
            profiles,
        })
    }

    fn require_profile(&self, id: &ProfileId) -> Result<(), ProfileMutationError> {
        if self.store.contains(id) {
            Ok(())
        } else {
            Err(ProfileMutationError::NotFound)
        }
    }

    fn retain_profile_logo(
        &self,
        id: &ProfileId,
        physical_generation: u64,
        snapshot: &Snapshot,
    ) -> Option<String> {
        let candidate = snapshot
            .service_logo_lease()
            .filter(|lease| lease.path().is_some_and(|path| path.is_file()));
        let mut leases = self
            .profile_logo_leases
            .lock()
            .expect("profile logo lease lock poisoned");
        if let Some(current) = leases.get(id)
            && current.physical_generation > physical_generation
        {
            return current
                .lease
                .as_ref()
                .and_then(ServiceLogoLease::path)
                .and_then(std::path::Path::to_str)
                .map(ToOwned::to_owned);
        }
        let path = candidate
            .as_ref()
            .and_then(ServiceLogoLease::path)
            .and_then(std::path::Path::to_str)
            .map(ToOwned::to_owned);
        leases.insert(
            id.clone(),
            ProfileLogoLease {
                physical_generation,
                lease: candidate,
            },
        );
        path
    }

    fn wait_for_list_logo_publish_hook(&self) {
        let hook = self
            .list_logo_publish_hook
            .lock()
            .expect("list logo publish hook lock poisoned")
            .take();
        if let Some(hook) = hook {
            let _ = hook.reached.send(());
            let _ = hook.resume.recv();
        }
    }

    fn remove_profile_logo(&self, id: &ProfileId) {
        self.profile_logo_leases
            .lock()
            .expect("profile logo lease lock poisoned")
            .remove(id);
    }

    pub async fn activate_subscription(
        &self,
        id: &ProfileId,
    ) -> Result<StatusDto, ProfileMutationError> {
        self.require_disconnected_mutation().await?;
        let _operation = self.operation.lock().await;
        self.require_disconnected_mutation().await?;
        let (mut revision, physical_generation, mut snapshot) = self
            .store
            .profile_snapshot_generation(id)
            .map_err(map_profile_error)?;
        self.retain_profile_logo(id, physical_generation, &snapshot);
        let prepared =
            (self.controller_factory)(&snapshot).map_err(|_| ProfileMutationError::Storage)?;
        let maintenance_debt = match self.store.activate(id) {
            Ok(_) => false,
            Err(error) if error.mutation_committed() => {
                let (active_id, active_revision, _, active_snapshot) =
                    match self.store.active_snapshot_generation() {
                        Ok(Some(loaded)) => loaded,
                        Ok(None) | Err(_) => {
                            self.reconcile_authoritative().await;
                            return Err(ProfileMutationError::Storage);
                        }
                    };
                if active_id != *id {
                    self.reconcile_authoritative().await;
                    return Err(ProfileMutationError::Storage);
                }
                revision = active_revision;
                snapshot = active_snapshot;
                true
            }
            Err(error) => return Err(map_profile_error(error)),
        };
        let (selections, explicit_selections) =
            initial_and_explicit_selections(&self.store, id, revision, &snapshot);
        let controller = prepared.controller();
        {
            let mut state = self.inner.write().await;
            state.clear_for_active(Some(id.clone()));
            state.apply_snapshot(
                id.clone(),
                snapshot,
                controller,
                revision,
                selections,
                explicit_selections,
            );
            if maintenance_debt {
                state.push_event(
                    Severity::Warning,
                    Component::Core,
                    "profile_maintenance",
                    "Profile activation committed with maintenance debt.",
                );
            }
        }
        if prepared.activate().is_err() {
            self.reconcile_authoritative().await;
            return Err(ProfileMutationError::Storage);
        }
        Ok(self.current_status().await)
    }

    pub async fn refresh_subscription_profile(
        &self,
        id: &ProfileId,
    ) -> Result<StatusDto, ProfileMutationError>
    where
        C: 'static,
    {
        self.require_refresh_delete_mutation().await?;
        let _operation = self.operation.lock().await;
        self.require_refresh_delete_mutation().await?;
        self.refresh_profile_locked(id).await
    }

    pub async fn replace_subscription_source_profile(
        &self,
        id: &ProfileId,
        request: ImportSubscriptionRequest,
    ) -> Result<StatusDto, ProfileMutationError>
    where
        C: 'static,
    {
        let _operation = self.operation.lock().await;
        self.require_disconnected_mutation().await?;
        self.replace_profile_source_locked(id, &request.url).await
    }

    pub async fn import_subscription_profile(
        &self,
        request: ImportSubscriptionRequest,
    ) -> Result<StatusDto, ProfileMutationError>
    where
        C: 'static,
    {
        let _operation = self.operation.lock().await;
        self.import_profile_locked(&request.url).await
    }

    pub async fn delete_subscription(
        &self,
        id: &ProfileId,
    ) -> Result<StatusDto, ProfileMutationError> {
        self.require_refresh_delete_mutation().await?;
        let _operation = self.operation.lock().await;
        self.require_refresh_delete_mutation().await?;
        let was_active = self.store.active_id().as_ref() == Some(id);
        let stopped_connection = if was_active {
            self.stop_connection_for_profile_mutation().await?
        } else {
            false
        };
        let maintenance_debt = match self.store.delete(id) {
            Ok(_) => false,
            Err(error) if error.mutation_committed() => true,
            Err(error) => {
                return Err(if stopped_connection {
                    ProfileMutationError::Committed
                } else {
                    map_profile_error(error)
                });
            }
        };
        self.remove_profile_logo(id);
        if was_active {
            self.reconcile_authoritative().await;
        }
        if maintenance_debt {
            let _ = self.store.cleanup_unindexed_profile(id);
        }
        if maintenance_debt {
            self.record_maintenance_debt("profile_delete").await;
        }
        Ok(self.current_status().await)
    }

    /// Downloads a subscription within one overall deadline that stays below
    /// the desktop's request timeout. Always records a URL-free diagnostic
    /// event (the reason, plus a redacted per-attempt breakdown - which of
    /// the native/Mihomo/Xray requests reached the host and what happened),
    /// so a user hitting this can grab the event log and hand it over
    /// instead of just "не удалось добавить подписку".
    async fn fetch_subscription(
        &self,
        url: &str,
        staging: &AtomicSnapshot,
    ) -> Result<SnapshotCommit<Arc<Snapshot>>, ProfileMutationError> {
        let deadline_diagnostics = FetchDiagnostics {
            host: "unknown-host".to_owned(),
            native: AttemptOutcome::Skipped,
            mihomo: AttemptOutcome::Skipped,
            xray: AttemptOutcome::Skipped,
            elapsed_ms: SUBSCRIPTION_FETCH_DEADLINE
                .as_millis()
                .try_into()
                .unwrap_or(u64::MAX),
        };
        let (result, diagnostics) = match tokio::time::timeout(
            SUBSCRIPTION_FETCH_DEADLINE,
            self.fetcher.refresh_with_diagnostics(url, staging),
        )
        .await
        {
            Ok((result, diagnostics)) => (result, diagnostics),
            Err(_) => (Err(FetchError::Timeout), deadline_diagnostics),
        };
        match result {
            Ok(commit) => Ok(commit),
            Err(error) => {
                let reason = subscription_fetch_reason(error);
                self.inner.write().await.push_event_with_details(
                    Severity::Warning,
                    Component::Subscription,
                    "subscription_fetch_failed",
                    &format!("Subscription download failed: {reason}."),
                    json!({
                        "host": diagnostics.host,
                        "native": diagnostics.native.to_string(),
                        "mihomo": diagnostics.mihomo.to_string(),
                        "xray": diagnostics.xray.to_string(),
                        "elapsed_ms": diagnostics.elapsed_ms,
                        "os": std::env::consts::OS,
                    }),
                );
                Err(ProfileMutationError::Fetch(error))
            }
        }
    }

    async fn require_disconnected_mutation(&self) -> Result<(), ProfileMutationError> {
        let state = self.inner.read().await;
        if state.degraded
            || matches!(
                state.state.current(),
                CoreConnectionState::Connecting | CoreConnectionState::Connected
            )
        {
            Err(ProfileMutationError::Connected)
        } else {
            Ok(())
        }
    }

    async fn require_refresh_delete_mutation(&self) -> Result<(), ProfileMutationError> {
        let state = self.inner.read().await;
        if state.degraded || state.state.current() == CoreConnectionState::Connecting {
            Err(ProfileMutationError::Connected)
        } else {
            Ok(())
        }
    }

    async fn stop_connection_for_profile_mutation(&self) -> Result<bool, ProfileMutationError> {
        let controller = {
            let state = self.inner.read().await;
            match state.state.current() {
                CoreConnectionState::Connected => state
                    .controller
                    .clone()
                    .ok_or(ProfileMutationError::Committed)?,
                CoreConnectionState::Connecting => return Err(ProfileMutationError::Connected),
                CoreConnectionState::Empty
                | CoreConnectionState::Ready
                | CoreConnectionState::Error => return Ok(false),
            }
        };

        let result = Supervisor::new(controller).disconnect().await;
        let mut state = self.inner.write().await;
        match result {
            Ok(()) => {
                state
                    .state
                    .transition(CoreConnectionState::Ready)
                    .map_err(|_| ProfileMutationError::Committed)?;
                state.message = None;
                state.degraded = false;
                state.push_event(
                    Severity::Info,
                    Component::Core,
                    "profile_mutation_disconnect",
                    "Connection stopped before changing the active subscription.",
                );
                Ok(true)
            }
            Err(_) => {
                state.mark_error(
                    "Could not safely stop the cores before changing the active subscription.",
                    true,
                );
                state.push_event(
                    Severity::Error,
                    Component::Core,
                    "profile_mutation_disconnect",
                    "Core shutdown failed before an active subscription mutation.",
                );
                Err(ProfileMutationError::Committed)
            }
        }
    }

    async fn reconcile_authoritative(&self) {
        let authoritative_id = self.store.active_id();
        let loaded = self.store.active_snapshot_generation().ok().flatten();
        match (&authoritative_id, &loaded) {
            (_, Some((id, _, physical_generation, snapshot))) => {
                self.retain_profile_logo(id, *physical_generation, snapshot);
            }
            (Some(id), None) => self.remove_profile_logo(id),
            (None, None) => {}
        }
        let mut state = self.inner.write().await;
        state.clear_for_active(authoritative_id);
        if let Some((id, revision, _, snapshot)) = loaded {
            let (selections, explicit_selections) =
                initial_and_explicit_selections(&self.store, &id, revision, &snapshot);
            state.apply_snapshot_without_controller(
                id,
                snapshot,
                revision,
                selections,
                explicit_selections,
            );
        }
    }

    async fn reconcile_authoritative_unreadable(&self) {
        let authoritative_id = self.store.active_id();
        if let Some(id) = &authoritative_id {
            self.remove_profile_logo(id);
        }
        self.inner.write().await.clear_for_active(authoritative_id);
    }

    async fn current_status(&self) -> StatusDto {
        let mut status = self.inner.read().await.status();
        status.active_profile_id = self.store.active_id().map(|id| id.as_str().to_owned());
        status
    }

    async fn record_maintenance_debt(&self, phase: &str) {
        self.inner.write().await.push_event(
            Severity::Warning,
            Component::Core,
            phase,
            "Profile mutation committed with maintenance debt.",
        );
    }

    async fn profile_snapshot_after_commit(
        &self,
        id: &ProfileId,
    ) -> Result<(u64, u64, Arc<Snapshot>), ProfileMutationError> {
        if self.post_commit_load_fault.swap(false, Ordering::SeqCst) {
            self.remove_profile_logo(id);
            self.reconcile_authoritative_unreadable().await;
            return Err(ProfileMutationError::Committed);
        }
        match self.store.profile_snapshot_generation(id) {
            Ok(loaded) => Ok(loaded),
            Err(_) => {
                self.remove_profile_logo(id);
                self.reconcile_authoritative().await;
                Err(ProfileMutationError::Committed)
            }
        }
    }

    async fn refresh_profile_locked(
        &self,
        id: &ProfileId,
    ) -> Result<StatusDto, ProfileMutationError>
    where
        C: 'static,
    {
        let (_, previous) = self.store.profile_snapshot(id).map_err(map_profile_error)?;
        let source_url = previous
            .subscription_source_url()
            .ok_or(ProfileMutationError::Storage)?
            .to_owned();
        self.replace_profile_source_locked(id, &source_url).await
    }

    async fn replace_profile_source_locked(
        &self,
        id: &ProfileId,
        source_url: &str,
    ) -> Result<StatusDto, ProfileMutationError>
    where
        C: 'static,
    {
        let source_url = source_url.trim();
        if source_url.is_empty() {
            return Err(ProfileMutationError::Storage);
        }
        let (previous_revision, previous) =
            self.store.profile_snapshot(id).map_err(map_profile_error)?;
        let active = self.store.active_id().as_ref() == Some(id);
        let connected =
            active && self.inner.read().await.state.current() == CoreConnectionState::Connected;
        let staging = AtomicSnapshot::default();
        let fetched = self.fetch_subscription(source_url, &staging).await?;
        let (snapshot, _) = fetched.into_parts();
        let prepared = if active && !connected {
            Some((self.controller_factory)(&snapshot).map_err(|_| ProfileMutationError::Prepare)?)
        } else {
            None
        };
        let stopped_connection = if active && !connected {
            self.stop_connection_for_profile_mutation().await?
        } else {
            false
        };
        let (revision, physical_generation, persisted, maintenance_debt) =
            match self.store.replace_profile_loaded(id, snapshot.clone()) {
                Ok((revision, physical_generation, persisted)) => {
                    (revision, physical_generation, persisted, false)
                }
                Err(error) if error.mutation_committed() => {
                    let (revision, physical_generation, persisted) = if connected {
                        self.store
                            .profile_snapshot_generation(id)
                            .map_err(|_| ProfileMutationError::Committed)?
                    } else {
                        self.profile_snapshot_after_commit(id).await?
                    };
                    (revision, physical_generation, persisted, true)
                }
                Err(error) => {
                    return Err(if stopped_connection {
                        ProfileMutationError::Committed
                    } else {
                        map_profile_error(error)
                    });
                }
            };
        carry_forward_selections(
            &self.store,
            id,
            previous_revision,
            &previous,
            revision,
            &persisted,
        )
        .map_err(|_| ProfileMutationError::Committed)?;
        if self.post_commit_load_fault.swap(false, Ordering::SeqCst) {
            if connected {
                return Err(ProfileMutationError::Committed);
            }
            self.remove_profile_logo(id);
            self.reconcile_authoritative_unreadable().await;
            return Err(ProfileMutationError::Committed);
        }
        self.retain_profile_logo(id, physical_generation, &persisted);
        if !active {
            if maintenance_debt {
                self.record_maintenance_debt("profile_refresh").await;
            }
            return Ok(self.current_status().await);
        }
        if connected {
            let mut state = self.inner.write().await;
            state.pending_snapshot = Some((revision, persisted));
            if maintenance_debt {
                state.push_event(
                    Severity::Warning,
                    Component::Subscription,
                    "profile_refresh",
                    "Profile refresh committed with maintenance debt.",
                );
            }
            return Ok(state.status());
        }
        let prepared = prepared.expect("active refresh prepares a controller");
        let (selections, explicit_selections) =
            initial_and_explicit_selections(&self.store, id, revision, &persisted);
        let controller = prepared.controller();
        {
            let mut state = self.inner.write().await;
            state.clear_for_active(Some(id.clone()));
            state.apply_snapshot(
                id.clone(),
                persisted,
                controller,
                revision,
                selections,
                explicit_selections,
            );
            if maintenance_debt {
                state.push_event(
                    Severity::Warning,
                    Component::Subscription,
                    "profile_refresh",
                    "Profile refresh committed with maintenance debt.",
                );
            }
        }
        if prepared.activate().is_err() {
            self.reconcile_authoritative().await;
            return Err(ProfileMutationError::Committed);
        }
        Ok(self.current_status().await)
    }

    async fn import_profile_locked(&self, url: &str) -> Result<StatusDto, ProfileMutationError>
    where
        C: 'static,
    {
        self.require_disconnected_mutation().await?;
        let trimmed_url = url.trim();
        if trimmed_url.is_empty() {
            return Err(ProfileMutationError::Storage);
        }
        let staging = AtomicSnapshot::default();
        let fetched = self.fetch_subscription(trimmed_url, &staging).await?;
        let (snapshot, _) = fetched.into_parts();
        let prepared =
            (self.controller_factory)(&snapshot).map_err(|_| ProfileMutationError::Prepare)?;
        let existing = self
            .store
            .find_exact_source(trimmed_url)
            .map_err(map_profile_error)?;
        let (id, maintenance_debt, previous) = if let Some(id) = existing {
            let previous = self
                .store
                .profile_snapshot(&id)
                .map_err(map_profile_error)?;
            let replace_debt = match self.store.replace_profile_loaded(&id, snapshot.clone()) {
                Ok(_) => false,
                Err(error) if error.mutation_committed() => {
                    self.profile_snapshot_after_commit(&id).await?;
                    true
                }
                Err(error) => return Err(map_profile_error(error)),
            };
            let activate_debt = match self.store.activate(&id) {
                Ok(_) => false,
                Err(error) if error.mutation_committed() => true,
                Err(error) => {
                    self.reconcile_authoritative().await;
                    return Err(map_profile_error(error));
                }
            };
            (id, replace_debt || activate_debt, Some(previous))
        } else {
            match self.store.create_profile(snapshot.clone()) {
                Ok(id) => (id, false, None),
                Err(error) if error.mutation_committed() => {
                    let Some(id) = self.store.active_id() else {
                        self.reconcile_authoritative().await;
                        return Err(ProfileMutationError::Storage);
                    };
                    (id, true, None)
                }
                Err(error) => return Err(map_profile_error(error)),
            }
        };
        let (revision, physical_generation, persisted) =
            self.profile_snapshot_after_commit(&id).await?;
        if let Some((previous_revision, previous)) = previous {
            carry_forward_selections(
                &self.store,
                &id,
                previous_revision,
                &previous,
                revision,
                &persisted,
            )
            .map_err(|_| ProfileMutationError::Committed)?;
        }
        self.retain_profile_logo(&id, physical_generation, &persisted);
        let (selections, explicit_selections) =
            initial_and_explicit_selections(&self.store, &id, revision, &persisted);
        let controller = prepared.controller();
        {
            let mut state = self.inner.write().await;
            state.clear_for_active(Some(id.clone()));
            state.apply_snapshot(
                id,
                persisted,
                controller,
                revision,
                selections,
                explicit_selections,
            );
            state.push_event(
                Severity::Info,
                Component::Subscription,
                "import",
                "Subscription imported.",
            );
            if maintenance_debt {
                state.push_event(
                    Severity::Warning,
                    Component::Subscription,
                    "profile_import",
                    "Profile import committed with maintenance debt.",
                );
            }
        }
        if prepared.activate().is_err() {
            self.reconcile_authoritative().await;
            return Err(ProfileMutationError::Storage);
        }
        Ok(self.current_status().await)
    }
}

fn map_profile_error(error: ProfileError) -> ProfileMutationError {
    match error {
        ProfileError::NotFound(_) => ProfileMutationError::NotFound,
        ProfileError::LimitReached => ProfileMutationError::LimitReached,
        _ => ProfileMutationError::Storage,
    }
}

fn default_selections(snapshot: &Snapshot) -> Vec<Option<String>> {
    selectable_groups(snapshot)
        .iter()
        .map(|group| group.nodes.first().map(|node| (*node).to_owned()))
        .collect()
}

fn restored_selections(
    store: &ProfileStore,
    id: &ProfileId,
    revision: u64,
    snapshot: &Snapshot,
) -> Vec<Option<String>> {
    let groups = selectable_groups(snapshot);
    let catalog_ids: Vec<_> = groups
        .iter()
        .enumerate()
        .map(|(group_index, group)| {
            (
                catalog_group_id(revision, group_index),
                group
                    .nodes
                    .iter()
                    .enumerate()
                    .map(|(node_index, _)| catalog_node_id(revision, group_index, node_index))
                    .collect::<Vec<_>>(),
            )
        })
        .collect();
    let catalog: Vec<_> = catalog_ids
        .iter()
        .map(|(group, nodes)| {
            (
                group.as_str(),
                nodes.iter().map(String::as_str).collect::<Vec<_>>(),
            )
        })
        .collect();
    let catalog: Vec<_> = catalog
        .iter()
        .map(|(group, nodes)| (*group, nodes.as_slice()))
        .collect();
    let saved = store
        .valid_selections(id, revision, &catalog)
        .unwrap_or_default();
    groups
        .iter()
        .enumerate()
        .map(|(group_index, group)| {
            let group_id = catalog_group_id(revision, group_index);
            saved.get(&group_id).and_then(|node_id| {
                group
                    .nodes
                    .iter()
                    .enumerate()
                    .find_map(|(node_index, node)| {
                        (catalog_node_id(revision, group_index, node_index) == *node_id)
                            .then(|| (*node).to_owned())
                    })
            })
        })
        .collect()
}

fn initial_and_explicit_selections(
    store: &ProfileStore,
    id: &ProfileId,
    revision: u64,
    snapshot: &Snapshot,
) -> (Vec<Option<String>>, Vec<Option<String>>) {
    let explicit = restored_selections(store, id, revision, snapshot);
    let mut selections = default_selections(snapshot);
    for (selected, restored) in selections.iter_mut().zip(&explicit) {
        if restored.is_some() {
            selected.clone_from(restored);
        }
    }
    (selections, explicit)
}

fn carry_forward_selections(
    store: &ProfileStore,
    id: &ProfileId,
    previous_revision: u64,
    previous: &Snapshot,
    next_revision: u64,
    next: &Snapshot,
) -> Result<(), ProfileError> {
    let previous_groups = selectable_groups(previous);
    let saved_nodes = restored_selections(store, id, previous_revision, previous);
    for (previous_group, saved_node) in previous_groups.iter().zip(saved_nodes) {
        let Some(saved_node) = saved_node else {
            continue;
        };
        let Some((group_id, node_id)) =
            selection_ids_for_names(next_revision, next, previous_group.name, &saved_node)
        else {
            continue;
        };
        store.save_selection(id, next_revision, &group_id, &node_id)?;
    }
    Ok(())
}

fn selection_ids_for_names(
    revision: u64,
    snapshot: &Snapshot,
    group_name: &str,
    node_name: &str,
) -> Option<(String, String)> {
    let groups = selectable_groups(snapshot);
    let mut matching_groups = groups
        .iter()
        .enumerate()
        .filter(|(_, group)| group.name == group_name);
    let (group_index, group) = matching_groups.next()?;
    if matching_groups.next().is_some() {
        return None;
    }
    let mut matching_nodes = group
        .nodes
        .iter()
        .enumerate()
        .filter(|(_, node)| **node == node_name);
    let (node_index, _) = matching_nodes.next()?;
    if matching_nodes.next().is_some() {
        return None;
    }
    Some((
        catalog_group_id(revision, group_index),
        catalog_node_id(revision, group_index, node_index),
    ))
}

#[async_trait]
impl<C, P, F> Backend for CoreBackend<C, P, F>
where
    C: HttpClient + 'static,
    P: ProcessController + 'static,
    F: 'static,
{
    async fn status(&self) -> Result<StatusDto, BackendError> {
        Ok(self.current_status().await)
    }

    async fn catalog(&self) -> Result<CatalogDto, BackendError> {
        let runtime = self.inner.read().await;
        Ok(runtime.catalog.clone())
    }

    async fn import_subscription(
        &self,
        request: ImportSubscriptionRequest,
    ) -> Result<StatusDto, BackendError> {
        let _operation = self.operation.lock().await;
        self.import_profile_locked(&request.url)
            .await
            .map_err(BackendError::from_profile_mutation)
    }

    async fn refresh_subscription(&self) -> Result<StatusDto, BackendError> {
        self.require_refresh_delete_mutation()
            .await
            .map_err(BackendError::from_profile_mutation)?;
        let _operation = self.operation.lock().await;
        self.require_refresh_delete_mutation()
            .await
            .map_err(BackendError::from_profile_mutation)?;
        let id = self
            .store
            .active_id()
            .ok_or_else(|| BackendError::from_profile_mutation(ProfileMutationError::NotFound))?;
        self.refresh_profile_locked(&id)
            .await
            .map_err(BackendError::from_profile_mutation)
    }

    async fn list_subscriptions(&self) -> Result<SubscriptionListDto, BackendError> {
        self.subscription_list_dto()
            .map_err(BackendError::from_profile_mutation)
    }

    async fn activate_subscription(&self, id: ProfileId) -> Result<StatusDto, BackendError> {
        self.require_profile(&id)
            .map_err(BackendError::from_profile_mutation)?;
        self.activate_subscription(&id)
            .await
            .map_err(BackendError::from_profile_mutation)
    }

    async fn refresh_subscription_profile(
        &self,
        id: ProfileId,
    ) -> Result<SubscriptionListDto, BackendError> {
        self.require_profile(&id)
            .map_err(BackendError::from_profile_mutation)?;
        self.require_refresh_delete_mutation()
            .await
            .map_err(BackendError::from_profile_mutation)?;
        let _operation = self.operation.lock().await;
        self.require_refresh_delete_mutation()
            .await
            .map_err(BackendError::from_profile_mutation)?;
        self.refresh_profile_locked(&id)
            .await
            .map_err(BackendError::from_profile_mutation)?;
        self.subscription_list_dto()
            .map_err(BackendError::from_profile_mutation)
    }

    async fn replace_subscription_source(
        &self,
        id: ProfileId,
        request: ImportSubscriptionRequest,
    ) -> Result<SubscriptionListDto, BackendError> {
        self.require_profile(&id)
            .map_err(BackendError::from_profile_mutation)?;
        let _operation = self.operation.lock().await;
        self.require_disconnected_mutation()
            .await
            .map_err(BackendError::from_profile_mutation)?;
        self.replace_profile_source_locked(&id, &request.url)
            .await
            .map_err(BackendError::from_profile_mutation)?;
        self.subscription_list_dto()
            .map_err(BackendError::from_profile_mutation)
    }

    async fn delete_subscription(&self, id: ProfileId) -> Result<StatusDto, BackendError> {
        self.require_profile(&id)
            .map_err(BackendError::from_profile_mutation)?;
        self.delete_subscription(&id)
            .await
            .map_err(BackendError::from_profile_mutation)
    }

    async fn connect(&self) -> Result<StatusDto, BackendError> {
        let _operation = self.operation.lock().await;
        let (snapshot, queued_selections) = {
            let state = self.inner.read().await;
            if state.degraded {
                return Err(BackendError::new());
            }
            if state.state.current() != CoreConnectionState::Ready {
                return Err(BackendError::new());
            }
            let snapshot = state.snapshot.clone().ok_or_else(BackendError::new)?;
            let queued_selections = selectable_groups(&snapshot)
                .iter()
                .zip(&state.explicit_selected_nodes)
                .filter_map(|(group, node)| {
                    node.as_ref()
                        .map(|node| (group.name.to_owned(), node.clone()))
                })
                .collect::<Vec<_>>();
            (snapshot, queued_selections)
        };
        let prepared = match (self.controller_factory)(&snapshot) {
            Ok(prepared) => prepared,
            Err(error) => {
                let mut state = self.inner.write().await;
                state.message =
                    Some("Не удалось подготовить сетевой интерфейс и адреса узлов.".to_owned());
                state.push_event(
                    Severity::Error,
                    Component::Core,
                    "runtime_publish",
                    "Подготовка конфигурации перед подключением завершилась ошибкой.",
                );
                return Err(error);
            }
        };
        let controller = prepared.controller();
        if prepared.activate().is_err() {
            let mut state = self.inner.write().await;
            state.message = Some("Не удалось активировать конфигурацию запуска.".to_owned());
            state.push_event(
                Severity::Error,
                Component::Core,
                "runtime_publish",
                "Активация конфигурации перед подключением завершилась ошибкой.",
            );
            return Err(BackendError::new());
        }
        {
            let mut state = self.inner.write().await;
            state.controller = Some(controller.clone());
            state
                .state
                .transition(CoreConnectionState::Connecting)
                .map_err(|_| BackendError::new())?;
            state.message = Some(CONNECTING_MESSAGE.to_owned());
            state.push_event(
                Severity::Info,
                Component::Core,
                "connect",
                "Запускается подключение.",
            );
        }
        // If this future is dropped before the outcome is recorded, leave a cleanable error.
        let mut abandoned = AbandonedConnectGuard::new(&self.inner);

        let result = Supervisor::new(controller.clone()).connect().await;
        if result.is_ok() {
            for (group, node) in queued_selections {
                if self.selector.select(&group, &node).await.is_err() {
                    let rollback = Supervisor::new(controller.clone()).disconnect().await;
                    let mut state = self.inner.write().await;
                    abandoned.disarm();
                    if rollback.is_ok() {
                        state
                            .state
                            .transition(CoreConnectionState::Ready)
                            .map_err(|_| BackendError::new())?;
                        state.message = Some(
                            "Mihomo did not apply the queued route selection; connection was rolled back."
                                .to_owned(),
                        );
                        state.degraded = false;
                    } else {
                        state.mark_error(
                            "Mihomo route selection failed and core rollback was incomplete.",
                            true,
                        );
                    }
                    state.push_event(
                        Severity::Error,
                        Component::Mihomo,
                        "selection_replay",
                        "Queued Mihomo selection replay failed during connection startup.",
                    );
                    return Err(BackendError::new());
                }
            }
        }
        let mut state = self.inner.write().await;
        abandoned.disarm();
        match result {
            Ok(()) => {
                state
                    .state
                    .transition(CoreConnectionState::Connected)
                    .map_err(|_| BackendError::new())?;
                state.message = None;
                state.degraded = false;
                state.push_event(
                    Severity::Info,
                    Component::Core,
                    "connect",
                    "Подключение установлено.",
                );
                Ok(state.status())
            }
            Err(multicore_core::SupervisorError::StartFailed {
                engine,
                rollback_failed,
                cause,
            }) => {
                if rollback_failed {
                    state.mark_error(
                        "Не удалось полностью отменить запуск. Выполните отключение для очистки.",
                        true,
                    );
                } else {
                    state
                        .state
                        .transition(CoreConnectionState::Ready)
                        .map_err(|_| BackendError::new())?;
                    state.message = Some(start_failure_message(engine, cause));
                    state.degraded = false;
                }
                state.push_event(
                    Severity::Error,
                    Component::Core,
                    "connect",
                    "Запуск подключения завершился ошибкой.",
                );
                Err(BackendError::new())
            }
            Err(multicore_core::SupervisorError::StopFailed) => {
                state.mark_error(
                    "Состояние процессов неизвестно. Выполните отключение для очистки.",
                    true,
                );
                state.push_event(
                    Severity::Error,
                    Component::Core,
                    "connect",
                    "Запуск подключения завершился ошибкой.",
                );
                Err(BackendError::new())
            }
        }
    }

    async fn disconnect(&self) -> Result<StatusDto, BackendError> {
        let _operation = self.operation.lock().await;
        let controller = {
            let state = self.inner.read().await;
            if matches!(
                state.state.current(),
                CoreConnectionState::Empty | CoreConnectionState::Ready
            ) {
                return Ok(state.status());
            }
            state.controller.clone().ok_or_else(BackendError::new)?
        };

        let result = Supervisor::new(controller).disconnect().await;
        let mut state = self.inner.write().await;
        match result {
            Ok(()) => {
                state
                    .state
                    .transition(CoreConnectionState::Ready)
                    .map_err(|_| BackendError::new())?;
                state.message = None;
                state.degraded = false;
                state.push_event(
                    Severity::Info,
                    Component::Core,
                    "disconnect",
                    "Подключение остановлено.",
                );
                drop(state);
                self.reconcile_authoritative().await;
                Ok(self.current_status().await)
            }
            Err(_) => {
                state.mark_error(
                    "Не удалось полностью остановить подключение. Повторите попытку.",
                    true,
                );
                state.push_event(
                    Severity::Error,
                    Component::Core,
                    "disconnect",
                    "Остановка подключения завершилась ошибкой.",
                );
                Err(BackendError::new())
            }
        }
    }

    async fn select(&self, request: SelectionRequest) -> Result<CatalogDto, SelectionError> {
        let _operation = self.operation.lock().await;
        let (
            snapshot,
            active_profile_id,
            connected,
            group_index,
            raw_group,
            raw_node,
            previous_raw_node,
            pending_snapshot,
        ) = {
            let state = self.inner.read().await;
            if !matches!(
                state.state.current(),
                CoreConnectionState::Ready | CoreConnectionState::Connected
            ) || state.degraded
            {
                return Err(SelectionError::NotConnected);
            }
            if request.revision != state.catalog.revision {
                return Err(SelectionError::StaleRevision);
            }
            let snapshot = state.snapshot.clone().ok_or(SelectionError::UnknownGroup)?;
            let active_profile_id = state
                .active_profile_id
                .clone()
                .ok_or(SelectionError::UnknownGroup)?;
            let connected = state.state.current() == CoreConnectionState::Connected;
            let groups = selectable_groups(&snapshot);
            let group_index = groups
                .iter()
                .enumerate()
                .find_map(|(index, _)| {
                    (catalog_group_id(request.revision, index) == request.group_id).then_some(index)
                })
                .ok_or(SelectionError::UnknownGroup)?;
            let group = &groups[group_index];
            let node_index = group
                .nodes
                .iter()
                .enumerate()
                .find_map(|(index, _)| {
                    (catalog_node_id(request.revision, group_index, index) == request.node_id)
                        .then_some(index)
                })
                .ok_or(SelectionError::UnknownNode)?;
            let raw_group = group.name.to_owned();
            let raw_node = group.nodes[node_index].to_owned();
            let previous_raw_node = state.selected_nodes[group_index].clone();
            (
                snapshot,
                active_profile_id,
                connected,
                group_index,
                raw_group,
                raw_node,
                previous_raw_node,
                state.pending_snapshot.clone(),
            )
        };

        if connected {
            self.selector
                .select(&raw_group, &raw_node)
                .await
                .map_err(|_| SelectionError::SelectorFailed)?;
        }

        let save_target = if let Some((pending_revision, pending)) = &pending_snapshot {
            selection_ids_for_names(*pending_revision, pending, &raw_group, &raw_node)
                .map(|(group_id, node_id)| (*pending_revision, group_id, node_id))
        } else {
            Some((
                request.revision,
                request.group_id.clone(),
                request.node_id.clone(),
            ))
        };
        let save_result = save_target
            .as_ref()
            .map_or(Ok(()), |(revision, group_id, node_id)| {
                self.store
                    .save_selection(&active_profile_id, *revision, group_id, node_id)
            });
        let maintenance_debt = match save_result {
            Ok(()) => false,
            Err(error) if error.mutation_committed() => true,
            Err(_) => {
                if connected
                    && let Some(previous_raw_node) = previous_raw_node
                    && self
                        .selector
                        .select(&raw_group, &previous_raw_node)
                        .await
                        .is_err()
                {
                    let mut state = self.inner.write().await;
                    state.mark_error(
                        "Mihomo selection changed but persistence and live compensation failed.",
                        true,
                    );
                    state.push_event(
                        Severity::Error,
                        Component::Mihomo,
                        "selection_compensation",
                        "Mihomo selection compensation failed after a storage error.",
                    );
                }
                return Err(SelectionError::StorageFailed);
            }
        };

        let mut state = self.inner.write().await;
        // All state-changing operations share `operation`, so this revision cannot change while
        // the controller request is in flight.
        if state.catalog.revision != request.revision {
            return Err(SelectionError::StaleRevision);
        }
        if maintenance_debt && pending_snapshot.is_none() {
            let (selected_nodes, explicit_selected_nodes) = initial_and_explicit_selections(
                &self.store,
                &active_profile_id,
                request.revision,
                &snapshot,
            );
            state.selected_nodes = selected_nodes;
            state.explicit_selected_nodes = explicit_selected_nodes;
            state.current_node = state.selected_nodes.first().cloned().flatten();
        } else {
            state.selected_nodes[group_index] = Some(raw_node.clone());
            state.explicit_selected_nodes[group_index] = Some(raw_node.clone());
            if group_index == 0 {
                state.current_node = Some(raw_node);
            }
        }
        state.catalog = build_catalog(
            &snapshot,
            request.revision,
            state.profile.as_deref(),
            &state.selected_nodes,
        );
        state.push_event(
            Severity::Info,
            Component::Mihomo,
            "selection",
            "Маршрут Mihomo изменён.",
        );
        if maintenance_debt {
            state.push_event(
                Severity::Warning,
                Component::Core,
                "selection_maintenance",
                "Selection committed with maintenance debt.",
            );
        }
        Ok(state.catalog.clone())
    }

    async fn latencies(&self, request: LatencyRequest) -> Result<LatencyResponseDto, LatencyError> {
        let batch_deadline = tokio::time::Instant::now() + LATENCY_BATCH_TIMEOUT;
        if request
            .group_ids
            .len()
            .saturating_add(request.node_ids.len())
            > MAX_LATENCY_RESULTS
        {
            return Err(LatencyError::TooManyTargets);
        }

        let mut targets = {
            let state = self.inner.read().await;
            if state.state.current() != CoreConnectionState::Connected || state.degraded {
                return Err(LatencyError::NotConnected);
            }
            if request
                .revision
                .is_some_and(|revision| revision != state.catalog.revision)
            {
                return Err(LatencyError::StaleRevision);
            }
            let revision = state.catalog.revision;
            let snapshot = state.snapshot.as_ref().ok_or(LatencyError::UnknownGroup)?;
            let groups = selectable_groups(snapshot);
            let requested_groups: HashSet<&str> =
                request.group_ids.iter().map(String::as_str).collect();
            let requested_nodes: HashSet<&str> =
                request.node_ids.iter().map(String::as_str).collect();

            let known_groups: HashSet<String> = (0..groups.len())
                .map(|group_index| catalog_group_id(revision, group_index))
                .collect();
            if !requested_groups
                .iter()
                .all(|group_id| known_groups.contains(*group_id))
            {
                return Err(LatencyError::UnknownGroup);
            }
            let known_nodes: HashSet<String> = groups
                .iter()
                .enumerate()
                .flat_map(|(group_index, group)| {
                    (0..group.nodes.len())
                        .map(move |node_index| catalog_node_id(revision, group_index, node_index))
                })
                .collect();
            if !requested_nodes
                .iter()
                .all(|node_id| known_nodes.contains(*node_id))
            {
                return Err(LatencyError::UnknownNode);
            }

            let probe_all = requested_groups.is_empty() && requested_nodes.is_empty();
            let mut targets: VecDeque<_> = groups
                .iter()
                .enumerate()
                .flat_map(|(group_index, group)| {
                    let group_id = catalog_group_id(revision, group_index);
                    let group_requested = requested_groups.contains(group_id.as_str());
                    let requested_nodes = &requested_nodes;
                    group
                        .nodes
                        .iter()
                        .enumerate()
                        .filter_map(move |(node_index, raw_node)| {
                            let node_id = catalog_node_id(revision, group_index, node_index);
                            (probe_all
                                || group_requested
                                || requested_nodes.contains(node_id.as_str()))
                            .then(|| (group_id.clone(), node_id, (*raw_node).to_owned()))
                        })
                })
                .collect();
            if targets.len() > MAX_LATENCY_RESULTS {
                if probe_all {
                    targets.truncate(MAX_LATENCY_RESULTS);
                } else {
                    return Err(LatencyError::TooManyTargets);
                }
            }
            targets
        };

        let all_targets: Vec<_> = targets.iter().cloned().collect();
        let mut probes = tokio::task::JoinSet::new();
        let mut next_index = 0_usize;
        while probes.len() < MAX_CONCURRENT_LATENCY_PROBES {
            let Some((group_id, node_id, raw_node)) = targets.pop_front() else {
                break;
            };
            let selector = self.selector.clone();
            let permits = self.latency_permits.clone();
            let index = next_index;
            next_index += 1;
            probes.spawn(probe_latency_target(
                selector, permits, index, group_id, node_id, raw_node,
            ));
        }

        let mut completed = vec![None; all_targets.len()];
        while !probes.is_empty() {
            match tokio::time::timeout_at(batch_deadline, probes.join_next()).await {
                Ok(Some(Ok((index, entry)))) => completed[index] = Some(entry),
                Ok(Some(Err(_))) => {}
                Ok(None) => break,
                Err(_) => {
                    probes.abort_all();
                    break;
                }
            }
            if tokio::time::Instant::now() < batch_deadline
                && let Some((group_id, node_id, raw_node)) = targets.pop_front()
            {
                let selector = self.selector.clone();
                let permits = self.latency_permits.clone();
                let index = next_index;
                next_index += 1;
                probes.spawn(probe_latency_target(
                    selector, permits, index, group_id, node_id, raw_node,
                ));
            }
        }
        let entries = all_targets
            .into_iter()
            .enumerate()
            .map(|(index, (group_id, node_id, _))| {
                completed[index].take().unwrap_or(LatencyResultDto {
                    group_id,
                    node_id,
                    latency_ms: None,
                    status: LatencyStatus::Timeout,
                })
            })
            .collect();
        Ok(LatencyResponseDto { entries })
    }

    async fn events_after(&self, after: u64) -> Result<Vec<EventDto>, BackendError> {
        let state = self.inner.read().await;
        Ok(state
            .events
            .after(after)
            .map(|(id, event)| core_event_to_dto(id, event))
            .collect())
    }

    async fn diagnostics(&self) -> Result<DiagnosticsDto, BackendError> {
        let (controller, snapshot) = {
            let state = self.inner.read().await;
            (state.controller.clone(), state.snapshot.clone())
        };
        let diagnostics = match controller {
            Some(controller) => controller.diagnostics().await,
            None => ProcessDiagnostics::default(),
        };
        let mut dto = process_diagnostics_to_dto(diagnostics);
        if let Some(snapshot) = snapshot {
            dto.mappings = diagnostic_mappings(&snapshot);
        }
        Ok(dto)
    }
}

fn diagnostic_mappings(snapshot: &Snapshot) -> Vec<ProxyMappingDto> {
    snapshot
        .mihomo
        .loopback_socks_mappings()
        .iter()
        .take(MAX_DIAGNOSTIC_MAPPINGS)
        .enumerate()
        .map(|(index, mapping)| {
            let label = safe_catalog_label(&mapping.name, "Proxy", index);
            ProxyMappingDto {
                proxy_name: label.clone(),
                address: format!("127.0.0.1:{}", mapping.port),
                xray_label: label,
            }
        })
        .collect()
}

fn process_diagnostics_to_dto(diagnostics: ProcessDiagnostics) -> DiagnosticsDto {
    DiagnosticsDto {
        xray: runtime_check_to_dto(diagnostics.xray),
        mihomo: runtime_check_to_dto(diagnostics.mihomo),
        tun: runtime_check_to_dto(diagnostics.tun),
        logs: diagnostics
            .logs
            .into_iter()
            .map(|record| DiagnosticLogDto {
                id: record.id,
                timestamp: record.timestamp_ms.to_string(),
                component: match record.engine {
                    multicore_core::Engine::Xray => "xray",
                    multicore_core::Engine::Mihomo => "mihomo",
                }
                .to_owned(),
                level: match record.stream {
                    DiagnosticStream::Stdout => "info",
                    DiagnosticStream::Stderr => "error",
                }
                .to_owned(),
                message: record.message,
            })
            .collect(),
        mappings: Vec::new(),
    }
}

fn runtime_check_to_dto(state: RuntimeCheckState) -> RuntimeCheckDto {
    match state {
        RuntimeCheckState::Stopped => RuntimeCheckDto::Stopped,
        RuntimeCheckState::Starting => RuntimeCheckDto::Starting,
        RuntimeCheckState::Ready => RuntimeCheckDto::Ready,
        RuntimeCheckState::Failed => RuntimeCheckDto::Failed,
        RuntimeCheckState::Unsupported => RuntimeCheckDto::Unsupported,
    }
}

/// Marks a connect whose future was dropped mid-start as a degraded error, so the state never
/// stays `Connecting` and the next Disconnect cleans up whatever the start left running.
struct AbandonedConnectGuard<'a, P: ProcessController> {
    inner: &'a RwLock<CoreRuntime<P>>,
    armed: bool,
}

impl<'a, P: ProcessController> AbandonedConnectGuard<'a, P> {
    fn new(inner: &'a RwLock<CoreRuntime<P>>) -> Self {
        Self { inner, armed: true }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl<P: ProcessController> Drop for AbandonedConnectGuard<'_, P> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        // Drop cannot await; readers hold this lock only briefly, so retry a bounded number of
        // times rather than leaving the state stuck.
        for _ in 0..10_000 {
            if let Ok(mut state) = self.inner.try_write() {
                if state.state.current() == CoreConnectionState::Connecting {
                    state.mark_error(
                        "Подключение прервано. Выполните отключение для очистки.",
                        true,
                    );
                    state.push_event(
                        Severity::Error,
                        Component::Core,
                        "connect",
                        "Запуск подключения был прерван.",
                    );
                }
                return;
            }
            std::thread::yield_now();
        }
    }
}

fn start_failure_message(
    engine: multicore_core::Engine,
    cause: multicore_core::StartFailureCause,
) -> String {
    use multicore_core::{Engine, StartFailureCause as Cause};
    let core = match engine {
        Engine::Xray => "Xray",
        Engine::Mihomo => "Mihomo",
    };
    match cause {
        Cause::ElevationCancelled => {
            "Подключение отменено: не выдано разрешение администратора.".to_owned()
        }
        Cause::ElevationFailed => {
            "Не удалось запустить службу подключения с правами администратора.".to_owned()
        }
        Cause::BrokerFailed => "Служба подключения не ответила. Повторите попытку.".to_owned(),
        Cause::CoreFailed => format!("Не удалось запустить {core}. Проверьте исполняемые файлы."),
        Cause::ReadinessTimedOut => {
            format!("{core} не успел запуститься вовремя. Повторите попытку.")
        }
        Cause::Unknown => {
            "Не удалось запустить подключение. Проверьте исполняемые файлы.".to_owned()
        }
    }
}

fn map_core_state(state: CoreConnectionState) -> ConnectionState {
    match state {
        CoreConnectionState::Empty => ConnectionState::Empty,
        CoreConnectionState::Ready => ConnectionState::Ready,
        CoreConnectionState::Connecting => ConnectionState::Connecting,
        CoreConnectionState::Connected => ConnectionState::Connected,
        CoreConnectionState::Error => ConnectionState::Error,
    }
}

fn build_catalog(
    snapshot: &Snapshot,
    revision: u64,
    profile: Option<&str>,
    selected_nodes: &[Option<String>],
) -> CatalogDto {
    let source_groups = selectable_groups(snapshot);
    let selected_group_index =
        profile.and_then(|profile| source_groups.iter().position(|group| group.name == profile));
    let groups = source_groups
        .iter()
        .enumerate()
        .map(|(group_index, group)| {
            let selected = selected_group_index == Some(group_index);
            let selected_node_index = selected_nodes
                .get(group_index)
                .and_then(Option::as_deref)
                .and_then(|current_node| group.nodes.iter().position(|node| *node == current_node));
            let nodes = group
                .nodes
                .iter()
                .enumerate()
                .map(|(node_index, node)| NodeDto {
                    id: catalog_node_id(revision, group_index, node_index),
                    label: safe_catalog_label(node, "Узел", node_index),
                    selected: selected_node_index == Some(node_index),
                    delay_ms: None,
                })
                .collect();
            GroupDto {
                id: catalog_group_id(revision, group_index),
                label: safe_catalog_label(group.name, "Группа", group_index),
                selected,
                nodes,
            }
        })
        .collect();
    CatalogDto { revision, groups }
}

struct SelectableGroup<'a> {
    name: &'a str,
    nodes: Vec<&'a str>,
}

/// The exact name-addressable namespace exposed by Mihomo's controller. Only interactive
/// selectors are included, and the first duplicate raw identifier wins.
fn selectable_groups(snapshot: &Snapshot) -> Vec<SelectableGroup<'_>> {
    let mut group_names = HashSet::new();
    snapshot
        .mihomo
        .groups()
        .iter()
        .filter(|group| group_names.insert(group.name.as_str()))
        .filter(|group| group.group_type.eq_ignore_ascii_case("select"))
        .take(MAX_CATALOG_GROUPS)
        .map(|group| {
            let mut node_names = HashSet::new();
            let nodes = group
                .proxies
                .iter()
                .map(String::as_str)
                .filter(|node| node_names.insert(*node))
                .take(MAX_CATALOG_NODES_PER_GROUP)
                .collect();
            SelectableGroup {
                name: &group.name,
                nodes,
            }
        })
        .collect()
}

fn catalog_group_id(revision: u64, group_index: usize) -> String {
    format!("g-{revision:016x}-{group_index:04x}")
}

fn catalog_node_id(revision: u64, group_index: usize, node_index: usize) -> String {
    format!("n-{revision:016x}-{group_index:04x}-{node_index:04x}")
}

/// Accepts only Unicode letters/numbers, ASCII space and this harmless punctuation:
/// `- _ . , ( ) [ ] + # !`. Everything else receives a positional generic label.
fn safe_catalog_label(source: &str, kind: &str, index: usize) -> String {
    // Nothing after this boundary can be published, so no presentation safety scan may spend
    // time or memory proportional to the rest of an untrusted (up to 32 MiB) YAML scalar.
    let published_end = source
        .char_indices()
        .nth(MAX_CATALOG_NAME_CHARS)
        .map_or(source.len(), |(byte_index, _)| byte_index);
    let published = &source[..published_end];
    if published.is_empty()
        || contains_uuid_like(published)
        || contains_structured_credential(published)
        || !published.chars().all(is_safe_catalog_label_char)
    {
        return format!("{kind} {}", index + 1);
    }
    let published = published.trim_matches(' ');
    if published.is_empty() {
        return format!("{kind} {}", index + 1);
    }
    published.to_owned()
}

/// Blocks recognizable credential structures. This intentionally does not claim to infer
/// arbitrary secrets from otherwise ordinary-looking names.
fn contains_structured_credential(source: &str) -> bool {
    let mut previous_was_api = false;
    for word in source
        .split(|character: char| !character.is_alphanumeric())
        .filter(|word| !word.is_empty())
    {
        let credential_word = [
            "authorization",
            "bearer",
            "basic",
            "token",
            "password",
            "passwd",
            "secret",
            "apikey",
            "subscription",
        ]
        .iter()
        .any(|credential| word.eq_ignore_ascii_case(credential));
        if credential_word || (previous_was_api && word.eq_ignore_ascii_case("key")) {
            return true;
        }
        previous_was_api = word.eq_ignore_ascii_case("api");
    }

    source
        .split(|character: char| {
            !(character.is_ascii_alphanumeric() || matches!(character, '+' | '/' | '_' | '-'))
        })
        .filter(|run| !run.is_empty())
        .any(base64_run_contains_credential)
}

fn base64_run_contains_credential(run: &str) -> bool {
    scan_base64_run(run).credential
}

#[derive(Debug, Default)]
struct Base64Scan {
    credential: bool,
    decode_attempts: usize,
    source_bytes_visited: usize,
    encoded_bytes_decoded: usize,
    decoded_bytes_visited: usize,
}

fn scan_base64_run(run: &str) -> Base64Scan {
    let mut scan = Base64Scan::default();
    if scan_explicit_base64_candidates(run, &mut scan)
        || scan_phase_aligned_segments(
            run,
            is_standard_base64_byte,
            &general_purpose::STANDARD_NO_PAD,
            &mut scan,
        )
        || scan_phase_aligned_segments(
            run,
            is_url_safe_base64_byte,
            &general_purpose::URL_SAFE_NO_PAD,
            &mut scan,
        )
    {
        scan.credential = true;
    }
    scan
}

fn scan_explicit_base64_candidates(run: &str, scan: &mut Base64Scan) -> bool {
    let standard_compatible = run.bytes().all(is_standard_base64_byte);
    scan.source_bytes_visited += run.len();
    let url_compatible = run.bytes().all(is_url_safe_base64_byte);
    scan.source_bytes_visited += run.len();

    if standard_compatible {
        if decode_candidate(run, &general_purpose::STANDARD_NO_PAD, true, scan) {
            return true;
        }
    } else if url_compatible && decode_candidate(run, &general_purpose::URL_SAFE_NO_PAD, true, scan)
    {
        return true;
    }

    // These characters are explicit presentation separators even though each belongs to one
    // Base64 alphabet. Checking the maximal alphanumeric components preserves standalone
    // short credentials such as `YTpi` without accepting arbitrary interior suffixes.
    scan.source_bytes_visited += run.len();
    run.split(['-', '_', '+', '/'])
        .filter(|candidate| !candidate.is_empty() && candidate.len() != run.len())
        .any(|candidate| decode_candidate(candidate, &general_purpose::STANDARD_NO_PAD, true, scan))
}

fn is_standard_base64_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'/')
}

fn is_url_safe_base64_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')
}

fn scan_phase_aligned_segments(
    run: &str,
    accepts: fn(u8) -> bool,
    engine: &general_purpose::GeneralPurpose,
    scan: &mut Base64Scan,
) -> bool {
    // An embedded Base64 token can start in only one of four source phases. Decode each phase
    // once per maximal alphabet-compatible segment, then inspect the decoded stream. Across
    // all segments this visits/decodes only a constant multiple of the capped source length.
    scan.source_bytes_visited += run.len();
    let bytes = run.as_bytes();
    let mut segment_start = 0;
    while segment_start < bytes.len() {
        while segment_start < bytes.len() && !accepts(bytes[segment_start]) {
            segment_start += 1;
        }
        let mut segment_end = segment_start;
        while segment_end < bytes.len() && accepts(bytes[segment_end]) {
            segment_end += 1;
        }
        for phase in 0..4.min(segment_end.saturating_sub(segment_start)) {
            let start = segment_start + phase;
            let mut end = segment_end;
            if (end - start) % 4 == 1 {
                end -= 1;
            }
            if end - start >= 4 && decode_candidate(&run[start..end], engine, false, scan) {
                return true;
            }
        }
        segment_start = segment_end.saturating_add(1);
    }
    false
}

fn decode_candidate(
    candidate: &str,
    engine: &general_purpose::GeneralPurpose,
    allow_short_pair: bool,
    scan: &mut Base64Scan,
) -> bool {
    if candidate.len() < 4 || candidate.len() > MAX_CATALOG_NAME_CHARS || candidate.len() % 4 == 1 {
        return false;
    }
    let mut decoded = [0_u8; MAX_CATALOG_NAME_CHARS];
    scan.decode_attempts += 1;
    scan.encoded_bytes_decoded += candidate.len();
    let Ok(decoded_len) = engine.decode_slice(candidate, &mut decoded) else {
        return false;
    };
    scan.decoded_bytes_visited += decoded_len;
    decoded_contains_credential(&decoded[..decoded_len], allow_short_pair)
}

fn decoded_contains_credential(decoded: &[u8], allow_short_pair: bool) -> bool {
    let entire_record_is_printable = decoded.iter().all(|byte| (b' '..=b'~').contains(byte));
    let semantic_credential = decoded
        .split(|byte| !byte.is_ascii_alphanumeric())
        .filter(|word| !word.is_empty())
        .any(|word| {
            [
                b"authorization".as_slice(),
                b"bearer",
                b"basic",
                b"token",
                b"password",
                b"passwd",
                b"secret",
                b"apikey",
                b"subscription",
            ]
            .iter()
            .any(|credential| word.eq_ignore_ascii_case(credential))
        });
    if semantic_credential {
        return true;
    }

    decoded.iter().enumerate().any(|(colon, byte)| {
        if *byte != b':' {
            return false;
        }
        let left_len = decoded[..colon]
            .iter()
            .rev()
            .take_while(|byte| byte.is_ascii_graphic() && **byte != b':')
            .count();
        let right_len = decoded[colon + 1..]
            .iter()
            .take_while(|byte| byte.is_ascii_graphic() && **byte != b':')
            .count();
        (left_len > 0 && right_len > 0 && allow_short_pair && entire_record_is_printable)
            || (left_len >= 3 && right_len >= 3)
            || (left_len >= 2
                && right_len >= 2
                && decoded[colon - left_len..colon]
                    .iter()
                    .chain(&decoded[colon + 1..colon + 1 + right_len])
                    .any(u8::is_ascii_alphanumeric))
    })
}

fn is_safe_catalog_label_char(character: char) -> bool {
    if character.is_control() {
        return false;
    }
    character.is_alphanumeric()
        || character == ' '
        || matches!(
            character,
            '-' | '_'
                | '.'
                | ','
                | '('
                | ')'
                | '['
                | ']'
                | '+'
                | '#'
                | '!'
                | '\u{2190}'..='\u{21ff}'
                | '\u{2600}'..='\u{27bf}'
                | '\u{1f1e6}'..='\u{1f1ff}'
                | '\u{1f300}'..='\u{1faff}'
                | '\u{fe0f}'
        )
}

fn contains_uuid_like(source: &str) -> bool {
    source.as_bytes().windows(36).any(|candidate| {
        candidate.iter().enumerate().all(|(index, byte)| {
            if matches!(index, 8 | 13 | 18 | 23) {
                *byte == b'-'
            } else {
                byte.is_ascii_hexdigit()
            }
        })
    })
}

fn core_event_to_dto(id: u64, event: &Event) -> EventDto {
    let value = serde_json::to_value(event).unwrap_or_default();
    let details = value
        .get("structured_details")
        .filter(
            |details| !matches!(details, serde_json::Value::Object(fields) if fields.is_empty()),
        )
        .cloned();
    EventDto {
        id,
        timestamp: value
            .get("timestamp_ms")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or_default()
            .to_string(),
        level: value
            .get("severity")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("error")
            .to_owned(),
        component: value
            .get("component")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("core")
            .to_owned(),
        message: value
            .get("safe_message")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("Событие недоступно.")
            .to_owned(),
        details,
    }
}

fn unix_timestamp_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

#[derive(Default)]
pub struct MemoryBackend {
    inner: RwLock<MemoryState>,
}

struct MemoryState {
    connection: ConnectionState,
    profiles: Vec<MemoryProfile>,
    active_profile_id: Option<ProfileId>,
    next_profile_id: u64,
    events: EventBuffer<EventDto>,
}

impl Default for MemoryState {
    fn default() -> Self {
        Self {
            connection: ConnectionState::Empty,
            profiles: Vec::new(),
            active_profile_id: None,
            next_profile_id: 1,
            events: EventBuffer::default(),
        }
    }
}

struct MemoryProfile {
    id: ProfileId,
    source_url: String,
    display_name: String,
}

impl MemoryState {
    fn status(&self) -> StatusDto {
        let active = self.active_profile_id.as_ref().and_then(|active_id| {
            self.profiles
                .iter()
                .find(|profile| &profile.id == active_id)
        });
        StatusDto {
            state: self.connection,
            active_profile_id: active.map(|profile| profile.id.as_str().to_owned()),
            profile: active.map(|profile| profile.display_name.clone()),
            current_node: active.map(|_| "Авто".to_owned()),
            message: None,
            degraded: false,
            subscription: None,
        }
    }

    fn subscriptions(&self) -> SubscriptionListDto {
        SubscriptionListDto {
            active_profile_id: self
                .active_profile_id
                .as_ref()
                .map(|id| id.as_str().to_owned()),
            profiles: self
                .profiles
                .iter()
                .map(|profile| SubscriptionProfileDto {
                    id: profile.id.as_str().to_owned(),
                    active: self.active_profile_id.as_ref() == Some(&profile.id),
                    available: true,
                    display_name: profile.display_name.clone(),
                    used_bytes: None,
                    total_bytes: None,
                    expires_at_unix: None,
                    service_logo_path: None,
                })
                .collect(),
        }
    }

    fn require_mutable(&self) -> Result<(), BackendError> {
        if matches!(
            self.connection,
            ConnectionState::Connecting | ConnectionState::Connected
        ) {
            Err(BackendError::from_profile_mutation(
                ProfileMutationError::Connected,
            ))
        } else {
            Ok(())
        }
    }

    fn push_event(&mut self, level: &str, message: &str) {
        self.events.push_with(|id| EventDto {
            id,
            timestamp: id.to_string(),
            level: level.to_owned(),
            component: "system".to_owned(),
            message: message.to_owned(),
            details: None,
        });
    }
}

#[async_trait]
impl Backend for MemoryBackend {
    async fn status(&self) -> Result<StatusDto, BackendError> {
        Ok(self.inner.read().await.status())
    }

    async fn catalog(&self) -> Result<CatalogDto, BackendError> {
        Ok(CatalogDto {
            revision: 0,
            groups: Vec::new(),
        })
    }

    async fn import_subscription(
        &self,
        request: ImportSubscriptionRequest,
    ) -> Result<StatusDto, BackendError> {
        let mut state = self.inner.write().await;
        state.require_mutable()?;
        let source_url = request.url.trim();
        if let Some(existing_id) = state
            .profiles
            .iter()
            .find(|profile| profile.source_url == source_url)
            .map(|profile| profile.id.clone())
        {
            state.active_profile_id = Some(existing_id);
        } else {
            if state.profiles.len() >= MAX_PROFILES {
                return Err(BackendError::from_profile_mutation(
                    ProfileMutationError::LimitReached,
                ));
            }
            let id = ProfileId::parse(&format!(
                "00000000-0000-4000-8000-{:012x}",
                state.next_profile_id
            ))
            .map_err(|_| BackendError::new())?;
            state.next_profile_id = state.next_profile_id.saturating_add(1);
            let display_name = format!("Профиль {}", state.profiles.len() + 1);
            state.profiles.push(MemoryProfile {
                id: id.clone(),
                source_url: source_url.to_owned(),
                display_name,
            });
            state.active_profile_id = Some(id);
        }
        state.connection = ConnectionState::Ready;
        state.push_event("info", "Подписка импортирована.");
        Ok(state.status())
    }

    async fn refresh_subscription(&self) -> Result<StatusDto, BackendError> {
        let mut state = self.inner.write().await;
        if state.active_profile_id.is_none() {
            return Err(BackendError::from_profile_mutation(
                ProfileMutationError::NotFound,
            ));
        }
        if state.connection == ConnectionState::Connecting {
            return Err(BackendError::from_profile_mutation(
                ProfileMutationError::Connected,
            ));
        }
        if state.connection == ConnectionState::Connected {
            state.connection = ConnectionState::Ready;
        }
        Ok(state.status())
    }

    async fn list_subscriptions(&self) -> Result<SubscriptionListDto, BackendError> {
        Ok(self.inner.read().await.subscriptions())
    }

    async fn activate_subscription(&self, id: ProfileId) -> Result<StatusDto, BackendError> {
        let mut state = self.inner.write().await;
        if !state.profiles.iter().any(|profile| profile.id == id) {
            return Err(BackendError::from_profile_mutation(
                ProfileMutationError::NotFound,
            ));
        }
        state.require_mutable()?;
        state.active_profile_id = Some(id);
        state.connection = ConnectionState::Ready;
        Ok(state.status())
    }

    async fn refresh_subscription_profile(
        &self,
        id: ProfileId,
    ) -> Result<SubscriptionListDto, BackendError> {
        let mut state = self.inner.write().await;
        if !state.profiles.iter().any(|profile| profile.id == id) {
            return Err(BackendError::from_profile_mutation(
                ProfileMutationError::NotFound,
            ));
        }
        if state.connection == ConnectionState::Connecting {
            return Err(BackendError::from_profile_mutation(
                ProfileMutationError::Connected,
            ));
        }
        if state.active_profile_id.as_ref() == Some(&id)
            && state.connection == ConnectionState::Connected
        {
            state.connection = ConnectionState::Ready;
        }
        Ok(state.subscriptions())
    }

    async fn replace_subscription_source(
        &self,
        id: ProfileId,
        request: ImportSubscriptionRequest,
    ) -> Result<SubscriptionListDto, BackendError> {
        let mut state = self.inner.write().await;
        let Some(position) = state.profiles.iter().position(|profile| profile.id == id) else {
            return Err(BackendError::from_profile_mutation(
                ProfileMutationError::NotFound,
            ));
        };
        state.require_mutable()?;
        state.profiles[position].source_url = request.url.trim().to_owned();
        Ok(state.subscriptions())
    }

    async fn delete_subscription(&self, id: ProfileId) -> Result<StatusDto, BackendError> {
        let mut state = self.inner.write().await;
        let Some(position) = state.profiles.iter().position(|profile| profile.id == id) else {
            return Err(BackendError::from_profile_mutation(
                ProfileMutationError::NotFound,
            ));
        };
        if state.connection == ConnectionState::Connecting {
            return Err(BackendError::from_profile_mutation(
                ProfileMutationError::Connected,
            ));
        }
        let was_active = state.active_profile_id.as_ref() == Some(&id);
        state.profiles.remove(position);
        if was_active {
            state.active_profile_id = state
                .profiles
                .get(position)
                .or_else(|| state.profiles.last())
                .map(|profile| profile.id.clone());
        }
        if was_active {
            state.connection = if state.profiles.is_empty() {
                ConnectionState::Empty
            } else {
                ConnectionState::Ready
            };
        }
        Ok(state.status())
    }

    async fn connect(&self) -> Result<StatusDto, BackendError> {
        let mut state = self.inner.write().await;
        state.connection = ConnectionState::Connected;
        state.push_event("info", "Подключение установлено.");
        Ok(state.status())
    }

    async fn disconnect(&self) -> Result<StatusDto, BackendError> {
        let mut state = self.inner.write().await;
        state.connection = if state.profiles.is_empty() {
            ConnectionState::Empty
        } else {
            ConnectionState::Ready
        };
        state.push_event("info", "Подключение остановлено.");
        Ok(state.status())
    }

    async fn events_after(&self, after: u64) -> Result<Vec<EventDto>, BackendError> {
        let state = self.inner.read().await;
        Ok(state
            .events
            .after(after)
            .map(|(_, event)| event.clone())
            .collect())
    }

    async fn diagnostics(&self) -> Result<DiagnosticsDto, BackendError> {
        Ok(DiagnosticsDto::default())
    }
}

#[cfg(test)]
mod presentation_safety_tests {
    use super::{MAX_CATALOG_NAME_CHARS, scan_base64_run};

    #[test]
    fn base64_scanner_has_linear_attempt_and_total_byte_bounds() {
        let solid = "A".repeat(MAX_CATALOG_NAME_CHARS);
        let mixed = "AAAA_AAAA+AAAA-AAAA/"
            .repeat(MAX_CATALOG_NAME_CHARS / 20 + 1)
            .chars()
            .take(MAX_CATALOG_NAME_CHARS)
            .collect::<String>();
        for adversarial in [solid, mixed] {
            let scan = scan_base64_run(&adversarial);
            assert!(!scan.credential);
            assert!(
                scan.decode_attempts <= MAX_CATALOG_NAME_CHARS * 4,
                "{} decoder attempts exceeded the linear bound",
                scan.decode_attempts
            );
            let total_bytes =
                scan.source_bytes_visited + scan.encoded_bytes_decoded + scan.decoded_bytes_visited;
            assert!(
                total_bytes <= MAX_CATALOG_NAME_CHARS * 24,
                "{total_bytes} total visited/decoded bytes exceeded the linear bound"
            );
        }
    }
}
