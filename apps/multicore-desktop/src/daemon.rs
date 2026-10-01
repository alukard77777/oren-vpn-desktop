use std::{
    error::Error,
    fmt,
    io::{Read, Seek, Write as _},
    net::{IpAddr, SocketAddr},
    time::Duration,
};

use reqwest::blocking::Client;
use serde::{Deserialize, Serialize, de::DeserializeOwned};

/// Raw, low-level detail for a daemon-HTTP-call failure - the exact
/// reqwest/io/serde error text, or a bounded snippet of an unparseable
/// response body. This exists specifically for failures the daemon's own
/// event log can never record: it has no way to know its own response
/// failed to reach or decode on the desktop, or that the desktop couldn't
/// even connect to it in the first place. A real report showed "Копировать
/// лог" exporting an empty daemon event log while the visible error was
/// still the generic fallback - i.e. exactly this class of failure with no
/// diagnostic trail at all. Only ever fed text this module already treats
/// as safe to show the user (see `DaemonError`'s doc comment), plus raw
/// reqwest/io/serde error text, which for a fixed loopback URL and a body
/// sent separately from the URL never carries the subscription URL or
/// token. Best-effort and bounded: a logging failure must never break the
/// request that triggered it.
const LOW_LEVEL_LOG_FILE_NAME: &str = "client-errors.log";
const MAX_LOW_LEVEL_LOG_BYTES: u64 = 262_144;

fn low_level_log_path() -> Option<std::path::PathBuf> {
    low_level_data_root().map(|root| root.join("logs").join(LOW_LEVEL_LOG_FILE_NAME))
}

/// The data root the log lives under. Unit tests replay failing daemon exchanges (forbidden
/// subscriptions, malformed bodies, private-node markers) through the very code that records
/// them, so under `cfg(test)` the log goes to a per-process directory in the OS temp
/// directory and never reaches the real user's diagnostics.
#[cfg(not(test))]
fn low_level_data_root() -> Option<std::path::PathBuf> {
    multicore_platform::dirs::user_data_root().ok()
}

#[cfg(test)]
fn low_level_data_root() -> Option<std::path::PathBuf> {
    Some(std::env::temp_dir().join(format!("multicore-desktop-test-{}", std::process::id())))
}

pub(crate) fn record_low_level(context: &str, detail: impl fmt::Display) {
    static LAST_ENTRY: std::sync::Mutex<Option<(String, std::time::Instant)>> =
        std::sync::Mutex::new(None);
    let entry = format!("{context}\t{detail}");
    // Status/event polling keeps failing the same way while the daemon is
    // down or restarting; logging every poll flooded this file and pushed
    // the one relevant failure out through rotation.
    if let Ok(mut last) = LAST_ENTRY.lock() {
        let now = std::time::Instant::now();
        if is_repeated_low_level_entry(last.as_ref(), &entry, now) {
            return;
        }
        *last = Some((entry.clone(), now));
    }
    if let Some(path) = low_level_log_path() {
        write_low_level_entry(&path, context, detail);
    }
}

const LOW_LEVEL_REPEAT_WINDOW: Duration = Duration::from_secs(60);

fn is_repeated_low_level_entry(
    last: Option<&(String, std::time::Instant)>,
    entry: &str,
    now: std::time::Instant,
) -> bool {
    last.is_some_and(|(last_entry, at)| {
        last_entry == entry && now.duration_since(*at) < LOW_LEVEL_REPEAT_WINDOW
    })
}

fn write_low_level_entry(path: &std::path::Path, context: &str, detail: impl fmt::Display) {
    // Writers seek to the end themselves (no O_APPEND, see below), so two threads writing at
    // once would land on the same offset and one line would overwrite the other; rotation
    // must not interleave with a write either.
    static WRITE: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _write = WRITE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let _ = (|| -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // Deliberately not opened with `.append(true)`: on Windows that
        // grants only FILE_APPEND_DATA, which can silently keep every write
        // pinned to end-of-file even after `set_len(0)`, defeating rotation
        // below. Tracking and seeking to the position ourselves (the same
        // pattern multicore-daemon's PersistentEventSink already uses)
        // avoids that entirely.
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .read(true)
            .open(path)?;
        let mut offset = file.seek(std::io::SeekFrom::End(0))?;
        if offset > MAX_LOW_LEVEL_LOG_BYTES {
            // Keep the newest half instead of starting from nothing, so a
            // rotation right after a real failure doesn't erase it.
            let mut contents = Vec::new();
            file.seek(std::io::SeekFrom::Start(0))?;
            file.read_to_end(&mut contents)?;
            let cut = contents
                .len()
                .saturating_sub((MAX_LOW_LEVEL_LOG_BYTES / 2) as usize);
            let keep_from = contents[cut..]
                .iter()
                .position(|&byte| byte == b'\n')
                .map_or(contents.len(), |newline| cut + newline + 1);
            let kept = &contents[keep_from..];
            file.set_len(0)?;
            file.seek(std::io::SeekFrom::Start(0))?;
            file.write_all(kept)?;
            offset = kept.len() as u64;
        }
        let timestamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis();
        // Only the field contents are flattened; the separators between
        // the three fields stay real tabs.
        let flatten = |text: String| -> String {
            text.chars()
                .map(|character| match character {
                    '\r' | '\n' | '\t' => ' ',
                    other => other,
                })
                .collect()
        };
        let line = format!(
            "{timestamp}\t{}\t{}",
            flatten(context.to_owned()),
            flatten(detail.to_string())
        );
        file.seek(std::io::SeekFrom::Start(offset))?;
        writeln!(file, "{line}")
    })();
}

/// Everything `record_low_level` has captured since this log was last
/// rotated, for the "Копировать лог" export to fold in alongside the
/// daemon's own event log. `None` when nothing has been recorded yet or the
/// file can't be read.
pub(crate) fn read_low_level_log() -> Option<String> {
    read_low_level_entries(&low_level_log_path()?)
}

fn read_low_level_entries(path: &std::path::Path) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .filter(|contents| !contents.trim().is_empty())
}

const STATUS_RESPONSE_LIMIT_BYTES: usize = 64 * 1024;
const EVENTS_RESPONSE_LIMIT_BYTES: usize = 2 * 1024 * 1024;
const DIAGNOSTICS_RESPONSE_LIMIT_BYTES: usize = 2 * 1024 * 1024;
const CATALOG_RESPONSE_LIMIT_BYTES: usize = 32 * 1024 * 1024;
const LATENCIES_RESPONSE_LIMIT_BYTES: usize = 32 * 1024 * 1024;
#[allow(dead_code)] // The profile client is wired into the desktop event loop in Task 6.
const SUBSCRIPTIONS_RESPONSE_LIMIT_BYTES: usize = 1024 * 1024;
const API_ERROR_RESPONSE_LIMIT_BYTES: usize = 16 * 1024;
const DASHBOARD_URL_RESPONSE_LIMIT_BYTES: usize = 4 * 1024;
const SESSION_RESPONSE_LIMIT_BYTES: usize = 4 * 1024;
// The daemon serves canonical group icons of at most 128 KiB.
const GROUP_ICON_RESPONSE_LIMIT_BYTES: usize = 256 * 1024;

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
pub struct DaemonStatus {
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub active_profile_id: Option<String>,
    #[serde(default, alias = "profile_name")]
    pub profile: Option<String>,
    #[serde(default, alias = "node")]
    pub current_node: Option<String>,
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default)]
    pub degraded: bool,
    #[serde(default)]
    pub subscription: Option<DaemonSubscriptionInfo>,
    /// The cores run (connected, or warm through the always-ready service).
    #[serde(default)]
    pub cores_ready: bool,
    /// The daemon drives the always-ready MultiCore service.
    #[serde(default)]
    pub service_mode: bool,
    /// Only in an import's answer: the profile it added. When it is not the active one, the
    /// import was kept in the list without switching to it.
    #[serde(default)]
    pub imported_profile_id: Option<String>,
}

/// Safe subscription collection returned by the daemon. Source URLs deliberately have no
/// representation in the desktop process.
#[allow(dead_code)] // The profile client is wired into the desktop event loop in Task 6.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
pub struct DaemonSubscriptionList {
    #[serde(default)]
    pub active_profile_id: Option<String>,
    #[serde(default)]
    pub profiles: Vec<DaemonSubscriptionProfile>,
}

/// Bounded presentation metadata for one daemon-owned subscription profile.
#[allow(dead_code)] // The profile client is wired into the desktop event loop in Task 6.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
pub struct DaemonSubscriptionProfile {
    pub id: String,
    #[serde(default)]
    pub active: bool,
    #[serde(default)]
    pub available: bool,
    #[serde(default)]
    pub display_name: String,
    #[serde(default)]
    pub used_bytes: Option<u64>,
    #[serde(default)]
    pub total_bytes: Option<u64>,
    #[serde(default)]
    pub expires_at_unix: Option<u64>,
    #[serde(default)]
    pub service_logo_path: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
pub struct DaemonSubscriptionInfo {
    #[serde(default)]
    pub source_name: String,
    #[serde(default)]
    pub display_name: String,
    #[serde(default)]
    pub uploaded_bytes: Option<u64>,
    #[serde(default)]
    pub downloaded_bytes: Option<u64>,
    #[serde(default)]
    pub used_bytes: Option<u64>,
    #[serde(default)]
    pub total_bytes: Option<u64>,
    #[serde(default)]
    pub expires_at_unix: Option<u64>,
    #[serde(default)]
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
    #[serde(default)]
    pub refresh_available: bool,
    /// The provider's `dumb-mode` header: only the first route group is shown.
    #[serde(default)]
    pub dumb_mode: bool,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
pub struct DaemonEvent {
    pub id: u64,
    #[serde(default)]
    pub timestamp: String,
    #[serde(default)]
    pub level: String,
    #[serde(default)]
    pub component: String,
    #[serde(default, alias = "summary")]
    pub message: String,
    #[serde(default)]
    pub details: Option<serde_json::Value>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
pub struct DaemonEventBatch {
    pub epoch: String,
    pub events: Vec<DaemonEvent>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
pub struct DaemonDiagnosticLog {
    pub id: u64,
    #[serde(default)]
    pub timestamp: String,
    #[serde(default)]
    pub component: String,
    #[serde(default)]
    pub level: String,
    #[serde(default)]
    pub message: String,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
pub struct DaemonProxyMapping {
    #[serde(default)]
    pub proxy_name: String,
    #[serde(default)]
    pub address: String,
    #[serde(default)]
    pub xray_label: String,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
pub struct DaemonDiagnostics {
    #[serde(default)]
    pub xray: String,
    #[serde(default)]
    pub mihomo: String,
    #[serde(default)]
    pub tun: String,
    #[serde(default)]
    pub logs: Vec<DaemonDiagnosticLog>,
    #[serde(default)]
    pub mappings: Vec<DaemonProxyMapping>,
}

/// GET /v1/session: when the live connection started and Mihomo's cumulative byte counters.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
pub struct DaemonSession {
    pub connected_since_unix_ms: u64,
    #[serde(default)]
    pub download_total: Option<u64>,
    #[serde(default)]
    pub upload_total: Option<u64>,
    /// The node an automatic exit group currently routes through (daemon-side label).
    #[serde(default)]
    pub exit_now: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
pub struct DaemonCatalog {
    pub revision: u64,
    pub groups: Vec<DaemonCatalogGroup>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
pub struct DaemonCatalogGroup {
    pub id: String,
    pub label: String,
    pub selected: bool,
    pub nodes: Vec<DaemonCatalogNode>,
    /// Opaque id of the group's downloaded icon image (`GET /v1/group-icons/{id}`).
    #[serde(default)]
    pub icon_id: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
pub struct DaemonCatalogNode {
    pub id: String,
    pub label: String,
    pub selected: bool,
    pub delay_ms: Option<u64>,
    /// Set when the node is itself a proxy group with a downloaded icon.
    #[serde(default)]
    pub icon_id: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
pub struct DaemonLatencies {
    #[serde(default)]
    pub entries: Vec<DaemonLatencyEntry>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
pub struct DaemonLatencyEntry {
    #[serde(default)]
    pub group_id: String,
    #[serde(default)]
    pub node_id: String,
    #[serde(default)]
    pub latency_ms: Option<u64>,
    #[serde(default)]
    pub status: String,
}

#[allow(dead_code)] // Profile-specific variants are consumed by Task 6 UI workers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DaemonErrorKind {
    Other,
    StaleRevision,
    DefinitiveHttp,
    Ambiguous,
    ProfileNotFound,
    ProfileConflict,
    ProfileServer,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DaemonError {
    message: String,
    kind: DaemonErrorKind,
    /// The message explains the actual cause and is known-safe to show
    /// verbatim: either chosen locally from an allowlisted daemon error
    /// code, or authored by this module itself about the local transport
    /// (see `transport`). Never set for text that might echo anything a
    /// remote server sent.
    specific: bool,
}

impl DaemonError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            kind: DaemonErrorKind::Other,
            specific: false,
        }
    }

