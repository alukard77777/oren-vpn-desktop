use std::{
    collections::BTreeSet,
    ffi::{OsStr, OsString},
    fs::{self, OpenOptions},
    io::{self, Write},
    net::{IpAddr, SocketAddr},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, RwLock,
        atomic::{AtomicU64, Ordering},
    },
};

use serde::{Deserialize, Serialize};
use serde_yaml_ng::Value as YamlValue;
use thiserror::Error;

use crate::{
    ConfigError, MAX_CONFIG_BYTES, MihomoConfig, XrayConfig,
    fetch::SubscriptionMetadataHeaders,
    parse_mihomo, parse_xray,
    service_logo::{MAX_SERVICE_LOGO_OUTPUT_BYTES, is_canonical_service_logo},
    store_root::{CapabilityFile, DirectoryCapability},
};

#[derive(Debug, Clone, PartialEq)]
pub struct Snapshot {
    pub mihomo: MihomoConfig,
    pub xray: XrayConfig,
    subscription: Option<SubscriptionRecord>,
    service_logo_png: Option<Vec<u8>>,
    service_logo_path: Option<PathBuf>,
    service_logo_file: Option<CapabilityFile>,
}

/// A lightweight lease that keeps a validated, app-owned service-logo file addressable.
///
/// On Linux the exposed path names the retained file descriptor. On Windows the retained handle
/// prevents the validated file from being replaced while another process reads it.
#[derive(Clone, Debug)]
pub struct ServiceLogoLease {
    retained: CapabilityFile,
}

