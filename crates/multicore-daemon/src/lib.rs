pub mod connection_mode;
pub mod dashboard_proxy;
pub mod elevated_controller;
pub mod service_client;
pub mod system_proxy;
pub mod troubleshoot;

use std::{
    collections::{BTreeMap, HashMap, HashSet, VecDeque},
    ffi::OsStr,
    fmt,
    fs::{self, File, OpenOptions},
    io::{self, Seek, SeekFrom, Write},
    marker::PhantomData,
    net::SocketAddr,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
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
    DiagnosticStream, Engine, Event, FetchDiagnostics, FetchError, HttpClient, MAX_PROFILES,
    ProcessController, ProcessDiagnostics, ProfileError, ProfileId, ProfileStore, ProfileSummary,
    RuntimeCheckState, ServiceLogoDownload, ServiceLogoLease, Severity, Snapshot, SnapshotCommit,
    StagedRuntime, StateMachine, SubscriptionFetcher, Supervisor,
};
pub use multicore_i18n::MessageKey;
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
/// Maximum number of in-flight sweep probes (whole groups) to the Mihomo controller.
pub const MAX_CONCURRENT_LATENCY_PROBES: usize = 8;
/// Maximum number of in-flight priority probes: explicitly requested nodes (a clicked, selected
/// or re-measured row) and each swept group's selected node. They have their own budget so
/// they never queue behind a sweep.
pub const MAX_PRIORITY_LATENCY_PROBES: usize = 4;
const LATENCY_PROBE_TIMEOUT: Duration = Duration::from_secs(3);
/// How long a finished probe stays available to requests that were already waiting for it.
const LATENCY_RESULT_RETENTION: Duration = Duration::from_secs(60);
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
const CONNECTING_MESSAGE: &str = "connect.starting";
#[cfg(not(windows))]
const CONNECTING_MESSAGE: &str = "connect.starting_confirm_admin";
const TUNNEL_ON_MESSAGE: &str = "connect.tunnel_on";
const WARM_UP_FAILED_MESSAGE: &str = "cores.warm_up_failed";
const CORES_LOST_MESSAGE: &str = "cores.lost";

/// A user-facing daemon message: the Russian text every client gets, and, for registered
/// messages, the stable code the desktop shows in its own language
/// (`multicore_i18n::daemon_messages`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DaemonText {
    pub text: String,
    pub key: Option<MessageKey>,
}

impl DaemonText {
    /// A registered message without arguments.
    ///
    /// # Panics
    /// When `code` is not registered (a programming error, covered by tests).
    #[must_use]
    pub fn code(code: &str) -> Self {
        MessageKey::new(code).into()
    }

    /// A registered message with arguments.
    #[must_use]
    pub fn with_args(code: &str, args: impl IntoIterator<Item = impl Into<String>>) -> Self {
        MessageKey::with_args(code, args).into()
    }
}

impl From<MessageKey> for DaemonText {
    fn from(key: MessageKey) -> Self {
        let text = key
            .russian()
            .unwrap_or_else(|| panic!("unregistered daemon message code {}", key.code));
        Self {
            text,
            key: Some(key),
        }
    }
}

impl From<String> for DaemonText {
    fn from(text: String) -> Self {
        Self { text, key: None }
    }
}

/// Text without a code (diagnostic English the desktop shows as is).
impl From<&str> for DaemonText {
    fn from(text: &str) -> Self {
        Self {
            text: text.to_owned(),
            key: None,
        }
    }
}

/// Always-ready mode: the state of the always-ready service as the daemon last saw it.
pub trait CoresReadiness: Send + Sync + 'static {
    /// The service's cores run for the generation it prepared.
    fn cores_ready(&self) -> bool;
    /// The service can be used right now (re-checked continuously, so a service that starts
    /// after the app is picked up). While false the daemon behaves as without a service.
    fn service_available(&self) -> bool {
        true
    }
    /// A connection the service carries (its tunnel, or its mixed port behind the system
    /// proxy) is gone.
    fn connection_lost(&self) -> bool {
        false
    }
    /// Lets the service stop the cores (Quit, or no subscription left). Blocking.
    fn release(&self) {}
}

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
    /// The stable code of `message` (see `multicore_i18n::daemon_messages`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_key: Option<MessageKey>,
    pub degraded: bool,
    pub subscription: Option<SubscriptionInfoDto>,
    /// Mihomo and Xray are running (connected, or warm through the always-ready service).
    #[serde(default)]
    pub cores_ready: bool,
    /// This daemon drives the always-ready MultiCore service.
    #[serde(default)]
    pub service_mode: bool,
    /// Only in an import's answer: the profile it added or updated. It differs from
    /// `active_profile_id` when the import was kept in the list without switching to it
    /// (connected, and switching would need a disconnect).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub imported_profile_id: Option<String>,
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
    /// The provider logo is being downloaded in the background: `service_logo_path` still
    /// names the previous one (or none) until it arrives.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub service_logo_pending: bool,
    pub refresh_available: bool,
    /// The provider's `dumb-mode` header: clients show only the first route group.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub dumb_mode: bool,
    /// The provider's `app-language` header (`ru`/`en`): the interface language the desktop
    /// applies until the user picks one in Settings.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app_language: Option<String>,
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
    /// The stable code of `message` (see `multicore_i18n::daemon_messages`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message_key: Option<MessageKey>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub details: Option<serde_json::Value>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct EventPageDto {
    pub epoch: String,
    pub events: Vec<EventDto>,
}

/// Per-connect choices sent as the optional JSON body of `POST /v1/connect`. An empty body
/// keeps the historical behaviour (TUN).
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ConnectOptions {
    /// `false` connects in system-proxy mode: no TUN adapter and no administrator prompt.
    #[serde(default = "default_tun")]
    pub tun: bool,
    /// Set by the always-ready warm-up, never by a client: the controller must come from the
    /// service, never from an on-demand elevation (no prompt without a Connect).
    #[serde(skip)]
    pub warm_up: bool,
}

fn default_tun() -> bool {
    true
}

impl Default for ConnectOptions {
    fn default() -> Self {
        Self {
            tun: true,
            warm_up: false,
        }
    }
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
    /// Deliberately off (TUN in system-proxy mode).
    Disabled,
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

/// `/v1/service-log`: the always-ready service's own log for «Сохранить лог…».
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct ServiceLogDto {
    pub status: ServiceLogStatus,
    /// `service.log`, then `service.log.1`; only files with text. Redacted by the service.
    #[serde(default)]
    pub files: Vec<ServiceLogFileDto>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ServiceLogStatus {
    /// The service answered; `files` is what it has.
    Included,
    /// No service is used by this copy (per-user mode, or none installed).
    NoService,
    /// The service predates `ServiceLog`.
    TooOld,
    /// Another user's session holds the service.
    Busy,
    /// The service did not answer.
    #[default]
    Unavailable,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct ServiceLogFileDto {
    pub name: String,
    pub text: String,
    /// Older lines exist that are not in `text`.
    #[serde(default)]
    pub truncated: bool,
}

/// `/v1/core-journal?since_ms=`: this user's core journal as the always-ready service
/// keeps it (see `multicore_core::core_sessions`), for «Сохранить лог…».
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct CoreJournalDto {
    pub status: ServiceLogStatus,
    /// Whole journal lines, oldest first, redacted by the service.
    #[serde(default)]
    pub text: String,
    /// Older lines exist that are not in `text` (the size cap was reached).
    #[serde(default)]
    pub truncated: bool,
}

/// The always-ready service's own log (see `service_client::ServiceSession::service_log`).
pub trait ServiceLogSource: Send + Sync + 'static {
    /// Blocking.
    fn service_log(&self) -> ServiceLogDto;

    /// Blocking. This user's core journal in the service, from segments written at or after
    /// `since_ms`.
    fn core_journal(&self, _since_ms: u64) -> CoreJournalDto {
        CoreJournalDto::default()
    }
}

/// The live connection's session: when it started and Mihomo's cumulative byte counters,
/// from which a client derives uptime and current speed.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SessionDto {
    pub connected_since_unix_ms: u64,
    /// Absent when the Mihomo controller could not be read this time.
    pub download_total: Option<u64>,
    pub upload_total: Option<u64>,
    /// The node an automatic group (url-test, fallback, ...) currently routes through,
    /// when the selected exit is such a group; absent when the exit is already a node.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_now: Option<String>,
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
    /// Opaque id of the group's downloaded `icon` image (`GET /v1/group-icons/{id}`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon_id: Option<String>,
    /// The group names an icon image that is still being downloaded: the UI shows a loading
    /// placeholder until the catalog carries `icon_id` (or settles without one).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub icon_pending: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct NodeDto {
    pub id: String,
    pub label: String,
    pub selected: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delay_ms: Option<u64>,
    /// When the node is itself a proxy group (e.g. a url-test "Auto"), that group's icon.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon_id: Option<String>,
    /// That icon is still being downloaded (see `GroupDto::icon_pending`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub icon_pending: bool,
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
    start_failure: Option<(Engine, multicore_core::StartFailureCause)>,
}

impl BackendError {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            profile_mutation: None,
            start_failure: None,
        }
    }

    #[must_use]
    pub const fn from_profile_mutation(error: ProfileMutationError) -> Self {
        Self {
            profile_mutation: Some(error),
            start_failure: None,
        }
    }

    /// A Connect whose cores did not start: which core, and why.
    #[must_use]
    pub const fn from_start_failure(
        engine: Engine,
        cause: multicore_core::StartFailureCause,
    ) -> Self {
        Self {
            profile_mutation: None,
            start_failure: Some((engine, cause)),
        }
    }

    #[must_use]
    pub const fn profile_mutation(&self) -> Option<ProfileMutationError> {
        self.profile_mutation
    }

    #[must_use]
    pub const fn start_failure(&self) -> Option<(Engine, multicore_core::StartFailureCause)> {
        self.start_failure
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
pub enum SessionError {
    NotConnected,
}

impl fmt::Display for SessionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("no live connection session")
    }
}

impl std::error::Error for SessionError {}

/// Mihomo's cumulative traffic counters since its start.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TrafficTotals {
    pub download: u64,
    pub upload: u64,
}

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

    async fn traffic_totals(&self) -> Result<TrafficTotals, SelectorError> {
        Err(SelectorError::new())
    }

    /// The `now` member of a Mihomo proxy group (`None` for a plain node).
    async fn proxy_now(&self, _raw_name: &str) -> Result<Option<String>, SelectorError> {
        Err(SelectorError::new())
    }
}

/// Deepest `now` chain followed from the selected exit (group -> group -> node).
const MAX_EXIT_NOW_DEPTH: usize = 8;
/// How long a resolved exit is reused; the session is polled about once a second.
const EXIT_NOW_TTL: Duration = Duration::from_secs(5);

/// Follows Mihomo `now` pointers from `start` to the node traffic actually leaves through.
/// Returns `None` when `start` is itself a node, on a cycle, on too deep a chain, or when the
/// controller cannot be read.
pub async fn resolve_exit_now(selector: &dyn MihomoSelector, start: &str) -> Option<String> {
    let mut visited = HashSet::from([start.to_owned()]);
    let mut current = start.to_owned();
    for _ in 0..MAX_EXIT_NOW_DEPTH {
        // A node that cannot be read (not a group, or the service's filtered controller
        // refused its name) ends the walk: what was reached so far is the exit. Only a
        // failure on the very first lookup means the controller itself is unreadable.
        let looked_up = match selector.proxy_now(&current).await {
            Ok(now) => now,
            Err(_) if current != start => None,
            Err(_) => return None,
        };
        match looked_up {
            Some(next) if !next.is_empty() => {
                if !visited.insert(next.clone()) {
                    return None;
                }
                current = next;
            }
            _ => return (current != start).then_some(current),
        }
    }
    None
}

struct ExitNowCache {
    at: std::time::Instant,
    start: String,
    resolved: Option<String>,
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
        multicore_core::ensure_crypto_provider_installed();
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

    async fn proxy_now(&self, raw_name: &str) -> Result<Option<String>, SelectorError> {
        let mut url = self.base_url.clone();
        url.path_segments_mut()
            .map_err(|_| SelectorError::new())?
            .extend(["proxies", raw_name]);
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
        Ok(live
            .get("now")
            .and_then(serde_json::Value::as_str)
            .filter(|now| !now.is_empty())
            .map(str::to_owned))
    }

    async fn traffic_totals(&self) -> Result<TrafficTotals, SelectorError> {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct ConnectionsTotals {
            download_total: u64,
            upload_total: u64,
        }

        let mut url = self.base_url.clone();
        url.path_segments_mut()
            .map_err(|_| SelectorError::new())?
            .push("connections");
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
        // The body also lists every open connection; busy sessions reach a few MiB.
        const MAX_CONNECTIONS_RESPONSE_BYTES: usize = 16 * 1024 * 1024;
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|_| SelectorError::new())? {
            if body.len().saturating_add(chunk.len()) > MAX_CONNECTIONS_RESPONSE_BYTES {
                return Err(SelectorError::new());
            }
            body.extend_from_slice(&chunk);
        }
        let totals: ConnectionsTotals =
            serde_json::from_slice(&body).map_err(|_| SelectorError::new())?;
        Ok(TrafficTotals {
            download: totals.download_total,
            upload: totals.upload_total,
        })
    }
}

type ProbeOutcome = Result<u64, LatencyProbeError>;

/// Probes in flight and recently finished, by raw Mihomo proxy name, shared by every latency
/// request: a node already being measured is joined instead of measured twice, and a sweep that
/// reaches a node measured since it began reuses that result.
#[derive(Default)]
struct LatencyFlights {
    in_flight: HashMap<String, tokio::sync::watch::Receiver<Option<ProbeOutcome>>>,
    finished: HashMap<String, (tokio::time::Instant, ProbeOutcome)>,
}

impl LatencyFlights {
    /// A finished result no older than `since`, or the receiver of a probe in flight.
    fn join(
        &mut self,
        raw_node: &str,
        since: tokio::time::Instant,
    ) -> Option<Result<ProbeOutcome, tokio::sync::watch::Receiver<Option<ProbeOutcome>>>> {
        if let Some((finished_at, outcome)) = self.finished.get(raw_node)
            && *finished_at >= since
        {
            return Some(Ok(*outcome));
        }
        let receiver = self.in_flight.get(raw_node)?;
        // A probe whose task is gone (runtime shutting down) is no probe to wait for.
        if receiver.has_changed().is_err() {
            self.in_flight.remove(raw_node);
            return None;
        }
        Some(Err(receiver.clone()))
    }
}

/// Where a latency target is measured.
#[derive(Clone)]
struct LatencyLane {
    selector: Arc<dyn MihomoSelector>,
    permits: Arc<tokio::sync::Semaphore>,
    flights: Arc<std::sync::Mutex<LatencyFlights>>,
}

async fn wait_for_probe(
    mut receiver: tokio::sync::watch::Receiver<Option<ProbeOutcome>>,
) -> ProbeOutcome {
    match receiver.wait_for(Option::is_some).await {
        Ok(outcome) => outcome.unwrap_or(Err(LatencyProbeError::Unavailable)),
        Err(_) => Err(LatencyProbeError::Unavailable),
    }
}

/// Measures `raw_node` once for everybody asking: joins a probe in flight, reuses one finished
/// since `since`, or takes a permit of `lane` and starts it. The probe itself runs detached and
/// bounded by `LATENCY_PROBE_TIMEOUT`, so a request that gives up (its batch deadline) never
/// cancels a measurement another request is waiting for.
async fn measure_latency(
    lane: LatencyLane,
    raw_node: String,
    since: tokio::time::Instant,
) -> ProbeOutcome {
    let joined = lane
        .flights
        .lock()
        .expect("latency flights lock")
        .join(&raw_node, since);
    match joined {
        Some(Ok(outcome)) => return outcome,
        Some(Err(receiver)) => return wait_for_probe(receiver).await,
        None => {}
    }
    let permit = lane
        .permits
        .clone()
        .acquire_owned()
        .await
        .map_err(|_| LatencyProbeError::Unavailable)?;
    let receiver = {
        let mut flights = lane.flights.lock().expect("latency flights lock");
        // Somebody may have started or finished it while this request queued for a permit.
        match flights.join(&raw_node, since) {
            Some(Ok(outcome)) => return outcome,
            Some(Err(receiver)) => Err(receiver),
            None => {
                let (sender, receiver) = tokio::sync::watch::channel(None);
                flights.in_flight.insert(raw_node.clone(), receiver.clone());
                Ok((sender, receiver))
            }
        }
    };
    let (sender, receiver) = match receiver {
        Ok(started) => started,
        Err(joined) => {
            drop(permit);
            return wait_for_probe(joined).await;
        }
    };
    let (selector, flights) = (lane.selector, lane.flights);
    tokio::spawn(async move {
        let outcome =
            tokio::time::timeout(LATENCY_PROBE_TIMEOUT, selector.probe_latency(&raw_node))
                .await
                .unwrap_or(Err(LatencyProbeError::Timeout));
        drop(permit);
        {
            let mut flights = flights.lock().expect("latency flights lock");
            let now = tokio::time::Instant::now();
            flights.in_flight.remove(&raw_node);
            flights.finished.retain(|_, (finished_at, _)| {
                now.saturating_duration_since(*finished_at) < LATENCY_RESULT_RETENTION
            });
            flights.finished.insert(raw_node, (now, outcome));
        }
        let _ = sender.send(Some(outcome));
    });
    wait_for_probe(receiver).await
}