    /// Whether the message names the concrete cause (for example an HTTP 403
    /// from the subscription server) rather than a generic failure.
    pub(crate) fn is_specific(&self) -> bool {
        self.specific
    }

    fn with_known_code(mut self, code: &str) -> Self {
        if let Some(message) = known_error_message(code) {
            self.message = message.to_owned();
            self.specific = true;
        }
        self
    }

    pub(crate) fn stale_revision() -> Self {
        Self {
            message: "Каталог маршрутов изменился.".into(),
            kind: DaemonErrorKind::StaleRevision,
            specific: false,
        }
    }

    pub(crate) fn is_stale_revision(&self) -> bool {
        self.kind == DaemonErrorKind::StaleRevision
    }

    /// Whether a successful selection may have reached the daemon even though the client could
    /// not safely decode its response. Callers should refresh state before making another choice.
    pub(crate) fn requires_reconciliation(&self) -> bool {
        self.kind == DaemonErrorKind::Ambiguous
    }

    #[allow(dead_code)] // Consumed by Task 6 UI workers.
    pub(crate) fn is_profile_not_found(&self) -> bool {
        self.kind == DaemonErrorKind::ProfileNotFound
    }

    #[allow(dead_code)] // Consumed by Task 6 UI workers.
    pub(crate) fn is_profile_conflict(&self) -> bool {
        self.kind == DaemonErrorKind::ProfileConflict
    }

    #[allow(dead_code)] // Consumed through the Task 6 profile client calls.
    fn profile_http(kind: DaemonErrorKind, message: &'static str) -> Self {
        Self {
            message: message.into(),
            kind,
            specific: false,
        }
    }

    fn definitive_http(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            kind: DaemonErrorKind::DefinitiveHttp,
            specific: false,
        }
    }

    /// Same shape as `definitive_http`, but for a local HTTP status the
    /// daemon returned with a body this module couldn't map to a known
    /// error code. Only the numeric status is interpolated - never
    /// anything from the response body - so, like `transport`, this is
    /// safe to show verbatim instead of it being replaced by a generic
    /// fallback that names no real cause.
    fn definitive_http_with_status(status: reqwest::StatusCode) -> Self {
        Self {
            message: format!("Фоновый сервис ответил ошибкой (HTTP {}).", status.as_u16()),
            kind: DaemonErrorKind::DefinitiveHttp,
            specific: true,
        }
    }

    /// No production call site builds a non-specific ambiguous error
    /// anymore (every safe, locally-authored message now uses `transport`
    /// instead - see its doc comment). Kept as a general-purpose test
    /// double for exercising `requires_reconciliation()` independently of
    /// `is_specific()`.
    #[allow(dead_code)]
    pub(crate) fn ambiguous(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            kind: DaemonErrorKind::Ambiguous,
            specific: false,
        }
    }

    /// Same shape as `ambiguous`, but for a message this module authored
    /// itself about the transport to the *local* daemon (couldn't connect,
    /// didn't respond in time, ...), never text derived from anything a
    /// remote server sent. The `&'static str` bound is what makes this safe
    /// to also mark `specific`: it can never carry interpolated content, so
    /// callers can show it verbatim instead of it being silently replaced by
    /// a generic fallback that has nothing to do with the real cause.
    pub(crate) fn transport(message: &'static str) -> Self {
        Self {
            message: message.into(),
            kind: DaemonErrorKind::Ambiguous,
            specific: true,
        }
    }
}

impl fmt::Display for DaemonError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl Error for DaemonError {}

pub struct UnavailableDaemonClient {
    message: String,
}

impl UnavailableDaemonClient {
    pub fn new(error: DaemonError) -> Self {
        Self {
            message: error.to_string(),
        }
    }

    fn unavailable<T>(&self) -> Result<T, DaemonError> {
        Err(DaemonError::new(self.message.clone()))
    }
}

pub trait DaemonClient: Send + Sync {
    /// GET /v1/status
    fn status(&self) -> Result<DaemonStatus, DaemonError>;

    /// POST /v1/subscriptions/import with `{ "url": "…" }`.
    fn import_subscription(&self, url: &str) -> Result<DaemonStatus, DaemonError>;

    /// POST /v1/subscriptions/refresh. The saved source URL never leaves the daemon.
    fn refresh_subscription(&self) -> Result<DaemonStatus, DaemonError> {
        Err(DaemonError::new("Обновление подписки недоступно."))
    }

    /// GET /v1/subscriptions. The response intentionally contains no source URL.
    #[allow(dead_code)] // Wired into the desktop event loop in Task 6.
    fn subscriptions(&self) -> Result<DaemonSubscriptionList, DaemonError> {
        Err(DaemonError::new("Subscription profiles are unavailable."))
    }

    /// PUT /v1/subscriptions/{profile_id}/activate.
    #[allow(dead_code)] // Wired into the desktop event loop in Task 6.
    fn activate_subscription(&self, _profile_id: &str) -> Result<DaemonStatus, DaemonError> {
        Err(DaemonError::new("Subscription activation is unavailable."))
    }

    /// POST /v1/subscriptions/{profile_id}/refresh.
    #[allow(dead_code)] // Wired into the desktop event loop in Task 6.
    fn refresh_subscription_by_id(
        &self,
        _profile_id: &str,
    ) -> Result<DaemonSubscriptionList, DaemonError> {
        Err(DaemonError::new("Subscription refresh is unavailable."))
    }

    /// PUT /v1/subscriptions/{profile_id}/source.
    fn replace_subscription_source(
        &self,
        _profile_id: &str,
        _url: &str,
    ) -> Result<DaemonSubscriptionList, DaemonError> {
        Err(DaemonError::new(
            "Subscription source editing is unavailable.",
        ))
    }

    /// DELETE /v1/subscriptions/{profile_id}.
    #[allow(dead_code)] // Wired into the desktop event loop in Task 6.
    fn delete_subscription(&self, _profile_id: &str) -> Result<DaemonStatus, DaemonError> {
        Err(DaemonError::new("Subscription deletion is unavailable."))
    }

    /// POST /v1/connect. Group and node choices remain daemon DTO values; the UI
    /// never interprets Xray JSON or constructs proxy mappings.
    fn connect(&self) -> Result<DaemonStatus, DaemonError>;

    /// POST /v1/connect choosing the mode: `tun == false` connects through the system proxy
    /// without an administrator prompt (Windows).
    fn connect_with_tun(&self, tun: bool) -> Result<DaemonStatus, DaemonError> {
        let _ = tun;
        self.connect()
    }

    /// POST /v1/disconnect.
    fn disconnect(&self) -> Result<DaemonStatus, DaemonError>;

    /// POST /v1/shutdown (Quit): the daemon disconnects and lets the always-ready service stop
    /// the cores. Daemons without the route answer 404, which is fine: their cores stop with
    /// the channel.
    fn shutdown(&self) -> Result<(), DaemonError> {
        Ok(())
    }

    /// GET /v1/events?after=<u64>[&epoch=<known-epoch>].
    fn events(
        &self,
        after: u64,
        known_epoch: Option<&str>,
    ) -> Result<DaemonEventBatch, DaemonError>;

    /// GET /v1/diagnostics. Contains only bounded, redacted runtime data.
    fn diagnostics(&self) -> Result<DaemonDiagnostics, DaemonError> {
        Err(DaemonError::new("Диагностика недоступна."))
    }

    /// GET /v1/dashboard-url. The returned URL embeds the ephemeral Mihomo controller secret for
    /// this session; callers must open it directly (e.g. in the default browser) and must never
    /// log it, persist it, or surface it in diagnostics.
    fn dashboard_url(&self) -> Result<String, DaemonError> {
        Err(DaemonError::new("Панель Mihomo недоступна."))
    }

    /// GET /v1/session. Polled about once a second while connected, so a refusal (the session
    /// just ended) is not recorded in the low-level log.
    fn session(&self) -> Result<DaemonSession, DaemonError> {
        Err(DaemonError::new("Данные сессии недоступны."))
    }

    /// POST /v1/service/update: the always-ready service downloads and installs `version`
    /// (reusing a release staged by `stage_update_via_service`).
    fn update_via_service(&self, _version: &str) -> Result<(), DaemonError> {
        Err(DaemonError::new("Служба MultiCore недоступна."))
    }

    /// POST /v1/service/update/stage: the service only downloads and verifies `version`; the
    /// tunnel keeps running.
    fn stage_update_via_service(&self, _version: &str) -> Result<(), DaemonError> {
        Err(DaemonError::new("Служба MultiCore недоступна."))
    }

    /// GET /v1/catalog. IDs are opaque daemon values; labels are presentation data.
    fn catalog(&self) -> Result<DaemonCatalog, DaemonError>;

    /// POST /v1/latencies. Entries contain only opaque catalog IDs and safe probe outcomes.
    fn latencies(
        &self,
        _revision: Option<u64>,
        _group_ids: &[String],
    ) -> Result<DaemonLatencies, DaemonError> {
        Err(DaemonError::new("Проверка задержки недоступна."))
    }

    /// POST /v1/latencies for exactly these nodes (a right-click on one route row).
    fn node_latencies(
        &self,
        _revision: Option<u64>,
        _node_ids: &[String],
    ) -> Result<DaemonLatencies, DaemonError> {
        Err(DaemonError::new("Проверка задержки недоступна."))
    }

    /// GET /v1/group-icons/{icon_id}: the canonical PNG of a catalog group icon.
    fn group_icon(&self, _icon_id: &str) -> Result<Vec<u8>, DaemonError> {
        Err(DaemonError::new("Значок группы недоступен."))
    }

    /// PUT /v1/selections/{group_id} with `{ "revision", "node_id" }`.
    fn select_node(
        &self,
        group_id: &str,
        revision: u64,
        node_id: &str,
    ) -> Result<DaemonCatalog, DaemonError>;
}

impl DaemonClient for UnavailableDaemonClient {
    fn status(&self) -> Result<DaemonStatus, DaemonError> {
        self.unavailable()
    }

    fn import_subscription(&self, _url: &str) -> Result<DaemonStatus, DaemonError> {
        self.unavailable()
    }

    fn refresh_subscription(&self) -> Result<DaemonStatus, DaemonError> {
        self.unavailable()
    }

    fn subscriptions(&self) -> Result<DaemonSubscriptionList, DaemonError> {
        self.unavailable()
    }

    fn activate_subscription(&self, _profile_id: &str) -> Result<DaemonStatus, DaemonError> {
        self.unavailable()
    }

    fn refresh_subscription_by_id(
        &self,
        _profile_id: &str,
    ) -> Result<DaemonSubscriptionList, DaemonError> {
        self.unavailable()
    }

    fn delete_subscription(&self, _profile_id: &str) -> Result<DaemonStatus, DaemonError> {
        self.unavailable()
    }

    fn connect(&self) -> Result<DaemonStatus, DaemonError> {
        self.unavailable()
    }

    fn disconnect(&self) -> Result<DaemonStatus, DaemonError> {
        self.unavailable()
    }

    fn events(
        &self,
        _after: u64,
        _known_epoch: Option<&str>,
    ) -> Result<DaemonEventBatch, DaemonError> {
        self.unavailable()
    }

    fn catalog(&self) -> Result<DaemonCatalog, DaemonError> {
        self.unavailable()
    }

    fn latencies(
        &self,
        _revision: Option<u64>,
        _group_ids: &[String],
    ) -> Result<DaemonLatencies, DaemonError> {
        self.unavailable()
    }

    fn select_node(
        &self,
        _group_id: &str,
        _revision: u64,
        _node_id: &str,
    ) -> Result<DaemonCatalog, DaemonError> {
        self.unavailable()
    }
}

const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
// A refresh can perform a native request, a legacy fallback pair, an optional logo fetch,
// and a managed core shutdown before the daemon can answer. It must not inherit the short
// timeout used by local status and selector calls.
const SUBSCRIPTION_MUTATION_TIMEOUT: Duration = Duration::from_secs(150);
// Linux polkit may wait 300 s for the user, then each core start waits up to 20 s for
// readiness before queued selections are replayed.
const CONNECT_REQUEST_TIMEOUT: Duration = Duration::from_secs(390);
// Disconnect stops Mihomo and Xray through the elevated host, each with its own 5 s budget,
// and then reconciles the profile state.
const DISCONNECT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Clone)]
pub struct HttpDaemonClient {
    base_url: reqwest::Url,
    token: String,
    http: Client,
    subscription_mutation_timeout: Duration,
    connect_request_timeout: Duration,
}

impl HttpDaemonClient {
    /// The service downloads (minutes) before it answers, hence the long timeout.
    fn post_service_update(
        &self,
        path: &str,
        version: &str,
        success: u16,
    ) -> Result<(), DaemonError> {
        #[derive(serde::Serialize)]
        struct UpdateRequest<'a> {
            version: &'a str,
        }
        let response = self
            .http
            .post(self.endpoint(path))
            .bearer_auth(&self.token)
            .timeout(Duration::from_secs(15 * 60))
            .json(&UpdateRequest { version })
            .send()
            .map_err(map_error)?;
        if response.status().as_u16() == success {
            return Ok(());
        }
        let code = error_code(response);
        Err(
            DaemonError::definitive_http("Служба не смогла установить обновление.")
                .with_known_code(&code),
        )
    }

