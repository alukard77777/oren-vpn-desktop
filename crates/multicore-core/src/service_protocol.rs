//! Protocol v2 between the user daemon and the always-ready MultiCore service. Same bounded
//! framing as v1 (`elevation_protocol`), a different version tag so the two never cross.

use std::fmt;
use std::sync::LazyLock;

use regex::Regex;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use zeroize::Zeroizing;

use crate::elevation_protocol::{
    BrokerErrorCode, ElevatedEngine, ElevatedLogRecord, ElevatedRuntimeState, ProtocolError,
    decode_frame_versioned, encode_frame_versioned,
};
use crate::troubleshoot::{FixOutcome, PrivilegedFix, validate_fix_batch};

pub const SERVICE_PROTOCOL_VERSION: u16 = 2;
/// Longest accepted `Update.version` (`major.minor.patch`).
pub const MAX_UPDATE_VERSION_CHARS: usize = 32;
/// The most of the service's own log a client is shown, across `service.log` and its rotated
/// predecessor: the service serves no chunk that starts further back than this from a file's
/// end, and the daemon stops paging once it holds this much text.
pub const MAX_SERVICE_LOG_TAIL_BYTES: u64 = 512 * 1024;

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ServiceCommand {
    Prepare {
        generation_id: u64,
    },
    SetTun {
        on: bool,
    },
    Status,
    Stop {
        engine: ElevatedEngine,
    },
    Diagnostics,
    Shutdown,
    /// Downloads and fully verifies `version` but keeps the cores (and the tunnel) running.
    /// Added after 1.1.1 without a protocol bump: it is additive, and a service that does not
    /// know it answers `Error{InvalidRequest}` instead of misreading it.
    StageUpdate {
        version: String,
    },
    /// Installs `version`: reuses the release a `StageUpdate` already verified (downloading and
    /// verifying it first otherwise), then stops the cores and hands over to the updater.
    Update {
        version: String,
    },
    /// «Устранить неполадки»: runs these fixed fixes in order and reports one outcome each.
    /// Additive like `StageUpdate`: an older service answers `Error{InvalidRequest}`.
    Troubleshoot {
        fixes: Vec<PrivilegedFix>,
    },
    /// «Сохранить лог…»: one redacted chunk of the service's own log, ending at byte `before`
    /// of `file` (at its end when `None`). Read-only; the service picks the file from the enum,
    /// never from a client path. Additive like `StageUpdate`: an older service answers
    /// `Error{InvalidRequest}`.
    ServiceLog {
        file: ServiceLogFile,
        before: Option<u64>,
    },
}

/// The service's own log files under `<service directory>/logs`.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ServiceLogFile {
    /// `service.log`.
    Current,
    /// `service.log.1`, the rotated predecessor.
    Previous,
}

impl ServiceLogFile {
    #[must_use]
    pub const fn file_name(self) -> &'static str {
        match self {
            Self::Current => "service.log",
            Self::Previous => "service.log.1",
        }
    }
}

impl ServiceCommand {
    fn validate(&self) -> Result<(), ProtocolError> {
        match self {
            Self::Prepare { generation_id: 0 } => Err(ProtocolError::InvalidGeneration),
            Self::Update { version } | Self::StageUpdate { version }
                if !is_release_version(version) =>
            {
                Err(ProtocolError::InvalidSchema)
            }
            Self::Troubleshoot { fixes } if validate_fix_batch(fixes, true).is_err() => {
                Err(ProtocolError::InvalidSchema)
            }
            _ => Ok(()),
        }
    }
}

fn is_release_version(value: &str) -> bool {
    if value.is_empty() || value.len() > MAX_UPDATE_VERSION_CHARS {
        return false;
    }
    let parts = value.split('.').collect::<Vec<_>>();
    parts.len() == 3
        && parts.iter().all(|part| {
            !part.is_empty()
                && part.len() <= 9
                && part.bytes().all(|byte| byte.is_ascii_digit())
                && !(part.len() > 1 && part.starts_with('0'))
        })
}

/// Where the daemon reaches the service's Mihomo: `127.0.0.1:port` with `secret`, and the
/// loopback mixed port the system proxy points at.
#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ControllerEndpoint {
    pub port: u16,
    pub secret: String,
    pub mixed_port: u16,
}

impl fmt::Debug for ControllerEndpoint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ControllerEndpoint")
            .field("port", &self.port)
            .field("secret", &"[REDACTED]")
            .field("mixed_port", &self.mixed_port)
            .finish()
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum ServiceResponse {
    Prepared {
        generation_id: u64,
        controller: ControllerEndpoint,
    },
    TunState {
        on: bool,
    },
    Status {
        generation_id: Option<u64>,
        xray: ElevatedRuntimeState,
        mihomo: ElevatedRuntimeState,
        tun_on: bool,
        failure: Option<BrokerErrorCode>,
    },
    Stopped {
        engine: ElevatedEngine,
    },
    ProcessDiagnostics {
        xray: ElevatedRuntimeState,
        mihomo: ElevatedRuntimeState,
        tun: ElevatedRuntimeState,
        logs: Vec<ElevatedLogRecord>,
    },
    ShuttingDown,
    /// `StageUpdate` finished: `version` is downloaded and verified, nothing was stopped.
    UpdateStaged {
        version: String,
    },
    UpdateStarted,
    /// `Troubleshoot` ran: one outcome per requested fix, in order.
    Troubleshot {
        outcomes: Vec<FixOutcome>,
    },
    /// `ServiceLog`: bytes `start..end` of `file` as whole, redacted lines. `start == 0`: the
    /// beginning of the file (or of what the service shows of it) was reached; a missing file
    /// is `0..0` with no text. `end` is the requested `before` unless the file shrank since
    /// (it was rotated), which tells the client to stop paging it.
    ServiceLog {
        file: ServiceLogFile,
        start: u64,
        end: u64,
        text: String,
    },
    /// Another interactive user's session holds the cores.
    Busy,
    Error {
        code: BrokerErrorCode,
    },
}

