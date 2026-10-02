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