    pub fn new(base_url: impl Into<String>, token: impl Into<String>) -> Result<Self, DaemonError> {
        Self::with_timeouts(
            base_url,
            token,
            REQUEST_TIMEOUT,
            SUBSCRIPTION_MUTATION_TIMEOUT,
            CONNECT_REQUEST_TIMEOUT,
        )
    }

    fn with_timeouts(
        base_url: impl Into<String>,
        token: impl Into<String>,
        request_timeout: Duration,
        subscription_mutation_timeout: Duration,
        connect_request_timeout: Duration,
    ) -> Result<Self, DaemonError> {
        let token = token.into();
        if token.trim().is_empty() {
            return Err(DaemonError::new("Daemon access token is not configured."));
        }
        let base_url = parse_daemon_base_url(&base_url.into())?;
        let http = Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(2))
            .timeout(request_timeout)
            .build()
            .map_err(map_error)?;
        Ok(Self {
            base_url,
            token,
            http,
            subscription_mutation_timeout,
            connect_request_timeout,
        })
    }

    fn endpoint(&self, path: &str) -> reqwest::Url {
        let mut endpoint = self.base_url.clone();
        endpoint.set_path(path);
        endpoint.set_query(None);
        endpoint
    }

    #[allow(dead_code)] // Used by profile HTTP calls wired in Task 6.
    fn endpoint_with_segment(
        &self,
        prefix: &str,
        segment: &str,
        suffix: Option<&str>,
    ) -> Result<reqwest::Url, DaemonError> {
        let mut endpoint = self.endpoint(prefix);
        let mut segments = endpoint
            .path_segments_mut()
            .map_err(|()| DaemonError::new("Invalid background service address."))?;
        segments.pop_if_empty().push(segment);
        if let Some(suffix) = suffix {
            segments.push(suffix);
        }
        drop(segments);
        Ok(endpoint)
    }

    #[allow(dead_code)] // Used by profile HTTP calls wired in Task 6.
    fn send_profile_json<T: DeserializeOwned>(
        &self,
        request: reqwest::blocking::RequestBuilder,
        response_limit: usize,
    ) -> Result<T, DaemonError> {
        let response = request.bearer_auth(&self.token).send().map_err(map_error)?;
        match response.status() {
            reqwest::StatusCode::NOT_FOUND => Err(DaemonError::profile_http(
                DaemonErrorKind::ProfileNotFound,
                "Подписка не найдена. Обновите список подписок.",
            )),
            reqwest::StatusCode::CONFLICT => {
                let code = error_code(response);
                Err(DaemonError::profile_http(
                    DaemonErrorKind::ProfileConflict,
                    "Сейчас подписку изменить нельзя.",
                )
                .with_known_code(&code))
            }
            status if status.is_server_error() => {
                let code = error_code(response);
                Err(if code == "profile_mutation_ambiguous" {
                    DaemonError::transport(
                        "Изменение могло сохраниться. Обновляем состояние подписок.",
                    )
                } else {
                    DaemonError::profile_http(
                        DaemonErrorKind::ProfileServer,
                        "Фоновый сервис не смог изменить подписку.",
                    )
                    .with_known_code(&code)
                })
            }
            status if !status.is_success() => {
                let code = error_code(response);
                Err(DaemonError::definitive_http_with_status(status).with_known_code(&code))
            }
            _ => decode_bounded_json(response, response_limit),
        }
    }

    fn send_json<T: DeserializeOwned>(
        &self,
        request: reqwest::blocking::RequestBuilder,
        response_limit: usize,
    ) -> Result<T, DaemonError> {
        let response = request.bearer_auth(&self.token).send().map_err(map_error)?;
        if !response.status().is_success() {
            let status = response.status();
            let code = error_code(response);
            record_low_level(
                "daemon_status_error",
                format!("status={status} code={code:?}"),
            );
            return Err(DaemonError::definitive_http_with_status(status).with_known_code(&code));
        }
        decode_bounded_json(response, response_limit)
    }

    fn parse_status(
        &self,
        request: reqwest::blocking::RequestBuilder,
    ) -> Result<DaemonStatus, DaemonError> {
        self.send_json(request, STATUS_RESPONSE_LIMIT_BYTES)
    }

    fn post_latencies(&self, request: LatencyRequest<'_>) -> Result<DaemonLatencies, DaemonError> {
        let response = self
            .http
            .post(self.endpoint("/v1/latencies"))
            .bearer_auth(&self.token)
            .json(&request)
            .send()
            .map_err(map_error)?;
        if response.status() == reqwest::StatusCode::CONFLICT {
            let stale_revision =
                decode_bounded_json::<ApiErrorBody>(response, API_ERROR_RESPONSE_LIMIT_BYTES)
                    .is_ok_and(|body| body.code == "stale_revision");
            return Err(if stale_revision {
                DaemonError::stale_revision()
            } else {
                DaemonError::definitive_http("Не удалось проверить задержку.")
            });
        }
        if !response.status().is_success() {
            return Err(DaemonError::definitive_http(
                "Фоновый сервис отклонил проверку задержки.",
            ));
        }
        decode_bounded_json(response, LATENCIES_RESPONSE_LIMIT_BYTES)
    }
}

fn parse_daemon_base_url(value: &str) -> Result<reqwest::Url, DaemonError> {
    let invalid = || DaemonError::new("Некорректный адрес фонового сервиса.");
    let url = reqwest::Url::parse(value.trim()).map_err(|_| invalid())?;
    if url.scheme() != "http"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.path() != "/"
    {
        return Err(invalid());
    }
    let host = url.host_str().ok_or_else(invalid)?;
    let address_text = host
        .strip_prefix('[')
        .and_then(|host| host.strip_suffix(']'))
        .unwrap_or(host);
    let ip = address_text.parse::<IpAddr>().map_err(|_| invalid())?;
    if !ip.is_loopback() {
        return Err(invalid());
    }
    let port = url.port_or_known_default().ok_or_else(invalid)?;
    let _address = SocketAddr::new(ip, port);
    Ok(url)
}

fn decode_bounded_json<T: DeserializeOwned>(
    response: reqwest::blocking::Response,
    response_limit: usize,
) -> Result<T, DaemonError> {
    if response
        .content_length()
        .is_some_and(|length| length > response_limit as u64)
    {
        return Err(response_too_large());
    }

    let mut bytes = Vec::with_capacity(
        response
            .content_length()
            .unwrap_or_default()
            .min(response_limit as u64) as usize,
    );
    response
        .take(response_limit as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| {
            record_low_level("daemon_response_read_failed", &error);
            DaemonError::transport("Не удалось прочитать ответ фонового сервиса.")
        })?;
    if bytes.len() > response_limit {
        return Err(response_too_large());
    }
    serde_json::from_slice(&bytes).map_err(|error| {
        let snippet: String = String::from_utf8_lossy(&bytes[..bytes.len().min(200)]).into_owned();
        record_low_level(
            "daemon_response_decode_failed",
            format!("{error}; body={snippet:?}"),
        );
        DaemonError::transport("Фоновый сервис вернул некорректный ответ.")
    })
}

fn response_too_large() -> DaemonError {
    DaemonError::transport("Ответ фонового сервиса превышает допустимый размер.")
}

#[derive(Serialize)]
struct ConnectRequest {
    tun: bool,
}

#[derive(Serialize)]
struct ImportRequest<'a> {
    url: &'a str,
}

#[derive(Serialize)]
struct SelectionRequest<'a> {
    revision: u64,
    node_id: &'a str,
}

#[derive(Serialize)]
struct LatencyRequest<'a> {
    revision: Option<u64>,
    group_ids: &'a [String],
    #[serde(skip_serializing_if = "<[String]>::is_empty")]
    node_ids: &'a [String],
}

#[derive(Deserialize)]
struct ApiErrorBody {
    #[serde(default)]
    code: String,
}

#[derive(Deserialize)]
struct DashboardUrlResponse {
    url: String,
}

/// Local texts for daemon error codes. The daemon's own message is never
/// displayed, so provider URLs or tokens can never be reflected into the UI.
fn known_error_message(code: &str) -> Option<&'static str> {
    Some(match code {
        "profile_busy" => "Отключитесь перед изменением подписки.",
        "profile_limit" => "Достигнут предел: не больше 16 подписок. Удалите ненужную и повторите.",
        "subscription_unreachable" => {
            "Сервер подписки недоступен. Проверьте подключение к интернету и повторите попытку."
        }
        "subscription_timeout" => {
            "Сервер подписки не ответил вовремя. Повторите попытку чуть позже."
        }
        "subscription_tls" => {
            "Сертификат сервера подписки не принят. Проверьте ссылку или обратитесь к провайдеру."
        }
        "subscription_redirect" => {
            "Сервер подписки перенаправил запрос небезопасно или слишком много раз."
        }
        "subscription_forbidden" => {
            "Сервер подписки отказал в доступе. Проверьте ссылку и срок действия подписки."
        }
        "subscription_not_found" => {
            "Подписка не найдена. Проверьте, что ссылка скопирована полностью."
        }
        "subscription_rate_limited" => {
            "Сервер подписки временно ограничил запросы. Повторите попытку через минуту."
        }
        "subscription_server_error" => {
            "Сервер подписки временно не работает. Повторите попытку позже."
        }
        "subscription_http_status" => "Сервер подписки ответил ошибкой.",
        "subscription_not_multicore" => {
            "По ссылке пришла веб-страница или список ссылок, а не подписка MultiCore. Скопируйте ссылку подписки для приложения у провайдера."
        }
        "subscription_invalid" => {
            "Провайдер прислал повреждённую или неподдерживаемую конфигурацию."
        }
        "subscription_too_large" => "Ответ сервера подписки слишком большой.",
        "subscription_storage" => {
            "Не удалось сохранить подписку на диск. Проверьте свободное место."
        }
        "subscription_prepare_failed" => {
            "Подписка загружена, но подготовить её к запуску не удалось. Проверьте сетевое подключение."
        }
        "not_connected" => "Подключитесь, чтобы открыть панель Mihomo.",
        "dashboard_unavailable" => "Панель Mihomo недоступна в этой сборке.",
        "service_busy" => "MultiCore уже запущен у другого пользователя.",
        "service_update_failed" => "Служба не смогла загрузить или проверить обновление.",
        "service_unavailable" => "Служба MultiCore недоступна.",
        _ => return None,
    })
}

fn error_code(response: reqwest::blocking::Response) -> String {
    decode_bounded_json::<ApiErrorBody>(response, API_ERROR_RESPONSE_LIMIT_BYTES)
        .map(|body| body.code)
        .unwrap_or_default()
}

impl DaemonClient for HttpDaemonClient {
    fn status(&self) -> Result<DaemonStatus, DaemonError> {
        self.parse_status(self.http.get(self.endpoint("/v1/status")))
    }

    fn import_subscription(&self, url: &str) -> Result<DaemonStatus, DaemonError> {
        self.parse_status(
            self.http
                .post(self.endpoint("/v1/subscriptions/import"))
                .timeout(self.subscription_mutation_timeout)
                .json(&ImportRequest { url }),
        )
    }

    fn refresh_subscription(&self) -> Result<DaemonStatus, DaemonError> {
        self.send_profile_json(
            self.http
                .post(self.endpoint("/v1/subscriptions/refresh"))
                .timeout(self.subscription_mutation_timeout),
            STATUS_RESPONSE_LIMIT_BYTES,
        )
    }

    fn subscriptions(&self) -> Result<DaemonSubscriptionList, DaemonError> {
        self.send_profile_json(
            self.http.get(self.endpoint("/v1/subscriptions")),
            SUBSCRIPTIONS_RESPONSE_LIMIT_BYTES,
        )
    }

    fn activate_subscription(&self, profile_id: &str) -> Result<DaemonStatus, DaemonError> {
        let endpoint =
            self.endpoint_with_segment("/v1/subscriptions/", profile_id, Some("activate"))?;
        // With the always-ready service a switch while connected includes a disconnect and a
        // full reconnect on the new generation.
        self.send_profile_json(
            self.http
                .put(endpoint)
                .timeout(self.subscription_mutation_timeout),
            STATUS_RESPONSE_LIMIT_BYTES,
        )
    }

