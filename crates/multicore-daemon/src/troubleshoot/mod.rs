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
use multicore_i18n::MessageKey;

use crate::service_client::ServiceError;

/// The most fix ids one `POST /v1/troubleshoot/fix` may carry.
pub const MAX_FIX_REQUEST: usize = 12;

/// A registered daemon message (`multicore_i18n::daemon_messages`) without arguments.
fn key(code: &str) -> MessageKey {
    MessageKey::new(code)
}

/// A registered daemon message with arguments.
fn key_with(code: &str, args: impl IntoIterator<Item = impl Into<String>>) -> MessageKey {
    MessageKey::with_args(code, args)
}

fn finding(tone: Tone, title: MessageKey, detail: MessageKey) -> Finding {
    Finding::keyed(tone, title, detail)
}

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
                finding(
                    Tone::Error,
                    key_with("ts.antivirus.found", [item.name]),
                    key_with("ts.antivirus.severe", [item.name]),
                )
            } else {
                finding(
                    Tone::Warning,
                    key_with("ts.antivirus.found", [item.name]),
                    key("ts.antivirus.mild"),
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
        report.findings.push(if state.running {
            finding(
                Tone::Error,
                key_with("ts.dpi.running", [name]),
                key(if driver {
                    "ts.dpi.driver_running"
                } else {
                    "ts.dpi.program_running"
                }),
            )
        } else {
            finding(
                Tone::Warning,
                key_with("ts.dpi.installed", [name]),
                key(if driver {
                    "ts.dpi.driver_stopped"
                } else {
                    "ts.dpi.program_stopped"
                }),
            )
        });
        report.fixes.push(FixOffer::keyed(
            FixId::Privileged(PrivilegedFix::RemoveDpiService {
                service: state.service,
            })
            .as_text(),
            key_with("ts.dpi.remove", [name]),
            key("ts.dpi.remove_detail"),
            true,
            false,
        ));
    }
    if !folders.is_empty() {
        report.findings.push(finding(
            Tone::Warning,
            key("ts.dpi.folders"),
            key_with("ts.dpi.folders_detail", [folders.join(", ")]),
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
        let port = owner.port.to_string();
        findings.push(finding(
            Tone::Warning,
            key_with(
                if owner.udp {
                    "ts.port.busy_udp"
                } else {
                    "ts.port.busy"
                },
                [port],
            ),
            owner.process_name.as_deref().map_or_else(
                || key("ts.port.unknown_owner"),
                |name| key_with("ts.port.named_owner", [name]),
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

/// The host of a proxy auto-configuration script.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PacHost<'a> {
    Named(&'a str),
    /// The script address is not a URL with a host.
    Unrecognized,
}

pub fn proxy_findings(outcome: &ProxyOutcome, pac_host: Option<PacHost<'_>>) -> Vec<Finding> {
    let mut findings = match outcome {
        ProxyOutcome::Direct => Vec::new(),
        ProxyOutcome::StaleOursCleared => vec![finding(
            Tone::Info,
            key("ts.proxy.stale_cleared"),
            key("ts.proxy.stale_cleared_detail"),
        )],
        ProxyOutcome::ForeignDisabled { server } => vec![finding(
            Tone::Info,
            key("ts.proxy.disabled"),
            key_with("ts.proxy.disabled_detail", [server.as_str()]),
        )],
        ProxyOutcome::DisableFailed { server } => vec![finding(
            Tone::Error,
            key("ts.proxy.disable_failed"),
            key_with("ts.proxy.disable_failed_detail", [server.as_str()]),
        )],
    };
    if let Some(host) = pac_host {
        findings.push(finding(
            Tone::Warning,
            key("ts.proxy.pac"),
            match host {
                PacHost::Named(host) => key_with("ts.proxy.pac_detail", [host]),
                PacHost::Unrecognized => key("ts.proxy.pac_detail_unparsed"),
            },
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
    vec![finding(
        Tone::Warning,
        key("ts.proxy.env"),
        key_with("ts.proxy.env_detail", [variables.join(", ")]),
    )]
}

pub fn vpn_findings(connections: &[String]) -> Vec<Finding> {
    connections
        .iter()
        .map(|name| {
            finding(
                Tone::Warning,
                key_with("ts.vpn.active", [name.as_str()]),
                key("ts.vpn.active_detail"),
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
            findings: vec![finding(
                Tone::Error,
                key("ts.drover.found"),
                key("ts.drover.found_running"),
            )],
            fixes: Vec::new(),
        };
    }
    StepReport {
        findings: vec![finding(
            Tone::Warning,
            key("ts.drover.found"),
            key("ts.drover.found_detail"),
        )],
        fixes: vec![FixOffer::keyed(
            FixId::DeleteDrover.as_text(),
            key("ts.drover.remove"),
            key_with("ts.drover.remove_detail", [state.files.to_string()]),
            false,
            false,
        )],
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
    vec![finding(
        Tone::Warning,
        key_with("ts.hosts.entries", [entries.len().to_string()]),
        key_with("ts.hosts.entries_detail", [path.to_owned(), examples]),
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
    vec![finding(
        Tone::Error,
        key("ts.policy.restricted"),
        key_with("ts.policy.restricted_detail", [restricted.join(", ")]),
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
    FixOffer::keyed(
        FixId::Privileged(PrivilegedFix::ResyncTime).as_text(),
        key("ts.clock.resync"),
        key("ts.clock.resync_detail"),
        true,
        false,
    )
}

/// «Часы спешат на 5 мин», «Часы отстают на 3 ч».
fn skew_title(seconds: i64) -> MessageKey {
    let minutes = seconds.unsigned_abs().div_ceil(60);
    let (code, amount) = match (seconds > 0, minutes >= 120) {
        (true, true) => ("ts.clock.ahead_hours", minutes / 60),
        (true, false) => ("ts.clock.ahead_minutes", minutes),
        (false, true) => ("ts.clock.behind_hours", minutes / 60),
        (false, false) => ("ts.clock.behind_minutes", minutes),
    };
    key_with(code, [amount.to_string()])
}

pub fn clock_report(state: ClockState, windows: bool) -> StepReport {
    match state {
        ClockState::InSync => StepReport::default(),
        ClockState::Skewed(seconds) => StepReport {
            findings: vec![finding(
                Tone::Warning,
                skew_title(seconds),
                key("ts.clock.skewed_detail"),
            )],
            fixes: if windows {
                vec![resync_offer()]
            } else {
                Vec::new()
            },
        },
        ClockState::Unknown => StepReport {
            findings: vec![finding(
                Tone::Info,
                key("ts.clock.unknown"),
                key("ts.clock.unknown_detail"),
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
        Some(true) => report.findings.push(finding(
            Tone::Info,
            key("ts.network.dns_flushed"),
            key("ts.network.dns_flushed_detail"),
        )),
        Some(false) => report.findings.push(finding(
            Tone::Warning,
            key("ts.network.dns_flush_failed"),
            key("ts.network.dns_flush_failed_detail"),
        )),
        None => {}
    }
    if let Some(proxy) = winhttp_proxy {
        report.findings.push(finding(
            Tone::Warning,
            key("ts.network.winhttp"),
            key_with("ts.network.winhttp_detail", [proxy]),
        ));
        report.fixes.push(FixOffer::keyed(
            FixId::Privileged(PrivilegedFix::WinhttpResetProxy).as_text(),
            key("ts.network.winhttp_reset"),
            key("ts.network.winhttp_reset_detail"),
            true,
            false,
        ));
    }
    report.fixes.push(FixOffer::keyed(
        FixId::Privileged(PrivilegedFix::WinsockReset).as_text(),
        key("ts.network.winsock_reset"),
        key("ts.network.winsock_reset_detail"),
        true,
        true,
    ));
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
        // TunRejected only comes from SetTun; a fix batch never sends one.
        Err(ServiceError::TimedOut | ServiceError::Rejected(_) | ServiceError::TunRejected(_)) => {
            Err(BatchError::Failed)
        }
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
        BatchError::Cancelled => finding(
            Tone::Error,
            key("ts.batch.cancelled"),
            key("ts.batch.cancelled_detail"),
        ),
        BatchError::ServiceTooOld => finding(
            Tone::Warning,
            key("ts.batch.service_too_old"),
            key("ts.batch.service_too_old_detail"),
        ),
        BatchError::Busy => finding(
            Tone::Error,
            key("ts.batch.busy"),
            key("ts.batch.busy_detail"),
        ),
        BatchError::TunnelUp => finding(
            Tone::Error,
            key("ts.batch.tunnel_up"),
            key("ts.batch.tunnel_up_detail"),
        ),
        BatchError::Failed => finding(
            Tone::Error,
            key("ts.batch.failed"),
            key("ts.batch.failed_detail"),
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
                Done => finding(
                    Tone::Success,
                    key_with("ts.fix.dpi_removed", [name]),
                    key("ts.fix.dpi_removed_detail"),
                ),
                NothingToDo => finding(
                    Tone::Info,
                    key_with("ts.fix.dpi_absent", [name]),
                    key("ts.nothing_to_remove"),
                ),
                Failed => finding(
                    Tone::Error,
                    key_with("ts.fix.dpi_remove_failed", [name]),
                    key("ts.fix.dpi_remove_failed_detail"),
                ),
            }
        }
        (PrivilegedFix::WinhttpResetProxy, Done | NothingToDo) => finding(
            Tone::Success,
            key("ts.fix.winhttp_done"),
            key("ts.fix.winhttp_done_detail"),
        ),
        (PrivilegedFix::WinhttpResetProxy, Failed) => finding(
            Tone::Error,
            key("ts.fix.winhttp_failed"),
            key("ts.fix.winhttp_failed_detail"),
        ),
        (PrivilegedFix::ResyncTime, Done | NothingToDo) => finding(
            Tone::Success,
            key("ts.fix.time_done"),
            key("ts.fix.time_done_detail"),
        ),
        (PrivilegedFix::ResyncTime, Failed) => finding(
            Tone::Error,
            key("ts.fix.time_failed"),
            key("ts.fix.time_failed_detail"),
        ),
        (PrivilegedFix::WinsockReset, Done | NothingToDo) => finding(
            Tone::Success,
            key("ts.fix.winsock_done"),
            key("ts.fix.winsock_done_detail"),
        ),
        (PrivilegedFix::WinsockReset, Failed) => finding(
            Tone::Error,
            key("ts.fix.winsock_failed"),
            key("ts.fix.winsock_failed_detail"),
        ),
        (PrivilegedFix::ReinstallWintun, Done) => finding(
            Tone::Success,
            key("ts.fix.wintun_done"),
            key("ts.fix.wintun_done_detail"),
        ),
        (PrivilegedFix::ReinstallWintun, NothingToDo) => finding(
            Tone::Info,
            key("ts.fix.wintun_absent"),
            key("ts.fix.wintun_absent_detail"),
        ),
        (PrivilegedFix::ReinstallWintun, Failed) => finding(
            Tone::Error,
            key("ts.fix.wintun_failed"),
            key("ts.fix.wintun_failed_detail"),
        ),
        (PrivilegedFix::ReinstallService, Done | NothingToDo) => finding(
            Tone::Success,
            key("ts.fix.service_done"),
            key("ts.fix.service_done_detail"),
        ),
        (PrivilegedFix::ReinstallService, Failed) => finding(
            Tone::Error,
            key("ts.fix.service_failed"),
            key("ts.fix.service_failed_detail"),
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
        DroverRemoval::Removed => finding(
            Tone::Success,
            key("ts.drover.removed"),
            key("ts.drover.removed_detail"),
        ),
        DroverRemoval::NothingFound => finding(
            Tone::Info,
            key("ts.drover.not_found"),
            key("ts.nothing_to_remove"),
        ),
        DroverRemoval::DiscordRunning => finding(
            Tone::Error,
            key("ts.drover.close_discord"),
            key("ts.drover.discord_running"),
        ),
        DroverRemoval::Failed => finding(
            Tone::Error,
            key("ts.drover.remove_failed"),
            key("ts.drover.remove_failed_detail"),
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
            findings: vec![finding(
                Tone::Info,
                key("ts.linux.fixes_unavailable"),
                key("ts.linux.fixes_unavailable_detail"),
            )],
            reboot_recommended: false,
        }
    }
}

#[cfg(test)]
mod tests;
