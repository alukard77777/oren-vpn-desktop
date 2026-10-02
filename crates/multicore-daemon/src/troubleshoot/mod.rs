//! «Устранить неполадки»: what the daemon detects on the user's machine and what it may fix.
//!
//! Detection and the user-level actions (the stale or foreign system proxy, the DNS cache,
//! Drover's files in the user's Discord folders) run here, as the logged-in user. Privileged
//! fixes are only ever the fixed [`PrivilegedFix`] variants: sent to the always-ready service
//! (`ServiceCommand::Troubleshoot`) or, without it (or for a standard user, or for the
//! service's own re-registration), to `multicore-core-host --troubleshoot` behind a UAC prompt.
//!
//! The decision functions below turn detected facts into result cards and fix offers and are
//! pure, so every message and every offer is covered by tests.

pub mod clock;
pub mod hosts;
#[cfg(windows)]
pub mod windows;

use std::collections::BTreeSet;

use multicore_core::elevation_protocol::BrokerErrorCode;
use multicore_core::troubleshoot::{
    CheckStep, DpiService, Finding, FixId, FixOffer, FixOutcome, FixReport, ListenPort,
    PrivilegedFix, StepReport, Tone,
};

use crate::service_client::ServiceError;

/// The most fix ids one `POST /v1/troubleshoot/fix` may carry.
pub const MAX_FIX_REQUEST: usize = 12;

/// Router-facing troubleshooting (production: [`windows::SystemTroubleshooting`]).
#[async_trait::async_trait]
pub trait Troubleshooting: Send + Sync + 'static {
    /// Runs one check. The proxy and DNS steps also apply their automatic, user-level fixes.
    async fn check(&self, step: CheckStep, ports: Vec<ListenPort>) -> StepReport;
    /// Runs confirmed fixes; privileged ones go through the service or a UAC prompt.
    async fn fix(&self, fixes: Vec<FixId>) -> FixReport;
}

/// Parses the fix ids of a request: known ids only, deduplicated, bounded.
pub fn parse_fix_request(ids: &[String]) -> Option<Vec<FixId>> {
    if ids.is_empty() || ids.len() > MAX_FIX_REQUEST {
        return None;
    }
    let fixes = ids
        .iter()
        .map(|id| FixId::parse(id))
        .collect::<Option<BTreeSet<_>>>()?;
    let fixes = fixes.into_iter().collect::<Vec<_>>();
    // The service re-registration restarts the service: it is its own action.
    if fixes.len() > 1 && fixes.contains(&FixId::Privileged(PrivilegedFix::ReinstallService)) {
        return None;
    }
    Some(fixes)
}

// ---------------------------------------------------------------------------------------------
// Detected facts and what they mean.
// ---------------------------------------------------------------------------------------------

/// A known antivirus whose service is running.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct Antivirus {
    pub name: &'static str,
    /// Known to break VPNs outright (Kaspersky, Dr.Web): an error, not a warning.
    pub severe: bool,
}

/// Running service names that identify an antivirus. Kaspersky's service carries its version
/// (`AVP21.3`), so it matches by prefix; everything else by exact name (case-insensitive).
pub fn classify_antivirus<'a>(service_names: impl IntoIterator<Item = &'a str>) -> Vec<Antivirus> {
    const EXACT: &[(&str, &str, bool)] = &[
        ("kavfs", "Kaspersky", true),
        ("klnagent", "Kaspersky", true),
        ("drwebservice", "Dr.Web", true),
        ("spider", "Dr.Web", true),
        ("drwebcom", "Dr.Web", true),
        ("avast! antivirus", "Avast", false),
        ("bdservicehost", "Bitdefender", false),
        ("ekrn", "ESET", false),
        ("mbamservice", "Malwarebytes", false),
        ("mcapexe", "McAfee", false),
        ("nortonsecurity", "Norton", false),
        ("sophoshealth", "Sophos", false),
        ("wrsvc", "Webroot", false),
    ];
    let mut found = BTreeSet::new();
    for name in service_names {
        let lower = name.to_ascii_lowercase();
        let kaspersky = lower == "avp"
            || lower
                .strip_prefix("avp")
                .is_some_and(|version| version.starts_with(|c: char| c.is_ascii_digit()));
        if kaspersky {
            found.insert(("Kaspersky", true));
        } else if let Some((_, product, severe)) =
            EXACT.iter().find(|(service, _, _)| *service == lower)
        {
            found.insert((*product, *severe));
        }
    }
    let mut antivirus = found
        .into_iter()
        .map(|(name, severe)| Antivirus { name, severe })
        .collect::<Vec<_>>();
    // Errors first.
    antivirus.sort_by_key(|item| (!item.severe, item.name));
    antivirus
}