async fn probe_latency_target(
    lane: LatencyLane,
    since: tokio::time::Instant,
    index: usize,
    group_id: String,
    node_id: String,
    raw_node: String,
) -> (usize, LatencyResultDto) {
    let (latency_ms, status) = match measure_latency(lane, raw_node, since).await {
        Ok(latency_ms) => (Some(latency_ms), LatencyStatus::Ok),
        Err(LatencyProbeError::Timeout) => (None, LatencyStatus::Timeout),
        Err(LatencyProbeError::Unavailable) => (None, LatencyStatus::Error),
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
    /// Connects with per-connect options; backends without modes ignore them.
    async fn connect_with(&self, _options: ConnectOptions) -> Result<StatusDto, BackendError> {
        self.connect().await
    }
    async fn disconnect(&self) -> Result<StatusDto, BackendError>;
    /// Quit: disconnect and let the always-ready service stop the cores.
    async fn shutdown(&self) -> Result<(), BackendError> {
        Ok(())
    }
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
    async fn session(&self) -> Result<SessionDto, SessionError> {
        Err(SessionError::NotConnected)
    }
    /// The canonical PNG of a catalog `icon_id`, when the active snapshot holds it.
    async fn group_icon(&self, _icon_id: &str) -> Option<Vec<u8>> {
        None
    }
    /// The ports the active subscription's cores listen on (troubleshooting's port check).
    async fn listen_ports(&self) -> Vec<multicore_core::troubleshoot::ListenPort> {
        Vec::new()
    }
    /// Records which troubleshooting fixes ran in the event log.
    async fn record_troubleshoot(&self, _message: DaemonText) {}
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
    /// Present when updates go through the always-ready service (Windows service install).
    service_updates: Option<Arc<dyn ServiceUpdates>>,
    /// «Устранить неполадки»; absent in tests and embedders that do not offer it.
    troubleshooting: Option<Arc<dyn troubleshoot::Troubleshooting>>,
    /// «Сохранить лог…»: the service's own log; absent means no service is used.
    service_log: Option<Arc<dyn ServiceLogSource>>,
}

/// Updates performed by the always-ready service (see `service_client::ServiceSession`).
pub trait ServiceUpdates: Send + Sync + 'static {
    /// Installs `version`, reusing a release `stage` already verified.
    fn update(&self, version: &str) -> Result<(), service_client::ServiceError>;
    /// Downloads and verifies `version` only; the cores and the tunnel keep running.
    fn stage(&self, version: &str) -> Result<(), service_client::ServiceError>;
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

fn build_router_shared(
    backend: Arc<dyn Backend>,
    bearer_token: impl AsRef<str>,
    event_epoch: impl Into<String>,
    dashboard_url: Option<Arc<str>>,
    service_updates: Option<Arc<dyn ServiceUpdates>>,
    troubleshooting: Option<Arc<dyn troubleshoot::Troubleshooting>>,
    service_log: Option<Arc<dyn ServiceLogSource>>,
) -> Router {
    let app_state = AppState {
        backend,
        event_epoch: Arc::from(event_epoch.into()),
        dashboard_url,
        service_updates,
        troubleshooting,
        service_log,
    };
    let auth_state = AuthState {
        token_hash: Sha256::digest(bearer_token.as_ref().as_bytes()).into(),
    };

    let v1 = Router::new()
        .route("/status", get(status))
        .route("/catalog", get(catalog))
        .route("/group-icons/{icon_id}", get(group_icon))
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
        .route("/shutdown", post(shutdown))
        .route("/events", get(events))
        .route("/diagnostics", get(diagnostics))
        .route("/dashboard-url", get(dashboard_url_handler))
        .route("/session", get(session))
        .route("/service/update", post(service_update))
        .route("/service/update/stage", post(service_update_stage))
        .route("/troubleshoot/check", post(troubleshoot_check))
        .route("/troubleshoot/fix", post(troubleshoot_fix))
        .route("/service-log", get(service_log_handler))
        .route("/core-journal", get(core_journal_handler))
        .fallback(not_found)
        .layer(RequestBodyLimitLayer::new(MAX_REQUEST_BODY_BYTES))
        .layer(middleware::from_fn(normalize_errors))
        .layer(middleware::from_fn_with_state(auth_state, authorize))
        .with_state(app_state);

    Router::new().nest("/v1", v1)
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
    build_router_shared(
        Arc::new(backend),
        bearer_token,
        event_epoch,
        dashboard_url,
        None,
        None,
        None,
    )
}

/// Builds the API over a backend the caller keeps a handle to (warm-up, notices).
pub fn try_router_shared(
    backend: Arc<dyn Backend>,
    bearer_token: impl AsRef<str>,
    dashboard_url: Option<Arc<str>>,
    service_updates: Option<Arc<dyn ServiceUpdates>>,
) -> Result<Router, getrandom::Error> {
    try_router_with_troubleshooting(backend, bearer_token, dashboard_url, service_updates, None)
}

/// [`try_router_shared`] that also serves `/v1/troubleshoot/*`.
pub fn try_router_with_troubleshooting(
    backend: Arc<dyn Backend>,
    bearer_token: impl AsRef<str>,
    dashboard_url: Option<Arc<str>>,
    service_updates: Option<Arc<dyn ServiceUpdates>>,
    troubleshooting: Option<Arc<dyn troubleshoot::Troubleshooting>>,
) -> Result<Router, getrandom::Error> {
    try_router_with_service_log(
        backend,
        bearer_token,
        dashboard_url,
        service_updates,
        troubleshooting,
        None,
    )
}

/// [`try_router_with_troubleshooting`] that also serves `/v1/service-log`.
pub fn try_router_with_service_log(
    backend: Arc<dyn Backend>,
    bearer_token: impl AsRef<str>,
    dashboard_url: Option<Arc<str>>,
    service_updates: Option<Arc<dyn ServiceUpdates>>,
    troubleshooting: Option<Arc<dyn troubleshoot::Troubleshooting>>,
    service_log: Option<Arc<dyn ServiceLogSource>>,
) -> Result<Router, getrandom::Error> {
    Ok(build_router_shared(
        backend,
        bearer_token,
        generate_event_epoch()?,
        dashboard_url,
        service_updates,
        troubleshooting,
        service_log,
    ))
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

async fn group_icon(
    State(state): State<AppState>,
    Path(icon_id): Path<String>,
    body: Bytes,
) -> Result<Response, ApiError> {
    reject_nonempty_body(&body)?;
    let png = if is_group_icon_id(&icon_id) {
        state.backend.group_icon(&icon_id).await
    } else {
        None
    };
    let png = png.ok_or_else(|| {
        ApiError::new(
            StatusCode::NOT_FOUND,
            "unknown_icon",
            "Значок группы не найден. Обновите каталог.",
        )
    })?;
    Ok((
        [
            (header::CONTENT_TYPE, "image/png"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        png,
    )
        .into_response())
}

fn is_group_icon_id(value: &str) -> bool {
    value.len() == 32
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
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

/// One download of a subscription (all its requests, through the connection and directly)
/// ends within this, with the subscription or with a reason: the refresh indicator never spins
/// for minutes. Well below the desktop's 150 s subscription request timeout.
const SUBSCRIPTION_FETCH_DEADLINE: Duration = Duration::from_secs(45);

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
        FetchError::DeviceLimit => "device limit".to_owned(),
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
            "По ссылке пришла веб-страница или список ссылок, а не подписка MULTICORE. Скопируйте ссылку подписки для приложения у провайдера."
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
        FetchError::DeviceLimit => (
            StatusCode::UNPROCESSABLE_ENTITY,
            "subscription_device_limit",
            "Достигнут лимит устройств для этой подписки. Отвяжите старое устройство в личном кабинете или боте провайдера либо обратитесь в поддержку."
                .to_owned(),
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
    let options = parse_connect_options(&body)?;
    state
        .backend
        .connect_with(options)
        .await
        .map(Json)
        .map_err(|error| match error.start_failure() {
            // The cores did not start: say which and why (the desktop shows a message per
            // code), instead of a bare 500 "backend_error".
            Some((engine, cause)) => ApiError::new(
                StatusCode::BAD_GATEWAY,
                start_failure_code(cause),
                &start_failure_message(engine, cause).text,
            ),
            None => ApiError::backend("Не удалось подключиться. Повторите попытку."),
        })
}

async fn shutdown(State(state): State<AppState>, body: Bytes) -> Result<StatusCode, ApiError> {
    reject_nonempty_body(&body)?;
    state
        .backend
        .shutdown()
        .await
        .map(|()| StatusCode::NO_CONTENT)
        .map_err(|_| ApiError::backend("Не удалось остановить ядра."))
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
    if status.state != ConnectionState::Connected && !status.cores_ready {
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

async fn session(State(state): State<AppState>, body: Bytes) -> Result<Json<SessionDto>, ApiError> {
    reject_nonempty_body(&body)?;
    state
        .backend
        .session()
        .await
        .map(Json)
        .map_err(|SessionError::NotConnected| {
            ApiError::new(
                StatusCode::CONFLICT,
                "not_connected",
                "Сначала установите подключение.",
            )
        })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ServiceUpdateRequest {
    version: String,
}

/// The always-ready service downloads and verifies `version` (minutes, while the cores keep
/// running), then stops the cores and hands over to its updater, which restarts the service
/// and the desktop. Waits on a blocking thread and holds no daemon lock meanwhile.
async fn service_update(
    State(state): State<AppState>,
    body: Bytes,
) -> Result<StatusCode, ApiError> {
    run_service_update(state, body, false).await
}

/// Only the download and verification of `service_update`: the connected desktop stages the
/// release with the tunnel up and disconnects only after this answered 200.
async fn service_update_stage(
    State(state): State<AppState>,
    body: Bytes,
) -> Result<StatusCode, ApiError> {
    run_service_update(state, body, true).await
}

async fn run_service_update(
    state: AppState,
    body: Bytes,
    stage_only: bool,
) -> Result<StatusCode, ApiError> {
    let Some(updates) = state.service_updates.clone() else {
        return Err(service_unavailable());
    };
    let request: ServiceUpdateRequest = serde_json::from_slice(&body)
        .map_err(|_| ApiError::bad_request("Некорректный запрос обновления."))?;
    let result = tokio::task::spawn_blocking(move || {
        if stage_only {
            updates.stage(&request.version)
        } else {
            updates.update(&request.version)
        }
    })
    .await
    .map_err(|_| ApiError::backend("Служба не ответила."))?;
    match result {
        Ok(()) if stage_only => Ok(StatusCode::OK),
        Ok(()) => Ok(StatusCode::ACCEPTED),
        Err(service_client::ServiceError::Busy) => Err(ApiError::new(
            StatusCode::CONFLICT,
            "service_busy",
            "MULTICORE уже запущен у другого пользователя.",
        )),
        // No service to talk to (removed, or not running right now): nothing was attempted.
        Err(
            service_client::ServiceError::NotInstalled
            | service_client::ServiceError::Unavailable
            | service_client::ServiceError::Impostor,
        ) => Err(service_unavailable()),
        Err(_) => Err(ApiError::new(
            StatusCode::BAD_GATEWAY,
            "service_update_failed",
            "Служба не смогла загрузить или проверить обновление.",
        )),
    }
}

/// «Сохранить лог…»: the service's own log, redacted by the service. Never an error: the
/// status says why there is no log (no service, a service too old to serve it, …).
async fn service_log_handler(
    State(state): State<AppState>,
    body: Bytes,
) -> Result<Json<ServiceLogDto>, ApiError> {
    reject_nonempty_body(&body)?;
    let Some(source) = state.service_log.clone() else {
        return Ok(Json(ServiceLogDto {
            status: ServiceLogStatus::NoService,
            files: Vec::new(),
        }));
    };
    let log = tokio::task::spawn_blocking(move || source.service_log())
        .await
        .unwrap_or_default();
    Ok(Json(log))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CoreJournalQuery {
    #[serde(default)]
    since_ms: u64,
}

/// «Сохранить лог…»: this user's core journal from the always-ready service. Never an error:
/// the status says why there is none (no service, a service too old to serve it, …).
async fn core_journal_handler(
    State(state): State<AppState>,
    query: Result<Query<CoreJournalQuery>, axum::extract::rejection::QueryRejection>,
    body: Bytes,
) -> Result<Json<CoreJournalDto>, ApiError> {
    reject_nonempty_body(&body)?;
    let Ok(Query(query)) = query else {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "invalid_query",
            "Некорректный запрос.",
        ));
    };
    let Some(source) = state.service_log.clone() else {
        return Ok(Json(CoreJournalDto {
            status: ServiceLogStatus::NoService,
            ..CoreJournalDto::default()
        }));
    };
    let journal = tokio::task::spawn_blocking(move || source.core_journal(query.since_ms))
        .await
        .unwrap_or_default();
    Ok(Json(journal))
}

/// Troubleshooting checks and fixes only run while nothing is connected: the proxy check
/// switches the system proxy off and the fixes reset networking.
async fn troubleshoot_ready(
    state: &AppState,
) -> Result<Arc<dyn troubleshoot::Troubleshooting>, ApiError> {
    let Some(troubleshooting) = state.troubleshooting.clone() else {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            "troubleshoot_unavailable",
            "Устранение неполадок недоступно.",
        ));
    };
    let status = state
        .backend
        .status()
        .await
        .map_err(|_| ApiError::backend("Не удалось проверить состояние подключения."))?;
    if matches!(
        status.state,
        ConnectionState::Connected | ConnectionState::Connecting
    ) {
        return Err(ApiError::new(
            StatusCode::CONFLICT,
            "disconnect_first",
            "Сначала отключитесь.",
        ));
    }
    Ok(troubleshooting)
}

async fn troubleshoot_check(
    State(state): State<AppState>,
    body: Bytes,
) -> Result<Json<multicore_core::troubleshoot::StepReport>, ApiError> {
    let request: multicore_core::troubleshoot::CheckRequest = serde_json::from_slice(&body)
        .map_err(|_| ApiError::bad_request("Некорректный запрос проверки."))?;
    let troubleshooting = troubleshoot_ready(&state).await?;
    let ports = state.backend.listen_ports().await;
    Ok(Json(troubleshooting.check(request.step, ports).await))
}

async fn troubleshoot_fix(
    State(state): State<AppState>,
    body: Bytes,
) -> Result<Json<multicore_core::troubleshoot::FixReport>, ApiError> {
    let request: multicore_core::troubleshoot::FixRequest = serde_json::from_slice(&body)
        .map_err(|_| ApiError::bad_request("Некорректный запрос исправления."))?;
    let fixes = troubleshoot::parse_fix_request(&request.fixes)
        .ok_or_else(|| ApiError::bad_request("Неизвестное исправление."))?;
    let troubleshooting = troubleshoot_ready(&state).await?;
    let report = troubleshooting.fix(fixes.clone()).await;
    {
        let summary = fixes
            .iter()
            .map(|fix| fix.as_text())
            .collect::<Vec<_>>()
            .join(", ");
        let failed = report
            .findings
            .iter()
            .filter(|finding| finding.tone == multicore_core::troubleshoot::Tone::Error)
            .count();
        state
            .backend
            .record_troubleshoot(DaemonText::with_args(
                "troubleshoot.fixes_applied",
                [summary, failed.to_string()],
            ))
            .await;
    }
    Ok(Json(report))
}

fn service_unavailable() -> ApiError {
    ApiError::new(
        StatusCode::NOT_FOUND,
        "service_unavailable",
        "Служба MULTICORE недоступна.",
    )
}

/// An empty body keeps the default (TUN); otherwise it must be exactly `ConnectOptions`.
fn parse_connect_options(body: &Bytes) -> Result<ConnectOptions, ApiError> {
    if body.is_empty() {
        return Ok(ConnectOptions::default());
    }
    serde_json::from_slice(body).map_err(|_| {
        ApiError::bad_request(
            "Неверные параметры подключения. Обновите приложение и повторите попытку.",
        )
    })
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
    persistent: Option<std::sync::Mutex<PersistentEventSink>>,
}

impl<T> Default for EventBuffer<T> {
    fn default() -> Self {
        Self {
            entries: VecDeque::with_capacity(MAX_RETAINED_EVENTS),
            next_event_id: 1,
            persistent: None,
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

/// Caps how large a persisted event log is allowed to grow before it is
/// cleared and started over, mirroring `multicore-core`'s
/// `MAX_PERSISTENT_LOG_BYTES` for the (separate) core stdout log.
const MAX_PERSISTENT_EVENT_LOG_BYTES: u64 = 1_048_576;

/// Disk-backed sibling of the in-memory `EventBuffer`. Unlike
/// `multicore-core`'s `PersistentLogSink` (which is scoped to a single
/// process run and truncates on open), this one is opened in place and keeps
/// prior runs' events, because the whole point is surviving a daemon
/// restart between a real failure and a user getting around to exporting a
/// diagnostics log. Every event was already redacted by `Event::new` before
/// it ever reaches here, so writing it verbatim is safe.
#[derive(Debug)]
struct PersistentEventSink {
    file: File,
    bytes_written: u64,
    disabled: bool,
}

impl PersistentEventSink {
    fn open(path: &std::path::Path) -> io::Result<Self> {
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            fs::create_dir_all(parent)?;
        }
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .read(true)
            .open(path)?;
        let bytes_written = file.seek(SeekFrom::End(0))?;
        Ok(Self {
            file,
            bytes_written,
            disabled: false,
        })
    }

    fn write_line(&mut self, line: &str) {
        if self.disabled {
            return;
        }
        let mut bytes = Vec::with_capacity(line.len() + 1);
        bytes.extend_from_slice(line.as_bytes());
        bytes.push(b'\n');
        let result = (|| -> io::Result<()> {
            if self.bytes_written.saturating_add(bytes.len() as u64)
                > MAX_PERSISTENT_EVENT_LOG_BYTES
            {
                self.file.set_len(0)?;
                self.file.seek(SeekFrom::Start(0))?;
                self.bytes_written = 0;
            }
            self.file.seek(SeekFrom::Start(self.bytes_written))?;
            self.file.write_all(&bytes)?;
            self.file.flush()?;
            self.bytes_written = self.bytes_written.saturating_add(bytes.len() as u64);
            Ok(())
        })();
        if result.is_err() {
            self.disabled = true;
        }
    }
}

fn load_persisted_events(path: &std::path::Path) -> Vec<Event> {
    let Ok(contents) = fs::read_to_string(path) else {
        return Vec::new();
    };
    contents
        .lines()
        .filter_map(|line| serde_json::from_str::<Event>(line).ok())
        .collect()
}

impl EventBuffer<Event> {
    /// Reloads events a prior daemon process persisted at `path`, then keeps
    /// appending new ones to the same file. Falls back to memory-only (the
    /// prior behaviour) if the file can't be created or read - a broken
    /// diagnostics log must never stop the daemon from running.
    fn persistent_or_memory(path: impl AsRef<std::path::Path>) -> Self {
        let path = path.as_ref();
        let mut buffer = Self::default();
        for event in load_persisted_events(path) {
            buffer.push_with(|_id| event);
        }
        if let Ok(sink) = PersistentEventSink::open(path) {
            buffer.persistent = Some(std::sync::Mutex::new(sink));
        }
        buffer
    }

    fn push_event_with(&mut self, build: impl FnOnce(u64) -> Event) -> u64 {
        let id = self.push_with(build);
        if let Some(sink) = &self.persistent
            && let Some((_, event)) = self.entries.back()
            && let Ok(line) = serde_json::to_string(event)
        {
            sink.lock()
                .expect("persistent event sink lock poisoned")
                .write_line(&line);
        }
        id
    }
}

#[cfg(test)]
mod event_persistence_tests {
    use super::{Component, Event, EventBuffer, Severity};

    fn sample_event(id: u64) -> Event {
        Event::new(
            id,
            Severity::Warning,
            Component::Subscription,
            "subscription_fetch_failed",
            format!("daemon-{id:016x}"),
            &format!("attempt {id} failed"),
            serde_json::json!({ "host": "example.invalid" }),
        )
    }

    #[test]
    fn events_persisted_by_one_buffer_are_recovered_by_a_fresh_one_at_the_same_path() {
        // Regression coverage for a real bug report: a genuine subscription-add
        // failure on a user's machine produced an empty log export
        // because the daemon's event buffer was purely in-memory and had been
        // wiped by a restart between the failure and the export.
        let directory = tempfile::tempdir().expect("temp dir");
        let path = directory.path().join("events.log");

        let mut first_run = EventBuffer::<Event>::persistent_or_memory(&path);
        first_run.push_event_with(|_| sample_event(1));
        first_run.push_event_with(|_| sample_event(2));
        drop(first_run);

        let second_run = EventBuffer::<Event>::persistent_or_memory(&path);
        let recovered: Vec<_> = second_run.after(0).map(|(_, event)| event).collect();
        assert_eq!(
            recovered.len(),
            2,
            "both prior-run events must survive a restart"
        );
        let serialized = recovered
            .iter()
            .map(|event| serde_json::to_string(event).unwrap())
            .collect::<Vec<_>>();
        assert!(serialized[0].contains("attempt 1 failed"));
        assert!(serialized[1].contains("attempt 2 failed"));
    }

    #[test]
    fn events_persisted_across_two_restarts_keep_accumulating() {
        let directory = tempfile::tempdir().expect("temp dir");
        let path = directory.path().join("events.log");

        let mut run_one = EventBuffer::<Event>::persistent_or_memory(&path);
        run_one.push_event_with(|_| sample_event(1));
        drop(run_one);

        let mut run_two = EventBuffer::<Event>::persistent_or_memory(&path);
        assert_eq!(
            run_two.after(0).count(),
            1,
            "run two must recover run one's event"
        );
        run_two.push_event_with(|_| sample_event(2));
        drop(run_two);

        let run_three = EventBuffer::<Event>::persistent_or_memory(&path);
        assert_eq!(
            run_three.after(0).count(),
            2,
            "run three must recover both prior events, not just the latest run's"
        );
    }

    #[test]
    fn missing_directory_falls_back_to_memory_instead_of_failing() {
        // The parent can't be created (it's actually a file), so
        // `PersistentEventSink::open` fails and persistence must be skipped
        // silently rather than ever stopping the daemon from starting.
        let directory = tempfile::tempdir().expect("temp dir");
        let blocking_file = directory.path().join("not-a-directory");
        std::fs::write(&blocking_file, b"blocked").expect("write blocking file");
        let path = blocking_file.join("events.log");

        let mut buffer = EventBuffer::<Event>::persistent_or_memory(&path);
        assert!(buffer.persistent.is_none());
        buffer.push_event_with(|_| sample_event(1));
        assert_eq!(buffer.after(0).count(), 1, "in-memory buffer still works");
    }
}

/// Whether a refresh made while connected left a pending snapshot the cores would run
/// differently. A refresh that only changed metadata (traffic, expiry, the provider notice, the
/// logo) keeps the connection: moving it would stop and restart the same configuration. The
/// pending snapshot is still what `status` reports, and it becomes current at the next
/// disconnect.
fn pending_changes_runtime<P: ProcessController>(state: &CoreRuntime<P>) -> bool {
    state.pending_snapshot.as_ref().is_some_and(|(_, pending)| {
        state
            .snapshot
            .as_ref()
            .is_none_or(|current| !same_runtime_payload(current, pending))
    })
}

/// Both snapshots stage the same runtime: identical Mihomo and Xray payloads.
fn same_runtime_payload(left: &Snapshot, right: &Snapshot) -> bool {
    left.mihomo.raw_yaml() == right.mihomo.raw_yaml()
        && left.xray.raw_bytes() == right.xray.raw_bytes()
}

/// Phases of an activation, in milliseconds, for the `subscription_switch` event.
#[derive(Clone, Copy, Debug, Default)]
struct ActivationTimings {
    /// Loading the profile's snapshot from the store.
    load_ms: u128,
    /// The controller factory: probing the service, staging the runtime generation.
    prepare_ms: u128,
    /// Committing the active profile and applying its catalog.
    commit_ms: u128,
}

pub struct PreparedController<P: ProcessController> {
    controller: Arc<P>,
    runtime: Option<StagedRuntime>,
    /// Why a service generation was staged instead of reusing the warm one (no secrets).
    restage_reason: Option<String>,
}

impl<P: ProcessController> PreparedController<P> {
    pub fn with_runtime(controller: Arc<P>, runtime: StagedRuntime) -> Self {
        Self {
            controller,
            runtime: Some(runtime),
            restage_reason: None,
        }
    }

    fn plain(controller: Arc<P>) -> Self {
        Self {
            controller,
            runtime: None,
            restage_reason: None,
        }
    }

    /// Records why the warm generation could not be reused, for the Connect diagnostics.
    #[must_use]
    pub fn restaged_because(mut self, reason: impl Into<String>) -> Self {
        self.restage_reason = Some(reason.into());
        self
    }

    /// A controller whose runtime generation is already active (a warm service generation).
    pub fn reuse(controller: Arc<P>) -> Self {
        Self::plain(controller)
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
    dyn Fn(&Snapshot, ConnectOptions) -> Result<PreparedController<P>, BackendError> + Send + Sync;

/// A factory that prepares the controller for the connect's [`ConnectOptions`] (TUN or system
/// proxy). Plain and transactional factories ignore the options.
pub struct OptionsControllerFactory<F>(F);

pub fn options_controller_factory<F>(factory: F) -> OptionsControllerFactory<F> {
    OptionsControllerFactory(factory)
}

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
    priority_latency_permits: Arc<tokio::sync::Semaphore>,
    latency_flights: Arc<std::sync::Mutex<LatencyFlights>>,
    exit_now_cache: std::sync::Mutex<Option<ExitNowCache>>,
    operation: tokio::sync::Mutex<()>,
    /// Held by a subscription switch or refresh across its disconnect, switch and reconnect,
    /// and by a user Connect or Disconnect, so a user request never lands between those steps
    /// (always taken before `operation`, never while holding it).
    generation_change: tokio::sync::Mutex<()>,
    /// Bumped by every Connect attempt: a warm-up that saw it change was superseded.
    connect_attempts: AtomicU64,
    /// Options of the latest connect; background preparation reuses them.
    connect_options: std::sync::Mutex<ConnectOptions>,
    post_commit_load_fault: AtomicBool,
    warm_failure_reported: AtomicBool,
    warm_signal: Arc<tokio::sync::Notify>,
    /// Quit: the service's cores were released and must not be warmed again.
    shut_down: AtomicBool,
    list_logo_publish_hook: std::sync::Mutex<Option<ListLogoPublishHook>>,
    profile_logo_leases: std::sync::Mutex<HashMap<ProfileId, ProfileLogoLease>>,
    /// Provider logos (shared with the runtime for the status).
    service_logos: Arc<ServiceLogos>,
    /// Wakes the background image task (group icons and provider logos).
    group_icons_requests: Arc<tokio::sync::Notify>,
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
    message: Option<DaemonText>,
    degraded: bool,
    /// Set on every transition into Connected (the only one is `connect`); only read while
    /// Connected, so a value left from an earlier session is never reported.
    connected_since_unix_ms: u64,
    events: EventBuffer<Event>,
    /// Catalog revision whose cores the always-ready service already runs while Ready.
    warm_revision: Option<u64>,
    readiness: Option<Arc<dyn CoresReadiness>>,
    /// Downloaded group icons; every installed snapshot gets the cached ones attached.
    group_icons: Arc<GroupIconCache>,
    /// Provider logos downloaded in the background, by profile.
    service_logos: Arc<ServiceLogos>,
    /// Catalog revision whose background icon download has finished (successfully or not).
    icons_settled_revision: Option<u64>,
    /// The per-connection log index (`multicore_core::core_sessions`); `None` until enabled.
    sessions: Option<Arc<multicore_core::core_sessions::SessionIndex>>,
    /// When the connection being recorded in `sessions` was attempted (unix ms).
    open_session_ms: Option<u64>,
}

/// Downloaded Mihomo group icons by icon URL key, shared by every profile and persisted in
/// the profile store. A subscription is saved without waiting for its icons: they are
/// downloaded in the background (`CoreBackend::fetch_group_icons`), and the next start shows
/// them at once without the icon host.
#[derive(Default)]
struct GroupIconCache {
    icons: std::sync::Mutex<BTreeMap<String, multicore_core::CachedGroupIcon>>,
    /// Wakes the background download after a snapshot with icons was installed.
    requests: Arc<tokio::sync::Notify>,
}

/// Provider logos are downloaded after the subscription was saved, never during the fetch:
/// the subscription keeps showing its previous logo (or the letter avatar) meanwhile.
#[derive(Default)]
struct ServiceLogos {
    /// The newest logo (or its removal) per profile, overriding the snapshot generation's.
    shown: std::sync::Mutex<HashMap<ProfileId, multicore_core::CachedServiceLogo>>,
    /// Downloads to do, by subscription link (an import has no profile id yet).
    jobs: std::sync::Mutex<Vec<ServiceLogoJob>>,
}

struct ServiceLogoJob {
    source_url: String,
    /// `None`: the refreshed subscription names no logo any more.
    logo_url: Option<String>,
    /// The profile was not stored yet (the import is still committing).
    attempts: u8,
}

const SERVICE_LOGO_JOB_ATTEMPTS: u8 = 5;

impl ServiceLogos {
    fn shown(&self, id: &ProfileId) -> Option<multicore_core::CachedServiceLogo> {
        self.shown
            .lock()
            .expect("service logo lock poisoned")
            .get(id)
            .cloned()
    }

    fn pending(&self, source_url: Option<&str>) -> bool {
        source_url.is_some_and(|source_url| {
            self.jobs
                .lock()
                .expect("service logo job lock poisoned")
                .iter()
                .any(|job| job.source_url == source_url && job.logo_url.is_some())
        })
    }

    /// The logo path to show for profile `id` whose generation carries `generation_logo`.
    fn path_for(
        &self,
        id: &ProfileId,
        generation_logo: Option<&std::path::Path>,
    ) -> Option<String> {
        match self.shown(id) {
            Some(multicore_core::CachedServiceLogo::Logo(lease)) => lease
                .path()
                .and_then(std::path::Path::to_str)
                .map(ToOwned::to_owned),
            Some(multicore_core::CachedServiceLogo::Removed) => None,
            None => generation_logo
                .and_then(std::path::Path::to_str)
                .map(ToOwned::to_owned),
        }
    }
}

/// A cached icon older than this is downloaded again in the background (the cached copy stays
/// on screen meanwhile), so a provider swapping the image behind a URL is picked up.
const GROUP_ICON_REFRESH_AFTER_SECS: u64 = 12 * 60 * 60;

impl GroupIconCache {
    fn new(icons: BTreeMap<String, multicore_core::CachedGroupIcon>) -> Self {
        Self {
            icons: std::sync::Mutex::new(icons),
            requests: Arc::default(),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, multicore_core::CachedGroupIcon>> {
        self.icons.lock().expect("group icon cache lock poisoned")
    }

    /// `snapshot` with the cached images of the icons its config names.
    fn fill(&self, snapshot: Arc<Snapshot>) -> Arc<Snapshot> {
        let images: BTreeMap<String, Vec<u8>> = {
            let icons = self.lock();
            snapshot
                .group_icon_urls()
                .into_iter()
                .filter_map(|(key, _)| icons.get(&key).map(|icon| (key, icon.png.clone())))
                .collect()
        };
        if images.is_empty() {
            snapshot
        } else {
            snapshot.with_group_icons(&images)
        }
    }

    fn needs_download(&self, key: &str, now: u64) -> bool {
        self.lock().get(key).is_none_or(|icon| {
            icon.fetched_at_unix
                .saturating_add(GROUP_ICON_REFRESH_AFTER_SECS)
                <= now
        })
    }

    /// Adds `images` (by key) downloaded at `fetched_at_unix`, or copies a snapshot carried
    /// when the cache has none (`fetched_at_unix` 0: downloaded again soon). Returns whether
    /// the cache changed.
    fn store(&self, images: BTreeMap<String, Vec<u8>>, fetched_at_unix: u64, keep: bool) -> bool {
        let mut icons = self.lock();
        let mut changed = false;
        for (key, png) in images {
            if keep && icons.contains_key(&key) {
                continue;
            }
            icons.insert(
                key,
                multicore_core::CachedGroupIcon {
                    png,
                    fetched_at_unix,
                },
            );
            changed = true;
        }
        changed
    }
}

impl<P: ProcessController> CoreRuntime<P> {
    /// The catalog of the installed snapshot, rebuilt in place (same revision).
    fn rebuild_catalog(&mut self) {
        if let Some(snapshot) = &self.snapshot {
            let revision = self.catalog.revision;
            self.catalog = build_catalog(
                snapshot,
                revision,
                self.profile.as_deref(),
                &self.selected_nodes,
                self.icons_pending(revision),
            );
        }
    }

    /// Icons of the installed snapshot are missing and their background download for this
    /// revision has not finished yet.
    fn icons_pending(&self, revision: u64) -> bool {
        self.icons_settled_revision != Some(revision)
            && self
                .snapshot
                .as_ref()
                .is_some_and(|snapshot| !snapshot.missing_group_icon_urls().is_empty())
    }

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
            connected_since_unix_ms: 0,
            events: EventBuffer::default(),
            warm_revision: None,
            readiness: None,
            group_icons: Arc::default(),
            service_logos: Arc::default(),
            icons_settled_revision: None,
            sessions: None,
            open_session_ms: None,
        }
    }

    /// A connection attempt begins: a new session in the index (ending one left open).
    fn begin_session(&mut self, tun: bool) {
        self.end_session("error");
        let Some(sessions) = self.sessions.clone() else {
            return;
        };
        let now = unix_timestamp_ms();
        let start = multicore_core::core_sessions::SessionStart {
            // Only Windows has system-proxy mode; elsewhere every connection is TUN.
            mode: if cfg!(windows) && !tun {
                "proxy"
            } else {
                "tun"
            }
            .to_owned(),
            service: self.service_active(),
            engines: vec!["xray".to_owned(), "mihomo".to_owned()],
            profile: self
                .active_profile_id
                .as_ref()
                .map(|id| multicore_core::core_sessions::profile_tag(id.as_str())),
        };
        sessions.start(now, &start);
        self.open_session_ms = Some(now);
    }

    /// The recorded connection ended (`user`, `switch`, `quit`, `cores_lost`, `failed`,
    /// `error`); nothing when none is open.
    fn end_session(&mut self, reason: &str) {
        let Some(connect_ms) = self.open_session_ms.take() else {
            return;
        };
        if let Some(sessions) = &self.sessions {
            sessions.end(connect_ms, unix_timestamp_ms(), reason);
        }
    }

    /// Always-ready mode is on and the service is usable right now.
    fn service_active(&self) -> bool {
        self.readiness
            .as_ref()
            .is_some_and(|readiness| readiness.service_available())
    }

    fn cores_live(&self) -> bool {
        if self.degraded {
            return false;
        }
        match self.state.current() {
            CoreConnectionState::Connected => true,
            CoreConnectionState::Ready => {
                self.warm_revision == Some(self.catalog.revision)
                    && self
                        .readiness
                        .as_ref()
                        .is_some_and(|readiness| readiness.cores_ready())
            }
            _ => false,
        }
    }

    fn with_events_log(path: &std::path::Path) -> Self {
        Self {
            events: EventBuffer::persistent_or_memory(path),
            ..Self::empty()
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
            message: self.message.as_ref().map(|message| message.text.clone()),
            message_key: self
                .message
                .as_ref()
                .and_then(|message| message.key.clone()),
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
                            service_logo_path: match &self.active_profile_id {
                                Some(id) => self
                                    .service_logos
                                    .path_for(id, snapshot.service_logo_path()),
                                None => snapshot
                                    .service_logo_path()
                                    .and_then(std::path::Path::to_str)
                                    .map(ToOwned::to_owned),
                            },
                            service_logo_pending: self
                                .service_logos
                                .pending(snapshot.subscription_source_url()),
                            refresh_available: snapshot.subscription_source_url().is_some(),
                            dumb_mode: info.dumb_mode,
                            app_language: info
                                .app_language
                                .map(|language| language.code().to_owned()),
                        })
                }),
            cores_ready: self.cores_live(),
            service_mode: self.service_active(),
            imported_profile_id: None,
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

    fn apply_snapshot_with_optional_controller(
        &mut self,
        id: ProfileId,
        snapshot: Arc<Snapshot>,
        controller: Option<Arc<P>>,
        revision: u64,
        selected_nodes: Vec<Option<String>>,
        explicit_selected_nodes: Vec<Option<String>>,
    ) {
        match controller {
            Some(controller) => self.apply_snapshot(
                id,
                snapshot,
                controller,
                revision,
                selected_nodes,
                explicit_selected_nodes,
            ),
            None => self.apply_snapshot_without_controller(
                id,
                snapshot,
                revision,
                selected_nodes,
                explicit_selected_nodes,
            ),
        }
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
        let primary = primary_group_index(&selectable);
        self.profile = primary.map(|index| selectable[index].name.to_owned());
        self.selected_nodes = selected_nodes;
        self.explicit_selected_nodes = explicit_selected_nodes;
        self.current_node = primary
            .and_then(|index| self.selected_nodes.get(index).cloned())
            .flatten();
        let names_icons = !snapshot.group_icon_urls().is_empty();
        self.snapshot = Some(self.group_icons.fill(snapshot));
        self.catalog.revision = revision;
        self.rebuild_catalog();
        if names_icons && self.icons_settled_revision != Some(revision) {
            self.group_icons.requests.notify_one();
        }
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
        self.warm_revision = None;
    }

    fn mark_error(&mut self, message: impl Into<DaemonText>, degraded: bool) {
        self.end_session("error");
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
        self.message = Some(message.into());
        self.degraded = degraded;
    }

    fn push_event(
        &mut self,
        severity: Severity,
        component: Component,
        phase: &str,
        message: impl Into<DaemonText>,
    ) {
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
        message: impl Into<DaemonText>,
        details: serde_json::Value,
    ) {
        let message = message.into();
        self.events.push_event_with(|id| {
            let event = Event::new(
                unix_timestamp_ms(),
                severity,
                component,
                phase,
                format!("daemon-{id:016x}"),
                &message.text,
                details,
            );
            match message.key {
                Some(key) => event.with_message_key(key),
                None => event,
            }
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
        let controller_factory: Arc<PreparedFactory<P>> =
            Arc::new(move |snapshot: &Snapshot, _options: ConnectOptions| {
                controller_factory(snapshot).map(PreparedController::plain)
            });
        Self::from_prepared_factory(store, fetcher, controller_factory, selector, None)
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
        Self::new_transactional_with_selector_and_events_log(
            store,
            fetcher,
            controller_factory,
            selector,
            None,
        )
    }

    /// Same as `new_transactional_with_selector`, but persists the daemon's
    /// event log to `events_log_path` (when given) so a real failure's
    /// diagnostics survive a daemon restart between the failure and a user
    /// exporting the log. Falls back to memory-only if the file can't be
    /// used. Production (`multicore-daemon`'s `main.rs`) always passes
    /// `Some(...)`; tests generally don't need this and can keep calling
    /// `new_transactional_with_selector`.
    pub fn new_transactional_with_selector_and_events_log(
        store: ProfileStore,
        fetcher: SubscriptionFetcher<C>,
        controller_factory: TransactionalControllerFactory<F>,
        selector: Arc<dyn MihomoSelector>,
        events_log_path: Option<std::path::PathBuf>,
    ) -> Result<Self, BackendError> {
        let factory = controller_factory.0;
        let controller_factory: Arc<PreparedFactory<P>> =
            Arc::new(move |snapshot: &Snapshot, _options: ConnectOptions| factory(snapshot));
        Self::from_prepared_factory(
            store,
            fetcher,
            controller_factory,
            selector,
            events_log_path,
        )
    }
}

impl<C, P, F> CoreBackend<C, P, OptionsControllerFactory<F>>
where
    C: HttpClient,
    P: ProcessController,
    F: Fn(&Snapshot, ConnectOptions) -> Result<PreparedController<P>, BackendError>
        + Send
        + Sync
        + 'static,
{
    /// Like `new_transactional_with_selector_and_events_log`, for a factory that honours
    /// [`ConnectOptions`] (production: TUN or system-proxy mode).
    pub fn new_with_connect_options(
        store: ProfileStore,
        fetcher: SubscriptionFetcher<C>,
        controller_factory: OptionsControllerFactory<F>,
        selector: Arc<dyn MihomoSelector>,
        events_log_path: Option<std::path::PathBuf>,
    ) -> Result<Self, BackendError> {
        let controller_factory: Arc<PreparedFactory<P>> = Arc::new(controller_factory.0);
        Self::from_prepared_factory(
            store,
            fetcher,
            controller_factory,
            selector,
            events_log_path,
        )
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
        events_log_path: Option<std::path::PathBuf>,
    ) -> Result<Self, BackendError> {
        let mut inner = match &events_log_path {
            Some(path) => CoreRuntime::with_events_log(path),
            None => CoreRuntime::empty(),
        };
        let mut profile_logo_leases = HashMap::new();
        inner.group_icons = Arc::new(GroupIconCache::new(run_blocking(|| {
            store.group_icon_cache()
        })));
        let service_logos = Arc::new(ServiceLogos::default());
        if let Ok(profiles) = run_blocking(|| store.list()) {
            let mut shown = service_logos
                .shown
                .lock()
                .expect("service logo lock poisoned");
            for profile in profiles {
                if let Some(logo) = store.cached_service_logo(&profile.id) {
                    shown.insert(profile.id, logo);
                }
            }
        }
        inner.service_logos = service_logos.clone();
        let group_icons_requests = inner.group_icons.requests.clone();
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
                run_blocking(|| initial_and_explicit_selections(&store, &id, revision, &snapshot));
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
            priority_latency_permits: Arc::new(tokio::sync::Semaphore::new(
                MAX_PRIORITY_LATENCY_PROBES,
            )),
            latency_flights: Arc::default(),
            exit_now_cache: std::sync::Mutex::new(None),
            operation: tokio::sync::Mutex::new(()),
            generation_change: tokio::sync::Mutex::new(()),
            connect_attempts: AtomicU64::new(0),
            connect_options: std::sync::Mutex::new(ConnectOptions::default()),
            post_commit_load_fault: AtomicBool::new(false),
            warm_failure_reported: AtomicBool::new(false),
            warm_signal: Arc::new(tokio::sync::Notify::new()),
            shut_down: AtomicBool::new(false),
            list_logo_publish_hook: std::sync::Mutex::new(None),
            profile_logo_leases: std::sync::Mutex::new(profile_logo_leases),
            service_logos,
            group_icons_requests,
            inner: RwLock::new(inner),
        })
    }

    /// Signalled whenever group icons of the installed snapshot should be downloaded; the
    /// daemon's icon task waits on it and runs `fetch_group_icons`.
    pub async fn group_icon_requests(&self) -> Arc<tokio::sync::Notify> {
        self.inner.read().await.group_icons.requests.clone()
    }

    /// Downloads the group icons the installed snapshot names but the cache lacks (or holds
    /// an old copy of), caches and persists them, and attaches them to the catalog. Runs in
    /// the background: imports, refreshes and switches never wait for icon hosts. Returns
    /// whether the catalog changed.
    pub async fn fetch_group_icons(&self) -> bool {
        let (snapshot, revision, cache) = {
            let state = self.inner.read().await;
            let Some(snapshot) = state.snapshot.clone() else {
                return false;
            };
            (snapshot, state.catalog.revision, state.group_icons.clone())
        };
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_secs());
        // Icons a 1.1.2 snapshot carried seed the cache (and are re-checked soon).
        let mut changed = cache.store(snapshot.group_icon_pngs(), 0, true);
        let wanted: Vec<(String, String)> = snapshot
            .group_icon_urls()
            .into_iter()
            .filter(|(key, _)| cache.needs_download(key, now))
            .collect();
        let downloaded = self.fetcher.download_group_icons(&wanted).await;
        changed |= cache.store(downloaded, now, false);
        if changed {
            let icons = cache.lock().clone();
            if run_blocking(|| self.store.save_group_icon_cache(&icons)).is_err() {
                self.inner.write().await.push_event(
                    Severity::Warning,
                    Component::Subscription,
                    "group_icon_cache",
                    "Group icons could not be saved; they are downloaded again next start.",
                );
            }
        }
        let mut state = self.inner.write().await;
        // A newer snapshot was installed meanwhile: it queued its own pass.
        if state.catalog.revision != revision {
            return false;
        }
        let Some(current) = state.snapshot.clone() else {
            return false;
        };
        state.snapshot = Some(cache.fill(current));
        let settled_now = state.icons_settled_revision != Some(revision);
        state.icons_settled_revision = Some(revision);
        let before = state.catalog.clone();
        state.rebuild_catalog();
        settled_now || state.catalog != before
    }

    /// Downloads the provider logos of subscriptions fetched since the last call and keeps
    /// them as their profiles' logos (a logo the provider dropped is removed). Returns whether
    /// a job must be retried shortly (its import was still being stored).
    pub async fn fetch_service_logos(&self) -> bool {
        let jobs = std::mem::take(
            &mut *self
                .service_logos
                .jobs
                .lock()
                .expect("service logo job lock poisoned"),
        );
        let mut retry = Vec::new();
        for job in jobs {
            let download = match &job.logo_url {
                Some(url) => Some(self.fetcher.download_service_logo(url).await),
                None => None,
            };
            let id = match run_blocking(|| self.store.find_exact_source(&job.source_url)) {
                Ok(Some(id)) => id,
                _ => {
                    if job.attempts + 1 < SERVICE_LOGO_JOB_ATTEMPTS {
                        retry.push(ServiceLogoJob {
                            attempts: job.attempts + 1,
                            ..job
                        });
                    }
                    continue;
                }
            };
            // Release the lease of the logo shown so far first: on Windows a held lease keeps
            // its file from being replaced (the store would wait for it).
            let previous = matches!(
                download,
                Some(ServiceLogoDownload::Logo(_) | ServiceLogoDownload::Gone) | None
            )
            .then(|| {
                self.service_logos
                    .shown
                    .lock()
                    .expect("service logo lock poisoned")
                    .remove(&id)
            })
            .flatten();
            let stored = match download {
                Some(ServiceLogoDownload::Logo(png)) => run_blocking(|| {
                    self.store
                        .save_service_logo(&id, &png)
                        .map(multicore_core::CachedServiceLogo::Logo)
                }),
                Some(ServiceLogoDownload::Gone) | None => run_blocking(|| {
                    self.store
                        .mark_service_logo_removed(&id)
                        .map(|()| multicore_core::CachedServiceLogo::Removed)
                }),
                // Unreachable this time: the logo shown stays.
                Some(ServiceLogoDownload::Failed) => continue,
            };
            // A failed write keeps showing the previous logo.
            if let Some(logo) = stored.ok().or(previous) {
                self.service_logos
                    .shown
                    .lock()
                    .expect("service logo lock poisoned")
                    .insert(id, logo);
            }
        }
        let mut jobs = self
            .service_logos
            .jobs
            .lock()
            .expect("service logo job lock poisoned");
        // A newer fetch of the same subscription queued its own job meanwhile.
        retry.retain(|job| {
            jobs.iter()
                .all(|queued| queued.source_url != job.source_url)
        });
        let retry_later = !retry.is_empty();
        jobs.extend(retry);
        retry_later
    }

    /// Always-ready mode: from now on the cores are kept running while Ready (whenever the
    /// readiness source reports the service as available).
    pub async fn enable_always_ready(&self, readiness: Arc<dyn CoresReadiness>) {
        self.inner.write().await.readiness = Some(readiness);
        self.request_warm();
    }

    /// Records every connection attempt in the per-connection log index in `directory`
    /// (`multicore_core::core_sessions`), whose journal this daemon also prunes.
    pub async fn enable_session_log(&self, directory: std::path::PathBuf) {
        let index = tokio::task::spawn_blocking(move || {
            multicore_core::core_sessions::SessionIndex::open(directory, unix_timestamp_ms())
        })
        .await
        .ok();
        self.inner.write().await.sessions = index.map(Arc::new);
    }

    /// Signalled whenever the cores should be (re-)warmed: after a snapshot change, a
    /// disconnect or a failed connect. The service watcher waits on it; request handlers never
    /// run the warm-up themselves.
    #[must_use]
    pub fn warm_requests(&self) -> Arc<tokio::sync::Notify> {
        self.warm_signal.clone()
    }

    fn request_warm(&self) {
        self.warm_signal.notify_one();
    }

    /// Starts the idle cores of the current snapshot (always-ready mode only; a no-op
    /// otherwise, while connected, without a profile, or when they already run for this
    /// catalog). Runs without the operation lock, so selections, disconnects and mutations
    /// never wait for a `Prepare`; a snapshot changed meanwhile is simply not marked warm.
    pub async fn warm_up(&self) -> Result<(), BackendError> {
        if self.shut_down.load(Ordering::SeqCst) {
            return Ok(());
        }
        let (snapshot, revision) = {
            let state = self.inner.read().await;
            if !state.service_active()
                || state.degraded
                || state.state.current() != CoreConnectionState::Ready
            {
                return Ok(());
            }
            let Some(snapshot) = state.snapshot.clone() else {
                return Ok(());
            };
            if state.cores_live() {
                return Ok(());
            }
            (snapshot, state.catalog.revision)
        };
        let attempts = self.connect_attempts.load(Ordering::SeqCst);
        let mut options = self
            .connect_options
            .lock()
            .map(|options| *options)
            .unwrap_or_default();
        options.warm_up = true;
        let result = match run_blocking(|| (self.controller_factory)(&snapshot, options)) {
            Ok(prepared) => {
                let controller = prepared.controller();
                match prepared.activate() {
                    // A Connect (and maybe its Disconnect) ran while the factory probed the
                    // service: this generation is stale, so do not Prepare it over what the
                    // connection left; warm again from the current state instead.
                    Ok(()) if self.warm_up_superseded(attempts, revision).await => {
                        self.request_warm();
                        return Ok(());
                    }
                    Ok(()) => controller
                        .start(Engine::Xray)
                        .await
                        .map_err(|_| BackendError::new()),
                    Err(error) => Err(error),
                }
            }
            Err(error) => Err(error),
        };
        if result.is_err() && self.connect_attempts.load(Ordering::SeqCst) != attempts {
            // Refused (`InUse`) because a Connect took the session meanwhile, even if that
            // Connect has already failed back to Ready: not a warm-up failure. The Connect
            // requested a fresh warm-up itself.
            return Ok(());
        }
        let mut state = self.inner.write().await;
        match result {
            Ok(()) => {
                if state.catalog.revision == revision {
                    state.warm_revision = Some(revision);
                }
                self.warm_failure_reported.store(false, Ordering::SeqCst);
                Ok(())
            }
            Err(error) => {
                if state.state.current() != CoreConnectionState::Ready {
                    // A Connect took the session meanwhile (the warm-up was refused): not a
                    // warm-up failure.
                    return Err(error);
                }
                if state.warm_revision == Some(revision) {
                    state.warm_revision = None;
                }
                if !self.warm_failure_reported.swap(true, Ordering::SeqCst) {
                    state.push_event(
                        Severity::Warning,
                        Component::Core,
                        "warm_up",
                        DaemonText::code(WARM_UP_FAILED_MESSAGE),
                    );
                }
                Err(error)
            }
        }
    }

    async fn warm_up_superseded(&self, attempts: u64, revision: u64) -> bool {
        if self.connect_attempts.load(Ordering::SeqCst) != attempts {
            return true;
        }
        let state = self.inner.read().await;
        state.state.current() != CoreConnectionState::Ready || state.catalog.revision != revision
    }

    /// One tick of the always-ready watcher, after the readiness source was refreshed:
    /// - Connected through the service and the service lost it: tear the connection down
    ///   (restoring the system proxy) and report it;
    /// - Ready: warm the cores;
    /// - no subscription left: let the service stop the cores.
    ///
    /// Returns false when something is wrong, so the caller can back off.
    pub async fn watch_service(&self) -> bool {
        if self.shut_down.load(Ordering::SeqCst) {
            return true;
        }
        let (current, readiness) = {
            let state = self.inner.read().await;
            let Some(readiness) = state.readiness.clone() else {
                return true;
            };
            (state.state.current(), readiness)
        };
        match current {
            CoreConnectionState::Connected if readiness.connection_lost() => {
                self.cores_lost().await;
                false
            }
            CoreConnectionState::Ready => {
                readiness.service_available() && self.warm_up().await.is_ok()
            }
            CoreConnectionState::Empty if readiness.cores_ready() => {
                let _ = tokio::task::spawn_blocking(move || readiness.release()).await;
                true
            }
            _ => true,
        }
    }

    /// The service lost the cores (or the tunnel) under an active connection: stop it the
    /// usual way, so proxy mode restores the system proxy at once, and report the loss.
    async fn cores_lost(&self) {
        let _operation = self.operation.lock().await;
        let controller = {
            let state = self.inner.read().await;
            let lost = state.state.current() == CoreConnectionState::Connected
                && state
                    .readiness
                    .as_ref()
                    .is_some_and(|readiness| readiness.connection_lost());
            match (lost, state.controller.clone()) {
                (true, Some(controller)) => controller,
                _ => return,
            }
        };
        let result = Supervisor::new(controller).disconnect().await;
        let mut state = self.inner.write().await;
        let stopped = result.is_ok();
        if stopped {
            state.end_session("cores_lost");
            let _ = state.state.transition(CoreConnectionState::Ready);
            state.degraded = false;
            state.warm_revision = None;
            drop(state);
            // Like a disconnect: drop the controller and apply a snapshot left pending.
            self.reconcile_authoritative().await;
            state = self.inner.write().await;
            state.message = Some(DaemonText::code(CORES_LOST_MESSAGE));
        } else {
            state.mark_error(DaemonText::code("disconnect.incomplete"), true);
        }
        state.push_event(
            Severity::Error,
            Component::Core,
            "service_lost",
            DaemonText::code(CORES_LOST_MESSAGE),
        );
        drop(state);
        self.request_warm();
    }

    /// Quit: disconnect, stop the service's cores and never warm them again.
    pub async fn shutdown_cores(&self) {
        self.shut_down.store(true, Ordering::SeqCst);
        let _ = self.disconnect_now("quit").await;
        let readiness = self.inner.read().await.readiness.clone();
        if let Some(readiness) = readiness {
            let _ = tokio::task::spawn_blocking(move || readiness.release()).await;
        }
    }

    /// A one-time user-facing notice (for example the fallback to on-demand elevation).
    pub async fn record_notice(&self, phase: &str, message: DaemonText) {
        let mut state = self.inner.write().await;
        state.push_event(Severity::Warning, Component::Core, phase, message.clone());
        if state.message.is_none() {
            state.message = Some(message);
        }
    }

    /// A connection is up (the watcher keeps its short interval then).
    pub async fn is_connected(&self) -> bool {
        self.inner.read().await.state.current() == CoreConnectionState::Connected
    }

    /// Clears a notice from `record_notice` once it no longer applies (for example the
    /// service became available).
    pub async fn clear_notice(&self, message: &DaemonText) {
        let mut state = self.inner.write().await;
        if state.message.as_ref() == Some(message) {
            state.message = None;
        }
    }

    async fn rewarm_after<T>(&self, result: Result<T, BackendError>) -> Result<T, BackendError> {
        if result.is_ok() {
            self.request_warm();
        }
        result
    }

    /// Reconnects after `pause_for_generation_change` (never after Quit).
    async fn resume_after_generation_change(&self, options: ConnectOptions) {
        if !self.shut_down.load(Ordering::SeqCst) {
            let _ = self.connect_now(options).await;
        }
    }

    async fn connect_now(&self, options: ConnectOptions) -> Result<StatusDto, BackendError> {
        let _operation = self.operation.lock().await;
        self.connect_attempts.fetch_add(1, Ordering::SeqCst);
        let started = std::time::Instant::now();
        if let Ok(mut current) = self.connect_options.lock() {
            *current = options;
        }
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
        let prepared = match run_blocking(|| (self.controller_factory)(&snapshot, options)) {
            Ok(prepared) => prepared,
            Err(error) => {
                let mut state = self.inner.write().await;
                state.message = Some(DaemonText::code("connect.prepare_failed"));
                state.push_event(
                    Severity::Error,
                    Component::Core,
                    "runtime_publish",
                    DaemonText::code("connect.prepare_failed_event"),
                );
                return Err(error);
            }
        };
        let controller = prepared.controller();
        let restage_reason = prepared.restage_reason.clone();
        let prepared_ms = started.elapsed().as_millis();
        if let Some(reason) = &restage_reason {
            // The warm generation was not reused, so this Connect staged and prepared again
            // (restarting the cores): the reason names which staged input differed.
            let mut state = self.inner.write().await;
            state.push_event_with_details(
                Severity::Debug,
                Component::Core,
                "connect",
                DaemonText::code("connect.restaged"),
                json!({ "reason": reason, "prepare_ms": prepared_ms }),
            );
        }
        if prepared.activate().is_err() {
            let mut state = self.inner.write().await;
            state.message = Some(DaemonText::code("connect.activate_failed"));
            state.push_event(
                Severity::Error,
                Component::Core,
                "runtime_publish",
                DaemonText::code("connect.activate_failed_event"),
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
            state.begin_session(options.tun);
            state.message = Some(DaemonText::code(if state.service_active() {
                TUNNEL_ON_MESSAGE
            } else {
                CONNECTING_MESSAGE
            }));
            state.push_event(
                Severity::Info,
                Component::Core,
                "connect",
                DaemonText::code("connect.starting"),
            );
        }
        // If this future is dropped before the outcome is recorded, leave a cleanable error.
        let mut abandoned = AbandonedConnectGuard::new(&self.inner);

        let result = Supervisor::new(controller.clone()).connect().await;
        let started_ms = started.elapsed().as_millis();
        if result.is_ok() {
            for (group, node) in queued_selections {
                if self.selector.select(&group, &node).await.is_err() {
                    let rollback = Supervisor::new(controller.clone()).disconnect().await;
                    let mut state = self.inner.write().await;
                    abandoned.disarm();
                    state.end_session("failed");
                    if rollback.is_ok() {
                        state
                            .state
                            .transition(CoreConnectionState::Ready)
                            .map_err(|_| BackendError::new())?;
                        state.message = Some(DaemonText::from(
                            "Mihomo did not apply the queued route selection; connection was rolled back.",
                        ));
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
        let core_log_tail = match &result {
            Err(multicore_core::SupervisorError::StartFailed { engine, .. }) => {
                failed_core_log_tail(controller.as_ref(), *engine).await
            }
            _ => Vec::new(),
        };
        let mut state = self.inner.write().await;
        abandoned.disarm();
        match result {
            Ok(()) => {
                state
                    .state
                    .transition(CoreConnectionState::Connected)
                    .map_err(|_| BackendError::new())?;
                state.connected_since_unix_ms = unix_timestamp_ms();
                state.message = None;
                state.degraded = false;
                // Where a slow Connect spends its time: the factory (staging or reusing the
                // warm generation), the cores' start (the service's tunnel switch) and the
                // selection replay.
                state.push_event_with_details(
                    Severity::Info,
                    Component::Core,
                    "connect",
                    DaemonText::code("connect.established"),
                    json!({
                        "prepare_ms": prepared_ms,
                        "restaged": restage_reason.is_some(),
                        "start_ms": started_ms.saturating_sub(prepared_ms),
                        "replay_ms": started.elapsed().as_millis().saturating_sub(started_ms),
                        "total_ms": started.elapsed().as_millis(),
                    }),
                );
                // The watcher checks the new connection right away and keeps its short interval.
                self.request_warm();
                Ok(state.status())
            }
            Err(multicore_core::SupervisorError::StartFailed {
                engine,
                rollback_failed,
                cause,
            }) => {
                state.end_session("failed");
                self.request_warm();
                if rollback_failed {
                    state.mark_error(DaemonText::code("connect.rollback_incomplete"), true);
                } else {
                    state
                        .state
                        .transition(CoreConnectionState::Ready)
                        .map_err(|_| BackendError::new())?;
                    state.message = Some(start_failure_message(engine, cause));
                    state.degraded = false;
                }
                // The cause used to reach only the status line, so a copied
                // log said "connect failed" with nothing to act on.
                let shown = start_failure_message(engine, cause).text;
                state.push_event_with_details(
                    Severity::Error,
                    Component::Core,
                    "connect",
                    DaemonText::code("connect.failed"),
                    json!({
                        "engine": format!("{engine:?}"),
                        "cause": format!("{cause:?}"),
                        "rollback_failed": rollback_failed,
                        "shown": shown,
                        "os": std::env::consts::OS,
                        // What the failed core itself printed last: usually the actual reason.
                        "core_log_tail": core_log_tail,
                    }),
                );
                Err(BackendError::from_start_failure(engine, cause))
            }
            Err(multicore_core::SupervisorError::StopFailed) => {
                state.end_session("failed");
                state.mark_error(DaemonText::code("connect.processes_unknown"), true);
                state.push_event_with_details(
                    Severity::Error,
                    Component::Core,
                    "connect",
                    DaemonText::code("connect.failed"),
                    json!({ "cause": "stop_failed_during_start", "os": std::env::consts::OS }),
                );
                Err(BackendError::new())
            }
        }
    }

    /// Spec §6: in always-ready mode a subscription switch or refresh while connected moves the
    /// connection to the new generation. Stops the connection when `condition` holds and
    /// returns the options to reconnect with. The caller holds `generation_change` across the
    /// pause, the switch and the reconnect, so a user Connect or Disconnect waits for the whole
    /// sequence instead of slipping in between.
    async fn pause_for_generation_change(
        &self,
        condition: impl FnOnce(&CoreRuntime<P>) -> bool,
    ) -> Option<ConnectOptions> {
        {
            let state = self.inner.read().await;
            if state.state.current() != CoreConnectionState::Connected
                || state.degraded
                || !state.service_active()
                || !condition(&state)
            {
                return None;
            }
        }
        self.disconnect_now("switch").await.ok()?;
        Some(
            self.connect_options
                .lock()
                .map(|options| *options)
                .unwrap_or_default(),
        )
    }

    /// Stops the connection; `reason` is recorded in the per-connection log index.
    async fn disconnect_now(&self, reason: &str) -> Result<StatusDto, BackendError> {
        let _operation = self.operation.lock().await;
        let started = std::time::Instant::now();
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
                state.end_session(reason);
                state
                    .state
                    .transition(CoreConnectionState::Ready)
                    .map_err(|_| BackendError::new())?;
                state.message = None;
                state.degraded = false;
                let stopped_ms = started.elapsed().as_millis();
                drop(state);
                self.reconcile_authoritative().await;
                self.inner.write().await.push_event_with_details(
                    Severity::Info,
                    Component::Core,
                    "disconnect",
                    DaemonText::code("disconnect.done"),
                    json!({
                        "stop_ms": stopped_ms,
                        "reconcile_ms": started.elapsed().as_millis().saturating_sub(stopped_ms),
                        "total_ms": started.elapsed().as_millis(),
                    }),
                );
                Ok(self.current_status().await)
            }
            Err(_) => {
                state.mark_error(DaemonText::code("disconnect.incomplete"), true);
                state.push_event(
                    Severity::Error,
                    Component::Core,
                    "disconnect",
                    DaemonText::code("disconnect.failed_event"),
                );
                Err(BackendError::new())
            }
        }
    }

    pub fn list_subscriptions(&self) -> Result<Vec<ProfileSummary>, ProfileMutationError> {
        run_blocking(|| self.store.list()).map_err(map_profile_error)
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
        let profiles = run_blocking(|| {
            self.store.map_loaded(|loaded| {
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
                let generation_logo = loaded.snapshot.as_ref().and_then(|snapshot| {
                    loaded.physical_generation.and_then(|physical_generation| {
                        self.wait_for_list_logo_publish_hook();
                        self.retain_profile_logo(&loaded.summary.id, physical_generation, snapshot)
                    })
                });
                let service_logo_path = self.service_logos.path_for(
                    &loaded.summary.id,
                    generation_logo.as_deref().map(std::path::Path::new),
                );
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
        self.activate_subscription_timed(id, &mut ActivationTimings::default())
            .await
    }

    /// `activate_subscription`, recording where the time went (see `subscription_switch`).
    async fn activate_subscription_timed(
        &self,
        id: &ProfileId,
        timings: &mut ActivationTimings,
    ) -> Result<StatusDto, ProfileMutationError> {
        self.require_disconnected_mutation().await?;
        let _operation = self.operation.lock().await;
        self.require_disconnected_mutation().await?;
        let started = std::time::Instant::now();
        let (mut revision, physical_generation, mut snapshot) =
            match run_blocking(|| self.store.profile_snapshot_generation(id)) {
                Ok(loaded) => loaded,
                Err(error) => {
                    return Err(self
                        .map_profile_error_logged("activate_load_failed", error)
                        .await);
                }
            };
        self.retain_profile_logo(id, physical_generation, &snapshot);
        timings.load_ms = started.elapsed().as_millis();
        let prepared = self
            .prepare_controller_or_defer("activate_prepare_deferred", &snapshot)
            .await;
        timings.prepare_ms = started
            .elapsed()
            .as_millis()
            .saturating_sub(timings.load_ms);
        let maintenance_debt = match run_blocking(|| self.store.activate(id)) {
            Ok(_) => false,
            Err(error) if error.mutation_committed() => {
                let (active_id, active_revision, _, active_snapshot) =
                    match run_blocking(|| self.store.active_snapshot_generation()) {
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
            Err(error) => {
                return Err(self
                    .map_profile_error_logged("activate_failed", error)
                    .await);
            }
        };
        let (selections, explicit_selections) =
            run_blocking(|| initial_and_explicit_selections(&self.store, id, revision, &snapshot));
        let controller = prepared.as_ref().map(PreparedController::controller);
        {
            let mut state = self.inner.write().await;
            state.clear_for_active(Some(id.clone()));
            state.apply_snapshot_with_optional_controller(
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
        if let Some(prepared) = prepared
            && prepared.activate().is_err()
        {
            self.reconcile_authoritative().await;
            return Err(ProfileMutationError::Storage);
        }
        timings.commit_ms = started
            .elapsed()
            .as_millis()
            .saturating_sub(timings.load_ms + timings.prepare_ms);
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
        let prefetched = self.prefetch_profile(id).await?;
        let _operation = self.operation.lock().await;
        self.require_refresh_delete_mutation().await?;
        self.refresh_profile_locked(id, Some(prefetched)).await
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
        self.replace_profile_source_locked(id, &request.url, None)
            .await
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
        // Release the background logo's lease before its file is deleted.
        self.service_logos
            .shown
            .lock()
            .expect("service logo lock poisoned")
            .remove(id);
        let maintenance_debt = match run_blocking(|| self.store.delete(id)) {
            Ok(_) => false,
            Err(error) if error.mutation_committed() => true,
            Err(error) => {
                let mapped = self.map_profile_error_logged("delete_failed", error).await;
                return Err(if stopped_connection {
                    ProfileMutationError::Committed
                } else {
                    mapped
                });
            }
        };
        self.remove_profile_logo(id);
        if was_active {
            self.reconcile_authoritative().await;
        }
        if maintenance_debt {
            let _ = run_blocking(|| self.store.cleanup_unindexed_profile(id));
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
            self.fetcher.refresh_deferring_logo(url, staging),
        )
        .await
        {
            Ok((result, diagnostics)) => (result, diagnostics),
            Err(_) => (Err(FetchError::Timeout), deadline_diagnostics),
        };
        if let Ok(commit) = &result {
            // The provider logo is downloaded in the background once the subscription is
            // stored (`fetch_service_logos`); the previous one stays on screen meanwhile.
            let logo_url = commit
                .committed()
                .subscription_logo_url()
                .map(ToOwned::to_owned);
            let logos = self.service_logos.clone();
            let mut jobs = logos.jobs.lock().expect("service logo job lock poisoned");
            jobs.retain(|job| job.source_url != url.trim());
            jobs.push(ServiceLogoJob {
                source_url: url.trim().to_owned(),
                logo_url,
                attempts: 0,
            });
            drop(jobs);
            self.group_icons_requests.notify_one();
        }
        match result {
            Ok(commit) => Ok(commit),
            Err(error) => {
                let reason = subscription_fetch_reason(error);
                self.inner.write().await.push_event_with_details(
                    Severity::Warning,
                    Component::Subscription,
                    "subscription_fetch_failed",
                    format!("Subscription download failed: {reason}.").as_str(),
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
                state.end_session("switch");
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
        let loaded = run_blocking(|| self.store.active_snapshot_generation())
            .ok()
            .flatten();
        match (&authoritative_id, &loaded) {
            (_, Some((id, _, physical_generation, snapshot))) => {
                self.retain_profile_logo(id, *physical_generation, snapshot);
            }
            (Some(id), None) => self.remove_profile_logo(id),
            (None, None) => {}
        }
        let mut state = self.inner.write().await;
        // The service keeps the cores of an unchanged catalog running across a disconnect.
        let warm = state.warm_revision;
        let warm_profile = state.active_profile_id.clone();
        state.clear_for_active(authoritative_id);
        if let Some((id, revision, _, snapshot)) = loaded {
            let (selections, explicit_selections) = run_blocking(|| {
                initial_and_explicit_selections(&self.store, &id, revision, &snapshot)
            });
            state.apply_snapshot_without_controller(
                id,
                snapshot,
                revision,
                selections,
                explicit_selections,
            );
            if warm.is_some()
                && warm == Some(state.catalog.revision)
                && warm_profile == state.active_profile_id
            {
                state.warm_revision = warm;
            }
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
        match run_blocking(|| self.store.profile_snapshot_generation(id)) {
            Ok(loaded) => Ok(loaded),
            Err(_) => {
                self.remove_profile_logo(id);
                self.reconcile_authoritative().await;
                Err(ProfileMutationError::Committed)
            }
        }
    }

    /// The source link of profile `id`.
    async fn profile_source_url(&self, id: &ProfileId) -> Result<String, ProfileMutationError> {
        let (_, previous) = match run_blocking(|| self.store.profile_snapshot(id)) {
            Ok(loaded) => loaded,
            Err(error) => {
                return Err(self
                    .map_profile_error_logged("refresh_load_failed", error)
                    .await);
            }
        };
        previous
            .subscription_source_url()
            .map(ToOwned::to_owned)
            .ok_or(ProfileMutationError::Storage)
    }

    /// Downloads, parses and sanitizes profile `id`'s subscription without the operation
    /// lock: route selections and other requests stay served while the provider answers.
    /// The lock is taken only to commit (`refresh_profile_locked`).
    async fn prefetch_profile(&self, id: &ProfileId) -> Result<Prefetched, ProfileMutationError> {
        let source_url = self.profile_source_url(id).await?;
        let staging = AtomicSnapshot::default();
        let (snapshot, _) = self
            .fetch_subscription(&source_url, &staging)
            .await?
            .into_parts();
        Ok(Prefetched {
            source_url,
            snapshot,
        })
    }

    /// Refreshes profile `id` under the operation lock. A `prefetched` download is committed
    /// when it still belongs to the profile's current link; otherwise (the link was replaced
    /// meanwhile) the subscription is downloaded again here. Everything else (the previous
    /// generation, saved selections, the connection state) is read now, at commit time, so
    /// a selection made during the download is carried over.
    async fn refresh_profile_locked(
        &self,
        id: &ProfileId,
        prefetched: Option<Prefetched>,
    ) -> Result<StatusDto, ProfileMutationError>
    where
        C: 'static,
    {
        let source_url = self.profile_source_url(id).await?;
        let prefetched = prefetched
            .filter(|prefetched| prefetched.source_url == source_url)
            .map(|prefetched| prefetched.snapshot);
        self.replace_profile_source_locked(id, &source_url, prefetched)
            .await
    }

    async fn replace_profile_source_locked(
        &self,
        id: &ProfileId,
        source_url: &str,
        prefetched: Option<Arc<Snapshot>>,
    ) -> Result<StatusDto, ProfileMutationError>
    where
        C: 'static,
    {
        let source_url = source_url.trim();
        if source_url.is_empty() {
            return Err(ProfileMutationError::Storage);
        }
        let (previous_revision, previous) = match run_blocking(|| self.store.profile_snapshot(id)) {
            Ok(loaded) => loaded,
            Err(error) => {
                return Err(self
                    .map_profile_error_logged("replace_load_failed", error)
                    .await);
            }
        };
        let active = self.store.active_id().as_ref() == Some(id);
        let connected =
            active && self.inner.read().await.state.current() == CoreConnectionState::Connected;
        let snapshot = match prefetched {
            Some(snapshot) => snapshot,
            None => {
                let staging = AtomicSnapshot::default();
                let (snapshot, _) = self
                    .fetch_subscription(source_url, &staging)
                    .await?
                    .into_parts();
                snapshot
            }
        };
        // A different source is a different provider; its logo is not ours to keep.
        let snapshot = if previous.subscription_source_url() == Some(source_url) {
            snapshot.with_service_logo_carried_from(&previous)
        } else {
            snapshot
        };
        let prepared = if active && !connected {
            self.prepare_controller_or_defer("refresh_prepare_deferred", &snapshot)
                .await
        } else {
            None
        };
        let stopped_connection = if active && !connected {
            self.stop_connection_for_profile_mutation().await?
        } else {
            false
        };
        let (revision, physical_generation, persisted, maintenance_debt) =
            match run_blocking(|| self.store.replace_profile_loaded(id, snapshot.clone())) {
                Ok((revision, physical_generation, persisted)) => {
                    (revision, physical_generation, persisted, false)
                }
                Err(error) if error.mutation_committed() => {
                    let (revision, physical_generation, persisted) = if connected {
                        run_blocking(|| self.store.profile_snapshot_generation(id))
                            .map_err(|_| ProfileMutationError::Committed)?
                    } else {
                        self.profile_snapshot_after_commit(id).await?
                    };
                    (revision, physical_generation, persisted, true)
                }
                Err(error) => {
                    let mapped = self.map_profile_error_logged("replace_failed", error).await;
                    return Err(if stopped_connection {
                        ProfileMutationError::Committed
                    } else {
                        mapped
                    });
                }
            };
        run_blocking(|| {
            carry_forward_selections(
                &self.store,
                id,
                previous_revision,
                &previous,
                revision,
                &persisted,
            )
        })
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
        let (selections, explicit_selections) =
            run_blocking(|| initial_and_explicit_selections(&self.store, id, revision, &persisted));
        let controller = prepared.as_ref().map(PreparedController::controller);
        {
            let mut state = self.inner.write().await;
            state.clear_for_active(Some(id.clone()));
            state.apply_snapshot_with_optional_controller(
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
        if let Some(prepared) = prepared
            && prepared.activate().is_err()
        {
            self.reconcile_authoritative().await;
            return Err(ProfileMutationError::Committed);
        }
        Ok(self.current_status().await)
    }

    /// Prepares a controller for a snapshot that is being made active while
    /// disconnected. That is only an optimisation: every connect prepares
    /// its own controller again, and a daemon that starts with a saved
    /// profile runs without one. So a preparation failure (no default
    /// network route, another VPN owning it, a staging error) must not fail
    /// the import/activation/refresh itself - the profile is kept, the
    /// failure is logged, and a later connect reports its real cause.
    async fn prepare_controller_or_defer(
        &self,
        phase: &str,
        snapshot: &Snapshot,
    ) -> Option<PreparedController<P>> {
        let options = self
            .connect_options
            .lock()
            .map(|options| *options)
            .unwrap_or_default();
        match run_blocking(|| (self.controller_factory)(snapshot, options)) {
            Ok(prepared) => Some(prepared),
            Err(_) => {
                self.inner.write().await.push_event_with_details(
                    Severity::Warning,
                    Component::Core,
                    phase,
                    DaemonText::code("connect.prepare_deferred"),
                    json!({ "phase": phase, "os": std::env::consts::OS }),
                );
                None
            }
        }
    }

    /// `map_profile_error`, but a failure that collapses into the generic
    /// Storage bucket is logged first (with its full error chain), so
    /// refresh/activate/delete failures leave the same diagnostic trail as
    /// import does. Not-found and limit outcomes already carry a specific
    /// user-facing message and are mapped without logging.
    async fn map_profile_error_logged(
        &self,
        phase: &str,
        error: ProfileError,
    ) -> ProfileMutationError {
        if !matches!(
            error,
            ProfileError::NotFound(_) | ProfileError::LimitReached
        ) {
            self.log_mutation_storage_failure(phase, error_chain_text(&error))
                .await;
        }
        map_profile_error(error)
    }

    /// Every path that collapses into the generic `ProfileMutationError::Storage`
    /// (and, via `profile_api_error`, the opaque "backend_error" the desktop
    /// shows as a plain HTTP 500) used to do so silently - a real report
    /// showed exactly that response with no daemon event at all, leaving no
    /// way to tell which of several very different underlying causes
    /// (profile-store I/O, runtime staging, the elevated broker, ...) it
    /// actually was. `detail` is this module's own `ProfileError`/`BackendError`
    /// Display text or a plain phase name - never anything from an external
    /// server - so it's exactly as safe to log as the rest of this event.
    async fn log_mutation_storage_failure(&self, phase: &str, detail: impl std::fmt::Display) {
        // `Event`'s own `phase` field never reaches `EventDto` (only
        // `message` and `details` do - see core_event_to_dto), so it's
        // repeated inside `details` too: that's what actually lets
        // «Сохранить лог…» show which of several very different branches
        // failed instead of the same generic sentence every time.
        self.inner.write().await.push_event_with_details(
            Severity::Warning,
            Component::Subscription,
            phase,
            DaemonText::code("subscription.save_failed"),
            json!({ "phase": phase, "detail": detail.to_string(), "os": std::env::consts::OS }),
        );
    }

    async fn connected_cleanly(&self) -> bool {
        let state = self.inner.read().await;
        !state.degraded && state.state.current() == CoreConnectionState::Connected
    }

    /// Adding a subscription never needs a disconnect. It is downloaded while the tunnel
    /// stays up (a failure leaves the connection alone), kept in the list, and then switched
    /// to exactly like a manual switch: in always-ready mode the connection moves to it; where
    /// switching needs a disconnect, it waits in the list and the current connection stays.
    async fn import_while_connected(&self, url: &str) -> Result<StatusDto, BackendError>
    where
        C: HttpClient + 'static,
        P: 'static,
        F: 'static,
    {
        let url = url.trim();
        if url.is_empty() {
            return Err(BackendError::from_profile_mutation(
                ProfileMutationError::Storage,
            ));
        }
        let existing = match run_blocking(|| self.store.find_exact_source(url)) {
            Ok(existing) => existing,
            Err(error) => {
                return Err(BackendError::from_profile_mutation(
                    self.map_profile_error_logged("import_lookup_failed", error)
                        .await,
                ));
            }
        };
        if let Some(id) = existing.as_ref()
            && self.store.active_id().as_ref() == Some(id)
        {
            // The connected subscription itself: adding it again refreshes it.
            let mut status = Backend::refresh_subscription(self).await?;
            status.imported_profile_id = Some(id.as_str().to_owned());
            return Ok(status);
        }
        let staging = AtomicSnapshot::default();
        let (snapshot, _) = self
            .fetch_subscription(url, &staging)
            .await
            .map_err(BackendError::from_profile_mutation)?
            .into_parts();
        let id = {
            let _operation = self.operation.lock().await;
            self.store_inactive_profile_locked(existing, snapshot)
                .await
                .map_err(BackendError::from_profile_mutation)?
        };
        let imported_profile_id = Some(id.as_str().to_owned());
        let mut status = match Backend::activate_subscription(self, id).await {
            Ok(status) => status,
            // Switching needs a disconnect in this mode: the subscription waits in the list.
            Err(error) if error.profile_mutation() == Some(ProfileMutationError::Connected) => {
                self.current_status().await
            }
            // Added, but the switch failed: the list has changed all the same.
            Err(_) => {
                return Err(BackendError::from_profile_mutation(
                    ProfileMutationError::Committed,
                ));
            }
        };
        status.imported_profile_id = imported_profile_id;
        Ok(status)
    }

    /// Stores a downloaded subscription without touching the active profile or the live
    /// connection: as a new profile, or over the inactive profile with the same link.
    async fn store_inactive_profile_locked(
        &self,
        existing: Option<ProfileId>,
        snapshot: Arc<Snapshot>,
    ) -> Result<ProfileId, ProfileMutationError> {
        let (id, previous, maintenance_debt) = match existing {
            Some(id) => {
                let previous = match run_blocking(|| self.store.profile_snapshot(&id)) {
                    Ok(previous) => previous,
                    Err(error) => {
                        return Err(self
                            .map_profile_error_logged("import_previous_load_failed", error)
                            .await);
                    }
                };
                let debt = match run_blocking(|| self.store.replace_profile(&id, snapshot)) {
                    Ok(_) => false,
                    Err(error) if error.mutation_committed() => true,
                    Err(error) => {
                        return Err(self
                            .map_profile_error_logged("import_replace_failed", error)
                            .await);
                    }
                };
                (id, Some(previous), debt)
            }
            None => match run_blocking(|| self.store.add_profile(snapshot)) {
                Ok(id) => (id, None, false),
                Err(
                    ProfileError::IndexMaintenanceDebt { id, .. }
                    | ProfileError::CleanupDebt {
                        id,
                        mutation_committed: true,
                        ..
                    },
                ) => (id, None, true),
                Err(error) => {
                    return Err(self
                        .map_profile_error_logged("import_create_failed", error)
                        .await);
                }
            },
        };
        let (revision, physical_generation, persisted) =
            run_blocking(|| self.store.profile_snapshot_generation(&id))
                .map_err(|_| ProfileMutationError::Committed)?;
        if let Some((previous_revision, previous)) = previous {
            run_blocking(|| {
                carry_forward_selections(
                    &self.store,
                    &id,
                    previous_revision,
                    &previous,
                    revision,
                    &persisted,
                )
            })
            .map_err(|_| ProfileMutationError::Committed)?;
        }
        self.retain_profile_logo(&id, physical_generation, &persisted);
        self.inner.write().await.push_event(
            Severity::Info,
            Component::Subscription,
            "import",
            "Subscription imported.",
        );
        if maintenance_debt {
            self.record_maintenance_debt("profile_import").await;
        }
        Ok(id)
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
        let prepared = self
            .prepare_controller_or_defer("import_prepare_deferred", &snapshot)
            .await;
        let existing = match run_blocking(|| self.store.find_exact_source(trimmed_url)) {
            Ok(existing) => existing,
            Err(error) => {
                return Err(self
                    .map_profile_error_logged("import_lookup_failed", error)
                    .await);
            }
        };
        let (id, maintenance_debt, previous) = if let Some(id) = existing {
            let previous = match run_blocking(|| self.store.profile_snapshot(&id)) {
                Ok(previous) => previous,
                Err(error) => {
                    return Err(self
                        .map_profile_error_logged("import_previous_load_failed", error)
                        .await);
                }
            };
            let replace_debt =
                match run_blocking(|| self.store.replace_profile_loaded(&id, snapshot.clone())) {
                    Ok(_) => false,
                    Err(error) if error.mutation_committed() => {
                        self.profile_snapshot_after_commit(&id).await?;
                        true
                    }
                    Err(error) => {
                        self.log_mutation_storage_failure(
                            "import_replace_failed",
                            error_chain_text(&error),
                        )
                        .await;
                        return Err(map_profile_error(error));
                    }
                };
            let activate_debt = match run_blocking(|| self.store.activate(&id)) {
                Ok(_) => false,
                Err(error) if error.mutation_committed() => true,
                Err(error) => {
                    self.log_mutation_storage_failure(
                        "import_activate_selection_failed",
                        error_chain_text(&error),
                    )
                    .await;
                    self.reconcile_authoritative().await;
                    return Err(map_profile_error(error));
                }
            };
            (id, replace_debt || activate_debt, Some(previous))
        } else {
            match run_blocking(|| self.store.create_profile(snapshot.clone())) {
                Ok(id) => (id, false, None),
                Err(error) if error.mutation_committed() => {
                    let Some(id) = self.store.active_id() else {
                        self.log_mutation_storage_failure(
                            "import_create_committed_without_active_id",
                            error_chain_text(&error),
                        )
                        .await;
                        self.reconcile_authoritative().await;
                        return Err(ProfileMutationError::Storage);
                    };
                    (id, true, None)
                }
                Err(error) => {
                    self.log_mutation_storage_failure(
                        "import_create_failed",
                        error_chain_text(&error),
                    )
                    .await;
                    return Err(map_profile_error(error));
                }
            }
        };
        let (revision, physical_generation, persisted) =
            self.profile_snapshot_after_commit(&id).await?;
        if let Some((previous_revision, previous)) = previous {
            run_blocking(|| {
                carry_forward_selections(
                    &self.store,
                    &id,
                    previous_revision,
                    &previous,
                    revision,
                    &persisted,
                )
            })
            .map_err(|_| ProfileMutationError::Committed)?;
        }
        self.retain_profile_logo(&id, physical_generation, &persisted);
        let (selections, explicit_selections) = run_blocking(|| {
            initial_and_explicit_selections(&self.store, &id, revision, &persisted)
        });
        let controller = prepared.as_ref().map(PreparedController::controller);
        {
            let mut state = self.inner.write().await;
            state.clear_for_active(Some(id.clone()));
            state.apply_snapshot_with_optional_controller(
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
        if let Some(prepared) = prepared
            && prepared.activate().is_err()
        {
            // `PreparedController::activate` only ever returns the opaque
            // `BackendError` (see its doc comment), so "runtime_commit"
            // is the most specific phase name available here - but it is
            // still real signal, distinguishing this from the profile-store
            // failures logged above.
            self.log_mutation_storage_failure("import_runtime_commit_failed", "runtime_commit")
                .await;
            self.reconcile_authoritative().await;
            return Err(ProfileMutationError::Storage);
        }
        Ok(self.current_status().await)
    }
}

/// Runs profile-store work (synchronous disk I/O, possibly waiting out a
/// transient Windows file lock) without stalling a tokio worker: on the
/// multi-threaded runtime the daemon uses, the worker is handed off via
/// `block_in_place`; elsewhere (current-thread test runtimes, where
/// `block_in_place` would panic) it simply runs inline.
fn run_blocking<T>(work: impl FnOnce() -> T) -> T {
    match tokio::runtime::Handle::try_current() {
        Ok(handle) if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
            tokio::task::block_in_place(work)
        }
        _ => work(),
    }
}

fn map_profile_error(error: ProfileError) -> ProfileMutationError {
    match error {
        ProfileError::NotFound(_) => ProfileMutationError::NotFound,
        ProfileError::LimitReached => ProfileMutationError::LimitReached,
        _ => ProfileMutationError::Storage,
    }
}

/// `ProfileError` (and the `PersistenceError`/`io::Error` it usually wraps)
/// each use a fixed, generic `#[error("...")]` message with no
/// interpolation - real coverage confirmed this via an actual failure that
/// only ever logged "profile snapshot operation failed", with the *actual*
/// OS error (permissions, disk full, a lock, ...) thrown away. Walking
/// `.source()` down to the innermost error recovers it - still entirely
/// this module's own or the OS's own text, never anything external, so
/// exactly as safe to log as the top-level message alone.
fn error_chain_text(error: &(dyn std::error::Error + 'static)) -> String {
    let mut parts = vec![error.to_string()];
    let mut current = error;
    while let Some(source) = current.source() {
        parts.push(source.to_string());
        current = source;
    }
    parts.join(" <- ")
}

#[cfg(test)]
mod error_chain_tests {
    use super::error_chain_text;
    use multicore_core::{PersistenceError, ProfileError};

    #[test]
    fn walks_through_generic_wrapper_messages_to_the_real_os_error() {
        // Regression coverage for a real bug: a genuine subscription-add
        // failure only ever logged "profile snapshot operation failed" -
        // ProfileError::Snapshot and PersistenceError::Io both use a fixed,
        // generic #[error("...")] message, so the actual OS error (which
        // would have said exactly what was wrong - permissions, disk full,
        // a lock, ...) was thrown away before it ever reached a log.
        let io_error = std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "Access is denied. (os error 5)",
        );
        let error = ProfileError::Snapshot(PersistenceError::Io(io_error));

        let chain = error_chain_text(&error);

        assert!(
            chain.contains("profile snapshot operation failed"),
            "{chain}"
        );
        assert!(
            chain.contains("snapshot persistence operation failed"),
            "{chain}"
        );
        assert!(chain.contains("Access is denied"), "{chain}");
    }
}

fn default_selections(snapshot: &Snapshot) -> Vec<Option<String>> {
    selectable_groups(snapshot)
        .iter()
        .map(|group| {
            group
                .offered_nodes()
                .next()
                .map(|(_, node)| node.to_owned())
        })
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
                group.offered_nodes().find_map(|(node_index, node)| {
                    (catalog_node_id(revision, group_index, node_index) == *node_id)
                        .then(|| node.to_owned())
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
    if matching_nodes.next().is_some() || !group.offers(node_index) {
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

    async fn group_icon(&self, icon_id: &str) -> Option<Vec<u8>> {
        let runtime = self.inner.read().await;
        runtime
            .snapshot
            .as_ref()?
            .group_icon_png(icon_id)
            .map(<[u8]>::to_vec)
    }

    async fn listen_ports(&self) -> Vec<multicore_core::troubleshoot::ListenPort> {
        let runtime = self.inner.read().await;
        runtime
            .snapshot
            .as_deref()
            .map(multicore_core::troubleshoot::snapshot_listen_ports)
            .unwrap_or_default()
    }

    async fn record_troubleshoot(&self, message: DaemonText) {
        self.inner.write().await.push_event(
            Severity::Info,
            Component::Core,
            "troubleshoot",
            message,
        );
    }

    async fn import_subscription(
        &self,
        request: ImportSubscriptionRequest,
    ) -> Result<StatusDto, BackendError> {
        if self.connected_cleanly().await {
            return self.import_while_connected(&request.url).await;
        }
        let result = {
            let _operation = self.operation.lock().await;
            self.import_profile_locked(&request.url)
                .await
                .map_err(BackendError::from_profile_mutation)
        };
        self.rewarm_after(result).await.map(|mut status| {
            status.imported_profile_id = status.active_profile_id.clone();
            status
        })
    }

    async fn refresh_subscription(&self) -> Result<StatusDto, BackendError> {
        self.require_refresh_delete_mutation()
            .await
            .map_err(BackendError::from_profile_mutation)?;
        let id = self
            .store
            .active_id()
            .ok_or_else(|| BackendError::from_profile_mutation(ProfileMutationError::NotFound))?;
        // The download runs without the operation lock: selections stay instant meanwhile.
        let prefetched = self
            .prefetch_profile(&id)
            .await
            .map_err(BackendError::from_profile_mutation)?;
        let result = {
            let _operation = self.operation.lock().await;
            self.require_refresh_delete_mutation()
                .await
                .map_err(BackendError::from_profile_mutation)?;
            // Another subscription became active meanwhile: refresh that one instead.
            let active = self.store.active_id().ok_or_else(|| {
                BackendError::from_profile_mutation(ProfileMutationError::NotFound)
            })?;
            let prefetched = (active == id).then_some(prefetched);
            self.refresh_profile_locked(&active, prefetched)
                .await
                .map_err(BackendError::from_profile_mutation)
        };
        // Refreshed while connected: the new snapshot waits as pending; in always-ready mode
        // the connection moves to it right away.
        let switch = self.generation_change.lock().await;
        let resume = self
            .pause_for_generation_change(pending_changes_runtime)
            .await;
        if let Some(options) = resume {
            self.resume_after_generation_change(options).await;
        }
        drop(switch);
        let result = self.rewarm_after(result).await;
        match result {
            Ok(_) if resume.is_some() => Ok(self.current_status().await),
            result => result,
        }
    }

    async fn list_subscriptions(&self) -> Result<SubscriptionListDto, BackendError> {
        self.subscription_list_dto()
            .map_err(BackendError::from_profile_mutation)
    }

    async fn activate_subscription(&self, id: ProfileId) -> Result<StatusDto, BackendError> {
        self.require_profile(&id)
            .map_err(BackendError::from_profile_mutation)?;
        // Switching while connected in always-ready mode: stop, switch, reconnect on the new
        // generation (spec 6). Otherwise switching while connected stays refused.
        let started = std::time::Instant::now();
        let switch = self.generation_change.lock().await;
        let waited_ms = started.elapsed().as_millis();
        let resume = self
            .pause_for_generation_change(|state| state.active_profile_id.as_ref() != Some(&id))
            .await;
        let paused_ms = started.elapsed().as_millis();
        let mut timings = ActivationTimings::default();
        let result = self
            .activate_subscription_timed(&id, &mut timings)
            .await
            .map_err(BackendError::from_profile_mutation);
        let activated_ms = started.elapsed().as_millis();
        if let Some(options) = resume {
            self.resume_after_generation_change(options).await;
        }
        drop(switch);
        if result.is_ok() {
            // Where a slow switch spends its time: waiting for another switch, stopping the
            // old connection, loading/staging/committing the new profile, reconnecting.
            self.inner.write().await.push_event_with_details(
                Severity::Debug,
                Component::Core,
                "subscription_switch",
                DaemonText::code("subscription.switched"),
                json!({
                    "wait_ms": waited_ms,
                    "pause_ms": paused_ms.saturating_sub(waited_ms),
                    "load_ms": timings.load_ms,
                    "prepare_ms": timings.prepare_ms,
                    "commit_ms": timings.commit_ms,
                    "resume_ms": started.elapsed().as_millis().saturating_sub(activated_ms),
                    "switch_ms": started.elapsed().as_millis(),
                    "reconnected": resume.is_some(),
                }),
            );
        }
        let result = self.rewarm_after(result).await;
        match result {
            Ok(_) if resume.is_some() => Ok(self.current_status().await),
            result => result,
        }
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
        let prefetched = self
            .prefetch_profile(&id)
            .await
            .map_err(BackendError::from_profile_mutation)?;
        let result = {
            let _operation = self.operation.lock().await;
            self.require_refresh_delete_mutation()
                .await
                .map_err(BackendError::from_profile_mutation)?;
            self.refresh_profile_locked(&id, Some(prefetched))
                .await
                .map_err(BackendError::from_profile_mutation)?;
            self.subscription_list_dto()
                .map_err(BackendError::from_profile_mutation)
        };
        let switch = self.generation_change.lock().await;
        let resume = self
            .pause_for_generation_change(pending_changes_runtime)
            .await;
        if let Some(options) = resume {
            self.resume_after_generation_change(options).await;
        }
        drop(switch);
        self.rewarm_after(result).await
    }

    async fn replace_subscription_source(
        &self,
        id: ProfileId,
        request: ImportSubscriptionRequest,
    ) -> Result<SubscriptionListDto, BackendError> {
        self.require_profile(&id)
            .map_err(BackendError::from_profile_mutation)?;
        let result = {
            let _operation = self.operation.lock().await;
            self.require_disconnected_mutation()
                .await
                .map_err(BackendError::from_profile_mutation)?;
            self.replace_profile_source_locked(&id, &request.url, None)
                .await
                .map_err(BackendError::from_profile_mutation)?;
            self.subscription_list_dto()
                .map_err(BackendError::from_profile_mutation)
        };
        self.rewarm_after(result).await
    }

    async fn delete_subscription(&self, id: ProfileId) -> Result<StatusDto, BackendError> {
        self.require_profile(&id)
            .map_err(BackendError::from_profile_mutation)?;
        let result = self
            .delete_subscription(&id)
            .await
            .map_err(BackendError::from_profile_mutation);
        self.rewarm_after(result).await
    }

    async fn connect(&self) -> Result<StatusDto, BackendError> {
        self.connect_with(ConnectOptions::default()).await
    }

    async fn connect_with(&self, options: ConnectOptions) -> Result<StatusDto, BackendError> {
        let _switch = if let Ok(switch) = self.generation_change.try_lock() {
            switch
        } else {
            // A subscription switch is moving the connection: wait for it. If it reconnected
            // (on the new profile), that connection is the answer to this Connect.
            let switch = self.generation_change.lock().await;
            if self.inner.read().await.state.current() == CoreConnectionState::Connected {
                return Ok(self.current_status().await);
            }
            switch
        };
        self.connect_now(options).await
    }

    async fn shutdown(&self) -> Result<(), BackendError> {
        self.shutdown_cores().await;
        Ok(())
    }

    async fn disconnect(&self) -> Result<StatusDto, BackendError> {
        // After a switch in progress, so its reconnect cannot undo this Disconnect.
        let _switch = self.generation_change.lock().await;
        let result = self.disconnect_now("user").await;
        // The cores stay (or become) warm in the background; the answer does not wait.
        let result = self.rewarm_after(result).await;
        match result {
            Ok(_) => Ok(self.current_status().await),
            Err(error) => Err(error),
        }
    }

    async fn select(&self, request: SelectionRequest) -> Result<CatalogDto, SelectionError> {
        let _operation = self.operation.lock().await;
        let (
            snapshot,
            active_profile_id,
            live,
            group_index,
            is_primary,
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
            // The selector is reachable while connected, or while the service keeps the cores
            // of this catalog warm.
            let live =
                state.state.current() == CoreConnectionState::Connected || state.cores_live();
            let groups = selectable_groups(&snapshot);
            let group_index = groups
                .iter()
                .enumerate()
                .find_map(|(index, _)| {
                    (catalog_group_id(request.revision, index) == request.group_id).then_some(index)
                })
                .ok_or(SelectionError::UnknownGroup)?;
            let group = &groups[group_index];
            if group.hidden {
                return Err(SelectionError::UnknownGroup);
            }
            let node_index = group
                .offered_nodes()
                .find_map(|(index, _)| {
                    (catalog_node_id(request.revision, group_index, index) == request.node_id)
                        .then_some(index)
                })
                .ok_or(SelectionError::UnknownNode)?;
            let is_primary = primary_group_index(&groups) == Some(group_index);
            let raw_group = group.name.to_owned();
            let raw_node = group.nodes[node_index].to_owned();
            let previous_raw_node = state.selected_nodes[group_index].clone();
            (
                snapshot,
                active_profile_id,
                live,
                group_index,
                is_primary,
                raw_group,
                raw_node,
                previous_raw_node,
                state.pending_snapshot.clone(),
            )
        };

        if live && self.selector.select(&raw_group, &raw_node).await.is_err() {
            self.inner.write().await.push_event(
                Severity::Warning,
                Component::Mihomo,
                "selection_failed",
                DaemonText::code("route.select_failed"),
            );
            return Err(SelectionError::SelectorFailed);
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
                if live
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
            let (selected_nodes, explicit_selected_nodes) = run_blocking(|| {
                initial_and_explicit_selections(
                    &self.store,
                    &active_profile_id,
                    request.revision,
                    &snapshot,
                )
            });
            state.selected_nodes = selected_nodes;
            state.explicit_selected_nodes = explicit_selected_nodes;
            state.current_node = primary_group_index(&selectable_groups(&snapshot))
                .and_then(|index| state.selected_nodes.get(index).cloned())
                .flatten();
        } else {
            state.selected_nodes[group_index] = Some(raw_node.clone());
            state.explicit_selected_nodes[group_index] = Some(raw_node.clone());
            if is_primary {
                state.current_node = Some(raw_node);
            }
        }
        state.rebuild_catalog();
        state.push_event(
            Severity::Info,
            Component::Mihomo,
            "selection",
            DaemonText::code("route.selected"),
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

    async fn session(&self) -> Result<SessionDto, SessionError> {
        let (connected, connected_since_unix_ms, exit_start) = {
            let state = self.inner.read().await;
            if !state.cores_live() {
                return Err(SessionError::NotConnected);
            }
            // The raw name behind the exit the status reports: the primary group's node.
            let exit_start = state.current_node.clone();
            (
                state.state.current() == CoreConnectionState::Connected,
                state.connected_since_unix_ms,
                exit_start,
            )
        };
        // Before connecting only the automatic exit is meaningful; counters and uptime belong
        // to a live connection.
        let totals = if connected {
            self.selector.traffic_totals().await.ok()
        } else {
            None
        };
        // At most one short Mihomo `now` walk per EXIT_NOW_TTL for an unchanged exit.
        let exit_now = match exit_start {
            Some(start) => {
                let cached = self.exit_now_cache.lock().ok().and_then(|cache| {
                    cache
                        .as_ref()
                        .filter(|cache| cache.start == start && cache.at.elapsed() < EXIT_NOW_TTL)
                        .map(|cache| cache.resolved.clone())
                });
                match cached {
                    Some(resolved) => resolved,
                    None => {
                        let resolved = resolve_exit_now(self.selector.as_ref(), &start).await;
                        if let Ok(mut cache) = self.exit_now_cache.lock() {
                            *cache = Some(ExitNowCache {
                                at: std::time::Instant::now(),
                                start,
                                resolved: resolved.clone(),
                            });
                        }
                        resolved
                    }
                }
            }
            None => None,
        };
        Ok(SessionDto {
            connected_since_unix_ms: if connected {
                connected_since_unix_ms
            } else {
                0
            },
            download_total: totals.map(|totals| totals.download),
            upload_total: totals.map(|totals| totals.upload),
            exit_now: exit_now.map(|raw| safe_catalog_label(&raw, "Узел", 0)),
        })
    }

    /// Measures the requested groups (a sweep) and nodes. Explicitly requested nodes and each
    /// swept group's selected node go first, on the priority lane with its own small budget, so
    /// they never wait behind the sweep; a node another request is already measuring is joined,
    /// and one measured since this request began is not measured again.
    async fn latencies(&self, request: LatencyRequest) -> Result<LatencyResponseDto, LatencyError> {
        let started = tokio::time::Instant::now();
        let batch_deadline = started + LATENCY_BATCH_TIMEOUT;
        if request
            .group_ids
            .len()
            .saturating_add(request.node_ids.len())
            > MAX_LATENCY_RESULTS
        {
            return Err(LatencyError::TooManyTargets);
        }

        let targets = {
            let state = self.inner.read().await;
            if !state.cores_live() {
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
                .filter(|group_index| !groups[*group_index].hidden)
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
                    group.offered_nodes().map(move |(node_index, _)| {
                        catalog_node_id(revision, group_index, node_index)
                    })
                })
                .collect();
            if !requested_nodes
                .iter()
                .all(|node_id| known_nodes.contains(*node_id))
            {
                return Err(LatencyError::UnknownNode);
            }

            let probe_all = requested_groups.is_empty() && requested_nodes.is_empty();
            let selected_nodes = &state.selected_nodes;
            let mut targets: Vec<_> = groups
                .iter()
                .enumerate()
                .flat_map(|(group_index, group)| {
                    let group_id = catalog_group_id(revision, group_index);
                    let group_requested = requested_groups.contains(group_id.as_str());
                    let requested_nodes = &requested_nodes;
                    let selected = selected_nodes.get(group_index).cloned().flatten();
                    group
                        .offered_nodes()
                        .filter_map(move |(node_index, raw_node)| {
                            let node_id = catalog_node_id(revision, group_index, node_index);
                            let explicit = requested_nodes.contains(node_id.as_str());
                            (probe_all || group_requested || explicit).then(|| {
                                let priority = explicit || selected.as_deref() == Some(raw_node);
                                (group_id.clone(), node_id, raw_node.to_owned(), priority)
                            })
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

        // Priority targets start first; within each lane the catalog order is kept. Every
        // target waits for its lane's permit, so the sweep never holds more than its budget.
        let mut order: Vec<usize> = (0..targets.len()).collect();
        order.sort_by_key(|index| !targets[*index].3);
        let mut probes = tokio::task::JoinSet::new();
        for index in order {
            let (_, node_id, raw_node, priority) = &targets[index];
            let lane = LatencyLane {
                selector: self.selector.clone(),
                permits: if *priority {
                    self.priority_latency_permits.clone()
                } else {
                    self.latency_permits.clone()
                },
                flights: self.latency_flights.clone(),
            };
            probes.spawn(probe_latency_target(
                lane,
                started,
                index,
                targets[index].0.clone(),
                node_id.clone(),
                raw_node.clone(),
            ));
        }

        let all_targets: Vec<_> = targets
            .into_iter()
            .map(|(group_id, node_id, _, _)| (group_id, node_id, ()))
            .collect();
        let mut completed = vec![None; all_targets.len()];
        while !probes.is_empty() {
            match tokio::time::timeout_at(batch_deadline, probes.join_next()).await {
                Ok(Some(Ok((index, entry)))) => completed[index] = Some(entry),
                Ok(Some(Err(_))) => {}
                Ok(None) => break,
                Err(_) => {
                    // Waiting requests stop here; probes already running finish on their own
                    // (bounded by the probe timeout) for anybody else waiting on them.
                    probes.abort_all();
                    break;
                }
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
        RuntimeCheckState::Disabled => RuntimeCheckDto::Disabled,
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
                    state.mark_error(DaemonText::code("connect.interrupted"), true);
                    state.push_event(
                        Severity::Error,
                        Component::Core,
                        "connect",
                        DaemonText::code("connect.aborted"),
                    );
                }
                return;
            }
            std::thread::yield_now();
        }
    }
}

/// At most this many of the failed core's last log lines go into the failure event.
const FAILED_CORE_LOG_TAIL_LINES: usize = 8;
const FAILED_CORE_LOG_LINE_CHARS: usize = 300;
/// The service answers `Diagnostics` within seconds; a failure report must not wait longer.
const FAILED_CORE_DIAGNOSTICS_TIMEOUT: Duration = Duration::from_secs(3);

/// The last lines the failed core wrote (its own explanation of the failure), oldest first.
async fn failed_core_log_tail<P: ProcessController + ?Sized>(
    controller: &P,
    engine: Engine,
) -> Vec<String> {
    let Ok(diagnostics) =
        tokio::time::timeout(FAILED_CORE_DIAGNOSTICS_TIMEOUT, controller.diagnostics()).await
    else {
        return Vec::new();
    };
    let mut tail: Vec<String> = diagnostics
        .logs
        .iter()
        .rev()
        .filter(|record| record.engine == engine)
        .take(FAILED_CORE_LOG_TAIL_LINES)
        .map(|record| {
            record
                .message
                .chars()
                .take(FAILED_CORE_LOG_LINE_CHARS)
                .collect()
        })
        .collect();
    tail.reverse();
    tail
}

/// The API error code of a failed Connect: the desktop maps each to its own message.
fn start_failure_code(cause: multicore_core::StartFailureCause) -> &'static str {
    use multicore_core::StartFailureCause as Cause;
    match cause {
        Cause::ElevationCancelled => "elevation_cancelled",
        Cause::ElevationFailed => "elevation_failed",
        Cause::BrokerFailed => "broker_failed",
        Cause::CoreFailed => "core_failed",
        Cause::CoreMissing => "core_missing",
        Cause::CoreExited => "core_exited",
        Cause::ReadinessTimedOut => "core_timed_out",
        Cause::TunFailed => "tun_failed",
        Cause::TunTimedOut => "tun_timed_out",
        Cause::SystemProxyFailed => "system_proxy_failed",
        Cause::ServiceBusy => "service_busy",
        Cause::Unknown => "connect_failed",
    }
}

fn start_failure_message(
    engine: multicore_core::Engine,
    cause: multicore_core::StartFailureCause,
) -> DaemonText {
    use multicore_core::{Engine, StartFailureCause as Cause};
    let core = match engine {
        Engine::Xray => "Xray",
        Engine::Mihomo => "Mihomo",
    };
    let with_core = |code| DaemonText::with_args(code, [core]);
    match cause {
        Cause::ElevationCancelled => DaemonText::code("start.elevation_cancelled"),
        Cause::ElevationFailed => DaemonText::code("start.elevation_failed"),
        Cause::BrokerFailed => DaemonText::code("start.broker_failed"),
        Cause::CoreFailed => with_core("start.core_failed"),
        Cause::CoreMissing => with_core("start.core_missing"),
        Cause::CoreExited => with_core("start.core_exited"),
        Cause::ReadinessTimedOut => with_core("start.core_timed_out"),
        Cause::TunFailed => DaemonText::code("start.tun_failed"),
        Cause::TunTimedOut => DaemonText::code("start.tun_timed_out"),
        Cause::SystemProxyFailed => DaemonText::code("start.system_proxy_failed"),
        Cause::ServiceBusy => DaemonText::code("start.service_busy"),
        Cause::Unknown => DaemonText::code("start.unknown"),
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
    icons_pending: bool,
) -> CatalogDto {
    let icon = |name: &str| {
        let id = snapshot.group_icon_id(name);
        let pending = icons_pending && id.is_none() && snapshot.names_group_icon(name);
        (id, pending)
    };
    let source_groups = selectable_groups(snapshot);
    let selected_group_index =
        profile.and_then(|profile| source_groups.iter().position(|group| group.name == profile));
    // Hidden groups and members Mihomo drops are left out; ids keep their positions in the
    // full lists, so every id maps to the same raw name whatever is shown.
    let groups = source_groups
        .iter()
        .enumerate()
        .filter(|(_, group)| !group.hidden)
        .map(|(group_index, group)| {
            let selected = selected_group_index == Some(group_index);
            let selected_node_index = selected_nodes
                .get(group_index)
                .and_then(Option::as_deref)
                .and_then(|current_node| group.nodes.iter().position(|node| *node == current_node));
            let nodes = group
                .offered_nodes()
                .map(|(node_index, node)| {
                    let (icon_id, icon_pending) = icon(node);
                    NodeDto {
                        id: catalog_node_id(revision, group_index, node_index),
                        label: safe_catalog_label(node, "Узел", node_index),
                        selected: selected_node_index == Some(node_index),
                        delay_ms: None,
                        icon_id,
                        icon_pending,
                    }
                })
                .collect();
            let (icon_id, icon_pending) = icon(group.name);
            GroupDto {
                id: catalog_group_id(revision, group_index),
                label: safe_catalog_label(group.name, "Группа", group_index),
                selected,
                nodes,
                icon_id,
                icon_pending,
            }
        })
        .collect();
    CatalogDto { revision, groups }
}

/// A subscription downloaded without the operation lock, waiting to be committed.
struct Prefetched {
    source_url: String,
    snapshot: Arc<Snapshot>,
}

struct SelectableGroup<'a> {
    name: &'a str,
    nodes: Vec<&'a str>,
    /// `offered[i]`: Mihomo keeps `nodes[i]` in the group, so it can be listed and selected.
    offered: Vec<bool>,
    /// `hidden: true`: the provider keeps this group out of the UI.
    hidden: bool,
}

impl SelectableGroup<'_> {
    fn offers(&self, node_index: usize) -> bool {
        !self.hidden && self.offered.get(node_index).copied().unwrap_or(false)
    }

    /// The selectable nodes with their positions in `nodes`.
    fn offered_nodes(&self) -> impl Iterator<Item = (usize, &str)> + '_ {
        self.nodes
            .iter()
            .enumerate()
            .filter(|(index, _)| self.offers(*index))
            .map(|(index, node)| (index, *node))
    }
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
            // Mihomo's members, in its order; ids index into this list, so members excluded
            // by `exclude-filter` / `exclude-type` keep their place but are never offered.
            let members = group.members.iter().take(MAX_CATALOG_NODES_PER_GROUP);
            SelectableGroup {
                name: &group.name,
                nodes: members.clone().map(|member| member.name.as_str()).collect(),
                offered: members.map(|member| member.offered).collect(),
                hidden: group.hidden,
            }
        })
        .collect()
}

/// The group the connection card names: the first select group the provider did not hide.
fn primary_group_index(groups: &[SelectableGroup<'_>]) -> Option<usize> {
    groups.iter().position(|group| !group.hidden)
}

fn catalog_group_id(revision: u64, group_index: usize) -> String {
    format!("g-{revision:016x}-{group_index:04x}")
}

fn catalog_node_id(revision: u64, group_index: usize, node_index: usize) -> String {
    format!("n-{revision:016x}-{group_index:04x}-{node_index:04x}")
}

/// Accepts Unicode letters/numbers, ASCII space, the punctuation and symbols server names use
/// (see `is_safe_catalog_label_char`) and emoji. Anything else, an address or `user:secret`
/// form, a UUID or a recognizable credential gets a positional generic label instead.
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
        || contains_address_or_userinfo(published)
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

/// Everything a provider writes in an ordinary server name. Deliberately absent: control and
/// format characters (bidi overrides, line/paragraph separators), `=`, `\` and backquotes;
/// `:`, `@` and `/` are allowed here and checked structurally (`contains_address_or_userinfo`).
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
                | '|'
                | '*'
                | '/'
                | ':'
                | '@'
                | '&'
                | '%'
                | '~'
                | '?'
                | ';'
                | '\''
                | '"'
                | '<'
                | '>'
                | '{'
                | '}'
                | '^'
                | '$'
                // Typographic dashes, quotes, bullets and signs.
                | '\u{2010}'..='\u{2015}'
                | '\u{2018}'..='\u{201f}'
                | '\u{2022}'
                | '\u{2026}'
                | '\u{00b7}'
                | '\u{00ab}'
                | '\u{00bb}'
                | '\u{00b0}'
                | '\u{00a9}'
                | '\u{00ae}'
                | '\u{2116}'
                | '\u{2122}'
                | '\u{203c}'
                | '\u{2049}'
                | '\u{2139}'
                // Arrows, technical symbols, enclosed numbers, shapes, symbols, dingbats.
                | '\u{2190}'..='\u{21ff}'
                | '\u{2300}'..='\u{23ff}'
                | '\u{2460}'..='\u{24ff}'
                | '\u{25a0}'..='\u{27bf}'
                | '\u{2b00}'..='\u{2bff}'
                | '\u{3030}'
                | '\u{303d}'
                | '\u{3297}'
                | '\u{3299}'
                // Emoji: pictographs, flags, skin tones, keycaps, ZWJ and tag sequences.
                | '\u{1f000}'..='\u{1faff}'
                | '\u{fe0f}'
                | '\u{200d}'
                | '\u{20e3}'
                | '\u{e0020}'..='\u{e007f}'
        )
}

/// A URL (`scheme://`) or a `user:secret` / `user@host` form: `:` or `@` directly between two
/// non-space characters. "Германия: Франкфурт" and "Nodes @ 10 Gbps" stay readable.
fn contains_address_or_userinfo(source: &str) -> bool {
    if source.contains("://") {
        return true;
    }
    let characters: Vec<char> = source.chars().collect();
    characters.windows(3).any(|window| {
        matches!(window[1], ':' | '@') && !window[0].is_whitespace() && !window[2].is_whitespace()
    })
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
        message_key: value
            .get("message_key")
            .cloned()
            .and_then(|key| serde_json::from_value(key).ok()),
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
            message_key: None,
            degraded: false,
            subscription: None,
            cores_ready: false,
            service_mode: false,
            imported_profile_id: None,
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
            message_key: None,
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
        if state.connection == ConnectionState::Connecting {
            return Err(BackendError::from_profile_mutation(
                ProfileMutationError::Connected,
            ));
        }
        // Connected: the subscription is added to the list and the connection stays as it is.
        let keep_active = state.connection == ConnectionState::Connected;
        let source_url = request.url.trim();
        let imported_id = if let Some(existing_id) = state
            .profiles
            .iter()
            .find(|profile| profile.source_url == source_url)
            .map(|profile| profile.id.clone())
        {
            existing_id
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
            id
        };
        if !keep_active {
            state.active_profile_id = Some(imported_id.clone());
            state.connection = ConnectionState::Ready;
        }
        state.push_event("info", "Подписка импортирована.");
        let mut status = state.status();
        status.imported_profile_id = Some(imported_id.as_str().to_owned());
        Ok(status)
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
