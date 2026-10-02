//! «Сохранить лог…»: one ZIP with everything a support request needs.
//!
//! Contents: `system.txt` (app, OS, renderer, install and connection facts), `diagnostics.txt`
//! (exactly what «Копировать лог» copies), `daemon-diagnostics.json` (the daemon's
//! `/v1/diagnostics`: core health, the bounded core log, proxy mappings), `preferences.json`
//! (with profile ids pseudonymised and paths dropped) and `logs/*` from the data directory
//! (`events.log`, `client-errors.log`, the per-user core log and rotated predecessors), plus
//! `logs/service.log` (and `service.log.1`) of the always-ready service.
//!
//! Every text entry goes through the same redaction as the event feed (`redact_event_text`:
//! URLs, `token=`/`secret=`/`password=` values, authorization headers, UUID-shaped ids such as
//! the HWID), plus quoted JSON keys of the same names, the device identity file's exact value
//! and the user's home directory. Nothing here reads anything the app does not already own:
//! the service's files are SYSTEM/root-only, so its core log arrives through the daemon's
//! diagnostics and its own log through `/v1/service-log` (redacted by the service first).
//! The uncompressed total is capped at [`MAX_BUNDLE_BYTES`]; files are taken newest first and
//! a file that does not fit keeps its newest lines.

use multicore_i18n::tr;
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Instant, SystemTime};

use regex::Regex;

use crate::daemon::{DaemonError, DaemonServiceLog};
use crate::view_model::redact_event_text;

/// Uncompressed size cap of everything in the archive.
pub(crate) const MAX_BUNDLE_BYTES: usize = 20 * 1024 * 1024;
/// A single preferences file is never larger than this (see `preferences.rs`).
const MAX_PREFERENCES_BYTES: u64 = 1024 * 1024;
/// Starts a file whose beginning did not fit in the archive.
fn truncated_marker() -> String {
    format!("{}\n", tr!("[… начало файла не поместилось в архив …]"))
}

static APP_STARTED: OnceLock<Instant> = OnceLock::new();

/// Called once at start-up, so `system.txt` can report how long the app has been running.
pub(crate) fn note_app_start() {
    let _ = APP_STARTED.set(Instant::now());
}

/// `MULTICORE-logs-20261002-153012.zip`, in local time.
pub(crate) fn default_file_name(now: chrono::DateTime<chrono::Local>) -> String {
    format!("MULTICORE-logs-{}.zip", now.format("%Y%m%d-%H%M%S"))
}

/// What the UI thread knows when the user asks for the archive.
#[derive(Clone, Debug, Default)]
pub(crate) struct UiFacts {
    pub(crate) scale_factor: f32,
    /// The connection state as the window shows it ("Подключено", "Защита выключена", …).
    pub(crate) connection: String,
    pub(crate) service_mode: bool,
    pub(crate) cores_ready: bool,
    pub(crate) tun_enabled: bool,
    /// The connection's uptime as shown on the home screen; empty while disconnected.
    pub(crate) session_uptime: String,
}

/// Everything that goes into the archive besides the files under the data directory.
#[derive(Clone, Debug, Default)]
pub(crate) struct BundleInputs {
    pub(crate) system: String,
    pub(crate) diagnostics: String,
    /// `/v1/diagnostics` as JSON, or why it could not be read.
    pub(crate) daemon_diagnostics: String,
    /// `%LOCALAPPDATA%\MultiCore` or `~/.local/share/multicore`; `None` when unknown.
    pub(crate) data_root: Option<PathBuf>,
    /// The user's home directory, replaced by `~` everywhere.
    pub(crate) home: Option<PathBuf>,
    /// The service's own log files (`service.log`, `service.log.1`) as the daemon returned them.
    pub(crate) service_logs: Vec<(String, String)>,
}

impl BundleInputs {
    /// Inputs for the running app: its own data directory and the user's home.
    pub(crate) fn for_this_user(system: String, diagnostics: String, daemon: String) -> Self {
        Self {
            system,
            diagnostics,
            daemon_diagnostics: daemon,
            data_root: multicore_platform::dirs::user_data_root().ok(),
            home: home_directory(),
            service_logs: Vec::new(),
        }
    }
}