pub fn antivirus_findings(antivirus: &[Antivirus]) -> Vec<Finding> {
    antivirus
        .iter()
        .map(|item| {
            if item.severe {
                Finding::new(
                    Tone::Error,
                    format!("Обнаружен {}", item.name),
                    format!(
                        "{} часто блокирует VPN. Если подключение не работает — выключите его \
                         или добавьте MULTICORE в исключения.",
                        item.name
                    ),
                )
            } else {
                Finding::new(
                    Tone::Warning,
                    format!("Обнаружен {}", item.name),
                    "Антивирус может мешать VPN. Если подключение не работает — добавьте \
                     MULTICORE в исключения или временно отключите защиту.",
                )
            }
        })
        .collect()
}

/// An allowlisted packet-filter service that exists on this machine.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DpiServiceState {
    pub service: DpiService,
    pub running: bool,
}

pub fn dpi_report(services: &[DpiServiceState], folders: &[String]) -> StepReport {
    let mut report = StepReport::default();
    for state in services {
        let name = state.service.display_name();
        let driver = matches!(
            state.service,
            DpiService::WinDivert | DpiService::WinDivert14
        );
        let what = if driver {
            "Драйвер перехвата пакетов (его используют zapret и GoodbyeDPI)"
        } else {
            "Программа обхода блокировок"
        };
        report.findings.push(if state.running {
            Finding::new(
                Tone::Error,
                format!("Работает {name}"),
                format!("{what} перехватывает трафик и мешает VPN."),
            )
        } else {
            Finding::new(
                Tone::Warning,
                format!("Установлена служба {name}"),
                format!("{what}: служба остановлена, но может запуститься снова."),
            )
        });
        report.fixes.push(FixOffer {
            id: FixId::Privileged(PrivilegedFix::RemoveDpiService {
                service: state.service,
            })
            .as_text(),
            title: format!("Остановить и удалить службу {name}"),
            detail: "Служба будет остановлена и удалена из Windows. Файлы программы \
                     останутся — при необходимости её можно установить снова."
                .into(),
            needs_admin: true,
            needs_reboot: false,
        });
    }
    if !folders.is_empty() {
        report.findings.push(Finding::new(
            Tone::Warning,
            "Найдены программы обхода блокировок",
            format!(
                "{}. Если они запущены — закройте их. MULTICORE эти папки не трогает.",
                folders.join(", ")
            ),
        ));
    }
    report
}

/// A listener on one of the subscription's ports.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PortOwner {
    pub port: u16,
    pub udp: bool,
    pub process_name: Option<String>,
    /// The image path, when this user may read it.
    pub image: Option<String>,
}

/// Image names of MULTICORE's own processes.
const OWN_IMAGES: [&str; 4] = [
    "mihomo.exe",
    "xray.exe",
    "multicore-core-host.exe",
    "multicore-daemon.exe",
];