pub fn encode_service_frame<T: Serialize>(value: &T) -> Result<Vec<u8>, ProtocolError> {
    encode_frame_versioned(SERVICE_PROTOCOL_VERSION, value)
}

pub fn decode_service_frame<T: DeserializeOwned>(frame: &[u8]) -> Result<T, ProtocolError> {
    decode_frame_versioned(SERVICE_PROTOCOL_VERSION, frame)
}

pub fn decode_service_command(frame: &[u8]) -> Result<ServiceCommand, ProtocolError> {
    let command: ServiceCommand = decode_service_frame(frame)?;
    command.validate()?;
    Ok(command)
}

pub fn write_service_frame<T: Serialize>(
    writer: &mut impl std::io::Write,
    value: &T,
) -> std::io::Result<()> {
    let frame = Zeroizing::new(
        encode_service_frame(value)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?,
    );
    writer.write_all(&frame)?;
    writer.flush()
}

#[must_use]
pub fn service_response_matches(command: &ServiceCommand, response: &ServiceResponse) -> bool {
    if matches!(
        response,
        ServiceResponse::Busy | ServiceResponse::Error { .. }
    ) {
        return true;
    }
    match (command, response) {
        (
            ServiceCommand::Prepare { generation_id },
            ServiceResponse::Prepared {
                generation_id: actual,
                ..
            },
        ) => generation_id == actual,
        (ServiceCommand::SetTun { on }, ServiceResponse::TunState { on: actual }) => on == actual,
        (ServiceCommand::Stop { engine }, ServiceResponse::Stopped { engine: actual }) => {
            engine == actual
        }
        (
            ServiceCommand::StageUpdate { version },
            ServiceResponse::UpdateStaged { version: actual },
        ) => version == actual,
        (ServiceCommand::Troubleshoot { fixes }, ServiceResponse::Troubleshot { outcomes }) => {
            fixes.len() == outcomes.len()
        }
        (
            ServiceCommand::ServiceLog { file, before },
            ServiceResponse::ServiceLog {
                file: actual,
                start,
                end,
                ..
            },
        ) => file == actual && start <= end && before.is_none_or(|before| *end <= before),
        (ServiceCommand::Status, ServiceResponse::Status { .. })
        | (ServiceCommand::Diagnostics, ServiceResponse::ProcessDiagnostics { .. })
        | (ServiceCommand::Shutdown, ServiceResponse::ShuttingDown)
        | (ServiceCommand::Update { .. }, ServiceResponse::UpdateStarted) => true,
        _ => false,
    }
}

static ANY_URL: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)\b[a-z][a-z0-9+.\-]*://[^\s"'<>]+"#).expect("static URL regex")
});
static HEADER: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)\b((?:proxy-)?authorization|cookie)(\s*:\s*).*").expect("static header regex")
});
static SENSITIVE_VALUE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r#"(?i)\b([a-z_\-]*(?:token|password|passwd|secret|api[_\-]?key|authorization|cookie|subscription|url)[a-z_\-]*)(\s*[:=]\s*)("(?:[^"\\]|\\.)*"|[^\s,;}\]]+)"#,
    )
    .expect("static sensitive value regex")
});
static WINDOWS_PROFILE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?i)\b([a-z]:[\\/]+(?:users|documents and settings)[\\/]+)[^\\/\s"':;]+"#)
        .expect("static Windows profile regex")
});
static UNIX_HOME: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(/(?:home|Users|var/home)/)[^/\s"':;]+"#).expect("static Unix home regex")
});
static SID: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\bS-1-5-21(?:-\d+)+").expect("static SID regex"));

/// What the service lets a client see of its own log. In every line: control characters
/// become spaces; URLs of any scheme (subscription and share links), `token=` / `secret:` /
/// `url=`-style values, authorization and cookie headers, UUIDs and user SIDs are redacted;
/// profile paths lose the user name (`C:\Users\[user]`, `/home/[user]`). A line carrying
/// configuration content is replaced whole, as the privileged core log does. The service log
/// is machine-wide, so this also keeps one user's paths and ids out of another user's archive.
#[must_use]
pub fn redact_service_log(text: &str) -> String {
    text.split('\n')
        .map(redact_service_log_line)
        .collect::<Vec<_>>()
        .join("\n")
}

fn redact_service_log_line(line: &str) -> String {
    let line = line
        .chars()
        .map(|c| if c.is_control() && c != '\t' { ' ' } else { c })
        .collect::<String>();
    let lower = line.to_ascii_lowercase();
    if lower.contains("\"outbounds\"") || lower.contains("proxies:") {
        // Keep the `<unix seconds>` the service writes first.
        return match line.split_once(' ') {
            Some((seconds, _))
                if !seconds.is_empty() && seconds.bytes().all(|byte| byte.is_ascii_digit()) =>
            {
                format!("{seconds} [redacted privileged diagnostic]")
            }
            _ => "[redacted privileged diagnostic]".to_owned(),
        };
    }
    let line = ANY_URL.replace_all(&line, "[redacted]");
    let line = HEADER.replace_all(&line, "$1$2[redacted]");
    let line = SENSITIVE_VALUE.replace_all(&line, "$1$2[redacted]");
    let line = crate::event::redact_text(&line);
    let line = WINDOWS_PROFILE.replace_all(&line, "$1[user]");
    let line = UNIX_HOME.replace_all(&line, "$1[user]");
    SID.replace_all(&line, "S-1-5-21-[redacted]").into_owned()
}