impl ServiceLogoLease {
    pub fn path(&self) -> Option<&Path> {
        self.retained.path()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubscriptionInfo {
    pub source_host: String,
    #[serde(default)]
    pub display_name: String,
    #[serde(default)]
    pub uploaded_bytes: Option<u64>,
    #[serde(default)]
    pub downloaded_bytes: Option<u64>,
    #[serde(default)]
    pub total_bytes: Option<u64>,
    #[serde(default)]
    pub expires_at_unix: Option<u64>,
    pub updated_at_unix: u64,
    #[serde(default)]
    pub refresh_interval_secs: Option<u64>,
    #[serde(default)]
    pub announcement_text: Option<String>,
    #[serde(default)]
    pub announcement_action_label: Option<String>,
    #[serde(default)]
    pub announcement_tone: Option<AnnouncementTone>,
}

impl SubscriptionInfo {
    pub fn used_bytes(&self) -> Option<u64> {
        match (self.uploaded_bytes, self.downloaded_bytes) {
            (Some(uploaded), Some(downloaded)) => Some(uploaded.saturating_add(downloaded)),
            (Some(uploaded), None) => Some(uploaded),
            (None, Some(downloaded)) => Some(downloaded),
            (None, None) => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AnnouncementTone {
    Info,
    Success,
    Danger,
}

impl AnnouncementTone {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Info => "info",
            Self::Success => "success",
            Self::Danger => "danger",
        }
    }
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
struct SubscriptionRecord {
    source_url: String,
    info: SubscriptionInfo,
    #[serde(default)]
    targets: SubscriptionTargets,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
struct SubscriptionTargets {
    #[serde(default)]
    home_url: Option<String>,
    #[serde(default)]
    support_url: Option<String>,
    #[serde(default)]
    announcement_url: Option<String>,
    #[serde(default)]
    logo_url: Option<String>,
}

impl std::fmt::Debug for SubscriptionRecord {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SubscriptionRecord")
            .field("source_url", &"[REDACTED]")
            .field("info", &self.info)
            .finish()
    }
}

impl Snapshot {
    pub fn parse(mihomo: &[u8], xray: &[u8]) -> Result<Self, ConfigError> {
        // Both halves are fully constructed before a Snapshot can exist.
        let mihomo = parse_mihomo(mihomo)?;
        let xray = parse_xray(xray)?;
        Ok(Self {
            mihomo,
            xray,
            subscription: None,
            service_logo_png: None,
            service_logo_path: None,
            service_logo_file: None,
        })
    }

    pub fn with_subscription_source(
        mut self,
        source_url: &str,
        userinfo_header: Option<&str>,
        updated_at_unix: u64,
    ) -> Result<Self, ConfigError> {
        let metadata = SubscriptionMetadataHeaders::from_legacy_userinfo(userinfo_header);
        self.attach_subscription(
            source_url,
            ParsedSubscriptionMetadata::parse(&metadata),
            updated_at_unix,
        )?;
        Ok(self)
    }

    pub(crate) fn with_subscription_metadata(
        mut self,
        source_url: &str,
        headers: &SubscriptionMetadataHeaders,
        updated_at_unix: u64,
    ) -> Result<Self, ConfigError> {
        self.attach_subscription(
            source_url,
            ParsedSubscriptionMetadata::parse(headers),
            updated_at_unix,
        )?;
        Ok(self)
    }

    pub(crate) fn with_merged_subscription_metadata(
        mut self,
        source_url: &str,
        mihomo: &SubscriptionMetadataHeaders,
        xray: &SubscriptionMetadataHeaders,
        updated_at_unix: u64,
    ) -> Result<Self, ConfigError> {
        let metadata = ParsedSubscriptionMetadata::parse(mihomo)
            .merge(ParsedSubscriptionMetadata::parse(xray));
        self.attach_subscription(source_url, metadata, updated_at_unix)?;
        Ok(self)
    }

    fn attach_subscription(
        &mut self,
        source_url: &str,
        metadata: ParsedSubscriptionMetadata,
        updated_at_unix: u64,
    ) -> Result<(), ConfigError> {
        let source_url = source_url.trim();
        let parsed = reqwest::Url::parse(source_url).map_err(|_| ConfigError::InvalidMihomo)?;
        if source_url.len() > 8192
            || !matches!(parsed.scheme(), "http" | "https")
            || parsed.host_str().is_none()
        {
            return Err(ConfigError::InvalidMihomo);
        }
        let source_host = parsed
            .host_str()
            .expect("validated subscription URL has a host")
            .to_owned();
        self.subscription = Some(SubscriptionRecord {
            source_url: source_url.to_owned(),
            info: SubscriptionInfo {
                display_name: metadata.title.unwrap_or_else(|| source_host.clone()),
                source_host,
                uploaded_bytes: metadata.uploaded_bytes,
                downloaded_bytes: metadata.downloaded_bytes,
                total_bytes: metadata.total_bytes.into_option(),
                expires_at_unix: metadata.expires_at_unix.into_option(),
                updated_at_unix,
                refresh_interval_secs: metadata.refresh_interval_secs,
                announcement_text: metadata.announcement_text.into_option(),
                announcement_action_label: metadata.announcement_action_label,
                announcement_tone: metadata.announcement_tone,
            },
            targets: metadata.targets,
        });
        Ok(())
    }

    pub fn subscription_source_url(&self) -> Option<&str> {
        self.subscription
            .as_ref()
            .map(|subscription| subscription.source_url.as_str())
    }

    pub fn subscription_info(&self) -> Option<&SubscriptionInfo> {
        self.subscription
            .as_ref()
            .map(|subscription| &subscription.info)
    }

    pub fn subscription_home_url(&self) -> Option<&str> {
        self.subscription.as_ref()?.targets.home_url.as_deref()
    }

    pub fn subscription_support_url(&self) -> Option<&str> {
        self.subscription.as_ref()?.targets.support_url.as_deref()
    }

    pub fn subscription_announcement_url(&self) -> Option<&str> {
        self.subscription
            .as_ref()?
            .targets
            .announcement_url
            .as_deref()
    }

    pub(crate) fn take_subscription_logo_url(&mut self) -> Option<String> {
        self.subscription.as_mut()?.targets.logo_url.take()
    }

    pub(crate) fn set_service_logo_png(&mut self, png: Vec<u8>) {
        self.service_logo_png = Some(png);
        self.service_logo_path = None;
        self.service_logo_file = None;
    }

    pub fn service_logo_path(&self) -> Option<&Path> {
        self.service_logo_path.as_deref()
    }

    pub fn service_logo_lease(&self) -> Option<ServiceLogoLease> {
        self.service_logo_file
            .as_ref()
            .cloned()
            .map(|retained| ServiceLogoLease { retained })
    }
}

#[derive(Default)]
struct ParsedSubscriptionMetadata {
    title: Option<String>,
    uploaded_bytes: Option<u64>,
    downloaded_bytes: Option<u64>,
    total_bytes: MetadataValue<u64>,
    expires_at_unix: MetadataValue<u64>,
    refresh_interval_secs: Option<u64>,
    announcement_text: MetadataValue<String>,
    announcement_action_label: Option<String>,
    announcement_tone: Option<AnnouncementTone>,
    targets: SubscriptionTargets,
}

#[derive(Default)]
enum MetadataValue<T> {
    #[default]
    Missing,
    Suppressed,
    Value(T),
}

impl<T> MetadataValue<T> {
    fn or(self, fallback: Self) -> Self {
        match self {
            Self::Missing => fallback,
            present => present,
        }
    }

    fn into_option(self) -> Option<T> {
        match self {
            Self::Value(value) => Some(value),
            Self::Missing | Self::Suppressed => None,
        }
    }

    fn is_missing(&self) -> bool {
        matches!(self, Self::Missing)
    }
}

impl ParsedSubscriptionMetadata {
    fn parse(headers: &SubscriptionMetadataHeaders) -> Self {
        let mut metadata = Self {
            title: first_decoded_text(headers, "profile-title", 128, true)
                .or_else(|| first_vendor_decoded_text(headers, "servicename", 128, true))
                .or_else(|| content_disposition_title(headers)),
            ..Self::default()
        };
        for header in headers.suffix_values("subscription-userinfo") {
            parse_subscription_userinfo(header, &mut metadata);
        }
        metadata.refresh_interval_secs = headers
            .values("profile-update-interval")
            .find_map(parse_refresh_interval);
        metadata.announcement_text = first_announcement(headers);
        metadata.announcement_action_label =
            first_plain_text(headers, &["sub-info-button-text", "banner-button-text"], 32);
        metadata.announcement_tone = headers
            .values("sub-info-color")
            .find_map(|value| match value.trim().to_ascii_lowercase().as_str() {
                "blue" => Some(AnnouncementTone::Info),
                "green" => Some(AnnouncementTone::Success),
                "red" => Some(AnnouncementTone::Danger),
                _ => None,
            });
        metadata.targets.home_url = first_url(headers, &["profile-web-page-url"], false, false);
        metadata.targets.support_url = first_url(headers, &["support-url"], true, false);
        metadata.targets.announcement_url = first_url(
            headers,
            &["announce-url", "sub-info-button-link", "banner-button-url"],
            true,
            false,
        );
        metadata.targets.logo_url = first_vendor_url(headers, "servicelogo", false, true);
        metadata
    }

    fn merge(self, fallback: Self) -> Self {
        Self {
            title: self.title.or(fallback.title),
            uploaded_bytes: self.uploaded_bytes.or(fallback.uploaded_bytes),
            downloaded_bytes: self.downloaded_bytes.or(fallback.downloaded_bytes),
            total_bytes: self.total_bytes.or(fallback.total_bytes),
            expires_at_unix: self.expires_at_unix.or(fallback.expires_at_unix),
            refresh_interval_secs: self
                .refresh_interval_secs
                .or(fallback.refresh_interval_secs),
            announcement_text: self.announcement_text.or(fallback.announcement_text),
            announcement_action_label: self
                .announcement_action_label
                .or(fallback.announcement_action_label),
            announcement_tone: self.announcement_tone.or(fallback.announcement_tone),
            targets: SubscriptionTargets {
                home_url: self.targets.home_url.or(fallback.targets.home_url),
                support_url: self.targets.support_url.or(fallback.targets.support_url),
                announcement_url: self
                    .targets
                    .announcement_url
                    .or(fallback.targets.announcement_url),
                logo_url: self.targets.logo_url.or(fallback.targets.logo_url),
            },
        }
    }
}

fn parse_subscription_userinfo(header: &str, metadata: &mut ParsedSubscriptionMetadata) {
    for field in header.split(';') {
        let Some((name, value)) = field.split_once('=') else {
            continue;
        };
        let Ok(value) = value.trim().parse::<u64>() else {
            continue;
        };
        match name.trim().to_ascii_lowercase().as_str() {
            "upload" if metadata.uploaded_bytes.is_none() => metadata.uploaded_bytes = Some(value),
            "download" if metadata.downloaded_bytes.is_none() => {
                metadata.downloaded_bytes = Some(value)
            }
            "total" if metadata.total_bytes.is_missing() => {
                metadata.total_bytes = if value == 0 {
                    MetadataValue::Suppressed
                } else {
                    MetadataValue::Value(value)
                }
            }
            "expire" if metadata.expires_at_unix.is_missing() => {
                metadata.expires_at_unix = if value == 0 {
                    MetadataValue::Suppressed
                } else {
                    MetadataValue::Value(value)
                }
            }
            _ => {}
        }
    }
}

fn first_decoded_text(
    headers: &SubscriptionMetadataHeaders,
    name: &str,
    limit: usize,
    reject_credential_like: bool,
) -> Option<String> {
    headers.values(name).find_map(|value| {
        let decoded = decode_text(value)?;
        if reject_credential_like {
            sanitize_title_candidate(&decoded, limit)
        } else {
            sanitize_text(&decoded, limit)
        }
    })
}

fn first_vendor_decoded_text(
    headers: &SubscriptionMetadataHeaders,
    suffix: &str,
    limit: usize,
    reject_credential_like: bool,
) -> Option<String> {
    headers.vendor_suffix_values(suffix).find_map(|value| {
        let decoded = decode_text(value)?;
        if reject_credential_like {
            sanitize_title_candidate(&decoded, limit)
        } else {
            sanitize_text(&decoded, limit)
        }
    })
}

fn sanitize_title_candidate(value: &str, limit: usize) -> Option<String> {
    if is_control_heavy(value) {
        return None;
    }
    let sanitized = sanitize_text(value, limit)?;
    (!looks_credential_like(&sanitized)).then_some(sanitized)
}

fn is_control_heavy(value: &str) -> bool {
    let mut total = 0_usize;
    let mut unsafe_scalars = 0_usize;
    for character in value.chars() {
        total += 1;
        if character.is_control() || is_bidi_control(character) {
            unsafe_scalars += 1;
        }
    }
    unsafe_scalars > 8 && unsafe_scalars.saturating_mul(4) > total
}

fn first_plain_text(
    headers: &SubscriptionMetadataHeaders,
    names: &[&str],
    limit: usize,
) -> Option<String> {
    names.iter().find_map(|name| {
        headers
            .values(name)
            .find_map(|value| sanitize_text(value, limit))
    })
}

fn first_announcement(headers: &SubscriptionMetadataHeaders) -> MetadataValue<String> {
    for name in ["announce", "sub-info-text", "banner-text"] {
        for value in headers.values(name) {
            let Some(decoded) = decode_text(value) else {
                continue;
            };
            if decoded.trim() == "0" {
                return MetadataValue::Suppressed;
            }
            if let Some(sanitized) = sanitize_text(&decoded, 512) {
                return MetadataValue::Value(sanitized);
            }
        }
    }
    MetadataValue::Missing
}

fn looks_credential_like(value: &str) -> bool {
    value.contains("://")
        || value
            .split_once('@')
            .is_some_and(|(authority, host)| authority.contains(':') && !host.is_empty())
}

fn decode_text(value: &str) -> Option<String> {
    let Some(encoded) = value.strip_prefix("base64:") else {
        return Some(value.to_owned());
    };
    use base64::Engine as _;
    let bytes = [
        &base64::engine::general_purpose::STANDARD,
        &base64::engine::general_purpose::STANDARD_NO_PAD,
        &base64::engine::general_purpose::URL_SAFE,
        &base64::engine::general_purpose::URL_SAFE_NO_PAD,
    ]
    .into_iter()
    .find_map(|engine| engine.decode(encoded.trim()).ok())?;
    String::from_utf8(bytes).ok()
}

fn sanitize_text(value: &str, limit: usize) -> Option<String> {
    let mut output = String::new();
    let mut pending_space = false;
    for character in value.chars() {
        if is_bidi_control(character) {
            continue;
        }
        if character.is_whitespace() || character.is_control() {
            pending_space = !output.is_empty();
            continue;
        }
        if pending_space && output.chars().count() < limit {
            output.push(' ');
        }
        pending_space = false;
        if output.chars().count() >= limit {
            break;
        }
        output.push(character);
    }
    (!output.is_empty()).then_some(output)
}

fn is_bidi_control(character: char) -> bool {
    matches!(
        character,
        '\u{061c}'
            | '\u{200e}'
            | '\u{200f}'
            | '\u{202a}'..='\u{202e}'
            | '\u{2066}'..='\u{2069}'
    )
}

fn content_disposition_title(headers: &SubscriptionMetadataHeaders) -> Option<String> {
    headers.values("content-disposition").find_map(|value| {
        value.split(';').skip(1).find_map(|parameter| {
            let (name, value) = parameter.trim().split_once('=')?;
            if !name.eq_ignore_ascii_case("filename") {
                return None;
            }
            sanitize_title_candidate(value.trim().trim_matches('"'), 128)
        })
    })
}

fn parse_refresh_interval(value: &str) -> Option<u64> {
    let hours = value.trim().parse::<u128>().ok()?;
    let seconds = hours
        .saturating_mul(60 * 60)
        .clamp(15 * 60, 30 * 24 * 60 * 60);
    Some(seconds as u64)
}

fn first_url(
    headers: &SubscriptionMetadataHeaders,
    names: &[&str],
    allow_tg: bool,
    https_only: bool,
) -> Option<String> {
    names.iter().find_map(|name| {
        headers
            .values(name)
            .find_map(|value| validate_url(value, allow_tg, https_only))
    })
}

fn first_vendor_url(
    headers: &SubscriptionMetadataHeaders,
    suffix: &str,
    allow_tg: bool,
    https_only: bool,
) -> Option<String> {
    headers
        .vendor_suffix_values(suffix)
        .find_map(|value| validate_url(value, allow_tg, https_only))
}

fn validate_url(value: &str, allow_tg: bool, https_only: bool) -> Option<String> {
    let value = value.trim();
    if value.is_empty() || value.len() > 2048 || value.chars().any(char::is_control) {
        return None;
    }
    let parsed = reqwest::Url::parse(value).ok()?;
    let scheme_allowed = if https_only {
        parsed.scheme() == "https"
    } else {
        matches!(parsed.scheme(), "http" | "https") || (allow_tg && parsed.scheme() == "tg")
    };
    if !scheme_allowed
        || parsed.host_str().is_none()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
    {
        return None;
    }
    Some(parsed.to_string())
}

#[derive(Debug, Default)]
pub struct AtomicSnapshot {
    current: RwLock<Option<Arc<Snapshot>>>,
}

impl AtomicSnapshot {
    pub fn current(&self) -> Option<Arc<Snapshot>> {
        self.current.read().expect("snapshot lock poisoned").clone()
    }

    pub fn commit(&self, snapshot: Snapshot) -> Arc<Snapshot> {
        let snapshot = Arc::new(snapshot);
        *self.current.write().expect("snapshot lock poisoned") = Some(snapshot.clone());
        snapshot
    }
}

pub trait SnapshotSink: Send + Sync {
    fn save(&self, snapshot: Snapshot) -> Result<SnapshotCommit<Arc<Snapshot>>, PersistenceError>;
}

impl SnapshotSink for AtomicSnapshot {
    fn save(&self, snapshot: Snapshot) -> Result<SnapshotCommit<Arc<Snapshot>>, PersistenceError> {
        Ok(SnapshotCommit {
            committed: self.commit(snapshot),
            maintenance_debt: None,
        })
    }
}

#[derive(Debug, Error)]
pub enum PersistenceError {
    #[error("snapshot persistence operation failed")]
    Io(#[from] io::Error),
    #[error("snapshot publication committed but directory durability maintenance is required")]
    MaintenanceDebt(#[source] io::Error),
}

#[derive(Debug)]
pub struct SnapshotCommit<T> {
    committed: T,
    maintenance_debt: Option<io::Error>,
}

impl<T> SnapshotCommit<T> {
    pub fn committed(&self) -> &T {
        &self.committed
    }

    pub fn maintenance_debt(&self) -> Option<&io::Error> {
        self.maintenance_debt.as_ref()
    }

    pub fn into_parts(self) -> (T, Option<io::Error>) {
        (self.committed, self.maintenance_debt)
    }

    pub fn into_committed(self) -> T {
        self.committed
    }
}

impl<T> std::ops::Deref for SnapshotCommit<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        &self.committed
    }
}

#[derive(Debug)]
pub struct PersistentSnapshotStore {
    root: DirectoryCapability,
    current: RwLock<Option<(u64, Arc<Snapshot>)>>,
    next_generation: AtomicU64,
    commit_lock: Mutex<()>,
    #[cfg(test)]
    logo_retention_fault: std::sync::atomic::AtomicBool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PublishedRuntime {
    pub directory: PathBuf,
    pub mihomo_config: PathBuf,
    pub xray_config: PathBuf,
}

/// A fully written and durable runtime generation which is not active yet. Dropping or aborting
/// it removes only this generation; committing it makes it active and then attempts old-generation
/// cleanup without turning an already durable activation into a failure.
#[derive(Debug)]
pub struct StagedRuntime {
    root: PathBuf,
    paths: PublishedRuntime,
    finished: bool,
}

impl StagedRuntime {
    pub fn paths(&self) -> &PublishedRuntime {
        &self.paths
    }

    pub fn commit(mut self) -> Result<PublishedRuntime, PersistenceError> {
        self.finished = true;
        let paths = self.paths.clone();
        // Cleanup is deliberately best-effort: failure cannot roll back the already durable
        // snapshot/controller activation. A later successful commit retries all old generations.
        let _ = cleanup_superseded_runtimes(&self.root, &paths.directory);
        Ok(paths)
    }

    pub fn abort(mut self) -> Result<(), PersistenceError> {
        remove_staged_runtime(&self.root, &self.paths.directory)?;
        self.finished = true;
        Ok(())
    }
}

impl Drop for StagedRuntime {
    fn drop(&mut self) {
        if !self.finished {
            let _ = remove_staged_runtime(&self.root, &self.paths.directory);
        }
    }
}

/// Private Mihomo controller settings injected only into an ephemeral runtime copy.
#[derive(Clone, PartialEq, Eq)]
pub struct MihomoRuntimeControl {
    address: SocketAddr,
    secret: String,
    xray_outbound_interface: Option<String>,
    external_ui_directory: Option<PathBuf>,
}

impl std::fmt::Debug for MihomoRuntimeControl {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("MihomoRuntimeControl")
            .field("address", &self.address)
            .field("secret", &"[REDACTED]")
            .field(
                "xray_outbound_interface",
                &self
                    .xray_outbound_interface
                    .as_ref()
                    .map(|_| "[CONFIGURED]"),
            )
            .field("external_ui_directory", &self.external_ui_directory)
            .finish()
    }
}

impl MihomoRuntimeControl {
    pub fn new(address: SocketAddr, secret: impl Into<String>) -> io::Result<Self> {
        let secret = secret.into();
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
        Ok(Self {
            address,
            secret,
            xray_outbound_interface: None,
            external_ui_directory: None,
        })
    }

    pub fn address(&self) -> SocketAddr {
        self.address
    }

    pub fn with_xray_outbound_interface(
        mut self,
        interface: impl Into<String>,
    ) -> io::Result<Self> {
        let interface = interface.into();
        let interface = interface.trim();
        if interface.is_empty() || interface.len() > 256 || interface.chars().any(char::is_control)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Xray outbound interface must be a non-empty Windows interface alias",
            ));
        }
        self.xray_outbound_interface = Some(interface.to_owned());
        Ok(self)
    }

    /// Points Mihomo's `external-ui` at a vendored, already-staged static dashboard directory so
    /// the runtime overlay serves it itself (see `crates/multicore-core/src/snapshot.rs` docs on
    /// `stage_runtime_with_mihomo_control`). The directory must be an absolute, existing directory
    /// containing an `index.html`; this crate never downloads or writes dashboard content, it only
    /// references a path the caller has already validated (checksum-verified at package time).
    ///
    /// Mihomo additionally requires this path to be a subpath of its own home directory, or listed
    /// in the `SAFE_PATHS` environment variable of the process that spawns it — this crate cannot
    /// enforce that from here; the caller that launches the Mihomo process is responsible for it.
    pub fn with_external_ui_directory(mut self, directory: impl Into<PathBuf>) -> io::Result<Self> {
        let directory = directory.into();
        if !directory.is_absolute() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Mihomo external-ui directory must be an absolute path",
            ));
        }
        if directory
            .as_os_str()
            .to_str()
            .is_none_or(|value| value.chars().any(char::is_control))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Mihomo external-ui directory must be valid, control-character-free Unicode",
            ));
        }
        if !fs::metadata(&directory).is_ok_and(|metadata| metadata.is_dir()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Mihomo external-ui directory must exist",
            ));
        }
        if !fs::metadata(directory.join("index.html")).is_ok_and(|metadata| metadata.is_file()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Mihomo external-ui directory must contain index.html",
            ));
        }
        self.external_ui_directory = Some(directory);
        Ok(self)
    }
}