/// Whether a listener belongs to MULTICORE: an image inside our package, or one of our image
/// names whose path this user cannot read (the service's cores run as SYSTEM).
pub fn is_own_listener(owner: &PortOwner, package_root: Option<&str>) -> bool {
    let normalize = |text: &str| text.replace('/', "\\").to_lowercase();
    match (&owner.image, package_root) {
        (Some(image), Some(root)) => {
            let root = normalize(root);
            let root = root.trim_end_matches('\\');
            normalize(image).starts_with(&format!("{root}\\"))
        }
        (Some(_), None) => false,
        (None, _) => owner
            .process_name
            .as_deref()
            .is_some_and(|name| OWN_IMAGES.contains(&name.to_lowercase().as_str())),
    }
}

/// Cards for the subscription's ports held by another program.
pub fn port_findings(
    wanted: &[ListenPort],
    owners: &[PortOwner],
    package_root: Option<&str>,
) -> Vec<Finding> {
    let mut seen = BTreeSet::new();
    let mut findings = Vec::new();
    for owner in owners {
        if !wanted
            .iter()
            .any(|port| port.port == owner.port && port.udp == owner.udp)
            || is_own_listener(owner, package_root)
            || !seen.insert((owner.port, owner.udp))
        {
            continue;
        }
        let process = owner
            .process_name
            .as_deref()
            .map_or_else(|| "другая программа".to_owned(), |name| format!("«{name}»"));
        findings.push(Finding::new(
            Tone::Warning,
            format!(
                "Порт {}{} занят",
                owner.port,
                if owner.udp { " (UDP)" } else { "" }
            ),
            format!(
                "Его занимает {process}, а он нужен подписке. Закройте эту программу и \
                 повторите подключение."
            ),
        ));
    }
    findings
}

/// The user's WinINet proxy as found, and what the check did about it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProxyOutcome {
    Direct,
    /// MULTICORE's own proxy outlived its connection (a crash): the user's earlier settings
    /// were put back, or the leftover was switched off.
    StaleOursCleared,
    /// Another program's proxy was switched off (its address is kept for switching it back on).
    ForeignDisabled {
        server: String,
    },
    DisableFailed {
        server: String,
    },
}

/// Whether `server` is the loopback proxy MULTICORE applies (`127.0.0.1:<port>`).
pub fn looks_like_our_proxy(server: &str, proxy_override: Option<&str>) -> bool {
    let ours_server = server
        .strip_prefix("127.0.0.1:")
        .is_some_and(|port| port.parse::<u16>().is_ok());
    ours_server && proxy_override == Some(crate::system_proxy::PROXY_OVERRIDE)
}

pub fn proxy_findings(outcome: &ProxyOutcome, pac_host: Option<&str>) -> Vec<Finding> {
    let mut findings = match outcome {
        ProxyOutcome::Direct => Vec::new(),
        ProxyOutcome::StaleOursCleared => vec![Finding::new(
            Tone::Info,
            "Убран прокси от прошлого подключения",
            "MULTICORE не успел выключить системный прокси. Прежние настройки восстановлены.",
        )],
        ProxyOutcome::ForeignDisabled { server } => vec![Finding::new(
            Tone::Info,
            "Системный прокси отключён",
            format!(
                "Был включён прокси {server} — он мешает подключению. Включить снова: \
                 Параметры → Сеть и Интернет → Прокси."
            ),
        )],
        ProxyOutcome::DisableFailed { server } => vec![Finding::new(
            Tone::Error,
            "Не удалось отключить системный прокси",
            format!(
                "Включён прокси {server}. Отключите его вручную: Параметры → Сеть и \
                 Интернет → Прокси."
            ),
        )],
    };
    if let Some(host) = pac_host {
        findings.push(Finding::new(
            Tone::Warning,
            "Включён сценарий настройки прокси",
            format!(
                "Адрес сценария: {host}. Если подключение не работает, выключите «Использовать \
                 сценарий настройки» в Параметры → Сеть и Интернет → Прокси."
            ),
        ));
    }
    findings
}