    fn refresh_subscription_by_id(
        &self,
        profile_id: &str,
    ) -> Result<DaemonSubscriptionList, DaemonError> {
        let endpoint =
            self.endpoint_with_segment("/v1/subscriptions/", profile_id, Some("refresh"))?;
        self.send_profile_json(
            self.http
                .post(endpoint)
                .timeout(self.subscription_mutation_timeout),
            SUBSCRIPTIONS_RESPONSE_LIMIT_BYTES,
        )
    }

    fn replace_subscription_source(
        &self,
        profile_id: &str,
        url: &str,
    ) -> Result<DaemonSubscriptionList, DaemonError> {
        let endpoint =
            self.endpoint_with_segment("/v1/subscriptions/", profile_id, Some("source"))?;
        self.send_profile_json(
            self.http
                .put(endpoint)
                .timeout(self.subscription_mutation_timeout)
                .json(&ImportRequest { url }),
            SUBSCRIPTIONS_RESPONSE_LIMIT_BYTES,
        )
    }

    fn delete_subscription(&self, profile_id: &str) -> Result<DaemonStatus, DaemonError> {
        let endpoint = self.endpoint_with_segment("/v1/subscriptions/", profile_id, None)?;
        self.send_profile_json(
            self.http
                .delete(endpoint)
                .timeout(self.subscription_mutation_timeout),
            STATUS_RESPONSE_LIMIT_BYTES,
        )
    }

    fn connect(&self) -> Result<DaemonStatus, DaemonError> {
        self.connect_with_tun(true)
    }

    fn connect_with_tun(&self, tun: bool) -> Result<DaemonStatus, DaemonError> {
        let request = self
            .http
            .post(self.endpoint("/v1/connect"))
            .timeout(self.connect_request_timeout);
        // TUN is the daemon's default, so only the proxy mode sends a body.
        let request = if tun {
            request
        } else {
            request.json(&ConnectRequest { tun })
        };
        self.parse_status(request)
    }

    fn disconnect(&self) -> Result<DaemonStatus, DaemonError> {
        self.parse_status(
            self.http
                .post(self.endpoint("/v1/disconnect"))
                .timeout(DISCONNECT_REQUEST_TIMEOUT),
        )
    }

    fn events(
        &self,
        after: u64,
        known_epoch: Option<&str>,
    ) -> Result<DaemonEventBatch, DaemonError> {
        let mut request = self
            .http
            .get(self.endpoint("/v1/events"))
            .bearer_auth(&self.token)
            .query(&[("after", after)]);
        if let Some(epoch) = known_epoch {
            request = request.query(&[("epoch", epoch)]);
        }
        self.send_json(request, EVENTS_RESPONSE_LIMIT_BYTES)
    }

    fn diagnostics(&self) -> Result<DaemonDiagnostics, DaemonError> {
        self.send_json(
            self.http.get(self.endpoint("/v1/diagnostics")),
            DIAGNOSTICS_RESPONSE_LIMIT_BYTES,
        )
    }

    fn shutdown(&self) -> Result<(), DaemonError> {
        let response = self
            .http
            .post(self.endpoint("/v1/shutdown"))
            .timeout(DISCONNECT_REQUEST_TIMEOUT)
            .bearer_auth(&self.token)
            .send()
            .map_err(map_error)?;
        if response.status().is_success() {
            Ok(())
        } else {
            Err(DaemonError::definitive_http(
                "Не удалось остановить ядра службы.",
            ))
        }
    }

    fn dashboard_url(&self) -> Result<String, DaemonError> {
        let response = self
            .http
            .get(self.endpoint("/v1/dashboard-url"))
            .bearer_auth(&self.token)
            .send()
            .map_err(map_error)?;
        if !response.status().is_success() {
            let code = error_code(response);
            return Err(
                DaemonError::definitive_http("Панель Mihomo сейчас недоступна.")
                    .with_known_code(&code),
            );
        }
        let response: DashboardUrlResponse =
            decode_bounded_json(response, DASHBOARD_URL_RESPONSE_LIMIT_BYTES)?;
        Ok(response.url)
    }

    fn session(&self) -> Result<DaemonSession, DaemonError> {
        let response = self
            .http
            .get(self.endpoint("/v1/session"))
            .bearer_auth(&self.token)
            .send()
            .map_err(map_error)?;
        if !response.status().is_success() {
            let code = error_code(response);
            return Err(
                DaemonError::definitive_http("Данные сессии недоступны.").with_known_code(&code)
            );
        }
        decode_bounded_json(response, SESSION_RESPONSE_LIMIT_BYTES)
    }

    fn update_via_service(&self, version: &str) -> Result<(), DaemonError> {
        self.post_service_update("/v1/service/update", version, 202)
    }

    fn stage_update_via_service(&self, version: &str) -> Result<(), DaemonError> {
        self.post_service_update("/v1/service/update/stage", version, 200)
    }

    fn catalog(&self) -> Result<DaemonCatalog, DaemonError> {
        self.send_json(
            self.http.get(self.endpoint("/v1/catalog")),
            CATALOG_RESPONSE_LIMIT_BYTES,
        )
    }

    fn latencies(
        &self,
        revision: Option<u64>,
        group_ids: &[String],
    ) -> Result<DaemonLatencies, DaemonError> {
        self.post_latencies(LatencyRequest {
            revision,
            group_ids,
            node_ids: &[],
        })
    }

    fn node_latencies(
        &self,
        revision: Option<u64>,
        node_ids: &[String],
    ) -> Result<DaemonLatencies, DaemonError> {
        self.post_latencies(LatencyRequest {
            revision,
            group_ids: &[],
            node_ids,
        })
    }

    fn group_icon(&self, icon_id: &str) -> Result<Vec<u8>, DaemonError> {
        let endpoint = self.endpoint_with_segment("/v1/group-icons/", icon_id, None)?;
        let response = self
            .http
            .get(endpoint)
            .bearer_auth(&self.token)
            .send()
            .map_err(map_error)?;
        if !response.status().is_success() {
            return Err(DaemonError::definitive_http("Значок группы недоступен."));
        }
        if response
            .content_length()
            .is_some_and(|length| length > GROUP_ICON_RESPONSE_LIMIT_BYTES as u64)
        {
            return Err(response_too_large());
        }
        let mut bytes = Vec::new();
        response
            .take(GROUP_ICON_RESPONSE_LIMIT_BYTES as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| DaemonError::transport("Не удалось прочитать ответ фонового сервиса."))?;
        if bytes.len() > GROUP_ICON_RESPONSE_LIMIT_BYTES {
            return Err(response_too_large());
        }
        Ok(bytes)
    }

    fn select_node(
        &self,
        group_id: &str,
        revision: u64,
        node_id: &str,
    ) -> Result<DaemonCatalog, DaemonError> {
        let mut endpoint = self.endpoint("/v1/selections/");
        endpoint
            .path_segments_mut()
            .map_err(|()| DaemonError::new("Некорректный адрес фонового сервиса."))?
            .pop_if_empty()
            .push(group_id);
        let response = self
            .http
            .put(endpoint)
            .bearer_auth(&self.token)
            .json(&SelectionRequest { revision, node_id })
            .send()
            .map_err(map_error)?;
        if response.status() == reqwest::StatusCode::CONFLICT {
            let stale_revision =
                decode_bounded_json::<ApiErrorBody>(response, API_ERROR_RESPONSE_LIMIT_BYTES)
                    .is_ok_and(|body| body.code == "stale_revision");
            return Err(if stale_revision {
                DaemonError::stale_revision()
            } else {
                DaemonError::definitive_http("Не удалось изменить маршрут.")
            });
        }
        if !response.status().is_success() {
            return Err(DaemonError::definitive_http(
                "Фоновый сервис отклонил изменение маршрута.",
            ));
        }
        decode_bounded_json(response, CATALOG_RESPONSE_LIMIT_BYTES)
    }
}

fn map_error(error: reqwest::Error) -> DaemonError {
    record_low_level("daemon_request_failed", &error);
    if error.is_timeout() {
        DaemonError::transport("Фоновый сервис не ответил вовремя.")
    } else if error.is_connect() {
        DaemonError::transport("Фоновый сервис не запущен.")
    } else {
        DaemonError::transport("Не удалось выполнить запрос к фоновому сервису.")
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use std::sync::Mutex;

    use super::*;

    pub struct MockDaemonClient {
        pub calls: Mutex<Vec<String>>,
        pub status: DaemonStatus,
        pub events: Vec<DaemonEvent>,
        pub event_epoch: String,
        pub catalog: DaemonCatalog,
        pub fail_import: bool,
        pub fail_events: bool,
        pub fail_catalog: bool,
        pub latencies: DaemonLatencies,
        pub fail_latencies: bool,
        #[allow(dead_code)]
        pub subscriptions: DaemonSubscriptionList,
        pub dashboard_url: Option<String>,
    }

    impl MockDaemonClient {
        pub fn ready() -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                status: DaemonStatus {
                    state: "ready".into(),
                    profile: Some("Тестовый".into()),
                    current_node: Some("Авто".into()),
                    ..DaemonStatus::default()
                },
                events: Vec::new(),
                event_epoch: "test-epoch".into(),
                catalog: DaemonCatalog::default(),
                fail_import: false,
                fail_events: false,
                fail_catalog: false,
                latencies: DaemonLatencies::default(),
                fail_latencies: false,
                subscriptions: DaemonSubscriptionList::default(),
                dashboard_url: None,
            }
        }

        fn record(&self, call: impl Into<String>) {
            self.calls.lock().expect("mock call lock").push(call.into());
        }
    }

    impl DaemonClient for MockDaemonClient {
        fn status(&self) -> Result<DaemonStatus, DaemonError> {
            self.record("GET /v1/status");
            Ok(self.status.clone())
        }

        fn import_subscription(&self, url: &str) -> Result<DaemonStatus, DaemonError> {
            self.record(format!("POST /v1/subscriptions/import {url}"));
            if self.fail_import {
                Err(DaemonError::new(
                    "unsafe upstream token=secret url=https://private.invalid",
                ))
            } else {
                Ok(self.status.clone())
            }
        }

        fn refresh_subscription(&self) -> Result<DaemonStatus, DaemonError> {
            self.record("POST /v1/subscriptions/refresh");
            Ok(self.status.clone())
        }

        fn subscriptions(&self) -> Result<DaemonSubscriptionList, DaemonError> {
            self.record("GET /v1/subscriptions");
            Ok(self.subscriptions.clone())
        }

        fn activate_subscription(&self, _profile_id: &str) -> Result<DaemonStatus, DaemonError> {
            self.record("PUT /v1/subscriptions/{opaque}/activate");
            Ok(self.status.clone())
        }

        fn refresh_subscription_by_id(
            &self,
            _profile_id: &str,
        ) -> Result<DaemonSubscriptionList, DaemonError> {
            self.record("POST /v1/subscriptions/{opaque}/refresh");
            Ok(self.subscriptions.clone())
        }

        fn delete_subscription(&self, _profile_id: &str) -> Result<DaemonStatus, DaemonError> {
            self.record("DELETE /v1/subscriptions/{opaque}");
            Ok(self.status.clone())
        }

        fn connect(&self) -> Result<DaemonStatus, DaemonError> {
            self.record("POST /v1/connect");
            Ok(self.status.clone())
        }

        fn disconnect(&self) -> Result<DaemonStatus, DaemonError> {
            self.record("POST /v1/disconnect");
            Ok(self.status.clone())
        }

        fn events(
            &self,
            after: u64,
            known_epoch: Option<&str>,
        ) -> Result<DaemonEventBatch, DaemonError> {
            let epoch = known_epoch
                .map(|epoch| format!(" epoch={epoch}"))
                .unwrap_or_default();
            self.record(format!("GET /v1/events after={after}{epoch}"));
            if self.fail_events {
                Err(DaemonError::new("unsafe upstream details"))
            } else {
                Ok(DaemonEventBatch {
                    epoch: self.event_epoch.clone(),
                    events: self.events.clone(),
                })
            }
        }

        fn catalog(&self) -> Result<DaemonCatalog, DaemonError> {
            self.record("GET /v1/catalog");
            if self.fail_catalog {
                Err(DaemonError::new("unsafe upstream catalog details"))
            } else {
                Ok(self.catalog.clone())
            }
        }

        fn latencies(
            &self,
            _revision: Option<u64>,
            group_ids: &[String],
        ) -> Result<DaemonLatencies, DaemonError> {
            self.record(format!("POST /v1/latencies groups={}", group_ids.join(",")));
            if self.fail_latencies {
                Err(DaemonError::new("unsafe upstream latency details"))
            } else {
                Ok(self.latencies.clone())
            }
        }

        fn select_node(
            &self,
            _group_id: &str,
            _revision: u64,
            _node_id: &str,
        ) -> Result<DaemonCatalog, DaemonError> {
            self.record("PUT /v1/selections/{opaque}");
            Ok(self.catalog.clone())
        }

        fn dashboard_url(&self) -> Result<String, DaemonError> {
            self.record("GET /v1/dashboard-url");
            self.dashboard_url
                .clone()
                .ok_or_else(|| DaemonError::new("Подключитесь, чтобы открыть панель Mihomo."))
        }
    }
}

