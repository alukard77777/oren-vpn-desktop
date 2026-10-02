//! The fixed, enumerated privileged fixes of «Устранить неполадки».
//!
//! A client never names a service, a path or a command: it picks from [`PrivilegedFix`], and
//! the service (`ServiceCommand::Troubleshoot`) or the UAC-elevated helper
//! (`multicore-core-host --troubleshoot <token>...`) maps each variant to one fixed operation.
//! Service names come from the [`DpiService`] allowlist only.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

/// The most fixes one request may carry: every service-side fix once, with every DPI service
/// (`ReinstallService` always runs alone). Also what the helper's exit code can report.
pub const MAX_PRIVILEGED_FIXES: usize = 8;

/// Third-party packet-filter services known to break MULTICORE's tunnel (zapret, GoodbyeDPI
/// and the WinDivert driver they load). The only services troubleshooting may stop and delete.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DpiService {
    WinDivert,
    WinDivert14,
    Zapret,
    GoodbyeDpi,
}

impl DpiService {
    pub const ALL: [Self; 4] = [
        Self::WinDivert,
        Self::WinDivert14,
        Self::Zapret,
        Self::GoodbyeDpi,
    ];

    /// The service key name in `HKLM\SYSTEM\CurrentControlSet\Services` (case-insensitive).
    #[must_use]
    pub fn service_name(self) -> &'static str {
        match self {
            Self::WinDivert => "WinDivert",
            Self::WinDivert14 => "WinDivert14",
            Self::Zapret => "zapret",
            Self::GoodbyeDpi => "GoodbyeDPI",
        }
    }

    /// What the user sees.
    #[must_use]
    pub fn display_name(self) -> &'static str {
        match self {
            Self::WinDivert => "WinDivert",
            Self::WinDivert14 => "WinDivert 1.4",
            Self::Zapret => "zapret",
            Self::GoodbyeDpi => "GoodbyeDPI",
        }
    }

    fn token(self) -> &'static str {
        match self {
            Self::WinDivert => "windivert",
            Self::WinDivert14 => "windivert14",
            Self::Zapret => "zapret",
            Self::GoodbyeDpi => "goodbyedpi",
        }
    }

    fn from_token(token: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|service| service.token() == token)
    }
}

/// One privileged troubleshooting fix.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum PrivilegedFix {
    /// `netsh winsock reset`: rebuilds the Winsock catalog; needs a reboot.
    WinsockReset,
    /// `netsh winhttp reset proxy`: removes a machine-wide WinHTTP proxy.
    WinhttpResetProxy,
    /// Starts Windows Time if needed and runs `w32tm /resync /force`.
    ResyncTime,
    /// Stops and deletes one allowlisted packet-filter service.
    RemoveDpiService { service: DpiService },
    /// Removes MULTICORE's own Wintun adapter (present or phantom); Mihomo creates it again on
    /// the next TUN connect. The shared Wintun driver package is never touched.
    ReinstallWintun,
    /// Stops, deletes and registers the `MultiCore` service again from the installed binaries.
    /// Elevated helper only: the service cannot re-register itself.
    ReinstallService,
}

impl PrivilegedFix {
    /// The command-line token of the elevated helper.
    #[must_use]
    pub fn token(self) -> String {
        match self {
            Self::WinsockReset => "winsock-reset".into(),
            Self::WinhttpResetProxy => "winhttp-reset-proxy".into(),
            Self::ResyncTime => "resync-time".into(),
            Self::RemoveDpiService { service } => format!("remove-dpi-service:{}", service.token()),
            Self::ReinstallWintun => "reinstall-wintun".into(),
            Self::ReinstallService => "reinstall-service".into(),
        }
    }

    /// Parses exactly what [`token`](Self::token) produces; anything else is `None`.
    #[must_use]
    pub fn parse_token(token: &str) -> Option<Self> {
        Some(match token {
            "winsock-reset" => Self::WinsockReset,
            "winhttp-reset-proxy" => Self::WinhttpResetProxy,
            "resync-time" => Self::ResyncTime,
            "reinstall-wintun" => Self::ReinstallWintun,
            "reinstall-service" => Self::ReinstallService,
            other => Self::RemoveDpiService {
                service: DpiService::from_token(other.strip_prefix("remove-dpi-service:")?)?,
            },
        })
    }

    /// Position in a batch: the zapret/GoodbyeDPI services before the WinDivert driver they keep
    /// loaded, and the Winsock reset (which wants a reboot) last.
    fn rank(self) -> u8 {
        match self {
            Self::RemoveDpiService {
                service: DpiService::Zapret | DpiService::GoodbyeDpi,
            } => 0,
            Self::RemoveDpiService { .. } => 1,
            Self::WinhttpResetProxy => 2,
            Self::ResyncTime => 3,
            Self::ReinstallWintun => 4,
            Self::ReinstallService => 5,
            Self::WinsockReset => 6,
        }
    }