/// Linux: proxy variables in the daemon's environment. Only names are shown: values can carry
/// credentials.
pub fn proxy_environment_findings(variables: &[String]) -> Vec<Finding> {
    if variables.is_empty() {
        return Vec::new();
    }
    vec![Finding::new(
        Tone::Warning,
        "Прокси задан в переменных окружения",
        format!(
            "{}. Программы могут отправлять трафик мимо VPN — уберите эти переменные, если \
             подключение не работает.",
            variables.join(", ")
        ),
    )]
}

pub fn vpn_findings(connections: &[String]) -> Vec<Finding> {
    connections
        .iter()
        .map(|name| {
            Finding::new(
                Tone::Warning,
                format!("Активно VPN-подключение «{name}»"),
                "Два VPN одновременно конфликтуют. Отключите его: Параметры → Сеть и \
                 Интернет → VPN.",
            )
        })
        .collect()
}

/// Drover (a Discord proxy loader) found in the user's Discord folders.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DroverState {
    pub files: usize,
    pub discord_running: bool,
}

pub fn drover_report(state: Option<&DroverState>) -> StepReport {
    let Some(state) = state.filter(|state| state.files > 0) else {
        return StepReport::default();
    };
    if state.discord_running {
        return StepReport {
            findings: vec![Finding::new(
                Tone::Error,
                "Найден Drover для Discord",
                "Подменная version.dll в папке Discord пускает его трафик мимо VPN. Закройте \
                 Discord и повторите проверку — тогда её можно будет удалить.",
            )],
            fixes: Vec::new(),
        };
    }
    StepReport {
        findings: vec![Finding::new(
            Tone::Warning,
            "Найден Drover для Discord",
            "Подменная version.dll в папке Discord пускает его трафик мимо VPN и может \
             мешать подключению.",
        )],
        fixes: vec![FixOffer {
            id: FixId::DeleteDrover.as_text(),
            title: "Удалить Drover из папки Discord".into(),
            detail: format!(
                "Будут удалены version.dll и drover.ini (файлов: {}). Сам Discord \
                 продолжит работать.",
                state.files
            ),
            needs_admin: false,
            needs_reboot: false,
        }],
    }
}

pub fn hosts_findings(path: &str, entries: &[String]) -> Vec<Finding> {
    if entries.is_empty() {
        return Vec::new();
    }
    let examples = entries
        .iter()
        .take(3)
        .map(|entry| format!("«{entry}»"))
        .collect::<Vec<_>>()
        .join(", ");
    vec![Finding::new(
        Tone::Warning,
        format!("В файле hosts есть записи: {}", entries.len()),
        format!(
            "{path}: {examples}. Они могут перенаправлять сайты. MULTICORE этот файл не \
             меняет — проверьте его вручную."
        ),
    )]
}

/// `HKLM\SOFTWARE\Policies\Microsoft\Windows\Network Connections` values set to 0.
pub const NETWORK_POLICY_VALUES: [&str; 3] = [
    "NC_AllowNetBridge_NLA",
    "NC_LanChangeProperties",
    "NC_AllowRasConnectionsToAllUsers",
];

pub fn policy_findings(restricted: &[&str]) -> Vec<Finding> {
    if restricted.is_empty() {
        return Vec::new();
    }
    vec![Finding::new(
        Tone::Error,
        "Ограничения сети",
        format!(
            "Групповые политики запрещают менять сетевые настройки ({}). Возможно, это \
             корпоративный или школьный ПК — обратитесь к его администратору.",
            restricted.join(", ")
        ),
    )]
}

/// The clock against an HTTPS `Date` header.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClockState {
    InSync,
    /// Seconds this clock is ahead (positive) or behind (negative).
    Skewed(i64),
    Unknown,
}

/// More than this is reported (TLS breaks well before an hour, some services at minutes).
pub const MAX_CLOCK_SKEW_SECONDS: i64 = 120;

pub fn clock_state(skew_seconds: Option<i64>) -> ClockState {
    match skew_seconds {
        None => ClockState::Unknown,
        Some(skew) if skew.abs() > MAX_CLOCK_SKEW_SECONDS => ClockState::Skewed(skew),
        Some(_) => ClockState::InSync,
    }
}