impl PersistentSnapshotStore {
    pub fn open(root: impl AsRef<Path>) -> Result<Self, PersistenceError> {
        let root = root.as_ref().to_path_buf();
        fs::create_dir_all(&root)?;
        secure_directory(&root)?;
        Self::open_capability(DirectoryCapability::open(&root)?)
    }

    pub(crate) fn open_capability(root: DirectoryCapability) -> Result<Self, PersistenceError> {
        let mut generations = generation_directories(&root)?;
        for (_, directory) in &generations {
            for config in [
                "mihomo.yaml",
                "xray.json",
                "subscription.json",
                "service-logo.png",
            ] {
                match directory.file_metadata(OsStr::new(config))? {
                    Some((true, _)) => directory.secure_regular_file(OsStr::new(config))?,
                    Some((false, _)) if config != "service-logo.png" => {
                        return Err(PersistenceError::Io(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "snapshot config is not a regular file",
                        )));
                    }
                    Some((false, _)) | None => {}
                }
            }
        }
        generations.sort_unstable_by_key(|entry| std::cmp::Reverse(entry.0));
        let highest_id = generations.first().map_or(0, |(id, _)| *id);
        let mut current = None;
        for (generation, directory) in generations {
            if let Some(snapshot) = load_generation(&directory) {
                current = Some((generation, Arc::new(snapshot)));
                break;
            }
        }