    /// Whether the service may run it (everything but re-registering the service itself).
    #[must_use]
    pub fn service_may_run(self) -> bool {
        self != Self::ReinstallService
    }
}

/// Why a batch of fixes is refused before anything runs.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FixBatchError {
    Empty,
    TooMany,
    Duplicate,
    /// `ReinstallService` asked of the service.
    NotForService,
    /// `ReinstallService` must run alone: it restarts the service the other fixes may need.
    ServiceReinstallNotAlone,
}

/// Checks a batch: non-empty, at most [`MAX_PRIVILEGED_FIXES`], no duplicates, and
/// `ReinstallService` only alone and only for the elevated helper.
pub fn validate_fix_batch(fixes: &[PrivilegedFix], for_service: bool) -> Result<(), FixBatchError> {
    if fixes.is_empty() {
        return Err(FixBatchError::Empty);
    }
    if fixes.len() > MAX_PRIVILEGED_FIXES {
        return Err(FixBatchError::TooMany);
    }
    if fixes.iter().collect::<BTreeSet<_>>().len() != fixes.len() {
        return Err(FixBatchError::Duplicate);
    }
    if fixes.contains(&PrivilegedFix::ReinstallService) {
        if for_service {
            return Err(FixBatchError::NotForService);
        }
        if fixes.len() != 1 {
            return Err(FixBatchError::ServiceReinstallNotAlone);
        }
    }
    Ok(())
}

/// The order in which a batch runs, as indices into `fixes`; outcomes are still reported in
/// request order.
#[must_use]
pub fn execution_order(fixes: &[PrivilegedFix]) -> Vec<usize> {
    let mut order = (0..fixes.len()).collect::<Vec<_>>();
    order.sort_by_key(|&index| (fixes[index].rank(), index));
    order
}

/// What one fix did.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FixOutcome {
    Done,
    /// Nothing to change (no such service, no adapter, the proxy was already direct).
    NothingToDo,
    Failed,
}

/// The elevated helper reports through its exit code, never through a file a user could
/// redirect: `0x4D43_0000` ("MC") in the high half, two bits per fix in the low half.
const HELPER_EXIT_MARKER: u32 = 0x4D43_0000;

#[must_use]
pub fn encode_helper_exit(outcomes: &[FixOutcome]) -> u32 {
    outcomes.iter().take(MAX_PRIVILEGED_FIXES).enumerate().fold(
        HELPER_EXIT_MARKER,
        |code, (index, outcome)| {
            let bits = match outcome {
                FixOutcome::Done => 1,
                FixOutcome::NothingToDo => 2,
                FixOutcome::Failed => 3,
            };
            code | (bits << (index * 2))
        },
    )
}

/// The outcomes of `count` fixes, or `None` when the exit code is not one the helper writes
/// (it refused its arguments, crashed, or was not the helper at all).
#[must_use]
pub fn decode_helper_exit(code: u32, count: usize) -> Option<Vec<FixOutcome>> {
    if code & 0xFFFF_0000 != HELPER_EXIT_MARKER || count == 0 || count > MAX_PRIVILEGED_FIXES {
        return None;
    }
    // Bits beyond `count` must be clear.
    if count < MAX_PRIVILEGED_FIXES && (code & 0xFFFF) >> (count * 2) != 0 {
        return None;
    }
    (0..count)
        .map(|index| match (code >> (index * 2)) & 0b11 {
            1 => Some(FixOutcome::Done),
            2 => Some(FixOutcome::NothingToDo),
            3 => Some(FixOutcome::Failed),
            _ => None,
        })
        .collect()
}

/// The elevated helper's arguments: `--troubleshoot` followed by fix tokens.
pub const HELPER_FLAG: &str = "--troubleshoot";

/// Parses the helper's arguments (after the program name) into a validated batch.
#[must_use]
pub fn parse_helper_arguments(arguments: &[String]) -> Option<Vec<PrivilegedFix>> {
    let (flag, tokens) = arguments.split_first()?;
    if flag != HELPER_FLAG {
        return None;
    }
    let fixes = tokens
        .iter()
        .map(|token| PrivilegedFix::parse_token(token))
        .collect::<Option<Vec<_>>>()?;
    validate_fix_batch(&fixes, false).is_ok().then_some(fixes)
}

/// The helper's command line for `fixes` (tokens contain no spaces or quotes).
#[must_use]
pub fn helper_arguments(fixes: &[PrivilegedFix]) -> Vec<String> {
    std::iter::once(HELPER_FLAG.to_owned())
        .chain(fixes.iter().map(|fix| fix.token()))
        .collect()
}