fn resync_offer() -> FixOffer {
    FixOffer {
        id: FixId::Privileged(PrivilegedFix::ResyncTime).as_text(),
        title: "Синхронизировать время".into(),
        detail: "Запустит службу времени Windows и сверит часы с сервером времени.".into(),
        needs_admin: true,
        needs_reboot: false,
    }
}

fn skew_text(seconds: i64) -> String {
    let minutes = seconds.unsigned_abs().div_ceil(60);
    let amount = if minutes >= 120 {
        format!("{} ч", minutes / 60)
    } else {
        format!("{minutes} мин")
    };
    if seconds > 0 {
        format!("спешат на {amount}")
    } else {
        format!("отстают на {amount}")
    }
}

pub fn clock_report(state: ClockState, windows: bool) -> StepReport {
    match state {
        ClockState::InSync => StepReport::default(),
        ClockState::Skewed(seconds) => StepReport {
            findings: vec![Finding::new(
                Tone::Warning,
                format!("Часы {}", skew_text(seconds)),
                "Неверное время ломает защищённые подключения. Включите «Установить время \
                 автоматически» или синхронизируйте часы.",
            )],
            fixes: if windows {
                vec![resync_offer()]
            } else {
                Vec::new()
            },
        },
        ClockState::Unknown => StepReport {
            findings: vec![Finding::new(
                Tone::Info,
                "Не удалось сверить время",
                "Сервер проверки времени не ответил — возможно, нет доступа в интернет.",
            )],
            fixes: if windows {
                vec![resync_offer()]
            } else {
                Vec::new()
            },
        },
    }
}

/// The network step: the DNS cache flush that already ran, a machine-wide WinHTTP proxy, and
/// the always-offered (never automatic) Winsock reset.
pub fn network_report(dns_flushed: Option<bool>, winhttp_proxy: Option<&str>) -> StepReport {
    let mut report = StepReport::default();
    match dns_flushed {
        Some(true) => report.findings.push(Finding::new(
            Tone::Info,
            "DNS-кэш очищен",
            "Устаревшие записи DNS больше не помешают подключению.",
        )),
        Some(false) => report.findings.push(Finding::new(
            Tone::Warning,
            "Не удалось очистить DNS-кэш",
            "Это не критично: кэш обновится сам или после перезагрузки.",
        )),
        None => {}
    }
    if let Some(proxy) = winhttp_proxy {
        report.findings.push(Finding::new(
            Tone::Warning,
            "Задан прокси WinHTTP",
            format!("Системные службы Windows ходят через {proxy}. Это может мешать VPN."),
        ));
        report.fixes.push(FixOffer {
            id: FixId::Privileged(PrivilegedFix::WinhttpResetProxy).as_text(),
            title: "Сбросить прокси WinHTTP".into(),
            detail: "То же, что «netsh winhttp reset proxy»: службы Windows снова будут \
                     подключаться напрямую."
                .into(),
            needs_admin: true,
            needs_reboot: false,
        });
    }
    report.fixes.push(FixOffer {
        id: FixId::Privileged(PrivilegedFix::WinsockReset).as_text(),
        title: "Сбросить сетевые настройки (Winsock)".into(),
        detail: "Восстанавливает сетевой стек Windows, если его испортила другая программа. \
                 Нужна перезагрузка ПК; некоторые VPN и антивирусы после сброса придётся \
                 переустановить."
            .into(),
        needs_admin: true,
        needs_reboot: true,
    });
    report
}

// ---------------------------------------------------------------------------------------------
// Running fixes.
// ---------------------------------------------------------------------------------------------

/// The always-ready service, as troubleshooting sees it.
pub trait ServiceFixes: Send + Sync {
    fn service_available(&self) -> bool;
    fn run_fixes(&self, fixes: &[PrivilegedFix]) -> Result<Vec<FixOutcome>, ServiceError>;
}