        Ok(Self {
            root,
            current: RwLock::new(current),
            next_generation: AtomicU64::new(highest_id.saturating_add(1).max(1)),
            commit_lock: Mutex::new(()),
            #[cfg(test)]
            logo_retention_fault: std::sync::atomic::AtomicBool::new(false),
        })
    }

    /// Loads the newest valid generation from an existing legacy root without creating,
    /// locking, permission-repairing, or otherwise mutating that tree.
    pub(crate) fn load_latest_readonly(
        root: &Path,
    ) -> Result<Option<Arc<Snapshot>>, PersistenceError> {
        let root = DirectoryCapability::open_readonly(root)?;
        let mut generations = generation_directories(&root)?;
        generations.sort_unstable_by_key(|entry| std::cmp::Reverse(entry.0));
        Ok(generations
            .into_iter()
            .find_map(|(_, directory)| load_generation(&directory).map(Arc::new)))
    }

    pub fn current(&self) -> Option<Arc<Snapshot>> {
        self.current_with_generation().map(|(_, snapshot)| snapshot)
    }

    pub fn current_with_generation(&self) -> Option<(u64, Arc<Snapshot>)> {
        self.current
            .read()
            .expect("persistent snapshot lock poisoned")
            .as_ref()
            .map(|(generation, snapshot)| (*generation, snapshot.clone()))
    }

    pub fn current_generation(&self) -> Option<u64> {
        self.current
            .read()
            .expect("persistent snapshot lock poisoned")
            .as_ref()
            .map(|(generation, _)| *generation)
    }

    pub fn commit(
        &self,
        snapshot: Snapshot,
    ) -> Result<SnapshotCommit<Arc<Snapshot>>, PersistenceError> {
        self.commit_status(snapshot)
    }

    pub fn commit_status(
        &self,
        snapshot: Snapshot,
    ) -> Result<SnapshotCommit<Arc<Snapshot>>, PersistenceError> {
        let outcome = self.commit_with_generation_status(snapshot)?;
        let ((_, snapshot), maintenance_debt) = outcome.into_parts();
        Ok(SnapshotCommit {
            committed: snapshot,
            maintenance_debt,
        })
    }

    pub fn commit_with_generation(
        &self,
        snapshot: Snapshot,
    ) -> Result<SnapshotCommit<(u64, Arc<Snapshot>)>, PersistenceError> {
        self.commit_with_generation_status(snapshot)
    }

    pub fn commit_with_generation_status(
        &self,
        mut snapshot: Snapshot,
    ) -> Result<SnapshotCommit<(u64, Arc<Snapshot>)>, PersistenceError> {
        let _guard = self
            .commit_lock
            .lock()
            .expect("snapshot commit lock poisoned");
        let generation = self.next_generation.fetch_add(1, Ordering::Relaxed);
        let published_name = format!("snapshot-{generation:020}");
        let (staging_name, staging) = create_snapshot_staging(&self.root, &published_name)?;
        snapshot.service_logo_path = None;
        snapshot.service_logo_file = None;

        let publication = (|| {
            staging.write_file(
                OsStr::new("mihomo.yaml"),
                snapshot.mihomo.raw_yaml().as_bytes(),
            )?;
            staging.write_file(OsStr::new("xray.json"), snapshot.xray.raw_bytes())?;
            if let Some(subscription) = &snapshot.subscription {
                let mut persisted = subscription.clone();
                persisted.targets.logo_url = None;
                let encoded = serde_json::to_vec(&persisted).map_err(io::Error::other)?;
                staging.write_file(OsStr::new("subscription.json"), &encoded)?;
            }
            if let Some(logo) = &snapshot.service_logo_png {
                staging.write_file(OsStr::new("service-logo.png"), logo)?;
            }
            staging.sync()?;
            self.root
                .rename_child(&staging, OsStr::new(&published_name))?;
            Ok::<_, io::Error>(self.root.sync())
        })();
        drop(staging);

        let directory_sync = match publication {
            Ok(directory_sync) => directory_sync,
            Err(error) => {
                if let Err(cleanup) = self.root.remove_child_tree(&staging_name) {
                    return Err(PersistenceError::Io(io::Error::new(
                        cleanup.kind(),
                        format!(
                            "snapshot publication failed ({error}); staging cleanup failed: {cleanup}"
                        ),
                    )));
                }
                return Err(PersistenceError::Io(error));
            }
        };
        let mut maintenance_debt = directory_sync.err();
        if snapshot.service_logo_png.is_some() {
            #[cfg(test)]
            let inject_retention_failure = self
                .logo_retention_fault
                .swap(false, std::sync::atomic::Ordering::SeqCst);
            #[cfg(not(test))]
            let inject_retention_failure = false;
            let retention = if inject_retention_failure {
                Err(io::Error::other(
                    "injected post-publication logo retention failure",
                ))
            } else {
                self.root
                    .open_child(OsStr::new(&published_name))
                    .and_then(|directory| {
                        directory
                            .read_file_retained(
                                OsStr::new("service-logo.png"),
                                MAX_SERVICE_LOGO_OUTPUT_BYTES as u64,
                            )
                            .map(|(_, retained)| retained)
                    })
            };
            match retention {
                Ok(retained) => {
                    snapshot.service_logo_path = retained.path().map(Path::to_owned);
                    snapshot.service_logo_file = Some(retained);
                }
                Err(error) => {
                    maintenance_debt = Some(combine_persistence_debt(maintenance_debt, error));
                }
            }
        }
        let snapshot = Arc::new(snapshot);
        *self
            .current
            .write()
            .expect("persistent snapshot lock poisoned") = Some((generation, snapshot.clone()));
        Ok(SnapshotCommit {
            committed: (generation, snapshot),
            maintenance_debt,
        })
    }
}