/// The `system.txt` line about the service's log, and its files for `logs/`. Never an error:
/// without a service (per-user mode), with a service too old to serve it, or with no answer,
/// the line says so and nothing else is added.
pub(crate) fn service_log_entries(
    result: Result<DaemonServiceLog, DaemonError>,
) -> (String, Vec<(String, String)>) {
    let line = |value: &str| format!("{:<22}{value}\n", tr!("Журнал службы"));
    let log = match result {
        Ok(log) => log,
        Err(_) => {
            return (
                line(tr!("недоступен: фоновый сервис не ответил")),
                Vec::new(),
            );
        }
    };
    let reason = match log.status.as_str() {
        "included" => None,
        "no_service" => Some(tr!("нет: служба не используется (режим без службы)")),
        "too_old" => Some(tr!("недоступен: служба слишком старая (service too old)")),
        "busy" => Some(tr!("недоступен: службой пользуется другой пользователь")),
        _ => Some(tr!("недоступен: служба не ответила")),
    };
    if let Some(reason) = reason {
        return (line(reason), Vec::new());
    }
    let files = log
        .files
        .into_iter()
        .filter(|file| matches!(file.name.as_str(), "service.log" | "service.log.1"))
        .map(|file| {
            let text = if file.truncated {
                format!("{}{}", truncated_marker(), file.text)
            } else {
                file.text
            };
            (file.name, text)
        })
        .collect::<Vec<_>>();
    let note = if files.is_empty() {
        tr!("пуст").to_owned()
    } else {
        files
            .iter()
            .map(|(name, _)| format!("logs/{name}"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    (line(&note), files)
}

fn home_directory() -> Option<PathBuf> {
    #[cfg(windows)]
    let home = std::env::var_os("USERPROFILE");
    #[cfg(not(windows))]
    let home = std::env::var_os("HOME");
    home.map(PathBuf::from)
        .filter(|path| path.is_absolute() && path.parent().is_some())
}

/// Writes the archive to `path` (replacing it). A failure leaves no half-written file behind.
pub(crate) fn write_bundle(path: &Path, inputs: &BundleInputs) -> io::Result<()> {
    let entries = collect_entries(inputs);
    let result = (|| -> io::Result<()> {
        let file = fs::File::create(path)?;
        let mut zip = zip::ZipWriter::new(io::BufWriter::new(file));
        let options = zip::write::SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Deflated);
        for (name, contents) in &entries {
            zip.start_file(name.as_str(), options)
                .map_err(io::Error::other)?;
            zip.write_all(contents)?;
        }
        let mut writer = zip.finish().map_err(io::Error::other)?;
        writer.flush()?;
        writer.get_ref().sync_all()
    })();
    if result.is_err() {
        let _ = fs::remove_file(path);
    }
    result
}

/// The archive's entries, already redacted and within [`MAX_BUNDLE_BYTES`].
pub(crate) fn collect_entries(inputs: &BundleInputs) -> Vec<(String, Vec<u8>)> {
    let scrubber = Scrubber::new(inputs);
    let mut budget = MAX_BUNDLE_BYTES;
    let mut entries = Vec::new();
    let mut push = |name: String, text: String, budget: &mut usize| {
        let text = keep_newest(&scrubber.scrub(&text), *budget);
        *budget = budget.saturating_sub(text.len());
        entries.push((name, text.into_bytes()));
    };
    push("system.txt".into(), inputs.system.clone(), &mut budget);
    push(
        "diagnostics.txt".into(),
        inputs.diagnostics.clone(),
        &mut budget,
    );
    push(
        "daemon-diagnostics.json".into(),
        inputs.daemon_diagnostics.clone(),
        &mut budget,
    );
    // Bounded by the service (512 KiB), so it comes before the user's own logs.
    let mut names = std::collections::BTreeSet::new();
    for (name, text) in &inputs.service_logs {
        if names.insert(name.clone()) {
            push(format!("logs/{name}"), text.clone(), &mut budget);
        }
    }
    if let Some(root) = &inputs.data_root {
        if let Some(preferences) = sanitized_preferences(&root.join("preferences.json")) {
            push("preferences.json".into(), preferences, &mut budget);
        }
        for (name, path) in log_files_newest_first(&root.join("logs")) {
            if budget == 0 {
                break;
            }
            // A ZIP cannot hold the same name twice.
            if !names.insert(name.clone()) {
                continue;
            }
            if let Ok(text) = read_tail(&path, budget) {
                push(format!("logs/{name}"), text, &mut budget);
            }
        }
    }
    entries
}

/// Regular files directly under `logs`, most recently modified first.
fn log_files_newest_first(directory: &Path) -> Vec<(String, PathBuf)> {
    let Ok(entries) = fs::read_dir(directory) else {
        return Vec::new();
    };
    let mut files = entries
        .filter_map(Result::ok)
        .filter_map(|entry| {
            // No links: only what the app itself wrote.
            let metadata = fs::symlink_metadata(entry.path()).ok()?;
            if !metadata.is_file() {
                return None;
            }
            let name = entry.file_name().to_str()?.to_owned();
            let safe = !name.is_empty()
                && name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'));
            safe.then(|| {
                let modified = metadata.modified().unwrap_or(SystemTime::UNIX_EPOCH);
                (modified, name, entry.path())
            })
        })
        .collect::<Vec<_>>();
    files.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    files
        .into_iter()
        .map(|(_, name, path)| (name, path))
        .collect()
}

/// The last `limit` bytes of a file (its newest lines), lossily decoded.
fn read_tail(path: &Path, limit: usize) -> io::Result<String> {
    let mut file = fs::File::open(path)?;
    let length = file.metadata()?.len();
    let start = length.saturating_sub(limit as u64);
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::new();
    file.take(limit as u64).read_to_end(&mut bytes)?;
    let mut text = String::from_utf8_lossy(&bytes).into_owned();
    if start > 0 {
        // Start at a whole line.
        let cut = text.find('\n').map_or(text.len(), |newline| newline + 1);
        text.replace_range(..cut, &truncated_marker());
    }
    Ok(text)
}

/// `text` if it fits in `budget` bytes, else its newest lines that do (with a marker).
fn keep_newest(text: &str, budget: usize) -> String {
    if text.len() <= budget {
        return text.to_owned();
    }
    let marker = truncated_marker();
    if budget <= marker.len() {
        return String::new();
    }
    let mut start = text.len() - (budget - marker.len());
    while !text.is_char_boundary(start) {
        start += 1;
    }
    let tail = &text[start..];
    let tail = tail.find('\n').map_or("", |newline| &tail[newline + 1..]);
    format!("{marker}{tail}")
}

/// The preferences without anything that identifies the user or a subscription: profile ids
/// become `profile-1`, `profile-2`, …; paths are dropped; every string is redacted.
fn sanitized_preferences(path: &Path) -> Option<String> {
    let metadata = fs::symlink_metadata(path).ok()?;
    if !metadata.is_file() || metadata.len() > MAX_PREFERENCES_BYTES {
        return None;
    }
    let mut value: serde_json::Value = serde_json::from_slice(&fs::read(path).ok()?).ok()?;
    let mut aliases = BTreeMap::new();
    let mut alias = |id: &str| -> String {
        let next = aliases.len() + 1;
        aliases
            .entry(id.to_owned())
            .or_insert_with(|| format!("profile-{next}"))
            .clone()
    };
    if let Some(object) = value.as_object_mut() {
        if let Some(hint) = object.get_mut("active_profile_hint")
            && let Some(id) = hint.as_str().map(str::to_owned)
        {
            *hint = serde_json::Value::String(alias(&id));
        }
        for key in ["last_group_by_profile", "selections_by_profile"] {
            if let Some(serde_json::Value::Object(map)) = object.get_mut(key) {
                let renamed = std::mem::take(map)
                    .into_iter()
                    .map(|(id, value)| (alias(&id), value))
                    .collect();
                *map = renamed;
            }
        }
        if object.contains_key("appimage_handoff_seen") {
            object.insert(
                "appimage_handoff_seen".into(),
                serde_json::Value::String("[path]".into()),
            );
        }
    }
    redact_json_strings(&mut value);
    serde_json::to_string_pretty(&value).ok()
}

fn redact_json_strings(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::String(text) => *text = redact_event_text(text),
        serde_json::Value::Array(items) => items.iter_mut().for_each(redact_json_strings),
        serde_json::Value::Object(map) => {
            let entries = std::mem::take(map);
            for (key, mut value) in entries {
                redact_json_strings(&mut value);
                map.insert(redact_event_text(&key), value);
            }
        }
        _ => {}
    }
}

/// Redaction applied to every text entry.
struct Scrubber {
    /// Exact values that must never appear (the device identity).
    secrets: Vec<String>,
    home: Option<String>,
}

impl Scrubber {
    fn new(inputs: &BundleInputs) -> Self {
        let secrets = inputs
            .data_root
            .as_ref()
            .and_then(|root| read_small(&root.join("device_id")))
            .into_iter()
            .flat_map(|contents| {
                contents
                    .split(|c: char| c.is_whitespace() || c == '\0')
                    .filter(|value| value.len() >= 8)
                    .map(str::to_owned)
                    .collect::<Vec<_>>()
            })
            .collect();
        Self {
            secrets,
            home: inputs
                .home
                .as_ref()
                .map(|home| home.to_string_lossy().into_owned()),
        }
    }

    fn scrub(&self, text: &str) -> String {
        static QUOTED: OnceLock<Regex> = OnceLock::new();
        let quoted = QUOTED.get_or_init(|| {
            Regex::new(
                r#"(?i)"([a-z_-]*(?:token|password|secret|api[_-]?key|subscription(?:_url)?|url|hwid|device[_-]?id)[a-z_-]*)"\s*:\s*"(?:[^"\\]|\\.)*""#,
            )
            .expect("quoted secret regex must compile")
        });
        let mut text = text.to_owned();
        for secret in &self.secrets {
            text = text.replace(secret.as_str(), "[REDACTED_DEVICE_ID]");
        }
        let text = quoted.replace_all(&text, "\"$1\":\"[REDACTED]\"");
        let mut text = redact_event_text(&text);
        if let Some(home) = &self.home {
            text = replace_home(&text, home);
        }
        text
    }
}

/// Replaces the home directory (both slash styles, any case on Windows) with `~`.
fn replace_home(text: &str, home: &str) -> String {
    let mut out = text.to_owned();
    for variant in [
        home.to_owned(),
        home.replace('\\', "/"),
        home.replace('\\', "\\\\"),
    ] {
        if variant.len() < 3 {
            continue;
        }
        if cfg!(windows) {
            let pattern = format!("(?i){}", regex::escape(&variant));
            if let Ok(regex) = Regex::new(&pattern) {
                out = regex.replace_all(&out, "~").into_owned();
            }
        } else {
            out = out.replace(&variant, "~");
        }
    }
    out
}

fn read_small(path: &Path) -> Option<String> {
    let metadata = fs::symlink_metadata(path).ok()?;
    if !metadata.is_file() || metadata.len() > 4096 {
        return None;
    }
    fs::read_to_string(path).ok()
}

/// `system.txt`.
pub(crate) fn system_report(ui: &UiFacts) -> String {
    let mut out = String::new();
    let line = |out: &mut String, key: &str, value: &str| {
        let _ = writeln!(out, "{key:<22}{value}");
    };
    line(
        &mut out,
        "MULTICORE",
        &format!(
            "{} ({}, {}-bit build)",
            env!("CARGO_PKG_VERSION"),
            std::env::consts::ARCH,
            usize::BITS
        ),
    );
    line(&mut out, tr!("ОС"), &os_description());
    line(&mut out, tr!("Процессор"), &cpu_architecture());
    line(
        &mut out,
        tr!("Язык"),
        &locale().unwrap_or_else(|| "—".into()),
    );
    line(
        &mut out,
        tr!("Масштаб экрана"),
        &format!("{:.0}%", f64::from(ui.scale_factor) * 100.0),
    );
    line(&mut out, tr!("Отрисовка"), &renderer_description());
    let executable = std::env::current_exe().ok();
    line(
        &mut out,
        tr!("Установка"),
        &install_description(executable.as_deref()),
    );
    line(
        &mut out,
        tr!("Путь установки"),
        &executable
            .as_deref()
            .and_then(Path::parent)
            .map_or_else(|| "—".into(), |path| path.display().to_string()),
    );
    line(
        &mut out,
        tr!("Служба"),
        if ui.service_mode {
            if ui.cores_ready {
                tr!("используется, ядра запущены")
            } else {
                tr!("используется, ядра не запущены")
            }
        } else {
            tr!("не используется")
        },
    );
    line(
        &mut out,
        tr!("Режим"),
        if ui.tun_enabled {
            tr!("TUN (весь трафик)")
        } else {
            tr!("системный прокси")
        },
    );
    // «Подключение» is also the connecting state ("Connecting") in the catalog.
    let connection = match multicore_i18n::current() {
        multicore_i18n::Lang::Ru => "Подключение", // i18n-ru-only
        multicore_i18n::Lang::En => "Connection",
    };
    line(&mut out, connection, &ui.connection);
    if !ui.session_uptime.is_empty() {
        line(&mut out, tr!("Время подключения"), &ui.session_uptime);
    }
    let uptime = APP_STARTED.get().map(Instant::elapsed).unwrap_or_default();
    line(
        &mut out,
        tr!("Приложение работает"),
        &format!(
            "{:02}:{:02}:{:02}",
            uptime.as_secs() / 3600,
            uptime.as_secs() / 60 % 60,
            uptime.as_secs() % 60
        ),
    );
    line(
        &mut out,
        tr!("Архив создан"),
        &chrono::Local::now()
            .format("%Y-%m-%d %H:%M:%S %:z")
            .to_string(),
    );
    out
}

fn renderer_description() -> String {
    let software = crate::renderer_fallback::software_renderer_requested();
    let gpu = if cfg!(windows) {
        "Skia (Direct3D 12)"
    } else {
        "Skia (OpenGL)"
    };
    let mut description = if software {
        tr!("программная (резервная: GPU-отрисовка не запустилась)").to_owned()
    } else {
        gpu.to_owned()
    };
    if std::env::var_os("SLINT_BACKEND").is_some() {
        description.push_str(tr!(" · задан SLINT_BACKEND"));
    }
    description
}

fn install_description(executable: Option<&Path>) -> String {
    #[cfg(windows)]
    {
        let per_user = match (executable, std::env::var_os("LOCALAPPDATA")) {
            (Some(executable), Some(local)) => {
                crate::service_install::is_per_user_install(executable, Path::new(&local))
            }
            _ => false,
        };
        let service = if crate::service_install::service_installed() {
            tr!("служба установлена")
        } else {
            tr!("без службы")
        };
        format!(
            "{} · {service}",
            if per_user {
                tr!("для текущего пользователя")
            } else {
                tr!("для всех пользователей")
            }
        )
    }
    #[cfg(not(windows))]
    {
        if std::env::var_os("APPIMAGE").is_some() {
            "AppImage".to_owned()
        } else if executable
            .is_some_and(|path| path.starts_with("/usr") || path.starts_with("/opt"))
        {
            tr!("системный пакет").to_owned()
        } else {
            tr!("другое").to_owned()
        }
    }
}

#[cfg(windows)]
fn os_description() -> String {
    const KEY: &str = r"SOFTWARE\Microsoft\Windows NT\CurrentVersion";
    let product = registry::string(KEY, "ProductName").unwrap_or_else(|| "Windows".into());
    let build = registry::string(KEY, "CurrentBuild")
        .or_else(|| registry::string(KEY, "CurrentBuildNumber"))
        .unwrap_or_default();
    let revision = registry::dword(KEY, "UBR");
    let display = registry::string(KEY, "DisplayVersion").unwrap_or_default();
    // Windows 11 still reports "Windows 10" in ProductName.
    let product = if build.parse::<u32>().is_ok_and(|build| build >= 22000) {
        product.replacen("Windows 10", "Windows 11", 1)
    } else {
        product
    };
    let mut description = product;
    if !display.is_empty() {
        let _ = write!(description, " {display}");
    }
    if !build.is_empty() {
        let _ = write!(description, " ({}", tr!("сборка {}", build));
        if let Some(revision) = revision {
            let _ = write!(description, ".{revision}");
        }
        description.push(')');
    }
    description
}

#[cfg(not(windows))]
fn os_description() -> String {
    let release = fs::read_to_string("/etc/os-release").unwrap_or_default();
    let pretty = release
        .lines()
        .find_map(|line| line.strip_prefix("PRETTY_NAME="))
        .map(|value| value.trim_matches('"').to_owned())
        .unwrap_or_else(|| "Linux".into());
    let kernel = fs::read_to_string("/proc/sys/kernel/osrelease").unwrap_or_default();
    let session = std::env::var("XDG_SESSION_TYPE").unwrap_or_default();
    let desktop = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default();
    tr!(
        "{} · ядро {} · {} {}",
        pretty,
        kernel.trim(),
        session.trim(),
        desktop.trim()
    )
    .trim_end()
    .to_owned()
}

fn cpu_architecture() -> String {
    #[cfg(windows)]
    {
        // A 32-bit build on 64-bit Windows sees the real architecture in PROCESSOR_ARCHITEW6432.
        std::env::var("PROCESSOR_ARCHITEW6432")
            .or_else(|_| std::env::var("PROCESSOR_ARCHITECTURE"))
            .unwrap_or_else(|_| std::env::consts::ARCH.to_owned())
    }
    #[cfg(not(windows))]
    {
        std::env::consts::ARCH.to_owned()
    }
}

#[cfg(windows)]
fn locale() -> Option<String> {
    registry::user_string(r"Control Panel\International", "LocaleName")
}

#[cfg(not(windows))]
fn locale() -> Option<String> {
    ["LC_ALL", "LC_MESSAGES", "LANG"]
        .iter()
        .find_map(|key| std::env::var(key).ok().filter(|value| !value.is_empty()))
}

#[cfg(windows)]
mod registry {
    use std::ffi::c_void;
    use std::ptr::null_mut;
    use windows_sys::Win32::Foundation::ERROR_SUCCESS;
    use windows_sys::Win32::System::Registry::{
        HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, RRF_RT_REG_DWORD, RRF_RT_REG_SZ, RegGetValueW,
    };

    fn wide(value: &str) -> Vec<u16> {
        value.encode_utf16().chain(Some(0)).collect()
    }

    fn read_string(root: HKEY, key: &str, name: &str) -> Option<String> {
        let (key, name) = (wide(key), wide(name));
        let mut buffer = vec![0_u16; 512];
        let mut bytes = (buffer.len() * 2) as u32;
        let status = unsafe {
            RegGetValueW(
                root,
                key.as_ptr(),
                name.as_ptr(),
                RRF_RT_REG_SZ,
                null_mut(),
                buffer.as_mut_ptr().cast::<c_void>(),
                &mut bytes,
            )
        };
        if status != ERROR_SUCCESS {
            return None;
        }
        buffer.truncate(bytes as usize / 2);
        while buffer.last() == Some(&0) {
            buffer.pop();
        }
        String::from_utf16(&buffer).ok()
    }

    pub(super) fn string(key: &str, name: &str) -> Option<String> {
        read_string(HKEY_LOCAL_MACHINE, key, name)
    }

    pub(super) fn user_string(key: &str, name: &str) -> Option<String> {
        read_string(HKEY_CURRENT_USER, key, name)
    }

    pub(super) fn dword(key: &str, name: &str) -> Option<u32> {
        let (key, name) = (wide(key), wide(name));
        let mut value = 0_u32;
        let mut bytes = 4_u32;
        let status = unsafe {
            RegGetValueW(
                HKEY_LOCAL_MACHINE,
                key.as_ptr(),
                name.as_ptr(),
                RRF_RT_REG_DWORD,
                null_mut(),
                (&mut value as *mut u32).cast::<c_void>(),
                &mut bytes,
            )
        };
        (status == ERROR_SUCCESS).then_some(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SUBSCRIPTION_URL: &str = "https://panel.example.net/api/sub/Zx9QpLm2Kd7Vb4Ns";
    const SUBSCRIPTION_TOKEN: &str = "Zx9QpLm2Kd7Vb4Ns";
    const CONTROLLER_SECRET: &str = "3f9a1c7e5b2d8f604a1e9c3b7d5f2a86";
    const DEVICE_ID: &str = "B64CD3CC-5D61-72F2-AE98-7277AF087AAD";
    const PROFILE_ID: &str = "8c1d6f0e2a4b49d7b3e5f7a9c1d3e5f7";

    fn read_zip(path: &Path) -> BTreeMap<String, String> {
        let mut archive = zip::ZipArchive::new(fs::File::open(path).unwrap()).unwrap();
        (0..archive.len())
            .map(|index| {
                let mut entry = archive.by_index(index).unwrap();
                let mut text = String::new();
                entry.read_to_string(&mut text).unwrap();
                (entry.name().to_owned(), text)
            })
            .collect()
    }

    fn data_root_with_secrets(root: &Path) {
        let logs = root.join("logs");
        fs::create_dir_all(&logs).unwrap();
        fs::write(root.join("device_id"), format!("{DEVICE_ID}\n")).unwrap();
        fs::write(
            logs.join("events.log"),
            format!(
                "{{\"id\":1,\"message\":\"fetch failed\",\"details\":{{\"url\":\"{SUBSCRIPTION_URL}\",\"secret\":\"{CONTROLLER_SECRET}\"}}}}\n\
                 {{\"id\":2,\"message\":\"x-hwid {DEVICE_ID}\"}}\n"
            ),
        )
        .unwrap();
        fs::write(
            logs.join("client-errors.log"),
            format!("1\tcontext\tGET {SUBSCRIPTION_URL} token={SUBSCRIPTION_TOKEN} failed\n"),
        )
        .unwrap();
        fs::write(
            logs.join("latest-core.log"),
            format!(
                "mihomo secret: {CONTROLLER_SECRET}\nAuthorization: Bearer {CONTROLLER_SECRET}\n\
                 device {}\n",
                DEVICE_ID.to_ascii_lowercase()
            ),
        )
        .unwrap();
        fs::write(
            root.join("preferences.json"),
            serde_json::json!({
                "schema_version": 1,
                "active_profile_hint": PROFILE_ID,
                "last_group_by_profile": { PROFILE_ID: "Proxy" },
                "selections_by_profile": { PROFILE_ID: { "Proxy": "Germany" } },
                "appimage_handoff_seen": "/home/someone/Apps/MultiCore.AppImage",
                "tun_enabled": true,
            })
            .to_string(),
        )
        .unwrap();
    }

    #[test]
    fn the_archive_never_contains_a_subscription_url_token_secret_or_device_id() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("MultiCore");
        data_root_with_secrets(&root);
        let inputs = BundleInputs {
            system: format!("install path {}", root.display()),
            diagnostics: format!("[12:00:00] error core: import {SUBSCRIPTION_URL} failed"),
            daemon_diagnostics: format!(
                "{{\"logs\":[{{\"message\":\"secret={CONTROLLER_SECRET} {SUBSCRIPTION_URL}\"}}]}}"
            ),
            data_root: Some(root.clone()),
            home: Some(directory.path().to_path_buf()),
            service_logs: Vec::new(),
        };
        let path = directory.path().join("bundle.zip");
        write_bundle(&path, &inputs).unwrap();
        let entries = read_zip(&path);

        for name in [
            "system.txt",
            "diagnostics.txt",
            "daemon-diagnostics.json",
            "preferences.json",
            "logs/events.log",
            "logs/client-errors.log",
            "logs/latest-core.log",
        ] {
            assert!(entries.contains_key(name), "missing {name}: {entries:?}");
        }
        let home = directory.path().to_string_lossy().into_owned();
        for (name, text) in &entries {
            for secret in [
                SUBSCRIPTION_URL,
                SUBSCRIPTION_TOKEN,
                CONTROLLER_SECRET,
                DEVICE_ID,
                &DEVICE_ID.to_ascii_lowercase(),
                PROFILE_ID,
                "panel.example.net",
                "/home/someone",
                home.as_str(),
            ] {
                assert!(!text.contains(secret), "{name} leaks {secret}: {text}");
            }
        }
        assert!(entries["preferences.json"].contains("profile-1"));
        assert!(entries["preferences.json"].contains("Germany"));
        assert!(entries["logs/client-errors.log"].contains("[REDACTED"));
    }

    #[test]
    fn a_large_log_keeps_its_newest_lines_within_the_size_cap() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("data");
        let logs = root.join("logs");
        fs::create_dir_all(&logs).unwrap();
        let mut big = String::new();
        let mut index = 0_u64;
        while big.len() < MAX_BUNDLE_BYTES + 4 * 1024 * 1024 {
            let _ = writeln!(big, "line {index:08} {}", "x".repeat(80));
            index += 1;
        }
        fs::write(logs.join("latest-core.log"), &big).unwrap();
        let inputs = BundleInputs {
            system: "system".into(),
            diagnostics: "diagnostics".into(),
            daemon_diagnostics: "{}".into(),
            data_root: Some(root),
            home: None,
            service_logs: Vec::new(),
        };
        let entries = collect_entries(&inputs);
        let total = entries.iter().map(|(_, bytes)| bytes.len()).sum::<usize>();
        assert!(total <= MAX_BUNDLE_BYTES, "{total}");
        let (_, core) = entries
            .iter()
            .find(|(name, _)| name == "logs/latest-core.log")
            .unwrap();
        let core = String::from_utf8_lossy(core);
        assert!(core.starts_with(&truncated_marker()));
        assert!(
            core.contains(&format!("line {:08}", index - 1)),
            "newest line kept"
        );
        assert!(!core.contains("line 00000000 "), "oldest line dropped");
    }

    fn service_log_response(files: &[(&str, &str, bool)]) -> DaemonServiceLog {
        DaemonServiceLog {
            status: "included".into(),
            files: files
                .iter()
                .map(
                    |(name, text, truncated)| crate::daemon::DaemonServiceLogFile {
                        name: (*name).into(),
                        text: (*text).into(),
                        truncated: *truncated,
                    },
                )
                .collect(),
        }
    }

    #[test]
    fn the_service_log_is_archived_and_a_planted_link_or_token_never_reaches_the_zip() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("MultiCore");
        data_root_with_secrets(&root);
        // The user's own logs directory happens to hold a `service.log` too.
        fs::write(root.join("logs").join("service.log"), "user copy\n").unwrap();
        let home = directory.path().to_string_lossy().into_owned();
        let planted = format!(
            "1700000000 prepare generation 2 refused (NotReady) at resolve: GET {SUBSCRIPTION_URL} token={SUBSCRIPTION_TOKEN}\n\
             1700000001 secret={CONTROLLER_SECRET} hwid {DEVICE_ID} home {home}\\x\n\
             1700000002 share vless://{DEVICE_ID}@node.example.org:443#Germany\n"
        );
        // What the service sends is already redacted; the archive redacts it once more.
        let served = multicore_core::service_protocol::redact_service_log(&planted);
        let (note, service_logs) = service_log_entries(Ok(service_log_response(&[
            ("service.log", &served, true),
            (
                "service.log.1",
                "1690000000 set_tun on ok in 812 ms\n",
                false,
            ),
            ("../evil", "nope", false),
        ])));
        assert!(
            note.contains("logs/service.log, logs/service.log.1"),
            "{note}"
        );
        let inputs = BundleInputs {
            system: format!("system\n{note}"),
            diagnostics: "diagnostics".into(),
            daemon_diagnostics: "{}".into(),
            data_root: Some(root.clone()),
            home: Some(directory.path().to_path_buf()),
            service_logs,
        };
        let path = directory.path().join("bundle.zip");
        write_bundle(&path, &inputs).unwrap();
        let entries = read_zip(&path);
        let service = &entries["logs/service.log"];
        assert!(service.starts_with(&truncated_marker()), "{service}");
        assert!(service.contains("1700000000 prepare generation 2 refused (NotReady) at resolve"));
        assert!(
            !service.contains("user copy"),
            "the service's file wins the name"
        );
        assert!(entries["logs/service.log.1"].contains("set_tun on ok in 812 ms"));
        assert!(!entries.keys().any(|name| name.contains("evil")));
        assert!(entries["system.txt"].contains("Журнал службы"));
        for (name, text) in &entries {
            for secret in [
                SUBSCRIPTION_URL,
                SUBSCRIPTION_TOKEN,
                CONTROLLER_SECRET,
                DEVICE_ID,
                "panel.example.net",
                "node.example.org",
                "vless://",
                home.as_str(),
            ] {
                assert!(!text.contains(secret), "{name} leaks {secret}: {text}");
            }
        }
    }

    #[test]
    fn without_a_usable_service_log_system_txt_says_why_and_nothing_else_is_added() {
        for (result, expected) in [
            (
                Ok(DaemonServiceLog {
                    status: "too_old".into(),
                    files: Vec::new(),
                }),
                "service too old",
            ),
            (
                Ok(DaemonServiceLog {
                    status: "no_service".into(),
                    files: Vec::new(),
                }),
                "режим без службы",
            ),
            (
                Ok(DaemonServiceLog {
                    status: "busy".into(),
                    files: Vec::new(),
                }),
                "другой пользователь",
            ),
            (
                Ok(DaemonServiceLog {
                    status: "something_new".into(),
                    files: Vec::new(),
                }),
                "не ответила",
            ),
            (Err(DaemonError::new("404")), "фоновый сервис не ответил"),
            (Ok(service_log_response(&[])), "пуст"),
        ] {
            let (note, files) = service_log_entries(result);
            assert!(note.starts_with("Журнал службы"), "{note}");
            assert!(note.contains(expected), "{note}");
            assert!(files.is_empty());
        }
        let inputs = BundleInputs {
            system: "system".into(),
            diagnostics: "diagnostics".into(),
            daemon_diagnostics: "{}".into(),
            data_root: None,
            home: None,
            service_logs: Vec::new(),
        };
        assert!(
            !collect_entries(&inputs)
                .iter()
                .any(|(name, _)| name.starts_with("logs/service"))
        );
    }

    #[test]
    fn default_name_is_timestamped() {
        use chrono::TimeZone;
        let now = chrono::Local
            .with_ymd_and_hms(2026, 10, 2, 15, 30, 12)
            .single()
            .unwrap();
        assert_eq!(default_file_name(now), "MULTICORE-logs-20261002-153012.zip");
    }

    #[test]
    fn system_report_names_the_version_and_the_mode() {
        let report = system_report(&UiFacts {
            scale_factor: 1.25,
            connection: "Подключено".into(),
            service_mode: true,
            cores_ready: true,
            tun_enabled: false,
            session_uptime: "00:01:02".into(),
        });
        assert!(report.contains(env!("CARGO_PKG_VERSION")));
        assert!(report.contains("125%"));
        assert!(report.contains("системный прокси"));
        assert!(report.contains("ядра запущены"));
    }
}

#[cfg(test)]
mod english_tests {
    use super::*;
    use multicore_i18n::{Lang, ThreadLanguage};

    #[test]
    fn the_service_log_line_reads_in_english() {
        let _en = ThreadLanguage::set(Lang::En);
        let (note, files) = service_log_entries(Err(DaemonError::new("404")));
        assert!(note.starts_with("Service log"), "{note}");
        assert!(note.contains("no background service reply"), "{note}");
        assert!(files.is_empty());
        assert!(truncated_marker().starts_with("[… file start"));
    }
}