impl<C: crate::service_client::ServiceConnector> ServiceFixes
    for crate::service_client::ServiceSession<C>
{
    fn service_available(&self) -> bool {
        self.probe_mode() == crate::service_client::ServiceMode::Service
    }

    fn run_fixes(&self, fixes: &[PrivilegedFix]) -> Result<Vec<FixOutcome>, ServiceError> {
        self.troubleshoot(fixes)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HelperError {
    /// The user dismissed the administrator prompt.
    Cancelled,
    Failed,
}

/// `multicore-core-host --troubleshoot` behind a UAC prompt.
pub trait ElevatedFixes: Send + Sync {
    fn run_fixes(&self, fixes: &[PrivilegedFix]) -> Result<Vec<FixOutcome>, HelperError>;
}

/// Why a privileged batch did not run at all.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BatchError {
    Cancelled,
    /// The installed service predates troubleshooting.
    ServiceTooOld,
    /// Another user's session holds the service.
    Busy,
    /// The service's tunnel is up.
    TunnelUp,
    Failed,
}

/// Runs a privileged batch: through the service when it is there (no prompt), through the UAC
/// helper otherwise, when the service says this user is not an administrator, or for the
/// service's own re-registration. A service that cannot be reached also falls back to the
/// helper; a timed-out one does not (its fixes may still be running).
pub fn run_privileged(
    fixes: &[PrivilegedFix],
    service: Option<&dyn ServiceFixes>,
    helper: &dyn ElevatedFixes,
) -> Result<Vec<FixOutcome>, BatchError> {
    let helper_run = || {
        helper.run_fixes(fixes).map_err(|error| match error {
            HelperError::Cancelled => BatchError::Cancelled,
            HelperError::Failed => BatchError::Failed,
        })
    };
    let via_service = fixes.iter().all(|fix| fix.service_may_run());
    let Some(service) = service.filter(|service| via_service && service.service_available()) else {
        return helper_run();
    };
    match service.run_fixes(fixes) {
        Ok(outcomes) if outcomes.len() == fixes.len() => Ok(outcomes),
        Ok(_) | Err(ServiceError::Protocol) => Err(BatchError::Failed),
        Err(ServiceError::Rejected(BrokerErrorCode::AuthenticationFailed)) => helper_run(),
        Err(ServiceError::Rejected(BrokerErrorCode::InvalidRequest)) => {
            Err(BatchError::ServiceTooOld)
        }
        Err(ServiceError::Rejected(BrokerErrorCode::NotReady)) => Err(BatchError::TunnelUp),
        Err(ServiceError::Busy) => Err(BatchError::Busy),
        Err(ServiceError::TimedOut | ServiceError::Rejected(_)) => Err(BatchError::Failed),
        Err(
            ServiceError::NotInstalled
            | ServiceError::Unavailable
            | ServiceError::Impostor
            | ServiceError::InUse
            | ServiceError::Disconnected,
        ) => helper_run(),
    }
}

pub fn batch_error_finding(error: BatchError) -> Finding {
    match error {
        BatchError::Cancelled => Finding::new(
            Tone::Error,
            "Исправления отменены",
            "Без разрешения администратора их не выполнить.",
        ),
        BatchError::ServiceTooOld => Finding::new(
            Tone::Warning,
            "Служба MULTICORE устарела",
            "Обновите MULTICORE — тогда служба сможет выполнять эти исправления.",
        ),
        BatchError::Busy => Finding::new(
            Tone::Error,
            "MULTICORE запущен у другого пользователя",
            "Исправления можно выполнить, когда он отключится.",
        ),
        BatchError::TunnelUp => Finding::new(
            Tone::Error,
            "Сначала отключитесь",
            "Пока работает подключение, адаптер Wintun удалять нельзя.",
        ),
        BatchError::Failed => Finding::new(
            Tone::Error,
            "Не удалось выполнить исправления",
            "Запуск с правами администратора не удался. Повторите попытку.",
        ),
    }
}

/// The card for one finished privileged fix.
pub fn privileged_finding(fix: PrivilegedFix, outcome: FixOutcome) -> Finding {
    use FixOutcome::{Done, Failed, NothingToDo};
    match (fix, outcome) {
        (PrivilegedFix::RemoveDpiService { service }, outcome) => {
            let name = service.display_name();
            match outcome {
                Done => Finding::new(
                    Tone::Success,
                    format!("Служба {name} удалена"),
                    "Если она не успела остановиться, то исчезнет после перезагрузки.",
                ),
                NothingToDo => Finding::new(
                    Tone::Info,
                    format!("Службы {name} уже нет"),
                    "Удалять нечего.",
                ),
                Failed => Finding::new(
                    Tone::Error,
                    format!("Не удалось удалить службу {name}"),
                    "Удалите программу через «Установка и удаление программ».",
                ),
            }
        }
        (PrivilegedFix::WinhttpResetProxy, Done | NothingToDo) => Finding::new(
            Tone::Success,
            "Прокси WinHTTP сброшен",
            "Службы Windows подключаются напрямую.",
        ),
        (PrivilegedFix::WinhttpResetProxy, Failed) => Finding::new(
            Tone::Error,
            "Не удалось сбросить прокси WinHTTP",
            "Выполните «netsh winhttp reset proxy» от имени администратора.",
        ),
        (PrivilegedFix::ResyncTime, Done | NothingToDo) => Finding::new(
            Tone::Success,
            "Время синхронизировано",
            "Часы сверены с сервером времени.",
        ),
        (PrivilegedFix::ResyncTime, Failed) => Finding::new(
            Tone::Error,
            "Не удалось синхронизировать время",
            "Включите «Установить время автоматически» в Параметры → Время и язык.",
        ),
        (PrivilegedFix::WinsockReset, Done | NothingToDo) => Finding::new(
            Tone::Success,
            "Настройки Winsock сброшены",
            "Перезагрузите ПК, чтобы изменения вступили в силу.",
        ),
        (PrivilegedFix::WinsockReset, Failed) => Finding::new(
            Tone::Error,
            "Не удалось сбросить Winsock",
            "Выполните «netsh winsock reset» от имени администратора.",
        ),
        (PrivilegedFix::ReinstallWintun, Done) => Finding::new(
            Tone::Success,
            "Адаптер Wintun удалён",
            "MULTICORE создаст его заново при следующем подключении.",
        ),
        (PrivilegedFix::ReinstallWintun, NothingToDo) => Finding::new(
            Tone::Info,
            "Адаптера Wintun нет",
            "Он будет создан заново при следующем подключении.",
        ),
        (PrivilegedFix::ReinstallWintun, Failed) => Finding::new(
            Tone::Error,
            "Не удалось удалить адаптер Wintun",
            "Перезагрузите ПК и повторите.",
        ),
        (PrivilegedFix::ReinstallService, Done | NothingToDo) => Finding::new(
            Tone::Success,
            "Служба MULTICORE переустановлена",
            "Служба зарегистрирована заново и запущена.",
        ),
        (PrivilegedFix::ReinstallService, Failed) => Finding::new(
            Tone::Error,
            "Не удалось переустановить службу",
            "Перезагрузите ПК и повторите или переустановите MULTICORE установщиком.",
        ),
    }
}

/// Whether a successful `fix` only takes effect after a reboot.
pub fn needs_reboot(fix: PrivilegedFix, outcome: FixOutcome) -> bool {
    outcome == FixOutcome::Done
        && matches!(
            fix,
            PrivilegedFix::WinsockReset | PrivilegedFix::RemoveDpiService { .. }
        )
}

/// Outcome of deleting Drover's files.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DroverRemoval {
    Removed,
    NothingFound,
    DiscordRunning,
    Failed,
}