impl SnapshotSink for PersistentSnapshotStore {
    fn save(&self, snapshot: Snapshot) -> Result<SnapshotCommit<Arc<Snapshot>>, PersistenceError> {
        self.commit_status(snapshot)
    }
}

#[cfg(test)]
impl PersistentSnapshotStore {
    pub(crate) fn inject_directory_sync_failure(&self) {
        self.root.inject_sync_failure();
    }

    fn inject_logo_retention_failure(&self) {
        self.logo_retention_fault
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

pub fn publish_runtime(
    snapshot: &Snapshot,
    runtime_root: impl AsRef<Path>,
) -> Result<PublishedRuntime, PersistenceError> {
    stage_runtime_payloads(
        snapshot.mihomo.raw_yaml().as_bytes(),
        snapshot.xray.raw_bytes(),
        runtime_root,
    )?
    .commit()
}

/// Publishes ephemeral core configs whose private controller and optional physical-interface
/// binding are owned by the daemon. The persisted subscription snapshot is never mutated.
pub fn publish_runtime_with_mihomo_control(
    snapshot: &Snapshot,
    runtime_root: impl AsRef<Path>,
    control: &MihomoRuntimeControl,
) -> Result<PublishedRuntime, PersistenceError> {
    stage_runtime_with_mihomo_control(snapshot, runtime_root, control)?.commit()
}

pub fn stage_runtime_with_mihomo_control(
    snapshot: &Snapshot,
    runtime_root: impl AsRef<Path>,
    control: &MihomoRuntimeControl,
) -> Result<StagedRuntime, PersistenceError> {
    let mut document = snapshot.mihomo.document().clone();
    let mapping = document.as_mapping_mut().ok_or_else(|| {
        PersistenceError::Io(io::Error::new(
            io::ErrorKind::InvalidData,
            "validated Mihomo document is not a mapping",
        ))
    })?;
    for key in [
        "external-controller-unix",
        "external-controller-pipe",
        "external-controller-tls",
        "external-controller-cors",
        "external-controller-routing-mark",
        "cors",
        "external-doh-server",
        "global-client-fingerprint",
    ] {
        mapping.remove(YamlValue::String(key.to_owned()));
    }
    let external_ui_keys: Vec<_> = mapping
        .keys()
        .filter(|key| {
            key.as_str()
                .is_some_and(|key| key.starts_with("external-ui"))
        })
        .cloned()
        .collect();
    for key in external_ui_keys {
        mapping.remove(&key);
    }
    mapping.insert(
        YamlValue::String("external-controller".to_owned()),
        YamlValue::String(control.address.to_string()),
    );
    mapping.insert(
        YamlValue::String("secret".to_owned()),
        YamlValue::String(control.secret.clone()),
    );
    if let Some(external_ui_directory) = &control.external_ui_directory {
        let external_ui_directory = external_ui_directory.to_str().ok_or_else(|| {
            PersistenceError::Io(io::Error::new(
                io::ErrorKind::InvalidData,
                "Mihomo external-ui directory is not valid Unicode",
            ))
        })?;
        mapping.insert(
            YamlValue::String("external-ui".to_owned()),
            YamlValue::String(external_ui_directory.to_owned()),
        );
    }
    let tun_key = YamlValue::String("tun".to_owned());
    let tun = mapping
        .entry(tun_key)
        .or_insert_with(|| YamlValue::Mapping(Default::default()));
    let tun = tun.as_mapping_mut().ok_or_else(|| {
        PersistenceError::Io(io::Error::new(
            io::ErrorKind::InvalidData,
            "validated Mihomo tun section is not a mapping",
        ))
    })?;
    tun.insert(
        YamlValue::String("device".to_owned()),
        YamlValue::String("MultiCore".to_owned()),
    );
    let profile_key = YamlValue::String("profile".to_owned());
    let profile = mapping
        .entry(profile_key)
        .or_insert_with(|| YamlValue::Mapping(Default::default()));
    if !profile.is_mapping() {
        *profile = YamlValue::Mapping(Default::default());
    }
    profile
        .as_mapping_mut()
        .expect("profile was replaced by mapping")
        .insert(
            YamlValue::String("store-selected".to_owned()),
            YamlValue::Bool(false),
        );
    let mihomo = serde_yaml_ng::to_string(&document)
        .map_err(|error| PersistenceError::Io(io::Error::new(io::ErrorKind::InvalidData, error)))?;
    let xray = xray_runtime_bytes(snapshot, control.xray_outbound_interface.as_deref())?;
    stage_runtime_payloads(mihomo.as_bytes(), &xray, runtime_root)
}

fn xray_runtime_bytes(
    snapshot: &Snapshot,
    outbound_interface: Option<&str>,
) -> Result<Vec<u8>, PersistenceError> {
    let Some(outbound_interface) = outbound_interface else {
        return Ok(snapshot.xray.raw_bytes().to_vec());
    };
    let invalid =
        |message: String| PersistenceError::Io(io::Error::new(io::ErrorKind::InvalidData, message));
    let mut document: serde_json::Value = serde_json::from_slice(snapshot.xray.raw_bytes())
        .map_err(|error| invalid(format!("validated Xray document is invalid: {error}")))?;
    // Xray strips the terminating FQDN dot before DNS matching.
    let domains: BTreeSet<_> = xray_outbound_server_domains(snapshot)?
        .into_iter()
        .map(|domain| {
            let domain = domain.trim();
            domain
                .strip_suffix('.')
                .unwrap_or(domain)
                .to_ascii_lowercase()
        })
        .collect();
    if !domains.is_empty()
        && (contains_endpoint_dns_tag(&document) || selects_endpoint_dns_outbound(&document))
    {
        return Err(invalid(
            "Xray subscription captures a reserved endpoint DNS tag".to_owned(),
        ));
    }
    let root = document
        .as_object_mut()
        .ok_or_else(|| invalid("validated Xray document is not an object".to_owned()))?;
    let outbounds = root
        .get_mut("outbounds")
        .and_then(serde_json::Value::as_array_mut)
        .ok_or_else(|| invalid("validated Xray outbounds section is not an array".to_owned()))?;
    if outbounds.is_empty() {
        return Err(invalid(
            "validated Xray outbounds section must not be empty".to_owned(),
        ));
    }

    for outbound in outbounds.iter_mut() {
        let outbound = outbound
            .as_object_mut()
            .ok_or_else(|| invalid("validated Xray outbound is not an object".to_owned()))?;
        let must_resolve_endpoint = !outbound_server_domains(outbound).is_empty();
        let stream_settings = outbound
            .entry("streamSettings")
            .or_insert_with(|| serde_json::Value::Object(Default::default()));
        if stream_settings.is_null() {
            *stream_settings = serde_json::Value::Object(Default::default());
        }
        let stream_settings = stream_settings
            .as_object_mut()
            .ok_or_else(|| invalid("validated Xray streamSettings is not an object".to_owned()))?;
        let sockopt = stream_settings
            .entry("sockopt")
            .or_insert_with(|| serde_json::Value::Object(Default::default()));
        if sockopt.is_null() {
            *sockopt = serde_json::Value::Object(Default::default());
        }
        let sockopt = sockopt
            .as_object_mut()
            .ok_or_else(|| invalid("validated Xray sockopt is not an object".to_owned()))?;
        sockopt.insert(
            "interface".to_owned(),
            serde_json::Value::String(outbound_interface.to_owned()),
        );
        if must_resolve_endpoint {
            sockopt.insert(
                "domainStrategy".to_owned(),
                serde_json::Value::String("ForceIP".to_owned()),
            );
        }
    }

    if !domains.is_empty() {
        // Append, never replace the provider's first/default outbound. DNS uses
        // a literal resolver IP, so this dial cannot recurse into endpoint DNS.
        outbounds.push(serde_json::json!({
            "tag": ENDPOINT_DNS_OUTBOUND_TAG,
            "protocol": "freedom",
            "settings": {"domainStrategy": "AsIs"},
            "streamSettings": {"sockopt": {
                "interface": outbound_interface, "domainStrategy": "AsIs"
            }}
        }));
        let dns = root
            .entry("dns")
            .or_insert_with(|| serde_json::Value::Object(Default::default()));
        if dns.is_null() {
            *dns = serde_json::Value::Object(Default::default());
        }
        let dns = dns
            .as_object_mut()
            .ok_or_else(|| invalid("validated Xray dns section is not an object".to_owned()))?;
        let servers = dns.entry("servers").or_insert(serde_json::Value::Null);
        // Preserve the old system resolver for unrelated Xray DNS consumers.
        // Endpoint lookups are scoped to the generated final-query entry.
        if servers.is_null() {
            *servers = serde_json::json!(["localhost"]);
        }
        let servers = servers.as_array_mut().ok_or_else(|| {
            invalid("validated Xray dns.servers section is not an array".to_owned())
        })?;
        if servers.is_empty() {
            servers.push(serde_json::json!("localhost"));
        }
        // Xray uses clients[0] even when skipFallback/disableFallback leave no
        // eligible clients. Keep the original resolver in that emergency slot,
        // with no matches and excluded from the normal fallback list.
        let mut emergency = match &servers[0] {
            serde_json::Value::String(address) => serde_json::json!({"address": address}),
            serde_json::Value::Object(_) => servers[0].clone(),
            _ => {
                return Err(invalid(
                    "validated Xray DNS server is not an object or string".to_owned(),
                ));
            }
        };
        emergency["domains"] = serde_json::json!([]);
        emergency["skipFallback"] = serde_json::json!(true);
        // Xray silently adds local-TLD/dotless rules to every localhost DNS
        // entry. Those would run before our scoped resolver. Such endpoints
        // require private DNS and cannot safely use this public-DNS policy.
        if emergency["address"]
            .as_str()
            .is_some_and(|address| address.trim().eq_ignore_ascii_case("localhost"))
            && domains.iter().any(|domain| local_dns_domain(domain))
        {
            return Err(invalid(
                "Xray public endpoint DNS cannot override local-only domains".to_owned(),
            ));
        }
        servers.insert(0, serde_json::json!({
            "address": "77.88.8.8",
            "domains": domains.iter().map(|domain| format!("full:{domain}")).collect::<Vec<_>>(),
            "skipFallback": true, "finalQuery": true,
            "disableCache": false, "serveStale": false,
            "tag": ENDPOINT_DNS_TAG, "timeoutMs": ENDPOINT_DNS_TIMEOUT_MS
        }));
        servers.insert(0, emergency);
        let routing = root.entry("routing").or_insert(serde_json::Value::Null);
        if routing.is_null() {
            *routing = serde_json::json!({});
        }
        let routing = routing
            .as_object_mut()
            .ok_or_else(|| invalid("validated Xray routing section is not an object".to_owned()))?;
        let rules = routing.entry("rules").or_insert(serde_json::Value::Null);
        if rules.is_null() {
            *rules = serde_json::json!([]);
        }
        let rules = rules.as_array_mut().ok_or_else(|| {
            invalid("validated Xray routing.rules section is not an array".to_owned())
        })?;
        rules.insert(
            0,
            serde_json::json!({
                "type": "field", "inboundTag": [ENDPOINT_DNS_TAG],
                "outboundTag": ENDPOINT_DNS_OUTBOUND_TAG
            }),
        );
    }

    serde_json::to_vec(&document)
        .map_err(|error| invalid(format!("Xray runtime serialization failed: {error}")))
}

const ENDPOINT_DNS_TAG: &str = "multicore-endpoint-dns";
const ENDPOINT_DNS_OUTBOUND_TAG: &str = "multicore-endpoint-dns-direct";
/// Shared with the readiness warm-up in `sidecar.rs`, which must bound its
/// wait for Xray's own DNS-failure close by the same value this config
/// actually uses - never a guessed constant that could silently drift.
pub(crate) const ENDPOINT_DNS_TIMEOUT_MS: u64 = 3000;

fn local_dns_domain(domain: &str) -> bool {
    !domain.contains('.')
        || [
            "local",
            "localdomain",
            "localhost",
            "lan",
            "home.arpa",
            "example",
            "invalid",
            "test",
        ]
        .iter()
        .any(|suffix| domain == *suffix || domain.ends_with(&format!(".{suffix}")))
}

fn selects_endpoint_dns_outbound(document: &serde_json::Value) -> bool {
    let matches = |selector: &serde_json::Value| {
        if let Some(prefixes) = selector.as_str() {
            // Xray's StringList accepts comma-separated scalar selectors.
            return prefixes
                .split(',')
                .any(|prefix| ENDPOINT_DNS_OUTBOUND_TAG.starts_with(prefix));
        }
        selector.as_array().is_some_and(|prefixes| {
            prefixes
                .iter()
                .filter_map(serde_json::Value::as_str)
                .any(|prefix| ENDPOINT_DNS_OUTBOUND_TAG.starts_with(prefix))
        })
    };
    document
        .pointer("/routing/balancers")
        .and_then(serde_json::Value::as_array)
        .is_some_and(|balancers| {
            balancers
                .iter()
                .any(|balancer| matches(&balancer["selector"]))
        })
        || matches(&document["observatory"]["subjectSelector"])
        || matches(&document["burstObservatory"]["subjectSelector"])
}

fn contains_endpoint_dns_tag(value: &serde_json::Value) -> bool {
    // Fail closed even for references to a missing provider tag: injecting an
    // outbound with that name could otherwise make an unrelated rule direct.
    match value {
        serde_json::Value::String(value) => {
            value == ENDPOINT_DNS_TAG || value == ENDPOINT_DNS_OUTBOUND_TAG
        }
        serde_json::Value::Array(values) => values.iter().any(contains_endpoint_dns_tag),
        serde_json::Value::Object(values) => values.values().any(contains_endpoint_dns_tag),
        _ => false,
    }
}

pub fn xray_outbound_server_domains(snapshot: &Snapshot) -> Result<Vec<String>, PersistenceError> {
    let document: serde_json::Value =
        serde_json::from_slice(snapshot.xray.raw_bytes()).map_err(|error| {
            PersistenceError::Io(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("validated Xray document is invalid: {error}"),
            ))
        })?;
    let outbounds = document
        .get("outbounds")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| {
            PersistenceError::Io(io::Error::new(
                io::ErrorKind::InvalidData,
                "validated Xray outbounds section is not an array",
            ))
        })?;
    let mut domains = BTreeSet::new();
    for outbound in outbounds {
        let outbound = outbound.as_object().ok_or_else(|| {
            PersistenceError::Io(io::Error::new(
                io::ErrorKind::InvalidData,
                "validated Xray outbound is not an object",
            ))
        })?;
        domains.extend(outbound_server_domains(outbound));
    }
    Ok(domains.into_iter().collect())
}