/// One check of the troubleshooting run; the desktop runs them in [`CheckStep::ALL`] order and
/// labels its progress with [`CheckStep::label`].
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckStep {
    Antivirus,
    Dpi,
    Ports,
    Proxy,
    Vpn,
    Discord,
    Hosts,
    Policies,
    Time,
    Network,
}

impl CheckStep {
    pub const ALL: [Self; 10] = [
        Self::Antivirus,
        Self::Dpi,
        Self::Ports,
        Self::Proxy,
        Self::Vpn,
        Self::Discord,
        Self::Hosts,
        Self::Policies,
        Self::Time,
        Self::Network,
    ];

    /// The checks that mean something on this platform: Linux only has a proxy environment,
    /// a hosts file and a clock to look at.
    #[must_use]
    pub fn for_platform(windows: bool) -> Vec<Self> {
        Self::ALL
            .into_iter()
            .filter(|step| windows || matches!(step, Self::Proxy | Self::Hosts | Self::Time))
            .collect()
    }

    #[must_use]
    pub fn label(self) -> &'static str {
        use multicore_i18n::tr;
        match self {
            Self::Antivirus => tr!("Антивирусы…"),
            Self::Dpi => tr!("Программы обхода блокировок…"),
            Self::Ports => tr!("Порты…"),
            Self::Proxy => tr!("Системный прокси…"),
            Self::Vpn => tr!("VPN-подключения…"),
            Self::Discord => "Discord…",
            Self::Hosts => tr!("Файл hosts…"),
            Self::Policies => tr!("Групповые политики…"),
            Self::Time => tr!("Системное время…"),
            Self::Network => tr!("Сеть и DNS…"),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Tone {
    Error,
    Warning,
    Info,
    Success,
}

/// A result card.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Finding {
    pub tone: Tone,
    pub title: String,
    pub detail: String,
    /// Stable codes of `title`/`detail` (`multicore_i18n::daemon_messages`): the desktop shows
    /// them in its language. `title`/`detail` stay the Russian text for older clients.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title_key: Option<multicore_i18n::MessageKey>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail_key: Option<multicore_i18n::MessageKey>,
}

impl Finding {
    pub fn new(tone: Tone, title: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            tone,
            title: title.into(),
            detail: detail.into(),
            title_key: None,
            detail_key: None,
        }
    }

    /// A card from two registered daemon messages.
    ///
    /// # Panics
    /// When a code is not in `multicore_i18n::daemon_messages` (a programming error).
    #[must_use]
    pub fn keyed(
        tone: Tone,
        title: multicore_i18n::MessageKey,
        detail: multicore_i18n::MessageKey,
    ) -> Self {
        Self {
            tone,
            title: russian(&title),
            detail: russian(&detail),
            title_key: Some(title),
            detail_key: Some(detail),
        }
    }

    /// The title in the interface language (the Russian text for an unknown code).
    #[must_use]
    pub fn localized_title(&self) -> String {
        localized(self.title_key.as_ref(), &self.title)
    }

    /// The detail in the interface language (the Russian text for an unknown code).
    #[must_use]
    pub fn localized_detail(&self) -> String {
        localized(self.detail_key.as_ref(), &self.detail)
    }
}

fn russian(key: &multicore_i18n::MessageKey) -> String {
    key.russian()
        .unwrap_or_else(|| panic!("unregistered daemon message code {}", key.code))
}

fn localized(key: Option<&multicore_i18n::MessageKey>, fallback: &str) -> String {
    key.and_then(multicore_i18n::MessageKey::localize)
        .unwrap_or_else(|| fallback.to_owned())
}

/// A fix the user may tick. Nothing in an offer runs until the user confirms it.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FixOffer {
    /// A [`FixId`] in its text form.
    pub id: String,
    pub title: String,
    pub detail: String,
    /// Runs through the service or an administrator prompt.
    pub needs_admin: bool,
    /// Takes effect only after a reboot.
    pub needs_reboot: bool,
    /// Stable codes of `title`/`detail`, as in [`Finding`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title_key: Option<multicore_i18n::MessageKey>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail_key: Option<multicore_i18n::MessageKey>,
}

impl FixOffer {
    /// An offer whose texts are registered daemon messages.
    ///
    /// # Panics
    /// When a code is not in `multicore_i18n::daemon_messages` (a programming error).
    #[must_use]
    pub fn keyed(
        id: String,
        title: multicore_i18n::MessageKey,
        detail: multicore_i18n::MessageKey,
        needs_admin: bool,
        needs_reboot: bool,
    ) -> Self {
        Self {
            id,
            title: russian(&title),
            detail: russian(&detail),
            needs_admin,
            needs_reboot,
            title_key: Some(title),
            detail_key: Some(detail),
        }
    }