pub fn drover_removal_finding(result: DroverRemoval) -> Finding {
    match result {
        DroverRemoval::Removed => Finding::new(
            Tone::Success,
            "Drover удалён",
            "Файлы удалены из папок Discord.",
        ),
        DroverRemoval::NothingFound => {
            Finding::new(Tone::Info, "Drover не найден", "Удалять нечего.")
        }
        DroverRemoval::DiscordRunning => Finding::new(
            Tone::Error,
            "Закройте Discord и повторите",
            "Пока Discord запущен, его файлы удалить нельзя.",
        ),
        DroverRemoval::Failed => Finding::new(
            Tone::Error,
            "Не удалось удалить Drover",
            "Закройте Discord и повторите.",
        ),
    }
}

/// Runs confirmed fixes: Drover's files here, the privileged ones in one batch (one prompt).
pub fn run_fixes(
    fixes: &[FixId],
    remove_drover: &dyn Fn() -> DroverRemoval,
    service: Option<&dyn ServiceFixes>,
    helper: &dyn ElevatedFixes,
) -> FixReport {
    let mut report = FixReport::default();
    if fixes.contains(&FixId::DeleteDrover) {
        report
            .findings
            .push(drover_removal_finding(remove_drover()));
    }
    let privileged = fixes
        .iter()
        .filter_map(|fix| match fix {
            FixId::Privileged(fix) => Some(*fix),
            FixId::DeleteDrover => None,
        })
        .collect::<Vec<_>>();
    if privileged.is_empty() {
        return report;
    }
    match run_privileged(&privileged, service, helper) {
        Ok(outcomes) => {
            for (fix, outcome) in privileged.iter().zip(outcomes) {
                report.reboot_recommended |= needs_reboot(*fix, outcome);
                report.findings.push(privileged_finding(*fix, outcome));
            }
        }
        Err(error) => report.findings.push(batch_error_finding(error)),
    }
    report
}