fn outbound_server_domains(outbound: &serde_json::Map<String, serde_json::Value>) -> Vec<String> {
    let Some(settings) = outbound
        .get("settings")
        .and_then(serde_json::Value::as_object)
    else {
        return Vec::new();
    };
    let direct = settings.get("address").and_then(serde_json::Value::as_str);
    let servers = ["servers", "vnext"]
        .into_iter()
        .filter_map(|key| settings.get(key).and_then(serde_json::Value::as_array))
        .flatten()
        .filter_map(serde_json::Value::as_object)
        .filter_map(|server| server.get("address"))
        .filter_map(serde_json::Value::as_str);
    direct
        .into_iter()
        .chain(servers)
        .filter(|address| address.parse::<IpAddr>().is_err())
        .filter(|address| !address.is_empty() && !address.chars().any(char::is_control))
        .map(ToOwned::to_owned)
        .collect()
}

fn stage_runtime_payloads(
    mihomo: &[u8],
    xray: &[u8],
    runtime_root: impl AsRef<Path>,
) -> Result<StagedRuntime, PersistenceError> {
    let runtime_root = runtime_root.as_ref();
    fs::create_dir_all(runtime_root)?;
    secure_directory(runtime_root)?;
    let generation = next_named_generation(runtime_root, "runtime-")?;
    let published_name = format!("runtime-{generation:020}");
    let published = runtime_root.join(&published_name);
    let staging = runtime_root.join(format!(".{published_name}.staging-{}", std::process::id()));

    let preparation = (|| {
        fs::create_dir(&staging)?;
        secure_directory(&staging)?;
        write_synced(&staging.join("mihomo.yaml"), mihomo)?;
        write_synced(&staging.join("xray.json"), xray)?;
        sync_directory(&staging)?;
        fs::rename(&staging, &published)
    })();
    if let Err(error) = preparation {
        if let Err(cleanup) = fs::remove_dir_all(&staging)
            && cleanup.kind() != io::ErrorKind::NotFound
        {
            return Err(PersistenceError::Io(io::Error::new(
                cleanup.kind(),
                format!("runtime preparation failed ({error}); staging cleanup failed: {cleanup}"),
            )));
        }
        return Err(PersistenceError::Io(error));
    }
    if let Err(error) = sync_directory(runtime_root) {
        return Err(PersistenceError::MaintenanceDebt(error));
    }

    Ok(StagedRuntime {
        root: runtime_root.to_owned(),
        paths: PublishedRuntime {
            mihomo_config: published.join("mihomo.yaml"),
            xray_config: published.join("xray.json"),
            directory: published,
        },
        finished: false,
    })
}