#[cfg(test)]
mod http_tests {
    use std::{
        ffi::OsString,
        io::{Read, Write},
        net::TcpListener,
        process::Command,
        sync::{
            Arc, Mutex,
            atomic::{AtomicBool, Ordering},
        },
        thread,
        time::{Duration, Instant},
    };

    use super::*;

    #[test]
    fn service_updates_are_posted_and_busy_is_explained() {
        for (status, body, expected) in [
            ("202 Accepted", "", None),
            (
                "409 Conflict",
                r#"{"code":"service_busy","message":"x"}"#,
                Some("MultiCore уже запущен у другого пользователя."),
            ),
        ] {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind update server");
            let address = listener.local_addr().expect("update server address");
            let server = thread::spawn(move || {
                let (mut stream, _) = listener.accept().expect("accept update request");
                let mut request = Vec::new();
                let mut buffer = [0_u8; 2048];
                while !String::from_utf8_lossy(&request).contains("\"version\"") {
                    let read = stream.read(&mut buffer).expect("read update request");
                    if read == 0 {
                        break;
                    }
                    request.extend_from_slice(&buffer[..read]);
                }
                write!(
                    stream,
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .expect("write update response");
                String::from_utf8_lossy(&request).into_owned()
            });
            let client = HttpDaemonClient::new(format!("http://{address}"), "token")
                .expect("loopback client");
            let result = client.update_via_service("1.2.0");
            let request = server.join().expect("update server");
            assert!(request.starts_with("POST /v1/service/update "), "{request}");
            assert!(request.contains(r#"{"version":"1.2.0"}"#));
            match expected {
                None => assert!(result.is_ok()),
                Some(message) => assert_eq!(result.unwrap_err().to_string(), message),
            }
        }
    }

    #[test]
    fn staging_an_update_is_posted_separately_and_failures_are_explained() {
        for (status, body, expected) in [
            ("200 OK", "", None),
            (
                "502 Bad Gateway",
                r#"{"code":"service_update_failed","message":"x"}"#,
                Some("Служба не смогла загрузить или проверить обновление."),
            ),
        ] {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind stage server");
            let address = listener.local_addr().expect("stage server address");
            let server = thread::spawn(move || {
                let (mut stream, _) = listener.accept().expect("accept stage request");
                let mut request = Vec::new();
                let mut buffer = [0_u8; 2048];
                while !String::from_utf8_lossy(&request).contains("\"version\"") {
                    let read = stream.read(&mut buffer).expect("read stage request");
                    if read == 0 {
                        break;
                    }
                    request.extend_from_slice(&buffer[..read]);
                }
                write!(
                    stream,
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .expect("write stage response");
                String::from_utf8_lossy(&request).into_owned()
            });
            let client = HttpDaemonClient::new(format!("http://{address}"), "token")
                .expect("loopback client");
            let result = client.stage_update_via_service("1.2.0");
            let request = server.join().expect("stage server");
            assert!(
                request.starts_with("POST /v1/service/update/stage "),
                "{request}"
            );
            assert!(request.contains(r#"{"version":"1.2.0"}"#));
            match expected {
                None => assert!(result.is_ok()),
                Some(message) => assert_eq!(result.unwrap_err().to_string(), message),
            }
        }
    }

    #[test]
    fn subscription_info_deserializes_legacy_payload_with_safe_defaults() {
        let info: DaemonSubscriptionInfo = serde_json::from_value(serde_json::json!({
            "source_name": "legacy.example",
            "downloaded_bytes": 7,
            "total_bytes": 10,
            "expires_at_unix": null,
            "updated_at_unix": 123,
            "refresh_available": true
        }))
        .expect("legacy subscription DTO");

        assert_eq!(info.display_name, "");
        assert_eq!(info.uploaded_bytes, None);
        assert_eq!(info.used_bytes, None);
        assert_eq!(info.refresh_interval_secs, None);
        assert_eq!(info.announcement_text, None);
        assert_eq!(info.announcement_action_label, None);
        assert_eq!(info.announcement_tone, None);
        assert!(!info.home_available);
        assert!(!info.support_available);
        assert!(!info.announcement_action_available);
        assert_eq!(info.service_logo_path, None);
        assert!(info.refresh_available);
    }

    #[test]
    fn subscription_info_deserializes_safe_metadata_without_any_url_field() {
        let json = serde_json::json!({
            "source_name": "fallback.example",
            "display_name": "Provider",
            "uploaded_bytes": 4,
            "downloaded_bytes": 8,
            "used_bytes": 12,
            "total_bytes": 100,
            "expires_at_unix": 456,
            "updated_at_unix": 123,
            "refresh_interval_secs": 900,
            "announcement_text": "Maintenance",
            "announcement_action_label": "Details",
            "announcement_tone": "blue",
            "home_available": true,
            "support_available": true,
            "announcement_action_available": true,
            "service_logo_path": "C:\\\\cache\\\\provider.png",
            "refresh_available": true
        });
        let info: DaemonSubscriptionInfo =
            serde_json::from_value(json.clone()).expect("current subscription DTO");

        assert_eq!(info.display_name, "Provider");
        assert_eq!(info.used_bytes, Some(12));
        assert_eq!(info.announcement_tone.as_deref(), Some("blue"));
        assert!(info.home_available);
        assert!(info.support_available);
        assert!(info.announcement_action_available);
        assert!(info.service_logo_path.is_some());
        assert!(json.get("url").is_none());
    }

    #[test]
    fn profile_list_decodes_exact_safe_dto_without_a_source_url_field() {
        let value = serde_json::json!({
            "active_profile_id": "profile-a",
            "profiles": [{
                "id": "profile-a",
                "active": true,
                "available": true,
                "display_name": "Work",
                "used_bytes": 1,
                "total_bytes": 2,
                "expires_at_unix": null,
                "service_logo_path": null
            }]
        });
        let list: DaemonSubscriptionList =
            serde_json::from_value(value.clone()).expect("safe profile list DTO");

        assert_eq!(list.active_profile_id.as_deref(), Some("profile-a"));
        assert_eq!(list.profiles[0].display_name, "Work");
        assert_eq!(value["profiles"][0].as_object().expect("profile").len(), 8);
        assert!(value["profiles"][0].get("url").is_none());
        assert!(!format!("{list:?}").contains("https://"));
    }

    fn start_server(request_count: usize) -> (String, thread::JoinHandle<Vec<String>>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test HTTP server");
        let address = listener.local_addr().expect("test server address");
        let handle = thread::spawn(move || {
            let mut requests = Vec::with_capacity(request_count);
            for _ in 0..request_count {
                let (mut stream, _) = listener.accept().expect("accept test request");
                let mut bytes = Vec::new();
                let mut buffer = [0_u8; 1024];
                loop {
                    let read = stream.read(&mut buffer).expect("read test request");
                    if read == 0 {
                        break;
                    }
                    bytes.extend_from_slice(&buffer[..read]);
                    if bytes.windows(4).any(|window| window == b"\r\n\r\n") {
                        break;
                    }
                }
                let request = String::from_utf8_lossy(&bytes).into_owned();
                let is_events = request.starts_with("GET /v1/events?");
                let is_catalog = request.starts_with("GET /v1/catalog ");
                let is_latencies = request.starts_with("POST /v1/latencies ");
                let is_diagnostics = request.starts_with("GET /v1/diagnostics ");
                let is_subscriptions = request.starts_with("GET /v1/subscriptions ")
                    || request.starts_with("POST /v1/subscriptions/");
                requests.push(request);

                let body = if is_subscriptions {
                    r#"{"active_profile_id":"profile-a","profiles":[{"id":"profile-a","active":true,"available":true,"display_name":"Work","used_bytes":1,"total_bytes":2,"expires_at_unix":null,"service_logo_path":null}]}"#
                } else if is_latencies {
                    r#"{"entries":[{"group_id":"group:auto","node_id":"node:a","latency_ms":42,"status":"ok"},{"group_id":"group:auto","node_id":"node:b","latency_ms":null,"status":"timeout"}]}"#
                } else if is_diagnostics {
                    r#"{"xray":"ready","mihomo":"ready","tun":"ready","logs":[{"id":7,"timestamp":"11","component":"mihomo","level":"error","message":"safe core line"}],"mappings":[{"proxy_name":"Sweden [se]","address":"127.0.0.1:31006","xray_label":"Sweden [se]"}]}"#
                } else if is_events {
                    r#"{"epoch":"boot-a","events":[{"id":43,"timestamp":"1","level":"info","message":"Подключение установлено."}]}"#
                } else if is_catalog {
                    r#"{"revision":17,"groups":[{"id":"group:auto","label":"Авто","selected":true,"nodes":[{"id":"node:a","label":"Север","selected":true,"delay_ms":42},{"id":"node:b","label":"Резерв","selected":false}]}]}"#
                } else {
                    r#"{"state":"ready","profile":"Test","current_node":"Auto","message":null,"degraded":false}"#
                };
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                )
                .expect("write test response");
            }
            requests
        });
        (format!("http://{address}"), handle)
    }

    fn start_single_response_server(body: &'static str) -> (String, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test HTTP server");
        let address = listener.local_addr().expect("test server address");
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept test request");
            let mut buffer = [0_u8; 2048];
            let _ = stream.read(&mut buffer).expect("read test request");
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .expect("write test response");
        });
        (format!("http://{address}"), handle)
    }

    fn start_selection_server(
        status: &'static str,
        body: &'static str,
    ) -> (String, thread::JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind selection HTTP server");
        let address = listener.local_addr().expect("selection server address");
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept selection request");
            let mut bytes = Vec::new();
            let mut buffer = [0_u8; 1024];
            let mut expected_len = None;
            loop {
                let read = stream.read(&mut buffer).expect("read selection request");
                if read == 0 {
                    break;
                }
                bytes.extend_from_slice(&buffer[..read]);
                if expected_len.is_none()
                    && let Some(headers_end) = bytes.windows(4).position(|part| part == b"\r\n\r\n")
                {
                    let headers = String::from_utf8_lossy(&bytes[..headers_end]);
                    let content_length = headers
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length: ")
                                .map(str::to_owned)
                        })
                        .and_then(|value| value.parse::<usize>().ok())
                        .unwrap_or_default();
                    expected_len = Some(headers_end + 4 + content_length);
                }
                if expected_len.is_some_and(|length| bytes.len() >= length) {
                    break;
                }
            }
            write!(
                stream,
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .expect("write selection response");
            String::from_utf8(bytes).expect("selection request is HTTP text")
        });
        (format!("http://{address}"), handle)
    }