/// Linux: the proxy environment, `/etc/hosts` and the clock. Fixes are Windows only.
#[cfg(not(windows))]
#[derive(Clone, Default)]
pub struct SystemTroubleshooting;

#[cfg(not(windows))]
#[async_trait::async_trait]
impl Troubleshooting for SystemTroubleshooting {
    async fn check(&self, step: CheckStep, _ports: Vec<ListenPort>) -> StepReport {
        let findings = |findings| StepReport {
            findings,
            fixes: Vec::new(),
        };
        match step {
            CheckStep::Time => clock_report(clock_state(clock::measure_skew().await), false),
            CheckStep::Proxy => {
                const NAMES: [&str; 8] = [
                    "http_proxy",
                    "https_proxy",
                    "all_proxy",
                    "ftp_proxy",
                    "HTTP_PROXY",
                    "HTTPS_PROXY",
                    "ALL_PROXY",
                    "FTP_PROXY",
                ];
                let set = NAMES
                    .into_iter()
                    .filter(|name| std::env::var_os(name).is_some_and(|value| !value.is_empty()))
                    .map(str::to_owned)
                    .collect::<Vec<_>>();
                findings(proxy_environment_findings(&set))
            }
            CheckStep::Hosts => {
                let path = std::path::Path::new("/etc/hosts");
                let machine = std::fs::read_to_string("/etc/hostname")
                    .ok()
                    .map(|name| name.trim().to_owned());
                let entries = hosts::read_hosts(path)
                    .map(|contents| hosts::custom_entries(&contents, machine.as_deref()))
                    .unwrap_or_default();
                findings(hosts_findings("/etc/hosts", &entries))
            }
            _ => StepReport::default(),
        }
    }

    async fn fix(&self, _fixes: Vec<FixId>) -> FixReport {
        FixReport {
            findings: vec![Finding::new(
                Tone::Info,
                "Доступно только в Windows",
                "В Linux исправления выполняются вручную.",
            )],
            reboot_recommended: false,
        }
    }
}

#[cfg(test)]
mod tests;