fn remove_staged_runtime(root: &Path, staged: &Path) -> Result<(), PersistenceError> {
    let canonical_root = fs::canonicalize(root)?;
    let canonical_staged = fs::canonicalize(staged)?;
    if canonical_staged.parent() != Some(canonical_root.as_path())
        || !is_runtime_generation_name(
            canonical_staged
                .file_name()
                .and_then(|name| name.to_str())
                .unwrap_or_default(),
        )
    {
        return Err(PersistenceError::Io(io::Error::new(
            io::ErrorKind::InvalidData,
            "staged runtime escaped its parent",
        )));
    }
    fs::remove_dir_all(canonical_staged)?;
    sync_directory(root)?;
    Ok(())
}

fn generation_directories(
    root: &DirectoryCapability,
) -> Result<Vec<(u64, DirectoryCapability)>, io::Error> {
    let mut generations = Vec::new();
    for entry in root.entries()? {
        if !entry.is_directory {
            continue;
        }
        let name = entry.name;
        let Some(name) = name.to_str() else {
            continue;
        };
        let Some(id) = name
            .strip_prefix("snapshot-")
            .and_then(|id| id.parse::<u64>().ok())
        else {
            continue;
        };
        generations.push((id, root.open_child(OsStr::new(name))?));
    }
    Ok(generations)
}

fn next_named_generation(root: &Path, prefix: &str) -> Result<u64, io::Error> {
    let mut highest = 0_u64;
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let Some(name) = entry.file_name().to_str().map(ToOwned::to_owned) else {
            continue;
        };
        if let Some(id) = name
            .strip_prefix(prefix)
            .and_then(|id| id.parse::<u64>().ok())
        {
            highest = highest.max(id);
        }
    }
    Ok(highest.saturating_add(1).max(1))
}

fn load_generation(directory: &DirectoryCapability) -> Option<Snapshot> {
    let mihomo = directory
        .read_file(OsStr::new("mihomo.yaml"), MAX_CONFIG_BYTES as u64)
        .ok()?;
    let xray = directory
        .read_file(OsStr::new("xray.json"), MAX_CONFIG_BYTES as u64)
        .ok()?;
    let mut snapshot = Snapshot::parse(&mihomo, &xray).ok()?;
    if directory
        .file_metadata(OsStr::new("subscription.json"))
        .ok()
        .flatten()
        .is_some_and(|(regular, len)| regular && len <= 16 * 1024)
        && let Ok(bytes) = directory.read_file(OsStr::new("subscription.json"), 16 * 1024)
        && let Ok(mut record) = serde_json::from_slice::<SubscriptionRecord>(&bytes)
        && let Ok(parsed) = reqwest::Url::parse(&record.source_url)
        && record.source_url.len() <= 8192
        && matches!(parsed.scheme(), "http" | "https")
        && parsed.host_str() == Some(record.info.source_host.as_str())
    {
        if record.info.display_name.is_empty() {
            record
                .info
                .display_name
                .clone_from(&record.info.source_host);
        }
        snapshot.subscription = Some(record);
    }
    if directory
        .file_metadata(OsStr::new("service-logo.png"))
        .ok()
        .flatten()
        .is_some_and(|(regular, len)| regular && len <= MAX_SERVICE_LOGO_OUTPUT_BYTES as u64)
        && let Ok((bytes, retained)) = directory.read_file_retained(
            OsStr::new("service-logo.png"),
            MAX_SERVICE_LOGO_OUTPUT_BYTES as u64,
        )
        && is_canonical_service_logo(&bytes)
    {
        snapshot.service_logo_png = Some(bytes);
        snapshot.service_logo_path = retained.path().map(Path::to_owned);
        snapshot.service_logo_file = Some(retained);
    }
    Some(snapshot)
}

fn create_snapshot_staging(
    root: &DirectoryCapability,
    published_name: &str,
) -> Result<(OsString, DirectoryCapability), PersistenceError> {
    let name = OsString::from(format!(".{published_name}.staging-{}", std::process::id()));
    let child = root.create_child(&name)?;
    Ok((name, child))
}