    #[must_use]
    pub fn localized_title(&self) -> String {
        localized(self.title_key.as_ref(), &self.title)
    }

    #[must_use]
    pub fn localized_detail(&self) -> String {
        localized(self.detail_key.as_ref(), &self.detail)
    }
}

/// `POST /v1/troubleshoot/check`.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CheckRequest {
    pub step: CheckStep,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct StepReport {
    pub findings: Vec<Finding>,
    pub fixes: Vec<FixOffer>,
}

/// `POST /v1/troubleshoot/fix`: fix ids from earlier offers (or the two separate actions).
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FixRequest {
    pub fixes: Vec<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FixReport {
    pub findings: Vec<Finding>,
    /// Something that was done only takes effect after a reboot.
    pub reboot_recommended: bool,
}

/// Every fix the desktop can ask for: the user-level Discord cleanup the daemon does itself,
/// or one privileged fix.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum FixId {
    /// Deletes Drover's `version.dll`/`drover.ini` from the user's Discord folders.
    DeleteDrover,
    Privileged(PrivilegedFix),
}

impl FixId {
    #[must_use]
    pub fn as_text(self) -> String {
        match self {
            Self::DeleteDrover => "delete-drover".into(),
            Self::Privileged(fix) => fix.token(),
        }
    }

    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        if text == "delete-drover" {
            return Some(Self::DeleteDrover);
        }
        PrivilegedFix::parse_token(text).map(Self::Privileged)
    }
}

/// A port the active subscription's cores listen on, from its Mihomo configuration.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ListenPort {
    pub port: u16,
    pub udp: bool,
    /// Mihomo's own listener (`port`, `mixed-port`, ...) rather than an Xray bridge port.
    pub mihomo: bool,
}

/// The ports `snapshot`'s cores listen on: the loopback SOCKS bridges to Xray, Mihomo's own
/// proxy listeners and its DNS listener.
#[must_use]
pub fn snapshot_listen_ports(snapshot: &crate::Snapshot) -> Vec<ListenPort> {
    let bridges = snapshot
        .mihomo
        .loopback_socks_mappings()
        .iter()
        .map(|mapping| mapping.port)
        .collect::<Vec<_>>();
    listen_ports(snapshot.mihomo.document(), &bridges)
}

fn listen_ports(document: &serde_yaml_ng::Value, bridges: &[u16]) -> Vec<ListenPort> {
    let port_of = |value: &serde_yaml_ng::Value| {
        value
            .as_u64()
            .and_then(|port| u16::try_from(port).ok())
            .filter(|port| *port != 0)
    };
    let mut ports = BTreeSet::new();
    for &port in bridges.iter().filter(|port| **port != 0) {
        ports.insert(ListenPort {
            port,
            udp: false,
            mihomo: false,
        });
    }
    for key in [
        "port",
        "socks-port",
        "mixed-port",
        "redir-port",
        "tproxy-port",
    ] {
        if let Some(port) = document.get(key).and_then(port_of) {
            ports.insert(ListenPort {
                port,
                udp: false,
                mihomo: true,
            });
        }
    }
    if let Some(port) = document
        .get("dns")
        .and_then(|dns| dns.get("listen"))
        .and_then(serde_yaml_ng::Value::as_str)
        .and_then(|listen| listen.rsplit_once(':'))
        .and_then(|(_, port)| port.parse::<u16>().ok())
        .filter(|port| *port != 0)
    {
        ports.insert(ListenPort {
            port,
            udp: true,
            mihomo: true,
        });
    }
    ports.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::{ListenPort, listen_ports};

    #[test]
    fn listen_ports_cover_bridges_mihomo_listeners_and_dns() {
        let document: serde_yaml_ng::Value = serde_yaml_ng::from_str(
            "mixed-port: 7890\nsocks-port: 0\nport: 70000\ndns:\n  listen: 0.0.0.0:1053\n",
        )
        .unwrap();
        let ports = listen_ports(&document, &[10808, 10809, 10808, 0]);
        let tcp = |port, mihomo| ListenPort {
            port,
            udp: false,
            mihomo,
        };
        assert_eq!(
            ports,
            [
                ListenPort {
                    port: 1053,
                    udp: true,
                    mihomo: true
                },
                tcp(7890, true),
                tcp(10808, false),
                tcp(10809, false),
            ]
        );
        let empty: serde_yaml_ng::Value = serde_yaml_ng::from_str("mode: rule\n").unwrap();
        assert!(listen_ports(&empty, &[]).is_empty());
    }
}