    fn start_observing_server(
        bind_address: &str,
        response: Vec<u8>,
        stop: Arc<AtomicBool>,
    ) -> (String, thread::JoinHandle<Option<String>>) {
        let listener = TcpListener::bind(bind_address).expect("bind observing HTTP server");
        listener
            .set_nonblocking(true)
            .expect("make observing server nonblocking");
        let address = listener.local_addr().expect("observing server address");
        let handle = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(15);
            while !stop.load(Ordering::SeqCst) && Instant::now() < deadline {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        stream
                            .set_nonblocking(false)
                            .expect("make observed stream blocking");
                        stream
                            .set_read_timeout(Some(Duration::from_secs(2)))
                            .expect("bound observed request read");
                        stream
                            .set_write_timeout(Some(Duration::from_secs(2)))
                            .expect("bound observing response write");
                        let mut bytes = Vec::new();
                        let mut buffer = [0_u8; 2048];
                        loop {
                            match stream.read(&mut buffer) {
                                Ok(0) => break,
                                Ok(read) => {
                                    bytes.extend_from_slice(&buffer[..read]);
                                    if bytes.windows(4).any(|window| window == b"\r\n\r\n") {
                                        break;
                                    }
                                }
                                Err(error)
                                    if matches!(
                                        error.kind(),
                                        std::io::ErrorKind::WouldBlock
                                            | std::io::ErrorKind::TimedOut
                                    ) =>
                                {
                                    break;
                                }
                                Err(_) => break,
                            }
                        }
                        let _ = stream.write_all(&response);
                        return Some(String::from_utf8_lossy(&bytes).into_owned());
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("accept observed request: {error}"),
                }
            }
            None
        });
        (format!("http://{address}"), handle)
    }

    fn start_raw_response_server(response: Vec<u8>) -> (String, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind raw HTTP server");
        let address = listener.local_addr().expect("raw server address");
        let handle = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept raw request");
            let mut buffer = [0_u8; 2048];
            let _ = stream.read(&mut buffer).expect("read raw request");
            stream.write_all(&response).expect("write raw response");
        });
        (format!("http://{address}"), handle)
    }

    struct EnvironmentGuard {
        values: Vec<(&'static str, Option<OsString>)>,
    }

    impl EnvironmentGuard {
        fn set(values: &[(&'static str, &str)]) -> Self {
            let previous = values
                .iter()
                .map(|(name, _)| (*name, std::env::var_os(name)))
                .collect();
            for (name, value) in values {
                // SAFETY: the proxy regression runs in an isolated child process and holds
                // ENVIRONMENT_LOCK for the entire mutation lifetime.
                unsafe { std::env::set_var(name, value) };
            }
            Self { values: previous }
        }
    }

    impl Drop for EnvironmentGuard {
        fn drop(&mut self) {
            for (name, value) in &self.values {
                // SAFETY: the isolated child still holds ENVIRONMENT_LOCK here.
                unsafe {
                    if let Some(value) = value {
                        std::env::set_var(name, value);
                    } else {
                        std::env::remove_var(name);
                    }
                }
            }
        }
    }

    static ENVIRONMENT_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn every_v1_request_sends_the_configured_bearer_token() {
        let (base_url, server) = start_server(13);
        let client = HttpDaemonClient::new(base_url, "test-daemon-token")
            .expect("construct authenticated client");

        client.status().expect("status request");
        client
            .import_subscription("https://subscription.invalid/value")
            .expect("import request");
        client
            .refresh_subscription()
            .expect("refresh subscription request");
        client.subscriptions().expect("subscriptions request");
        client
            .activate_subscription("profile-a")
            .expect("activate subscription request");
        client
            .refresh_subscription_by_id("profile-a")
            .expect("targeted subscription refresh request");
        client
            .delete_subscription("profile-a")
            .expect("delete subscription request");
        client.connect().expect("connect request");
        client.disconnect().expect("disconnect request");
        client.events(0, None).expect("events request");
        client.catalog().expect("catalog request");
        client
            .latencies(Some(17), &["opaque:selected".to_owned()])
            .expect("latency request");
        client.diagnostics().expect("diagnostics request");

        let requests = server.join().expect("join test HTTP server");
        assert_eq!(requests.len(), 13);
        for request in requests {
            assert!(
                request
                    .to_ascii_lowercase()
                    .contains("authorization: bearer test-daemon-token\r\n"),
                "request omitted the Bearer header: {request}"
            );
        }
    }

    #[test]
    fn diagnostics_request_decodes_owned_health_and_core_logs() {
        let (base_url, server) = start_server(1);
        let client = HttpDaemonClient::new(base_url, "token").expect("authenticated client");

        let diagnostics = client.diagnostics().expect("diagnostics request");

        let requests = server.join().expect("join test HTTP server");
        assert!(requests[0].starts_with("GET /v1/diagnostics HTTP/1.1\r\n"));
        assert_eq!(diagnostics.xray, "ready");
        assert_eq!(diagnostics.mihomo, "ready");
        assert_eq!(diagnostics.tun, "ready");
        assert_eq!(diagnostics.logs.len(), 1);
        assert_eq!(diagnostics.logs[0].component, "mihomo");
        assert_eq!(diagnostics.logs[0].message, "safe core line");
        assert_eq!(diagnostics.mappings.len(), 1);
        assert_eq!(diagnostics.mappings[0].proxy_name, "Sweden [se]");
        assert_eq!(diagnostics.mappings[0].address, "127.0.0.1:31006");
        assert_eq!(diagnostics.mappings[0].xray_label, "Sweden [se]");
    }

    #[test]
    fn catalog_request_decodes_exact_dto_and_optional_delay() {
        let (base_url, server) = start_server(1);
        let client = HttpDaemonClient::new(base_url, "token").expect("authenticated client");

        let catalog = client.catalog().expect("catalog request");

        let requests = server.join().expect("join test HTTP server");
        assert!(requests[0].starts_with("GET /v1/catalog HTTP/1.1\r\n"));
        assert_eq!(
            catalog,
            DaemonCatalog {
                revision: 17,
                groups: vec![DaemonCatalogGroup {
                    id: "group:auto".into(),
                    label: "Авто".into(),
                    selected: true,
                    icon_id: None,
                    nodes: vec![
                        DaemonCatalogNode {
                            id: "node:a".into(),
                            label: "Север".into(),
                            selected: true,
                            delay_ms: Some(42),
                            icon_id: None,
                        },
                        DaemonCatalogNode {
                            id: "node:b".into(),
                            label: "Резерв".into(),
                            selected: false,
                            delay_ms: None,
                            icon_id: None,
                        },
                    ],
                }],
            }
        );
    }

    #[test]
    fn latency_request_posts_and_decodes_only_opaque_ids_safe_status_and_optional_value() {
        let body = r#"{"entries":[{"group_id":"group:auto","node_id":"node:a","latency_ms":42,"status":"ok"},{"group_id":"group:auto","node_id":"node:b","latency_ms":null,"status":"timeout"}]}"#;
        let (base_url, server) = start_selection_server("200 OK", body);
        let client =
            HttpDaemonClient::new(base_url, "latency-token").expect("authenticated client");

        let latencies = client
            .latencies(Some(17), &["group:auto".to_owned()])
            .expect("latency request");

        let request = server.join().expect("join latency server");
        assert!(request.starts_with("POST /v1/latencies HTTP/1.1\r\n"));
        assert!(request.contains("\"group_ids\":[\"group:auto\"]"));
        assert!(
            request
                .to_ascii_lowercase()
                .contains("authorization: bearer latency-token\r\n")
        );
        assert!(request.ends_with(r#"{"revision":17,"group_ids":["group:auto"]}"#));
        assert_eq!(
            latencies,
            DaemonLatencies {
                entries: vec![
                    DaemonLatencyEntry {
                        group_id: "group:auto".into(),
                        node_id: "node:a".into(),
                        latency_ms: Some(42),
                        status: "ok".into(),
                    },
                    DaemonLatencyEntry {
                        group_id: "group:auto".into(),
                        node_id: "node:b".into(),
                        latency_ms: None,
                        status: "timeout".into(),
                    },
                ],
            }
        );
    }

    #[test]
    fn node_latency_request_posts_only_the_node_id() {
        let body = r#"{"entries":[{"group_id":"group:auto","node_id":"node:a","latency_ms":42,"status":"ok"}]}"#;
        let (base_url, server) = start_selection_server("200 OK", body);
        let client =
            HttpDaemonClient::new(base_url, "latency-token").expect("authenticated client");

        let latencies = client
            .node_latencies(Some(17), &["node:a".to_owned()])
            .expect("node latency request");

        let request = server.join().expect("join latency server");
        assert!(request.starts_with("POST /v1/latencies HTTP/1.1\r\n"));
        assert!(request.ends_with(r#"{"revision":17,"group_ids":[],"node_ids":["node:a"]}"#));
        assert_eq!(latencies.entries.len(), 1);
        assert_eq!(latencies.entries[0].latency_ms, Some(42));
    }

    #[test]
    fn group_icon_is_fetched_by_opaque_id_and_bounded() {
        let (base_url, server) = start_selection_server("200 OK", "PNGBYTES");
        let client = HttpDaemonClient::new(base_url, "icon-token").expect("authenticated client");
        let bytes = client
            .group_icon("0123456789abcdef0123456789abcdef")
            .expect("group icon");
        let request = server.join().expect("join icon server");
        assert!(
            request
                .starts_with("GET /v1/group-icons/0123456789abcdef0123456789abcdef HTTP/1.1\r\n")
        );
        assert!(
            request
                .to_ascii_lowercase()
                .contains("authorization: bearer icon-token\r\n")
        );
        assert_eq!(bytes, b"PNGBYTES");

        let (base_url, server) = start_selection_server("404 Not Found", "{}");
        let client = HttpDaemonClient::new(base_url, "icon-token").expect("authenticated client");
        assert!(
            client
                .group_icon("0123456789abcdef0123456789abcdef")
                .is_err()
        );
        server.join().expect("join icon server");
    }

    #[test]
    fn latency_conflict_decodes_stale_revision_without_exposing_response_details() {
        let (base_url, server) = start_selection_server(
            "409 Conflict",
            r#"{"code":"stale_revision","message":"private node marker"}"#,
        );
        let client = HttpDaemonClient::new(base_url, "latency-token").expect("loopback client");

        let error = client
            .latencies(Some(17), &["group:auto".to_owned()])
            .expect_err("stale latency revision");

        let request = server.join().expect("join latency conflict server");
        assert!(request.starts_with("POST /v1/latencies HTTP/1.1\r\n"));
        assert!(error.is_stale_revision());
        assert!(!error.to_string().contains("private node marker"));
    }

    #[test]
    fn events_request_includes_cursor_and_url_encoded_known_epoch() {
        let (base_url, server) = start_server(2);
        let client =
            HttpDaemonClient::new(base_url, "token").expect("construct authenticated client");

        let first_events = client.events(0, None).expect("first events request");
        let events = client
            .events(42, Some("boot/a b"))
            .expect("subsequent events request");

        let requests = server.join().expect("join test HTTP server");
        assert!(
            requests[0].starts_with("GET /v1/events?after=0 HTTP/1.1\r\n"),
            "first request must omit epoch: {}",
            requests[0]
        );
        assert!(
            requests[1].starts_with("GET /v1/events?after=42&epoch=boot%2Fa+b HTTP/1.1\r\n"),
            "unexpected known-epoch request: {}",
            requests[1]
        );
        assert_eq!(first_events.epoch, "boot-a");
        assert_eq!(
            events,
            DaemonEventBatch {
                epoch: "boot-a".into(),
                events: vec![DaemonEvent {
                    id: 43,
                    timestamp: "1".into(),
                    level: "info".into(),
                    component: String::new(),
                    message: "Подключение установлено.".into(),
                    details: None,
                }],
            }
        );
    }

    #[test]
    fn empty_daemon_token_is_rejected_without_making_a_request() {
        let result = HttpDaemonClient::new("http://127.0.0.1:1", "  ");

        assert!(result.is_err());
    }

    #[test]
    fn daemon_base_url_accepts_only_plain_http_loopback_socket_roots() {
        assert!(HttpDaemonClient::new("http://127.0.0.1:12345", "token").is_ok());
        assert!(HttpDaemonClient::new("http://127.9.8.7:12345/", "token").is_ok());
        assert!(HttpDaemonClient::new("http://[::1]:12345", "token").is_ok());

        for invalid in [
            "https://127.0.0.1:12345",
            "http://localhost:12345",
            "http://192.0.2.1:12345",
            "http://[2001:db8::1]:12345",
            "http://user@127.0.0.1:12345",
            "http://user:password@127.0.0.1:12345",
            "http://127.0.0.1:12345/v1",
            "http://127.0.0.1:12345/?mode=test",
            "http://127.0.0.1:12345/#fragment",
        ] {
            assert!(
                HttpDaemonClient::new(invalid, "token").is_err(),
                "accepted unsafe daemon URL: {invalid}"
            );
        }
    }

    #[test]
    fn redirects_are_not_followed_and_are_definitive_http_errors() {
        let stop = Arc::new(AtomicBool::new(false));
        let ok_body = r#"{"state":"ready"}"#;
        let ok_response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            ok_body.len(),
            ok_body
        )
        .into_bytes();
        let (redirect_target, target) =
            start_observing_server("127.0.0.1:0", ok_response, Arc::clone(&stop));
        let redirect_response = format!(
            "HTTP/1.1 302 Found\r\nLocation: {redirect_target}/v1/status\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
        )
        .into_bytes();
        let (base_url, redirector) = start_raw_response_server(redirect_response);
        let client = HttpDaemonClient::new(base_url, "redirect-token").expect("loopback client");

        let error = client.status().expect_err("redirect must not be followed");

        redirector.join().expect("join redirect server");
        stop.store(true, Ordering::SeqCst);
        let target_request = target.join().expect("join redirect target");
        assert!(target_request.is_none(), "redirect leaked a request");
        assert!(!error.requires_reconciliation());
        assert!(!error.to_string().contains("redirect-token"));
    }

    #[test]
    fn proxy_environment_cannot_receive_a_daemon_request_or_token() {
        let stop = Arc::new(AtomicBool::new(false));
        let status_body = r#"{"state":"ready"}"#;
        let status_response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            status_body.len(),
            status_body
        )
        .into_bytes();
        let proxy_response =
            b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec();
        let (daemon_url, daemon) =
            start_observing_server("127.0.0.2:0", status_response, Arc::clone(&stop));
        let (proxy_url, proxy) =
            start_observing_server("127.0.0.1:0", proxy_response, Arc::clone(&stop));

        let output = Command::new(std::env::current_exe().expect("current test executable"))
            .args([
                "--ignored",
                "--exact",
                "daemon::http_tests::proxy_environment_child",
            ])
            .env("MULTICORE_PROXY_TEST_DAEMON_URL", &daemon_url)
            .env("MULTICORE_PROXY_TEST_PROXY_URL", &proxy_url)
            .output()
            .expect("run isolated proxy regression child");

        stop.store(true, Ordering::SeqCst);
        let daemon_request = daemon.join().expect("join daemon observer");
        let proxy_request = proxy.join().expect("join proxy observer");
        assert!(
            output.status.success(),
            "proxy child failed:\nstdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let daemon_request = daemon_request.expect("daemon must receive direct request");
        assert!(
            daemon_request
                .to_ascii_lowercase()
                .contains("authorization: bearer proxy-regression-secret\r\n")
        );
        assert!(
            proxy_request.is_none(),
            "proxy received daemon credentials: {proxy_request:?}"
        );
    }

    #[test]
    #[ignore = "isolated helper invoked by proxy_environment_cannot_receive_a_daemon_request_or_token"]
    fn proxy_environment_child() {
        let daemon_url =
            std::env::var("MULTICORE_PROXY_TEST_DAEMON_URL").expect("proxy test daemon URL");
        let proxy_url =
            std::env::var("MULTICORE_PROXY_TEST_PROXY_URL").expect("proxy test proxy URL");
        let _environment_lock = ENVIRONMENT_LOCK.lock().expect("environment lock");
        let _environment = EnvironmentGuard::set(&[
            ("HTTP_PROXY", &proxy_url),
            ("http_proxy", &proxy_url),
            ("ALL_PROXY", &proxy_url),
            ("all_proxy", &proxy_url),
            ("NO_PROXY", ""),
            ("no_proxy", ""),
        ]);

        let client = HttpDaemonClient::new(daemon_url, "proxy-regression-secret")
            .expect("construct isolated proxy test client");
        client.status().expect("direct daemon status request");
    }

    #[test]
    fn catalog_rejects_oversized_content_length_before_decoding() {
        let response = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 33554433\r\nConnection: close\r\n\r\n".to_vec();
        let (base_url, server) = start_raw_response_server(response);
        let client = HttpDaemonClient::new(base_url, "catalog-token").expect("loopback client");

        let error = client.catalog().expect_err("oversized catalog must fail");

        server.join().expect("join oversized catalog server");
        assert!(error.requires_reconciliation());
        assert!(!error.to_string().contains("catalog-token"));
    }

    #[test]
    fn decoded_status_body_is_bounded_for_chunked_and_close_delimited_responses() {
        let oversized = vec![b'x'; 65_537];
        let mut chunked = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n{:x}\r\n",
            oversized.len()
        )
        .into_bytes();
        chunked.extend_from_slice(&oversized);
        chunked.extend_from_slice(b"\r\n0\r\n\r\n");

        let mut close_delimited =
            b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n"
                .to_vec();
        close_delimited.extend_from_slice(&oversized);

        for response in [chunked, close_delimited] {
            let (base_url, server) = start_raw_response_server(response);
            let client = HttpDaemonClient::new(base_url, "status-token").expect("loopback client");

            let error = client.status().expect_err("oversized status must fail");

            server.join().expect("join oversized status server");
            assert!(error.requires_reconciliation());
            assert!(!error.to_string().contains("status-token"));
        }
    }

    #[test]
    fn malformed_events_json_is_returned_as_an_error() {
        let (base_url, server) = start_single_response_server(r#"{"events":"not-an-array"}"#);
        let client =
            HttpDaemonClient::new(base_url, "token").expect("construct authenticated client");

        let result = client.events(0, None);

        server.join().expect("join test HTTP server");
        assert!(result.is_err());
    }

    #[test]
    fn malformed_catalog_json_is_returned_as_an_error() {
        let (base_url, server) = start_single_response_server(r#"{"revision":1,"groups":{}}"#);
        let client = HttpDaemonClient::new(base_url, "token").expect("authenticated client");

        let result = client.catalog();

        server.join().expect("join test HTTP server");
        assert!(result.is_err());
    }

    #[test]
    fn selection_request_percent_encodes_group_and_sends_exact_json_and_auth() {
        let response = r#"{"revision":91,"groups":[]}"#;
        let (base_url, server) = start_selection_server("200 OK", response);
        let client = HttpDaemonClient::new(base_url, "selection-token")
            .expect("construct authenticated client");

        let catalog = client
            .select_node("opaque/group ?#%", 91, "opaque-node")
            .expect("selection request");

        let request = server.join().expect("join selection server");
        let (headers, body) = request.split_once("\r\n\r\n").expect("HTTP request");
        assert!(headers.starts_with("PUT /v1/selections/opaque%2Fgroup%20%3F%23%25 HTTP/1.1\r\n"));
        assert!(
            headers
                .to_ascii_lowercase()
                .contains("authorization: bearer selection-token\r\n")
        );
        assert_eq!(body, r#"{"revision":91,"node_id":"opaque-node"}"#);
        assert_eq!(catalog.revision, 91);
    }

    #[test]
    fn profile_requests_use_exact_methods_auth_and_one_percent_encoded_id_segment() {
        let id = "opaque/profile ?#%";
        let encoded = "opaque%2Fprofile%20%3F%23%25";
        let list_body = r#"{"active_profile_id":"profile-a","profiles":[]}"#;
        let status_body = r#"{"state":"ready","active_profile_id":"profile-a"}"#;

        let (base_url, server) = start_selection_server("200 OK", list_body);
        let client = HttpDaemonClient::new(base_url, "profile-token").expect("list client");
        client.subscriptions().expect("list profiles");
        let request = server.join().expect("list server");
        assert_profile_request(&request, "GET /v1/subscriptions", "profile-token");

        let (base_url, server) = start_selection_server("200 OK", status_body);
        let client = HttpDaemonClient::new(base_url, "profile-token").expect("activate client");
        client.activate_subscription(id).expect("activate profile");
        let request = server.join().expect("activate server");
        assert_profile_request(
            &request,
            &format!("PUT /v1/subscriptions/{encoded}/activate"),
            "profile-token",
        );

        let (base_url, server) = start_selection_server("200 OK", list_body);
        let client = HttpDaemonClient::new(base_url, "profile-token").expect("refresh client");
        client
            .refresh_subscription_by_id(id)
            .expect("refresh profile");
        let request = server.join().expect("refresh server");
        assert_profile_request(
            &request,
            &format!("POST /v1/subscriptions/{encoded}/refresh"),
            "profile-token",
        );

        let (base_url, server) = start_selection_server("200 OK", list_body);
        let client = HttpDaemonClient::new(base_url, "profile-token").expect("edit client");
        client
            .replace_subscription_source(id, "https://replacement.invalid/sub?token=secret")
            .expect("replace subscription source");
        let request = server.join().expect("edit server");
        let (headers, body) = request.split_once("\r\n\r\n").expect("HTTP request");
        assert!(headers.starts_with(&format!(
            "PUT /v1/subscriptions/{encoded}/source HTTP/1.1\r\n"
        )));
        assert!(
            headers
                .to_ascii_lowercase()
                .contains("authorization: bearer profile-token\r\n")
        );
        assert_eq!(
            body,
            r#"{"url":"https://replacement.invalid/sub?token=secret"}"#
        );

        let (base_url, server) = start_selection_server("200 OK", status_body);
        let client = HttpDaemonClient::new(base_url, "profile-token").expect("delete client");
        client.delete_subscription(id).expect("delete profile");
        let request = server.join().expect("delete server");
        assert_profile_request(
            &request,
            &format!("DELETE /v1/subscriptions/{encoded}"),
            "profile-token",
        );
    }

    fn assert_profile_request(request: &str, request_line: &str, token: &str) {
        let (headers, body) = request.split_once("\r\n\r\n").expect("HTTP request");
        assert!(
            headers.starts_with(&format!("{request_line} HTTP/1.1\r\n")),
            "unexpected request: {headers}"
        );
        assert!(
            headers
                .to_ascii_lowercase()
                .contains(&format!("authorization: bearer {token}\r\n"))
        );
        assert!(body.is_empty(), "profile mutation sent a request body");
    }

    #[test]
    fn profile_http_errors_are_typed_and_never_reflect_body_id_or_token() {
        let sensitive_id = "private-profile-marker";
        let sensitive_token = "private-token-marker";
        let sensitive_body = r#"{"code":"private","message":"https://private.invalid/?token=private-token-marker private-profile-marker"}"#;

        for (status, not_found, conflict) in [
            ("404 Not Found", true, false),
            ("409 Conflict", false, true),
            ("500 Internal Server Error", false, false),
        ] {
            let (base_url, server) = start_selection_server(status, sensitive_body);
            let client = HttpDaemonClient::new(base_url, sensitive_token).expect("profile client");
            let error = client
                .activate_subscription(sensitive_id)
                .expect_err("profile request must fail");
            server.join().expect("profile error server");

            assert_eq!(error.is_profile_not_found(), not_found);
            assert_eq!(error.is_profile_conflict(), conflict);
            let text = error.to_string();
            assert!(!text.contains(sensitive_id));
            assert!(!text.contains(sensitive_token));
            assert!(!text.contains("https://"));
        }
    }

    #[test]
    fn import_failures_show_local_text_for_known_codes_without_reflecting_the_body() {
        for (status, code, expected) in [
            (
                "502 Bad Gateway",
                "subscription_forbidden",
                "отказал в доступе",
            ),
            (
                "422 Unprocessable Entity",
                "subscription_not_multicore",
                "веб-страница",
            ),
            (
                "409 Conflict",
                "profile_busy",
                "Отключитесь перед изменением",
            ),
        ] {
            let body = format!(
                r#"{{"code":"{code}","message":"https://private.invalid/?token=secret-marker"}}"#
            );
            let body: &'static str = Box::leak(body.into_boxed_str());
            let (base_url, server) = start_selection_server(status, body);
            let client = HttpDaemonClient::new(base_url, "import-token").expect("import client");
            let error = client
                .import_subscription("https://private.invalid/?token=secret-marker")
                .expect_err("import must fail");
            server.join().expect("import error server");

            assert!(error.is_specific(), "{code}");
            let text = error.to_string();
            assert!(text.contains(expected), "{code}: {text}");
            assert!(!text.contains("secret-marker"));
            assert!(!text.contains("https://"));
        }

        // Regression coverage for a real bug report: an import failure whose
        // code the client doesn't recognize used to fall all the way back
        // to a generic "check your URL" message that named no real cause at
        // all - and, worse, a request that failed even earlier (before the
        // daemon ever attempted the subscription fetch) left the daemon's
        // own event log completely empty too, so there was no diagnostic
        // trail whatsoever. The numeric HTTP status alone is always safe to
        // show (never anything from the body), so this must stay specific.
        let (base_url, server) =
            start_selection_server("500 Internal Server Error", r#"{"code":"unknown"}"#);
        let client = HttpDaemonClient::new(base_url, "import-token").expect("import client");
        let error = client
            .import_subscription("https://example.invalid/sub")
            .expect_err("import must fail");
        server.join().expect("import error server");
        assert!(error.is_specific());
        assert!(error.to_string().contains("HTTP 500"));
    }

    #[test]
    fn committed_profile_mutation_error_requires_authoritative_reconciliation() {
        let (base_url, server) = start_selection_server(
            "500 Internal Server Error",
            r#"{"code":"profile_mutation_ambiguous","message":"private committed marker"}"#,
        );
        let client = HttpDaemonClient::new(base_url, "profile-token").expect("profile client");

        let error = client
            .replace_subscription_source("profile-a", "https://replacement.invalid/private")
            .expect_err("committed mutation response must not be accepted as success");
        server.join().expect("profile error server");

        assert!(error.requires_reconciliation());
        assert!(!error.to_string().contains("private committed marker"));
        assert!(!error.to_string().contains("replacement.invalid"));
    }

    #[test]
    fn committed_active_refresh_error_requires_authoritative_reconciliation() {
        let (base_url, server) = start_selection_server(
            "500 Internal Server Error",
            r#"{"code":"profile_mutation_ambiguous","message":"private committed marker"}"#,
        );
        let client = HttpDaemonClient::new(base_url, "refresh-token").expect("refresh client");

        let error = client
            .refresh_subscription()
            .expect_err("committed refresh response must not be accepted as success");
        let request = server.join().expect("refresh error server");

        assert_profile_request(&request, "POST /v1/subscriptions/refresh", "refresh-token");
        assert!(error.requires_reconciliation());
        assert!(!error.to_string().contains("private committed marker"));
    }

    #[test]
    fn profile_list_response_is_bounded_before_json_decoding() {
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            SUBSCRIPTIONS_RESPONSE_LIMIT_BYTES + 1
        )
        .into_bytes();
        let (base_url, server) = start_raw_response_server(response);
        let client = HttpDaemonClient::new(base_url, "profile-token").expect("profile client");

        let error = client
            .subscriptions()
            .expect_err("oversized profile list must fail");

        server.join().expect("oversized profile server");
        assert!(error.requires_reconciliation());
        assert!(!error.to_string().contains("profile-token"));
    }

    #[test]
    fn selection_conflict_is_classified_without_exposing_response_details() {
        let sensitive = "submitted-private-node-marker";
        let (base_url, server) = start_selection_server(
            "409 Conflict",
            r#"{"code":"stale_revision","message":"submitted-private-node-marker"}"#,
        );
        let client = HttpDaemonClient::new(base_url, "selection-token")
            .expect("construct authenticated client");

        let error = client
            .select_node("opaque-group", 90, sensitive)
            .expect_err("stale selection must fail");

        server.join().expect("join selection server");
        assert!(error.is_stale_revision());
        assert!(!error.requires_reconciliation());
        assert!(!error.to_string().contains(sensitive));
    }

    #[test]
    fn non_stale_selection_conflicts_are_ordinary_safe_errors() {
        let sensitive = "submitted-private-node-marker";
        for (case, body) in [
            (
                "not connected",
                r#"{"code":"not_connected","message":"submitted-private-node-marker"}"#,
            ),
            (
                "unknown code",
                r#"{"code":"future_conflict","message":"submitted-private-node-marker"}"#,
            ),
            (
                "malformed JSON",
                r#"{"code":"stale_revision","message":"submitted-private-node-marker""#,
            ),
        ] {
            let (base_url, server) = start_selection_server("409 Conflict", body);
            let client = HttpDaemonClient::new(base_url, "selection-token")
                .expect("construct authenticated client");

            let error = client
                .select_node("opaque-group", 90, sensitive)
                .unwrap_err();

            server.join().expect("join selection server");
            assert!(!error.is_stale_revision(), "case: {case}");
            assert!(!error.requires_reconciliation(), "case: {case}");
            assert_eq!(
                error.to_string(),
                "Не удалось изменить маршрут.",
                "case: {case}"
            );
            assert!(!error.to_string().contains(sensitive), "case: {case}");
        }
    }

    #[test]
    fn selection_errors_reconcile_only_after_ambiguous_success_or_transport() {
        let sensitive = "private-selection-marker";

        let (base_url, server) = start_selection_server(
            "500 Internal Server Error",
            r#"{"message":"private-selection-marker"}"#,
        );
        let client = HttpDaemonClient::new(base_url, "selection-token").expect("loopback client");
        let definitive = client
            .select_node("group", 1, sensitive)
            .expect_err("HTTP business failure");
        server.join().expect("join business-error server");
        assert!(!definitive.requires_reconciliation());
        assert!(!definitive.to_string().contains(sensitive));

        let (base_url, server) = start_selection_server("200 OK", r#"{"revision":1,"groups":{}}"#);
        let client = HttpDaemonClient::new(base_url, "selection-token").expect("loopback client");
        let malformed = client
            .select_node("group", 1, sensitive)
            .expect_err("malformed successful response");
        server.join().expect("join malformed-response server");
        assert!(malformed.requires_reconciliation());
        assert!(!malformed.to_string().contains(sensitive));

        let response = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 33554433\r\nConnection: close\r\n\r\n".to_vec();
        let (base_url, server) = start_raw_response_server(response);
        let client = HttpDaemonClient::new(base_url, "selection-token").expect("loopback client");
        let oversized = client
            .select_node("group", 1, sensitive)
            .expect_err("oversized successful response");
        server.join().expect("join oversized-response server");
        assert!(oversized.requires_reconciliation());
        assert!(!oversized.to_string().contains(sensitive));

        let listener = TcpListener::bind("127.0.0.1:0").expect("reserve unused port");
        let address = listener.local_addr().expect("unused port address");
        drop(listener);
        let client = HttpDaemonClient::new(format!("http://{address}"), "selection-token")
            .expect("loopback client");
        let transport = client
            .select_node("group", 1, sensitive)
            .expect_err("transport failure");
        assert!(transport.requires_reconciliation());
        assert!(!transport.to_string().contains(sensitive));
    }

    fn start_delayed_status_server(delay: Duration) -> (String, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind delayed HTTP server");
        let address = listener.local_addr().expect("delayed server address");
        let handle = thread::spawn(move || {
            for _ in 0..2 {
                let (mut stream, _) = listener.accept().expect("accept delayed request");
                let mut buffer = [0_u8; 2048];
                let _ = stream.read(&mut buffer);
                thread::sleep(delay);
                let body = r#"{"state":"ready"}"#;
                let _ = stream.write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    )
                    .as_bytes(),
                );
            }
        });
        (format!("http://{address}"), handle)
    }

    fn start_delayed_subscription_server(delay: Duration) -> (String, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind delayed subscription server");
        let address = listener.local_addr().expect("delayed server address");
        let handle = thread::spawn(move || {
            for _ in 0..4 {
                let (mut stream, _) = listener.accept().expect("accept delayed request");
                let mut buffer = [0_u8; 2048];
                let length = stream.read(&mut buffer).expect("read delayed request");
                let request = String::from_utf8_lossy(&buffer[..length]);
                thread::sleep(delay);
                let body = if request.contains("/v1/subscriptions/profile-a/refresh") {
                    r#"{"active_profile_id":"profile-a","profiles":[]}"#
                } else {
                    r#"{"state":"ready","active_profile_id":"profile-a"}"#
                };
                stream
                    .write_all(
                        format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                            body.len(),
                            body
                        )
                        .as_bytes(),
                    )
                    .expect("write delayed response");
            }
        });
        (format!("http://{address}"), handle)
    }

    #[test]
    fn connect_sends_the_mode_only_when_tun_is_off() {
        let (base_url, server) = start_server(2);
        let client = HttpDaemonClient::new(base_url, "token").unwrap();
        client.connect_with_tun(true).unwrap();
        client.connect_with_tun(false).unwrap();
        let requests = server.join().unwrap();
        assert!(requests[0].starts_with("POST /v1/connect "));
        assert!(
            !requests[0]
                .to_ascii_lowercase()
                .contains("content-type: application/json")
        );
        assert!(requests[1].starts_with("POST /v1/connect "));
        assert!(
            requests[1]
                .to_ascii_lowercase()
                .contains("content-length: 13")
        );
    }

    #[test]
    fn connect_waits_for_elevation_longer_than_ordinary_requests() {
        let (url, server) = start_delayed_status_server(Duration::from_millis(600));
        let client = HttpDaemonClient::with_timeouts(
            url,
            "token",
            Duration::from_millis(200),
            Duration::from_millis(200),
            Duration::from_secs(5),
        )
        .unwrap();
        assert!(client.status().is_err());
        assert!(client.connect().is_ok());
        server.join().unwrap();
        assert!(CONNECT_REQUEST_TIMEOUT > REQUEST_TIMEOUT);
        assert!(CONNECT_REQUEST_TIMEOUT >= Duration::from_secs(300 + 2 * 20 + 30));
        assert!(DISCONNECT_REQUEST_TIMEOUT >= Duration::from_secs(2 * 5 + 10));
    }

    #[test]
    fn subscription_mutations_wait_for_remote_fetch_and_core_shutdown() {
        let (url, server) = start_delayed_subscription_server(Duration::from_millis(600));
        let client = HttpDaemonClient::with_timeouts(
            url,
            "token",
            Duration::from_millis(200),
            Duration::from_secs(5),
            Duration::from_secs(5),
        )
        .unwrap();

        assert!(client.refresh_subscription().is_ok());
        assert!(client.refresh_subscription_by_id("profile-a").is_ok());
        assert!(client.delete_subscription("profile-a").is_ok());
        assert!(client.activate_subscription("profile-a").is_ok());
        server.join().unwrap();
        assert!(SUBSCRIPTION_MUTATION_TIMEOUT > REQUEST_TIMEOUT);
        assert!(SUBSCRIPTION_MUTATION_TIMEOUT >= Duration::from_secs(150));
    }

    #[test]
    fn tests_never_write_the_real_user_low_level_log() {
        let path = low_level_log_path().expect("test log path");
        if let Ok(real_root) = multicore_platform::dirs::user_data_root() {
            assert!(
                !path.starts_with(&real_root),
                "{path:?} is inside the real data directory {real_root:?}"
            );
        }
        assert!(path.starts_with(std::env::temp_dir()));
        // Unique, so neither the repeat filter nor another test's line can stand in for it.
        let probe = format!(
            "isolation_probe_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        );
        record_low_level(&probe, "test only");
        assert!(read_low_level_log().is_some_and(|log| log.contains(&probe)));
    }

    #[test]
    fn low_level_entries_accumulate_and_read_back_together() {
        let directory = tempfile::tempdir().expect("temp dir");
        let path = directory.path().join("logs").join("client-errors.log");

        assert_eq!(read_low_level_entries(&path), None);

        write_low_level_entry(&path, "daemon_request_failed", "connection refused");
        write_low_level_entry(
            &path,
            "daemon_response_decode_failed",
            "unexpected end of json",
        );

        let contents = read_low_level_entries(&path).expect("entries recorded");
        assert!(contents.contains("daemon_request_failed"));
        assert!(contents.contains("connection refused"));
        assert!(contents.contains("daemon_response_decode_failed"));
        assert!(contents.contains("unexpected end of json"));
    }

    #[test]
    fn low_level_entry_control_characters_never_split_a_line() {
        let directory = tempfile::tempdir().expect("temp dir");
        let path = directory.path().join("client-errors.log");

        write_low_level_entry(
            &path,
            "daemon_response_decode_failed",
            "line one\nline two\ttabbed",
        );

        let contents = read_low_level_entries(&path).expect("entry recorded");
        assert_eq!(contents.lines().count(), 1, "{contents:?}");
    }

    #[test]
    fn identical_low_level_entries_are_suppressed_only_within_the_window() {
        let now = std::time::Instant::now();
        let last = ("daemon_request_failed\tconnection refused".to_owned(), now);
        assert!(is_repeated_low_level_entry(
            Some(&last),
            "daemon_request_failed\tconnection refused",
            now + Duration::from_secs(5)
        ));
        assert!(!is_repeated_low_level_entry(
            Some(&last),
            "daemon_request_failed\ttimed out",
            now + Duration::from_secs(5)
        ));
        assert!(!is_repeated_low_level_entry(
            Some(&last),
            "daemon_request_failed\tconnection refused",
            now + LOW_LEVEL_REPEAT_WINDOW
        ));
        assert!(!is_repeated_low_level_entry(None, "anything", now));
    }

    #[test]
    fn rotation_keeps_the_newest_entries() {
        let directory = tempfile::tempdir().expect("temp dir");
        let path = directory.path().join("client-errors.log");
        write_low_level_entry(&path, "daemon_request_failed", "oldest entry");
        for _ in 0..40 {
            write_low_level_entry(&path, "daemon_request_failed", "x".repeat(8_192));
        }
        write_low_level_entry(&path, "daemon_status_error", "newest entry");
        write_low_level_entry(&path, "daemon_status_error", "after rotation");

        let contents = read_low_level_entries(&path).expect("entries recorded");
        assert!(!contents.contains("oldest entry"));
        assert!(
            contents.contains("newest entry"),
            "rotation dropped recent entries"
        );
        assert!(contents.contains("after rotation"));
        assert!(contents.lines().all(|line| line.split('\t').count() == 3));
    }

    #[test]
    fn low_level_log_rotates_instead_of_growing_without_bound() {
        let directory = tempfile::tempdir().expect("temp dir");
        let path = directory.path().join("client-errors.log");

        let entry_bound = 8_192 + 256;
        let mut largest = 0;
        for _ in 0..256 {
            write_low_level_entry(&path, "daemon_request_failed", "x".repeat(8_192));
            largest = largest.max(std::fs::metadata(&path).expect("log exists").len());
        }
        assert!(
            largest <= MAX_LOW_LEVEL_LOG_BYTES + entry_bound,
            "log grew past its bound: {largest}"
        );

        write_low_level_entry(&path, "daemon_request_failed", "after rotation");
        assert!(
            read_low_level_entries(&path)
                .expect("entry recorded")
                .contains("after rotation")
        );
    }
}