fn combine_persistence_debt(existing: Option<io::Error>, next: io::Error) -> io::Error {
    match existing {
        Some(existing) => io::Error::new(
            next.kind(),
            format!(
                "directory sync failed ({existing}); post-publication maintenance failed: {next}"
            ),
        ),
        None => next,
    }
}

fn write_synced(path: &Path, bytes: &[u8]) -> Result<(), io::Error> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    secure_file(path)?;
    file.write_all(bytes)?;
    file.flush()?;
    file.sync_all()
}

fn cleanup_superseded_runtimes(root: &Path, active: &Path) -> io::Result<()> {
    let canonical_root = fs::canonicalize(root)?;
    let canonical_active = fs::canonicalize(active)?;
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if !is_runtime_generation_name(name) {
            continue;
        }
        let file_type = entry.file_type()?;
        if !file_type.is_dir() || file_type.is_symlink() {
            continue;
        }
        let candidate = fs::canonicalize(entry.path())?;
        if candidate == canonical_active {
            continue;
        }
        if candidate.parent() != Some(canonical_root.as_path()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "runtime generation escaped its parent",
            ));
        }
        fs::remove_dir_all(candidate)?;
    }
    Ok(())
}

fn is_runtime_generation_name(name: &str) -> bool {
    name.strip_prefix("runtime-").is_some_and(|suffix| {
        suffix.len() == 20 && suffix.bytes().all(|byte| byte.is_ascii_digit())
    })
}

#[cfg(unix)]
pub(crate) fn secure_directory(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))
}

#[cfg(unix)]
pub(crate) fn secure_file(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
}

#[cfg(windows)]
pub(crate) fn secure_directory(path: &Path) -> io::Result<()> {
    apply_owner_only_acl(path, true)
}

#[cfg(windows)]
pub(crate) fn secure_file(path: &Path) -> io::Result<()> {
    apply_owner_only_acl(path, false)
}

#[cfg(windows)]
fn apply_owner_only_acl(path: &Path, inheritable: bool) -> io::Result<()> {
    use std::{ffi::c_void, os::windows::ffi::OsStrExt, ptr};
    use windows_sys::Win32::{
        Foundation::{CloseHandle, ERROR_SUCCESS, HANDLE, LocalFree},
        Security::{
            Authorization::{
                EXPLICIT_ACCESS_W, NO_MULTIPLE_TRUSTEE, SE_FILE_OBJECT, SET_ACCESS,
                SetEntriesInAclW, SetNamedSecurityInfoW, TRUSTEE_IS_SID, TRUSTEE_IS_USER,
                TRUSTEE_W,
            },
            DACL_SECURITY_INFORMATION, GetTokenInformation, PROTECTED_DACL_SECURITY_INFORMATION,
            SUB_CONTAINERS_AND_OBJECTS_INHERIT, TOKEN_QUERY, TOKEN_USER, TokenUser,
        },
        Storage::FileSystem::FILE_ALL_ACCESS,
        System::Threading::{GetCurrentProcess, OpenProcessToken},
    };

    let mut token: HANDLE = ptr::null_mut();
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let result = (|| {
        let mut bytes_needed = 0_u32;
        unsafe {
            GetTokenInformation(token, TokenUser, ptr::null_mut(), 0, &mut bytes_needed);
        }
        if bytes_needed == 0 {
            return Err(io::Error::last_os_error());
        }
        let word_size = std::mem::size_of::<usize>();
        let mut token_info = vec![0_usize; (bytes_needed as usize).div_ceil(word_size)];
        if unsafe {
            GetTokenInformation(
                token,
                TokenUser,
                token_info.as_mut_ptr().cast::<c_void>(),
                bytes_needed,
                &mut bytes_needed,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        let user = unsafe { &*(token_info.as_ptr().cast::<TOKEN_USER>()) };
        let trustee = TRUSTEE_W {
            pMultipleTrustee: ptr::null_mut(),
            MultipleTrusteeOperation: NO_MULTIPLE_TRUSTEE,
            TrusteeForm: TRUSTEE_IS_SID,
            TrusteeType: TRUSTEE_IS_USER,
            ptstrName: user.User.Sid.cast(),
        };
        let access = EXPLICIT_ACCESS_W {
            grfAccessPermissions: FILE_ALL_ACCESS,
            grfAccessMode: SET_ACCESS,
            grfInheritance: if inheritable {
                SUB_CONTAINERS_AND_OBJECTS_INHERIT
            } else {
                0
            },
            Trustee: trustee,
        };
        let mut acl = ptr::null_mut();
        let status = unsafe { SetEntriesInAclW(1, &access, ptr::null(), &mut acl) };
        if status != ERROR_SUCCESS {
            return Err(io::Error::from_raw_os_error(status as i32));
        }
        let mut wide_path: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        let status = unsafe {
            SetNamedSecurityInfoW(
                wide_path.as_mut_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                ptr::null_mut(),
                ptr::null_mut(),
                acl,
                ptr::null(),
            )
        };
        unsafe { LocalFree(acl.cast()) };
        if status != ERROR_SUCCESS {
            return Err(io::Error::from_raw_os_error(status as i32));
        }
        Ok(())
    })();
    unsafe { CloseHandle(token) };
    result
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn secure_directory(_path: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "owner-only filesystem protection is unsupported on this platform",
    ))
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn secure_file(_path: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "owner-only filesystem protection is unsupported on this platform",
    ))
}

#[cfg(unix)]
pub(crate) fn sync_directory(path: &Path) -> Result<(), io::Error> {
    fs::File::open(path)?.sync_all()
}

#[cfg(windows)]
pub(crate) fn sync_directory(path: &Path) -> Result<(), io::Error> {
    DirectoryCapability::open(path)?.sync()
}

#[cfg(test)]
mod persistence_tests {
    use super::*;

    #[test]
    fn committed_outcome_carries_snapshot_when_post_publication_sync_fails() {
        let root = tempfile::tempdir().unwrap();
        let store = PersistentSnapshotStore::open(root.path()).unwrap();
        store.inject_directory_sync_failure();
        let snapshot = Snapshot::parse(
            b"proxies: []\nproxy-groups: []\nrules: []\n",
            br#"{"outbounds":[]}"#,
        )
        .unwrap();

        let outcome = store.commit_with_generation(snapshot).unwrap();
        assert!(outcome.maintenance_debt().is_some());
        let ((generation, committed), debt) = outcome.into_parts();
        assert_eq!(generation, 1);
        assert!(debt.is_some());
        assert!(Arc::ptr_eq(&committed, &store.current().unwrap()));
    }

    #[test]
    fn committed_outcome_survives_post_publication_logo_retention_failure() {
        let root = tempfile::tempdir().unwrap();
        let store = PersistentSnapshotStore::open(root.path()).unwrap();
        let mut snapshot = Snapshot::parse(
            b"proxies: []\nproxy-groups: []\nrules: []\n",
            br#"{"outbounds":[]}"#,
        )
        .unwrap();
        snapshot.set_service_logo_png(b"persisted logo bytes".to_vec());
        let first = store.commit(snapshot).unwrap().into_committed();
        assert!(first.service_logo_path().is_some());

        store.inject_logo_retention_failure();
        let outcome = store.commit_with_generation((*first).clone()).unwrap();
        assert!(outcome.maintenance_debt().is_some());
        let ((generation, committed), debt) = outcome.into_parts();
        assert_eq!(generation, 2);
        assert!(debt.is_some());
        assert!(committed.service_logo_path().is_none());
        assert!(Arc::ptr_eq(&committed, &store.current().unwrap()));
        assert!(
            root.path()
                .join("snapshot-00000000000000000002/service-logo.png")
                .is_file()
        );
    }
}
