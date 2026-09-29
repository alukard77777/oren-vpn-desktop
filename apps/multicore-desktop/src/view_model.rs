use std::{
    collections::BTreeMap,
    path::Path,
    sync::{Arc, OnceLock},
    time::{SystemTime, UNIX_EPOCH},
};

use regex::Regex;
use sha2::{Digest, Sha256};

use crate::daemon::{
    DaemonCatalog, DaemonClient, DaemonDiagnostics, DaemonEvent, DaemonEventBatch, DaemonLatencies,
    DaemonSession, DaemonStatus, DaemonSubscriptionInfo, DaemonSubscriptionList,
    DaemonSubscriptionProfile,
};

#[allow(dead_code)] // Wired into the Slint shell in Task 6.
const MAX_SUBSCRIPTION_PROFILES: usize = 16;
pub(crate) const MAX_SUBSCRIPTION_URL_BYTES: usize = 8_192;
const MAX_SERVICE_LOGO_PATH_BYTES: usize = 1024;
const STABLE_ROUTE_KEY_PREFIX: &str = "label-sha256:";

fn stable_route_key(label: &str) -> String {
    format!(
        "{STABLE_ROUTE_KEY_PREFIX}{:x}",
        Sha256::digest(label.as_bytes())
    )
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UiState {
    Empty,
    Importing,
    Ready {
        profile: String,
        node: String,
    },
    Connecting {
        profile: String,
        node: String,
        step: String,
    },
    Disconnecting {
        profile: String,
        node: String,
    },
    Connected {
        profile: String,
        node: String,
    },
    Error {
        message: String,
    },
    /// The status poll itself failed: the local service did not answer, so the
    /// tunnel state is unknown rather than a failed connection attempt.
    ServiceUnreachable {
        message: String,
    },
    DaemonDegraded {
        message: String,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PrimaryAction {
    None,
    Connect,
    Disconnect,
    Retry,
    Cleanup,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Panel {
    #[default]
    None,
    Subscription,
    Routes,
    Events,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SemanticTone {
    Neutral,
    Accent,
    Success,
    Danger,
    Warning,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct MutationToken(u64);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MutationKind {
    Refresh,
    Import,
    Connect,
    Disconnect,
    DisconnectReconciliation,
    SubscriptionRefresh,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ActiveMutation {
    token: MutationToken,
    kind: MutationKind,
    profile_epoch: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct EventRequestToken(u64);

pub(crate) struct EventRequest {
    pub(crate) client: Arc<dyn DaemonClient>,
    pub(crate) after: u64,
    pub(crate) known_epoch: Option<String>,
    pub(crate) token: EventRequestToken,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct DiagnosticsRequestToken(u64);

pub(crate) struct DiagnosticsRequest {
    pub(crate) client: Arc<dyn DaemonClient>,
    pub(crate) token: DiagnosticsRequestToken,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CatalogRequestToken(u64);

pub(crate) struct CatalogRequest {
    pub(crate) client: Arc<dyn DaemonClient>,
    pub(crate) token: CatalogRequestToken,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct LatencyRequestToken(u64);

pub(crate) struct LatencyRequest {
    pub(crate) client: Arc<dyn DaemonClient>,
    pub(crate) token: LatencyRequestToken,
    pub(crate) catalog_revision: Option<u64>,
    pub(crate) group_ids: Vec<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct LatencyPresentation {
    pub(crate) loading: bool,
    pub(crate) queued: bool,
    pub(crate) error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LatencyOutcome {
    pub(crate) refresh_catalog: bool,
    pub(crate) rerun: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SelectionRequestToken(u64);

pub(crate) struct NodeSelectionRequest {
    pub(crate) client: Arc<dyn DaemonClient>,
    pub(crate) token: SelectionRequestToken,
    pub(crate) group_id: String,
    pub(crate) revision: u64,
    pub(crate) node_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct QueuedNodeSelection {
    pub(crate) group_id: String,
    pub(crate) revision: u64,
    pub(crate) node_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SelectionOutcome {
    pub(crate) refresh_catalog: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ActiveSelection {
    token: SelectionRequestToken,
    group_id: String,
    previously_selected_node_ids: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CatalogGroupPresentation {
    pub(crate) id: String,
    pub(crate) label: String,
    pub(crate) selected: bool,
    pub(crate) selected_node_label: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TrayCatalogGroupPresentation {
    pub(crate) id: String,
    pub(crate) label: String,
    pub(crate) selected_node_label: String,
    pub(crate) nodes: Vec<CatalogNodePresentation>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CatalogNodePresentation {
    pub(crate) id: String,
    pub(crate) label: String,
    pub(crate) selected: bool,
    pub(crate) delay_ms: Option<u64>,
    pub(crate) latency_text: String,
    pub(crate) latency_tone: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct CatalogPresentation {
    pub(crate) revision: Option<u64>,
    pub(crate) groups: Vec<CatalogGroupPresentation>,
    pub(crate) selected_group_id: Option<String>,
    pub(crate) nodes: Vec<CatalogNodePresentation>,
    pub(crate) loading: bool,
    pub(crate) group_navigation_enabled: bool,
    pub(crate) selection_enabled: bool,
    pub(crate) selection_pending: bool,
    #[allow(dead_code)]
    // Queue state is asserted in tests; the UI shows the queued route as selected.
    pub(crate) selection_queued: bool,
    pub(crate) error: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct SubscriptionPresentation {
    pub(crate) display_name: String,
    pub(crate) usage: String,
    pub(crate) expiry: String,
    pub(crate) updated: String,
    pub(crate) updated_stale: bool,
    pub(crate) announcement_text: String,
    pub(crate) announcement_tone: String,
    pub(crate) service_logo_path: Option<String>,
    pub(crate) refresh_available: bool,
    pub(crate) refresh_pending: bool,
    pub(crate) error: Option<String>,
}

#[allow(dead_code)] // Wired into the Slint shell in Task 6.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SubscriptionProfilePresentation {
    pub(crate) id: String,
    pub(crate) active: bool,
    pub(crate) available: bool,
    pub(crate) title: String,
    pub(crate) usage: String,
    pub(crate) expiry: String,
    pub(crate) service_logo_path: Option<String>,
}

#[allow(dead_code)] // Wired into the Slint shell in Task 6.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SubscriptionProfileOperation {
    List,
    Activate,
    Refresh,
    Edit,
    Delete,
}

#[allow(dead_code)] // Wired into the Slint shell in Task 6.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct SubscriptionProfilesPresentation {
    pub(crate) active_id: Option<String>,
    pub(crate) profiles: Vec<SubscriptionProfilePresentation>,
    pub(crate) loading: bool,
    /// The profile list could not be loaded; retrying the list request helps.
    pub(crate) error: Option<String>,
    /// A profile mutation was rejected or failed. Reloading the list does not
    /// resolve it, so it is shown without a retry action.
    pub(crate) mutation_error: Option<String>,
    /// Switching the active subscription requires a disconnected tunnel.
    pub(crate) switch_locked: bool,
    pub(crate) pending_operation: Option<SubscriptionProfileOperation>,
    pub(crate) pending_id: Option<String>,
}

#[allow(dead_code)] // Wired into the Slint shell in Task 6.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ProfileRequestToken(u64);

#[derive(Clone, Debug, PartialEq, Eq)]
struct ActiveProfileRequest {
    token: ProfileRequestToken,
    operation: SubscriptionProfileOperation,
    profile_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct CatalogGroup {
    id: String,
    label: String,
    is_primary: bool,
    nodes: Vec<CatalogNodePresentation>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum EventFilter {
    #[default]
    All,
    System,
    Error,
    /// Just the vendored cores' own startup/runtime log lines (Xray,
    /// Mihomo), stripped of every other app event - the "first 15 seconds
    /// of startup" a user chasing a real connect failure actually needs,
    /// without having to scroll past unrelated events to find them.
    Cores,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StatePresentation {
    pub eyebrow: &'static str,
    pub headline: String,
    pub supporting: String,
    pub profile: String,
    pub node: String,
    pub primary_label: &'static str,
    pub primary_action: PrimaryAction,
    pub primary_enabled: bool,
    pub semantic_tone: SemanticTone,
    pub has_profile: bool,
    pub is_connected: bool,
    pub is_busy: bool,
    pub is_error: bool,
    pub is_degraded: bool,
}

impl UiState {
    pub fn presentation(&self) -> StatePresentation {
        match self {
            Self::Empty => StatePresentation {
                eyebrow: "Профиль не добавлен",
                headline: "Добавьте подписку".into(),
                supporting: "Вставьте URL — остальное настроим автоматически.".into(),
                profile: "Нет профиля".into(),
                node: "Маршрут не выбран".into(),
                primary_label: "Подключиться",
                primary_action: PrimaryAction::None,
                primary_enabled: false,
                semantic_tone: SemanticTone::Neutral,
                has_profile: false,
                is_connected: false,
                is_busy: false,
                is_error: false,
                is_degraded: false,
            },
            Self::Importing => StatePresentation {
                eyebrow: "Импорт",
                headline: "Добавляем профиль…".into(),
                supporting: "Проверяем подписку и доступные маршруты.".into(),
                profile: "Новый профиль".into(),
                node: "Маршрут не выбран".into(),
                primary_label: "Добавляем…",
                primary_action: PrimaryAction::None,
                primary_enabled: false,
                semantic_tone: SemanticTone::Warning,
                has_profile: false,
                is_connected: false,
                is_busy: true,
                is_error: false,
                is_degraded: false,
            },
            Self::Ready { profile, node } => StatePresentation {
                eyebrow: "Готово",
                headline: "Можно подключаться".into(),
                supporting: String::new(),
                profile: profile.clone(),
                node: node.clone(),
                primary_label: "Подключиться",
                primary_action: PrimaryAction::Connect,
                primary_enabled: true,
                semantic_tone: SemanticTone::Accent,
                has_profile: true,
                is_connected: false,
                is_busy: false,
                is_error: false,
                is_degraded: false,
            },
            Self::Connecting {
                profile,
                node,
                step,
            } => StatePresentation {
                eyebrow: "Подключение",
                headline: "Устанавливаем соединение…".into(),
                supporting: step.clone(),
                profile: profile.clone(),
                node: node.clone(),
                primary_label: "Подключаем…",
                primary_action: PrimaryAction::None,
                primary_enabled: false,
                semantic_tone: SemanticTone::Warning,
                has_profile: true,
                is_connected: false,
                is_busy: true,
                is_error: false,
                is_degraded: false,
            },
            Self::Connected { profile, node } => StatePresentation {
                eyebrow: "Подключено",
                headline: "Соединение защищено".into(),
                supporting: "Текущий маршрут".into(),
                profile: profile.clone(),
                node: node.clone(),
                primary_label: "Отключиться",
                primary_action: PrimaryAction::Disconnect,
                primary_enabled: true,
                semantic_tone: SemanticTone::Success,
                has_profile: true,
                is_connected: true,
                is_busy: false,
                is_error: false,
                is_degraded: false,
            },
            Self::Disconnecting { profile, node } => StatePresentation {
                eyebrow: "Отключение",
                headline: "Завершаем соединение…".into(),
                supporting: "Отключаем локальный маршрут.".into(),
                profile: profile.clone(),
                node: node.clone(),
                primary_label: "Отключаем…",
                primary_action: PrimaryAction::None,
                primary_enabled: false,
                semantic_tone: SemanticTone::Warning,
                has_profile: true,
                is_connected: true,
                is_busy: true,
                is_error: false,
                is_degraded: false,
            },
            Self::Error { message } => StatePresentation {
                eyebrow: "Не удалось подключиться",
                headline: "Что-то пошло не так".into(),
                supporting: message.clone(),
                profile: "—".into(),
                node: "Авто".into(),
                primary_label: "Проверить снова",
                primary_action: PrimaryAction::Retry,
                primary_enabled: true,
                semantic_tone: SemanticTone::Danger,
                has_profile: true,
                is_connected: false,
                is_busy: false,
                is_error: true,
                is_degraded: false,
            },
            Self::ServiceUnreachable { message } => StatePresentation {
                eyebrow: "Нет связи с сервисом",
                headline: "Сервис не отвечает".into(),
                supporting: message.clone(),
                profile: "Состояние неизвестно".into(),
                node: "—".into(),
                primary_label: "Проверить снова",
                primary_action: PrimaryAction::Retry,
                primary_enabled: true,
                semantic_tone: SemanticTone::Danger,
                has_profile: true,
                is_connected: false,
                is_busy: false,
                is_error: true,
                is_degraded: false,
            },
            Self::DaemonDegraded { message } => StatePresentation {
                eyebrow: "Сервис недоступен",
                headline: "Ограниченный режим".into(),
                supporting: message.clone(),
                profile: "Состояние неизвестно".into(),
                node: "—".into(),
                primary_label: "Остановить туннель",
                primary_action: PrimaryAction::Cleanup,
                primary_enabled: true,
                semantic_tone: SemanticTone::Warning,
                has_profile: true,
                is_connected: false,
                is_busy: false,
                is_error: false,
                is_degraded: true,
            },
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PresentedEvent {
    pub id: u64,
    pub timestamp: String,
    pub component: String,
    pub level: String,
    pub message: String,
}

impl From<DaemonEvent> for PresentedEvent {
    fn from(event: DaemonEvent) -> Self {
        let mut message = redact_event_text(&event.message);
        if let Some(details) = format_event_details(event.details.as_ref()) {
            message.push(' ');
            message.push_str(&redact_event_text(&details));
        }
        Self {
            id: event.id,
            timestamp: event.timestamp,
            component: if event.component.is_empty() {
                "system".into()
            } else {
                event.component
            },
            level: event.level,
            message,
        }
    }
}

/// Turns the daemon's already-redacted structured detail object into a
/// compact `[key=value key=value]` suffix for the event message - the
/// event feed's only real estate. Applies `redact_event_text` again on the
/// caller's side (see above) as defense in depth: this is the whole reason
/// the field exists, so a user hitting an opaque failure can grab the
/// event feed text and hand it over instead of just "не удалось...".
fn format_event_details(details: Option<&serde_json::Value>) -> Option<String> {
    let fields = details?.as_object()?;
    if fields.is_empty() {
        return None;
    }
    let mut keys: Vec<&String> = fields.keys().collect();
    keys.sort();
    let rendered = keys
        .into_iter()
        .map(|key| {
            let value = &fields[key];
            let value = match value {
                serde_json::Value::String(text) => text.clone(),
                other => other.to_string(),
            };
            format!("{key}={value}")
        })
        .collect::<Vec<_>>()
        .join(" ");
    Some(format!("[{rendered}]"))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RuntimeCheckPresentation {
    pub value: String,
    pub tone: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiagnosticsPresentation {
    pub xray: RuntimeCheckPresentation,
    pub mihomo: RuntimeCheckPresentation,
    pub tun: RuntimeCheckPresentation,
    pub mappings: Vec<ProxyMappingPresentation>,
    pub loading: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProxyMappingPresentation {
    pub proxy_name: String,
    pub address: String,
    pub xray_label: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ConflictingAppsPresentation {
    pub visible: bool,
    pub apps: Vec<String>,
    pub message: String,
}

/// Builds the Russian banner copy for one or more detected conflicting apps,
/// appending a note about any app a close attempt could not fully remove.
fn conflicting_apps_message(apps: &[&'static str], close_note: &[&'static str]) -> String {
    let names = apps.join(", ");
    let mut message = if apps.len() > 1 {
        format!(
            "Обнаружены другие VPN-приложения ({names}): они могут конфликтовать с MultiCore. Закройте их перед подключением."
        )
    } else {
        format!(
            "Обнаружено другое VPN-приложение ({names}): оно может конфликтовать с MultiCore. Закройте его перед подключением."
        )
    };
    if !close_note.is_empty() {
        let stuck = close_note.join(", ");
        message.push_str(&format!(
            " Закрыть удалось не всё: {stuck} запускает свой движок как службу Windows, \
            которую наша кнопка не трогает без повышения прав. Отключитесь в самом \
            приложении перед закрытием или остановите его службу в «Службах Windows»."
        ));
    }
    message
}

pub fn redact_event_text(input: &str) -> String {
    static AUTHORIZATION: OnceLock<Regex> = OnceLock::new();
    static SENSITIVE_VALUE: OnceLock<Regex> = OnceLock::new();
    static URL: OnceLock<Regex> = OnceLock::new();
    static UUID: OnceLock<Regex> = OnceLock::new();

    let authorization = AUTHORIZATION.get_or_init(|| {
        Regex::new(r"(?im)\b(proxy-authorization|authorization)\s*:\s*[^\r\n]+")
            .expect("authorization redaction regex must compile")
    });
    let sensitive = SENSITIVE_VALUE.get_or_init(|| {
        Regex::new(
            r"(?i)\b(token|password|secret|api[_-]?key|subscription(?:_url)?|url)\s*([:=])\s*([^\s,;]+)",
        )
        .expect("redaction regex must compile")
    });
    let url = URL.get_or_init(|| {
        Regex::new(r"(?i)\bhttps?://[^\s,;]+").expect("URL redaction regex must compile")
    });
    let uuid = UUID.get_or_init(|| {
        Regex::new(r"(?i)\b[0-9a-f]{8}(?:-[0-9a-f]{4}){3}-[0-9a-f]{12}\b")
            .expect("UUID redaction regex must compile")
    });

    let without_authorization = authorization.replace_all(input, "$1: [REDACTED]");
    let without_values = sensitive.replace_all(&without_authorization, "$1$2[REDACTED]");
    let without_urls = url.replace_all(&without_values, "[REDACTED_URL]");
    uuid.replace_all(&without_urls, "[REDACTED_UUID]")
        .into_owned()
}

pub fn state_from_status(status: DaemonStatus) -> UiState {
    if status.degraded {
        return UiState::DaemonDegraded {
            message: status
                .message
                .unwrap_or_else(|| "Фоновый сервис отвечает не полностью.".into()),
        };
    }

    let profile = status.profile.unwrap_or_else(|| "Основной".into());
    let node = status.current_node.unwrap_or_else(|| "Авто".into());
    match status.state.as_str() {
        "empty" => UiState::Empty,
        "importing" => UiState::Importing,
        "connecting" => UiState::Connecting {
            profile,
            node,
            step: status
                .message
                .unwrap_or_else(|| "Проверяем доступность маршрута…".into()),
        },
        "connected" => UiState::Connected { profile, node },
        "ready" | "disconnected" => UiState::Ready { profile, node },
        "error" => UiState::Error {
            message: status
                .message
                .unwrap_or_else(|| "Повторите попытку через несколько секунд.".into()),
        },
        _ => UiState::Error {
            message: "Фоновый сервис вернул неизвестное состояние.".into(),
        },
    }
}

pub struct DesktopViewModel {
    client: Arc<dyn DaemonClient>,
    state: UiState,
    panel: Panel,
    import_pending: bool,
    import_error: Option<String>,
    import_previous_state: Option<UiState>,
    connection_notice: Option<String>,
    subscription: Option<DaemonSubscriptionInfo>,
    subscription_refresh_error: Option<String>,
    active_profile_id: Option<String>,
    subscription_profiles: Vec<SubscriptionProfilePresentation>,
    profile_error: Option<String>,
    profile_list_error: Option<String>,
    profile_status_reload_needed: bool,
    profile_reload_needed: bool,
    profile_catalog_reload_needed: bool,
    profile_status_reconciliation_pending: bool,
    profile_list_reconciliation_pending: bool,
    profile_catalog_reconciliation_pending: bool,
    #[allow(dead_code)] // Consumed by the profile menu in Task 6.
    profile_close_intent: bool,
    profile_epoch: u64,
    next_profile_list_generation: u64,
    profile_list_request_pending: Option<ProfileRequestToken>,
    next_profile_mutation_generation: u64,
    active_profile_mutation: Option<ActiveProfileRequest>,
    next_mutation_generation: u64,
    active_mutation: Option<ActiveMutation>,
    event_epoch: Option<String>,
    event_cursor: u64,
    event_cache: Vec<PresentedEvent>,
    event_load_error: Option<PresentedEvent>,
    event_filter: EventFilter,
    next_event_generation: u64,
    event_request_pending: Option<EventRequestToken>,
    diagnostics: Option<DaemonDiagnostics>,
    diagnostics_error: Option<PresentedEvent>,
    next_diagnostics_generation: u64,
    diagnostics_request_pending: Option<DiagnosticsRequestToken>,
    catalog_revision: Option<u64>,
    catalog_groups: Vec<CatalogGroup>,
    selected_catalog_group: Option<String>,
    restored_route_group: Option<String>,
    restored_route_nodes: BTreeMap<String, String>,
    catalog_error: Option<String>,
    next_catalog_generation: u64,
    catalog_request_pending: Option<CatalogRequestToken>,
    catalog_request_deferred: bool,
    next_selection_generation: u64,
    active_selection: Option<ActiveSelection>,
    queued_selections: Vec<QueuedNodeSelection>,
    catalog_reconciliation_required: bool,
    catalog_notice_after_refresh: Option<String>,
    next_latency_generation: u64,
    latency_request_pending: Option<LatencyRequestToken>,
    latency_request_revision: Option<u64>,
    latency_request_group_ids: Vec<String>,
    latency_queued: bool,
    latency_error: Option<String>,
    conflicting_apps: Vec<&'static str>,
    conflicting_apps_dismissed_key: Option<String>,
    /// Apps a close attempt just targeted but could not fully remove (some of
    /// their processes are still running after `close_running`). Common
    /// cause: the app's actual engine runs as a Windows service in Session 0
    /// (a separate, protected security context), while its GUI runs in the
    /// user's own session - our close button is deliberately unprivileged
    /// and can only close what the user's own token can, so it closes the
    /// GUI but the service outlives it. Surfaced so the button closing
    /// something doesn't silently look like it did nothing.
    conflicting_apps_close_note: Vec<&'static str>,
}

impl DesktopViewModel {
    pub fn new(client: Arc<dyn DaemonClient>) -> Self {
        Self {
            client,
            state: UiState::DaemonDegraded {
                message: "Проверяем фоновый сервис…".into(),
            },
            panel: Panel::None,
            import_pending: false,
            import_error: None,
            import_previous_state: None,
            connection_notice: None,
            subscription: None,
            subscription_refresh_error: None,
            active_profile_id: None,
            subscription_profiles: Vec::new(),
            profile_error: None,
            profile_list_error: None,
            profile_status_reload_needed: false,
            profile_reload_needed: false,
            profile_catalog_reload_needed: false,
            profile_status_reconciliation_pending: false,
            profile_list_reconciliation_pending: false,
            profile_catalog_reconciliation_pending: false,
            profile_close_intent: false,
            profile_epoch: 0,
            next_profile_list_generation: 0,
            profile_list_request_pending: None,
            next_profile_mutation_generation: 0,
            active_profile_mutation: None,
            next_mutation_generation: 0,
            active_mutation: None,
            event_epoch: None,
            event_cursor: 0,
            event_cache: Vec::new(),
            event_load_error: None,
            event_filter: EventFilter::All,
            next_event_generation: 0,
            event_request_pending: None,
            diagnostics: None,
            diagnostics_error: None,
            next_diagnostics_generation: 0,
            diagnostics_request_pending: None,
            catalog_revision: None,
            catalog_groups: Vec::new(),
            selected_catalog_group: None,
            restored_route_group: None,
            restored_route_nodes: BTreeMap::new(),
            catalog_error: None,
            next_catalog_generation: 0,
            catalog_request_pending: None,
            catalog_request_deferred: false,
            next_selection_generation: 0,
            active_selection: None,
            queued_selections: Vec::new(),
            catalog_reconciliation_required: false,
            catalog_notice_after_refresh: None,
            next_latency_generation: 0,
            latency_request_pending: None,
            latency_request_revision: None,
            latency_request_group_ids: Vec::new(),
            latency_queued: false,
            latency_error: None,
            conflicting_apps: Vec::new(),
            conflicting_apps_dismissed_key: None,
            conflicting_apps_close_note: Vec::new(),
        }
    }

    pub fn presentation(&self) -> StatePresentation {
        let mut presentation = self.state.presentation();
        if let Some(notice) = &self.connection_notice {
            presentation.supporting = notice.clone();
            presentation.is_error = true;
        }
        if self
            .active_mutation
            .is_some_and(|active| active.kind == MutationKind::DisconnectReconciliation)
        {
            presentation.eyebrow = "Проверка";
            presentation.headline = "Проверяем состояние…".into();
            presentation.primary_label = "Проверяем…";
            presentation.semantic_tone = SemanticTone::Warning;
            presentation.is_connected = false;
            presentation.is_busy = true;
        }
        if self.active_mutation.is_some()
            || self.import_pending
            || self.profile_mutation_pending()
            || self.profile_reconciliation_required()
            || self.active_selection.is_some()
            || self.catalog_reconciliation_required
        {
            presentation.primary_enabled = false;
        }
        presentation
    }

    pub(crate) fn panel(&self) -> Panel {
        self.panel
    }

    pub(crate) fn import_pending(&self) -> bool {
        self.import_pending
    }

    pub(crate) fn import_error(&self) -> Option<&str> {
        self.import_error.as_deref()
    }

    pub(crate) fn set_import_error_message(&mut self, message: &'static str) {
        self.import_error = Some(message.to_owned());
    }

    pub(crate) fn dismiss_import_error(&mut self) {
        self.import_error = None;
    }

    pub(crate) fn daemon_client(&self) -> Arc<dyn DaemonClient> {
        self.client.clone()
    }

    fn apply_daemon_status(&mut self, status: DaemonStatus) -> UiState {
        self.set_authoritative_active_profile(status.active_profile_id.clone());
        self.subscription = status.subscription.clone();
        state_from_status(status)
    }
}

#[allow(dead_code)] // Profile operations are wired to background workers in Task 6.
impl DesktopViewModel {
    pub(crate) fn profile_presentation(&self) -> SubscriptionProfilesPresentation {
        SubscriptionProfilesPresentation {
            active_id: self.active_profile_id.clone(),
            profiles: self.subscription_profiles.clone(),
            loading: self.profile_list_request_pending.is_some(),
            error: self.profile_list_error.clone(),
            mutation_error: self.profile_error.clone(),
            switch_locked: matches!(
                self.state,
                UiState::Connected { .. }
                    | UiState::Connecting { .. }
                    | UiState::Disconnecting { .. }
            ),
            pending_operation: self
                .active_profile_mutation
                .as_ref()
                .map(|request| request.operation)
                .or_else(|| {
                    self.profile_list_request_pending
                        .map(|_| SubscriptionProfileOperation::List)
                }),
            pending_id: self
                .active_profile_mutation
                .as_ref()
                .and_then(|request| request.profile_id.clone()),
        }
    }

    pub(crate) fn begin_profile_list(&mut self) -> Option<ProfileRequestToken> {
        self.profile_error = None;
        self.profile_list_error = None;
        self.profile_reload_needed = false;
        self.next_profile_list_generation = self.next_profile_list_generation.wrapping_add(1);
        let token = ProfileRequestToken(self.next_profile_list_generation);
        self.profile_list_request_pending = Some(token);
        Some(token)
    }

    pub(crate) fn finish_profile_list(
        &mut self,
        token: ProfileRequestToken,
        result: Result<DaemonSubscriptionList, crate::daemon::DaemonError>,
    ) -> bool {
        if self.profile_list_request_pending != Some(token) {
            return false;
        }
        self.profile_list_request_pending = None;
        match result {
            Ok(list) => {
                self.replace_profile_list(list);
                self.profile_error = None;
                self.profile_list_error = None;
                self.profile_list_reconciliation_pending = false;
                self.arm_profile_catalog_reconciliation();
            }
            Err(_) => {
                self.profile_list_error =
                    Some("Не удалось загрузить подписки. Повторите попытку.".into());
                if self.profile_list_reconciliation_pending {
                    self.profile_reload_needed = true;
                }
            }
        }
        true
    }

    pub(crate) fn begin_profile_activation(
        &mut self,
        profile_id: &str,
    ) -> Option<ProfileRequestToken> {
        if !self.profile_mutation_preflight(profile_id, false, false) {
            return None;
        }
        if self.active_profile_id.as_deref() == Some(profile_id) {
            return None;
        }
        Some(self.next_profile_mutation(
            SubscriptionProfileOperation::Activate,
            Some(profile_id.to_owned()),
        ))
    }

    pub(crate) fn finish_profile_activation(
        &mut self,
        token: ProfileRequestToken,
        result: Result<DaemonStatus, crate::daemon::DaemonError>,
    ) -> bool {
        if !self.is_current_profile_mutation(token, SubscriptionProfileOperation::Activate) {
            if profile_result_may_have_committed(&result) {
                self.require_full_profile_reconciliation();
            }
            return false;
        }
        self.active_profile_mutation = None;
        match result {
            Ok(status) => {
                self.invalidate_profile_list_request();
                self.advance_profile_epoch();
                self.state = self.apply_profile_changing_status(status);
                self.profile_error = None;
                self.profile_reload_needed = true;
                self.profile_catalog_reload_needed = true;
                self.profile_list_reconciliation_pending = true;
                self.profile_catalog_reconciliation_pending = true;
                true
            }
            Err(error) => {
                self.profile_error = Some(profile_mutation_error(&error, "активировать"));
                if error.requires_reconciliation() {
                    self.require_full_profile_reconciliation();
                }
                false
            }
        }
    }

    pub(crate) fn begin_profile_target_refresh(
        &mut self,
        profile_id: &str,
    ) -> Option<ProfileRequestToken> {
        if !self.profile_mutation_preflight(profile_id, true, true) {
            return None;
        }
        Some(self.next_profile_mutation(
            SubscriptionProfileOperation::Refresh,
            Some(profile_id.to_owned()),
        ))
    }

    pub(crate) fn finish_profile_target_refresh(
        &mut self,
        token: ProfileRequestToken,
        result: Result<DaemonSubscriptionList, crate::daemon::DaemonError>,
    ) -> bool {
        let Some(request) =
            self.take_current_profile_mutation(token, SubscriptionProfileOperation::Refresh)
        else {
            if profile_result_may_have_committed(&result) {
                self.require_full_profile_reconciliation();
            }
            return false;
        };
        let Some(profile_id) = request.profile_id else {
            return false;
        };
        let refreshes_active_profile = self.active_profile_id.as_deref() == Some(&profile_id);
        match result {
            Ok(list) => {
                self.invalidate_profile_list_request();
                self.advance_profile_epoch();
                if let Some(profile) = list
                    .profiles
                    .into_iter()
                    .find(|profile| profile.id == profile_id)
                    .and_then(present_subscription_profile)
                    && let Some(existing) = self
                        .subscription_profiles
                        .iter_mut()
                        .find(|profile| profile.id == profile_id)
                {
                    // A targeted refresh is not authoritative for ordering, other rows, or the
                    // active ID. It changes only the requested row until the follow-up list.
                    let mut profile = profile;
                    profile.active = existing.active;
                    *existing = profile;
                }
                if refreshes_active_profile {
                    self.invalidate_profile_runtime();
                    self.require_profile_reconciliation(true, true, true);
                } else {
                    self.profile_reload_needed = true;
                    self.profile_list_reconciliation_pending = true;
                }
                self.profile_error = None;
                true
            }
            Err(error) => {
                self.profile_error = Some(profile_mutation_error(&error, "обновить"));
                if error.requires_reconciliation() {
                    self.require_full_profile_reconciliation();
                }
                false
            }
        }
    }

    pub(crate) fn begin_profile_source_edit(
        &mut self,
        profile_id: &str,
        url: &str,
    ) -> Option<ProfileRequestToken> {
        if !valid_subscription_url(url) {
            self.profile_error = Some("Введите корректный HTTP(S) URL.".into());
            return None;
        }
        if !self.profile_mutation_preflight(profile_id, false, false) {
            return None;
        }
        Some(self.next_profile_mutation(
            SubscriptionProfileOperation::Edit,
            Some(profile_id.to_owned()),
        ))
    }

    pub(crate) fn finish_profile_source_edit(
        &mut self,
        token: ProfileRequestToken,
        result: Result<DaemonSubscriptionList, crate::daemon::DaemonError>,
    ) -> bool {
        let Some(request) =
            self.take_current_profile_mutation(token, SubscriptionProfileOperation::Edit)
        else {
            if profile_result_may_have_committed(&result) {
                self.require_full_profile_reconciliation();
            }
            return false;
        };
        let Some(profile_id) = request.profile_id else {
            return false;
        };
        let edits_active_profile = self.active_profile_id.as_deref() == Some(&profile_id);
        match result {
            Ok(list) => {
                self.invalidate_profile_list_request();
                self.advance_profile_epoch();
                if let Some(profile) = list
                    .profiles
                    .into_iter()
                    .find(|profile| profile.id == profile_id)
                    .and_then(present_subscription_profile)
                    && let Some(existing) = self
                        .subscription_profiles
                        .iter_mut()
                        .find(|profile| profile.id == profile_id)
                {
                    let mut profile = profile;
                    profile.active = existing.active;
                    *existing = profile;
                }
                if edits_active_profile {
                    self.invalidate_profile_runtime();
                    self.require_profile_reconciliation(true, true, true);
                } else {
                    self.profile_reload_needed = true;
                    self.profile_list_reconciliation_pending = true;
                }
                self.profile_error = None;
                true
            }
            Err(error) => {
                self.profile_error = Some(profile_mutation_error(&error, "изменить ссылку"));
                if error.requires_reconciliation() {
                    self.require_full_profile_reconciliation();
                }
                false
            }
        }
    }

    pub(crate) fn begin_profile_delete(&mut self, profile_id: &str) -> Option<ProfileRequestToken> {
        if !self.profile_mutation_preflight(profile_id, false, true) {
            return None;
        }
        Some(self.next_profile_mutation(
            SubscriptionProfileOperation::Delete,
            Some(profile_id.to_owned()),
        ))
    }

    pub(crate) fn finish_profile_delete(
        &mut self,
        token: ProfileRequestToken,
        result: Result<DaemonStatus, crate::daemon::DaemonError>,
    ) -> bool {
        let Some(request) =
            self.take_current_profile_mutation(token, SubscriptionProfileOperation::Delete)
        else {
            if profile_result_may_have_committed(&result) {
                self.require_full_profile_reconciliation();
            }
            return false;
        };
        let Some(profile_id) = request.profile_id else {
            return false;
        };
        match result {
            Ok(status) => {
                self.invalidate_profile_list_request();
                self.advance_profile_epoch();
                let previous_active = self.active_profile_id.clone();
                self.subscription_profiles
                    .retain(|profile| profile.id != profile_id);
                self.state = self.apply_daemon_status(status);
                if self.active_profile_id != previous_active {
                    self.profile_catalog_reload_needed = true;
                    self.profile_catalog_reconciliation_pending = true;
                }
                self.profile_close_intent = self.subscription_profiles.is_empty();
                self.profile_error = None;
                self.profile_reload_needed = true;
                self.profile_list_reconciliation_pending = true;
                true
            }
            Err(error) => {
                self.profile_error = Some(profile_mutation_error(&error, "удалить"));
                if error.requires_reconciliation() {
                    self.require_full_profile_reconciliation();
                }
                false
            }
        }
    }

    pub(crate) fn take_profile_list_reload_needed(&mut self) -> bool {
        std::mem::take(&mut self.profile_reload_needed)
    }

    pub(crate) fn defer_profile_list_reload(&mut self) {
        self.profile_reload_needed = true;
    }

    pub(crate) fn take_profile_close_intent(&mut self) -> bool {
        std::mem::take(&mut self.profile_close_intent)
    }

    pub(crate) fn take_profile_catalog_reload_needed(&mut self) -> bool {
        std::mem::take(&mut self.profile_catalog_reload_needed)
    }

    pub(crate) fn defer_profile_catalog_reload(&mut self) {
        self.profile_catalog_reload_needed = true;
    }

    pub(crate) fn take_profile_status_reload_needed(&mut self) -> bool {
        std::mem::take(&mut self.profile_status_reload_needed)
    }

    fn next_profile_mutation(
        &mut self,
        operation: SubscriptionProfileOperation,
        profile_id: Option<String>,
    ) -> ProfileRequestToken {
        self.invalidate_profile_list_request();
        self.next_profile_mutation_generation =
            self.next_profile_mutation_generation.wrapping_add(1);
        let token = ProfileRequestToken(self.next_profile_mutation_generation);
        self.active_profile_mutation = Some(ActiveProfileRequest {
            token,
            operation,
            profile_id,
        });
        token
    }

    fn is_current_profile_mutation(
        &self,
        token: ProfileRequestToken,
        operation: SubscriptionProfileOperation,
    ) -> bool {
        self.active_profile_mutation
            .as_ref()
            .is_some_and(|request| request.token == token && request.operation == operation)
    }

    fn take_current_profile_mutation(
        &mut self,
        token: ProfileRequestToken,
        operation: SubscriptionProfileOperation,
    ) -> Option<ActiveProfileRequest> {
        self.is_current_profile_mutation(token, operation)
            .then(|| self.active_profile_mutation.take())
            .flatten()
    }

    fn profile_mutation_preflight(
        &mut self,
        profile_id: &str,
        requires_available: bool,
        allow_connected: bool,
    ) -> bool {
        if matches!(self.state, UiState::Connecting { .. })
            || (!allow_connected && matches!(self.state, UiState::Connected { .. }))
        {
            self.profile_error = Some("Отключитесь перед изменением подписки.".into());
            return false;
        }
        if matches!(self.state, UiState::DaemonDegraded { .. }) {
            self.profile_error = Some("Дождитесь восстановления фонового сервиса.".into());
            return false;
        }
        if self.active_profile_mutation.is_some()
            || self
                .active_mutation
                .is_some_and(|active| active.kind != MutationKind::Refresh)
            || self.import_pending
            || self.active_selection.is_some()
            || self.catalog_reconciliation_required
            || self.profile_reconciliation_required()
            || self.state.presentation().is_busy
        {
            self.profile_error = Some("Дождитесь завершения текущей операции.".into());
            return false;
        }
        let Some(profile) = self
            .subscription_profiles
            .iter()
            .find(|profile| profile.id == profile_id)
        else {
            self.profile_error = Some("Подписка больше недоступна. Обновите список.".into());
            return false;
        };
        if requires_available && !profile.available {
            self.profile_error = Some("Эту подписку сейчас нельзя обновить.".into());
            return false;
        }
        self.profile_error = None;
        true
    }

    fn profile_mutation_pending(&self) -> bool {
        self.active_profile_mutation.is_some()
    }

    fn profile_reconciliation_required(&self) -> bool {
        self.profile_status_reconciliation_pending
            || self.profile_list_reconciliation_pending
            || self.profile_catalog_reconciliation_pending
    }

    fn invalidate_profile_list_request(&mut self) {
        self.next_profile_list_generation = self.next_profile_list_generation.wrapping_add(1);
        self.profile_list_request_pending = None;
    }

    fn advance_profile_epoch(&mut self) {
        self.profile_epoch = self.profile_epoch.wrapping_add(1);
        if self
            .active_mutation
            .is_some_and(|mutation| mutation.kind == MutationKind::Refresh)
        {
            self.active_mutation = None;
        }
    }

    fn require_profile_reconciliation(&mut self, status: bool, list: bool, catalog: bool) {
        self.profile_status_reconciliation_pending |= status;
        self.profile_list_reconciliation_pending |= list;
        self.profile_catalog_reconciliation_pending |= catalog;
        self.profile_status_reload_needed |= status;
        self.profile_reload_needed |= list;
        self.profile_catalog_reload_needed |= catalog;
        self.arm_profile_catalog_reconciliation();
    }

    fn arm_profile_catalog_reconciliation(&mut self) {
        if self.profile_catalog_reconciliation_pending
            && !self.profile_status_reconciliation_pending
            && !self.profile_list_reconciliation_pending
        {
            self.profile_catalog_reload_needed = true;
        }
    }

    fn require_full_profile_reconciliation(&mut self) {
        self.active_profile_mutation = None;
        self.invalidate_profile_list_request();
        self.advance_profile_epoch();
        self.invalidate_profile_runtime();
        self.active_profile_id = None;
        self.sync_profile_active_flags();
        self.state = UiState::DaemonDegraded {
            message: "Проверяем состояние подписок…".into(),
        };
        if self.profile_error.is_none() {
            self.profile_error =
                Some("Не удалось подтвердить изменение. Обновляем состояние подписок.".into());
        }
        self.require_profile_reconciliation(true, true, true);
    }

    fn replace_profile_list(&mut self, list: DaemonSubscriptionList) {
        self.set_authoritative_active_profile(
            list.active_profile_id.as_deref().and_then(safe_profile_id),
        );
        self.subscription_profiles = list
            .profiles
            .into_iter()
            .take(MAX_SUBSCRIPTION_PROFILES)
            .filter_map(present_subscription_profile)
            .map(|mut profile| {
                profile.active = self.active_profile_id.as_deref() == Some(profile.id.as_str());
                profile
            })
            .collect();
    }

    fn set_authoritative_active_profile(&mut self, profile_id: Option<String>) {
        let profile_id = profile_id.as_deref().and_then(safe_profile_id);
        if self.active_profile_id != profile_id {
            self.invalidate_profile_runtime();
            self.active_profile_id = profile_id;
            self.sync_profile_active_flags();
            self.arm_profile_catalog_reconciliation();
        }
    }

    fn apply_profile_changing_status(&mut self, status: DaemonStatus) -> UiState {
        self.invalidate_profile_runtime();
        self.active_profile_id = status
            .active_profile_id
            .as_deref()
            .and_then(safe_profile_id);
        self.sync_profile_active_flags();
        self.subscription = status.subscription.clone();
        state_from_status(status)
    }

    fn sync_profile_active_flags(&mut self) {
        for profile in &mut self.subscription_profiles {
            profile.active = self.active_profile_id.as_deref() == Some(profile.id.as_str());
        }
    }

    fn invalidate_profile_runtime(&mut self) {
        self.subscription = None;
        self.subscription_refresh_error = None;

        self.invalidate_catalog_runtime();
    }

    fn invalidate_catalog_runtime(&mut self) {
        self.next_catalog_generation = self.next_catalog_generation.wrapping_add(1);
        self.catalog_request_pending = None;
        self.catalog_request_deferred = false;
        self.catalog_revision = None;
        self.catalog_groups.clear();
        self.selected_catalog_group = None;
        self.restored_route_group = None;
        self.restored_route_nodes.clear();
        self.catalog_error = None;
        self.catalog_reconciliation_required = false;
        self.catalog_notice_after_refresh = None;

        self.next_selection_generation = self.next_selection_generation.wrapping_add(1);
        self.active_selection = None;
        self.queued_selections.clear();

        self.next_latency_generation = self.next_latency_generation.wrapping_add(1);
        self.latency_request_pending = None;
        self.latency_request_revision = None;
        self.latency_request_group_ids.clear();
        self.latency_queued = false;
        self.latency_error = None;
    }
}

impl DesktopViewModel {
    pub(crate) fn subscription_presentation(&self) -> SubscriptionPresentation {
        self.subscription_presentation_at(current_unix_time())
    }

    pub(crate) fn subscription_presentation_at(&self, now_unix: u64) -> SubscriptionPresentation {
        let Some(info) = &self.subscription else {
            return SubscriptionPresentation {
                display_name: "Подписка".into(),
                usage: "Нет данных".into(),
                expiry: "Добавьте ссылку".into(),
                updated: "Не обновлялась".into(),
                updated_stale: false,
                announcement_text: String::new(),
                announcement_tone: "info".into(),
                service_logo_path: None,
                refresh_available: false,
                refresh_pending: false,
                error: self.subscription_refresh_error.clone(),
            };
        };
        let used_bytes =
            info.used_bytes
                .or_else(|| match (info.uploaded_bytes, info.downloaded_bytes) {
                    (Some(uploaded), Some(downloaded)) => Some(uploaded.saturating_add(downloaded)),
                    (Some(uploaded), None) => Some(uploaded),
                    (None, Some(downloaded)) => Some(downloaded),
                    (None, None) => None,
                });
        let usage = match (used_bytes, info.total_bytes) {
            (Some(used), Some(total)) => format!("{} / {}", format_gib(used), format_gib(total)),
            (Some(used), None) => format_gib(used),
            (None, Some(total)) => format!("— / {}", format_gib(total)),
            _ => "Нет данных".into(),
        };
        let expiry = format_expiry(info.expires_at_unix, now_unix);
        let updated = format_update_date(info.updated_at_unix);
        let updated_stale = info.updated_at_unix != 0
            && now_unix
                >= info
                    .updated_at_unix
                    .saturating_add(subscription_refresh_interval(info));
        let display_name = bounded_plain_text(&info.display_name, 128)
            .filter(|name| !name.is_empty())
            .or_else(|| bounded_plain_text(&info.source_name, 128))
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| "Подписка".into());
        let announcement_text = info
            .announcement_text
            .as_deref()
            .and_then(|text| bounded_plain_text(text, 512))
            .unwrap_or_default();
        let announcement_tone = match info.announcement_tone.as_deref() {
            Some("green" | "success") => "success",
            Some("red" | "danger") => "danger",
            _ => "info",
        };
        SubscriptionPresentation {
            display_name,
            usage,
            expiry,
            updated,
            updated_stale,
            announcement_text,
            announcement_tone: announcement_tone.into(),
            service_logo_path: info.service_logo_path.as_deref().and_then(local_logo_path),
            refresh_available: info.refresh_available,
            refresh_pending: self
                .active_mutation
                .is_some_and(|active| active.kind == MutationKind::SubscriptionRefresh),
            error: self.subscription_refresh_error.clone(),
        }
    }

    pub(crate) fn automatic_subscription_refresh_due_at(&self, now_unix: u64) -> bool {
        let Some(info) = self.subscription.as_ref() else {
            return false;
        };
        if !matches!(
            self.state,
            UiState::Ready { .. } | UiState::Connected { .. }
        ) || !info.refresh_available
        {
            return false;
        }
        now_unix
            >= info
                .updated_at_unix
                .saturating_add(subscription_refresh_interval(info))
    }

    pub(crate) fn begin_subscription_refresh(&mut self) -> Option<MutationToken> {
        if self.active_mutation.is_some()
            || self.import_pending
            || self.profile_mutation_pending()
            || self.profile_reconciliation_required()
            || self.active_selection.is_some()
            || self.catalog_reconciliation_required
            || matches!(self.state, UiState::Connecting { .. })
            || !self
                .subscription
                .as_ref()
                .is_some_and(|info| info.refresh_available)
        {
            return None;
        }
        self.subscription_refresh_error = None;
        Some(self.next_mutation(MutationKind::SubscriptionRefresh))
    }

    pub(crate) fn finish_subscription_refresh(
        &mut self,
        token: MutationToken,
        result: Result<DaemonStatus, crate::daemon::DaemonError>,
    ) -> bool {
        if !self.is_current_mutation(token, MutationKind::SubscriptionRefresh) {
            return false;
        }
        let succeeded = result.is_ok();
        match result {
            Ok(status) => {
                self.state = self.apply_daemon_status(status);
                self.subscription_refresh_error = None;
                self.catalog_revision = None;
                self.queued_selections.clear();
                self.latency_queued = true;
            }
            Err(error) => {
                self.subscription_refresh_error =
                    Some("Не удалось обновить. Рабочий профиль сохранён.".into());
                if error.requires_reconciliation() {
                    self.require_full_profile_reconciliation();
                }
            }
        }
        self.active_mutation = None;
        succeeded
    }

    fn next_mutation(&mut self, kind: MutationKind) -> MutationToken {
        self.next_mutation_generation = self.next_mutation_generation.wrapping_add(1);
        let token = MutationToken(self.next_mutation_generation);
        if kind == MutationKind::Refresh && self.profile_mutation_pending() {
            return token;
        }
        self.active_mutation = Some(ActiveMutation {
            token,
            kind,
            profile_epoch: self.profile_epoch,
        });
        token
    }

    fn is_current_mutation(&self, token: MutationToken, kind: MutationKind) -> bool {
        self.active_mutation.is_some_and(|active| {
            active.token == token
                && active.kind == kind
                && active.profile_epoch == self.profile_epoch
        })
    }

    pub(crate) fn begin_refresh(&mut self) -> MutationToken {
        self.connection_notice = None;
        if self.profile_status_reconciliation_pending {
            self.profile_status_reload_needed = false;
        }
        self.next_mutation(MutationKind::Refresh)
    }

    pub(crate) fn begin_disconnect_reconciliation(&mut self) -> MutationToken {
        self.next_mutation(MutationKind::DisconnectReconciliation)
    }

    pub fn open_panel(&mut self, panel: Panel) {
        self.panel = panel;
    }

    pub fn close_panel_on_escape(&mut self) -> bool {
        if self.panel == Panel::None {
            return false;
        }
        self.panel = Panel::None;
        true
    }

    pub fn begin_primary_action(&mut self) -> Option<(PrimaryAction, MutationToken)> {
        if self.active_mutation.is_some()
            || self.import_pending
            || self.profile_mutation_pending()
            || self.profile_reconciliation_required()
            || self.active_selection.is_some()
            || self.catalog_reconciliation_required
        {
            return None;
        }
        let action = self.state.presentation().primary_action;
        if action == PrimaryAction::None {
            return None;
        }
        let kind = match action {
            PrimaryAction::Connect => MutationKind::Connect,
            PrimaryAction::Disconnect | PrimaryAction::Cleanup => MutationKind::Disconnect,
            PrimaryAction::Retry => MutationKind::Refresh,
            PrimaryAction::None => return None,
        };
        let token = self.next_mutation(kind);
        self.connection_notice = None;
        if action == PrimaryAction::Connect {
            let current = self.state.presentation();
            self.state = UiState::Connecting {
                profile: current.profile,
                node: current.node,
                step: "Запускаем локальный маршрут…".into(),
            };
        } else if action == PrimaryAction::Disconnect {
            let current = self.state.presentation();
            self.state = UiState::Disconnecting {
                profile: current.profile,
                node: current.node,
            };
        }
        Some((action, token))
    }

    pub(crate) fn can_begin_external_import(&self) -> bool {
        !self.presentation().is_connected
            && !self.import_pending
            && !self.profile_mutation_pending()
            && !self.profile_reconciliation_required()
            && !self.profile_status_reload_needed
            && !self.profile_reload_needed
            && !self.profile_catalog_reload_needed
            && self.profile_list_request_pending.is_none()
            && self.catalog_request_pending.is_none()
            && self.active_selection.is_none()
            && !self.catalog_reconciliation_required
            && self.active_mutation.is_none()
    }

    pub fn begin_import(&mut self, url: &str) -> Option<MutationToken> {
        if self.import_pending
            || self.profile_mutation_pending()
            || self.profile_reconciliation_required()
            || self.active_selection.is_some()
            || self.catalog_reconciliation_required
        {
            return None;
        }
        if self
            .active_mutation
            .is_some_and(|active| active.kind != MutationKind::Refresh)
        {
            self.import_error = Some("Дождитесь завершения операции подключения.".into());
            return None;
        }
        if !valid_subscription_url(url) {
            self.import_error = Some("Введите корректный HTTP(S) URL.".into());
            return None;
        }
        self.import_error = None;
        self.import_previous_state = Some(self.state.clone());
        self.import_pending = true;
        Some(self.next_mutation(MutationKind::Import))
    }

    pub(crate) fn finish_refresh(
        &mut self,
        token: MutationToken,
        result: Result<DaemonStatus, crate::daemon::DaemonError>,
    ) {
        if !self.is_current_mutation(token, MutationKind::Refresh) {
            return;
        }
        if self.profile_mutation_pending() {
            self.active_mutation = None;
            return;
        }
        self.state = match result {
            Ok(status) => {
                self.profile_status_reconciliation_pending = false;
                let state = self.apply_daemon_status(status);
                self.arm_profile_catalog_reconciliation();
                state
            }
            Err(error) => {
                if self.profile_status_reconciliation_pending {
                    self.profile_status_reload_needed = true;
                }
                UiState::ServiceUnreachable {
                    message: error.to_string(),
                }
            }
        };
        self.active_mutation = None;
    }

    pub(crate) fn finish_import(
        &mut self,
        token: MutationToken,
        result: Result<DaemonStatus, crate::daemon::DaemonError>,
    ) {
        if !self.is_current_mutation(token, MutationKind::Import) {
            return;
        }
        let previous_state = self
            .import_previous_state
            .take()
            .unwrap_or_else(|| self.state.clone());
        match result {
            Ok(status) => {
                self.invalidate_profile_list_request();
                self.advance_profile_epoch();
                self.subscription_profiles.clear();
                self.profile_error = None;
                self.profile_reload_needed = true;
                self.profile_catalog_reload_needed = true;
                self.state = self.apply_profile_changing_status(status);
                self.import_error = None;
            }
            Err(error) => {
                self.state = previous_state;
                self.import_error = Some(if error.is_specific() {
                    error.to_string()
                } else {
                    "Не удалось добавить подписку. Проверьте URL и повторите попытку.".into()
                });
                if error.requires_reconciliation() {
                    self.require_full_profile_reconciliation();
                }
            }
        }
        self.import_pending = false;
        self.active_mutation = None;
    }

    pub(crate) fn finish_connect(
        &mut self,
        token: MutationToken,
        result: Result<DaemonStatus, crate::daemon::DaemonError>,
    ) {
        if !self.is_current_mutation(token, MutationKind::Connect) {
            return;
        }
        let succeeded = result.is_ok();
        self.state = match result {
            Ok(status) => self.apply_daemon_status(status),
            Err(error) => UiState::Error {
                message: error.to_string(),
            },
        };
        if succeeded {
            self.catalog_revision = None;
        }
        self.active_mutation = None;
    }

    pub(crate) fn finish_disconnect(
        &mut self,
        token: MutationToken,
        result: Result<DaemonStatus, crate::daemon::DaemonError>,
    ) -> bool {
        if !self.is_current_mutation(token, MutationKind::Disconnect) {
            return false;
        }
        let rollback = match &self.state {
            UiState::Disconnecting { profile, node } => Some(UiState::Connected {
                profile: profile.clone(),
                node: node.clone(),
            }),
            _ => None,
        };
        let (state, reconcile_status, succeeded) = match result {
            Ok(status) => (self.apply_daemon_status(status), false, true),
            Err(error) if error.requires_reconciliation() => (
                UiState::DaemonDegraded {
                    message: "Не удалось подтвердить завершение соединения. Проверяем состояние…"
                        .into(),
                },
                true,
                false,
            ),
            Err(_) => {
                self.connection_notice =
                    Some("Не удалось отключиться. Соединение остаётся активным.".into());
                (
                    rollback.unwrap_or_else(|| UiState::Error {
                        message: "Не удалось отключиться. Повторите попытку.".into(),
                    }),
                    false,
                    false,
                )
            }
        };
        self.state = state;
        if succeeded || reconcile_status {
            self.catalog_revision = None;
        }
        self.active_mutation = None;
        reconcile_status
    }

    pub(crate) fn finish_disconnect_reconciliation(
        &mut self,
        token: MutationToken,
        result: Result<DaemonStatus, crate::daemon::DaemonError>,
    ) -> bool {
        if !self.is_current_mutation(token, MutationKind::DisconnectReconciliation) {
            return false;
        }
        self.connection_notice = None;
        self.state = match result {
            Ok(status) => self.apply_daemon_status(status),
            Err(_) => UiState::DaemonDegraded {
                message: "Не удалось подтвердить состояние соединения. Повторите проверку.".into(),
            },
        };
        self.active_mutation = None;
        true
    }

    pub(crate) fn set_event_filter(&mut self, level: Option<&str>) -> Vec<PresentedEvent> {
        self.event_filter = match level {
            Some(level) if level.eq_ignore_ascii_case("system") => EventFilter::System,
            Some(level) if level.eq_ignore_ascii_case("error") => EventFilter::Error,
            Some(level) if level.eq_ignore_ascii_case("cores") => EventFilter::Cores,
            _ => EventFilter::All,
        };
        self.filtered_events()
    }

    pub(crate) fn begin_events_request(&mut self) -> Option<EventRequest> {
        if self.event_request_pending.is_some() {
            return None;
        }
        self.next_event_generation = self.next_event_generation.wrapping_add(1);
        let token = EventRequestToken(self.next_event_generation);
        self.event_request_pending = Some(token);
        Some(EventRequest {
            client: self.client.clone(),
            after: self.event_cursor,
            known_epoch: self.event_epoch.clone(),
            token,
        })
    }

    pub(crate) fn begin_diagnostics_request(&mut self) -> Option<DiagnosticsRequest> {
        if self.diagnostics_request_pending.is_some() {
            return None;
        }
        self.next_diagnostics_generation = self.next_diagnostics_generation.wrapping_add(1);
        let token = DiagnosticsRequestToken(self.next_diagnostics_generation);
        self.diagnostics_request_pending = Some(token);
        self.diagnostics_error = None;
        Some(DiagnosticsRequest {
            client: self.client.clone(),
            token,
        })
    }

    pub(crate) fn finish_diagnostics(
        &mut self,
        token: DiagnosticsRequestToken,
        result: Result<DaemonDiagnostics, crate::daemon::DaemonError>,
    ) -> Option<Vec<PresentedEvent>> {
        if self.diagnostics_request_pending != Some(token) {
            return None;
        }
        self.diagnostics_request_pending = None;
        match result {
            Ok(mut diagnostics) => {
                diagnostics.logs.truncate(256);
                self.diagnostics = Some(diagnostics);
                self.diagnostics_error = None;
            }
            Err(_) => {
                self.diagnostics_error = Some(PresentedEvent {
                    id: 0,
                    timestamp: String::new(),
                    component: "system".into(),
                    level: "error".into(),
                    message: "Не удалось обновить диагностику. Повторно откройте экран.".into(),
                });
            }
        }
        Some(self.filtered_events())
    }

    pub fn diagnostics_presentation(&self) -> DiagnosticsPresentation {
        let loading = self.diagnostics_request_pending.is_some();
        let checks = self.diagnostics.as_ref();
        DiagnosticsPresentation {
            xray: present_runtime_check(checks.map(|value| value.xray.as_str()), loading),
            mihomo: present_runtime_check(checks.map(|value| value.mihomo.as_str()), loading),
            tun: present_runtime_check(checks.map(|value| value.tun.as_str()), loading),
            mappings: checks
                .map(|value| {
                    value
                        .mappings
                        .iter()
                        .map(|mapping| ProxyMappingPresentation {
                            proxy_name: mapping.proxy_name.clone(),
                            address: mapping.address.clone(),
                            xray_label: mapping.xray_label.clone(),
                        })
                        .collect()
                })
                .unwrap_or_default(),
            loading,
        }
    }

    pub(crate) fn finish_events(
        &mut self,
        token: EventRequestToken,
        result: Result<DaemonEventBatch, crate::daemon::DaemonError>,
    ) -> Option<Vec<PresentedEvent>> {
        if self.event_request_pending != Some(token) {
            return None;
        }
        self.event_request_pending = None;
        match result {
            Ok(batch) => {
                self.event_load_error = None;
                if self.event_epoch.as_deref() != Some(batch.epoch.as_str()) {
                    self.event_epoch = Some(batch.epoch);
                    self.event_cursor = 0;
                    self.event_cache.clear();
                }
                let next_cursor = batch
                    .events
                    .iter()
                    .map(|event| event.id)
                    .max()
                    .unwrap_or(self.event_cursor)
                    .max(self.event_cursor);
                self.event_cache.extend(
                    batch
                        .events
                        .into_iter()
                        .filter(|event| event.id > self.event_cursor)
                        .map(PresentedEvent::from),
                );
                self.event_cursor = next_cursor;
                if self.event_cache.len() > 500 {
                    let remove = self.event_cache.len() - 500;
                    self.event_cache.drain(..remove);
                }
            }
            Err(_) => {
                self.event_load_error = Some(PresentedEvent {
                    id: 0,
                    timestamp: String::new(),
                    component: "system".into(),
                    level: "error".into(),
                    message: "Не удалось загрузить события. Повторите попытку.".into(),
                });
            }
        }
        Some(self.filtered_events())
    }

    pub(crate) fn begin_catalog_request(&mut self) -> Option<CatalogRequest> {
        if self.catalog_request_pending.is_some()
            || self.profile_mutation_pending()
            || (self.profile_catalog_reconciliation_pending
                && (self.profile_status_reconciliation_pending
                    || self.profile_list_reconciliation_pending))
        {
            return None;
        }
        if self.active_selection.is_some() {
            self.catalog_request_deferred = true;
            return None;
        }
        self.catalog_request_deferred = false;
        if self.profile_catalog_reconciliation_pending {
            self.profile_catalog_reload_needed = false;
        }
        self.next_catalog_generation = self.next_catalog_generation.wrapping_add(1);
        let token = CatalogRequestToken(self.next_catalog_generation);
        self.catalog_request_pending = Some(token);
        if self.catalog_notice_after_refresh.is_none() {
            self.catalog_error = None;
        }
        Some(CatalogRequest {
            client: self.client.clone(),
            token,
        })
    }

    pub(crate) fn latency_presentation(&self) -> LatencyPresentation {
        LatencyPresentation {
            loading: self.latency_request_pending.is_some(),
            queued: self.latency_queued,
            error: self.latency_error.clone(),
        }
    }

    /// Records the currently-detected conflicting VPN client apps (from a
    /// local process-name scan; see `conflicting_apps.rs`). When the set
    /// becomes empty — meaning every previously-detected app is no longer
    /// running — any prior dismissal is cleared too, so the banner reappears
    /// if the same app is closed and later reopened, rather than staying
    /// silently dismissed forever.
    pub(crate) fn set_conflicting_apps(&mut self, mut apps: Vec<&'static str>) {
        apps.sort_unstable();
        apps.dedup();
        if apps.is_empty() {
            self.conflicting_apps_dismissed_key = None;
        }
        // Drop the note for any noted app that isn't detected anymore
        // (whether or not other apps are still/newly present).
        self.conflicting_apps_close_note
            .retain(|noted| apps.contains(noted));
        self.conflicting_apps = apps;
    }

    /// Called right after a close-app attempt re-scans. Any `targeted` app
    /// still present afterward gets an explanatory note appended to the
    /// banner (see `conflicting_apps_close_note`'s doc comment for why this
    /// happens); a `targeted` app no longer present needs no note.
    pub(crate) fn record_close_attempt(&mut self, targeted: &[&'static str]) {
        self.conflicting_apps_close_note = targeted
            .iter()
            .filter(|app| self.conflicting_apps.contains(app))
            .copied()
            .collect();
    }

    /// Dismisses the banner for exactly the currently-detected set of apps.
    /// A later detection of a *different* set (a new app appears, or this
    /// set shrinks/grows) compares unequal and the banner reappears.
    pub(crate) fn dismiss_conflicting_apps_banner(&mut self) {
        self.conflicting_apps_dismissed_key = Some(self.conflicting_apps_key());
    }

    fn conflicting_apps_key(&self) -> String {
        self.conflicting_apps.join("\u{1f}")
    }

    /// The currently-detected app names, as the canonical `&'static str`
    /// values (not the display-only `String`s `conflicting_apps_presentation`
    /// returns), so callers can round-trip them into `close_running` and
    /// `record_close_attempt` without re-parsing.
    pub(crate) fn conflicting_apps(&self) -> Vec<&'static str> {
        self.conflicting_apps.clone()
    }

    pub(crate) fn conflicting_apps_presentation(&self) -> ConflictingAppsPresentation {
        if self.conflicting_apps.is_empty() {
            return ConflictingAppsPresentation::default();
        }
        // Purely informational: never shown once the tunnel is already
        // connected and working, only before/while attempting to connect.
        if matches!(self.state, UiState::Connected { .. }) {
            return ConflictingAppsPresentation::default();
        }
        let visible =
            self.conflicting_apps_dismissed_key.as_deref() != Some(&self.conflicting_apps_key());
        ConflictingAppsPresentation {
            visible,
            apps: self
                .conflicting_apps
                .iter()
                .map(|s| s.to_string())
                .collect(),
            message: conflicting_apps_message(
                &self.conflicting_apps,
                &self.conflicting_apps_close_note,
            ),
        }
    }

    pub(crate) fn begin_latency_request(&mut self) -> Option<LatencyRequest> {
        self.latency_error = None;
        if self.latency_request_pending.is_some()
            || self.profile_mutation_pending()
            || self.profile_reconciliation_required()
        {
            self.latency_queued = true;
            return None;
        }
        if !matches!(self.state, UiState::Connected { .. })
            || self.catalog_revision.is_none()
            || self.selected_catalog_group.is_none()
        {
            self.latency_queued = true;
            return None;
        }
        let group_ids = vec![self.selected_catalog_group.clone()?];
        self.next_latency_generation = self.next_latency_generation.wrapping_add(1);
        let token = LatencyRequestToken(self.next_latency_generation);
        self.latency_request_pending = Some(token);
        self.latency_request_revision = self.catalog_revision;
        self.latency_request_group_ids.clone_from(&group_ids);
        self.latency_queued = false;
        Some(LatencyRequest {
            client: self.client.clone(),
            token,
            catalog_revision: self.catalog_revision,
            group_ids,
        })
    }

    pub(crate) fn finish_latencies(
        &mut self,
        token: LatencyRequestToken,
        result: Result<DaemonLatencies, crate::daemon::DaemonError>,
    ) -> Option<LatencyOutcome> {
        if self.latency_request_pending != Some(token) {
            return None;
        }
        self.latency_request_pending = None;
        let targeted_group_ids = std::mem::take(&mut self.latency_request_group_ids);
        if self.latency_request_revision.take() != self.catalog_revision {
            self.latency_queued = true;
            return Some(LatencyOutcome {
                refresh_catalog: false,
                rerun: true,
            });
        }
        let mut refresh_catalog = false;
        match result {
            Ok(latencies) => {
                for entry in &latencies.entries {
                    if !targeted_group_ids.iter().any(|id| id == &entry.group_id) {
                        continue;
                    }
                    if let Some(node) = self
                        .catalog_groups
                        .iter_mut()
                        .find(|group| group.id == entry.group_id)
                        .and_then(|group| {
                            group.nodes.iter_mut().find(|node| node.id == entry.node_id)
                        })
                    {
                        let (text, tone) = latency_display(&entry.status, entry.latency_ms);
                        node.delay_ms = if entry.status == "ok" {
                            entry.latency_ms
                        } else {
                            None
                        };
                        node.latency_text = text;
                        node.latency_tone = tone;
                    }
                }
                self.latency_error = None;
            }
            Err(error) if error.is_stale_revision() => {
                self.catalog_revision = None;
                self.latency_queued = true;
                self.latency_error =
                    Some("Каталог маршрутов изменился. Обновляем задержку…".into());
                refresh_catalog = true;
            }
            Err(_) => {
                self.latency_error =
                    Some("Не удалось проверить задержку. Повторите попытку.".into());
            }
        }
        Some(LatencyOutcome {
            refresh_catalog,
            rerun: self.latency_queued && !refresh_catalog,
        })
    }

    pub(crate) fn finish_catalog(
        &mut self,
        token: CatalogRequestToken,
        result: Result<DaemonCatalog, crate::daemon::DaemonError>,
    ) -> Option<CatalogPresentation> {
        if self.catalog_request_pending != Some(token) {
            return None;
        }
        self.catalog_request_pending = None;
        match result {
            Ok(catalog) => {
                let reconciliation = self.catalog_reconciliation_required;
                self.replace_catalog(catalog);
                if reconciliation {
                    self.sync_connected_route_from_catalog();
                }
                self.catalog_reconciliation_required = false;
                self.profile_catalog_reconciliation_pending = false;
                self.catalog_error = self.catalog_notice_after_refresh.take();
            }
            Err(_) => {
                self.catalog_revision = None;
                self.catalog_reconciliation_required = false;
                if self.profile_catalog_reconciliation_pending {
                    self.profile_catalog_reload_needed = true;
                }
                self.catalog_error = self
                    .catalog_notice_after_refresh
                    .take()
                    .or_else(|| Some("Не удалось загрузить маршруты. Повторите попытку.".into()));
            }
        }
        Some(self.catalog_presentation())
    }

    pub(crate) fn select_catalog_group(&mut self, id: &str) -> bool {
        if self.catalog_request_pending.is_some()
            || !self.catalog_groups.iter().any(|group| group.id == id)
        {
            return false;
        }
        self.selected_catalog_group = Some(id.to_owned());
        self.latency_queued = true;
        true
    }

    /// Supplies the route choices loaded from preferences before the first catalog refresh.
    /// Legacy opaque IDs and stable unique labels are both accepted.
    pub(crate) fn restore_route_preferences(
        &mut self,
        group_id: Option<&str>,
        selections: &BTreeMap<String, String>,
    ) {
        self.restored_route_group = group_id.map(str::to_owned);
        self.restored_route_nodes = selections.clone();
        if !self.catalog_groups.is_empty() {
            self.apply_restored_route_preferences();
        }
    }

    /// Keeps daemon node IDs opaque; only the locally viewed group gets a stable label hash.
    pub(crate) fn route_preferences(
        &self,
    ) -> Option<(String, Option<String>, BTreeMap<String, String>)> {
        if self.catalog_groups.is_empty() {
            return None;
        }
        let selections = self
            .catalog_groups
            .iter()
            .filter_map(|group| {
                group
                    .nodes
                    .iter()
                    .find(|node| node.selected)
                    .map(|node| (group.id.clone(), node.id.clone()))
            })
            .collect();
        let selected_group = self
            .selected_catalog_group
            .as_ref()
            .and_then(|id| self.catalog_groups.iter().find(|group| &group.id == id))
            .and_then(|group| {
                let key = stable_route_key(&group.label);
                (self.resolve_group_preference_id(&key).as_deref() == Some(group.id.as_str()))
                    .then_some(key)
            });
        Some((self.active_profile_id.clone()?, selected_group, selections))
    }

    pub(crate) fn resolve_group_preference_id(&self, key: &str) -> Option<String> {
        if key.starts_with(STABLE_ROUTE_KEY_PREFIX) {
            let mut matching = self
                .catalog_groups
                .iter()
                .filter(|group| stable_route_key(&group.label) == key);
            let group = matching.next()?;
            return matching.next().is_none().then(|| group.id.clone());
        }
        if let Some(group) = self.catalog_groups.iter().find(|group| group.id == key) {
            return Some(group.id.clone());
        }
        None
    }

    pub(crate) fn resolve_route_preference_ids(
        &self,
        group_key: &str,
        node_key: &str,
    ) -> Option<(String, String)> {
        let group_id = self.resolve_group_preference_id(group_key)?;
        let group = self
            .catalog_groups
            .iter()
            .find(|group| group.id == group_id)?;
        if let Some(node) = group.nodes.iter().find(|node| node.id == node_key) {
            return Some((group_id, node.id.clone()));
        }
        None
    }

    pub(crate) fn begin_node_selection(&mut self, node_id: &str) -> Option<NodeSelectionRequest> {
        if self.active_selection.is_some()
            || self.catalog_request_pending.is_some()
            || self.profile_mutation_pending()
            || self.profile_reconciliation_required()
            || self.active_mutation.is_some()
            || self.import_pending
            || self.catalog_reconciliation_required
            || !matches!(
                self.state,
                UiState::Connected { .. } | UiState::Ready { .. }
            )
        {
            return None;
        }
        let revision = self.catalog_revision?;
        let group_id = self.selected_catalog_group.clone()?;
        let group = self
            .catalog_groups
            .iter_mut()
            .find(|group| group.id == group_id)?;
        let target_index = group.nodes.iter().position(|node| node.id == node_id)?;
        if group.nodes[target_index].selected {
            return None;
        }
        let previously_selected_node_ids = group
            .nodes
            .iter()
            .filter(|node| node.selected)
            .map(|node| node.id.clone())
            .collect();
        for node in &mut group.nodes {
            node.selected = false;
        }
        group.nodes[target_index].selected = true;
        let selected_label = group.nodes[target_index].label.clone();
        let updates_connection_summary = group.is_primary;
        if matches!(self.state, UiState::Ready { .. }) {
            let queued = QueuedNodeSelection {
                group_id,
                revision,
                node_id: node_id.to_owned(),
            };
            if let Some(existing) = self
                .queued_selections
                .iter_mut()
                .find(|existing| existing.group_id == queued.group_id)
            {
                *existing = queued;
            } else {
                self.queued_selections.push(queued);
            }
            if updates_connection_summary && let UiState::Ready { node, .. } = &mut self.state {
                *node = selected_label;
            }
            self.catalog_error = None;
            return None;
        }
        self.queued_selections.clear();
        self.next_selection_generation = self.next_selection_generation.wrapping_add(1);
        let token = SelectionRequestToken(self.next_selection_generation);
        self.active_selection = Some(ActiveSelection {
            token,
            group_id: group_id.clone(),
            previously_selected_node_ids,
        });
        self.catalog_error = None;
        Some(NodeSelectionRequest {
            client: self.client.clone(),
            token,
            group_id,
            revision,
            node_id: node_id.to_owned(),
        })
    }

    pub(crate) fn queued_node_selections(&self) -> Vec<QueuedNodeSelection> {
        self.queued_selections.clone()
    }

    pub(crate) fn finish_queued_node_selections(
        &mut self,
        queued: &[QueuedNodeSelection],
        results: Vec<Result<DaemonCatalog, crate::daemon::DaemonError>>,
    ) -> bool {
        if self.queued_selections != queued {
            return false;
        }
        if results.len() != queued.len() {
            return false;
        }
        self.queued_selections.clear();
        let mut failed = false;
        for result in results {
            match result {
                Ok(catalog) => {
                    self.replace_catalog(catalog);
                    self.sync_connected_route_from_catalog();
                }
                Err(_) => failed = true,
            }
        }
        if failed {
            self.catalog_revision = None;
            self.catalog_error = Some(
                "Подключено, но часть маршрутов не применилась. Проверьте выбор ещё раз.".into(),
            );
        } else {
            self.catalog_error = None;
        }
        true
    }

    pub(crate) fn finish_node_selection(
        &mut self,
        token: SelectionRequestToken,
        result: Result<DaemonCatalog, crate::daemon::DaemonError>,
    ) -> Option<SelectionOutcome> {
        if self
            .active_selection
            .as_ref()
            .map(|selection| selection.token)
            != Some(token)
        {
            return None;
        }
        let active = self
            .active_selection
            .take()
            .expect("checked active selection");
        match result {
            Ok(catalog) => {
                self.replace_catalog(catalog);
                if let Some(label) = self
                    .catalog_groups
                    .iter()
                    .find(|group| group.id == active.group_id && group.is_primary)
                    .and_then(|group| group.nodes.iter().find(|node| node.selected))
                    .map(|node| node.label.clone())
                    && let UiState::Connected { node, .. } = &mut self.state
                {
                    *node = label;
                }
                self.catalog_error = None;
                self.catalog_reconciliation_required = true;
                Some(SelectionOutcome {
                    refresh_catalog: true,
                })
            }
            Err(error) => {
                self.restore_selection(&active);
                let stale_revision = error.is_stale_revision();
                let ambiguous = error.requires_reconciliation();
                let refresh_catalog = stale_revision || ambiguous || self.catalog_request_deferred;
                if stale_revision {
                    self.catalog_revision = None;
                    self.catalog_error =
                        Some("Каталог маршрутов изменился. Обновляем его; повторите выбор.".into());
                    self.catalog_notice_after_refresh =
                        Some("Каталог маршрутов обновлён. Повторите выбор.".into());
                } else if ambiguous {
                    self.catalog_revision = None;
                    let message = "Не удалось подтвердить изменение маршрута. Каталог обновляется; повторите попытку.".to_owned();
                    self.catalog_error = Some(message.clone());
                    self.catalog_notice_after_refresh = Some(message);
                } else {
                    let message = "Не удалось изменить маршрут. Повторите попытку.".to_owned();
                    self.catalog_error = Some(message.clone());
                    if refresh_catalog {
                        self.catalog_revision = None;
                        self.catalog_notice_after_refresh = Some(message);
                    }
                }
                self.catalog_reconciliation_required = refresh_catalog;
                Some(SelectionOutcome { refresh_catalog })
            }
        }
    }

    pub(crate) fn catalog_presentation(&self) -> CatalogPresentation {
        let selected = self.selected_catalog_group.as_deref();
        let groups = self
            .catalog_groups
            .iter()
            .map(|group| CatalogGroupPresentation {
                id: group.id.clone(),
                label: group.label.clone(),
                selected: selected == Some(group.id.as_str()),
                selected_node_label: group
                    .nodes
                    .iter()
                    .find(|node| node.selected)
                    .map(|node| node.label.clone())
                    .unwrap_or_else(|| "Не выбран".into()),
            })
            .collect();
        let nodes = selected
            .and_then(|id| self.catalog_groups.iter().find(|group| group.id == id))
            .map(|group| group.nodes.clone())
            .unwrap_or_default();
        CatalogPresentation {
            revision: self.catalog_revision,
            groups,
            selected_group_id: self.selected_catalog_group.clone(),
            nodes,
            loading: self.catalog_request_pending.is_some(),
            group_navigation_enabled: !self.catalog_groups.is_empty()
                && self.catalog_request_pending.is_none(),
            selection_enabled: matches!(
                self.state,
                UiState::Connected { .. } | UiState::Ready { .. }
            ) && self.catalog_revision.is_some()
                && self.catalog_request_pending.is_none()
                && self.active_selection.is_none()
                && self.active_mutation.is_none()
                && !self.import_pending
                && !self.catalog_reconciliation_required,
            selection_pending: self.active_selection.is_some(),
            selection_queued: !self.queued_selections.is_empty(),
            error: self.catalog_error.clone(),
        }
    }

    pub(crate) fn tray_catalog_groups(
        &self,
        max_groups: usize,
        max_nodes: usize,
    ) -> Vec<TrayCatalogGroupPresentation> {
        let mut remaining_nodes = max_nodes;
        self.catalog_groups
            .iter()
            .take(max_groups)
            .map(|group| {
                let nodes = group
                    .nodes
                    .iter()
                    .take(remaining_nodes)
                    .cloned()
                    .collect::<Vec<_>>();
                remaining_nodes = remaining_nodes.saturating_sub(nodes.len());
                TrayCatalogGroupPresentation {
                    id: group.id.clone(),
                    label: group.label.clone(),
                    selected_node_label: group
                        .nodes
                        .iter()
                        .find(|node| node.selected)
                        .map(|node| node.label.clone())
                        .unwrap_or_else(|| "Не выбран".into()),
                    nodes,
                }
            })
            .collect()
    }

    fn replace_catalog(&mut self, catalog: DaemonCatalog) {
        let previous_groups =
            (self.catalog_revision == Some(catalog.revision)).then(|| self.catalog_groups.clone());
        let preserve_queued = self
            .queued_selections
            .iter()
            .all(|queued| queued.revision == catalog.revision);
        if !preserve_queued {
            self.queued_selections.clear();
        }
        self.catalog_revision = Some(catalog.revision);
        let primary_group_id = catalog
            .groups
            .iter()
            .find(|group| group.selected)
            .or_else(|| catalog.groups.first())
            .map(|group| group.id.clone());
        self.selected_catalog_group = self
            .selected_catalog_group
            .as_deref()
            .and_then(|selected| catalog.groups.iter().find(|group| group.id == selected))
            .or_else(|| catalog.groups.iter().find(|group| group.selected))
            .or_else(|| catalog.groups.first())
            .map(|group| group.id.clone());
        self.catalog_groups = catalog
            .groups
            .into_iter()
            .map(|group| {
                let old_group = previous_groups
                    .as_ref()
                    .and_then(|groups| groups.iter().find(|old| old.id == group.id));
                CatalogGroup {
                    is_primary: primary_group_id.as_deref() == Some(group.id.as_str()),
                    id: group.id,
                    label: safe_catalog_label(&group.label),
                    nodes: group
                        .nodes
                        .into_iter()
                        .map(|node| {
                            let old_node = old_group.and_then(|old| {
                                old.nodes.iter().find(|old_node| old_node.id == node.id)
                            });
                            // A real delay from the catalog is authoritative. When a
                            // same-revision reload omits it, preserve the complete last-known
                            // display tuple so internal data and visible text cannot diverge.
                            let (delay_ms, latency_text, latency_tone) =
                                if let Some(delay) = node.delay_ms {
                                    let (text, tone) = latency_display("ok", Some(delay));
                                    (Some(delay), text, tone)
                                } else if let Some(old) = old_node {
                                    (
                                        old.delay_ms,
                                        old.latency_text.clone(),
                                        old.latency_tone.clone(),
                                    )
                                } else {
                                    (None, String::new(), "neutral".into())
                                };
                            CatalogNodePresentation {
                                id: node.id,
                                label: safe_catalog_label(&node.label),
                                selected: node.selected,
                                delay_ms,
                                latency_text,
                                latency_tone,
                            }
                        })
                        .collect(),
                }
            })
            .collect();
        if !self.catalog_groups.is_empty() {
            self.latency_queued = true;
        }
        self.apply_restored_route_preferences();
        if preserve_queued {
            for queued in &self.queued_selections {
                if let Some(group) = self
                    .catalog_groups
                    .iter_mut()
                    .find(|group| group.id == queued.group_id)
                    && let Some(target_index) = group
                        .nodes
                        .iter()
                        .position(|node| node.id == queued.node_id)
                {
                    for node in &mut group.nodes {
                        node.selected = false;
                    }
                    group.nodes[target_index].selected = true;
                    if group.is_primary
                        && let UiState::Ready { node, .. } = &mut self.state
                    {
                        *node = group.nodes[target_index].label.clone();
                    }
                }
            }
        }
    }

    fn apply_restored_route_preferences(&mut self) {
        if self.catalog_groups.is_empty() {
            return;
        }
        if let Some(group_key) = self.restored_route_group.take()
            && let Some(group_id) = self.resolve_group_preference_id(&group_key)
        {
            self.selected_catalog_group = Some(group_id);
        }
        if self.restored_route_nodes.is_empty() {
            return;
        }
        let resolved: BTreeMap<_, _> = self
            .restored_route_nodes
            .iter()
            .filter_map(|(group, node)| self.resolve_route_preference_ids(group, node))
            .collect();
        for group in &mut self.catalog_groups {
            let Some(node_id) = resolved.get(&group.id) else {
                continue;
            };
            let Some(target_index) = group.nodes.iter().position(|node| node.id == *node_id) else {
                continue;
            };
            for node in &mut group.nodes {
                node.selected = false;
            }
            group.nodes[target_index].selected = true;
            if group.is_primary
                && let UiState::Ready { node, .. } = &mut self.state
            {
                *node = group.nodes[target_index].label.clone();
            }
        }
        self.restored_route_nodes.clear();
    }

    fn restore_selection(&mut self, active: &ActiveSelection) {
        if let Some(group) = self
            .catalog_groups
            .iter_mut()
            .find(|group| group.id == active.group_id)
        {
            for node in &mut group.nodes {
                node.selected = active
                    .previously_selected_node_ids
                    .iter()
                    .any(|id| id == &node.id);
            }
        }
    }

    fn sync_connected_route_from_catalog(&mut self) {
        let label = self
            .catalog_groups
            .iter()
            .find(|group| group.is_primary)
            .and_then(|group| group.nodes.iter().find(|node| node.selected))
            .map(|node| node.label.clone());
        if let (Some(label), UiState::Connected { node, .. }) = (label, &mut self.state) {
            *node = label;
        }
    }

    fn all_events(&self) -> Vec<PresentedEvent> {
        let mut events = self
            .diagnostics
            .as_ref()
            .map(|diagnostics| {
                diagnostics
                    .logs
                    .iter()
                    .map(|record| PresentedEvent {
                        id: record.id,
                        timestamp: record.timestamp.clone(),
                        component: match record.component.as_str() {
                            "xray" => "xray",
                            "mihomo" => "mihomo",
                            _ => "core",
                        }
                        .to_owned(),
                        level: match record.level.as_str() {
                            "error" => "error",
                            "warning" => "warning",
                            _ => "info",
                        }
                        .to_owned(),
                        message: redact_event_text(&record.message),
                    })
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        events.extend(self.event_cache.clone());
        if let Some(error) = &self.diagnostics_error {
            events.insert(0, error.clone());
        }
        if let Some(error) = &self.event_load_error {
            events.insert(0, error.clone());
        }
        events
    }

    /// A plain-text export of every currently-known event (ignoring the
    /// active tab filter), newest first, for a "copy diagnostics" action: a
    /// user hitting an opaque failure can paste this into a support message
    /// instead of just describing "не удалось добавить подписку". Every
    /// message here has already gone through the same redaction as the live
    /// feed, so this is exactly as safe to hand over as what is already on
    /// screen.
    pub(crate) fn diagnostics_export_text(&self) -> String {
        let mut events = self.all_events();
        events.sort_by_key(|event| std::cmp::Reverse(event.id));
        if events.is_empty() {
            return "Событий пока нет.".to_owned();
        }
        events
            .into_iter()
            .map(|event| {
                format!(
                    "[{}] {} {}: {}",
                    event.timestamp, event.level, event.component, event.message
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn filtered_events(&self) -> Vec<PresentedEvent> {
        let mut events = self.all_events();
        match self.event_filter {
            EventFilter::All => {}
            EventFilter::System => {
                events.retain(|event| !event.level.eq_ignore_ascii_case("error"));
            }
            EventFilter::Error => {
                events.retain(|event| event.level.eq_ignore_ascii_case("error"));
            }
            EventFilter::Cores => {
                events
                    .retain(|event| matches!(event.component.as_str(), "xray" | "mihomo" | "core"));
            }
        }
        events
    }
}

fn present_runtime_check(value: Option<&str>, loading: bool) -> RuntimeCheckPresentation {
    if loading && value.is_none() {
        return RuntimeCheckPresentation {
            value: "Проверка".into(),
            tone: "accent".into(),
        };
    }
    let (value, tone) = match value {
        Some("ready") => ("Готов", "success"),
        Some("starting") => ("Запуск", "warning"),
        Some("failed") => ("Сбой", "danger"),
        Some("unsupported") => ("Без проверки", "neutral"),
        Some("disabled") => ("Выключен", "neutral"),
        Some("stopped") => ("Выкл", "neutral"),
        Some(_) => ("Неизвестно", "danger"),
        None => ("Нет данных", "neutral"),
    };
    RuntimeCheckPresentation {
        value: value.into(),
        tone: tone.into(),
    }
}

fn latency_display(status: &str, latency_ms: Option<u64>) -> (String, String) {
    match (status, latency_ms) {
        // Any answered ping is a working node: the exact millisecond count
        // isn't a failure signal, so it stays green regardless of value.
        ("ok", Some(value)) => (format!("{value} ms"), "success".into()),
        ("timeout", _) => ("таймаут".into(), "danger".into()),
        ("error" | "unavailable", _) => ("нет связи".into(), "danger".into()),
        _ => ("нет данных".into(), "neutral".into()),
    }
}

fn safe_catalog_label(label: &str) -> String {
    let label = redact_event_text(label)
        .replace(['\r', '\n'], " ")
        .trim()
        .to_owned();
    if label.is_empty() {
        "Без названия".into()
    } else {
        label
    }
}

fn current_unix_time() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

const MIN_AUTO_REFRESH_SECS: u64 = 15 * 60;
const DEFAULT_AUTO_REFRESH_SECS: u64 = 6 * 60 * 60;
const MAX_AUTO_REFRESH_SECS: u64 = 30 * 24 * 60 * 60;

fn subscription_refresh_interval(info: &DaemonSubscriptionInfo) -> u64 {
    info.refresh_interval_secs
        .unwrap_or(DEFAULT_AUTO_REFRESH_SECS)
        .clamp(MIN_AUTO_REFRESH_SECS, MAX_AUTO_REFRESH_SECS)
}

fn format_update_date(updated_at_unix: u64) -> String {
    if updated_at_unix == 0 {
        return "Не обновлялась".into();
    }

    // Convert whole UTC days since the Unix epoch to a Gregorian date without
    // pulling a date-time dependency into the desktop shell.
    let days = (updated_at_unix / 86_400) as i64;
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
    let month = month_prime + if month_prime < 10 { 3 } else { -9 };
    if month <= 2 {
        year += 1;
    }
    format!("{day:02}.{month:02}.{year:04}")
}

fn format_gib(bytes: u64) -> String {
    format!("{:.1} GB", bytes as f64 / 1024_f64.powi(3))
}

#[allow(dead_code)] // Used by profile presentation methods wired in Task 6.
fn present_subscription_profile(
    profile: DaemonSubscriptionProfile,
) -> Option<SubscriptionProfilePresentation> {
    let id = safe_profile_id(&profile.id)?;
    let redacted_title = redact_event_text(&profile.display_name);
    let title = bounded_plain_text(&redacted_title, 128).unwrap_or_else(|| "Подписка".into());
    let usage = match (profile.used_bytes, profile.total_bytes) {
        (Some(used), Some(total)) => format!("{} / {}", format_gib(used), format_gib(total)),
        (Some(used), None) => format_gib(used),
        (None, Some(total)) => format!("— / {}", format_gib(total)),
        (None, None) => "Нет данных".into(),
    };
    let expiry = format_expiry(profile.expires_at_unix, current_unix_time());
    Some(SubscriptionProfilePresentation {
        id,
        active: profile.active,
        available: profile.available,
        title,
        usage,
        expiry,
        service_logo_path: profile
            .service_logo_path
            .as_deref()
            .and_then(local_logo_path),
    })
}

fn safe_profile_id(value: &str) -> Option<String> {
    let value = value.trim();
    (!value.is_empty()
        && value.len() <= 128
        && value.is_ascii()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':')))
    .then(|| value.to_owned())
}

fn valid_subscription_url(value: &str) -> bool {
    let value = value.trim();
    value.len() <= MAX_SUBSCRIPTION_URL_BYTES
        && reqwest::Url::parse(value)
            .is_ok_and(|url| matches!(url.scheme(), "http" | "https") && url.host_str().is_some())
}

#[allow(dead_code)] // Used by profile presentation methods wired in Task 6.
fn profile_mutation_error(error: &crate::daemon::DaemonError, action: &str) -> String {
    if error.is_profile_conflict() {
        return "Отключитесь перед изменением подписки.".into();
    }
    if error.is_profile_not_found() {
        return "Подписка больше недоступна. Обновите список.".into();
    }
    let action = bounded_plain_text(action, 32).unwrap_or_else(|| "изменить".into());
    format!("Не удалось {action} подписку. Повторите попытку.")
}

fn profile_result_may_have_committed<T>(result: &Result<T, crate::daemon::DaemonError>) -> bool {
    result
        .as_ref()
        .map(|_| true)
        .unwrap_or_else(|error| error.requires_reconciliation())
}

fn bounded_plain_text(value: &str, max_chars: usize) -> Option<String> {
    let filtered = value
        .chars()
        .filter(|character| {
            !character.is_control()
                && !matches!(
                    character,
                    '\u{061c}'
                        | '\u{200e}'
                        | '\u{200f}'
                        | '\u{202a}'..='\u{202e}'
                        | '\u{2066}'..='\u{2069}'
                )
        })
        .collect::<String>();
    let bounded = filtered
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(max_chars)
        .collect::<String>();
    (!bounded.is_empty()).then_some(bounded)
}

fn is_non_file_reference(value: &str) -> bool {
    let lowercase = value.to_ascii_lowercase();
    value.contains("://") || lowercase.starts_with("data:")
}

#[cfg(windows)]
fn local_logo_path(value: &str) -> Option<String> {
    use std::path::{Component, Prefix};

    let value = value.trim();
    if value.is_empty() || value.len() > MAX_SERVICE_LOGO_PATH_BYTES || is_non_file_reference(value)
    {
        return None;
    }
    let path = Path::new(value);
    let is_local_drive = matches!(
        path.components().next(),
        Some(Component::Prefix(prefix)) if matches!(prefix.kind(), Prefix::Disk(_))
    );
    (is_local_drive && path.is_absolute()).then(|| value.to_owned())
}

#[cfg(not(windows))]
fn local_logo_path(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() || value.len() > MAX_SERVICE_LOGO_PATH_BYTES || is_non_file_reference(value)
    {
        return None;
    }
    Path::new(value).is_absolute().then(|| value.to_owned())
}

/// Providers mark unlimited subscriptions with an expiry decades away; a
/// real one reported "Осталось 12073 дня".
const UNLIMITED_EXPIRY_DAYS: u64 = 10 * 365;

fn format_expiry(expires_at_unix: Option<u64>, now_unix: u64) -> String {
    let Some(expires) = expires_at_unix else {
        return "Срок неизвестен".into();
    };
    if expires <= now_unix {
        return "Срок истёк".into();
    }
    let days = (expires - now_unix).div_ceil(86_400);
    if days > UNLIMITED_EXPIRY_DAYS {
        "Бессрочно".into()
    } else {
        format!("Осталось {days} {}", russian_days(days))
    }
}

fn russian_days(days: u64) -> &'static str {
    let last_two = days % 100;
    let last = days % 10;
    if (11..=14).contains(&last_two) {
        "дней"
    } else if last == 1 {
        "день"
    } else if (2..=4).contains(&last) {
        "дня"
    } else {
        "дней"
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CatalogDisplayKind {
    Group,
    Node,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CatalogDisplay {
    pub(crate) icon: String,
    pub(crate) label: String,
}

pub(crate) fn derive_catalog_display(label: &str, kind: CatalogDisplayKind) -> CatalogDisplay {
    let value = label.trim();
    // Any leading emoji (a flag, a ZWJ sequence, a skin-toned hand, ...)
    // becomes the icon, drawn as an embedded image by `emoji_images`.
    if let Some(end) = crate::emoji_images::leading_emoji_len(value) {
        return CatalogDisplay {
            icon: value[..end].to_owned(),
            label: trim_catalog_decorators(&value[end..]).to_owned(),
        };
    }

    for prefix in [
        "🌍", "🌎", "🌏", "🌐", "🎮", "🔄", "↻", "▶️", "▶", "📺", "🎬", "⛔", "🚫", "↘", "⚙️", "⚙",
    ] {
        if let Some(rest) = value.strip_prefix(prefix) {
            return CatalogDisplay {
                icon: prefix.to_owned(),
                label: trim_catalog_decorators(rest).to_owned(),
            };
        }
    }

    let lowercase = value.to_lowercase();
    let icon = if lowercase.contains("minecraft")
        || lowercase.contains("game")
        || lowercase.contains("игр")
    {
        "🎮"
    } else if lowercase.contains("youtube")
        || lowercase.contains("video")
        || lowercase.contains("media")
        || lowercase.contains("стрим")
    {
        "▶"
    } else if lowercase.contains("без vpn")
        || lowercase.contains("direct")
        || lowercase.contains("прям")
    {
        "↗"
    } else if lowercase.contains("auto") || lowercase.contains("авто") {
        "↻"
    } else {
        match kind {
            CatalogDisplayKind::Group => "◎",
            CatalogDisplayKind::Node => "⚙",
        }
    };

    CatalogDisplay {
        icon: icon.to_owned(),
        label: value.to_owned(),
    }
}

/// Splits a leading flag or known symbol off `label` so it can be drawn with
/// the emoji font (the UI text font renders regional-indicator flags as
/// plain letters, e.g. "SE"). Unlike `derive_catalog_display`, no icon is
/// ever invented: a label without such a prefix comes back unchanged.
pub(crate) fn split_leading_symbol(label: &str) -> (String, String) {
    let value = label.trim();
    let display = derive_catalog_display(value, CatalogDisplayKind::Node);
    if display.label == value {
        (String::new(), label.to_owned())
    } else {
        (display.icon, display.label)
    }
}

fn trim_catalog_decorators(value: &str) -> &str {
    let trimmed = value.trim_start_matches(|character: char| {
        character.is_whitespace()
            || matches!(
                character,
                '-' | '–' | '—' | ':' | '|' | '·' | '_' | '/' | '.' | ','
            )
    });
    if trimmed.is_empty() {
        value.trim()
    } else {
        trimmed
    }
}

/// What the home toggle's exit line shows for a live session. Empty strings mean "not known
/// yet" (no second sample, or Mihomo's counters were unreadable) and are left out by the UI.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct SessionLine {
    pub(crate) uptime: String,
    pub(crate) down: String,
    pub(crate) up: String,
}

/// Turns successive `/v1/session` samples into uptime and current speed. Speed is the byte
/// delta between two samples over the time between them.
#[derive(Debug, Default)]
pub(crate) struct SessionTraffic {
    previous: Option<TrafficSample>,
}

#[derive(Clone, Copy, Debug)]
struct TrafficSample {
    at: std::time::Instant,
    connected_since_unix_ms: u64,
    download: u64,
    upload: u64,
}

impl SessionTraffic {
    pub(crate) fn reset(&mut self) {
        self.previous = None;
    }

    pub(crate) fn sample(
        &mut self,
        session: &DaemonSession,
        at: std::time::Instant,
        now_unix_ms: u64,
    ) -> SessionLine {
        let uptime = format_uptime(now_unix_ms.saturating_sub(session.connected_since_unix_ms));
        let (Some(download), Some(upload)) = (session.download_total, session.upload_total) else {
            // Keep the last good sample: the next readable one still yields a real rate.
            return SessionLine {
                uptime,
                ..SessionLine::default()
            };
        };
        let current = TrafficSample {
            at,
            connected_since_unix_ms: session.connected_since_unix_ms,
            download,
            upload,
        };
        let previous = self.previous.replace(current);
        let Some(previous) = previous
            .filter(|previous| previous.connected_since_unix_ms == current.connected_since_unix_ms)
        else {
            return SessionLine {
                uptime,
                ..SessionLine::default()
            };
        };
        let seconds = at.saturating_duration_since(previous.at).as_secs_f64();
        if seconds <= 0.0 {
            return SessionLine {
                uptime,
                ..SessionLine::default()
            };
        }
        // Counters restart with Mihomo; a smaller value is a reset, not negative traffic.
        let rate = |now: u64, before: u64| now.saturating_sub(before) as f64 / seconds;
        SessionLine {
            uptime,
            down: format_speed(rate(download, previous.download)),
            up: format_speed(rate(upload, previous.upload)),
        }
    }
}

/// "00:12:43"; hours keep growing past 99 instead of wrapping.
pub(crate) fn format_uptime(elapsed_ms: u64) -> String {
    let seconds = elapsed_ms / 1000;
    format!(
        "{:02}:{:02}:{:02}",
        seconds / 3600,
        seconds / 60 % 60,
        seconds % 60
    )
}

/// "0 Б/с", "320 КБ/с", "2.4 МБ/с": binary units, one decimal below 10, whole numbers above.
pub(crate) fn format_speed(bytes_per_second: f64) -> String {
    const UNITS: [&str; 4] = ["Б/с", "КБ/с", "МБ/с", "ГБ/с"];
    let mut value = bytes_per_second.max(0.0);
    let mut unit = 0;
    while unit + 1 < UNITS.len() && value >= 1024.0 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 && value.round() < 1024.0 {
        return format!("{} {}", value.round(), UNITS[0]);
    }
    if unit == 0 {
        return format!("1.0 {}", UNITS[1]);
    }
    if value < 10.0 && format!("{value:.1}") != "10.0" {
        format!("{value:.1} {}", UNITS[unit])
    } else if value.round() >= 1024.0 && unit + 1 < UNITS.len() {
        format!("1.0 {}", UNITS[unit + 1])
    } else {
        format!("{} {}", value.round(), UNITS[unit])
    }
}

#[cfg(test)]
mod session_traffic_tests {
    use std::time::{Duration, Instant};

    use super::{DaemonSession, SessionLine, SessionTraffic, format_speed, format_uptime};

    fn session(since: u64, down: Option<u64>, up: Option<u64>) -> DaemonSession {
        DaemonSession {
            connected_since_unix_ms: since,
            download_total: down,
            upload_total: up,
        }
    }

    #[test]
    fn uptime_is_hours_minutes_seconds() {
        assert_eq!(format_uptime(0), "00:00:00");
        assert_eq!(format_uptime(763_999), "00:12:43");
        assert_eq!(format_uptime(3_600_000), "01:00:00");
        assert_eq!(format_uptime(360_000_000), "100:00:00");
    }

    #[test]
    fn speed_uses_binary_russian_units_and_one_decimal_below_ten() {
        assert_eq!(format_speed(0.0), "0 Б/с");
        assert_eq!(format_speed(-5.0), "0 Б/с");
        assert_eq!(format_speed(512.0), "512 Б/с");
        assert_eq!(format_speed(1023.4), "1023 Б/с");
        assert_eq!(format_speed(1023.6), "1.0 КБ/с");
        assert_eq!(format_speed(1024.0), "1.0 КБ/с");
        assert_eq!(format_speed(320.0 * 1024.0), "320 КБ/с");
        assert_eq!(format_speed(2.4 * 1024.0 * 1024.0), "2.4 МБ/с");
        assert_eq!(format_speed(9.99 * 1024.0), "10 КБ/с");
        assert_eq!(format_speed(1023.7 * 1024.0), "1.0 МБ/с");
        assert_eq!(format_speed(3.0 * 1024.0 * 1024.0 * 1024.0), "3.0 ГБ/с");
    }

    #[test]
    fn speed_is_the_delta_between_samples_and_a_counter_reset_reads_as_zero() {
        let mut traffic = SessionTraffic::default();
        let start = Instant::now();
        let first = traffic.sample(&session(1_000, Some(10_000), Some(500)), start, 61_000);
        assert_eq!(
            first,
            SessionLine {
                uptime: "00:01:00".into(),
                down: String::new(),
                up: String::new(),
            }
        );

        let second = traffic.sample(
            &session(1_000, Some(10_000 + 2 * 1024 * 1024), Some(500 + 2048)),
            start + Duration::from_secs(2),
            63_000,
        );
        assert_eq!(second.uptime, "00:01:02");
        assert_eq!(second.down, "1.0 МБ/с");
        assert_eq!(second.up, "1.0 КБ/с");

        // Mihomo restarted its counters: never negative, just zero for this interval.
        let reset = traffic.sample(
            &session(1_000, Some(100), Some(0)),
            start + Duration::from_secs(3),
            64_000,
        );
        assert_eq!(reset.down, "0 Б/с");
        assert_eq!(reset.up, "0 Б/с");
    }

    #[test]
    fn unreadable_counters_and_a_new_session_restart_the_rate() {
        let mut traffic = SessionTraffic::default();
        let start = Instant::now();
        traffic.sample(&session(1_000, Some(0), Some(0)), start, 2_000);

        let unreadable = traffic.sample(
            &session(1_000, None, None),
            start + Duration::from_secs(1),
            3_000,
        );
        assert_eq!(unreadable.uptime, "00:00:02");
        assert!(unreadable.down.is_empty() && unreadable.up.is_empty());

        // A reconnect is a new session: its first sample has no rate yet.
        let reconnected = traffic.sample(
            &session(9_000, Some(50_000), Some(50_000)),
            start + Duration::from_secs(2),
            10_000,
        );
        assert_eq!(reconnected.uptime, "00:00:01");
        assert!(reconnected.down.is_empty());

        traffic.reset();
        let after_reset = traffic.sample(
            &session(9_000, Some(60_000), Some(60_000)),
            start + Duration::from_secs(3),
            11_000,
        );
        assert!(after_reset.down.is_empty());
    }
}

#[cfg(test)]
mod tests {
    use std::{
        sync::{Mutex, mpsc},
        thread,
    };

    use super::*;
    use crate::daemon::{
        DaemonCatalog, DaemonCatalogGroup, DaemonCatalogNode, DaemonDiagnosticLog, DaemonError,
        DaemonLatencies, DaemonLatencyEntry, DaemonProxyMapping, DaemonSubscriptionInfo,
        test_support::MockDaemonClient,
    };

    fn profile(id: &str, active: bool, title: &str) -> DaemonSubscriptionProfile {
        DaemonSubscriptionProfile {
            id: id.into(),
            active,
            available: true,
            display_name: title.into(),
            used_bytes: Some(1024),
            total_bytes: Some(2048),
            expires_at_unix: None,
            service_logo_path: None,
        }
    }

    fn profile_list(
        active_profile_id: Option<&str>,
        profiles: Vec<DaemonSubscriptionProfile>,
    ) -> DaemonSubscriptionList {
        DaemonSubscriptionList {
            active_profile_id: active_profile_id.map(str::to_owned),
            profiles,
        }
    }

    fn model_with_profiles() -> DesktopViewModel {
        let mut model = DesktopViewModel::new(Arc::new(MockDaemonClient::ready()));
        let refresh = model.begin_refresh();
        model.finish_refresh(
            refresh,
            Ok(DaemonStatus {
                state: "ready".into(),
                active_profile_id: Some("profile-a".into()),
                profile: Some("Visible title must not be a key".into()),
                current_node: Some("Auto".into()),
                ..DaemonStatus::default()
            }),
        );
        let list = model.begin_profile_list().expect("profile list");
        assert!(model.finish_profile_list(
            list,
            Ok(profile_list(
                Some("profile-a"),
                vec![
                    profile("profile-a", true, "Alpha"),
                    profile("profile-b", false, "Beta"),
                ],
            )),
        ));
        model
    }

    #[test]
    fn profile_list_generation_suppresses_only_older_list_responses() {
        let mut model = model_with_profiles();
        let old = model.begin_profile_list().expect("older profile list");
        let newer = model.begin_profile_list().expect("newer profile list");

        assert!(!model.finish_profile_list(
            old,
            Ok(profile_list(
                Some("profile-b"),
                vec![profile("profile-b", true, "Stale Beta")],
            )),
        ));
        assert!(model.finish_profile_list(
            newer,
            Ok(profile_list(
                Some("profile-a"),
                vec![
                    profile("profile-a", true, "Alpha newest"),
                    profile("profile-b", false, "Beta"),
                ],
            )),
        ));
        let presentation = model.profile_presentation();
        assert_eq!(presentation.active_id.as_deref(), Some("profile-a"));
        assert_eq!(presentation.profiles[0].title, "Alpha newest");
    }

    #[test]
    fn list_during_activation_never_discards_or_overwrites_the_committing_mutation() {
        let mut model = model_with_profiles();
        let activation = model
            .begin_profile_activation("profile-b")
            .expect("activation");
        let precommit_list = model.begin_profile_list().expect("parallel profile list");
        assert!(model.finish_profile_list(
            precommit_list,
            Ok(profile_list(
                Some("profile-a"),
                vec![
                    profile("profile-a", true, "Alpha before commit"),
                    profile("profile-b", false, "Beta before commit"),
                ],
            )),
        ));

        assert!(model.finish_profile_activation(
            activation,
            Ok(DaemonStatus {
                state: "ready".into(),
                active_profile_id: Some("profile-b".into()),
                profile: Some("Beta".into()),
                ..DaemonStatus::default()
            }),
        ));
        let presentation = model.profile_presentation();
        assert_eq!(presentation.active_id.as_deref(), Some("profile-b"));
        assert!(!presentation.profiles[0].active);
        assert!(presentation.profiles[1].active);
        assert!(model.take_profile_list_reload_needed());

        let mut late_list_model = model_with_profiles();
        let activation = late_list_model
            .begin_profile_activation("profile-b")
            .expect("activation");
        let stale_list = late_list_model
            .begin_profile_list()
            .expect("precommit list");
        assert!(late_list_model.finish_profile_activation(
            activation,
            Ok(DaemonStatus {
                state: "ready".into(),
                active_profile_id: Some("profile-b".into()),
                ..DaemonStatus::default()
            }),
        ));
        assert!(!late_list_model.finish_profile_list(
            stale_list,
            Ok(profile_list(
                Some("profile-a"),
                vec![profile("profile-a", true, "Stale Alpha")],
            )),
        ));
        assert_eq!(
            late_list_model.profile_presentation().active_id.as_deref(),
            Some("profile-b")
        );
    }

    #[test]
    fn status_captured_before_activation_cannot_restore_the_old_active_profile() {
        let mut model = model_with_profiles();
        let old_status = model.begin_refresh();
        let activation = model
            .begin_profile_activation("profile-b")
            .expect("activation supersedes status polling");

        model.finish_refresh(
            old_status,
            Ok(DaemonStatus {
                state: "ready".into(),
                active_profile_id: Some("profile-c".into()),
                profile: Some("Captured stale profile".into()),
                ..DaemonStatus::default()
            }),
        );
        assert_eq!(
            model.profile_presentation().active_id.as_deref(),
            Some("profile-a")
        );
        assert_ne!(model.presentation().profile, "Captured stale profile");
        assert!(model.finish_profile_activation(
            activation,
            Ok(DaemonStatus {
                state: "ready".into(),
                active_profile_id: Some("profile-b".into()),
                profile: Some("Beta".into()),
                ..DaemonStatus::default()
            }),
        ));
        model.finish_refresh(
            old_status,
            Ok(DaemonStatus {
                state: "ready".into(),
                active_profile_id: Some("profile-a".into()),
                profile: Some("Old Alpha".into()),
                ..DaemonStatus::default()
            }),
        );
        assert_eq!(
            model.profile_presentation().active_id.as_deref(),
            Some("profile-b")
        );
        assert_eq!(model.presentation().profile, "Beta");
    }

    #[test]
    fn activation_invalidates_old_catalog_latency_selection_and_worker_tokens() {
        let mut model = model_with_profiles();
        let old_catalog = model.begin_catalog_request().expect("old catalog request");
        let activation = model
            .begin_profile_activation("profile-b")
            .expect("activation");
        assert!(model.finish_profile_activation(
            activation,
            Ok(DaemonStatus {
                state: "ready".into(),
                active_profile_id: Some("profile-b".into()),
                profile: Some("Beta".into()),
                ..DaemonStatus::default()
            }),
        ));

        assert!(
            model
                .finish_catalog(old_catalog.token, Ok(catalog_fixture()))
                .is_none()
        );
        assert!(model.catalog_presentation().groups.is_empty());
        assert!(model.take_profile_list_reload_needed());
        assert_eq!(
            model.profile_presentation().active_id.as_deref(),
            Some("profile-b")
        );
        let rows = model.profile_presentation().profiles;
        assert!(!rows[0].active);
        assert!(rows[1].active);

        let failed_reload = model.begin_profile_list().expect("failed list reload");
        assert!(model.finish_profile_list(
            failed_reload,
            Err(crate::daemon::DaemonError::new("unsafe list details")),
        ));
        let rows_after_failure = model.profile_presentation().profiles;
        assert!(!rows_after_failure[0].active);
        assert!(rows_after_failure[1].active);
    }

    #[test]
    fn active_delete_immediately_reconciles_cached_row_flags() {
        let mut model = model_with_profiles();
        let deletion = model
            .begin_profile_delete("profile-a")
            .expect("delete active profile");

        assert!(model.finish_profile_delete(
            deletion,
            Ok(DaemonStatus {
                state: "ready".into(),
                active_profile_id: Some("profile-b".into()),
                profile: Some("Beta".into()),
                ..DaemonStatus::default()
            }),
        ));

        let presentation = model.profile_presentation();
        assert_eq!(presentation.active_id.as_deref(), Some("profile-b"));
        assert_eq!(presentation.profiles.len(), 1);
        assert_eq!(presentation.profiles[0].id, "profile-b");
        assert!(presentation.profiles[0].active);
    }

    #[test]
    fn route_preferences_are_keyed_by_authoritative_opaque_profile_id() {
        let mut model = model_with_profiles();
        let catalog = model.begin_catalog_request().expect("catalog request");
        model.finish_catalog(catalog.token, Ok(catalog_fixture()));

        let (profile_key, _, _) = model.route_preferences().expect("route preferences");

        assert_eq!(profile_key, "profile-a");
        assert_ne!(profile_key, model.presentation().profile);
    }

    #[test]
    fn deleting_last_profile_yields_empty_rows_and_close_intent() {
        let mut model = model_with_profiles();
        let list = model.begin_profile_list().expect("single profile list");
        model.finish_profile_list(
            list,
            Ok(profile_list(
                Some("profile-a"),
                vec![profile("profile-a", true, "Alpha")],
            )),
        );
        let deletion = model
            .begin_profile_delete("profile-a")
            .expect("delete last profile");

        assert!(model.finish_profile_delete(
            deletion,
            Ok(DaemonStatus {
                state: "empty".into(),
                active_profile_id: None,
                ..DaemonStatus::default()
            }),
        ));
        let presentation = model.profile_presentation();
        assert_eq!(presentation.active_id, None);
        assert!(presentation.profiles.is_empty());
        assert!(model.take_profile_close_intent());
        assert!(!model.take_profile_close_intent());
    }

    #[test]
    fn inactive_targeted_refresh_updates_only_matching_metadata_until_list_reload() {
        let mut model = model_with_profiles();
        let refresh = model
            .begin_profile_target_refresh("profile-b")
            .expect("targeted refresh");

        assert!(model.finish_profile_target_refresh(
            refresh,
            Ok(profile_list(
                Some("profile-b"),
                vec![
                    profile("profile-a", false, "Wrong speculative update"),
                    profile("profile-b", true, "Beta updated"),
                ],
            )),
        ));
        let presentation = model.profile_presentation();
        assert_eq!(presentation.active_id.as_deref(), Some("profile-a"));
        assert_eq!(presentation.profiles[0].title, "Alpha");
        assert!(presentation.profiles[0].active);
        assert_eq!(presentation.profiles[1].title, "Beta updated");
        assert!(!presentation.profiles[1].active);
        assert!(model.take_profile_list_reload_needed());
        assert!(!model.take_profile_catalog_reload_needed());
    }

    #[test]
    fn profile_source_edit_validates_url_and_reconciles_the_target_profile() {
        let mut model = model_with_profiles();
        assert!(
            model
                .begin_profile_source_edit("profile-a", "not a URL")
                .is_none()
        );
        assert_eq!(
            model.profile_presentation().mutation_error.as_deref(),
            Some("Введите корректный HTTP(S) URL.")
        );
        assert_eq!(model.profile_presentation().error, None);

        let edit = model
            .begin_profile_source_edit("profile-a", "https://replacement.invalid/sub?token=secret")
            .expect("start source edit");
        let pending = model.profile_presentation();
        assert_eq!(
            pending.pending_operation,
            Some(SubscriptionProfileOperation::Edit)
        );
        assert_eq!(pending.pending_id.as_deref(), Some("profile-a"));

        assert!(model.finish_profile_source_edit(
            edit,
            Ok(profile_list(
                Some("profile-a"),
                vec![
                    profile("profile-a", true, "Alpha replaced"),
                    profile("profile-b", false, "Beta"),
                ],
            )),
        ));
        assert_eq!(
            model.profile_presentation().profiles[0].title,
            "Alpha replaced"
        );
        assert!(model.profile_reconciliation_required());
    }

    #[test]
    fn ambiguous_profile_source_edit_forces_full_authoritative_reconciliation() {
        let mut model = model_with_profiles();
        let edit = model
            .begin_profile_source_edit("profile-b", "https://replacement.invalid/sub")
            .expect("start source edit");

        assert!(!model.finish_profile_source_edit(
            edit,
            Err(DaemonError::ambiguous("private committed marker")),
        ));

        assert!(model.profile_reconciliation_required());
        assert!(model.take_profile_status_reload_needed());
        assert!(model.take_profile_list_reload_needed());
        assert!(model.take_profile_catalog_reload_needed());
        assert!(
            !format!("{:?}", model.profile_presentation()).contains("private committed marker")
        );
    }

    #[test]
    fn active_targeted_refresh_invalidates_old_catalog_and_latency_workers() {
        let mut catalog_model = model_with_profiles();
        catalog_model.subscription = Some(DaemonSubscriptionInfo {
            display_name: "Stale active shelf".into(),
            total_bytes: Some(42),
            ..DaemonSubscriptionInfo::default()
        });
        let old_catalog = catalog_model
            .begin_catalog_request()
            .expect("in-flight old catalog");
        let refresh = catalog_model
            .begin_profile_target_refresh("profile-a")
            .expect("active targeted refresh");
        assert!(catalog_model.finish_profile_target_refresh(
            refresh,
            Ok(profile_list(
                Some("profile-a"),
                vec![profile("profile-a", true, "Alpha refreshed")],
            )),
        ));
        assert!(
            catalog_model
                .finish_catalog(old_catalog.token, Ok(catalog_fixture()))
                .is_none()
        );
        assert!(catalog_model.catalog_presentation().groups.is_empty());
        assert!(catalog_model.subscription.is_none());
        assert!(catalog_model.take_profile_status_reload_needed());
        assert!(catalog_model.take_profile_list_reload_needed());
        assert!(catalog_model.take_profile_catalog_reload_needed());

        let mut latency_model = model_with_profiles();
        let catalog = latency_model
            .begin_catalog_request()
            .expect("catalog before latency");
        latency_model.finish_catalog(catalog.token, Ok(catalog_fixture()));
        latency_model.state = UiState::Connected {
            profile: "Alpha".into(),
            node: "Auto".into(),
        };
        let old_latency = latency_model
            .begin_latency_request()
            .expect("in-flight old latency");
        latency_model.state = UiState::Ready {
            profile: "Alpha".into(),
            node: "Auto".into(),
        };
        let refresh = latency_model
            .begin_profile_target_refresh("profile-a")
            .expect("active refresh after disconnect");
        assert!(latency_model.finish_profile_target_refresh(
            refresh,
            Ok(profile_list(
                Some("profile-a"),
                vec![profile("profile-a", true, "Alpha refreshed")],
            )),
        ));
        assert!(
            latency_model
                .finish_latencies(old_latency.token, Ok(DaemonLatencies::default()))
                .is_none()
        );
        assert!(latency_model.catalog_presentation().groups.is_empty());
        assert!(latency_model.take_profile_catalog_reload_needed());
    }

    #[test]
    fn profile_presentation_debug_never_contains_provider_urls() {
        let mut model = model_with_profiles();
        let list = model.begin_profile_list().expect("unsafe profile list");
        let mut unsafe_profile = profile("profile-a", true, "https://private.invalid/?token=x");
        unsafe_profile.service_logo_path = Some("https://private.invalid/logo.png".into());
        model.finish_profile_list(
            list,
            Ok(profile_list(Some("profile-a"), vec![unsafe_profile])),
        );

        let debug = format!("{:?}", model.profile_presentation());
        assert!(!debug.contains("https://"));
        assert!(!debug.contains("private.invalid"));
        assert!(!debug.contains("token=x"));
    }

    #[test]
    fn profile_rows_and_provider_titles_are_bounded_and_remote_logos_are_rejected() {
        let mut model = model_with_profiles();
        let list = model.begin_profile_list().expect("bounded profile list");
        let profiles = (0..32)
            .map(|index| {
                let mut item = profile(&format!("profile-{index}"), index == 0, &"x".repeat(512));
                item.service_logo_path = Some(if index == 1 {
                    format!(
                        r"C:\cache\{}\logo.png",
                        "x".repeat(MAX_SERVICE_LOGO_PATH_BYTES)
                    )
                } else {
                    "https://private.invalid/logo.png".into()
                });
                item
            })
            .collect();
        model.finish_profile_list(list, Ok(profile_list(Some("profile-0"), profiles)));

        let presentation = model.profile_presentation();
        assert_eq!(presentation.profiles.len(), MAX_SUBSCRIPTION_PROFILES);
        assert!(
            presentation
                .profiles
                .iter()
                .all(|profile| profile.title.chars().count() <= 128)
        );
        assert!(
            presentation
                .profiles
                .iter()
                .all(|profile| profile.service_logo_path.is_none())
        );
    }

    #[test]
    fn import_success_exposes_reload_intents_without_gating_the_current_shell() {
        let mut model = model_with_profiles();
        let old_catalog = model.begin_catalog_request().expect("old catalog request");
        let import = model
            .begin_import("https://subscription.invalid/new")
            .expect("import");

        model.finish_import(
            import,
            Ok(DaemonStatus {
                state: "ready".into(),
                active_profile_id: Some("profile-new".into()),
                profile: Some("New".into()),
                ..DaemonStatus::default()
            }),
        );

        assert!(
            model
                .finish_catalog(old_catalog.token, Ok(catalog_fixture()))
                .is_none()
        );
        assert!(model.catalog_presentation().groups.is_empty());
        assert!(model.profile_presentation().profiles.is_empty());
        assert_eq!(
            model.profile_presentation().active_id.as_deref(),
            Some("profile-new")
        );
        assert!(model.take_profile_list_reload_needed());
        assert!(model.take_profile_catalog_reload_needed());
        assert!(!model.profile_reconciliation_required());
        assert!(model.presentation().primary_enabled);
        assert!(model.begin_primary_action().is_some());

        let main = include_str!("main.rs");
        let import_worker = main
            .split("fn run_import(")
            .nth(1)
            .and_then(|source| source.split("fn run_connect(").next())
            .expect("run_import source");
        assert!(import_worker.contains("model.finish_import(mutation, result)"));
        assert!(!import_worker.contains("begin_profile_list"));
    }

    #[test]
    fn connected_profile_refresh_and_delete_issue_tokens_but_activation_stays_blocked() {
        let mut model = model_with_profiles();
        let refresh = model.begin_refresh();
        model.finish_refresh(
            refresh,
            Ok(DaemonStatus {
                state: "connected".into(),
                active_profile_id: Some("profile-a".into()),
                profile: Some("Alpha".into()),
                ..DaemonStatus::default()
            }),
        );

        assert!(model.begin_profile_activation("profile-b").is_none());
        assert!(model.begin_profile_target_refresh("profile-b").is_some());

        let mut deletion = model_with_profiles();
        let refresh = deletion.begin_refresh();
        deletion.finish_refresh(
            refresh,
            Ok(DaemonStatus {
                state: "connected".into(),
                active_profile_id: Some("profile-a".into()),
                profile: Some("Alpha".into()),
                ..DaemonStatus::default()
            }),
        );
        assert!(deletion.begin_profile_delete("profile-b").is_some());
    }

    #[test]
    fn connected_state_locks_profile_switching_and_reports_it_as_a_mutation_error() {
        let mut model = model_with_profiles();
        assert!(!model.profile_presentation().switch_locked);
        let refresh = model.begin_refresh();
        model.finish_refresh(
            refresh,
            Ok(DaemonStatus {
                state: "connected".into(),
                active_profile_id: Some("profile-a".into()),
                profile: Some("Alpha".into()),
                ..DaemonStatus::default()
            }),
        );

        assert!(model.profile_presentation().switch_locked);
        assert!(model.begin_profile_activation("profile-b").is_none());
        let presentation = model.profile_presentation();
        assert_eq!(presentation.error, None);
        assert_eq!(
            presentation.mutation_error.as_deref(),
            Some("Отключитесь перед изменением подписки.")
        );
    }

    #[test]
    fn profile_list_failure_is_retryable_and_cleared_by_the_next_list_request() {
        let mut model = model_with_profiles();
        let list = model.begin_profile_list().expect("profile list");
        assert!(
            model.finish_profile_list(list, Err(crate::daemon::DaemonError::new("unavailable")),)
        );
        let presentation = model.profile_presentation();
        assert_eq!(
            presentation.error.as_deref(),
            Some("Не удалось загрузить подписки. Повторите попытку.")
        );
        assert_eq!(presentation.mutation_error, None);

        model.begin_profile_list().expect("retry list");
        assert_eq!(model.profile_presentation().error, None);
    }

    #[test]
    fn failed_status_poll_reports_an_unreachable_service_not_a_failed_connection() {
        let mut model = DesktopViewModel::new(Arc::new(MockDaemonClient::ready()));
        let refresh = model.begin_refresh();
        model.finish_refresh(refresh, Err(crate::daemon::DaemonError::new("unavailable")));
        let presentation = model.presentation();
        assert_eq!(presentation.eyebrow, "Нет связи с сервисом");
        assert_eq!(presentation.primary_label, "Проверить снова");
        assert_eq!(presentation.primary_action, PrimaryAction::Retry);
        assert_eq!(presentation.semantic_tone, SemanticTone::Danger);
    }

    #[test]
    fn degraded_profile_mutations_are_rejected_without_issuing_tokens() {
        let mut model = model_with_profiles();
        model.state = UiState::DaemonDegraded {
            message: "unsafe https://private.invalid token=secret".into(),
        };

        assert!(model.begin_profile_activation("profile-b").is_none());
        assert!(model.begin_profile_delete("profile-b").is_none());
        assert!(model.begin_profile_target_refresh("profile-b").is_none());
        let presentation = model.profile_presentation();
        assert_eq!(presentation.pending_operation, None);
        assert_eq!(
            presentation.error, None,
            "mutation errors are not list errors"
        );
        let error = presentation.mutation_error.expect("bounded degraded error");
        assert!(error.chars().count() < 128);
        assert!(!error.contains("private.invalid"));
        assert!(!error.contains("secret"));
        assert!(!error.contains("profile-b"));
    }

    fn assert_full_profile_reconciliation_required(model: &mut DesktopViewModel) {
        assert_eq!(model.profile_presentation().active_id, None);
        assert!(model.subscription.is_none());
        assert!(model.catalog_presentation().groups.is_empty());
        assert!(model.take_profile_status_reload_needed());
        assert!(model.take_profile_list_reload_needed());
        assert!(model.take_profile_catalog_reload_needed());
        assert!(model.begin_catalog_request().is_none());
        assert!(model.profile_reconciliation_required());
        assert!(model.begin_primary_action().is_none());
        assert!(model.begin_profile_activation("profile-b").is_none());
    }

    #[test]
    fn ambiguous_profile_mutations_clear_runtime_and_require_full_reconciliation() {
        let mut activation = model_with_profiles();
        let catalog = activation
            .begin_catalog_request()
            .expect("activation catalog");
        activation.finish_catalog(catalog.token, Ok(catalog_fixture()));
        activation.subscription = Some(DaemonSubscriptionInfo {
            display_name: "Unsafe old shelf".into(),
            ..DaemonSubscriptionInfo::default()
        });
        let token = activation
            .begin_profile_activation("profile-b")
            .expect("activation");
        assert!(!activation.finish_profile_activation(
            token,
            Err(DaemonError::ambiguous(
                "unsafe activation transport details"
            )),
        ));
        assert_full_profile_reconciliation_required(&mut activation);

        let status = activation.begin_refresh();
        activation.finish_refresh(
            status,
            Ok(DaemonStatus {
                state: "ready".into(),
                active_profile_id: Some("profile-b".into()),
                profile: Some("Beta".into()),
                ..DaemonStatus::default()
            }),
        );
        let list = activation
            .begin_profile_list()
            .expect("reconciliation list");
        activation.finish_profile_list(
            list,
            Ok(profile_list(
                Some("profile-b"),
                vec![profile("profile-b", true, "Beta")],
            )),
        );
        let catalog = activation
            .begin_catalog_request()
            .expect("reconciliation catalog");
        activation.finish_catalog(catalog.token, Ok(catalog_fixture()));
        assert!(!activation.profile_reconciliation_required());
        assert!(activation.presentation().primary_enabled);

        let mut refresh = model_with_profiles();
        let token = refresh
            .begin_profile_target_refresh("profile-a")
            .expect("target refresh");
        assert!(!refresh.finish_profile_target_refresh(
            token,
            Err(DaemonError::ambiguous("unsafe refresh transport details")),
        ));
        assert_full_profile_reconciliation_required(&mut refresh);

        let mut deletion = model_with_profiles();
        let token = deletion.begin_profile_delete("profile-b").expect("delete");
        assert!(!deletion.finish_profile_delete(
            token,
            Err(DaemonError::ambiguous("unsafe delete transport details")),
        ));
        assert_full_profile_reconciliation_required(&mut deletion);
    }

    #[test]
    fn ambiguous_profile_reconciliation_sequences_catalog_after_status_and_list_in_every_order() {
        const ORDERS: [[&str; 3]; 6] = [
            ["status", "list", "catalog"],
            ["status", "catalog", "list"],
            ["list", "status", "catalog"],
            ["list", "catalog", "status"],
            ["catalog", "status", "list"],
            ["catalog", "list", "status"],
        ];

        for order in ORDERS {
            let mut model = model_with_profiles();
            let mutation = model
                .begin_profile_activation("profile-b")
                .expect("activation");
            assert!(!model.finish_profile_activation(
                mutation,
                Err(DaemonError::ambiguous("unsafe transport details")),
            ));
            assert!(model.take_profile_catalog_reload_needed());

            let status = model.begin_refresh();
            let list = model.begin_profile_list().expect("profile list");
            let mut catalog_finished = false;
            for phase in order {
                match phase {
                    "status" => model.finish_refresh(
                        status,
                        Ok(DaemonStatus {
                            state: "ready".into(),
                            active_profile_id: Some("profile-b".into()),
                            profile: Some("Beta".into()),
                            ..DaemonStatus::default()
                        }),
                    ),
                    "list" => {
                        assert!(model.finish_profile_list(
                            list,
                            Ok(profile_list(
                                Some("profile-b"),
                                vec![profile("profile-b", true, "Beta")],
                            )),
                        ));
                    }
                    "catalog" => {
                        if let Some(request) = model.begin_catalog_request() {
                            model.finish_catalog(request.token, Ok(catalog_fixture()));
                            catalog_finished = true;
                        }
                    }
                    _ => unreachable!("fixed reconciliation phase"),
                }
            }

            if !catalog_finished {
                assert!(model.take_profile_catalog_reload_needed());
                let request = model
                    .begin_catalog_request()
                    .expect("catalog is armed after status and list");
                model.finish_catalog(request.token, Ok(catalog_fixture()));
            }
            assert!(!model.catalog_presentation().groups.is_empty(), "{order:?}");
            assert!(!model.profile_reconciliation_required(), "{order:?}");
            assert!(model.presentation().primary_enabled, "{order:?}");
        }
    }

    #[test]
    fn latency_check_queues_while_disconnected_and_allows_only_one_request_when_connected() {
        let mut view_model = DesktopViewModel::new(Arc::new(MockDaemonClient::ready()));
        let refresh = view_model.begin_refresh();
        view_model.finish_refresh(refresh, Ok(MockDaemonClient::ready().status));

        assert!(view_model.begin_latency_request().is_none());
        assert!(view_model.latency_presentation().queued);

        let connect = view_model.begin_primary_action().expect("connect").1;
        view_model.finish_connect(
            connect,
            Ok(DaemonStatus {
                state: "connected".into(),
                profile: Some("Тестовый".into()),
                current_node: Some("Авто".into()),
                ..DaemonStatus::default()
            }),
        );
        let catalog = view_model.begin_catalog_request().expect("catalog request");
        view_model
            .finish_catalog(catalog.token, Ok(catalog_fixture()))
            .expect("catalog completion");
        let request = view_model
            .begin_latency_request()
            .expect("queued latency request after connect");
        assert_eq!(request.group_ids, vec!["opaque:selected"]);
        assert!(view_model.begin_latency_request().is_none());
        assert!(view_model.latency_presentation().loading);
        assert!(view_model.latency_presentation().queued);

        let outcome = view_model
            .finish_latencies(request.token, Ok(DaemonLatencies::default()))
            .expect("first completion");
        assert!(outcome.rerun);
        assert!(view_model.latency_presentation().queued);

        let rerun = view_model
            .begin_latency_request()
            .expect("one coalesced rerun");
        let outcome = view_model
            .finish_latencies(rerun.token, Ok(DaemonLatencies::default()))
            .expect("rerun completion");
        assert!(!outcome.rerun);
        assert!(!view_model.latency_presentation().queued);
    }

    #[test]
    fn first_successful_catalog_load_queues_selected_group_latency() {
        let mut view_model = DesktopViewModel::new(Arc::new(MockDaemonClient::ready()));
        let refresh = view_model.begin_refresh();
        view_model.finish_refresh(
            refresh,
            Ok(DaemonStatus {
                state: "connected".into(),
                profile: Some("Тестовый".into()),
                current_node: Some("Авто".into()),
                ..DaemonStatus::default()
            }),
        );
        assert!(!view_model.latency_presentation().queued);

        let catalog = view_model
            .begin_catalog_request()
            .expect("first catalog request");
        view_model
            .finish_catalog(catalog.token, Ok(catalog_fixture()))
            .expect("first catalog completion");

        assert!(view_model.latency_presentation().queued);
        let request = view_model
            .begin_latency_request()
            .expect("automatic selected-group latency");
        assert_eq!(request.catalog_revision, Some(91));
        assert_eq!(request.group_ids, vec!["opaque:selected"]);
    }

    #[test]
    fn any_answered_ping_is_success_only_timeout_and_errors_are_danger() {
        for value in [1, 79, 80, 149, 150, 999, 5000] {
            assert_eq!(
                latency_display("ok", Some(value)),
                (format!("{value} ms"), "success".into())
            );
        }
        assert_eq!(latency_display("timeout", None).1, "danger");
        assert_eq!(latency_display("unavailable", None).1, "danger");
        assert_eq!(latency_display("unknown", None).1, "neutral");
    }

    #[test]
    fn targeted_latency_updates_preserve_other_groups_and_missing_entries() {
        let mut view_model = connected_view_model_with_catalog();
        let first = view_model
            .begin_latency_request()
            .expect("selected group request");
        view_model
            .finish_latencies(
                first.token,
                Ok(DaemonLatencies {
                    entries: vec![DaemonLatencyEntry {
                        group_id: "opaque:selected".into(),
                        node_id: "opaque:node-b".into(),
                        latency_ms: Some(79),
                        status: "ok".into(),
                    }],
                }),
            )
            .expect("selected group completion");
        let selected = view_model.catalog_presentation();
        assert_eq!(selected.nodes[0].latency_text, "79 ms");
        assert_eq!(selected.nodes[0].latency_tone, "success");
        assert_eq!(selected.nodes[0].delay_ms, Some(79));
        assert_eq!(selected.nodes[1].latency_text, "51 ms");
        assert_eq!(selected.nodes[1].delay_ms, Some(51));

        assert!(view_model.select_catalog_group("opaque:first"));
        let second = view_model
            .begin_latency_request()
            .expect("new group request");
        assert_eq!(second.group_ids, vec!["opaque:first"]);
        view_model
            .finish_latencies(second.token, Ok(DaemonLatencies::default()))
            .expect("empty targeted completion");
        assert_eq!(view_model.catalog_presentation().nodes[0].latency_text, "");

        assert!(view_model.select_catalog_group("opaque:selected"));
        let selected = view_model.catalog_presentation();
        assert_eq!(selected.nodes[0].latency_text, "79 ms");
        assert_eq!(selected.nodes[0].delay_ms, Some(79));
        assert_eq!(selected.nodes[1].latency_text, "51 ms");
    }

    #[test]
    fn same_revision_reload_preserves_omitted_latency_and_changed_revision_uses_fresh_data() {
        let mut view_model = connected_view_model_with_catalog();
        let request = view_model.begin_latency_request().expect("latency request");
        view_model
            .finish_latencies(
                request.token,
                Ok(DaemonLatencies {
                    entries: vec![DaemonLatencyEntry {
                        group_id: "opaque:selected".into(),
                        node_id: "opaque:node-b".into(),
                        latency_ms: Some(42),
                        status: "ok".into(),
                    }],
                }),
            )
            .expect("latency completion");

        let same = view_model.begin_catalog_request().expect("same revision");
        let mut same_fixture = catalog_fixture();
        same_fixture.groups[1].nodes[0].delay_ms = None;
        view_model
            .finish_catalog(same.token, Ok(same_fixture))
            .expect("same catalog");
        assert_eq!(
            view_model.catalog_presentation().nodes[0].latency_text,
            "42 ms"
        );
        assert_eq!(
            view_model.catalog_presentation().nodes[0].delay_ms,
            Some(42)
        );

        let changed = view_model
            .begin_catalog_request()
            .expect("changed revision");
        let mut fixture = catalog_fixture();
        fixture.revision += 1;
        view_model
            .finish_catalog(changed.token, Ok(fixture))
            .expect("changed catalog");
        assert_eq!(
            view_model.catalog_presentation().nodes[0].latency_text,
            "37 ms"
        );
        assert_eq!(
            view_model.catalog_presentation().nodes[0].delay_ms,
            Some(37)
        );
        assert_eq!(
            view_model.catalog_presentation().nodes[0].latency_tone,
            "success"
        );

        let cleared = view_model
            .begin_catalog_request()
            .expect("cleared revision");
        let mut cleared_fixture = catalog_fixture();
        cleared_fixture.revision += 2;
        cleared_fixture.groups[1].nodes[0].delay_ms = None;
        view_model
            .finish_catalog(cleared.token, Ok(cleared_fixture))
            .expect("cleared catalog");
        assert_eq!(view_model.catalog_presentation().nodes[0].latency_text, "");
        assert_eq!(view_model.catalog_presentation().nodes[0].delay_ms, None);
        assert_eq!(
            view_model.catalog_presentation().nodes[0].latency_tone,
            "neutral"
        );
    }

    #[test]
    fn successful_subscription_refresh_queues_automatic_latency_check() {
        let mut view_model = DesktopViewModel::new(Arc::new(MockDaemonClient::ready()));
        let refresh = view_model.begin_refresh();
        view_model.finish_refresh(
            refresh,
            Ok(DaemonStatus {
                state: "ready".into(),
                profile: Some("Тестовый".into()),
                current_node: Some("Авто".into()),
                subscription: Some(DaemonSubscriptionInfo {
                    refresh_available: true,
                    ..DaemonSubscriptionInfo::default()
                }),
                ..DaemonStatus::default()
            }),
        );
        let refresh = view_model
            .begin_subscription_refresh()
            .expect("subscription refresh");
        assert!(view_model.finish_subscription_refresh(
            refresh,
            Ok(DaemonStatus {
                state: "ready".into(),
                profile: Some("Тестовый".into()),
                current_node: Some("Авто".into()),
                ..DaemonStatus::default()
            })
        ));
        assert!(view_model.latency_presentation().queued);
    }

    #[test]
    fn connected_home_subscription_refresh_starts_and_applies_disconnected_status() {
        let mut view_model = DesktopViewModel::new(Arc::new(MockDaemonClient::ready()));
        view_model.state = UiState::Connected {
            profile: "Тестовый".into(),
            node: "Авто".into(),
        };
        view_model.subscription = Some(DaemonSubscriptionInfo {
            refresh_available: true,
            ..DaemonSubscriptionInfo::default()
        });

        let refresh = view_model
            .begin_subscription_refresh()
            .expect("connected refresh");
        assert!(view_model.finish_subscription_refresh(
            refresh,
            Ok(DaemonStatus {
                state: "ready".into(),
                profile: Some("Тестовый".into()),
                current_node: Some("Авто".into()),
                ..DaemonStatus::default()
            })
        ));
        assert!(matches!(view_model.state, UiState::Ready { .. }));
    }

    #[test]
    fn latency_results_are_compact_safe_and_stale_completions_are_ignored() {
        let mut view_model = connected_view_model_with_catalog();
        let first = view_model.begin_latency_request().expect("first request");
        view_model
            .finish_latencies(
                first.token,
                Ok(DaemonLatencies {
                    entries: vec![
                        DaemonLatencyEntry {
                            group_id: "opaque:selected".into(),
                            node_id: "opaque:node-b".into(),
                            latency_ms: Some(42),
                            status: "ok".into(),
                        },
                        DaemonLatencyEntry {
                            group_id: "opaque:selected".into(),
                            node_id: "opaque:node-c".into(),
                            latency_ms: None,
                            status: "timeout".into(),
                        },
                    ],
                }),
            )
            .expect("current completion");
        let shown = view_model.catalog_presentation();
        assert_eq!(shown.nodes[0].latency_text, "42 ms");
        assert_eq!(shown.nodes[1].latency_text, "таймаут");
        assert_eq!(shown.nodes[0].delay_ms, Some(42));
        assert_eq!(shown.nodes[1].delay_ms, None);

        let newer = view_model.begin_latency_request().expect("new request");
        assert!(
            view_model
                .finish_latencies(
                    first.token,
                    Err(DaemonError::new("private upstream detail"))
                )
                .is_none()
        );
        assert!(view_model.latency_presentation().loading);
        view_model
            .finish_latencies(
                newer.token,
                Err(DaemonError::new("private upstream detail")),
            )
            .expect("current failure");
        assert_eq!(
            view_model.latency_presentation().error.as_deref(),
            Some("Не удалось проверить задержку. Повторите попытку.")
        );
        assert!(!format!("{:?}", view_model.latency_presentation()).contains("private"));
    }

    #[test]
    fn latency_completion_for_an_older_catalog_revision_is_suppressed() {
        let mut view_model = connected_view_model_with_catalog();
        let latency = view_model.begin_latency_request().expect("latency request");
        let catalog = view_model.begin_catalog_request().expect("catalog refresh");
        let mut replacement = catalog_fixture();
        replacement.revision += 1;
        view_model
            .finish_catalog(catalog.token, Ok(replacement))
            .expect("new catalog");

        view_model
            .finish_latencies(
                latency.token,
                Ok(DaemonLatencies {
                    entries: vec![DaemonLatencyEntry {
                        group_id: "opaque:selected".into(),
                        node_id: "opaque:node-b".into(),
                        latency_ms: Some(999),
                        status: "ok".into(),
                    }],
                }),
            )
            .expect("stale completion is consumed without applying it");

        assert_eq!(
            view_model.catalog_presentation().nodes[0].latency_text,
            "37 ms"
        );
        assert!(!view_model.latency_presentation().loading);
    }

    #[test]
    fn stale_latency_revision_invalidates_catalog_and_queues_safe_retry() {
        let mut view_model = connected_view_model_with_catalog();
        let latency = view_model.begin_latency_request().expect("latency request");

        let outcome = view_model
            .finish_latencies(latency.token, Err(DaemonError::stale_revision()))
            .expect("current stale completion");

        assert!(outcome.refresh_catalog);
        assert!(view_model.catalog_presentation().revision.is_none());
        assert!(view_model.latency_presentation().queued);
        assert_eq!(
            view_model.latency_presentation().error.as_deref(),
            Some("Каталог маршрутов изменился. Обновляем задержку…")
        );
    }

    #[test]
    fn same_revision_catalog_reload_preserves_and_reapplies_queued_choice_for_connect() {
        let mut view_model = DesktopViewModel::new(Arc::new(MockDaemonClient::ready()));
        let refresh = view_model.begin_refresh();
        view_model.finish_refresh(refresh, Ok(MockDaemonClient::ready().status));
        let catalog = view_model.begin_catalog_request().expect("catalog request");
        view_model.finish_catalog(catalog.token, Ok(catalog_fixture()));
        assert!(view_model.select_catalog_group("opaque:selected"));
        assert!(view_model.begin_node_selection("opaque:node-c").is_none());

        let reload = view_model
            .begin_catalog_request()
            .expect("same revision reload");
        view_model
            .finish_catalog(reload.token, Ok(catalog_fixture()))
            .expect("catalog completion");

        let presentation = view_model.catalog_presentation();
        assert!(presentation.selection_queued);
        assert!(presentation.nodes[1].selected);
        assert!(!presentation.nodes[0].selected);
        let queued = view_model.queued_node_selections();
        assert_eq!(queued.len(), 1);
        assert_eq!(queued[0].revision, 91);
        assert_eq!(queued[0].node_id, "opaque:node-c");
        assert_eq!(
            view_model
                .begin_primary_action()
                .expect("connect queued choice")
                .0,
            PrimaryAction::Connect
        );
    }

    #[test]
    fn changed_catalog_revision_discards_queued_choice() {
        let mut view_model = DesktopViewModel::new(Arc::new(MockDaemonClient::ready()));
        let refresh = view_model.begin_refresh();
        view_model.finish_refresh(refresh, Ok(MockDaemonClient::ready().status));
        let catalog = view_model.begin_catalog_request().expect("catalog request");
        view_model.finish_catalog(catalog.token, Ok(catalog_fixture()));
        assert!(view_model.begin_node_selection("opaque:node-c").is_none());

        let reload = view_model
            .begin_catalog_request()
            .expect("changed revision reload");
        let mut changed = catalog_fixture();
        changed.revision += 1;
        view_model
            .finish_catalog(reload.token, Ok(changed))
            .expect("changed catalog completion");

        assert!(view_model.queued_node_selections().is_empty());
        assert!(!view_model.catalog_presentation().selection_queued);
        assert!(view_model.catalog_presentation().nodes[0].selected);
    }

    #[test]
    fn compact_latency_ui_source_contract() {
        let app = include_str!("../ui/app.slint");
        let main = include_str!("main.rs");
        assert!(!app.contains("callback check-latencies();"));
        assert!(!app.contains("latency-check-enabled"));
        assert!(!app.contains("Задержка обновляется автоматически"));
        assert!(app.contains("routes-heading := HorizontalLayout"));
        assert!(app.contains("route-updated-meta := Text"));
        assert!(app.contains(
            "text: root.latency-check-loading ? \"· Замеряем задержку…\" : root.subscription-updated == \"Не обновлялась\" ? \"· \" + root.subscription-updated : \"· Обновлено \" + root.subscription-updated;"
        ));
        assert!(app.contains(
            "color: !root.latency-check-loading && root.subscription-updated-stale ? Theme.warning : Theme.text-muted;"
        ));
        assert!(app.contains("horizontal-alignment: right;"));
        assert!(app.contains("trailing-text: node.latency-text;"));
        assert!(!main.contains("ui.on_check_latencies"));
        assert!(main.contains("TimerMode::Repeated"));
        assert!(main.contains("Duration::from_secs(60)"));
        assert!(main.contains("ui.window().is_visible()"));
        assert!(main.contains("if latency_queued"));
        assert!(main.contains("apply_latest_snapshot(&ui, &model).latency.queued"));
        assert!(app.contains("trailing-tone: node.latency-tone;"));
        assert!(!app.contains("Local signal grid: structural guides"));
        // The connection module keeps a state-tinted hairline.
        assert!(app.contains("? 88px"));
        assert!(app.contains(": root.status-tone == \"success\" ? #3ccb7f59"));
        assert!(include_str!("../ui/components.slint").contains("width: 76px;"));
        let route_row = include_str!("../ui/components.slint")
            .split("export component RouteNodeRow")
            .nth(1)
            .expect("route row source");
        assert!(route_row.contains("x: parent.width - self.width - (root.pending ? 44px : 20px);"));
        assert!(route_row.contains("parent.width - self.x - trailing-label.width - 36px"));
        assert!(!route_row.contains("trailing-label.width - (root.selected"));
        assert!(!route_row.contains("self.width - (root.selected"));
        // The selection reads from the tint and blue bar alone: no check mark.
        assert!(!route_row.contains("if root.selected && !root.pending: Image {"));
        assert!(!route_row.contains("check.svg"));
        assert!(route_row.contains("if root.pending: Text {"));
        assert!(route_row.contains("text: \"…\";"));
        assert!(!route_row.contains("\"✓\""));
        assert!(main.contains("run_latency_check"));
        assert!(main.contains("if outcome.refresh_catalog"));
    }

    #[test]
    fn one_page_home_shell_avoids_routes_blank_and_layout_regressions() {
        let app = include_str!("../ui/app.slint");
        let normalized_app = app.replace("\r\n", "\n");
        let components = include_str!("../ui/components.slint");
        let main = include_str!("main.rs");

        assert!(app.contains(
            "root.active-panel == \"routes\" ? \"home\" : root.active-panel == \"events\" ? \"status\" : root.local-page"
        ));
        assert!(!app.contains("root.active-panel == \"routes\" ? \"routes\""));
        assert!(!app.contains("if root.visible-page == \"routes\""));
        assert!(main.contains("if panel == Panel::Routes"));
        assert!(main.contains("ui.set_local_page(\"home\".into());"));
        assert!(main.contains("load_catalog(weak.clone(), open_model.clone());"));

        assert!(!app.contains("Text { text: \"Главная\";"));
        assert!(!app.contains("profile-name"));
        assert!(!main.contains("set_profile_name"));
        let group_track = app
            .split("route-group-track := Rectangle {")
            .nth(1)
            .and_then(|source| {
                source
                    .split("for group in root.catalog-groups: QuietChip")
                    .next()
            })
            .expect("route group track");
        assert!(group_track.contains("height: 38px;"));
        assert!(group_track.contains("border-radius: 10px;"));
        assert!(group_track.contains("ScrollView {"));
        assert!(
            !normalized_app
                .contains("height: 60px;\n                                horizontal-stretch: 1;")
        );
        // The home shelf moved into the window title: the provider logo leads the
        // subscription chip, and a failed refresh replaces the expiry in place.
        assert!(!app.contains("subscription-strip"));
        assert!(!app.contains("service-logo-badge"));
        let chip = app
            .split("subscription-chip := Rectangle {")
            .nth(1)
            .and_then(|source| {
                source
                    .split("subscription-refresh-action := FocusScope")
                    .next()
            })
            .expect("title subscription chip source");
        assert!(chip.contains("if root.has-service-logo: Image {"));
        assert!(chip.contains("source: root.service-logo;"));
        assert!(chip.contains("root.subscription-refresh-error != \"\" ? \"Не удалось обновить\""));
        assert!(!chip.contains("if root.subscription-refresh-error != \"\": Text"));
        assert!(app.contains("toggle-module := Rectangle"));
        for shield in ["shield-check.svg", "shield-alert.svg", "shield-off.svg"] {
            assert!(app.contains(&format!("@image-url(\"../assets/{shield}\")")));
        }

        assert!(!components.contains("#0091ff"));
        // Route flags sit bare in an inset grouped list; no tinted tile remains.
        assert!(!components.contains("#4c9dff42"));
        assert!(components.contains("in property <bool> show-separator: false;"));
    }

    #[test]
    fn home_subscription_refresh_button_has_balanced_vertical_padding() {
        let app = include_str!("../ui/app.slint");
        let title_subscription = app
            .split("title-subscription := Rectangle {")
            .nth(1)
            .expect("title subscription source");
        let refresh_action = title_subscription
            .split("subscription-refresh-action := FocusScope")
            .nth(1)
            .and_then(|source| source.split("announcement-bell := FocusScope").next())
            .expect("title subscription refresh action source");

        // A square icon button centred in the 28px chip row of the 44px title bar.
        assert!(title_subscription.contains("y: 8px;"));
        assert!(title_subscription.contains("height: 28px;"));
        assert!(refresh_action.contains("width: 28px;"));
        assert!(refresh_action.contains("height: 28px;"));
        assert!(refresh_action.contains("x: 7px;"));
        assert!(refresh_action.contains("y: 7px;"));
        assert!(refresh_action.contains("width: 14px;"));
        assert!(refresh_action.contains("height: 14px;"));
        assert!(
            refresh_action
                .contains("property <bool> source-mode: !root.subscription-refresh-available;")
        );
        assert!(refresh_action.contains("\"Указать ссылку подписки\""));
        assert!(refresh_action.contains("root.refresh-subscription();"));
        assert!(refresh_action.contains("subscription-refresh-action.run();"));
    }

    #[test]
    fn state_eyebrows_are_sentence_case_and_ready_has_one_route_label() {
        let states = [
            UiState::Empty,
            UiState::Importing,
            UiState::Ready {
                profile: "Работа".into(),
                node: "Авто".into(),
            },
            UiState::Connecting {
                profile: "Работа".into(),
                node: "Авто".into(),
                step: "Запуск".into(),
            },
            UiState::Connected {
                profile: "Работа".into(),
                node: "nl-01".into(),
            },
            UiState::Disconnecting {
                profile: "Работа".into(),
                node: "nl-01".into(),
            },
            UiState::Error {
                message: "Ошибка".into(),
            },
            UiState::DaemonDegraded {
                message: "Недоступно".into(),
            },
        ];
        for state in states {
            let eyebrow = state.presentation().eyebrow;
            assert_ne!(
                eyebrow,
                eyebrow.to_uppercase(),
                "all-caps eyebrow: {eyebrow}"
            );
        }
        let ready = UiState::Ready {
            profile: "Работа".into(),
            node: "Авто".into(),
        }
        .presentation();
        assert_eq!(ready.eyebrow, "Готово");
        assert_eq!(ready.node, "Авто");
        assert_eq!(ready.supporting, "");

        for state in [UiState::Empty, UiState::Importing] {
            assert_eq!(state.presentation().node, "Маршрут не выбран");
        }
    }

    #[test]
    fn subscription_presentation_uses_real_usage_expiry_and_safe_source_name() {
        let mut view_model = DesktopViewModel::new(Arc::new(MockDaemonClient::ready()));
        let refresh = view_model.begin_refresh();
        view_model.finish_refresh(
            refresh,
            Ok(DaemonStatus {
                state: "ready".into(),
                profile: Some("Основной".into()),
                current_node: Some("Auto Sweden".into()),
                subscription: Some(DaemonSubscriptionInfo {
                    source_name: "subscription.example".into(),
                    display_name: "Provider One".into(),
                    uploaded_bytes: Some(u64::MAX),
                    downloaded_bytes: Some(938_375_741_110),
                    used_bytes: Some(u64::MAX),
                    total_bytes: None,
                    expires_at_unix: Some(1_792_851_157),
                    updated_at_unix: 1_789_405_200,
                    refresh_available: true,
                    ..DaemonSubscriptionInfo::default()
                }),
                ..DaemonStatus::default()
            }),
        );

        let subscription = view_model.subscription_presentation_at(1_789_405_200);
        assert_eq!(subscription.display_name, "Provider One");
        assert_eq!(subscription.usage, "17179869184.0 GB");
        assert_eq!(subscription.expiry, "Осталось 40 дней");
        assert_eq!(subscription.updated, "14.09.2026");
        assert!(!subscription.updated_stale);
        assert!(subscription.refresh_available);
        assert!(!format!("{subscription:?}").contains("https://"));

        let stale = view_model.subscription_presentation_at(1_789_405_200 + 6 * 60 * 60);
        assert_eq!(stale.updated, "14.09.2026");
        assert!(stale.updated_stale);
    }

    #[test]
    fn subscription_update_date_is_compact_and_explicit() {
        assert_eq!(format_update_date(0), "Не обновлялась");
        assert_eq!(format_update_date(1), "01.01.1970");
        assert_eq!(format_update_date(1_789_405_200), "14.09.2026");
    }

    #[test]
    fn automatic_subscription_refresh_uses_provider_interval_or_safe_default() {
        let mut client = MockDaemonClient::ready();
        client.status.subscription = Some(DaemonSubscriptionInfo {
            updated_at_unix: 10_000,
            refresh_interval_secs: Some(3_600),
            refresh_available: true,
            ..DaemonSubscriptionInfo::default()
        });
        let mut model = DesktopViewModel::new(Arc::new(client));
        let refresh = model.begin_refresh();
        let status = model.daemon_client().status().expect("status");
        model.finish_refresh(refresh, Ok(status));

        assert!(!model.automatic_subscription_refresh_due_at(13_599));
        assert!(model.automatic_subscription_refresh_due_at(13_600));

        model.subscription.as_mut().unwrap().refresh_interval_secs = None;
        assert!(!model.automatic_subscription_refresh_due_at(10_000 + 6 * 60 * 60 - 1));
        assert!(model.automatic_subscription_refresh_due_at(10_000 + 6 * 60 * 60));

        model.state = UiState::Connected {
            profile: "Основной".into(),
            node: "Auto".into(),
        };
        assert!(model.automatic_subscription_refresh_due_at(u64::MAX));
        assert!(model.begin_subscription_refresh().is_some());
    }

    #[test]
    fn subscription_presentation_does_not_invent_unlimited_usage_or_expiry() {
        let mut client = MockDaemonClient::ready();
        client.status.subscription = Some(DaemonSubscriptionInfo::default());
        let mut view_model = DesktopViewModel::new(Arc::new(client));
        let refresh = view_model.begin_refresh();
        let status = view_model.daemon_client().status().expect("status");
        view_model.finish_refresh(refresh, Ok(status));

        let subscription = view_model.subscription_presentation_at(0);
        assert_eq!(subscription.usage, "Нет данных");
        assert_eq!(subscription.expiry, "Срок неизвестен");
    }

    #[test]
    fn subscription_presentation_preserves_legitimate_total_only_metadata() {
        let mut client = MockDaemonClient::ready();
        client.status.subscription = Some(DaemonSubscriptionInfo {
            total_bytes: Some(10 * 1024 * 1024 * 1024),
            ..DaemonSubscriptionInfo::default()
        });
        let mut view_model = DesktopViewModel::new(Arc::new(client));
        let refresh = view_model.begin_refresh();
        let status = view_model.daemon_client().status().expect("status");
        view_model.finish_refresh(refresh, Ok(status));

        let subscription = view_model.subscription_presentation_at(0);
        assert_eq!(subscription.usage, "— / 10.0 GB");
        assert_eq!(subscription.expiry, "Срок неизвестен");
    }

    #[cfg(windows)]
    #[test]
    fn local_logo_path_accepts_only_fully_qualified_local_drive_paths_on_windows() {
        assert_eq!(
            local_logo_path(r"C:\cache\provider\logo.png").as_deref(),
            Some(r"C:\cache\provider\logo.png")
        );
        for rejected in [
            r"\\server\share\logo.png",
            r"\\?\UNC\server\share\logo.png",
            r"\\.\device\logo.png",
            r"\rooted\logo.png",
            r"C:relative\logo.png",
            "https://provider.invalid/logo.png",
            "HTTP://provider.invalid/logo.png",
            "data:image/png;base64,AA==",
        ] {
            assert_eq!(local_logo_path(rejected), None, "accepted {rejected}");
        }
    }

    #[test]
    fn subscription_presentation_bounds_metadata_and_rejects_remote_logo_urls() {
        let mut client = MockDaemonClient::ready();
        client.status.subscription = Some(DaemonSubscriptionInfo {
            source_name: "fallback.example".into(),
            display_name: format!("  {}  ", "P".repeat(200)),
            announcement_text: Some(format!("  Notice\n{}  ", "x".repeat(600))),
            announcement_tone: Some("provider-purple".into()),
            service_logo_path: Some("https://provider.invalid/logo.png".into()),
            ..DaemonSubscriptionInfo::default()
        });
        let mut view_model = DesktopViewModel::new(Arc::new(client));
        let refresh = view_model.begin_refresh();
        let status = view_model.daemon_client().status().expect("status");
        view_model.finish_refresh(refresh, Ok(status));

        let subscription = view_model.subscription_presentation_at(0);
        assert_eq!(subscription.display_name.chars().count(), 128);
        assert_eq!(subscription.announcement_text.chars().count(), 512);
        assert_eq!(subscription.announcement_tone, "info");
        assert_eq!(subscription.service_logo_path, None);
    }

    #[test]
    fn subscription_presentation_falls_back_to_host_and_keeps_local_logo_path() {
        let logo = tempfile::tempdir()
            .expect("logo temp directory")
            .path()
            .join("provider.png");
        let mut client = MockDaemonClient::ready();
        client.status.subscription = Some(DaemonSubscriptionInfo {
            source_name: "fallback.example".into(),
            display_name: " \n ".into(),
            uploaded_bytes: Some(u64::MAX),
            downloaded_bytes: Some(42),
            total_bytes: Some(u64::MAX),
            announcement_tone: Some("red".into()),
            service_logo_path: Some(logo.to_string_lossy().into_owned()),
            ..DaemonSubscriptionInfo::default()
        });
        let mut view_model = DesktopViewModel::new(Arc::new(client));
        let refresh = view_model.begin_refresh();
        let status = view_model.daemon_client().status().expect("status");
        view_model.finish_refresh(refresh, Ok(status));

        let subscription = view_model.subscription_presentation_at(0);
        assert_eq!(subscription.display_name, "fallback.example");
        assert_eq!(subscription.usage, "17179869184.0 GB / 17179869184.0 GB");
        assert_eq!(subscription.announcement_tone, "danger");
        assert_eq!(subscription.service_logo_path.as_deref(), logo.to_str());
    }

    #[test]
    fn failed_subscription_refresh_stays_ready_and_reports_inline() {
        let mut client = MockDaemonClient::ready();
        client.status.subscription = Some(DaemonSubscriptionInfo {
            source_name: "subscription.example".into(),
            refresh_available: true,
            ..DaemonSubscriptionInfo::default()
        });
        let mut view_model = DesktopViewModel::new(Arc::new(client));
        let startup = view_model.begin_refresh();
        view_model.finish_refresh(
            startup,
            Ok(DaemonStatus {
                state: "ready".into(),
                profile: Some("Основной".into()),
                subscription: Some(DaemonSubscriptionInfo {
                    source_name: "subscription.example".into(),
                    refresh_available: true,
                    ..DaemonSubscriptionInfo::default()
                }),
                ..DaemonStatus::default()
            }),
        );
        let refresh = view_model
            .begin_subscription_refresh()
            .expect("refresh from ready state");
        assert!(view_model.subscription_presentation().refresh_pending);
        assert!(!view_model.finish_subscription_refresh(
            refresh,
            Err(DaemonError::new("unsafe url=https://private.invalid")),
        ));
        assert!(matches!(view_model.state, UiState::Ready { .. }));
        let subscription = view_model.subscription_presentation();
        assert_eq!(
            subscription.error.as_deref(),
            Some("Не удалось обновить. Рабочий профиль сохранён.")
        );
        assert!(!format!("{subscription:?}").contains("private.invalid"));
    }

    #[test]
    fn ambiguous_subscription_refresh_forces_full_authoritative_reconciliation() {
        let mut client = MockDaemonClient::ready();
        client.status.subscription = Some(DaemonSubscriptionInfo {
            source_name: "subscription.example".into(),
            refresh_available: true,
            ..DaemonSubscriptionInfo::default()
        });
        let mut view_model = DesktopViewModel::new(Arc::new(client));
        let startup = view_model.begin_refresh();
        let status = view_model.daemon_client().status().expect("status");
        view_model.finish_refresh(startup, Ok(status));

        let refresh = view_model
            .begin_subscription_refresh()
            .expect("refresh from ready state");
        assert!(!view_model.finish_subscription_refresh(
            refresh,
            Err(DaemonError::ambiguous("private committed marker")),
        ));

        assert!(matches!(view_model.state, UiState::DaemonDegraded { .. }));
        assert!(view_model.profile_status_reconciliation_pending);
        assert!(view_model.profile_list_reconciliation_pending);
        assert!(view_model.profile_catalog_reconciliation_pending);
        assert!(view_model.profile_status_reload_needed);
        assert!(view_model.profile_reload_needed);
        assert!(view_model.profile_catalog_reload_needed);
        assert!(!format!("{:?}", view_model.subscription_presentation()).contains("private"));
    }

    struct DelayedDaemonClient {
        import_started: Mutex<Option<mpsc::Sender<()>>>,
        import_release: Mutex<mpsc::Receiver<()>>,
        events_started: Mutex<Option<mpsc::Sender<()>>>,
        events_release: Mutex<mpsc::Receiver<()>>,
    }

    type DelayedClientHarness = (
        Arc<DelayedDaemonClient>,
        mpsc::Receiver<()>,
        mpsc::Sender<()>,
        mpsc::Receiver<()>,
        mpsc::Sender<()>,
    );

    impl DelayedDaemonClient {
        fn harness() -> DelayedClientHarness {
            let (import_started_tx, import_started_rx) = mpsc::channel();
            let (import_release_tx, import_release_rx) = mpsc::channel();
            let (events_started_tx, events_started_rx) = mpsc::channel();
            let (events_release_tx, events_release_rx) = mpsc::channel();
            (
                Arc::new(Self {
                    import_started: Mutex::new(Some(import_started_tx)),
                    import_release: Mutex::new(import_release_rx),
                    events_started: Mutex::new(Some(events_started_tx)),
                    events_release: Mutex::new(events_release_rx),
                }),
                import_started_rx,
                import_release_tx,
                events_started_rx,
                events_release_tx,
            )
        }

        fn ready_status() -> DaemonStatus {
            DaemonStatus {
                state: "ready".into(),
                profile: Some("Тестовый".into()),
                current_node: Some("Авто".into()),
                ..DaemonStatus::default()
            }
        }
    }

    impl DaemonClient for DelayedDaemonClient {
        fn status(&self) -> Result<DaemonStatus, DaemonError> {
            Ok(Self::ready_status())
        }

        fn import_subscription(&self, _url: &str) -> Result<DaemonStatus, DaemonError> {
            if let Some(started) = self
                .import_started
                .lock()
                .expect("import started lock")
                .take()
            {
                started.send(()).expect("announce import start");
            }
            self.import_release
                .lock()
                .expect("import release lock")
                .recv()
                .expect("release delayed import");
            Ok(Self::ready_status())
        }

        fn connect(&self) -> Result<DaemonStatus, DaemonError> {
            Ok(Self::ready_status())
        }

        fn disconnect(&self) -> Result<DaemonStatus, DaemonError> {
            Ok(Self::ready_status())
        }

        fn events(
            &self,
            _after: u64,
            _known_epoch: Option<&str>,
        ) -> Result<DaemonEventBatch, DaemonError> {
            if let Some(started) = self
                .events_started
                .lock()
                .expect("events started lock")
                .take()
            {
                started.send(()).expect("announce events start");
            }
            self.events_release
                .lock()
                .expect("events release lock")
                .recv()
                .expect("release delayed events");
            Ok(DaemonEventBatch {
                epoch: "delayed".into(),
                events: Vec::new(),
            })
        }

        fn catalog(&self) -> Result<DaemonCatalog, DaemonError> {
            Ok(DaemonCatalog::default())
        }

        fn select_node(
            &self,
            _group_id: &str,
            _revision: u64,
            _node_id: &str,
        ) -> Result<DaemonCatalog, DaemonError> {
            Ok(DaemonCatalog::default())
        }
    }

    struct DelayedCatalogClient {
        started: Mutex<Option<mpsc::Sender<()>>>,
        release: Mutex<mpsc::Receiver<()>>,
    }

    struct DelayedSelectionClient {
        started: Mutex<Option<mpsc::Sender<()>>>,
        release: Mutex<mpsc::Receiver<()>>,
    }

    impl DaemonClient for DelayedCatalogClient {
        fn status(&self) -> Result<DaemonStatus, DaemonError> {
            Ok(DaemonStatus::default())
        }

        fn import_subscription(&self, _url: &str) -> Result<DaemonStatus, DaemonError> {
            Ok(DaemonStatus::default())
        }

        fn connect(&self) -> Result<DaemonStatus, DaemonError> {
            Ok(DaemonStatus::default())
        }

        fn disconnect(&self) -> Result<DaemonStatus, DaemonError> {
            Ok(DaemonStatus::default())
        }

        fn events(
            &self,
            _after: u64,
            _known_epoch: Option<&str>,
        ) -> Result<DaemonEventBatch, DaemonError> {
            Ok(DaemonEventBatch::default())
        }

        fn catalog(&self) -> Result<DaemonCatalog, DaemonError> {
            if let Some(started) = self.started.lock().expect("catalog started lock").take() {
                started.send(()).expect("announce catalog start");
            }
            self.release
                .lock()
                .expect("catalog release lock")
                .recv()
                .expect("release delayed catalog");
            Ok(DaemonCatalog::default())
        }

        fn select_node(
            &self,
            _group_id: &str,
            _revision: u64,
            _node_id: &str,
        ) -> Result<DaemonCatalog, DaemonError> {
            Ok(DaemonCatalog::default())
        }
    }

    impl DaemonClient for DelayedSelectionClient {
        fn status(&self) -> Result<DaemonStatus, DaemonError> {
            Ok(DaemonStatus::default())
        }

        fn import_subscription(&self, _url: &str) -> Result<DaemonStatus, DaemonError> {
            Ok(DaemonStatus::default())
        }

        fn connect(&self) -> Result<DaemonStatus, DaemonError> {
            Ok(DaemonStatus::default())
        }

        fn disconnect(&self) -> Result<DaemonStatus, DaemonError> {
            Ok(DaemonStatus::default())
        }

        fn events(
            &self,
            _after: u64,
            _known_epoch: Option<&str>,
        ) -> Result<DaemonEventBatch, DaemonError> {
            Ok(DaemonEventBatch::default())
        }

        fn catalog(&self) -> Result<DaemonCatalog, DaemonError> {
            Ok(catalog_fixture())
        }

        fn select_node(
            &self,
            _group_id: &str,
            _revision: u64,
            _node_id: &str,
        ) -> Result<DaemonCatalog, DaemonError> {
            if let Some(started) = self.started.lock().expect("selection started lock").take() {
                started.send(()).expect("announce selection start");
            }
            self.release
                .lock()
                .expect("selection release lock")
                .recv()
                .expect("release delayed selection");
            let mut catalog = catalog_fixture();
            catalog.groups[1].nodes[0].selected = false;
            catalog.groups[1].nodes[1].selected = true;
            Ok(catalog)
        }
    }

    fn refresh(view_model: &mut DesktopViewModel) {
        let token = view_model.begin_refresh();
        let result = view_model.daemon_client().status();
        view_model.finish_refresh(token, result);
    }

    fn import_subscription(view_model: &mut DesktopViewModel, url: &str) {
        let token = view_model
            .active_mutation
            .filter(|active| active.kind == MutationKind::Import)
            .map(|active| active.token)
            .or_else(|| view_model.begin_import(url))
            .expect("start import mutation");
        let result = view_model.daemon_client().import_subscription(url);
        view_model.finish_import(token, result);
    }

    fn connect(view_model: &mut DesktopViewModel) {
        let token = view_model.next_mutation(MutationKind::Connect);
        let result = view_model.daemon_client().connect();
        view_model.finish_connect(token, result);
    }

    fn disconnect(view_model: &mut DesktopViewModel) {
        let token = view_model.next_mutation(MutationKind::Disconnect);
        let result = view_model.daemon_client().disconnect();
        view_model.finish_disconnect(token, result);
    }

    fn events(view_model: &mut DesktopViewModel, level: Option<&str>) -> Vec<PresentedEvent> {
        let filtered = view_model.set_event_filter(level);
        let Some(request) = view_model.begin_events_request() else {
            return filtered;
        };
        let result = request
            .client
            .events(request.after, request.known_epoch.as_deref());
        view_model
            .finish_events(request.token, result)
            .expect("current event request")
    }

    #[test]
    fn delayed_import_does_not_hold_the_model_lock_and_escape_stays_immediate() {
        let (client, started, release, _events_started, _events_release) =
            DelayedDaemonClient::harness();
        let model = Arc::new(Mutex::new(DesktopViewModel::new(client)));
        let (daemon, mutation) = {
            let mut model = model.lock().expect("view model lock");
            model.open_panel(Panel::Subscription);
            let mutation = model
                .begin_import("https://subscription.invalid/private")
                .expect("start import");
            (model.daemon_client(), mutation)
        };
        let worker_model = model.clone();
        let worker = thread::spawn(move || {
            let result = daemon.import_subscription("https://subscription.invalid/private");
            worker_model
                .lock()
                .expect("view model lock")
                .finish_import(mutation, result);
        });

        started.recv().expect("delayed import started");
        {
            let mut model = model
                .try_lock()
                .expect("model lock must stay available during import I/O");
            assert!(model.close_panel_on_escape());
        }
        release.send(()).expect("release import");
        worker.join().expect("join delayed import");
        assert_eq!(model.lock().expect("view model lock").panel, Panel::None);
    }

    #[test]
    fn delayed_events_do_not_block_panel_changes_or_filter_completion() {
        let (client, _import_started, _import_release, started, release) =
            DelayedDaemonClient::harness();
        let model = Arc::new(Mutex::new(DesktopViewModel::new(client)));
        let request = model
            .lock()
            .expect("view model lock")
            .begin_events_request()
            .expect("start events request");
        let worker_model = model.clone();
        let worker = thread::spawn(move || {
            let result = request
                .client
                .events(request.after, request.known_epoch.as_deref());
            worker_model
                .lock()
                .expect("view model lock")
                .finish_events(request.token, result)
        });

        started.recv().expect("delayed events started");
        {
            let mut model = model
                .try_lock()
                .expect("model lock must stay available during events I/O");
            model.open_panel(Panel::Routes);
            assert_eq!(model.panel, Panel::Routes);
        }
        release.send(()).expect("release events");
        assert!(
            worker
                .join()
                .expect("join delayed events")
                .expect("current event request")
                .is_empty()
        );
    }

    #[test]
    fn opening_a_panel_replaces_the_previous_panel_and_escape_closes_it() {
        let mut view_model = DesktopViewModel::new(Arc::new(MockDaemonClient::ready()));

        assert_eq!(view_model.panel, Panel::None);
        view_model.open_panel(Panel::Subscription);
        assert_eq!(view_model.panel, Panel::Subscription);
        view_model.open_panel(Panel::Routes);
        assert_eq!(view_model.panel, Panel::Routes);
        view_model.open_panel(Panel::Events);
        assert_eq!(view_model.panel, Panel::Events);

        assert!(view_model.close_panel_on_escape());
        assert_eq!(view_model.panel, Panel::None);
        assert!(!view_model.close_panel_on_escape());
    }

    #[test]
    fn a_pending_connection_action_suppresses_repeated_activation() {
        let mut view_model = DesktopViewModel::new(Arc::new(MockDaemonClient::ready()));
        refresh(&mut view_model);

        assert_eq!(
            view_model.begin_primary_action().map(|(action, _)| action),
            Some(PrimaryAction::Connect)
        );
        assert!(!view_model.presentation().primary_enabled);
        assert_eq!(view_model.begin_primary_action(), None);
    }

    #[test]
    fn external_import_waits_for_existing_import_or_connected_tunnel() {
        let mut disconnected = DesktopViewModel::new(Arc::new(MockDaemonClient::ready()));
        assert!(disconnected.can_begin_external_import());
        let refresh = disconnected.begin_refresh();
        assert!(!disconnected.can_begin_external_import());
        disconnected.finish_refresh(
            refresh,
            Ok(DaemonStatus {
                state: "empty".into(),
                ..DaemonStatus::default()
            }),
        );
        assert!(disconnected.can_begin_external_import());
        let import = disconnected
            .begin_import("https://subscription.invalid/first")
            .expect("first import starts");
        assert!(!disconnected.can_begin_external_import());
        disconnected.finish_import(import, Err(DaemonError::new("offline")));
        assert!(disconnected.can_begin_external_import());

        let connected = connected_view_model_with_catalog();
        assert!(!connected.can_begin_external_import());
    }

    #[test]
    fn external_import_waits_for_profile_list_and_catalog_after_success() {
        let mut model = DesktopViewModel::new(Arc::new(MockDaemonClient::ready()));
        let import = model
            .begin_import("https://subscription.invalid/first")
            .expect("start first import");
        model.finish_import(
            import,
            Ok(DaemonStatus {
                state: "ready".into(),
                profile: Some("First".into()),
                ..DaemonStatus::default()
            }),
        );
        assert!(!model.can_begin_external_import());

        let list = model.begin_profile_list().expect("reconcile list");
        assert!(!model.can_begin_external_import());
        assert!(model.finish_profile_list(list, Ok(profile_list(None, vec![]))));
        assert!(!model.can_begin_external_import());

        assert!(model.take_profile_catalog_reload_needed());
        let catalog = model.begin_catalog_request().expect("reconcile catalog");
        assert!(!model.can_begin_external_import());
        assert!(
            model
                .finish_catalog(catalog.token, Ok(DaemonCatalog::default()))
                .is_some()
        );
        assert!(model.can_begin_external_import());
    }

    #[test]
    fn ambiguous_import_failure_requires_reconciliation_before_next_deep_link() {
        let mut model = DesktopViewModel::new(Arc::new(MockDaemonClient::ready()));
        let import = model
            .begin_import("https://subscription.invalid/first")
            .expect("start first import");
        model.finish_import(import, Err(DaemonError::ambiguous("response lost")));
        assert!(!model.can_begin_external_import());
        assert!(model.take_profile_status_reload_needed());
    }

    #[test]
    fn transport_level_import_failure_shows_its_real_cause_not_a_generic_url_hint() {
        // Regression coverage for a real bug report: a friend's genuine
        // subscription-add failure showed "Не удалось добавить подписку.
        // Проверьте URL и повторите попытку." even though the actual cause
        // (confirmed by tracing map_error in daemon.rs) had nothing to do
        // with the URL - the desktop's own request to the *local* daemon
        // hadn't connected or hadn't heard back in time. That accurate,
        // already-safe message was being discarded in favor of a misleading
        // generic one just because it wasn't daemon-error-code-derived.
        let mut model = DesktopViewModel::new(Arc::new(MockDaemonClient::ready()));
        let import = model
            .begin_import("https://subscription.invalid/first")
            .expect("start first import");
        model.finish_import(
            import,
            Err(DaemonError::transport("Фоновый сервис не ответил вовремя.")),
        );
        assert_eq!(
            model.import_error.as_deref(),
            Some("Фоновый сервис не ответил вовремя.")
        );
    }

    #[test]
    fn failed_deep_link_still_reloads_preexisting_profiles_after_startup_status() {
        let mut model = DesktopViewModel::new(Arc::new(MockDaemonClient::ready()));
        let import = model
            .begin_import("https://subscription.invalid/failing")
            .expect("deep-link import starts");
        model.defer_profile_list_reload();
        assert!(!model.can_begin_external_import());
        model.finish_import(import, Err(DaemonError::new("offline")));
        assert!(model.take_profile_list_reload_needed());
    }

    #[test]
    fn failed_deep_link_still_reloads_catalog_after_disconnect() {
        let mut model = DesktopViewModel::new(Arc::new(MockDaemonClient::ready()));
        let import = model
            .begin_import("https://subscription.invalid/failing")
            .expect("deep-link import starts");
        model.defer_profile_catalog_reload();
        assert!(!model.can_begin_external_import());
        model.finish_import(import, Err(DaemonError::new("offline")));
        assert!(model.take_profile_catalog_reload_needed());
    }

    #[test]
    fn stale_startup_refresh_cannot_overwrite_a_newer_import() {
        let mut view_model = DesktopViewModel::new(Arc::new(MockDaemonClient::ready()));
        let refresh = view_model.begin_refresh();
        view_model.open_panel(Panel::Subscription);
        let import = view_model
            .begin_import("https://subscription.invalid/private")
            .expect("import supersedes startup refresh");

        view_model.finish_import(
            import,
            Ok(DaemonStatus {
                state: "ready".into(),
                profile: Some("Новый".into()),
                current_node: Some("Авто".into()),
                ..DaemonStatus::default()
            }),
        );
        view_model.finish_refresh(
            refresh,
            Ok(DaemonStatus {
                state: "empty".into(),
                ..DaemonStatus::default()
            }),
        );

        assert_eq!(
            view_model.state,
            UiState::Ready {
                profile: "Новый".into(),
                node: "Авто".into(),
            }
        );
    }

    #[test]
    fn import_and_connection_mutations_are_mutually_serialized() {
        let mut view_model = DesktopViewModel::new(Arc::new(MockDaemonClient::ready()));
        view_model.state = UiState::Ready {
            profile: "Работа".into(),
            node: "Авто".into(),
        };
        let import = view_model
            .begin_import("https://subscription.invalid/private")
            .expect("start import");
        assert!(!view_model.presentation().primary_enabled);
        assert_eq!(view_model.begin_primary_action(), None);
        view_model.finish_import(import, Ok(MockDaemonClient::ready().status));
        let list = view_model
            .begin_profile_list()
            .expect("post-import profile list");
        view_model.finish_profile_list(list, Ok(profile_list(None, Vec::new())));
        let catalog = view_model
            .begin_catalog_request()
            .expect("post-import catalog");
        view_model.finish_catalog(catalog.token, Ok(catalog_fixture()));

        let (action, mutation) = view_model
            .begin_primary_action()
            .expect("start connection mutation");
        assert_eq!(action, PrimaryAction::Connect);
        assert_eq!(
            view_model.begin_import("https://subscription.invalid/second"),
            None
        );
        assert_eq!(
            view_model.import_error.as_deref(),
            Some("Дождитесь завершения операции подключения.")
        );
        view_model.finish_connect(mutation, Ok(MockDaemonClient::ready().status));
    }

    #[test]
    fn event_epoch_change_resets_cache_cursor_and_cache_is_bounded() {
        let mut view_model = DesktopViewModel::new(Arc::new(MockDaemonClient::ready()));
        let first = view_model
            .begin_events_request()
            .expect("first event request");
        assert_eq!(first.after, 0);
        assert_eq!(first.known_epoch, None);
        let events = (1..=505)
            .map(|id| DaemonEvent {
                id,
                timestamp: id.to_string(),
                level: "info".into(),
                component: String::new(),
                message: format!("event {id}"),
                details: None,
            })
            .collect();
        let first_page = view_model
            .finish_events(
                first.token,
                Ok(DaemonEventBatch {
                    epoch: "epoch-a".into(),
                    events,
                }),
            )
            .expect("current completion");
        assert_eq!(first_page.len(), 500);
        assert_eq!(first_page.first().map(|event| event.id), Some(6));

        let second = view_model
            .begin_events_request()
            .expect("second event request");
        assert_eq!(second.after, 505);
        assert_eq!(second.known_epoch.as_deref(), Some("epoch-a"));
        let second_page = view_model
            .finish_events(
                second.token,
                Ok(DaemonEventBatch {
                    epoch: "epoch-b".into(),
                    events: vec![DaemonEvent {
                        id: 1,
                        timestamp: "new".into(),
                        level: "warning".into(),
                        component: String::new(),
                        message: "new epoch".into(),
                        details: None,
                    }],
                }),
            )
            .expect("epoch completion");
        assert_eq!(
            second_page.iter().map(|event| event.id).collect::<Vec<_>>(),
            [1]
        );
        let request = view_model
            .begin_events_request()
            .expect("request after epoch reset");
        assert_eq!(request.after, 1);
        assert_eq!(request.known_epoch.as_deref(), Some("epoch-b"));
    }

    #[test]
    fn event_requests_coalesce_and_completion_uses_the_current_local_filter() {
        let client = Arc::new(MockDaemonClient::ready());
        let mut view_model = DesktopViewModel::new(client.clone());
        let request = view_model.begin_events_request().expect("event request");
        assert!(view_model.begin_events_request().is_none());
        let local = view_model.set_event_filter(Some("error"));
        assert!(local.is_empty());
        assert!(client.calls.lock().expect("mock call lock").is_empty());

        let filtered = view_model
            .finish_events(
                request.token,
                Ok(DaemonEventBatch {
                    epoch: "epoch-a".into(),
                    events: vec![
                        DaemonEvent {
                            id: 1,
                            timestamp: "1".into(),
                            level: "info".into(),
                            component: String::new(),
                            message: "info".into(),
                            details: None,
                        },
                        DaemonEvent {
                            id: 2,
                            timestamp: "2".into(),
                            level: "error".into(),
                            component: String::new(),
                            message: "error".into(),
                            details: None,
                        },
                    ],
                }),
            )
            .expect("current completion");
        assert_eq!(
            filtered.iter().map(|event| event.id).collect::<Vec<_>>(),
            [2]
        );

        let newer = view_model.begin_events_request().expect("newer request");
        assert!(
            view_model
                .finish_events(
                    request.token,
                    Ok(DaemonEventBatch {
                        epoch: "epoch-a".into(),
                        events: Vec::new(),
                    }),
                )
                .is_none()
        );
        assert!(
            view_model
                .finish_events(
                    newer.token,
                    Ok(DaemonEventBatch {
                        epoch: "epoch-a".into(),
                        events: Vec::new(),
                    }),
                )
                .is_some()
        );
    }

    #[test]
    fn diagnostics_export_includes_structured_details_ignores_the_active_filter_and_stays_redacted()
    {
        let client = Arc::new(MockDaemonClient::ready());
        let mut view_model = DesktopViewModel::new(client);
        let request = view_model.begin_events_request().expect("event request");
        view_model
            .finish_events(
                request.token,
                Ok(DaemonEventBatch {
                    epoch: "epoch-a".into(),
                    events: vec![DaemonEvent {
                        id: 1,
                        timestamp: "12:00".into(),
                        level: "warning".into(),
                        component: "subscription".into(),
                        message: "Subscription download failed: timed_out.".into(),
                        details: Some(serde_json::json!({
                            "host": "provider.example",
                            "native": "http_status(404)",
                            "mihomo": "tls_rejected",
                            "xray": "timed_out",
                            "elapsed_ms": 1234,
                            "os": "windows",
                        })),
                    }],
                }),
            )
            .expect("current completion");
        // Only errors would survive this filter; the export must ignore it.
        view_model.set_event_filter(Some("error"));

        let export = view_model.diagnostics_export_text();
        assert!(export.contains("provider.example"), "{export}");
        assert!(export.contains("tls_rejected"), "{export}");
        assert!(export.contains("timed_out"), "{export}");
        assert!(export.contains("elapsed_ms=1234"), "{export}");
        assert!(export.contains("subscription"), "{export}");
        assert!(!export.contains("://"), "{export}");
    }

    #[test]
    fn diagnostics_export_reports_an_empty_state_instead_of_a_blank_string() {
        let client = Arc::new(MockDaemonClient::ready());
        let view_model = DesktopViewModel::new(client);
        assert_eq!(view_model.diagnostics_export_text(), "Событий пока нет.");
    }

    fn catalog_fixture() -> DaemonCatalog {
        DaemonCatalog {
            revision: 91,
            groups: vec![
                DaemonCatalogGroup {
                    id: "opaque:first".into(),
                    label: "Резерв https://private.invalid/path".into(),
                    selected: false,
                    nodes: vec![DaemonCatalogNode {
                        id: "opaque:node-a".into(),
                        label: "Север".into(),
                        selected: false,
                        delay_ms: None,
                    }],
                },
                DaemonCatalogGroup {
                    id: "opaque:selected".into(),
                    label: "Основной".into(),
                    selected: true,
                    nodes: vec![
                        DaemonCatalogNode {
                            id: "opaque:node-b".into(),
                            label: "Восток".into(),
                            selected: true,
                            delay_ms: Some(37),
                        },
                        DaemonCatalogNode {
                            id: "opaque:node-c".into(),
                            label: "Запад".into(),
                            selected: false,
                            delay_ms: Some(51),
                        },
                    ],
                },
            ],
        }
    }

    #[test]
    fn catalog_preserves_revision_and_opaque_ids_and_uses_selected_group() {
        let mut view_model = DesktopViewModel::new(Arc::new(MockDaemonClient::ready()));
        let request = view_model.begin_catalog_request().expect("catalog request");
        assert!(view_model.begin_catalog_request().is_none());

        let catalog = view_model
            .finish_catalog(request.token, Ok(catalog_fixture()))
            .expect("current catalog completion");

        assert_eq!(catalog.revision, Some(91));
        assert_eq!(catalog.groups[0].id, "opaque:first");
        assert_eq!(catalog.groups[1].id, "opaque:selected");
        assert_eq!(
            catalog.selected_group_id.as_deref(),
            Some("opaque:selected")
        );
        assert_eq!(catalog.nodes[0].id, "opaque:node-b");
        assert_eq!(catalog.nodes[0].delay_ms, Some(37));
        assert!(!catalog.groups[0].label.contains("private.invalid"));
        assert!(catalog.error.is_none());
    }

    #[test]
    fn tray_catalog_snapshot_bounds_groups_and_total_nodes() {
        let view_model = connected_view_model_with_catalog();

        let one_group = view_model.tray_catalog_groups(1, 1);
        assert_eq!(one_group.len(), 1);
        assert_eq!(one_group[0].nodes.len(), 1);

        let all_groups = view_model.tray_catalog_groups(12, 1);
        assert_eq!(all_groups.len(), 2);
        assert_eq!(
            all_groups
                .iter()
                .map(|group| group.nodes.len())
                .sum::<usize>(),
            1
        );
    }

    #[test]
    fn restored_route_preferences_are_applied_once_when_catalog_arrives() {
        let mut view_model = DesktopViewModel::new(Arc::new(MockDaemonClient::ready()));
        view_model.active_profile_id = Some("profile-a".into());
        let mut selections = BTreeMap::new();
        selections.insert("opaque:selected".into(), "opaque:node-c".into());
        view_model.restore_route_preferences(Some("opaque:selected"), &selections);

        let request = view_model.begin_catalog_request().expect("catalog request");
        view_model
            .finish_catalog(request.token, Ok(catalog_fixture()))
            .expect("catalog completion");

        let presentation = view_model.catalog_presentation();
        assert_eq!(
            presentation.selected_group_id.as_deref(),
            Some("opaque:selected")
        );
        let selected = presentation
            .nodes
            .iter()
            .find(|node| node.selected)
            .expect("restored selected node");
        assert_eq!(selected.id, "opaque:node-c");
        let saved = view_model.route_preferences().expect("route preferences").2;
        assert_eq!(saved, selections);
    }

    #[test]
    fn viewed_group_survives_revision_without_replaying_stale_node_ids() {
        let old = catalog_fixture();
        let mut original = DesktopViewModel::new(Arc::new(MockDaemonClient::ready()));
        original.active_profile_id = Some("profile-a".into());
        let request = original.begin_catalog_request().unwrap();
        original
            .finish_catalog(request.token, Ok(old.clone()))
            .unwrap();
        let (_, selected_group, selections) = original.route_preferences().unwrap();
        let group_key = selected_group.as_deref().unwrap();
        assert!(group_key.starts_with("label-sha256:"));
        assert!(!group_key.contains(&old.groups[1].label));
        assert_eq!(
            selections.get("opaque:selected").map(String::as_str),
            Some("opaque:node-b")
        );

        let mut revised = old;
        revised.revision += 1;
        for (group_index, group) in revised.groups.iter_mut().enumerate() {
            group.id = format!("new-group-{group_index}");
            group.selected = false;
            for (node_index, node) in group.nodes.iter_mut().enumerate() {
                node.id = format!("new-node-{group_index}-{node_index}");
                node.selected = false;
            }
        }
        let mut restored = DesktopViewModel::new(Arc::new(MockDaemonClient::ready()));
        restored.active_profile_id = Some("profile-a".into());
        restored.restore_route_preferences(selected_group.as_deref(), &selections);
        let request = restored.begin_catalog_request().unwrap();
        restored.finish_catalog(request.token, Ok(revised)).unwrap();
        let catalog = restored.catalog_presentation();
        assert_eq!(catalog.selected_group_id.as_deref(), Some("new-group-1"));
        assert!(catalog.nodes.iter().all(|node| !node.selected));
    }

    #[test]
    fn delayed_catalog_network_call_does_not_hold_the_model_lock() {
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let client = Arc::new(DelayedCatalogClient {
            started: Mutex::new(Some(started_tx)),
            release: Mutex::new(release_rx),
        });
        let model = Arc::new(Mutex::new(DesktopViewModel::new(client)));
        let request = model
            .lock()
            .expect("view model lock")
            .begin_catalog_request()
            .expect("catalog request");
        let worker_model = model.clone();
        let worker = thread::spawn(move || {
            let result = request.client.catalog();
            worker_model
                .lock()
                .expect("view model lock")
                .finish_catalog(request.token, result)
        });

        started_rx.recv().expect("catalog request started");
        {
            let mut model = model
                .try_lock()
                .expect("model lock must remain available during catalog I/O");
            model.open_panel(Panel::Subscription);
            assert_eq!(model.panel(), Panel::Subscription);
        }
        release_tx.send(()).expect("release catalog request");
        assert!(worker.join().expect("join catalog worker").is_some());
    }

    #[test]
    fn catalog_uses_first_group_when_none_selected_and_changes_group_locally_by_id() {
        let client = Arc::new(MockDaemonClient::ready());
        let mut view_model = DesktopViewModel::new(client.clone());
        let mut fixture = catalog_fixture();
        for group in &mut fixture.groups {
            group.selected = false;
        }
        let request = view_model.begin_catalog_request().expect("catalog request");
        view_model
            .finish_catalog(request.token, Ok(fixture))
            .expect("catalog completion");

        assert_eq!(
            view_model
                .catalog_presentation()
                .selected_group_id
                .as_deref(),
            Some("opaque:first")
        );
        assert!(view_model.select_catalog_group("opaque:selected"));
        let selected = view_model.catalog_presentation();
        assert_eq!(
            selected.selected_group_id.as_deref(),
            Some("opaque:selected")
        );
        assert_eq!(selected.nodes[0].id, "opaque:node-b");
        assert!(client.calls.lock().expect("mock calls").is_empty());
        assert!(!view_model.select_catalog_group("unknown"));
    }

    #[test]
    fn empty_and_failed_catalogs_have_safe_visible_states_and_stale_results_are_ignored() {
        let mut view_model = DesktopViewModel::new(Arc::new(MockDaemonClient::ready()));
        let empty = view_model
            .begin_catalog_request()
            .expect("empty catalog request");
        let presentation = view_model
            .finish_catalog(
                empty.token,
                Ok(DaemonCatalog {
                    revision: 5,
                    groups: Vec::new(),
                }),
            )
            .expect("empty catalog completion");
        assert_eq!(presentation.revision, Some(5));
        assert!(presentation.groups.is_empty());
        assert!(presentation.nodes.is_empty());
        assert!(presentation.error.is_none());

        let failed = view_model
            .begin_catalog_request()
            .expect("failed catalog request");
        let failure = view_model
            .finish_catalog(
                failed.token,
                Err(DaemonError::new(
                    "unsafe https://private.invalid token=secret",
                )),
            )
            .expect("current failed completion");
        assert_eq!(
            failure.error.as_deref(),
            Some("Не удалось загрузить маршруты. Повторите попытку.")
        );
        assert!(
            !failure
                .error
                .as_deref()
                .unwrap_or_default()
                .contains("private")
        );

        let newer = view_model
            .begin_catalog_request()
            .expect("new catalog request");
        assert!(
            view_model
                .finish_catalog(failed.token, Ok(catalog_fixture()))
                .is_none()
        );
        assert!(
            view_model
                .finish_catalog(newer.token, Ok(catalog_fixture()))
                .is_some()
        );
    }

    fn connected_view_model_with_catalog() -> DesktopViewModel {
        let mut view_model = DesktopViewModel::new(Arc::new(MockDaemonClient::ready()));
        let refresh = view_model.begin_refresh();
        view_model.finish_refresh(
            refresh,
            Ok(DaemonStatus {
                state: "connected".into(),
                profile: Some("Тестовый".into()),
                current_node: Some("Первый".into()),
                ..DaemonStatus::default()
            }),
        );
        let request = view_model.begin_catalog_request().expect("catalog request");
        view_model
            .finish_catalog(request.token, Ok(catalog_fixture()))
            .expect("catalog completion");
        view_model
    }

    #[test]
    fn selection_is_allowed_only_for_connected_fresh_catalog_and_suppresses_repeated_clicks() {
        let mut view_model = connected_view_model_with_catalog();
        assert!(view_model.catalog_presentation().selection_enabled);

        let request = view_model
            .begin_node_selection("opaque:node-c")
            .expect("valid selection");
        assert_eq!(request.revision, 91);
        assert!(!view_model.catalog_presentation().selection_enabled);
        assert!(view_model.begin_node_selection("opaque:node-c").is_none());
        assert!(
            view_model
                .catalog_presentation()
                .nodes
                .iter()
                .find(|node| node.id == "opaque:node-c")
                .expect("selected node")
                .selected
        );

        let mut disconnected = DesktopViewModel::new(Arc::new(MockDaemonClient::ready()));
        let refresh = disconnected.begin_refresh();
        disconnected.finish_refresh(refresh, Ok(MockDaemonClient::ready().status));
        let catalog = disconnected
            .begin_catalog_request()
            .expect("catalog request while disconnected");
        disconnected.finish_catalog(catalog.token, Ok(catalog_fixture()));
        assert!(disconnected.begin_node_selection("opaque:node-c").is_none());
        let queued = disconnected.queued_node_selections();
        assert_eq!(queued.len(), 1);
        let queued = &queued[0];
        assert_eq!(queued.group_id, "opaque:selected");
        assert_eq!(queued.node_id, "opaque:node-c");
        assert_eq!(queued.revision, 91);
        assert!(disconnected.catalog_presentation().selection_queued);
    }

    #[test]
    fn ready_state_queues_one_selection_per_group_for_connect() {
        let mut view_model = DesktopViewModel::new(Arc::new(MockDaemonClient::ready()));
        let refresh = view_model.begin_refresh();
        view_model.finish_refresh(refresh, Ok(MockDaemonClient::ready().status));
        let catalog = view_model
            .begin_catalog_request()
            .expect("catalog request while ready");
        view_model.finish_catalog(catalog.token, Ok(catalog_fixture()));

        assert!(view_model.begin_node_selection("opaque:node-c").is_none());
        assert!(view_model.select_catalog_group("opaque:first"));
        assert!(view_model.begin_node_selection("opaque:node-a").is_none());

        let queued = view_model.queued_node_selections();
        assert_eq!(queued.len(), 2);
        assert_eq!(queued[0].group_id, "opaque:selected");
        assert_eq!(queued[0].node_id, "opaque:node-c");
        assert_eq!(queued[1].group_id, "opaque:first");
        assert_eq!(queued[1].node_id, "opaque:node-a");
    }

    #[test]
    fn secondary_group_selection_never_overrides_primary_connection_summary() {
        let mut view_model = DesktopViewModel::new(Arc::new(MockDaemonClient::ready()));
        let refresh = view_model.begin_refresh();
        view_model.finish_refresh(refresh, Ok(MockDaemonClient::ready().status));
        let catalog = view_model.begin_catalog_request().expect("catalog request");
        view_model.finish_catalog(catalog.token, Ok(catalog_fixture()));

        assert!(view_model.begin_node_selection("opaque:node-c").is_none());
        let primary_summary = view_model.presentation().node;

        assert!(view_model.select_catalog_group("opaque:first"));
        assert!(view_model.begin_node_selection("opaque:node-a").is_none());
        assert_eq!(
            view_model.presentation().node,
            primary_summary,
            "a secondary selector must not replace the primary server summary"
        );
    }

    #[test]
    fn incomplete_queued_selection_batch_is_preserved_for_connect_retry() {
        let mut view_model = DesktopViewModel::new(Arc::new(MockDaemonClient::ready()));
        let refresh = view_model.begin_refresh();
        view_model.finish_refresh(refresh, Ok(MockDaemonClient::ready().status));
        let catalog = view_model.begin_catalog_request().expect("catalog request");
        view_model.finish_catalog(catalog.token, Ok(catalog_fixture()));
        assert!(view_model.begin_node_selection("opaque:node-c").is_none());
        let queued = view_model.queued_node_selections();

        assert!(!view_model.finish_queued_node_selections(&queued, Vec::new()));
        assert_eq!(view_model.queued_node_selections(), queued);
    }

    #[test]
    fn failed_connect_preserves_queued_selections_for_retry() {
        let mut view_model = DesktopViewModel::new(Arc::new(MockDaemonClient::ready()));
        let refresh = view_model.begin_refresh();
        view_model.finish_refresh(refresh, Ok(MockDaemonClient::ready().status));
        let catalog = view_model.begin_catalog_request().expect("catalog request");
        view_model.finish_catalog(catalog.token, Ok(catalog_fixture()));
        assert!(view_model.begin_node_selection("opaque:node-c").is_none());
        let queued = view_model.queued_node_selections();
        let (action, mutation) = view_model.begin_primary_action().expect("connect mutation");
        assert_eq!(action, PrimaryAction::Connect);

        view_model.finish_connect(mutation, Err(DaemonError::new("connect failed")));

        assert_eq!(view_model.queued_node_selections(), queued);
        assert!(view_model.catalog_presentation().selection_queued);
    }

    #[test]
    fn group_navigation_remains_enabled_for_loaded_read_only_catalogs_but_locks_while_loading() {
        for daemon_state in ["ready", "disconnected"] {
            let mut view_model = DesktopViewModel::new(Arc::new(MockDaemonClient::ready()));
            let refresh = view_model.begin_refresh();
            view_model.finish_refresh(
                refresh,
                Ok(DaemonStatus {
                    state: daemon_state.into(),
                    profile: Some("Тестовый".into()),
                    current_node: Some("Восток".into()),
                    ..DaemonStatus::default()
                }),
            );
            let catalog = view_model.begin_catalog_request().expect("catalog request");
            view_model
                .finish_catalog(catalog.token, Ok(catalog_fixture()))
                .expect("loaded catalog");

            let read_only = view_model.catalog_presentation();
            assert!(read_only.group_navigation_enabled, "state: {daemon_state}");
            assert!(read_only.selection_enabled, "state: {daemon_state}");
            assert!(view_model.select_catalog_group("opaque:first"));

            let loading = view_model
                .begin_catalog_request()
                .expect("refresh loaded catalog");
            assert!(!view_model.catalog_presentation().group_navigation_enabled);
            assert!(!view_model.select_catalog_group("opaque:selected"));
            view_model.finish_catalog(loading.token, Ok(catalog_fixture()));
        }
    }

    #[test]
    fn slint_group_chips_use_local_navigation_gate_not_node_selection_gate() {
        let source = include_str!("../ui/app.slint");

        assert!(source.contains(
            "private property <bool> catalog-group-navigation-enabled: !root.catalog-loading && root.catalog-groups.length > 0;"
        ));
        assert!(source.contains("enabled: root.catalog-group-navigation-enabled;"));
        assert!(!source.contains(
            "enabled: root.catalog-selection-enabled;\n                            checkable: true;"
        ));
    }

    #[test]
    fn expiry_decades_away_reads_as_unlimited() {
        let now = 1_790_000_000;
        let day = 86_400;
        assert_eq!(format_expiry(None, now), "Срок неизвестен");
        assert_eq!(format_expiry(Some(now), now), "Срок истёк");
        assert_eq!(format_expiry(Some(now + day), now), "Осталось 1 день");
        assert_eq!(
            format_expiry(Some(now + UNLIMITED_EXPIRY_DAYS * day), now),
            "Осталось 3650 дней"
        );
        assert_eq!(format_expiry(Some(now + 12_073 * day), now), "Бессрочно");
    }

    #[test]
    fn status_node_splits_only_a_real_leading_flag_or_symbol() {
        assert_eq!(
            split_leading_symbol("🇸🇪 Auto Sweden"),
            ("🇸🇪".to_owned(), "Auto Sweden".to_owned())
        );
        assert_eq!(
            split_leading_symbol("🌍 Автовыбор"),
            ("🌍".to_owned(), "Автовыбор".to_owned())
        );
        assert_eq!(
            split_leading_symbol("Auto Sweden"),
            (String::new(), "Auto Sweden".to_owned()),
            "no icon may be invented for a plain name"
        );
        assert_eq!(
            split_leading_symbol("Маршрут не выбран"),
            (String::new(), "Маршрут не выбран".to_owned())
        );
    }

    #[test]
    fn catalog_display_keeps_reference_flag_and_symbol_ux() {
        assert_eq!(
            derive_catalog_display("🇩🇪 Germany [de]", CatalogDisplayKind::Group),
            CatalogDisplay {
                icon: "🇩🇪".into(),
                label: "Germany [de]".into(),
            }
        );
        assert_eq!(
            derive_catalog_display("🌍 Сервер", CatalogDisplayKind::Group),
            CatalogDisplay {
                icon: "🌍".into(),
                label: "Сервер".into(),
            }
        );
        assert_eq!(
            derive_catalog_display("🎮 Игры", CatalogDisplayKind::Group),
            CatalogDisplay {
                icon: "🎮".into(),
                label: "Игры".into(),
            }
        );
        assert_eq!(
            derive_catalog_display("Без VPN", CatalogDisplayKind::Node),
            CatalogDisplay {
                icon: "↗".into(),
                label: "Без VPN".into(),
            }
        );
        assert_eq!(
            derive_catalog_display("Minecraft (javaw.exe)", CatalogDisplayKind::Group),
            CatalogDisplay {
                icon: "🎮".into(),
                label: "Minecraft (javaw.exe)".into(),
            }
        );
    }

    #[test]
    #[ignore = "superseded by desktop_control_center_source_contract"]
    fn reference_driven_catalog_visual_contract() {
        let theme = include_str!("../ui/theme.slint");
        let components = include_str!("../ui/components.slint");
        let app = include_str!("../ui/app.slint");
        let build = include_str!("../build.rs");
        let main = include_str!("main.rs");

        for token in [
            "void: #08090c;",
            "canvas: #0a0a0d;",
            "surface: #111114;",
            "elevated: #1a1a1f;",
            "line: #242429;",
            "accent-blue: #0091ff;",
            "accent-green: #22c55e;",
            "radius-large: 14px;",
        ] {
            assert!(theme.contains(token), "missing reference token: {token}");
        }
        assert!(app.matches("icon: string,").count() >= 2);
        assert!(app.contains("icon: group.icon;"));
        assert!(app.contains("icon: node.icon;"));
        assert!(app.contains("preferred-height: 92px;"));
        assert!(!app.contains("preferred-height: 136px;"));
        assert!(app.contains("if !root.has-profile: PrimaryAction"));
        assert!(app.contains("connection-touch := TouchArea"));
        assert!(app.contains("accessible-label: root.primary-label;"));
        assert!(components.matches("in property <string> icon;").count() >= 2);
        assert!(components.contains("text: root.icon;"));
        assert!(theme.contains("emoji-font-family: \"Twemoji Mozilla\";"));
        assert!(
            components
                .matches("font-family: Theme.emoji-font-family;")
                .count()
                >= 2
        );
        assert!(components.contains("import \"../assets/fonts/Twemoji.Mozilla.ttf\";"));
        assert!(build.contains("EmbedResourcesKind::EmbedFiles"));
        let notices = include_str!("../../../packaging/windows-x64/THIRD_PARTY_NOTICES.md");
        assert!(notices.contains("Twemoji Mozilla font"));
        assert!(notices.contains("https://github.com/mozilla/twemoji-colr"));
        assert!(notices.contains("CC BY 4.0"));
        assert!(components.contains("text: root.selected ? \"✓\""));
        assert!(main.contains("derive_catalog_display"));
        assert!(main.contains("icon: display.icon.into()"));
        assert!(main.contains("label: display.label.into()"));

        for rejected in [
            "BrandMark",
            "text: \"Локально\";",
            "text: \"Подписка\";",
            "label: \"Заменить\";",
            "? \"✓\" : \"↗\"",
        ] {
            assert!(
                !app.contains(rejected),
                "rejected visual remains: {rejected}"
            );
        }
        assert!(app.contains("source: @image-url(\"../assets/power.svg\");"));
        assert!(app.contains("state-label: \"Изменить ссылку подписки\";"));
        assert!(components.contains("accessible-label: root.state-label;"));
        assert!(app.contains("utility-bar := Rectangle"));
        assert!(app.contains("text: \"Главная\";"));
        assert!(
            app.contains(
                "label: root.subscription-refresh-pending ? \"Обновляем\" : \"Обновить\";"
            )
        );
    }

    #[test]
    #[ignore = "superseded by desktop_control_center_source_contract"]
    fn quiet_signal_responsive_inline_home_contract() {
        let source = include_str!("../ui/app.slint");
        let build = include_str!("../build.rs");
        let main = include_str!("main.rs");

        for required in [
            "preferred-width: 480px;",
            "preferred-height: 720px;",
            "min-width: 420px;",
            "min-height: 640px;",
            "max-width: 620px;",
            "max-height: 920px;",
            "VerticalLayout",
            "HorizontalLayout",
            "padding: 20px;",
            "preferred-height: 44px;",
            "preferred-height: 92px;",
            "LineEdit",
            "multicore://install-sub?url=…",
            "QuietChip",
            "RouteNodeRow",
            "ScrollView",
            "DetailsSurface",
            "Диагностика",
            "root.open-panel(\"events\")",
            "root.refresh-subscription();",
            "subscription-refresh-enabled",
            "root.catalog-selection-queued ? \"После подключения\"",
            "event.text == Key.Escape",
            "diagnostics-action.focus();",
        ] {
            assert!(
                source.contains(required),
                "missing inline-home contract: {required}"
            );
        }

        assert_eq!(
            source.matches("PrimaryAction {").count(),
            1,
            "the main surface must expose exactly one dominant action"
        );
        assert!(source.contains("root.import-url(root.subscription-draft);"));
        assert!(source.contains("root.primary-action();"));
        assert!(source.contains("activated => { root.select-catalog-group(group.id); }"));
        assert!(source.contains("activated => { root.select-catalog-node(node.id); }"));

        for rejected in [
            "Button",
            "StatePill",
            "GlowCard",
            "NavigationRow",
            "OverlayPanel",
            "events-row",
            "active-panel == \"subscription\"",
            "active-panel == \"routes\"",
            "background: #06060acc",
        ] {
            assert!(
                !source.contains(rejected),
                "legacy or fixed-screen composition remains: {rejected}"
            );
        }
        assert!(
            !source
                .lines()
                .any(|line| line.trim_start().starts_with("y:")),
            "fixed screen-position y bindings remain"
        );

        for legacy in ["StatePill", "GlowCard", "NavigationRow", "OverlayPanel"] {
            assert!(
                !build.contains(legacy),
                "build validation still imports {legacy}"
            );
        }
        assert!(main.contains("ui.set_has_profile(presentation.has_profile);"));
        assert!(main.contains("if refresh_catalog && current.presentation.has_profile"));
        assert!(main.contains("load_catalog(weak, model);"));
    }

    #[test]
    fn home_background_is_bounded_pausable_and_non_interactive() {
        let source = include_str!("../ui/app.slint");
        let components = include_str!("../ui/components.slint");

        for required in [
            "in-out property <bool> ambient-background-enabled",
            "in-out property <bool> shell-active",
            "in property <bool> reduced-motion",
            "wave-timer := Timer",
            "wave-kickoff := Timer",
            "private property <bool> ambient-wave-started: false",
            "running: root.ambient-motion-active && !root.ambient-wave-started",
            "interval: 80ms",
            "root.ambient-wave-started = true",
            "root.ambient-wave-phase = !root.ambient-wave-phase",
            "private property <bool> ambient-motion-active: root.ambient-background-enabled && root.shell-active && !root.reduced-motion && !root.minimized && root.visible-page == \"home\"",
            "interval: 12s",
        ] {
            assert!(
                source.contains(required),
                "missing ambient contract: {required}"
            );
        }

        assert_eq!(source.matches("ambient-wave-a := Path").count(), 1);
        assert_eq!(source.matches("ambient-wave-b := Path").count(), 1);
        assert!(!source.to_ascii_lowercase().contains("scanline"));
        assert!(!source.to_ascii_lowercase().contains("particle"));

        let first_wave = source
            .find("ambient-wave-a :=")
            .expect("first ambient wave marker");
        let home_content = source[first_wave..]
            .find("VerticalLayout {")
            .map(|offset| first_wave + offset)
            .expect("home content follows ambient waves");
        let wave_layer = &source[first_wave..home_content];
        let home_layer = source
            .split("if root.visible-page == \"home\": Rectangle {")
            .nth(1)
            .and_then(|source| source.split("VerticalLayout {").next())
            .expect("home ambient layer");
        assert!(home_layer.contains("clip: true"));
        assert!(!wave_layer.contains("TouchArea"));
        assert!(!wave_layer.contains("blur"));
        assert!(wave_layer.contains("ambient-wave-fade := Rectangle"));
        assert!(wave_layer.contains("@linear-gradient(180deg"));
        assert!(wave_layer.contains("Theme.canvas 88%"));

        let wave_a = source
            .split("ambient-wave-a := Path {")
            .nth(1)
            .and_then(|source| source.split("ambient-wave-b := Path {").next())
            .expect("wave A block");
        for required in [
            "x: root.ambient-wave-phase ? -12px : -48px;",
            "y: root.ambient-wave-phase ? 48px : 76px;",
            "height: parent.height + 80px;",
            "viewbox-height: 800;",
            "L 1000 800 L 0 800 Z",
            "opacity: 0.026;",
            "animate x { duration: 12000ms; easing: ease-in-out; }",
            "animate y { duration: 12000ms; easing: ease-in-out; }",
        ] {
            assert!(wave_a.contains(required), "wave A missing: {required}");
        }
        let wave_b = source
            .split("ambient-wave-b := Path {")
            .nth(1)
            .and_then(|source| source.split("VerticalLayout {").next())
            .expect("wave B block");
        for required in [
            "x: root.ambient-wave-phase ? -58px : -22px;",
            "y: root.ambient-wave-phase ? 288px : 254px;",
            "height: parent.height + 80px;",
            "viewbox-height: 800;",
            "L 1000 800 L 0 800 Z",
            "opacity: 0.018;",
            "animate x { duration: 12000ms; easing: ease-in-out; }",
            "animate y { duration: 12000ms; easing: ease-in-out; }",
        ] {
            assert!(wave_b.contains(required), "wave B missing: {required}");
        }
        // The connection module is a flat surface; state lives in its hairline and badge.
        assert!(source.contains("background: root.has-profile ? Theme.surface : #101721;"));
        assert!(components.contains(
            "background: root.selected ? Theme.elevated : row-touch.has-hover && root.enabled ? #ffffff06 : transparent;"
        ));
        assert!(!components.contains("#172433"));
        assert!(home_layer.contains("ambient-pointer-glow := Rectangle"));
        assert!(home_layer.contains("ambient-touch.mouse-x"));
        assert!(home_layer.contains("ambient-touch.mouse-y"));
        for cursor in ["ns-resize", "ew-resize", "nwse-resize", "nesw-resize"] {
            assert!(source.contains(&format!("mouse-cursor: {cursor};")));
        }
    }

    #[test]
    fn home_chrome_has_no_orphan_update_dot_and_uses_system_spacing() {
        let source = include_str!("../ui/app.slint");
        let title_bar = source
            .split("title-bar := Rectangle")
            .nth(1)
            .and_then(|source| source.split("HorizontalLayout {").next())
            .expect("title bar source");
        assert!(!title_bar.contains("if root.update-available: Rectangle"));

        let home_stack = source
            .split("if root.visible-page == \"home\": Rectangle {")
            .nth(1)
            .and_then(|source| source.split("connection-strip := Rectangle").next())
            .and_then(|source| source.rsplit("VerticalLayout {").next())
            .expect("home content stack");
        assert!(home_stack.contains("padding: 20px;"));
        // One step between the connection module and the routes (no extra gap).
        assert!(home_stack.contains("spacing: 16px;"));
        assert!(!home_stack.contains("spacing: 8px;"));
        assert!(!home_stack.contains("spacing: 10px;"));
    }

    #[test]
    fn status_and_settings_keep_stable_bottom_rhythm_and_build_identity() {
        let source = include_str!("../ui/app.slint");
        let status = source
            .split("if root.visible-page == \"status\": Rectangle {")
            .nth(1)
            .and_then(|source| source.split("if root.visible-page == \"settings\"").next())
            .expect("status page block");
        for padding in [
            "padding-left: 28px;",
            "padding-right: 28px;",
            "padding-top: 28px;",
            "padding-bottom: 40px;",
        ] {
            assert!(status.contains(padding), "status missing {padding}");
        }

        assert!(!source.contains("Mihomo 1.19.30"));
        assert!(source.contains(
            "if root.visible-page == \"settings\": Text { height: 24px; text: root.runtime-versions;"
        ));
        assert!(
            include_str!("main.rs")
                .contains("ui.set_runtime_versions(env!(\"MULTICORE_RUNTIME_VERSIONS\").into());")
        );
        assert!(include_str!("../build.rs").contains("packaging/windows-x64/versions.json"));
    }

    #[test]
    fn settings_page_scrolls_at_minimum_height_without_flattening_hierarchy() {
        let source = include_str!("../ui/app.slint");
        let settings = source
            .split(
                "if root.visible-page == \"settings\" || root.visible-page == \"subscriptions\": Rectangle {",
            )
            .nth(1)
            .and_then(|source| source.split("changed minimized =>").next())
            .expect("settings page block");

        assert!(settings.contains("settings-scroll := ScrollView"));
        assert!(settings.contains("settings-content := VerticalLayout"));
        assert!(settings.contains("vertical-stretch: 1;"));
        assert!(settings.contains("width: settings-scroll.visible-width;"));
        assert!(settings.contains("min-height: settings-scroll.visible-height;"));
        assert!(settings.contains("padding: 28px;"));
        assert!(settings.contains("? settings-scroll.visible-width - 56px"));
        let subscription_header = settings
            .split("if root.visible-page == \"subscriptions\": VerticalLayout")
            .nth(1)
            .and_then(|source| source.split("if root.replace-editor-open").next())
            .expect("subscription header");
        assert!(subscription_header.contains("width: parent.width - 128px;"));
        assert!(subscription_header.contains("overflow: elide;"));
        assert!(settings.contains("// Historical minimum-height contract used spacing: 18px;."));
        assert!(settings.contains("text <=> root.subscription-draft;"));
        assert!(settings.matches("ToggleRow {").count() >= 2);
        assert!(
            settings.find("settings-scroll := ScrollView").unwrap()
                < settings.find("settings-content := VerticalLayout").unwrap()
        );
    }

    #[test]
    fn a_disabled_tun_reads_as_switched_off_not_as_a_failure() {
        let check = super::present_runtime_check(Some("disabled"), false);
        assert_eq!(check.value, "Выключен");
        assert_eq!(check.tone, "neutral");
    }

    #[test]
    fn tun_mode_toggle_is_windows_only_locked_while_connected_and_persisted() {
        let app = include_str!("../ui/app.slint");
        let main = include_str!("main.rs");
        let row = app
            .split("if root.tun-toggle-available: ToggleRow {")
            .nth(1)
            .and_then(|source| source.split("toggled(value)").next())
            .expect("TUN mode row");
        assert!(row.contains("label: \"Режим TUN — весь трафик\";"));
        assert!(row.contains("enabled: !root.toggle-on && !root.toggle-busy"));
        assert!(row.contains("\"Применится при следующем подключении\""));
        // First row of the general settings card.
        assert!(
            app.find("if root.tun-toggle-available: ToggleRow {")
                .unwrap()
                < app
                    .find("label: \"Запускать при входе в \" + root.system-name;")
                    .unwrap()
        );
        assert!(main.contains("ui.set_tun_toggle_available(cfg!(windows));"));
        assert!(main.contains("ui.set_tun_enabled(loaded_preferences.tun_enabled);"));
        assert!(main.contains("tracker.set_tun_enabled(ui.get_tun_enabled());"));
        assert!(main.contains("client.connect_with_tun(tun)"));
        assert!(main.contains("!ui.get_tun_toggle_available() || ui.get_tun_enabled()"));
    }

    #[test]
    fn expanded_rail_and_route_skeletons_match_the_mockup() {
        let app = include_str!("../ui/app.slint").replace("\r\n", "\n");
        let main = include_str!("main.rs");
        // The host resizes the window when the user toggles the rail.
        assert!(app.contains("callback navigation-toggled(bool);"));
        assert!(app.contains("root.navigation-toggled(root.navigation-collapsed);"));
        assert!(main.contains("ui.on_navigation_toggled(move |collapsed| {"));
        assert!(main.contains("rail_grow_delta("));
        assert!(main.contains("rail_shrink_width(growth.take(), maximized, inner.width)"));
        // Expanded rail: 18px icons, 13px labels, double chevron, hairlines.
        assert!(app.contains("width: root.compact ? 20px : 18px;"));
        assert!(app.contains("\"M 8 3 L 3 8 L 8 13 M 13 3 L 8 8 L 13 13\""));
        assert!(app.contains("brand-divider := Rectangle {"));
        assert!(app.contains("x: root.navigation-collapsed ? 56px : 184px;"));
        // Skeletons: shimmer reads animation-tick() only while a skeleton shows.
        assert!(app.contains(
            "private property <bool> catalog-skeleton: root.catalog-loading && root.catalog-nodes.length == 0;"
        ));
        assert!(app.contains(
            "private property <float> skeleton-pulse: (root.catalog-skeleton || root.profile-menu-loading) && !root.reduced-motion"
        ));
        assert!(app.contains("if root.catalog-skeleton: route-skeleton := VerticalLayout {"));
        assert!(app.contains("accessible-label: \"Загружаем маршруты\";"));
        assert!(!app.contains("\"Загружаем маршруты…\""));
        assert!(app.contains(
            "if root.catalog-loading && root.catalog-groups.length == 0: HorizontalLayout {"
        ));
        // Real rows fade in once, when they replace the skeleton.
        assert!(app.contains("routes-enter := Timer {\n        interval: 16ms;\n        running: root.routes-entering;"));
        assert!(app.contains("opacity: root.routes-entering ? 0 : 1;"));
    }

    #[test]
    fn redesign_motion_is_lightweight_and_respects_reduced_motion() {
        // Checkouts may use CRLF; the multi-line contracts below are written with LF.
        let app = include_str!("../ui/app.slint").replace("\r\n", "\n");
        // Page enter is a one-frame flag, not a running animation clock.
        assert!(app.contains(
            "page-enter := Timer {
        interval: 16ms;
        running: root.page-entering;"
        ));
        assert!(app.contains(
            "changed visible-page => {
        if (!root.reduced-motion) {
            root.page-entering = true;"
        ));
        for page in ["\"home\"", "\"status\""] {
            let head = format!(
                "if root.visible-page == {page}: Rectangle {{
                opacity: root.page-entering ? 0 : 1;"
            );
            assert!(app.contains(&head), "{page} must fade in");
        }
        // Continuous animation-tick() work only runs while its state is active.
        assert!(app.contains("if root.toggle-busy && !root.reduced-motion: Rectangle {"));
        assert!(app.contains(
            "transform-rotation: root.subscription-refresh-pending && !root.reduced-motion"
        ));
        // Rail labels fade in after the rail has widened.
        assert!(app.contains("animate opacity { duration: root.compact ? 80ms : 160ms; delay: root.compact ? 0ms : 90ms; easing: ease-out; }"));
        // Session timer shares the title's bottom edge; speeds show only with a value.
        assert!(app.contains("if root.session-uptime != \"\": Text {"));
        assert!(app.contains("if root.traffic-down != \"\": Text {"));
        assert!(app.contains("if root.traffic-up != \"\": Text {"));
    }

    #[test]
    fn first_run_home_uses_inline_subscription_onboarding() {
        let source = include_str!("../ui/app.slint");
        let onboarding = source
            .split("first-run-onboarding := VerticalLayout")
            .nth(1)
            .and_then(|source| source.split("first-run-toggle-preview := Rectangle").next())
            .expect("inline first-run onboarding block");

        for required in [
            "callback paste-subscription();",
            "callback dismiss-first-run-import();",
            "in-out property <bool> first-run-manual-open: false;",
            "private property <int> first-run-input-focus-serial: 0;",
            "private property <int> first-run-trigger-focus-serial: 0;",
            "private property <int> power-action-focus-serial: 0;",
        ] {
            assert!(
                source.contains(required),
                "missing root contract: {required}"
            );
        }

        for required in [
            "text: \"Добавьте подписку\";",
            "text: \"Скопируйте ссылку у провайдера — мы подхватим её из буфера обмена\";",
            "text: \"Ссылка хранится только на этом устройстве\";",
            "onboarding-copy := VerticalLayout",
            "first-run-actions := HorizontalLayout",
            "label: root.import-pending ? \"Добавляем…\" : \"Вставить из буфера\";",
            "root.paste-subscription();",
            "label: \"Ввести вручную\";",
            "focus-epoch: root.first-run-trigger-focus-serial;",
            "first-run-url-input := LineEdit",
            "accessible-label: \"URL подписки\";",
            "changed focus-serial => { self.focus(); }",
            "init => { self.focus(); }",
            "label: root.import-pending ? \"Добавляем…\" : \"Добавить\";",
            "root.import-url(root.subscription-draft);",
            "label: \"Отмена\";",
            "root.first-run-trigger-focus-serial += 1;",
            "root.dismiss-first-run-import();",
            "text: root.import-error;",
            "accessible-live-region: polite;",
        ] {
            assert!(
                onboarding.contains(required),
                "missing first-run source contract: {required}"
            );
        }

        assert!(onboarding.contains("if root.first-run-manual-open"));
        let paste_action = onboarding
            .split("PrimaryAction {")
            .nth(1)
            .and_then(|action| action.split("SmallAction {").next())
            .expect("paste action");
        assert!(paste_action.contains("enabled: !root.import-pending;"));
        let manual_trigger = onboarding
            .split("label: \"Ввести вручную\";")
            .nth(1)
            .and_then(|action| action.split("activated =>").next())
            .expect("manual trigger");
        assert!(manual_trigger.contains("enabled: !root.import-pending;"));
        let add_action = onboarding
            .split("label: root.import-pending ? \"Добавляем…\" : \"Добавить\";")
            .nth(1)
            .and_then(|action| action.split("activated =>").next())
            .expect("manual add action");
        assert!(
            add_action
                .contains("enabled: !root.import-pending && root.subscription-draft != \"\";")
        );
        let cancel_action = onboarding
            .split("label: \"Отмена\";")
            .nth(1)
            .and_then(|action| action.split("activated =>").next())
            .expect("manual cancel action");
        assert!(cancel_action.contains("enabled: !root.import-pending;"));
        assert!(
            onboarding.contains("if (!root.import-pending && root.subscription-draft != \"\")")
        );
        assert!(!onboarding.contains("root.local-page ="));
        assert!(!onboarding.contains("root.settings-section ="));
        assert!(!onboarding.contains("root.add-subscription();"));
        assert!(!onboarding.contains("root.open-panel("));

        let escape_handler = source
            .split("visual-frame := FocusScope {")
            .nth(1)
            .and_then(|frame| frame.split("\n        key-pressed(event) =>").nth(1))
            .and_then(|handler| handler.split("return reject;").next())
            .expect("root Escape handler");
        assert!(escape_handler.contains("if (!root.has-profile && root.first-run-manual-open)"));
        assert!(escape_handler.contains("root.dismiss-first-run-import();"));
        assert!(
            escape_handler.find("root.first-run-manual-open").unwrap()
                < escape_handler
                    .find("root.profile-delete-confirm-id")
                    .unwrap()
        );

        let success_handler = source
            .split("changed import-success-serial =>")
            .nth(1)
            .and_then(|source| source.split("changed active-panel =>").next())
            .expect("import success handler");
        assert!(success_handler.contains("root.subscription-draft = \"\";"));
        assert!(success_handler.contains("root.first-run-manual-open = false;"));
        assert!(success_handler.contains("if (root.has-profile && root.visible-page == \"home\")"));
        assert!(success_handler.contains("root.power-action-focus-serial += 1;"));
        assert!(success_handler.contains("root.profile-shelf-focus-serial += 1;"));

        let power_action = source
            .split("power-action := FocusScope")
            .nth(1)
            .and_then(|source| source.split("power-button := Rectangle").next())
            .expect("power action focus scope");
        assert!(
            power_action.contains("property <int> focus-epoch: root.power-action-focus-serial;")
        );
        assert!(power_action.contains("changed focus-epoch =>"));
        assert!(power_action.contains("self.focus();"));

        // Subscription management retains its independent add/edit flow.
        let management = source
            .split("if root.visible-page == \"subscriptions\": VerticalLayout")
            .nth(1)
            .expect("subscription management page");
        assert!(management.contains("root.add-subscription();"));
        assert!(management.contains("text <=> root.subscription-draft;"));
        assert!(management.contains("accessible-label: \"URL подписки\";"));
        assert!(management.contains("accepted => {"));
        assert!(management.contains("label: \"Отмена\";"));
        assert!(management.contains("root.import-pending ? \"Добавляем…\" : \"Добавить\""));
        assert!(management.contains("root.import-url(root.subscription-draft);"));
        // A failed deep-link import renders on the page even without the editor.
        assert!(
            management
                .contains("if root.import-error != \"\" && !root.replace-editor-open: Text {")
        );

        // First launch shows only onboarding: no subscription chip, toggle or route list noise.
        assert!(source.contains("if root.has-profile: title-subscription := Rectangle {"));
        assert!(source.contains("if root.has-profile: toggle-module := Rectangle {"));
        assert!(source.contains("if root.has-profile: mesh-clip := Rectangle {"));
        assert!(source.contains("if root.has-profile: inline-routes := Rectangle {"));
    }

    #[test]
    fn desktop_control_center_source_contract() {
        let source = include_str!("../ui/app.slint");
        // Checkouts may use CRLF; multi-line contracts below are written with LF.
        let components = include_str!("../ui/components.slint").replace("\r\n", "\n");
        let main = include_str!("main.rs");
        let logo_cache = include_str!("logo_cache.rs");

        let small_action = source
            .split("component SmallAction")
            .nth(1)
            .and_then(|source| source.split("component HealthRow").next())
            .expect("SmallAction source");
        assert!(small_action.contains("horizontal-stretch: 0;"));
        assert!(small_action.contains("preferred-width: small-label.preferred-width + 28px;"));
        assert!(small_action.contains("height: 44px;"));
        assert!(small_action.contains("in property <bool> selected: false;"));
        assert!(small_action.contains("accessible-description:"));

        assert!(source.contains("inline-routes := Rectangle"));
        assert!(source.contains("connection-strip := Rectangle"));
        assert!(source.contains("height: root.has-profile"));
        assert!(source.contains("? 88px"));
        assert!(source.contains("power-action := FocusScope"));
        assert!(source.contains("accessible-label: root.primary-label;"));
        assert!(source.contains("in property <image> service-logo;"));
        assert!(source.contains("in property <bool> has-service-logo: false;"));
        assert!(source.contains("source: root.service-logo;"));
        assert!(!source.contains("if !root.has-service-logo: Image"));
        // The power circle became a switch; the provider logo moved to the title chip.
        assert!(!source.contains("power-glyph := Image"));
        assert!(!source.contains("service-logo-badge := Rectangle"));
        assert!(source.contains("signal-mesh-field := Rectangle"));
        assert!(source.contains("strip-glow-touch := TouchArea"));
        let connection_strip = source
            .split("connection-strip := Rectangle")
            .nth(1)
            .and_then(|source| source.split("update-banner := Rectangle").next())
            .expect("connection strip source");
        let toggle_module = connection_strip
            .split("toggle-module := Rectangle")
            .nth(1)
            .and_then(|source| {
                source
                    .split("first-run-onboarding := VerticalLayout")
                    .next()
            })
            .expect("toggle module source");
        assert!(toggle_module.contains("shield-badge := Rectangle"));
        assert!(toggle_module.contains("width: 44px;"));
        assert!(toggle_module.contains("text: root.toggle-title;"));
        assert!(toggle_module.contains("if root.toggle-detail-is-exit: HorizontalLayout"));
        assert!(toggle_module.contains("alignment: start;"));
        assert!(toggle_module.contains("width: 72px;"));
        assert!(toggle_module.contains("height: 40px;"));
        assert!(toggle_module.contains("toggle-knob := Rectangle"));
        assert!(
            toggle_module.contains("x: root.toggle-on ? parent.width - self.width - 4px : 4px;")
        );
        assert!(
            toggle_module.contains("animate x, width { duration: 180ms; easing: ease-in-out; }")
        );
        assert!(
            connection_strip
                .find("signal-mesh-field := Rectangle")
                .expect("pill mesh")
                < connection_strip
                    .find("toggle-module := Rectangle")
                    .expect("toggle module")
        );
        let pill_mesh = connection_strip
            .split("signal-mesh-field := Rectangle")
            .nth(1)
            .and_then(|source| source.split("toggle-module := Rectangle").next())
            .expect("pill mesh source");
        assert!(pill_mesh.contains("x: 0;"));
        assert!(pill_mesh.contains("y: 0;"));
        assert!(pill_mesh.contains("width: parent.width;"));
        assert!(pill_mesh.contains("height: parent.height;"));
        assert!(pill_mesh.contains("strip-glow-touch.has-hover"));
        assert!(pill_mesh.contains("property <length> cursor-x: strip-glow-touch.mouse-x"));
        assert!(pill_mesh.contains("property <length> cursor-y: strip-glow-touch.mouse-y"));
        assert!(!connection_strip.contains("power-touch := TouchArea"));
        assert!(!connection_strip.contains("power-hover"));
        assert!(!connection_strip.contains("power-cursor-x"));
        assert!(!connection_strip.contains("power-cursor-y"));
        assert_eq!(
            connection_strip
                .matches("strip-glow-touch := TouchArea")
                .count(),
            1
        );
        assert!(connection_strip.contains("width: root.has-profile ? parent.width : 0px;"));
        assert!(connection_strip.contains("height: root.has-profile ? parent.height : 0px;"));
        assert!(connection_strip.contains("if (root.primary-enabled)"));
        assert!(connection_strip.contains("root.primary-action();"));
        assert!(connection_strip.contains("strip-glow-touch.pressed"));
        assert!(source.contains("subscription-refresh-action := FocusScope"));
        // The subscription shelf is gone from home: name, expiry, refresh and the
        // provider-notice bell live in the window title.
        assert!(!source.contains("subscription-strip := Rectangle"));
        assert!(!source.contains("profile-menu-reserve"));
        assert!(!source.contains("home-banner-offset"));
        let title_subscription = source
            .split("title-subscription := Rectangle {")
            .nth(1)
            .and_then(|source| source.split("TitleBarButton {").next())
            .expect("title subscription source");
        assert!(title_subscription.contains("subscription-chip := Rectangle"));
        assert!(title_subscription.contains("shelf-open-focus := FocusScope"));
        assert!(title_subscription.contains("chip-chevron := Path"));
        assert!(
            title_subscription
                .contains("root.subscription-refresh-pending ? \"Подписка обновляется\"")
        );
        assert!(title_subscription.contains("root.refresh-subscription();"));
        assert!(title_subscription.contains("announcement-bell := FocusScope"));
        assert!(title_subscription.contains("root.announcement-open = !root.announcement-open;"));
        assert!(title_subscription.contains("animate x { duration: 180ms; easing: ease-in-out; }"));
        assert!(pill_mesh.contains("opacity: self.active ? 1 : 0;"));
        assert!(!pill_mesh.contains("opacity: self.active ? 1 : 0.1;"));
        assert!(!source.contains("cursor-glow-field := Rectangle"));
        assert!(!source.contains("glow-x"));
        assert!(!source.contains("circle 176px at glow-x glow-y"));
        assert!(pill_mesh.contains("circle 160px at cursor-x cursor-y"));
        assert!(pill_mesh.contains("glow-strong 0%"));
        assert!(pill_mesh.contains("glow-medium 26%"));
        assert!(pill_mesh.contains("glow-soft 52%"));
        assert!(source.contains("mesh-clip := Rectangle"));
        assert!(source.contains("for _[grid-x] in 120"));
        // 11 rows of 8px cells cover the 88px connection module.
        assert!(source.contains("for _[grid-cell-y] in 11"));
        assert!(source.contains("property <float> distance: Math.sqrt("));
        assert!(source.contains("border-width: 0.5px;"));
        assert!(source.contains("signal-mesh-field.active ? Math.max(0, 1 - self.distance) : 0"));
        assert!(source.contains("grid-line-strong"));
        assert!(source.contains("border-color: signal-mesh-field.grid-line-strong;"));
        assert!(source.contains("#8ea39770"));
        assert!(!source.contains("144px * self.reveal"));
        assert!(!source.contains("352px * self.reveal"));
        assert!(!source.contains("connect-halo := Rectangle"));
        assert!(!source.contains("power-cursor-wash := Rectangle"));
        assert!(!source.contains("power-mesh := Rectangle"));
        assert!(!source.contains("button-grid-x"));
        assert!(!source.contains("button-grid-y"));
        assert!(!source.contains("Local signal grid: structural guides"));
        assert!(!source.contains("background: #ffffff05;"));
        assert!(source.contains("subscription-announcement-text"));
        assert!(source.contains("subscription-announcement-tone"));
        for semantic_icon in [
            "announcement-info.svg",
            "announcement-success.svg",
            "announcement-danger.svg",
        ] {
            assert!(source.contains(semantic_icon));
        }
        // The notice is a popover under the title chip, closed by an outside click.
        assert!(!source.contains("announcement-strip"));
        let announcement = source
            .split("announcement-popover := Rectangle {")
            .nth(1)
            .expect("announcement popover source");
        let announcement = announcement.chars().take(2400).collect::<String>();
        assert!(announcement.contains("x: -8px;"));
        assert!(announcement.contains("width: Math.min(420px, parent.width - 16px);"));
        assert!(announcement.contains("wrap: word-wrap;"));
        assert!(announcement.contains("animate opacity { duration: 160ms; easing: ease-out; }"));
        assert!(announcement.contains("accessible-role: none;"));
        assert!(source.contains("clicked => { root.announcement-open = false; }"));
        assert!(source.contains("in property <string> subscription-title:"));
        assert!(source.contains("text: root.subscription-title;"));
        assert!(!source.to_ascii_lowercase().contains("text: \"acmevpn\""));
        assert!(!source.to_ascii_lowercase().contains("text: \"work\""));
        assert!(main.contains("logo_cache::with_logo_cache"));
        assert!(main.contains("cache.retain_paths"));
        assert!(main.contains("cache.image_for_path"));
        assert!(!main.contains("slint::Image::load_from_path"));
        assert!(!logo_cache.contains("slint::Image::load_from_path"));
        assert_eq!(
            logo_cache.matches("crate::service_logo::load").count(),
            1,
            "PNG decoding must have one cache-owned entry point"
        );
        assert!(main.contains("ui.set_has_service_logo(service_logo.is_some());"));
        assert!(main.contains("ui.set_service_logo(service_logo.unwrap_or_default());"));
        let snapshot = main
            .split("struct UiSnapshot")
            .nth(1)
            .and_then(|source| source.split("fn main()").next())
            .expect("UiSnapshot source");
        assert!(!snapshot.contains("slint::Image"));
        assert!(!main.contains("Image::load_from_path(Path::new(\"http"));
        assert!(!source.contains("if root.has-profile: PrimaryAction"));
        let quiet_chip = components
            .split("export component QuietChip")
            .nth(1)
            .and_then(|source| source.split("export component RouteNodeRow").next())
            .expect("QuietChip source");
        // Group chips are equal-width segments of one track.
        assert!(quiet_chip.contains("horizontal-stretch: 1;"));
        assert!(quiet_chip.contains("min-width: 96px;"));
        assert!(!quiet_chip.contains("max-width: 164px;"));
        assert!(quiet_chip.contains("border-radius: 8px;"));
        assert!(quiet_chip.contains("background: root.selected ? Theme.elevated"));
        assert!(quiet_chip.contains("chip-cursor-wash := Rectangle"));
        let route_row = components
            .split("export component RouteNodeRow")
            .nth(1)
            .expect("RouteNodeRow source");
        // iOS-style inset grouped list: one card, inset hairlines, bare flags, a check.
        assert!(route_row.contains("selection-bar := Rectangle"));
        assert!(route_row.contains("row-cursor-wash := Rectangle"));
        assert!(route_row.contains(
            "if root.show-separator: Rectangle {
        x: 56px;"
        ));
        let route_card = source
            .split("route-list-card := Rectangle {")
            .nth(1)
            .and_then(|card| card.split("route-list := VerticalLayout").next())
            .expect("route list card");
        assert!(route_card.contains("border-radius: 14px;"));
        assert!(route_card.contains("clip: true;"));
        assert!(!source.contains("min-width: 132px;"));
        assert!(source.contains("padding: 20px;"));
        assert!(source.contains("for node[index] in root.catalog-nodes: RouteNodeRow"));
        assert!(source.contains("show-separator: index < root.catalog-nodes.length - 1;"));
        assert!(source.contains("height: 48px;"));
        for filter in ["Все", "Система", "Ошибки"] {
            assert!(source.contains(&format!("selected: root.event-filter == \"{filter}\";")));
        }
        assert!(!source.contains("connection-center := Rectangle"));
        assert!(!source.contains("height: 238px;"));

        for required in [
            "preferred-width: 840px;",
            "preferred-height: 720px;",
            "min-width: 700px;",
            "min-height: 620px;",
            "no-frame: true;",
            "title-bar := Rectangle",
            "callback window-drag();",
            "callback window-minimize();",
            "callback window-toggle-maximize();",
            "callback window-close();",
            "width: 44px;",
            "height: 44px;",
            "navigation-rail := Rectangle",
            "main-pane := Rectangle",
            "in-out property <string> local-page: \"home\";",
            "in property <image> icon-source;",
            "source: root.icon-source;",
            "@image-url(\"../assets/home.svg\")",
            "@image-url(\"../assets/status.svg\")",
            "@image-url(\"../assets/settings.svg\")",
            "label: \"Главная\";",
            "label: \"Состояние\";",
            "label: \"Настройки\";",
            "text: \"Маршруты\";",
            "routes-heading := HorizontalLayout",
            "route-updated-meta := Text",
            "root.subscription-updated;",
            "root.open-panel(\"events\")",
            "root.refresh-subscription();",
            "root.primary-action();",
            "root.select-catalog-group(group.id);",
            "root.select-catalog-node(node.id);",
            "first-run-onboarding := VerticalLayout",
            "callback paste-subscription();",
            "callback update-action();",
            "root.update-action();",
            "Скачать обновление и перезапустить клиент?",
        ] {
            assert!(
                source.contains(required),
                "missing desktop control-center contract: {required}"
            );
        }

        for rejected in [
            "preferred-width: 480px;",
            "min-width: 420px;",
            "max-width: 620px;",
            "max-height: 920px;",
            "text: \"ГОТОВО\";",
            "traffic-chart",
            "fake-metric",
            "if root.visible-page == \"routes\"",
            "label: \"Все маршруты\"",
        ] {
            assert!(
                !source
                    .to_ascii_lowercase()
                    .contains(&rejected.to_ascii_lowercase()),
                "mobile or decorative pattern remains: {rejected}"
            );
        }
    }

    #[test]
    #[ignore = "superseded by desktop_control_center_source_contract"]
    fn quiet_signal_review_security_focus_and_intrinsic_layout_contract() {
        let source = include_str!("../ui/app.slint");
        let components = include_str!("../ui/components.slint");
        let main = include_str!("main.rs");

        for required in [
            "title: \"MultiCore\";",
            "in-out property <string> subscription-draft;",
            "in-out property <bool> replace-editor-open: false;",
            "in-out property <int> import-success-serial: 0;",
            "changed import-success-serial",
            "root.subscription-draft = \"\";",
            "root.replace-editor-open = false;",
            "focus-epoch: root.import-success-serial;",
            "focus-on-init: true;",
            "accessible-label: root.has-profile ? \"Новый URL подписки\" : \"URL подписки\";",
            "accessible-live-region: polite;",
            "if !root.has-profile || root.replace-editor-open: Rectangle",
            "preferred-height:",
            "vertical-stretch: 1;",
        ] {
            assert!(
                source.contains(required),
                "missing review contract: {required}"
            );
        }

        for rejected in [
            "title: \"Quiet Signal\";",
            "text: \"Quiet Signal\";",
            "BrandMark",
            "text: \"MultiCore\";",
            "text: \"Локально\";",
            "text: \"Подписка\";",
            "label: \"Заменить\";",
            "init => { self.focus(); }",
            "label: root.replace-editor-open ? \"Скрыть\" : \"Заменить\";",
        ] {
            assert!(
                !source.contains(rejected),
                "rejected review pattern remains: {rejected}"
            );
        }

        assert!(components.contains("changed focus-epoch"));
        let route = components
            .split("export component RouteNodeRow")
            .nth(1)
            .expect("RouteNodeRow source");
        assert!(route.contains("font-size: Theme.type-metadata-size;"));
        assert!(route.contains("text: root.icon;"));

        let import_worker = main
            .split("fn run_import(")
            .nth(1)
            .and_then(|source| source.split("fn run_connect(").next())
            .expect("run_import source");
        assert!(import_worker.contains("let import_succeeded = result.is_ok();"));
        assert!(import_worker.contains("import_succeeded"));
        assert!(main.contains("ui.set_import_success_serial("));
        assert!(!main.contains("set_subscription_draft(url"));

        let disconnect_worker = main
            .split("fn run_disconnect(")
            .nth(1)
            .and_then(|source| source.split("fn run_disconnect_reconciliation(").next())
            .expect("run_disconnect source");
        assert!(disconnect_worker.contains("model.finish_disconnect(mutation, result)"));
        assert!(disconnect_worker.contains("model.begin_disconnect_reconciliation()"));
        assert!(disconnect_worker.contains("run_disconnect_reconciliation("));

        let reconciliation_worker = main
            .split("fn run_disconnect_reconciliation(")
            .nth(1)
            .and_then(|source| source.split("fn apply_snapshot_from_worker(").next())
            .expect("disconnect reconciliation source");
        assert!(reconciliation_worker.contains("let result = client.status();"));
        assert!(
            reconciliation_worker.contains("finish_disconnect_reconciliation(mutation, result)")
        );
    }

    #[test]
    fn disconnecting_is_busy_and_definitive_failure_rolls_back_without_reconciliation() {
        let mut view_model = connected_view_model_with_catalog();
        let previous = view_model.state.clone();

        let (action, mutation) = view_model.begin_primary_action().expect("start disconnect");
        assert_eq!(action, PrimaryAction::Disconnect);
        assert_eq!(
            view_model.state,
            UiState::Disconnecting {
                profile: "Тестовый".into(),
                node: "Первый".into(),
            }
        );

        let pending = view_model.presentation();
        assert_eq!(pending.primary_label, "Отключаем…");
        assert_eq!(pending.primary_action, PrimaryAction::None);
        assert!(!pending.primary_enabled);
        assert!(pending.has_profile);
        assert!(pending.is_connected);
        assert!(pending.is_busy);
        assert!(view_model.begin_primary_action().is_none());
        assert!(
            view_model
                .begin_import("https://subscription.invalid/private")
                .is_none()
        );
        assert!(view_model.begin_node_selection("opaque:node-c").is_none());

        assert!(!view_model.finish_disconnect(
            mutation,
            Err(DaemonError::new(
                "definitive rejection https://private.invalid token=secret"
            ))
        ));
        assert_eq!(view_model.state, previous);
        let rolled_back = view_model.presentation();
        assert_eq!(rolled_back.primary_action, PrimaryAction::Disconnect);
        assert_eq!(
            rolled_back.supporting,
            "Не удалось отключиться. Соединение остаётся активным."
        );
        assert!(!rolled_back.supporting.contains("private"));
        assert!(!rolled_back.supporting.contains("secret"));
    }

    #[test]
    fn ambiguous_disconnect_response_loss_is_degraded_until_authoritative_status() {
        for status in [
            DaemonStatus {
                state: "ready".into(),
                profile: Some("Тестовый".into()),
                current_node: Some("Авто".into()),
                ..DaemonStatus::default()
            },
            DaemonStatus {
                state: "connected".into(),
                profile: Some("Тестовый".into()),
                current_node: Some("Первый".into()),
                ..DaemonStatus::default()
            },
        ] {
            let mut view_model = connected_view_model_with_catalog();
            let (_, disconnect) = view_model.begin_primary_action().expect("start disconnect");
            assert!(view_model.finish_disconnect(
                disconnect,
                Err(DaemonError::ambiguous(
                    "response lost https://private.invalid token=secret"
                ))
            ));

            let uncertain = view_model.presentation();
            assert!(uncertain.is_degraded);
            assert!(!uncertain.is_connected);
            assert!(!uncertain.supporting.contains("private"));
            assert!(!uncertain.supporting.contains("secret"));

            let reconciliation = view_model.begin_disconnect_reconciliation();
            assert!(view_model.presentation().is_degraded);
            assert!(view_model.presentation().is_busy);
            assert!(
                view_model
                    .begin_import("https://subscription.invalid/private")
                    .is_none()
            );
            assert!(
                view_model.finish_disconnect_reconciliation(reconciliation, Ok(status.clone()))
            );
            assert_eq!(
                view_model.presentation().is_connected,
                status.state == "connected"
            );
            assert!(!view_model.presentation().is_degraded);
        }
    }

    #[test]
    fn successful_disconnect_status_is_authoritative_without_reconciliation() {
        let mut view_model = connected_view_model_with_catalog();
        let (_, disconnect) = view_model.begin_primary_action().expect("start disconnect");

        assert!(!view_model.finish_disconnect(
            disconnect,
            Ok(DaemonStatus {
                state: "ready".into(),
                profile: Some("Тестовый".into()),
                current_node: Some("Авто".into()),
                ..DaemonStatus::default()
            })
        ));

        assert_eq!(
            view_model.state,
            UiState::Ready {
                profile: "Тестовый".into(),
                node: "Авто".into(),
            }
        );
        let confirmed = view_model.presentation();
        assert!(!confirmed.is_connected);
        assert!(!confirmed.is_busy);
        assert_eq!(confirmed.primary_action, PrimaryAction::Connect);
    }

    #[test]
    fn failed_or_stale_disconnect_reconciliation_never_claims_connected() {
        let mut failed = connected_view_model_with_catalog();
        let (_, disconnect) = failed.begin_primary_action().expect("start disconnect");
        assert!(failed.finish_disconnect(disconnect, Err(DaemonError::ambiguous("response lost"))));
        let reconciliation = failed.begin_disconnect_reconciliation();
        assert!(failed.finish_disconnect_reconciliation(
            reconciliation,
            Err(DaemonError::ambiguous("private status decode failure"))
        ));
        let presentation = failed.presentation();
        assert!(presentation.is_degraded);
        assert!(!presentation.is_connected);
        assert!(!presentation.supporting.contains("private"));

        let mut stale = connected_view_model_with_catalog();
        let (_, disconnect) = stale.begin_primary_action().expect("start disconnect");
        assert!(stale.finish_disconnect(disconnect, Err(DaemonError::ambiguous("response lost"))));
        let older = stale.begin_disconnect_reconciliation();
        let newer = stale.begin_refresh();
        assert!(!stale.finish_disconnect_reconciliation(
            older,
            Ok(DaemonStatus {
                state: "connected".into(),
                profile: Some("Устаревший".into()),
                current_node: Some("Устаревший".into()),
                ..DaemonStatus::default()
            })
        ));
        assert!(stale.presentation().is_degraded);
        stale.finish_refresh(
            newer,
            Ok(DaemonStatus {
                state: "ready".into(),
                profile: Some("Актуальный".into()),
                current_node: Some("Авто".into()),
                ..DaemonStatus::default()
            }),
        );
        assert_eq!(stale.presentation().profile, "Актуальный");
        assert!(!stale.presentation().is_connected);
    }

    #[test]
    fn quiet_signal_primitives() {
        let theme = include_str!("../ui/theme.slint");
        let components = include_str!("../ui/components.slint");
        let build = include_str!("../build.rs");

        for token in [
            "void: #0d0f12;",
            "canvas: #0d0f12;",
            "surface: #15181d;",
            "elevated: #1c2026;",
            "line: #2a3038;",
            "accent-blue: #4c9dff;",
            "focus: #83bdff;",
            "type-metadata-size: 12px;",
            "type-label-size: 14px;",
            "type-value-size: 16px;",
            "type-title-size: 22px;",
            "radius-small: 8px;",
            "radius-medium: 8px;",
            "radius-large: 12px;",
            "radius-panel: 12px;",
            "interactive-min: 44px;",
            "focus-width: 2px;",
        ] {
            assert!(
                theme.contains(token),
                "missing exact Native Control Center token: {token}"
            );
        }
        assert!(!theme.contains("type-icon-size"));
        let visual_sources = format!("{theme}\n{components}");
        for rejected in ["999px", "blur", "shadow"] {
            assert!(
                !visual_sources.to_ascii_lowercase().contains(rejected),
                "rejected visual effect or radius remains: {rejected}"
            );
        }
        assert!(components.contains("chip-cursor-wash := Rectangle"));
        assert!(components.contains("row-cursor-wash := Rectangle"));
        assert!(components.contains("chip-touch.mouse-x"));
        assert!(components.contains("row-touch.mouse-x"));
        for legacy in ["StatePill", "GlowCard", "NavigationRow", "OverlayPanel"] {
            assert!(
                !components.contains(legacy),
                "legacy component remains: {legacy}"
            );
            assert!(
                !build.contains(legacy),
                "legacy build import remains: {legacy}"
            );
        }
        assert!(!components.contains("std-widgets.slint"));
        assert!(!components.contains(" Button "));

        for unused in ["IconAction", "DetailsSurface"] {
            assert!(!components.contains(&format!("export component {unused}")));
            assert!(!build.contains(unused));
        }
        for component in ["PrimaryAction", "QuietChip", "RouteNodeRow"] {
            assert!(
                components.contains(&format!("export component {component}")),
                "missing Quiet Signal primitive: {component}"
            );
            assert!(
                build.contains(&format!("{component} {{")),
                "build-time component validation must instantiate {component}"
            );
        }

        let component_source = |name: &str, next: Option<&str>| {
            let source = components
                .split(&format!("export component {name}"))
                .nth(1)
                .unwrap_or_else(|| panic!("missing component source for {name}"));
            next.and_then(|next| source.split(&format!("export component {next}")).next())
                .unwrap_or(source)
        };
        for (name, next) in [
            ("PrimaryAction", "QuietChip"),
            ("QuietChip", "RouteNodeRow"),
            ("RouteNodeRow", "RouteNodeRow-end"),
        ] {
            let source = component_source(name, Some(next));
            assert!(
                source.contains("min-height: Theme.interactive-min;")
                    || (name == "RouteNodeRow" && source.contains("min-height: 52px;"))
                    || (name == "QuietChip" && source.contains("min-height: 32px;")),
                "{name}"
            );
            assert!(source.contains("Theme.focus-width"), "{name}");
            assert!(source.contains("event.text == \" \""), "{name}");
            assert!(source.contains("event.text == \"\\n\""), "{name}");
            assert!(source.contains("accessible-role: button;"), "{name}");
            assert!(source.contains("accessible-label:"), "{name}");
        }
        let primary = component_source("PrimaryAction", Some("QuietChip"));
        assert!(primary.contains("font-size: Theme.type-label-size;"));
        assert!(primary.contains("action-touch.pressed"));
        assert!(
            primary.contains("max-height: Theme.interactive-min;"),
            "primary action must not stretch into a giant color block"
        );
        assert!(
            primary.contains("max-width: 184px;"),
            "primary action must have a real width cap, not one a long label can override \
             (regression: a long label like \"Вставить из буфера\" used to blow the button out \
             to fit its own text instead of eliding)"
        );
        assert!(!primary.contains("destructive"));

        let route = component_source("RouteNodeRow", None);
        assert!(route.contains("pending-state-label"));
        assert!(route.contains("if root.pending: Text {"));
        assert!(route.contains("font-size: Theme.type-metadata-size;"));
        assert!(route.contains("min-height: Theme.interactive-min;"));
        assert!(route.contains("preferred-height: 48px;"));
        assert!(route.contains("max-height: 48px;"));
        assert!(
            !route.contains("opacity: root.enabled"),
            "read-only and selected route rows must remain fully legible"
        );
        assert!(route.contains("accessible-enabled: root.enabled;"));
        assert!(route.contains("TouchArea"));
        assert!(route.contains("enabled: root.enabled;"));
        assert!(route.contains("clicked => { root.activated(); }"));
        assert!(!route.contains("parent.width - 136px"));

        let capture_runner = include_str!("../../../scripts/run-preview-capture.ps1");
        assert!(capture_runner.contains("$captureFileNames = @("));
        assert!(capture_runner.contains("Name = \"glow-hover\""));
        assert!(capture_runner.contains("-HoverConnection"));
        assert!(capture_runner.contains("Remove-Item -LiteralPath $captureFile -Force"));
        assert!(!capture_runner.contains("Get-ChildItem -LiteralPath $outputPath -Filter"));
        assert!(capture_runner.contains("$allowedOutputRoot"));
        assert!(capture_runner.contains("StartsWith($allowedOutputPrefix"));

        let capture_helper = include_str!("../../../scripts/capture-preview.ps1");
        assert!(capture_helper.contains("function Test-PreviewFrame"));
        assert!(capture_helper.contains("$requiredStableFrames = 2"));
        assert!(capture_helper.contains("Preview frame validation failed"));
        assert!(!capture_helper.contains("Name = \"product header\""));
        assert!(!capture_helper.contains("Name = \"window title\""));
        assert!(!capture_helper.contains("Name = \"window controls\""));
        assert!(capture_helper.contains(") -ge 160); Name = \"control-center content\""));
        assert!(
            capture_helper.contains("ExpectedState -eq \"selection-pending\"")
                && capture_helper.contains("$Bitmap.Width - 180")
                && capture_helper.contains("$Bitmap.Width - 55")
                && capture_helper
                    .matches("Get-SemanticPixelCountInRegion")
                    .count()
                    >= 3
        );
        let click_helper = capture_helper
            .split("function Click-Preview")
            .nth(1)
            .and_then(|source| source.split("function Test-ColorNear").next())
            .expect("Click-Preview helper");
        assert!(capture_helper.contains("public static void ClickClient"));
        assert!(capture_helper.contains("public static void MoveClient"));
        assert!(capture_helper.contains("if ($HoverConnection)"));
        assert!(capture_helper.contains("Move-Preview $handle 300 112"));
        assert!(capture_helper.contains("PostMessage(window, 0x0201"));
        assert!(!capture_helper.contains("PostMessage(window, 0x02A1"));
        assert!(click_helper.contains("[PreviewWindow]::ClickClient($Handle, $ClientX, $ClientY)"));
        assert!(!capture_helper.contains("mouse_event"));
        assert!(
            capture_helper
                .contains("Click-Preview $handle ([Math]::Floor(($Width + 176) / 2)) 420")
        );
        assert!(
            capture_helper.contains("[System.Drawing.Imaging.PixelFormat]::Format24bppRgb"),
            "captured PNG must be fully opaque instead of inheriting sparse DWM alpha"
        );

        let preview_fixture = include_str!("../../../scripts/preview-fixture.ps1");
        assert!(preview_fixture.contains("delay_ms = 34"));
        assert!(preview_fixture.contains("events = @()"));
        for rejected_fixture_copy in ["Automatic", "Route A", "Local profile"] {
            assert!(
                !preview_fixture.contains(rejected_fixture_copy),
                "preview fixture must use neutral Russian copy without fake telemetry: {rejected_fixture_copy}"
            );
        }
    }

    #[test]
    fn delayed_selection_network_call_does_not_hold_the_model_lock() {
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let client = Arc::new(DelayedSelectionClient {
            started: Mutex::new(Some(started_tx)),
            release: Mutex::new(release_rx),
        });
        let mut view_model = DesktopViewModel::new(client);
        let refresh = view_model.begin_refresh();
        view_model.finish_refresh(
            refresh,
            Ok(DaemonStatus {
                state: "connected".into(),
                profile: Some("Тестовый".into()),
                current_node: Some("Восток".into()),
                ..DaemonStatus::default()
            }),
        );
        let catalog = view_model.begin_catalog_request().expect("catalog request");
        let result = catalog.client.catalog();
        view_model.finish_catalog(catalog.token, result);
        let model = Arc::new(Mutex::new(view_model));
        let request = model
            .lock()
            .expect("view model lock")
            .begin_node_selection("opaque:node-c")
            .expect("selection request");
        let worker_model = model.clone();
        let worker = thread::spawn(move || {
            let result =
                request
                    .client
                    .select_node(&request.group_id, request.revision, &request.node_id);
            worker_model
                .lock()
                .expect("view model lock")
                .finish_node_selection(request.token, result)
        });

        started_rx.recv().expect("selection request started");
        {
            let mut model = model
                .try_lock()
                .expect("model lock must remain available during selection I/O");
            model.open_panel(Panel::Events);
            assert_eq!(model.panel(), Panel::Events);
        }
        release_tx.send(()).expect("release selection request");
        assert!(worker.join().expect("join selection worker").is_some());
    }

    #[test]
    fn selection_success_applies_returned_catalog_and_current_route_label() {
        let mut view_model = connected_view_model_with_catalog();
        let request = view_model
            .begin_node_selection("opaque:node-c")
            .expect("valid selection");
        let mut updated = catalog_fixture();
        updated.groups[1].nodes[0].selected = false;
        updated.groups[1].nodes[1].selected = true;
        updated.groups[1].nodes[1].label = "Новый маршрут".into();

        let outcome = view_model
            .finish_node_selection(request.token, Ok(updated))
            .expect("current selection completion");

        assert!(outcome.refresh_catalog);
        assert_eq!(view_model.presentation().node, "Новый маршрут");
        assert!(!view_model.catalog_presentation().selection_enabled);
        assert!(view_model.catalog_presentation().error.is_none());
    }

    #[test]
    fn selection_response_keeps_the_group_the_user_is_editing() {
        let mut view_model = connected_view_model_with_catalog();
        let request = view_model
            .begin_node_selection("opaque:node-c")
            .expect("selection in the visible group");
        let mut updated = catalog_fixture();
        updated.groups[0].selected = true;
        updated.groups[1].selected = false;
        updated.groups[1].nodes[0].selected = false;
        updated.groups[1].nodes[1].selected = true;

        view_model
            .finish_node_selection(request.token, Ok(updated))
            .expect("current selection completion");

        let presentation = view_model.catalog_presentation();
        assert_eq!(
            presentation.selected_group_id.as_deref(),
            Some("opaque:selected")
        );
        assert!(presentation.nodes[1].selected);
    }

    #[test]
    fn selection_failure_rolls_back_and_stale_completion_is_ignored() {
        let mut view_model = connected_view_model_with_catalog();
        let request = view_model
            .begin_node_selection("opaque:node-c")
            .expect("valid selection");

        let outcome = view_model
            .finish_node_selection(
                request.token,
                Err(DaemonError::new(
                    "unsafe opaque:node-b https://private.invalid token=secret",
                )),
            )
            .expect("current failed completion");

        assert!(!outcome.refresh_catalog);
        let presentation = view_model.catalog_presentation();
        assert!(presentation.nodes[0].selected);
        assert!(!presentation.nodes[1].selected);
        assert_eq!(
            presentation.error.as_deref(),
            Some("Не удалось изменить маршрут. Повторите попытку.")
        );
        assert!(!presentation.error.unwrap().contains("private"));
        assert!(
            view_model
                .finish_node_selection(request.token, Ok(catalog_fixture()))
                .is_none()
        );
    }

    #[test]
    fn catalog_request_is_deferred_without_superseding_an_active_selection() {
        let mut view_model = connected_view_model_with_catalog();
        let selection = view_model
            .begin_node_selection("opaque:node-c")
            .expect("selection");

        assert!(view_model.begin_catalog_request().is_none());
        let pending = view_model.catalog_presentation();
        assert!(pending.selection_pending);
        assert!(pending.nodes[1].selected);

        let mut selected = catalog_fixture();
        selected.groups[1].nodes[0].selected = false;
        selected.groups[1].nodes[1].selected = true;
        let outcome = view_model
            .finish_node_selection(selection.token, Ok(selected))
            .expect("selection completion");
        assert!(outcome.refresh_catalog);
        assert!(!view_model.catalog_presentation().selection_enabled);
        assert!(view_model.begin_catalog_request().is_some());
    }

    #[test]
    fn post_selection_refresh_reconciles_a_delayed_older_catalog_authoritatively() {
        let mut view_model = connected_view_model_with_catalog();
        let delayed_old = view_model
            .begin_catalog_request()
            .expect("delayed pre-selection catalog");
        assert!(view_model.begin_node_selection("opaque:node-c").is_none());
        view_model
            .finish_catalog(delayed_old.token, Ok(catalog_fixture()))
            .expect("older catalog completion");

        let selection = view_model
            .begin_node_selection("opaque:node-c")
            .expect("selection after old GET");
        let mut put_catalog = catalog_fixture();
        put_catalog.groups[1].nodes[0].selected = false;
        put_catalog.groups[1].nodes[1].selected = true;
        put_catalog.groups[1].nodes[1].label = "Ответ выбора".into();
        let outcome = view_model
            .finish_node_selection(selection.token, Ok(put_catalog))
            .expect("PUT completion");
        assert!(outcome.refresh_catalog);

        let authoritative = view_model
            .begin_catalog_request()
            .expect("required authoritative GET");
        let mut final_catalog = catalog_fixture();
        final_catalog.groups[1].nodes[0].selected = false;
        final_catalog.groups[1].nodes[1].selected = true;
        final_catalog.groups[1].nodes[1].label = "Подтверждённый маршрут".into();
        view_model
            .finish_catalog(authoritative.token, Ok(final_catalog))
            .expect("authoritative catalog completion");

        assert!(view_model.catalog_presentation().selection_enabled);
        assert_eq!(view_model.presentation().node, "Подтверждённый маршрут");
    }

    #[test]
    fn stale_revision_rolls_back_invalidates_catalog_and_requests_safe_refresh() {
        let mut view_model = connected_view_model_with_catalog();
        let request = view_model
            .begin_node_selection("opaque:node-c")
            .expect("valid selection");

        let outcome = view_model
            .finish_node_selection(request.token, Err(DaemonError::stale_revision()))
            .expect("current stale completion");

        assert!(outcome.refresh_catalog);
        let stale = view_model.catalog_presentation();
        assert!(stale.nodes[0].selected);
        assert!(!stale.nodes[1].selected);
        assert!(!stale.selection_enabled);
        assert!(stale.revision.is_none());
        assert_eq!(
            stale.error.as_deref(),
            Some("Каталог маршрутов изменился. Обновляем его; повторите выбор.")
        );
    }

    #[test]
    fn ambiguous_selection_failure_rolls_back_then_reconciles_without_losing_safe_error() {
        let mut view_model = connected_view_model_with_catalog();
        let request = view_model
            .begin_node_selection("opaque:node-c")
            .expect("valid selection");

        let outcome = view_model
            .finish_node_selection(
                request.token,
                Err(DaemonError::ambiguous(
                    "unsafe transport detail https://private.invalid token=secret",
                )),
            )
            .expect("ambiguous completion");

        assert!(outcome.refresh_catalog);
        let rolled_back = view_model.catalog_presentation();
        assert!(rolled_back.nodes[0].selected);
        assert!(!rolled_back.nodes[1].selected);
        assert!(!rolled_back.selection_enabled);
        assert_eq!(
            rolled_back.error.as_deref(),
            Some(
                "Не удалось подтвердить изменение маршрута. Каталог обновляется; повторите попытку."
            )
        );

        let refresh = view_model
            .begin_catalog_request()
            .expect("reconciliation catalog request");
        assert_eq!(
            view_model.catalog_presentation().error.as_deref(),
            rolled_back.error.as_deref()
        );
        view_model
            .finish_catalog(refresh.token, Ok(catalog_fixture()))
            .expect("reconciliation completion");
        let reconciled = view_model.catalog_presentation();
        assert!(reconciled.selection_enabled);
        assert_eq!(reconciled.error, rolled_back.error);
    }

    #[test]
    fn selection_and_connection_or_import_mutations_are_gated_in_both_directions() {
        let mut selecting = connected_view_model_with_catalog();
        let selection = selecting
            .begin_node_selection("opaque:node-c")
            .expect("selection");
        assert!(selecting.begin_primary_action().is_none());
        assert!(
            selecting
                .begin_import("https://subscription.invalid/private")
                .is_none()
        );
        assert!(!selecting.catalog_presentation().selection_enabled);
        selecting
            .finish_node_selection(selection.token, Ok(catalog_fixture()))
            .expect("finish selection");

        let mut disconnecting = connected_view_model_with_catalog();
        assert!(disconnecting.begin_primary_action().is_some());
        assert!(
            disconnecting
                .begin_node_selection("opaque:node-c")
                .is_none()
        );
        assert!(!disconnecting.catalog_presentation().selection_enabled);

        let mut importing = connected_view_model_with_catalog();
        assert!(
            importing
                .begin_import("https://subscription.invalid/private")
                .is_some()
        );
        assert!(importing.begin_node_selection("opaque:node-c").is_none());
        assert!(!importing.catalog_presentation().selection_enabled);
    }

    #[test]
    fn subscription_panel_stays_open_and_repeated_import_is_suppressed() {
        let client = Arc::new(MockDaemonClient::ready());
        let mut view_model = DesktopViewModel::new(client.clone());
        refresh(&mut view_model);
        view_model.open_panel(Panel::Subscription);

        assert!(
            view_model
                .begin_import("https://subscription.invalid/private")
                .is_some()
        );
        assert!(view_model.import_pending);
        assert!(
            view_model
                .begin_import("https://subscription.invalid/second")
                .is_none()
        );
        import_subscription(&mut view_model, "https://subscription.invalid/private");

        assert_eq!(view_model.panel, Panel::Subscription);
        assert!(!view_model.import_pending);
        assert_eq!(view_model.import_error, None);
        assert_eq!(
            client.calls.lock().expect("mock call lock").as_slice(),
            [
                "GET /v1/status",
                "POST /v1/subscriptions/import https://subscription.invalid/private",
            ]
        );
    }

    #[test]
    fn import_validation_and_server_failures_are_inline_safe_and_preserve_state() {
        let mut client = MockDaemonClient::ready();
        client.fail_import = true;
        let client = Arc::new(client);
        let mut view_model = DesktopViewModel::new(client);
        refresh(&mut view_model);
        view_model.state = UiState::Connected {
            profile: "Рабочий профиль".into(),
            node: "nl-01".into(),
        };
        let previous_state = view_model.state.clone();
        view_model.open_panel(Panel::Subscription);

        assert!(view_model.begin_import("not a URL").is_none());
        assert_eq!(
            view_model.import_error.as_deref(),
            Some("Введите корректный HTTP(S) URL.")
        );
        assert!(view_model.begin_import("   ").is_none());
        assert_eq!(
            view_model.import_error.as_deref(),
            Some("Введите корректный HTTP(S) URL.")
        );
        assert!(
            view_model
                .begin_import("https://secret.example/path?token=private")
                .is_some()
        );
        import_subscription(&mut view_model, "https://secret.example/path?token=private");

        assert_eq!(view_model.state, previous_state);
        assert_eq!(view_model.panel, Panel::Subscription);
        assert!(!view_model.import_pending);
        assert_eq!(
            view_model.import_error.as_deref(),
            Some("Не удалось добавить подписку. Проверьте URL и повторите попытку.")
        );
        assert!(
            !view_model
                .import_error
                .as_deref()
                .unwrap()
                .contains("secret")
        );
        assert!(
            !view_model
                .import_error
                .as_deref()
                .unwrap()
                .contains("token")
        );
    }

    #[test]
    fn subscription_url_validation_uses_the_shared_persistence_byte_limit() {
        let prefix = "https://example.com/";
        let exact = format!(
            "{prefix}{}",
            "a".repeat(MAX_SUBSCRIPTION_URL_BYTES - prefix.len())
        );
        let over = format!("{exact}a");

        assert_eq!(exact.len(), MAX_SUBSCRIPTION_URL_BYTES);
        assert!(valid_subscription_url(&exact));
        assert!(!valid_subscription_url(&over));
    }

    #[test]
    fn dismiss_import_error_clears_validation_and_server_errors_without_changing_state() {
        let mut validation_model = DesktopViewModel::new(Arc::new(MockDaemonClient::ready()));
        refresh(&mut validation_model);
        validation_model.open_panel(Panel::Subscription);
        assert!(validation_model.begin_import("not a URL").is_none());
        let validation_state = validation_model.state.clone();
        let validation_panel = validation_model.panel;

        validation_model.dismiss_import_error();

        assert_eq!(validation_model.import_error, None);
        assert_eq!(validation_model.state, validation_state);
        assert_eq!(validation_model.panel, validation_panel);
        assert!(!validation_model.import_pending);

        let mut client = MockDaemonClient::ready();
        client.fail_import = true;
        let mut server_model = DesktopViewModel::new(Arc::new(client));
        refresh(&mut server_model);
        import_subscription(
            &mut server_model,
            "https://subscription.invalid/server-error",
        );
        assert!(server_model.import_error.is_some());
        let server_state = server_model.state.clone();
        let server_panel = server_model.panel;

        server_model.dismiss_import_error();

        assert_eq!(server_model.import_error, None);
        assert_eq!(server_model.state, server_state);
        assert_eq!(server_model.panel, server_panel);
        assert!(!server_model.import_pending);
    }

    #[test]
    fn external_import_error_survives_unrelated_presentation_until_dismissed() {
        let mut model = DesktopViewModel::new(Arc::new(MockDaemonClient::ready()));
        let state = model.state.clone();

        model.set_import_error_message("Не удалось прочитать буфер обмена");
        let _ = model.presentation();

        assert_eq!(
            model.import_error(),
            Some("Не удалось прочитать буфер обмена")
        );
        assert_eq!(model.state, state);

        model.dismiss_import_error();
        assert_eq!(model.import_error(), None);
    }

    #[test]
    fn presentation_exposes_exact_status_tone_and_inline_profile_mode() {
        let cases = [
            (
                UiState::Empty,
                "Профиль не добавлен",
                SemanticTone::Neutral,
                false,
            ),
            (UiState::Importing, "Импорт", SemanticTone::Warning, false),
            (
                UiState::Ready {
                    profile: "Работа".into(),
                    node: "Авто".into(),
                },
                "Готово",
                SemanticTone::Accent,
                true,
            ),
            (
                UiState::Connecting {
                    profile: "Работа".into(),
                    node: "Авто".into(),
                    step: "Запуск".into(),
                },
                "Подключение",
                SemanticTone::Warning,
                true,
            ),
            (
                UiState::Connected {
                    profile: "Работа".into(),
                    node: "nl-01".into(),
                },
                "Подключено",
                SemanticTone::Success,
                true,
            ),
            (
                UiState::Error {
                    message: "Ошибка".into(),
                },
                "Не удалось подключиться",
                SemanticTone::Danger,
                true,
            ),
            (
                UiState::ServiceUnreachable {
                    message: "Нет ответа".into(),
                },
                "Нет связи с сервисом",
                SemanticTone::Danger,
                true,
            ),
            (
                UiState::DaemonDegraded {
                    message: "Сервис".into(),
                },
                "Сервис недоступен",
                SemanticTone::Warning,
                true,
            ),
        ];

        for (state, label, tone, has_profile) in cases {
            let presentation = state.presentation();
            assert_eq!(presentation.eyebrow, label);
            assert_eq!(presentation.semantic_tone, tone);
            assert_eq!(presentation.has_profile, has_profile);
        }
    }

    #[test]
    fn events_advance_the_monotonic_cursor_and_do_not_replay_cached_ids() {
        let mut client = MockDaemonClient::ready();
        client.events = vec![
            DaemonEvent {
                id: 7,
                timestamp: "1".into(),
                level: "info".into(),
                component: String::new(),
                message: "Started".into(),
                details: None,
            },
            DaemonEvent {
                id: 9,
                timestamp: "2".into(),
                level: "warning".into(),
                component: String::new(),
                message: "Updated".into(),
                details: None,
            },
        ];
        let client = Arc::new(client);
        let mut view_model = DesktopViewModel::new(client.clone());

        let first = events(&mut view_model, None);
        let second = events(&mut view_model, None);

        assert_eq!(
            first.iter().map(|event| event.id).collect::<Vec<_>>(),
            [7, 9]
        );
        assert_eq!(
            second.iter().map(|event| event.id).collect::<Vec<_>>(),
            [7, 9]
        );
        assert_eq!(
            client.calls.lock().expect("mock call lock").as_slice(),
            [
                "GET /v1/events after=0",
                "GET /v1/events after=9 epoch=test-epoch"
            ]
        );
    }

    #[test]
    fn maps_each_state_to_one_contextual_action_and_copy() {
        let cases = [
            (
                UiState::Empty,
                "Добавьте подписку",
                PrimaryAction::None,
                "Подключиться",
            ),
            (
                UiState::Importing,
                "Добавляем профиль…",
                PrimaryAction::None,
                "Добавляем…",
            ),
            (
                UiState::Ready {
                    profile: "Работа".into(),
                    node: "Авто".into(),
                },
                "Можно подключаться",
                PrimaryAction::Connect,
                "Подключиться",
            ),
            (
                UiState::Connecting {
                    profile: "Работа".into(),
                    node: "Авто".into(),
                    step: "Проверяем маршрут…".into(),
                },
                "Устанавливаем соединение…",
                PrimaryAction::None,
                "Подключаем…",
            ),
            (
                UiState::Connected {
                    profile: "Работа".into(),
                    node: "nl-01".into(),
                },
                "Соединение защищено",
                PrimaryAction::Disconnect,
                "Отключиться",
            ),
            (
                UiState::Error {
                    message: "Нет сети".into(),
                },
                "Что-то пошло не так",
                PrimaryAction::Retry,
                "Проверить снова",
            ),
            (
                UiState::ServiceUnreachable {
                    message: "Нет ответа".into(),
                },
                "Сервис не отвечает",
                PrimaryAction::Retry,
                "Проверить снова",
            ),
            (
                UiState::DaemonDegraded {
                    message: "Сервис запускается".into(),
                },
                "Ограниченный режим",
                PrimaryAction::Cleanup,
                "Остановить туннель",
            ),
        ];

        for (state, headline, action, label) in cases {
            let presentation = state.presentation();
            assert_eq!(presentation.headline, headline);
            assert_eq!(presentation.primary_action, action);
            assert_eq!(presentation.primary_label, label);
        }
    }

    #[test]
    fn connected_state_exposes_current_route_line() {
        let presentation = UiState::Connected {
            profile: "Основной".into(),
            node: "Stockholm 02".into(),
        }
        .presentation();

        assert_eq!(presentation.supporting, "Текущий маршрут");
        assert!(presentation.is_connected);
    }

    #[test]
    fn event_presentation_redacts_secrets_and_subscription_urls() {
        let event = DaemonEvent {
            id: 1,
            timestamp: "12:04:07".into(),
            level: "info".into(),
            component: String::new(),
            message: "import url=https://user:pass@example.test/sub?token=abc token=xyz password: hunter2"
                .into(),
            details: None,
        };

        let presented = PresentedEvent::from(event);

        assert_eq!(
            presented.message,
            "import url=[REDACTED] token=[REDACTED] password:[REDACTED]"
        );
        assert!(!presented.message.contains("hunter2"));
        assert!(!presented.message.contains("example.test"));
    }

    #[test]
    fn event_presentation_redacts_bare_urls_authorization_and_uuid() {
        let redacted = redact_event_text(
            "Authorization: Bearer top-secret proxy https://alice:pass@edge.example/path?id=550e8400-e29b-41d4-a716-446655440000 uuid=550e8400-e29b-41d4-a716-446655440000",
        );

        assert_eq!(redacted, "Authorization: [REDACTED]");
        assert!(!redacted.contains("top-secret"));
        assert!(!redacted.contains("edge.example"));
        assert!(!redacted.contains("550e8400"));
    }

    #[test]
    fn event_presentation_redacts_entire_authorization_values_for_any_scheme() {
        let redacted = redact_event_text(
            "Authorization: Basic dXNlcjpzZWNyZXQ=\nProxy-Authorization: Negotiate private-ticket",
        );

        assert_eq!(
            redacted,
            "Authorization: [REDACTED]\nProxy-Authorization: [REDACTED]"
        );
        assert!(!redacted.contains("Basic"));
        assert!(!redacted.contains("Negotiate"));
        assert!(!redacted.contains("private-ticket"));
    }

    #[test]
    fn view_model_uses_daemon_contract_and_maps_its_dto() {
        let client = Arc::new(MockDaemonClient::ready());
        let mut view_model = DesktopViewModel::new(client.clone());

        refresh(&mut view_model);
        import_subscription(&mut view_model, "https://subscription.invalid/private");
        connect(&mut view_model);
        disconnect(&mut view_model);
        let _ = events(&mut view_model, Some("error"));

        assert_eq!(
            client.calls.lock().expect("mock call lock").as_slice(),
            [
                "GET /v1/status",
                "POST /v1/subscriptions/import https://subscription.invalid/private",
                "POST /v1/connect",
                "POST /v1/disconnect",
                "GET /v1/events after=0",
            ]
        );
        assert_eq!(
            view_model.state,
            UiState::Ready {
                profile: "Тестовый".into(),
                node: "Авто".into(),
            }
        );
    }

    #[test]
    fn events_failure_is_presented_safely_instead_of_as_an_empty_list() {
        let mut view_model = DesktopViewModel::new(Arc::new(MockDaemonClient::ready()));
        let initial = view_model
            .begin_events_request()
            .expect("initial event request");
        view_model
            .finish_events(
                initial.token,
                Ok(DaemonEventBatch {
                    epoch: "stable-epoch".into(),
                    events: vec![DaemonEvent {
                        id: 7,
                        timestamp: "now".into(),
                        level: "info".into(),
                        component: String::new(),
                        message: "cached".into(),
                        details: None,
                    }],
                }),
            )
            .expect("successful initial batch");
        let before = (
            view_model.event_epoch.clone(),
            view_model.event_cursor,
            view_model.event_cache.clone(),
        );

        let failed = view_model
            .begin_events_request()
            .expect("failing event request");
        let events = view_model
            .finish_events(
                failed.token,
                Err(DaemonError::new("unsafe upstream details")),
            )
            .expect("current failed request");

        assert_eq!(events.len(), 2);
        assert_eq!(events[0].level, "error");
        assert_eq!(
            events[0].message,
            "Не удалось загрузить события. Повторите попытку."
        );
        assert!(!events[0].message.contains("upstream"));
        assert_eq!(events[1].message, "cached");
        assert_eq!(
            (
                view_model.event_epoch.clone(),
                view_model.event_cursor,
                view_model.event_cache.clone(),
            ),
            before,
            "a failed request must not mutate epoch, cursor, or cached events"
        );

        let after_filter_change = view_model.set_event_filter(Some("system"));
        assert_eq!(after_filter_change, vec![events[1].clone()]);

        let retry = view_model
            .begin_events_request()
            .expect("retry event request");
        let recovered = view_model
            .finish_events(
                retry.token,
                Ok(DaemonEventBatch {
                    epoch: "stable-epoch".into(),
                    events: Vec::new(),
                }),
            )
            .expect("successful retry");
        assert_eq!(recovered.len(), 1);
        assert_eq!(recovered[0].message, "cached");
    }

    #[test]
    fn system_filter_uses_real_daemon_severity_levels() {
        let mut client = MockDaemonClient::ready();
        client.events = vec![
            DaemonEvent {
                id: 1,
                timestamp: "1".into(),
                level: "info".into(),
                component: String::new(),
                message: "Started".into(),
                details: None,
            },
            DaemonEvent {
                id: 2,
                timestamp: "2".into(),
                level: "warning".into(),
                component: String::new(),
                message: "Degraded".into(),
                details: None,
            },
            DaemonEvent {
                id: 3,
                timestamp: "3".into(),
                level: "error".into(),
                component: String::new(),
                message: "Failed".into(),
                details: None,
            },
        ];
        let mut view_model = DesktopViewModel::new(Arc::new(client));

        let system = events(&mut view_model, Some("system"));
        let errors = events(&mut view_model, Some("error"));

        assert_eq!(
            system
                .iter()
                .map(|event| event.level.as_str())
                .collect::<Vec<_>>(),
            ["info", "warning"]
        );
        assert_eq!(
            errors
                .iter()
                .map(|event| event.level.as_str())
                .collect::<Vec<_>>(),
            ["error"]
        );
    }

    #[test]
    fn diagnostics_present_owned_health_and_keep_logs_when_refresh_fails() {
        let mut view_model = DesktopViewModel::new(Arc::new(MockDaemonClient::ready()));
        let request = view_model
            .begin_diagnostics_request()
            .expect("start diagnostics request");
        assert!(view_model.diagnostics_presentation().loading);

        let feed = view_model
            .finish_diagnostics(
                request.token,
                Ok(DaemonDiagnostics {
                    xray: "ready".into(),
                    mihomo: "failed".into(),
                    tun: "starting".into(),
                    logs: vec![DaemonDiagnosticLog {
                        id: 7,
                        timestamp: "11".into(),
                        component: "mihomo".into(),
                        level: "error".into(),
                        message: "Authorization: Bearer private-secret".into(),
                    }],
                    mappings: vec![DaemonProxyMapping {
                        proxy_name: "Sweden [se]".into(),
                        address: "127.0.0.1:31006".into(),
                        xray_label: "Sweden [se]".into(),
                    }],
                }),
            )
            .expect("current diagnostics completion");
        let presentation = view_model.diagnostics_presentation();
        assert_eq!(presentation.xray.value, "Готов");
        assert_eq!(presentation.xray.tone, "success");
        assert_eq!(presentation.mihomo.value, "Сбой");
        assert_eq!(presentation.mihomo.tone, "danger");
        assert_eq!(presentation.tun.value, "Запуск");
        assert_eq!(presentation.mappings.len(), 1);
        assert_eq!(presentation.mappings[0].proxy_name, "Sweden [se]");
        assert_eq!(presentation.mappings[0].address, "127.0.0.1:31006");
        assert_eq!(presentation.mappings[0].xray_label, "Sweden [se]");
        assert!(feed[0].message.contains("[REDACTED]"));
        assert!(!feed[0].message.contains("private-secret"));

        let retry = view_model
            .begin_diagnostics_request()
            .expect("start diagnostics retry");
        let retained = view_model
            .finish_diagnostics(retry.token, Err(DaemonError::new("upstream secret")))
            .expect("current diagnostics failure");
        assert!(
            retained
                .iter()
                .any(|event| event.message.contains("Не удалось"))
        );
        assert!(retained.iter().any(|event| event.id == 7));
        assert!(!format!("{retained:?}").contains("upstream secret"));
    }

    #[test]
    fn cores_filter_isolates_xray_and_mihomo_log_lines_from_other_events() {
        let mut view_model = DesktopViewModel::new(Arc::new(MockDaemonClient::ready()));
        let request = view_model
            .begin_diagnostics_request()
            .expect("start diagnostics request");
        view_model
            .finish_diagnostics(
                request.token,
                Ok(DaemonDiagnostics {
                    xray: "ready".into(),
                    mihomo: "ready".into(),
                    tun: "ready".into(),
                    logs: vec![
                        DaemonDiagnosticLog {
                            id: 1,
                            timestamp: "1".into(),
                            component: "xray".into(),
                            level: "info".into(),
                            message: "xray started".into(),
                        },
                        DaemonDiagnosticLog {
                            id: 2,
                            timestamp: "2".into(),
                            component: "mihomo".into(),
                            level: "info".into(),
                            message: "mihomo started".into(),
                        },
                        DaemonDiagnosticLog {
                            id: 3,
                            timestamp: "3".into(),
                            component: "unlabeled".into(),
                            level: "info".into(),
                            message: "unlabeled core line".into(),
                        },
                    ],
                    mappings: Vec::new(),
                }),
            )
            .expect("current diagnostics completion");

        let events = view_model.set_event_filter(Some("cores"));
        assert_eq!(
            events.iter().map(|event| event.id).collect::<Vec<_>>(),
            [1, 2, 3]
        );
        assert!(
            events
                .iter()
                .all(|event| matches!(event.component.as_str(), "xray" | "mihomo" | "core"))
        );

        // An unrelated app-level event must not appear under "Ядра".
        view_model.event_cache.push(PresentedEvent {
            id: 99,
            timestamp: "99".into(),
            component: "subscription".into(),
            level: "info".into(),
            message: "profile imported".into(),
        });
        let events = view_model.set_event_filter(Some("cores"));
        assert!(!events.iter().any(|event| event.id == 99));
    }

    #[test]
    #[ignore = "superseded by desktop_control_center_source_contract"]
    fn diagnostics_surface_has_compact_owned_health_dashboard_and_log_feed() {
        let source = include_str!("../ui/app.slint");
        for required in [
            "diagnostics-xray-value",
            "diagnostics-mihomo-value",
            "diagnostics-tun-value",
            "diagnostics-loading",
            "event.component",
            "Диагностика Xray",
            "Диагностика Mihomo",
            "Диагностика TUN MultiCore",
        ] {
            assert!(source.contains(required), "missing {required}");
        }
        assert!(source.contains("height: 76px"));
        assert!(!source.contains("Диагностика FlClashX"));
    }

    #[test]
    fn conflicting_apps_banner_is_hidden_with_no_detections() {
        let model = DesktopViewModel::new(Arc::new(MockDaemonClient::ready()));
        let presentation = model.conflicting_apps_presentation();
        assert!(!presentation.visible);
        assert!(presentation.apps.is_empty());
    }

    #[test]
    fn conflicting_apps_banner_shows_one_detection() {
        let mut model = DesktopViewModel::new(Arc::new(MockDaemonClient::ready()));
        model.set_conflicting_apps(vec!["FlClash"]);
        let presentation = model.conflicting_apps_presentation();
        assert!(presentation.visible);
        assert_eq!(presentation.apps, vec!["FlClash".to_string()]);
        assert!(presentation.message.contains("FlClash"));
        assert!(presentation.message.contains("оно может конфликтовать"));
    }

    #[test]
    fn conflicting_apps_banner_lists_multiple_simultaneous_detections() {
        let mut model = DesktopViewModel::new(Arc::new(MockDaemonClient::ready()));
        model.set_conflicting_apps(vec!["Koala Clash", "FlClash", "Incy"]);
        let presentation = model.conflicting_apps_presentation();
        assert!(presentation.visible);
        // Sorted and deduplicated.
        assert_eq!(
            presentation.apps,
            vec![
                "FlClash".to_string(),
                "Incy".to_string(),
                "Koala Clash".to_string(),
            ]
        );
        assert!(presentation.message.contains("они могут конфликтовать"));
        assert!(presentation.message.contains("FlClash"));
        assert!(presentation.message.contains("Incy"));
        assert!(presentation.message.contains("Koala Clash"));
    }

    #[test]
    fn conflicting_apps_banner_deduplicates_repeated_detections() {
        let mut model = DesktopViewModel::new(Arc::new(MockDaemonClient::ready()));
        model.set_conflicting_apps(vec!["FlClash", "FlClash"]);
        assert_eq!(
            model.conflicting_apps_presentation().apps,
            vec!["FlClash".to_string()]
        );
    }

    #[test]
    fn record_close_attempt_notes_an_app_that_survives_the_close() {
        let mut model = DesktopViewModel::new(Arc::new(MockDaemonClient::ready()));
        model.set_conflicting_apps(vec!["FlClash"]);
        assert!(
            !model
                .conflicting_apps_presentation()
                .message
                .contains("службу Windows")
        );

        // Closing found nothing this token could terminate (e.g. only a
        // Session-0 service is left), so the rescan still detects it.
        model.record_close_attempt(&["FlClash"]);
        let message = model.conflicting_apps_presentation().message;
        assert!(message.contains("FlClash"));
        assert!(message.contains("службу Windows"));
    }

    #[test]
    fn record_close_attempt_adds_no_note_when_the_app_is_gone() {
        let mut model = DesktopViewModel::new(Arc::new(MockDaemonClient::ready()));
        model.set_conflicting_apps(vec!["FlClash"]);
        model.record_close_attempt(&["FlClash"]);
        assert!(
            model
                .conflicting_apps_presentation()
                .message
                .contains("службу Windows")
        );

        // The next scan (run before record_close_attempt, matching the real
        // main.rs call order) no longer detects it at all.
        model.set_conflicting_apps(vec![]);
        assert!(!model.conflicting_apps_presentation().visible);
    }

    #[test]
    fn close_note_for_one_app_does_not_leak_onto_a_different_later_detection() {
        let mut model = DesktopViewModel::new(Arc::new(MockDaemonClient::ready()));
        model.set_conflicting_apps(vec!["FlClash"]);
        model.record_close_attempt(&["FlClash"]);

        // FlClash's service finally exits, but a different app now shows up.
        model.set_conflicting_apps(vec!["Incy"]);
        let message = model.conflicting_apps_presentation().message;
        assert!(message.contains("Incy"));
        assert!(!message.contains("FlClash"));
        assert!(!message.contains("службу Windows"));
    }

    #[test]
    fn dismissing_conflicting_apps_banner_hides_it_for_the_same_detection() {
        let mut model = DesktopViewModel::new(Arc::new(MockDaemonClient::ready()));
        model.set_conflicting_apps(vec!["FlClash"]);
        assert!(model.conflicting_apps_presentation().visible);

        model.dismiss_conflicting_apps_banner();
        assert!(!model.conflicting_apps_presentation().visible);

        // Re-detecting the exact same set on a later scan stays dismissed.
        model.set_conflicting_apps(vec!["FlClash"]);
        assert!(!model.conflicting_apps_presentation().visible);
    }

    #[test]
    fn dismissed_banner_reappears_for_a_newly_detected_different_app() {
        let mut model = DesktopViewModel::new(Arc::new(MockDaemonClient::ready()));
        model.set_conflicting_apps(vec!["FlClash"]);
        model.dismiss_conflicting_apps_banner();
        assert!(!model.conflicting_apps_presentation().visible);

        // A different app joins the detected set: the banner reappears even
        // though FlClash is still present, because the set changed.
        model.set_conflicting_apps(vec!["FlClash", "Incy"]);
        assert!(model.conflicting_apps_presentation().visible);
    }

    #[test]
    fn conflicting_apps_clearing_resets_dismissal_so_a_relaunch_warns_again() {
        let mut model = DesktopViewModel::new(Arc::new(MockDaemonClient::ready()));
        model.set_conflicting_apps(vec!["FlClash"]);
        model.dismiss_conflicting_apps_banner();
        assert!(!model.conflicting_apps_presentation().visible);

        // The app closes: the scan reports nothing running anymore.
        model.set_conflicting_apps(vec![]);
        assert!(!model.conflicting_apps_presentation().visible);

        // The same app is reopened later: since the conflict fully cleared,
        // dismissal was reset and the banner warns again.
        model.set_conflicting_apps(vec!["FlClash"]);
        assert!(model.conflicting_apps_presentation().visible);
    }

    #[test]
    fn conflicting_apps_banner_is_never_shown_while_connected() {
        let mut model = DesktopViewModel::new(Arc::new(MockDaemonClient::ready()));
        model.set_conflicting_apps(vec!["FlClash"]);
        model.state = UiState::Connected {
            profile: "Profile".into(),
            node: "Node".into(),
        };
        let presentation = model.conflicting_apps_presentation();
        assert!(!presentation.visible);
        assert!(presentation.apps.is_empty());
    }
}
